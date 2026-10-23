/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore tool global state, and centralized methods corresponding to the centralized portion of
//! the Detcore tool.

mod exec_identity;
#[cfg(feature = "physical-mm-test-support")]
pub mod initial_mm_probe;
pub(crate) mod mapping_capture;
pub mod native_exec;
mod parked;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::btree_map::Entry;
use std::fmt::Debug;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::num::NonZeroUsize;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering::SeqCst;
use std::task::Poll;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::bail;
use chrono::DateTime;
use chrono::Utc;
use detcore_model::host_input::HostFileIdentity;
use detcore_model::host_input::HostInputRecord;
use detcore_model::network_engine::NetworkEngine;
use detcore_model::network_engine::NetworkEngineError;
use detcore_model::network_engine::NetworkReply;
use detcore_model::network_engine::NetworkRequest;
use detcore_model::network_trace::NetworkTraceMode;
use detcore_model::network_trace::NetworkTraceV2;
use detcore_model::procfs::mount_ids_are_ordered_subset;
use detcore_model::summary::PreemptionSummary;
use detcore_model::summary::RunSummary;
use detcore_model::summary::TimesliceStats;
pub(crate) use exec_identity::reconnect_exec;
pub(crate) use exec_identity::retire_exec;
use nix::sys::signal;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
pub(crate) use parked::child_wait_request;
pub(crate) use parked::parked_wait_request;
pub(crate) use parked::polled_read_request;
pub(crate) use parked::signal_dequeued;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Tid;
use reverie::syscalls::AddrMut;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;

use crate::config::Config;
#[cfg(test)]
use crate::consts::ROOT_DETPID;
use crate::fd::SharedOpenFileError;
use crate::ivar::Ivar;
use crate::preemptions::ThreadHistory;
use crate::preemptions::boundary_recording::HistoryReader;
use crate::procfs_inode;
use crate::procfs_inode::ProcfsName;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::ChaosEpochTransition;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::AdmitIntent;
use crate::scheduler::AdmitSide;
use crate::scheduler::ConsumeResult;
use crate::scheduler::DEFAULT_PRIORITY;
use crate::scheduler::ExecReconnect;
use crate::scheduler::FutexSignalWatch;
use crate::scheduler::HostTimedSignalScope;
use crate::scheduler::MaybePrintStack;
use crate::scheduler::OwnProcessSignalInputs;
use crate::scheduler::Priority;
use crate::scheduler::SchedResponse;
use crate::scheduler::SchedValue;
use crate::scheduler::Scheduler;
use crate::scheduler::ThreadNextTurn;
use crate::scheduler::entropy_to_priority;
use crate::scheduler::parked::*;
use crate::scheduler::real_timer::DequeueAck;
use crate::scheduler::real_timer::ItimerSnapshot;
use crate::scheduler::real_timer::TimerFailure;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::scheduler::runqueue::REPLAY_DEFERRED_PRIORITY;
use crate::scheduler::runqueue::REPLAY_FOREGROUND_PRIORITY;
use crate::scheduler::runqueue::is_ordinary_priority;
use crate::scheduler::sched_loop;
use crate::scheduler::sched_loop_external;
use crate::scheduler::timed_waiters::SignalTarget;
use crate::shared_open_files::OpenFileControl;
use crate::shared_open_files::OpenFileControlReply;
use crate::shared_open_files::SharedOpenFileStore;
use crate::tool_local::Detcore;
use crate::tool_local::ExecFdBlockingOverrides;
use crate::tool_local::ExecFdState;
use crate::tool_local::RobustListWake;
use crate::types::*;

pub(crate) async fn yield_once() {
    let mut yielded = false;
    std::future::poll_fn(|context| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

#[derive(Debug)]
struct InodePool {
    // TODO(T87258449): merge these two maps:
    inodes: HashMap<RawInode, DetInode>,
    detinodes_info: HashMap<DetInode, DetInodeInfo>,
    /// Counter backing the minted [`DetInode`]s. Deliberately a plain integer:
    /// it is the *source* of deterministic inodes, not one itself, and typing
    /// it `RawInode` previously blurred that distinction.
    ///
    /// It counts *requests*, not distinct host inodes: see [`Self::add_inode`].
    next_inode: u64,
    /// Virtual mtimes set (by a write or `utimensat`) on host inodes that have
    /// no deterministic number yet. Setting an mtime does not number the file:
    /// whether those calls reach the pool depends on host state (see
    /// [`Self::set_mtime`]), so they must not consume counter values. The
    /// file's first numbering request takes the pending mtime.
    pending_mtimes: HashMap<RawInode, LogicalTime>,
    /// Mappings whose file lost its last name (see [`Self::retire`]), kept
    /// for the descriptors that can still reach it.
    retired: HashMap<RawInode, DetInode>,
    /// Files that lost their last name before anything numbered them, with
    /// the virtual mtime a write through a remaining descriptor set, if any.
    /// A descriptor sighting numbers such a file as retired, and a later name
    /// is another file (see [`Self::forget_retired`]), so the next file the
    /// host gives the inode inherits neither the number nor the mtime.
    retired_unnumbered: HashMap<RawInode, Option<LogicalTime>>,
    /// When each retired inode, numbered or not, was retired: the value of
    /// `retirements` then. A directory snapshot taken before that cannot show
    /// a new file under the inode (see [`InodeSighting::Listed`]).
    retired_at: HashMap<RawInode, u64>,
    /// How many retirements there have been. Each follows a guest's removal
    /// of a last name, so the count is the guest's, like the counter.
    retirements: u64,
    /// Every file that has held each host inode, oldest first, kept after a
    /// later file discards a retirement: a directory snapshot taken while an
    /// earlier file held the inode must still name that file (codex
    /// re-review of <https://github.com/rrnewton/hermit/pull/3849>, P1).
    generations: HashMap<RawInode, Vec<Generation>>,
    /// How many history records the snapshot lookups have examined, for the
    /// test that bounds the lookup's cost.
    #[cfg(test)]
    listed_probes: u64,
    /// How many retired mappings a later sighting showed the host had given to
    /// another file (see [`Self::forget_retired`]). Diagnostic only: it
    /// follows host state and never reaches the guest.
    reused: u64,
    /// The key each procfs host inode, and each procfs path key, is pooled
    /// under (see [`Self::procfs_key`]): the host inode its entry was first
    /// numbered under. Every value is also its own key.
    procfs_keys: HashMap<RawInode, RawInode>,
    /// For each thread ID, the value of `threads_counted` just after each
    /// guest thread given that ID was counted, in order (see
    /// [`Self::count_incarnation`]).
    incarnations: HashMap<i32, Vec<u64>>,
    /// How many guest threads have been counted (see
    /// [`Self::count_incarnation`]).
    threads_counted: u64,
    /// The state Linux keeps on a per-process `/proc` file's inode that
    /// Detcore keeps for the files it renders, by their inode number (see
    /// [`Self::rendered_file_state`]).
    rendered_file_states: HashMap<u64, crate::procfs::listing::RenderedFileState>,
}

/// How a numbering request reached its file, which decides whether a retired
/// mapping (see `InodePool::retire`) answers it.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum InodeSighting {
    /// Through a descriptor without a link count, or a stat that reports no
    /// link (`fstat` after `unlink`, `/proc/self/fd/N` of an unlinked file).
    /// Either can reach the file a retired mapping describes.
    Descriptor,
    /// A file that has a name: a directory entry read now, or any stat that
    /// reports a nonzero `st_nlink`. A retired file has no link and can never
    /// get one back, so this is another file that the host gave the freed
    /// inode.
    Name,
    /// A directory entry from a snapshot taken when there had been this many
    /// retirements. The snapshot can be older than the entry's removal: a
    /// stream reads the whole directory at its first `getdents` and returns
    /// the rest later (codex review of
    /// <https://github.com/rrnewton/hermit/pull/3849>, P1). So it shows a
    /// new file only for an inode retired before it was taken.
    Listed(u64),
}

/// What a directory snapshot records of the inode pool when it is taken, so
/// that the entries it returns later are numbered as they were then.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SnapshotMark {
    /// How many retirements there had been (see [`InodeSighting::Listed`]):
    /// an entry can be returned after its file's last name is gone.
    pub retirements: u64,
    /// How many guest threads had been counted (see
    /// `InodePool::count_incarnation`): an entry can be returned after a task
    /// it names exited and a new thread took its ID.
    pub threads_counted: u64,
}

/// One file that held a host inode (see `InodePool::generations`). It held
/// the inode from just after the previous file's retirement until its own.
#[derive(Debug)]
struct Generation {
    /// Its number, or `None` while nothing has numbered it.
    number: Option<DetInode>,
    /// The retirement count at which it lost its last name, if it has.
    retired: Option<u64>,
    /// The mtime a write left on it before anything numbered it.
    pending: Option<LogicalTime>,
}

/// Everything we know (globally) about a DetInode.
#[derive(Debug)]
struct DetInodeInfo {
    /// The virtual mtime, or `None` while no stat has observed the file and no
    /// write or explicit time update has set it. Many paths (open, read,
    /// getdents) mint a DetInode before any stat, so the first-seen policy is
    /// applied at the first stat rather than at minting; otherwise whether a
    /// file kept a canonical mtime would depend on which syscall touched it
    /// first. An unresolved mtime reads as the epoch.
    mtime: Option<LogicalTime>,
}

/// Whole-second host mtimes, with a zero nanosecond part, that a file keeps
/// when Hermit first sees it, instead of having its mtime replaced by the
/// epoch (<https://github.com/rrnewton/hermit/issues/3639>).
///
/// These values are canonical and content-independent: they are written
/// deliberately by tools that erase timestamps, so they carry no host clock
/// or run-specific information. Revealing them makes whether a file's host
/// mtime is canonical part of Hermit's input, like the file's contents: a
/// guest that rewrites the host mtime of an input file (`touch f` after
/// `stat f` on a canonical file) changes the input of the next run, exactly
/// as rewriting its contents would.
/// - `1` (1970-01-01T00:00:01Z) is the mtime of every file and directory in
///   the Nix store (seconds 1, nanoseconds 0). nixpkgs' stdenv derives
///   `SOURCE_DATE_EPOCH` from the newest source mtime, so reporting the epoch
///   instead moves embedded build timestamps away from a native build's.
/// - `0` is what `SOURCE_DATE_EPOCH=0` tooling and archive normalizers write.
///
/// Matching is exact. A non-zero nanosecond part, or any other second, is a
/// real timestamp, and the file gets the epoch as before.
pub const CANONICAL_FILE_MTIME_SECONDS: [i64; 2] = [0, 1];

/// What a syscall observed about a file's host mtime, already reduced to the
/// only part the virtual mtime may depend on.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum ObservedMtime {
    /// The caller did not read the file's metadata (open, read, getdents...).
    Unobserved,
    /// A stat observed a host mtime that is a real timestamp.
    HostSpecific,
    /// A stat observed one of [`CANONICAL_FILE_MTIME_SECONDS`], which is kept.
    Canonical(LogicalTime),
}

impl ObservedMtime {
    /// Classify a host mtime that a stat reported.
    pub fn from_host_mtime(tv_sec: i64, tv_nsec: i64) -> Self {
        match u64::try_from(tv_sec) {
            Ok(secs) if tv_nsec == 0 && CANONICAL_FILE_MTIME_SECONDS.contains(&tv_sec) => {
                ObservedMtime::Canonical(LogicalTime::from_secs(secs))
            }
            _ => ObservedMtime::HostSpecific,
        }
    }

    /// The virtual mtime to assign to a file whose mtime is still unresolved:
    /// a canonical value is kept, a real timestamp becomes the epoch, and no
    /// observation leaves it unresolved.
    fn first_seen_mtime(self, epoch: LogicalTime) -> Option<LogicalTime> {
        match self {
            ObservedMtime::Unobserved => None,
            ObservedMtime::HostSpecific => Some(epoch),
            ObservedMtime::Canonical(mtime) => Some(mtime),
        }
    }
}

/// Everything the global scheduler needs to register a new child thread. A
/// normal clone is registered by the parent; a `CLONE_VFORK` child registers
/// itself (with `parent_is_kernel_blocked` set) because its parent is blocked
/// inside the kernel until the child execs or exits.
struct ChildRegistration {
    parent_dettid: DetTid,
    parent_detpid: DetPid,
    child_dettid: DetTid,
    child_tid_addr: usize,
    flags: Option<CloneFlags>,
    exit_signal: libc::c_int,
    physical_ids: Option<(i32, i32)>,
    maybe_priority: Option<Priority>,
    parent_is_kernel_blocked: bool,
}

#[derive(Debug)]
struct PendingInheritedInput {
    lease: reverie::InheritedInputLease,
    root: Option<(DetTid, MmId)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingExecState {
    caller: DetTid,
    process: DetPid,
    mm: MmId,
    fd_blocking: ExecFdState,
}

/// Separate terminal cleanup outcomes; neither replaces the backend failure.
pub struct BackendFailureCleanup {
    /// Natural scheduler completion, retaining a task panic or cancellation.
    pub scheduler: Result<(), tokio::task::JoinError>,
    /// The requested partial recording's write result; no destination is success.
    pub preemption_recording: Result<(), String>,
}

#[derive(Clone, Copy)]
struct RpcIncarnation {
    dettid: DetTid,
    mm: MmId,
}

impl Default for InodePool {
    fn default() -> Self {
        InodePool::new()
    }
}

impl InodePool {
    fn new() -> Self {
        InodePool {
            inodes: HashMap::new(),
            detinodes_info: HashMap::new(),
            // Above the fixed stdio inodes (`DET_SPECIAL_INODE_OFFSET`), so a
            // pooled number can never collide with one of them.
            next_inode: crate::consts::DET_INODE_OFFSET.as_raw(),
            pending_mtimes: HashMap::new(),
            retired: HashMap::new(),
            retired_unnumbered: HashMap::new(),
            retired_at: HashMap::new(),
            retirements: 0,
            generations: HashMap::new(),
            #[cfg(test)]
            listed_probes: 0,
            reused: 0,
            procfs_keys: HashMap::new(),
            incarnations: HashMap::new(),
            threads_counted: 0,
            rendered_file_states: HashMap::new(),
        }
    }

    /// How many guest threads have been given task ID `tid` in this run (see
    /// [`Self::count_incarnation`]): the incarnation of the one holding it
    /// now, 0 for an ID no guest thread has held.
    fn incarnation(&self, tid: i32) -> u32 {
        self.incarnations.get(&tid).map_or(0, |counted| {
            u32::try_from(counted.len()).unwrap_or(u32::MAX)
        })
    }

    /// What Detcore's process table says of the task `named` and its thread
    /// `thread` (`existence`: `Scheduler::procfs_task`'s whether it exists
    /// and whether it has exited), with the incarnation each number has now
    /// ([`Self::incarnation`]).
    fn procfs_task_answer(
        &self,
        named: DetTid,
        thread: Option<DetTid>,
        (exists, without_mm): (bool, bool),
    ) -> crate::procfs::listing::ProcfsTaskAnswer {
        crate::procfs::listing::ProcfsTaskAnswer {
            exists,
            without_mm,
            process_incarnation: self.incarnation(named.as_raw()),
            thread_incarnation: thread.map(|thread| self.incarnation(thread.as_raw())),
            mm: None,
            runtime_private_memory: None,
        }
    }

    /// Apply `change` to the state of the rendered per-process `/proc` file
    /// numbered `inode`, and return the state. Every change comes in its
    /// guest thread's turn, so their order is the scheduler's.
    ///
    /// Only a file whose owner wrote its state (an mtime, a write hint) has
    /// an entry; a read of any other inserts none. Entries are kept for the
    /// run: a per-process inode names one task generation
    /// (`procfs_inode::per_process_inode`), and a descriptor or a directory
    /// of a task that is gone can still reach its file (an `fstat` of a
    /// descriptor kept past a reap reads its mtime), so no entry can be
    /// retired exactly. The table holds one entry per per-process inode whose
    /// owner set a time or a hint, and an inode is a spelling's identity
    /// (`procfs_inode::per_process_inode`): for a thread group of N threads,
    /// up to `(N + N²) × PER_PROCESS_NAMES.len()` inodes per group of task
    /// generations (N process spellings, N² `/proc/<n>/task/<m>` spellings,
    /// `procfs::per_process_path`), and only those the guest wrote.
    // TODO-HUMAN-REVIEW(#4213): Review the per-inode mtime and write-hint
    // state Detcore keeps for its rendered per-process files, in the global
    // tool, changed in the changing thread's turn.
    fn rendered_file_state(
        &mut self,
        inode: u64,
        change: crate::procfs::listing::RenderedFileChange,
    ) -> crate::procfs::listing::RenderedFileState {
        use crate::procfs::listing::RenderedFileChange;
        let state = match change {
            RenderedFileChange::Read => {
                return self
                    .rendered_file_states
                    .get(&inode)
                    .copied()
                    .unwrap_or_default();
            }
            _ => self.rendered_file_states.entry(inode).or_default(),
        };
        match change {
            RenderedFileChange::Read => {}
            RenderedFileChange::Mtime(seconds, nanoseconds) => {
                state.mtime = Some((seconds, nanoseconds));
            }
            RenderedFileChange::WriteHint(hint) => state.write_hint = hint,
        }
        *state
    }

    /// The key to pool the procfs entry at host inode `host` under, given the
    /// key of the path it was reached by (see [`Self::procfs_path_key`]).
    ///
    /// Linux numbers per-process procfs entries when it builds their
    /// dentries, and builds an evicted one again under a new host inode
    /// (see `procfs_inode`). A host inode the pool has seen keeps its key,
    /// so aliases (`/proc/<pid>/net/tcp` and `/proc/<pid>/task/<tid>/net/tcp`
    /// are one inode) and sightings without a path agree. A host inode the
    /// pool has not seen, reached by a path that already named an entry, is
    /// that entry rebuilt: it takes the entry's key, and with it its number
    /// and virtual mtime. Consumes no counter value.
    fn procfs_key(&mut self, host: RawInode, path: RawInode) -> RawInode {
        let key = match self.procfs_keys.get(&host) {
            Some(key) => *key,
            None if self.inodes.contains_key(&host)
                || self.retired.contains_key(&host)
                || self.retired_unnumbered.contains_key(&host) =>
            {
                host
            }
            None => self.procfs_keys.get(&path).copied().unwrap_or(host),
        };
        self.procfs_keys.insert(host, key);
        self.procfs_keys.insert(path, key);
        // A write through a descriptor nothing had registered left its mtime
        // under the host inode.
        if key != host
            && let Some(mtime) = self.pending_mtimes.remove(&host)
        {
            self.set_mtime(key, mtime);
        }
        key
    }

    /// The key `raw_inode` is pooled under: its procfs key if it has one
    /// (see [`Self::procfs_key`]), otherwise itself.
    fn key(&self, raw_inode: RawInode) -> RawInode {
        self.procfs_keys
            .get(&raw_inode)
            .copied()
            .unwrap_or(raw_inode)
    }

    /// Counts a new guest thread with thread ID `tid`. A process or thread
    /// ID the kernel gives out again names a new task, and Linux drops the
    /// old task's procfs entries when it exits, so the new task's entries
    /// are new files (see [`Self::procfs_path_key`]).
    fn count_incarnation(&mut self, tid: i32) {
        self.threads_counted += 1;
        self.incarnations
            .entry(tid)
            .or_default()
            .push(self.threads_counted);
    }

    /// The path key of the procfs entry `name`: the key of its path, salted
    /// with the incarnation of each task the path names (see
    /// [`Self::count_incarnation`]), so that an entry of a task whose ID was
    /// reused does not adopt the number of the old task's entry, which a
    /// descriptor may still hold. An entry of a directory snapshot names the
    /// incarnation that held the ID when the snapshot was taken.
    fn procfs_path_key(&self, name: &ProcfsName) -> RawInode {
        name.tasks.iter().fold(name.path, |key, task| {
            let counted = self.incarnations.get(task).map_or(&[][..], Vec::as_slice);
            let incarnation = match name.threads_counted {
                Some(snapshot) => counted.partition_point(|at| *at <= snapshot),
                None => counted.len(),
            };
            match u32::try_from(incarnation).unwrap_or(u32::MAX) {
                0 => key,
                incarnation => procfs_inode::with_incarnation(key, *task, incarnation),
            }
        })
    }

    // Allocate the next deterministic inode.  This takes the raw-inode and
    // can return an existing mapping or extend the mapping by creating a
    // new deterministic inode. Minted numbers are strictly increasing and
    // never handed out twice.
    //
    // Host inode reuse: a filesystem such as ext4 or XFS gives a freed
    // `(dev, ino)` to a new file, and whether it does differs between runs
    // (https://github.com/rrnewton/hermit/issues/3840). A file's mapping, or
    // its pending mtime if nothing numbered it yet, is retired when the guest
    // removes its last name (`Self::retire`). A `Name` sighting of a retired
    // `(dev, ino)` discards it, so the new file is numbered by this request
    // in every run, reused or not, and inherits no mtime. A `Descriptor`
    // sighting still gets the retired number: an unlinked file reached through
    // a descriptor keeps its inode number, as on Linux, and a descriptor to a
    // new file was opened by name, which discarded the retired mapping
    // (`Self::forget_retired`).
    //
    // Still only contained, not handled: a file that never had a name
    // (`O_TMPFILE`, never linked) is never retired, nor is a file that
    // disappears without the guest removing its name (another process
    // deletes it, or its filesystem is unmounted). A named file the host
    // gives such a file's freed inode inherits its number and mtime.
    //
    // Every call consumes the next counter value, whether or not it mints.
    // Whether a host inode is "new" depends on host state the guest does not
    // control (a file replaced by another process, an inode the filesystem
    // reused, a hard link), while the sequence of requests is the guest's
    // own. Counting only mints let one such host-dependent sighting shift the
    // number of every file minted after it, on every device
    // (https://github.com/rrnewton/hermit/issues/2897); counting requests
    // confines the difference to the file whose host identity differed.
    // Numbers stay unique and increasing, just not dense.
    //
    // Also return the virtual mtime. When it is still unresolved, `observed`
    // resolves it under the first-seen policy (see `ObservedMtime`); an
    // unresolved mtime reads as `epoch`.
    fn add_sighted_inode(
        &mut self,
        raw_inode: RawInode,
        observed: ObservedMtime,
        epoch: LogicalTime,
        sighting: InodeSighting,
    ) -> (DetInode, LogicalTime) {
        let raw_inode = self.key(raw_inode);
        let ordinal = self.next_inode;
        self.next_inode += 1;
        match sighting {
            InodeSighting::Name => self.forget_retired(raw_inode),
            InodeSighting::Listed(snapshot) => {
                if let Some(answer) = self.listed_generation(raw_inode, snapshot, ordinal) {
                    let mtime = self
                        .detinodes_info
                        .get_mut(&answer)
                        .expect("Internal invariant broken, det_ino missing entry");
                    if mtime.mtime.is_none() {
                        mtime.mtime = observed.first_seen_mtime(epoch);
                    }
                    return (answer, mtime.mtime.unwrap_or(epoch));
                }
                if self
                    .retired_at
                    .get(&raw_inode)
                    .is_some_and(|retired| *retired < snapshot)
                {
                    self.forget_retired(raw_inode);
                }
            }
            InodeSighting::Descriptor => {}
        }
        let dino = match self
            .inodes
            .get(&raw_inode)
            .or_else(|| self.retired.get(&raw_inode))
        {
            Some(dino) => *dino,
            None => {
                // THE determinization boundary: the single place a host inode
                // is deliberately mapped to a deterministic one. The value is
                // minted from a monotonic counter, never derived from the host
                // inode's bits.
                let new = DetInode::mint(ordinal);
                // A file whose last name went away before it was numbered,
                // reached through a descriptor, is numbered as retired.
                let mtime = match self.retired_unnumbered.remove(&raw_inode) {
                    Some(mtime) => {
                        self.retired.insert(raw_inode, new);
                        if let Some(last) = self
                            .generations
                            .get_mut(&raw_inode)
                            .and_then(|generations| generations.last_mut())
                        {
                            last.number = Some(new);
                        }
                        mtime
                    }
                    None => {
                        assert!(self.inodes.insert(raw_inode, new).is_none());
                        self.generations
                            .entry(raw_inode)
                            .or_default()
                            .push(Generation {
                                number: Some(new),
                                retired: None,
                                pending: None,
                            });
                        self.pending_mtimes.remove(&raw_inode)
                    }
                };
                let prev = self.detinodes_info.insert(new, DetInodeInfo { mtime });
                assert!(prev.is_none()); // Should not have been previously used.
                new
            }
        };
        let info = self
            .detinodes_info
            .get_mut(&dino)
            .expect("Internal invariant broken, det_ino missing entry");
        if info.mtime.is_none() {
            info.mtime = observed.first_seen_mtime(epoch);
        }
        (dino, info.mtime.unwrap_or(epoch))
    }

    /// [`Self::add_sighted_inode`] for a [`InodeSighting::Descriptor`].
    fn add_inode(
        &mut self,
        raw_inode: RawInode,
        observed: ObservedMtime,
        epoch: LogicalTime,
    ) -> (DetInode, LogicalTime) {
        self.add_sighted_inode(raw_inode, observed, epoch, InodeSighting::Descriptor)
    }

    // Set a file's virtual mtime, replacing whatever the first-seen policy
    // chose or would choose. This never consumes a counter value: its callers
    // (a write's mtime bump, `utimensat`) reach the pool or not depending on
    // host state, such as whether the target's host identity held still
    // during the call, and a request counted here would shift the number of
    // every file numbered after it. A file not yet numbered keeps the mtime
    // pending until its first numbering request.
    fn set_mtime(&mut self, raw_inode: RawInode, mtime: LogicalTime) {
        let raw_inode = self.key(raw_inode);
        // A retired file is written through a descriptor opened before its
        // last name went away; a new file's descriptor was opened by name,
        // which discarded the retired mapping.
        match self
            .inodes
            .get(&raw_inode)
            .or_else(|| self.retired.get(&raw_inode))
        {
            Some(dino) => {
                self.detinodes_info
                    .get_mut(dino)
                    // TODO(T87258449): remove this `expect`:
                    .expect("Invariant violation: det inode missing from map.")
                    .mtime = Some(mtime);
            }
            None => match self.retired_unnumbered.get_mut(&raw_inode) {
                Some(retired) => {
                    *retired = Some(mtime);
                    // Also in its history record, which a directory snapshot
                    // consults once the host has reused the inode (claude
                    // re-check of https://github.com/rrnewton/hermit/pull/3849).
                    if let Some(last) = self
                        .generations
                        .get_mut(&raw_inode)
                        .and_then(|generations| generations.last_mut())
                    {
                        last.pending = Some(mtime);
                    }
                }
                None => {
                    self.pending_mtimes.insert(raw_inode, mtime);
                }
            },
        }
    }

    /// The file behind `raw_inode` lost its last name (an `unlink`,
    /// `rmdir` or replacing `rename` removed it). Its number stays for the
    /// descriptors that can still reach it, but a later [`InodeSighting::Name`]
    /// of `raw_inode` is another file. Consumes no counter value: whether a
    /// removal reaches here follows host state (its `st_nlink`), and must not
    /// shift other files' numbers.
    fn retire(&mut self, raw_inode: RawInode) {
        let raw_inode = self.key(raw_inode);
        let stamp = self.retirements;
        self.retired_at.insert(raw_inode, stamp);
        self.retirements += 1;
        let generations = self.generations.entry(raw_inode).or_default();
        match self.inodes.remove(&raw_inode) {
            Some(dino) => {
                self.retired.insert(raw_inode, dino);
                match generations.last_mut() {
                    Some(last) if last.number == Some(dino) && last.retired.is_none() => {
                        last.retired = Some(stamp);
                    }
                    _ => generations.push(Generation {
                        number: Some(dino),
                        retired: Some(stamp),
                        pending: None,
                    }),
                }
            }
            // Nothing numbered it yet, but a write may have left a pending
            // mtime, which must stay with this file (claude review of
            // https://github.com/rrnewton/hermit/pull/3849, P2-2).
            None => {
                let pending = self.pending_mtimes.remove(&raw_inode);
                self.retired_unnumbered.insert(raw_inode, pending);
                generations.push(Generation {
                    number: None,
                    retired: Some(stamp),
                    pending,
                });
            }
        }
    }

    /// The number of the file that held `raw_inode` when a directory snapshot
    /// was taken after `snapshot` retirements, if that file has since lost its
    /// last name. A file that held it then and was never numbered is numbered
    /// now, as `ordinal`. `None` when the inode's holder at the snapshot still
    /// holds it, or nothing ever retired one of its holders, so that the
    /// current file answers.
    ///
    /// A file held the inode from just after the previous holder's retirement
    /// (the host cannot reuse an inode before it is freed) through its own:
    /// the retirement with stamp `r` happened when the count was `r`, so a
    /// snapshot that saw it still named has `snapshot <= r`.
    fn listed_generation(
        &mut self,
        raw_inode: RawInode,
        snapshot: u64,
        ordinal: u64,
    ) -> Option<DetInode> {
        let generations = self.generations.get_mut(&raw_inode)?;
        // Holders are retired in order and the stamps only grow, and only the
        // last can still hold the inode, so the stamps are sorted. A snapshot
        // newer than the last retirement saw the current holder, the common
        // case, answered without a search; an older one finds its holder by
        // binary search: the first holder retired at or after the snapshot.
        // The previous holder was retired before the snapshot, so this one
        // held the inode then (codex re-check of
        // <https://github.com/rrnewton/hermit/pull/3849>: a scan from the
        // oldest made churn on one inode quadratic).
        let last_retired = match generations.last()?.retired {
            Some(retired) => Some(retired),
            None => generations.iter().rev().nth(1).and_then(|g| g.retired),
        };
        if last_retired.is_none_or(|retired| snapshot > retired) {
            return None;
        }
        #[cfg(test)]
        let mut probes = 0;
        let index = generations.partition_point(|generation| {
            #[cfg(test)]
            {
                probes += 1;
            }
            generation.retired.is_some_and(|retired| retired < snapshot)
        });
        #[cfg(test)]
        {
            self.listed_probes += probes;
        }
        // The holder at the snapshot still holds the inode: the current file
        // answers.
        generations.get(index)?.retired?;
        let current = index + 1 == generations.len();
        let generation = &mut generations[index];
        if let Some(number) = generation.number {
            return Some(number);
        }
        let new = DetInode::mint(ordinal);
        generation.number = Some(new);
        let mut mtime = generation.pending;
        // The latest holder, still retired and reached first by this entry, is
        // numbered as a descriptor would number it.
        if current && let Some(pending) = self.retired_unnumbered.remove(&raw_inode) {
            self.retired.insert(raw_inode, new);
            mtime = pending;
        }
        let prev = self.detinodes_info.insert(new, DetInodeInfo { mtime });
        assert!(prev.is_none()); // Should not have been previously used.
        Some(new)
    }

    /// `raw_inode` names a file that has a name, or that was just created
    /// (an `open` that found it linked, or `O_TMPFILE`), so a retired mapping
    /// of it described a file the host has freed: discard it. Consumes no
    /// counter value.
    fn forget_retired(&mut self, raw_inode: RawInode) {
        let raw_inode = self.key(raw_inode);
        self.retired_at.remove(&raw_inode);
        // Its number and mtime stay in `generations` and `detinodes_info` for
        // directory snapshots taken while it held the inode.
        if self.retired.remove(&raw_inode).is_some() {
            self.reused += 1;
        }
        if self.retired_unnumbered.remove(&raw_inode).is_some() {
            self.reused += 1;
        }
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1056): Deterministic remapping of device numbers (st_dev).
/// Deterministic remapping of device numbers (`st_dev`).
///
/// The kernel assigns anonymous block-device numbers to filesystems without a
/// backing block device (procfs, sysfs, tmpfs, devpts) from a global,
/// host-wide counter (`get_anon_bdev`). The raw `st_dev` a guest observes for
/// such a filesystem therefore drifts between otherwise-identical runs — and
/// even between the two runs of `--verify`, because the first run's mounts are
/// still live when the second run mounts fresh copies, so the second run's
/// procfs gets a different anonymous device number. That leaked host state into
/// a guest-visible `stat`/`statx` field.
///
/// We replace each distinct raw device number with a strictly-increasing
/// synthetic id assigned in first-observation order. Both `stat`/`statx` and a
/// virtualized mountinfo snapshot use this pool. A mountinfo read intentionally
/// pre-populates it in that snapshot's row order, and later metadata syscalls
/// reuse those assignments.
///
/// This guarantees identity consistency within one Detcore run. It is not an
/// unconditional cross-machine guarantee: hosts with different filesystem
/// layouts can expose different device equivalence classes or first-observation
/// order. The remapping still preserves distinctness and equality within the
/// run, so `find -xdev`, `du -x`, and `(st_dev, st_ino)` checks behave
/// consistently with the mountinfo device column.
#[derive(Debug)]
struct DevicePool {
    devices: HashMap<u64, u64>,
    next_device: u64,
}

/// Run-global guest-visible mount IDs, shared by `/proc/*/mountinfo` and
/// `/proc/*/fdinfo/*`.
///
/// Linux gives pseudo filesystems such as pipefs, sockfs, anon_inodefs, nsfs,
/// and pidfs their own mount IDs without listing those mounts in
/// `/proc/*/mountinfo`. The raw numbers are host-assigned. Preserve equality
/// and distinctness by keying on the raw mount ID, and number raw IDs in the
/// order the guest first observes them: an fdinfo read assigns its one ID, and
/// a mountinfo read assigns its not-yet-numbered row and parent IDs in row
/// order. The namespace's row order is kept only to validate later views.
///
/// Numbering must not depend on mounts the guest never observed. A run's mount
/// namespace starts as a copy of the host's, so its row count changes with
/// unrelated host activity, such as another process's loop mount under a
/// shared-propagation directory. Numbering by position in that table made the
/// fdinfo `mnt_id` of a pipe or of `/dev/null`, neither of which mountinfo
/// lists, differ between two runs of the same guest.
#[derive(Debug)]
enum MountIdPool {
    Uninitialized,
    Invalid,
    Ready {
        /// Raw mount ID to guest-visible mount ID, for every raw ID numbered
        /// so far. `mount_ids[&assignment_order[i]] == i + 1`.
        mount_ids: BTreeMap<u64, u64>,
        /// Raw mount IDs in the order they were numbered.
        assignment_order: Vec<u64>,
        /// The namespace's raw row/parent order. Validation only.
        mountinfo_order: Vec<u64>,
        allow_visible_subsets: bool,
    },
}

/// The raw identity orders observed by the producer and required to reconstruct
/// the same guest-visible mount IDs during replay.
pub struct MountIdentityProvenance {
    /// The namespace's raw mountinfo row/parent order, which later mountinfo
    /// views are validated against.
    pub mountinfo_order: Vec<u64>,
    /// Raw mount IDs in the order the run numbered them.
    pub assignment_order: Vec<u64>,
}

impl MountIdPool {
    fn from_config(mount_ids: &[u64], captured: bool, assignment_order: &[u64]) -> Self {
        if !captured {
            if !mount_ids.is_empty() || !assignment_order.is_empty() {
                return Self::Invalid;
            }
            return Self::Uninitialized;
        }
        Self::from_orders(mount_ids, assignment_order, true).unwrap_or(Self::Invalid)
    }

    fn from_orders(
        mount_ids: &[u64],
        assignment_order: &[u64],
        allow_visible_subsets: bool,
    ) -> Option<Self> {
        // A recorded assignment may include IDs mountinfo does not list, such
        // as pipefs. It may include raw 0 only if the namespace order names 0
        // as a row or parent: an fdinfo zero is the anonymous class and is
        // never numbered, and a mountinfo zero must pass order validation.
        if !detcore_model::procfs::mount_identity_provenance_is_well_formed(
            mount_ids,
            assignment_order,
        ) {
            return None;
        }
        let mut mappings = BTreeMap::new();
        for (index, raw) in assignment_order.iter().enumerate() {
            let virtual_mount_id = u64::try_from(index).ok()?.checked_add(1)?;
            if mappings.insert(*raw, virtual_mount_id).is_some() {
                return None;
            }
        }
        Some(Self::Ready {
            mount_ids: mappings,
            assignment_order: assignment_order.to_vec(),
            mountinfo_order: mount_ids.to_vec(),
            allow_visible_subsets,
        })
    }

    fn validate_mountinfo_order(&mut self, mountinfo_order: &[u64]) -> bool {
        if matches!(self, Self::Uninitialized) {
            *self = Self::from_orders(mountinfo_order, &[], false).unwrap_or(Self::Invalid);
        }
        let Self::Ready {
            mountinfo_order: expected,
            allow_visible_subsets,
            ..
        } = self
        else {
            return false;
        };
        if *allow_visible_subsets {
            mount_ids_are_ordered_subset(mountinfo_order, expected)
        } else {
            mountinfo_order == expected
        }
    }

    /// Return the guest-visible ID for `raw_mount_id`, numbering it next if
    /// this is its first observation. The pool must be `Ready`.
    fn assign(&mut self, raw_mount_id: u64) -> Option<u64> {
        let Self::Ready {
            mount_ids,
            assignment_order,
            ..
        } = self
        else {
            return None;
        };
        if let Some(virtual_mount_id) = mount_ids.get(&raw_mount_id) {
            return Some(*virtual_mount_id);
        }
        let virtual_mount_id = u64::try_from(assignment_order.len()).ok()?.checked_add(1)?;
        mount_ids.insert(raw_mount_id, virtual_mount_id);
        assignment_order.push(raw_mount_id);
        Some(virtual_mount_id)
    }

    fn determinize(&mut self, raw_mount_id: u64, mountinfo_order: Option<&[u64]>) -> Option<u64> {
        // Linux uses zero for anonymous objects such as memfd. It is one
        // equivalence class regardless of Detcore's descriptor classification.
        if raw_mount_id == 0 {
            return Some(0);
        }
        if let Some(order) = mountinfo_order {
            if !self.validate_mountinfo_order(order) {
                return None;
            }
        } else if matches!(self, Self::Uninitialized) {
            return None;
        }
        self.assign(raw_mount_id)
    }

    /// Validate one mountinfo snapshot and return the guest-visible ID of each
    /// entry of `observation_order`, numbering unseen entries in that order.
    ///
    /// `mountinfo_order` is the snapshot's canonical order, checked against
    /// the namespace order. `observation_order` is the same set of raw IDs in
    /// the order the view presents them (rows, then parent-only IDs). Only it
    /// decides numbering, so a captured namespace order that the guest cannot
    /// see never changes a guest-visible number.
    fn resolve_mountinfo(
        &mut self,
        mountinfo_order: &[u64],
        observation_order: &[u64],
    ) -> Option<Vec<u64>> {
        let entries = mountinfo_order.iter().copied().collect::<BTreeSet<_>>();
        let mut observed = BTreeSet::new();
        if entries.len() != mountinfo_order.len()
            || observation_order.len() != mountinfo_order.len()
            || !observation_order
                .iter()
                .all(|raw| entries.contains(raw) && observed.insert(*raw))
        {
            return None;
        }
        if !self.validate_mountinfo_order(mountinfo_order) {
            return None;
        }
        observation_order
            .iter()
            .map(|raw| self.assign(*raw))
            .collect()
    }

    fn provenance(&self) -> Result<Option<MountIdentityProvenance>, &'static str> {
        match self {
            Self::Uninitialized => Ok(None),
            Self::Invalid => Err("mount identity provenance is invalid"),
            Self::Ready {
                mountinfo_order,
                assignment_order,
                ..
            } => Ok(Some(MountIdentityProvenance {
                mountinfo_order: mountinfo_order.clone(),
                assignment_order: assignment_order.clone(),
            })),
        }
    }
}

impl Default for DevicePool {
    fn default() -> Self {
        DevicePool::new()
    }
}

impl DevicePool {
    fn new() -> Self {
        // Start at 1 so no file reports st_dev == 0, which some tools treat as
        // "no device".
        DevicePool {
            devices: HashMap::new(),
            next_device: 1,
        }
    }

    /// The pool of a run whose guest mount namespace is this process's: its
    /// procfs device reports Detcore's procfs device, as the procfs files
    /// Detcore renders do (<https://github.com/rrnewton/hermit/issues/4213>).
    /// Outside record and replay, the process holding the global state is in
    /// the guest's mount namespace on every backend. Replay's tracer is not,
    /// and its devices are the recording's, so record and replay number
    /// devices as before ([`Self::new`]).
    fn for_this_mount_namespace() -> Self {
        let mut pool = Self::new();
        match std::fs::metadata("/proc") {
            Ok(proc) => {
                pool.devices.insert(
                    std::os::unix::fs::MetadataExt::dev(&proc),
                    procfs_inode::VIRTUAL_PROC_DEVICE,
                );
            }
            Err(error) => warn!(
                "[detcore] /proc could not be read, so its device is numbered when first seen: {error}"
            ),
        }
        pool
    }

    /// Return the deterministic device id for `raw_device`, allocating a new one
    /// (in first-observation order) the first time a raw device is seen. The
    /// procfs device is never allocated to another device.
    fn determinize(&mut self, raw_device: u64) -> u64 {
        match self.devices.get(&raw_device) {
            Some(dev) => *dev,
            None => {
                if self.next_device == procfs_inode::VIRTUAL_PROC_DEVICE {
                    self.next_device += 1;
                }
                let new = self.next_device;
                self.next_device += 1;
                self.devices.insert(raw_device, new);
                new
            }
        }
    }
}

/// Global state associated with the detcore tool.
///
/// This is a singleton, and the one object of this type lives inside a central
/// address space, generally the "tracer" in a Reverie backend.
#[derive(Debug)]
pub struct GlobalState {
    sched: Arc<Mutex<Scheduler>>,
    #[cfg(feature = "physical-mm-test-support")]
    initial_mm_probe: Mutex<Option<Arc<initial_mm_probe::Probe>>>,

    inodes: Arc<Mutex<InodePool>>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping state.
    devices: Arc<Mutex<DevicePool>>,

    /// Shared fdinfo mount-ID equivalence classes for this Detcore run.
    mount_ids: Mutex<MountIdPool>,

    // next port to use if input port is 0
    next_port: AtomicU16,

    // used ports
    used_ports: Mutex<HashSet<u16>>,

    // Unsupported syscall names observed across every process in this run.
    unsupported_syscalls: Mutex<BTreeSet<String>>,

    // Signals whose requested disposition Detcore did not install, already
    // announced in this run (`GlobalRequest::AnnounceSignalHandlerNotInstalled`).
    announced_handlers_not_installed: Mutex<BTreeSet<i32>>,
    /// The signal controls an in-guest runtime refused that this run has
    /// announced (`GlobalRequest::AnnounceRefusedSignalControls`): the signals,
    /// bit `n - 1` for signal `n`, and whether an alternate stack.
    announced_refused_signal_controls: Mutex<(u64, bool)>,

    // Why the fail-closed policy ended the run, from the Tool image that
    // refused (`GlobalRequest::PolicyRefusal`); the first report is kept.
    policy_refusal: Mutex<Option<String>>,

    // The host identity of each file the guest opened in this run, in the
    // order the opens were reported, when `Config::record_host_inputs` is set
    // (see `detcore_model::host_input`). Written to `Config::host_input_log`
    // when the run ends normally; dropped with this state otherwise.
    host_inputs: Mutex<Vec<HostInputRecord>>,

    // The paths the guest itself rebound in its file namespace in this run,
    // when `Config::record_host_inputs` is set (see
    // `detcore_model::host_input::HostMutationRecord`). Written beside
    // `host_inputs`.
    host_mutations: Mutex<BTreeSet<String>>,

    // The canonical models of open file descriptions that several guest
    // processes share (see `crate::shared_open_files`). Locked after `sched`,
    // never before it.
    shared_open_files: Arc<Mutex<SharedOpenFileStore>>,

    // Optional append-only sink shared by DBT fork descendants.
    unsupported_syscall_report_fd: Option<Mutex<File>>,

    // Open file description to bound port.
    open_file_to_port: Mutex<HashMap<OpenFileId, u16>>,

    port_start_range: AtomicU16,
    port_end_range: AtomicU16,

    // False initially after fork, and true when we begin executing the guest binary.
    past_first_execve: AtomicBool,
    /// Unlike the logging flag above, this changes only at successful image completion.
    initial_descriptor_image_complete: AtomicBool,
    /// Actual controller input, held only until the registered root image
    /// consumes it. The lease keeps its raw identity live, not guest-visible.
    inherited_input: Mutex<Option<PendingInheritedInput>>,
    // Installed only on the external controller by a contained launcher.
    mapping_capture_run: Option<reverie::MappingCaptureRun>,
    mapping_capture_pending: Mutex<Option<mapping_capture::Pending>>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1154): Review the SaBRe exec descriptor-status handoff.
    /// Pre-exec identity and descriptor state awaiting a successful image transition.
    // TODO-HUMAN-REVIEW(PR-1173): Review SaBRe exec incarnation fencing.
    pending_exec_states: Arc<Mutex<BTreeMap<DetPid, PendingExecState>>>,
    // Installed only by the trusted watched native launcher, never by an RPC.
    native_exec_registry: OnceLock<Mutex<native_exec::NativeExecRegistry>>,
    /// Successful identity transfers whose RPC acknowledgment may still be in flight.
    /// Nesting either exec map with global_time requires holding sched first;
    /// the scheduler mutex serializes their inner lock orders. Process
    /// cleanup also holds sched before acquiring these stores.
    completed_exec_transfers: Arc<Mutex<BTreeMap<DetPid, exec_identity::ExecTransferReceipt>>>,

    /// Descriptor state retained after the one-shot scheduler transition is consumed.
    post_exec_fd_blocking: Arc<Mutex<BTreeMap<DetTid, ExecFdState>>>,

    /// The image each guest process executes, as its last successful exec
    /// recorded and the kernel's link confirmed (`crate::exec_record`), for
    /// another process's `/proc/<pid>/exe`. A new process starts with its
    /// creator's; the record goes with the process.
    exec_images: Arc<Mutex<BTreeMap<DetPid, crate::exec_record::ExecRecord>>>,
    /// The auxiliary vector of each guest process's current image, for its
    /// `/proc/<pid>/auxv` ([`ProcessAuxvTable`]).
    process_auxv: Arc<Mutex<ProcessAuxvTable>>,
    /// Which guest descriptor tables reference each open file description
    /// whose OFD or flock locks Detcore has read, and which locks it holds,
    /// so that `/proc/locks` keeps exactly the guest's own rows
    /// (`procfs::locks::LockLedger`). Locked after `sched`, never before it.
    lock_ledger: Mutex<crate::procfs::locks::LockLedger>,
    /// The registered root thread, whose first image
    /// `Config::launch_image` names, and whether that image was completed.
    root_dettid: OnceLock<DetTid>,
    root_image_marked: AtomicBool,
    /// The device and inode of `Config::launch_image`'s launcher, read once.
    launcher_identity: OnceLock<Option<(u64, u64)>>,

    sched_handle: Option<tokio::task::JoinHandle<()>>,

    /// Global time is a *volatile* vector clock of individual thread progress. Each
    /// thread can independently update its own progress, even (potentially) asynchronously.
    ///
    /// LockOrdering: this lock can be acquired while holding the sched lock (but not vice
    /// versa).
    //
    // TODO: it would be more future-proof to provide a non-blocking way to retrieve a
    // (nondeterministic) monotonic lower bound on global time.
    global_time: Arc<Mutex<GlobalTime>>,

    /// Just cache the config so we can access it from everywhere.
    cfg: Config,

    /// Storage for the preemption record read from `replay_preemptions_from`.
    preemptions_to_replay: Option<HistoryReader>,

    /// The start is when we construct the global state.  Close enough.
    realtime_start: SystemTime,

    /// External network record or replay state, present only when a network
    /// trace mode is configured. Every operation reads global time under this
    /// lock, so an input's release point is fixed by the schedule alone.
    network: Option<Mutex<NetworkEngine>>,
}

/// The stores a process's physical exit must clean up, shared with the
/// GlobalState that owns them, so that the cleanup can run as one step under
/// the scheduler lock. Locked after `sched`, never before it, as every other
/// nested user of these stores does.
#[derive(Clone, Debug, Default)]
struct ProcessExitCleanup {
    shared_open_files: Arc<Mutex<SharedOpenFileStore>>,
    pending_exec_states: Arc<Mutex<BTreeMap<DetPid, PendingExecState>>>,
    completed_exec_transfers: Arc<Mutex<BTreeMap<DetPid, exec_identity::ExecTransferReceipt>>>,
    post_exec_fd_blocking: Arc<Mutex<BTreeMap<DetTid, ExecFdState>>>,
    exec_images: Arc<Mutex<BTreeMap<DetPid, crate::exec_record::ExecRecord>>>,
    process_auxv: Arc<Mutex<ProcessAuxvTable>>,
}

/// What `/proc/<pid>/auxv` reads for each guest process, on every backend
/// that reports an initial stack pointer (package K slice K2,
/// <https://github.com/rrnewton/hermit/issues/4213>): the auxiliary vector its
/// current image found on its initial stack when it was exec'd
/// (`Detcore::record_process_auxv`), as Linux's `saved_auxv`. Another
/// process's access to it is decided by the kernel beneath at the open
/// (`Detcore::ptrace_read_allowed`), so nothing about access is kept here.
///
/// One entry per live process, at most `procfs::MAX_AUXV_BYTES` (Linux's
/// `saved_auxv`, 52 words), forgotten at the process's exit. A descriptor
/// keeps the snapshot its open took, through any later exec or exit.
///
/// Not kept: the vectors of earlier images for a descriptor that a backend
/// rebuilds from its placeholder's name after an exec. Only backends that
/// rebuild descriptors that way (DBT, SaBRe, LiteInst) would need them, and
/// none of them reports an initial stack pointer, so none renders this file.
/// That path returns, with end-to-end tests, when DBT implements
/// `Guest::initial_stack_pointer`.
///
/// It also keeps the other fact a `/proc/<pid>` file Detcore renders depends
/// on: which tasks have a changed root, whose `mountinfo` and `mounts` are
/// then the kernel's view, rooted where the task is (K3a of
/// <https://github.com/rrnewton/hermit/issues/4213>). Linux keeps a task's
/// root in its `fs_struct`, which `clone(CLONE_FS)` shares between creator
/// and child (thread or process) and which a clone without it copies, so the
/// table keeps each task's filesystem group the same way ([`FsRoots`]).
#[derive(Debug, Default)]
pub(crate) struct ProcessAuxvTable {
    /// Each live process's current image's auxiliary vector.
    processes: BTreeMap<DetPid, Vec<u8>>,
    /// Each task's filesystem group, and the groups whose root changed.
    roots: FsRoots,
    /// Whether this run has warned that a backend reports no initial stack
    /// pointer, and that an initial stack held no plausible vector
    /// ([`ProcessAuxvTable::first_kernel_file`]).
    kernel_file_warned: [bool; 2],
}

/// Each task's filesystem group: the tasks that share one Linux `fs_struct`,
/// and so one root. A task is in its creator's group when it was created with
/// `CLONE_FS`, and in a new group, starting from a copy of its creator's
/// root, otherwise. `unshare(CLONE_FS)` would detach a task too, but Detcore
/// refuses every `unshare` (EPERM), so no guest task does. A task Detcore has
/// not seen created (none in practice) has an unchanged root.
#[derive(Debug, Default)]
struct FsRoots {
    /// Each task's group, by its thread number.
    groups: BTreeMap<DetPid, u64>,
    /// The groups whose root changed.
    changed: BTreeSet<u64>,
    /// The next group's number.
    next: u64,
}

impl FsRoots {
    fn new_group(&mut self, changed: bool) -> u64 {
        let group = self.next;
        self.next += 1;
        if changed {
            self.changed.insert(group);
        }
        group
    }

    fn clone_task(&mut self, parent: DetPid, child: DetPid, shares_fs: bool) {
        let group = match self.groups.get(&parent).copied() {
            Some(group) if shares_fs => group,
            parent_group => {
                let changed = parent_group.is_some_and(|group| self.changed.contains(&group));
                let group = self.new_group(changed);
                if shares_fs {
                    // A creator Detcore had not seen created shares the new
                    // group from now on.
                    self.groups.insert(parent, group);
                }
                group
            }
        };
        self.groups.insert(child, group);
    }

    fn mark_changed(&mut self, task: DetPid) {
        let group = match self.groups.get(&task).copied() {
            Some(group) => group,
            None => {
                let group = self.new_group(false);
                self.groups.insert(task, group);
                group
            }
        };
        self.changed.insert(group);
    }

    fn changed(&self, task: DetPid) -> bool {
        self.groups
            .get(&task)
            .is_some_and(|group| self.changed.contains(group))
    }

    fn transfer(&mut self, former: DetPid, current: DetPid) {
        if let Some(group) = self.groups.remove(&former) {
            self.groups.insert(current, group);
        }
    }

    fn forget(&mut self, task: DetPid) {
        if let Some(group) = self.groups.remove(&task)
            && !self.groups.values().any(|other| *other == group)
        {
            self.changed.remove(&group);
        }
    }
}

/// A process's `/proc/<pid>/auxv` ([`ProcessAuxvTable::current`]).
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub struct ProcessAuxv {
    /// The process, which a thread's number names too.
    pub process: DetPid,
    /// Its current image's auxiliary vector.
    pub bytes: Vec<u8>,
}

/// Why an image's `/proc/<pid>/auxv` is left to the kernel beneath
/// ([`ProcessAuxvTable::first_kernel_file`]).
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum AuxvFromKernel {
    /// The backend reports no initial stack pointer.
    NoInitialStackPointer,
    /// The initial stack held no plausible auxiliary vector.
    Implausible,
}

impl ProcessAuxvTable {
    /// Record `bytes`, the auxiliary vector `process`'s new image found on its
    /// initial stack, as its current vector.
    pub(crate) fn record(&mut self, process: DetPid, bytes: Vec<u8>) {
        self.processes.insert(process, bytes);
    }

    /// `process`'s current image's auxiliary vector.
    pub(crate) fn current(&self, process: DetPid) -> Option<ProcessAuxv> {
        Some(ProcessAuxv {
            process,
            bytes: self.processes.get(&process)?.clone(),
        })
    }

    /// A new process `child` runs its creator `parent`'s image until it
    /// execs.
    pub(crate) fn inherit(&mut self, parent: DetPid, child: DetPid) {
        match self.processes.get(&parent).cloned() {
            Some(bytes) => self.processes.insert(child, bytes),
            None => self.processes.remove(&child),
        };
    }

    /// A new task `child` of `parent`: it shares `parent`'s filesystem group
    /// (its root) with `CLONE_FS`, and starts a group of its own with a copy
    /// of `parent`'s root without it, as Linux's `copy_fs` does.
    pub(crate) fn clone_root(&mut self, parent: DetPid, child: DetPid, shares_fs: bool) {
        self.roots.clone_task(parent, child, shares_fs);
    }

    /// `task` changed its root (a successful `chroot`): so did every task in
    /// its filesystem group.
    pub(crate) fn mark_changed_root(&mut self, task: DetPid) {
        self.roots.mark_changed(task);
    }

    /// Whether `task`'s root has changed.
    pub(crate) fn root_changed(&self, task: DetPid) -> bool {
        self.roots.changed(task)
    }

    /// A successful exec by the non-leader thread `former` gave it its
    /// process's number `current` (Linux's `de_thread`); its filesystem group
    /// goes with it. An exec does not detach a shared filesystem group:
    /// Linux's `check_unsafe_exec` only marks such an exec unsafe for set-id
    /// programs, and `begin_new_exec` unshares the descriptor table, not
    /// `fs_struct`.
    pub(crate) fn transfer_root(&mut self, former: DetPid, current: DetPid) {
        self.roots.transfer(former, current);
    }

    /// `process` is gone, or its new image has no vector Detcore recorded
    /// (whatever it held before the exec, its own earlier image's or its
    /// creator's, is stale), so its file is the kernel's. Descriptors already
    /// open keep their snapshots.
    pub(crate) fn forget(&mut self, process: DetPid) {
        self.processes.remove(&process);
    }

    /// `process` is gone: none of its facts stay.
    pub(crate) fn forget_process(&mut self, process: DetPid) {
        self.processes.remove(&process);
        self.roots.forget(process);
    }

    /// How many processes have a current vector.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.processes.len()
    }

    /// Whether this is the run's first image whose `/proc/<pid>/auxv` is left
    /// to the kernel beneath for `why`, which the run warns of once.
    pub(crate) fn first_kernel_file(&mut self, why: AuxvFromKernel) -> bool {
        let index = match why {
            AuxvFromKernel::NoInitialStackPointer => 0,
            AuxvFromKernel::Implausible => 1,
        };
        !std::mem::replace(&mut self.kernel_file_warned[index], true)
    }
}

impl ProcessExitCleanup {
    /// Complete the exit `watch` observed, unless a later registration has
    /// replaced it. Nothing calls this in production yet; the daemon-point
    /// observation of <https://github.com/rrnewton/hermit/issues/4362> part 2
    /// will (2ee7190125's free-running observer was reverted).
    #[cfg_attr(not(test), allow(dead_code))]
    fn complete_observed_locked(
        &self,
        sched: &mut Scheduler,
        watch: &Arc<crate::scheduler::ProcessExitWatch>,
    ) -> Option<(bool, Option<String>)> {
        if !sched.process_exit_watch_is_current(watch) {
            return None;
        }
        sched.finish_process_exit_watch(watch);
        Some(self.complete_locked(sched, watch.process, true))
    }

    fn complete_locked(
        &self,
        sched: &mut Scheduler,
        process: DetPid,
        consume_unreported: bool,
    ) -> (bool, Option<String>) {
        let loss = (consume_unreported && sched.has_unreported_exit(process)).then(|| {
            let reason = format!(
                "process {process} exited without deregistering; its exit was not scheduled"
            );
            // Publish refusal before cancellation can wake a request. Normal
            // deregistration and this entire operation share the sched lock.
            crate::detlog::record_determinism_loss(&reason);
            reason
        });
        self.pending_exec_states.lock().unwrap().remove(&process);
        self.completed_exec_transfers
            .lock()
            .unwrap()
            .remove(&process);
        self.post_exec_fd_blocking.lock().unwrap().remove(&process);
        self.exec_images.lock().unwrap().remove(&process);
        self.process_auxv.lock().unwrap().forget_process(process);
        if consume_unreported {
            let consumed = sched.consume_unreported_exit(process);
            debug_assert_eq!(consumed, loss.is_some());
        }
        self.shared_open_files
            .lock()
            .unwrap()
            .end_leases_of(process);
        (sched.complete_physical_process_exit(process), loss)
    }
}

impl Default for GlobalState {
    fn default() -> Self {
        // TODO(T77816673): eventually we want to remove this requirement.
        // In the meantime... just don't call this.
        panic!("Detcore GlobalState Default impl should not be called");
    }
}

impl Drop for GlobalState {
    fn drop(&mut self) {
        // TODO-HUMAN-REVIEW(PR-643): Review shutdown-time aggregate warning delivery.
        if let Some(message) =
            format_unsupported_syscall_warning(&self.unsupported_syscalls.lock().unwrap())
        {
            warn!("{}", message);
        }
        info!("detcore shut down, destroying global state");
    }
}

impl GlobalState {
    /// Transfer the completed run's diagnostic aggregate to its output owner.
    /// The backend must first reclaim exclusive ownership after all RPC clients
    /// have drained. Callers that do not take it retain the ordinary Drop warning.
    pub fn take_unsupported_syscalls(&mut self) -> BTreeSet<String> {
        std::mem::take(self.unsupported_syscalls.get_mut().unwrap())
    }

    /// Ordinary RPC mutation must linearize before terminal publication under
    /// the same mutex as scheduler grants. A losing callback stays pending for
    /// the backend's failure subscription to drop; no normal reply is invented.
    /// Consuming exit RPCs retain their existing validation/accounting path.
    async fn lock_rpc_scheduler(
        &self,
        consuming_cleanup: bool,
    ) -> std::sync::MutexGuard<'_, Scheduler> {
        std::future::poll_fn(|_| {
            let sched = self.sched.lock().unwrap();
            if !consuming_cleanup && sched.backend_failed() {
                Poll::Pending
            } else {
                Poll::Ready(sched)
            }
        })
        .await
    }

    /// The deterministic numbers of the files whose last name the run removed
    /// while they were numbered and that no later name showed to be reused
    /// (see `InodePool::retire`), in increasing order. Diagnostic, for tests
    /// of the removal paths (<https://github.com/rrnewton/hermit/issues/3840>).
    pub fn retired_inodes(&self) -> Vec<DetInode> {
        let mut retired: Vec<DetInode> = self
            .inodes
            .lock()
            .unwrap()
            .retired
            .values()
            .copied()
            .collect();
        retired.sort();
        retired
    }

    /// How many retired inode numbers a later name or `open` showed the host
    /// had given to another file. Diagnostic, for tests that make a host reuse
    /// an inode (<https://github.com/rrnewton/hermit/issues/3840>).
    pub fn reused_inode_count(&self) -> u64 {
        self.inodes.lock().unwrap().reused
    }

    /// Return the producer-observed mount identity order after a run.
    ///
    /// The first vector is the exact mountinfo row/parent order. The second is
    /// the order in which the run numbered raw mount IDs.
    pub fn mount_identity_provenance(
        &self,
    ) -> Result<Option<MountIdentityProvenance>, &'static str> {
        self.mount_ids.lock().unwrap().provenance()
    }

    /// A global state with no scheduler task, for unit tests elsewhere in the
    /// crate that send RPCs to it directly.
    #[cfg(test)]
    pub(crate) fn for_tests(cfg: &Config) -> Self {
        Self::initialize(cfg, false)
    }

    fn initialize(cfg: &Config, spawn_scheduler: bool) -> Self {
        let sched = Arc::new(Mutex::new(Scheduler::new(cfg)));
        let global_time = Arc::new(Mutex::new(GlobalTime::new(cfg)));
        let process_exit_cleanup = ProcessExitCleanup::default();
        let handle = if cfg.sequentialize_threads && spawn_scheduler {
            // Announce before spawning, not from inside the spawned task. The
            // task's first poll is unordered with respect to the rest of this
            // bootstrap, so emitting there raced the root thread's seeding
            // lines and produced a nondeterministic INFO stream. Emitting here
            // sequences it: `Scheduler::new`'s SCHEDRAND line above, then this,
            // then the root `ThreadState::new` seeding lines.
            info!("[scheduler] daemon task starting up, waiting for guest thread start..");
            Some(tokio::spawn(sched_loop(sched.clone(), global_time.clone())))
        } else {
            None
        };

        let preemptions_to_replay: Option<HistoryReader> =
            cfg.replay_preemptions_from.as_ref().map(|path| {
                HistoryReader::read(path)
                    .unwrap_or_else(|error| crate::scheduler::exit_on_scheduler_refusal(&error))
            });
        let range = Self::read_port_range();
        let network = Self::network_engine(cfg).map(Mutex::new);

        let unsupported_syscall_report_fd = cfg.unsupported_syscall_report_fd.and_then(|fd| {
            // This writer is internal controller state. In an in-process DBT
            // runtime it must not leak into the next guest image across exec
            // (hence F_DUPFD_CLOEXEC), and it must not perturb the descriptor
            // namespace the *current* guest observes. The backend places the
            // report fd itself high, out of the guest's working range (e.g. 199
            // for the DBT backend). Duplicating with a min hint of `fd` keeps
            // this private copy up in that same reserved band instead of
            // grabbing the lowest free descriptor (fd 3), which would shift
            // every fd the guest subsequently opens and diverge from the golden
            // ptrace reference (where this fd is unset and no dup happens).
            let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, fd) };
            if duplicate == -1 {
                warn!(
                    "failed to duplicate unsupported-syscall report fd {fd}: {}",
                    std::io::Error::last_os_error()
                );
                None
            } else {
                // SAFETY: dup returned a new owned descriptor.
                Some(Mutex::new(unsafe { File::from_raw_fd(duplicate) }))
            }
        });

        Self {
            sched,
            #[cfg(feature = "physical-mm-test-support")]
            initial_mm_probe: Mutex::new(None),
            next_port: AtomicU16::new(range[0]),
            used_ports: Mutex::new(HashSet::new()),
            unsupported_syscalls: Mutex::new(BTreeSet::new()),
            announced_handlers_not_installed: Mutex::new(BTreeSet::new()),
            announced_refused_signal_controls: Mutex::new((0, false)),
            policy_refusal: Mutex::new(None),
            host_inputs: Mutex::new(Vec::new()),
            host_mutations: Mutex::new(BTreeSet::new()),
            shared_open_files: process_exit_cleanup.shared_open_files,
            unsupported_syscall_report_fd,
            port_start_range: AtomicU16::new(range[0]),
            port_end_range: AtomicU16::new(range[1]),
            open_file_to_port: Mutex::new(HashMap::new()),
            past_first_execve: AtomicBool::new(false),
            initial_descriptor_image_complete: AtomicBool::new(false),
            inherited_input: Mutex::new(None),
            mapping_capture_run: None,
            mapping_capture_pending: Mutex::new(None),
            pending_exec_states: process_exit_cleanup.pending_exec_states,
            completed_exec_transfers: process_exit_cleanup.completed_exec_transfers,
            post_exec_fd_blocking: process_exit_cleanup.post_exec_fd_blocking,
            exec_images: process_exit_cleanup.exec_images,
            process_auxv: process_exit_cleanup.process_auxv,
            lock_ledger: Mutex::new(Default::default()),
            root_dettid: OnceLock::new(),
            root_image_marked: AtomicBool::new(false),
            launcher_identity: OnceLock::new(),
            native_exec_registry: OnceLock::new(),
            inodes: Arc::new(Mutex::new(InodePool::new())),
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping state.
            devices: Arc::new(Mutex::new(if cfg.recordreplay_modes {
                DevicePool::new()
            } else {
                DevicePool::for_this_mount_namespace()
            })),
            mount_ids: Mutex::new(MountIdPool::from_config(
                &cfg.mountinfo_mount_ids,
                cfg.mountinfo_mount_ids_captured,
                &cfg.mount_id_assignment_order,
            )),
            sched_handle: handle,
            cfg: cfg.clone(),
            realtime_start: SystemTime::now(),
            global_time,
            preemptions_to_replay,
            network,
        }
    }

    /// The network engine for the configured trace mode. The CLI validates a
    /// replay trace before launch; failing here is a backstop.
    fn network_engine(cfg: &Config) -> Option<NetworkEngine> {
        let path = cfg.network_trace.path.as_ref();
        match cfg.network_trace.mode {
            NetworkTraceMode::Off => None,
            NetworkTraceMode::Record => Some(NetworkEngine::new_record(cfg.epoch)),
            NetworkTraceMode::Replay => {
                let path = path.expect("network trace replay requires a trace path");
                let trace = File::open(path)
                    .map_err(|error| error.to_string())
                    .and_then(|mut file| {
                        NetworkTraceV2::read_framed(&mut file).map_err(|error| error.to_string())
                    })
                    .unwrap_or_else(|error| {
                        panic!("cannot read network trace {}: {error}", path.display())
                    });
                Some(NetworkEngine::new_replay(trace))
            }
        }
    }

    /// Write the network trace of a record run, or check that a replay used
    /// the whole recording. Returns the refusal a replay ends with.
    fn finish_network_trace(&mut self) -> Option<String> {
        let engine = self.network.take()?;
        let engine = engine.into_inner().unwrap();
        if !engine.is_record() {
            return engine
                .finish_replay()
                .err()
                .map(|error| format!("hermit: {error}. {}", error.remedy()));
        }
        let path = self
            .cfg
            .network_trace
            .path
            .as_ref()
            .expect("network trace recording requires a trace path");
        // Encode before opening the output, so a trace that cannot be encoded
        // leaves the file as it was instead of truncated.
        let result = engine
            .finish()
            .map_err(|error| error.to_string())
            .and_then(|trace| trace.encode_framed().map_err(|error| error.to_string()))
            .and_then(|bytes| {
                let mut file = File::create(path).map_err(|error| error.to_string())?;
                file.write_all(&bytes).map_err(|error| error.to_string())
            });
        if let Err(error) = result {
            panic!("cannot write network trace {}: {error}", path.display());
        }
        None
    }

    /// Initializes global state whose sequential scheduler is driven by an
    /// external backend executor.
    pub fn init_for_external_scheduler(cfg: &Config) -> Self {
        assert!(
            cfg.sequentialize_threads,
            "an external scheduler is only meaningful when threads are sequentialized"
        );
        Self::initialize(cfg, false)
    }

    /// Returns the sequential scheduler loop for a backend-owned executor to run.
    ///
    /// This is deliberately not an `async fn`: the announcement is logged when
    /// this is called, not when the returned future is first polled. A backend
    /// can then order it ahead of anything it releases before polling (DBT
    /// publishes `READY_IMAGE`, which lets the root guest thread log its
    /// seeding lines). See <https://github.com/rrnewton/hermit/issues/3463>.
    pub fn run_external_scheduler(
        &self,
        observer: Arc<dyn Fn(&'static str) + Send + Sync>,
    ) -> impl Future<Output = ()> + use<> {
        // Emitted at the call site for the same reason as the spawned path in
        // `initialize`, so both ways of starting the daemon place this line at
        // a deterministic point in the caller's program order.
        info!("[scheduler] daemon task starting up, waiting for guest thread start..");
        sched_loop_external(self.sched.clone(), self.global_time.clone(), observer)
    }

    /// Reports that a backend supervisor received a process's final kernel exit status.
    ///
    /// This records a barrier observation when the backend advertises physical-exit
    /// reporting or asynchronous exit completion; it is a no-op for ptrace, DBT and KVM.
    /// The exact process's barrier is released at this physical-waitability boundary.
    ///
    /// With asynchronous exit completion, registrations the process did not deregister
    /// itself are retired here first, in the same locked step, and a determinism loss is
    /// recorded: such a process died without finishing its exit hook, at a moment the host
    /// chose, so the run must not be compared.
    pub fn complete_physical_process_exit(&self, raw_pid: i32) {
        let detpid = DetPid::from_raw(raw_pid);
        let (completed, loss) = {
            let mut sched = self.sched.lock().unwrap();
            self.process_exit_cleanup().complete_locked(
                &mut sched,
                detpid,
                self.cfg.backend.process_exits_complete_asynchronously,
            )
        };
        if completed {
            trace!(
                "[detcore, dpid {}] backend completed final physical process exit",
                detpid
            );
        }
        if let Some(loss) = loss {
            crate::detlog::write_loss_notice(&loss);
        }
    }

    fn process_exit_cleanup(&self) -> ProcessExitCleanup {
        ProcessExitCleanup {
            shared_open_files: self.shared_open_files.clone(),
            pending_exec_states: self.pending_exec_states.clone(),
            completed_exec_transfers: self.completed_exec_transfers.clone(),
            post_exec_fd_blocking: self.post_exec_fd_blocking.clone(),
            exec_images: self.exec_images.clone(),
            process_auxv: self.process_auxv.clone(),
        }
    }

    /// The device and inode of the launcher `Config::launch_image` names,
    /// the file the kernel's `exe` link names in a launched guest's place.
    fn launcher_identity(&self) -> Option<(u64, u64)> {
        *self.launcher_identity.get_or_init(|| {
            use std::os::unix::fs::MetadataExt;
            let launcher = self.cfg.launch_image.as_ref()?.launcher.as_ref()?;
            let meta = std::fs::metadata(launcher).ok()?;
            Some((meta.dev(), meta.ino()))
        })
    }

    /// Answers a control message about a shared open file description. Only
    /// the thread that holds the scheduler's serial grant may send one, so
    /// access to a shared description is exclusive and happens in canonical
    /// order (`crate::shared_open_files`).
    ///
    /// A refusal is an infrastructure failure, not something the guest's
    /// syscall can report: the caller's process stops, and this reports a
    /// backend failure, which ends the whole run before any further grant.
    fn shared_open_file_control(
        &self,
        thread: DetTid,
        control: OpenFileControl,
    ) -> Result<OpenFileControlReply, SharedOpenFileError> {
        // The refusal and the failure it reports are one locked step, so the
        // scheduler can grant no other thread in between.
        let (result, wakes) = {
            let mut sched = self.sched.lock().unwrap();
            let process = sched.registered_process(thread);
            let result = if !sched.holds_serial_grant(thread) {
                Err(SharedOpenFileError(format!(
                    "thread {thread} sent {control:?} without holding the serial grant"
                )))
            } else if let Some(process) = process {
                self.shared_open_files
                    .lock()
                    .unwrap()
                    .handle(process, thread, control)
            } else {
                Err(SharedOpenFileError(format!(
                    "thread {thread} belongs to no registered process"
                )))
            };
            let wakes = result.as_ref().err().map(|error| {
                tracing::error!("[detcore] shared open file control refused: {error}");
                let wake = sched.report_backend_failure(reverie::BackendFailure {
                    pid: reverie::Pid::from_raw(process.unwrap_or(thread).as_raw()),
                    tid: Tid::from_raw(thread.as_raw()),
                    phase: "shared open file control",
                });
                (wake, sched.take_signal_failure_wakes())
            });
            (result, wakes)
        };
        // Notified after the lock is released, as `report_backend_failure`
        // does.
        if let Some((wake, deferred)) = wakes {
            for wake in deferred {
                let _ = wake.send(());
            }
            if let Some(wake) = wake {
                let _ = wake.send(());
            }
        }
        result
    }

    /// Releases all physical-process-exit barriers after a backend supervisor has drained every
    /// tracee and no guest thread can race another lifecycle event.
    pub fn release_all_physical_process_exits(&self) {
        self.pending_exec_states.lock().unwrap().clear();
        self.completed_exec_transfers.lock().unwrap().clear();
        self.post_exec_fd_blocking.lock().unwrap().clear();
        self.exec_images.lock().unwrap().clear();
        *self.process_auxv.lock().unwrap() = ProcessAuxvTable::default();
        let released = self
            .sched
            .lock()
            .unwrap()
            .release_all_physical_process_exits();
        if released != 0 {
            trace!("released {released} final physical process-exit barrier(s)");
        }
    }

    /// Why the fail-closed policy ended the run, when a Tool image reported
    /// it before exiting (`GlobalRequest::PolicyRefusal`).
    pub fn policy_refusal(&self) -> Option<String> {
        self.policy_refusal.lock().unwrap().clone()
    }

    /// Unrecoverable fatal erorr. Bring things to a close cleanly, but as quickly as
    /// possible.
    pub fn force_shutdown_with_error(&self) {
        let start = std::time::Instant::now();
        let sched = loop {
            if start.elapsed().as_millis() > 1000 {
                eprintln!(
                    "Could not acquire scheduler lock during forced shutdown (timeout)... proceeding anyway."
                );
                return;
            }
            match self.sched.try_lock() {
                Ok(guard) => {
                    break guard;
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::yield_now();
                    continue;
                }
                Err(e) => {
                    eprintln!(
                        "Could not acquire scheduler lock during forced shutdown ({})... proceeding anyway.",
                        e
                    );
                    return;
                }
            }
        };
        info!("Scheduler state at exit:\n{}", sched.full_summary());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-744): Review explicit abnormal-backend scheduler cancellation.
    /// Cancels the internally spawned scheduler task after a backend guest exits abnormally.
    ///
    /// External-scheduler states do not own a task and are left unchanged.
    pub async fn cancel_internal_scheduler(&mut self) {
        if let Some(handle) = self.sched_handle.take() {
            handle.abort();
            match handle.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => panic!("cancelled scheduler task panicked: {error}"),
            }
        }
    }

    /// Consume failed-run state after the scheduler has naturally finished.
    ///
    /// A failed run has no successful run summary. Preserve the scheduler's
    /// join error and any requested partial preemption recording's write error
    /// for the caller, without allowing either to replace the backend failure.
    pub async fn clean_up_after_backend_failure(mut self) -> BackendFailureCleanup {
        let scheduler = if let Some(handle) = self.sched_handle.take() {
            handle.await
        } else {
            Ok(())
        };
        // A scheduler panic can poison this mutex. Its JoinError is returned
        // below; recovering only to finish output must not replace that error.
        let writer = self
            .sched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .preemption_writer
            .take();
        let preemption_recording = if self.cfg.record_preemptions_to.is_some() {
            writer.map_or(Ok(()), |writer| writer.flush())
        } else {
            // The writer's destination is fixed from this same configuration.
            // In-memory-only recordings have no Drop write to finish.
            drop(writer);
            Ok(())
        };
        BackendFailureCleanup {
            scheduler,
            preemption_recording,
        }
    }

    /// Shut down anything running, in particular wait on the scheduler.
    ///
    /// This is basically the destructor for the global state, but is here rather than in the
    /// Drop instance so that it can be async, and is more explicitly sequenced in the program.
    ///
    /// Print a summary of the execution, typically called when it is complete.
    ///
    /// If the boolean argument is true, print to stderr, otherwise only print the summary
    /// to the log.
    ///
    /// Returns terminal replay, recording, or summary errors after consuming
    /// the remaining recording writer. Callers retain their outer owners until
    /// this returns and must not publish completed metadata on failure.
    pub async fn clean_up(
        self,
        to_stderr: bool,
        print_summary_to_json_file: &Option<PathBuf>,
    ) -> anyhow::Result<()> {
        self.clean_up_with_dispatch_stats(to_stderr, print_summary_to_json_file, None)
            .await
    }

    /// [`Self::clean_up`], also carrying the backend's dispatch record into the
    /// machine-readable summary. The human-readable report, including the INFO
    /// view `--verify` compares, never shows it.
    pub async fn clean_up_with_dispatch_stats(
        mut self,
        to_stderr: bool,
        print_summary_to_json_file: &Option<PathBuf>,
        dispatch_stats: Option<reverie::DispatchStats>,
    ) -> anyhow::Result<()> {
        if let Some(handle) = self.sched_handle.take() {
            debug!("Global state cleanup, confirming scheduler has shut down...");
            handle.await.expect("Global scheduler clean shutdown");
            debug!("Global state cleanup, continuing...");
        }
        let network_refusal = self.finish_network_trace();
        let banner =
            "  ------------------------------ hermit run report ------------------------------";
        let recording_destination = self.cfg.record_preemptions_to.clone();
        // The run is over: write what it opened for --verify.
        if let Some(log) = &self.cfg.host_input_log {
            crate::host_inputs::write(
                log,
                &self.host_inputs.lock().unwrap(),
                &self.host_mutations.lock().unwrap(),
            );
        }
        let summary_result = self
            .finalize_run_summary()
            .and_then(|(mut summary, description)| {
                summary.dispatch_stats = dispatch_stats;
                // No summary is published unless terminal validation and recording
                // output both succeeded. Output errors retain their ordinary class.
                if let Some(path) = print_summary_to_json_file {
                    let json = serde_json::to_string_pretty(&summary)?;
                    fs::write(path, json + "\n")?;
                }
                Ok((summary, description))
            });
        // A saved network refusal is an additional observed cause on this new
        // error path. The existing network-only report/exit path stays below.
        let (mut summary, info_reprio_descrip) = match summary_result {
            Ok(summary) => summary,
            Err(error) => {
                return crate::combine_finalization(
                    Err(error),
                    network_refusal.map_or(Ok(()), |refusal| Err(anyhow::anyhow!(refusal))),
                );
            }
        };
        // Print human-readable summary:
        if to_stderr {
            // In this case, print summary irrespective of logging level.
            // TODO: output summary in machine-readable, JSON form.
            //
            // NOT `eprint!`: a guest that set O_NONBLOCK on the inherited fd 2
            // makes `write_all` fail with EAGAIN here, which panics the print
            // macro and loses both the summary and the panic message. See
            // `crate::util::RetryingStderr`.
            {
                use std::io::Write;
                let _ = write!(crate::util::RetryingStderr, "{}\n{}", banner, summary);
            }
        } else {
            // Separate out the nondeterministic bits and print them at debug level:
            let rt = summary.realtime_elapsed.take();
            log_run_summary(
                banner,
                &summary,
                info_reprio_descrip.as_deref(),
                recording_destination.as_deref(),
            );
            if let Some(x) = rt {
                debug!("Nondeterministic realtime elapsed: {:?}", x);
            }
        }
        // After the report, so a refused replay still describes the run.
        if let Some(refusal) = network_refusal {
            {
                use std::io::Write;
                let _ = writeln!(crate::util::RetryingStderr, "{refusal}");
            }
            std::process::exit(detcore_model::HERMIT_POLICY_REFUSAL_EXIT);
        }
        Ok(())
    }

    #[cfg(test)]
    fn into_run_summary(self) -> anyhow::Result<RunSummary> {
        self.finalize_run_summary().map(|(summary, _)| summary)
    }

    // Keep GlobalState alive until every remaining writer has been consumed.
    // In particular, strict finish can fail before the summary takes it. A
    // consumed failed write is absent and must never be retried by Drop.
    fn finalize_run_summary(&self) -> anyhow::Result<(RunSummary, Option<String>)> {
        let summary = self.run_summary_for_log();
        let writer = self.sched.lock().unwrap().preemption_writer.take();
        // Do not hold the scheduler mutex while doing output I/O.
        let flushed = if self.cfg.record_preemptions_to.is_some() {
            writer.map_or(Ok(()), |writer| writer.flush().map_err(anyhow::Error::msg))
        } else {
            drop(writer);
            Ok(())
        };
        crate::combine_finalization(summary, flushed)
    }

    fn run_summary_for_log(&self) -> anyhow::Result<(RunSummary, Option<String>)> {
        // First, the scheduler can generate part of the summary (and flush once).
        let (mut summary, info_reprio_descrip) = {
            let mut sched = self.sched.lock().unwrap();
            sched.generate_partial_run_summary_for_log(self.cfg.record_preemptions_to.as_ref())?
        };
        // Second, we fill in the rest based on global state.
        // Record configuration and the runtime's timer mechanism, not whether
        // a timer is currently armed or has delivered an event. This metadata
        // stays separate from the INFO guest-behavior view.
        summary.preemption = Some(PreemptionSummary {
            max_timeslice_ns: self.cfg.max_timeslice.map(|maximum| maximum.get()),
            timer_enabled: Some(self.cfg.use_preemption_timer()),
        });
        //
        // Real time report:
        // N.B.: We don't have a job-level exit hook atm (T76248597), so we use the
        // CURRENT time -- that we are calling summarize -- as the end time:
        summary.realtime_elapsed = Some(self.realtime_start.elapsed()?);

        if self.cfg.virtualize_time {
            let final_time = self.global_time.lock().unwrap();
            summary.virttime_final = final_time.as_nanos().as_nanos();
            summary.virttime_elapsed = match final_time.elapsed_nanos() {
                Some(elapsed) => elapsed.as_nanos(),
                None => bail!(
                    "Internal invariant violated! Global time {} is before its epoch baseline",
                    final_time.as_nanos()
                ),
            };
        }

        Ok((summary, info_reprio_descrip))
    }
}

fn log_run_summary(
    banner: &str,
    summary: &RunSummary,
    info_reprio_descrip: Option<&str>,
    recording_destination: Option<&std::path::Path>,
) {
    info!("\n{}\n{}", banner, summary.info(info_reprio_descrip));
    debug!(
        replayed_events = summary.schedevent_replayed,
        ?recording_destination,
        "Run recording/replay bookkeeping"
    );
}

#[reverie::global_tool]
impl GlobalTool for GlobalState {
    type Config = Config;

    /// A request asks the scheduler to perform an RPC, which includes multiple kinds of
    /// actions, and, most importantly, permission to acquire resources and run the guest thread.
    ///
    /// Irrespective of which method we execute, we can "tick" our local component of the
    /// global time in the process.
    type Request = (DetTime, MmId, GlobalRequest);

    /// Response from the global portion of the Detcore instrumentation tool.
    /// The exact form of the response depends on which method was executed.
    ///
    /// Irrespective of which method was called, the global handling may have consumed
    /// logical time, in which case the scheduler can send a new thread-local time back to
    /// the caller.  Unfortunately, information is lost as this is collapsed to a flat
    /// scalar instead of a rich `DetTime`.
    type Response = (Option<LogicalTime>, GlobalResponse);

    /// Called once during startup.
    async fn init_global_state(cfg: &Config) -> GlobalState {
        // A backend whose guest thread IDs are not host tasks never reads a
        // host task's `/proc` signal state for them (`read_kernel_signal_state`).
        crate::syscalls::refuse_host_signal_state_reads(&cfg.backend);
        GlobalState::initialize(cfg, true)
    }

    fn install_inherited_input(&mut self, input: reverie::InheritedInputLease) {
        // Called once by the launcher before this global state is published.
        // All fallible endpoint/lease creation happened before initialization.
        *self.inherited_input.get_mut().unwrap() = Some(PendingInheritedInput {
            lease: input,
            root: None,
        });
    }

    fn install_mapping_capture_run(&mut self, run: reverie::MappingCaptureRun) {
        self.mapping_capture_run = Some(run);
    }

    fn install_backend_signal_control(
        &self,
        control: Option<reverie::BackendSignalControl>,
    ) -> Result<reverie::BackendSignalControlMode, reverie::Error> {
        self.sched
            .lock()
            .unwrap()
            .install_signal_control(control, self.cfg.sequentialize_threads)
    }

    /// Dequeues carry real-timer signals only in a run that installed the
    /// backend's control; any other run leaves them to the guest kernel.
    fn observe_signal_dequeues(&self, mode: reverie::BackendSignalControlMode) -> bool {
        mode == reverie::BackendSignalControlMode::ToolControlled
    }

    /// Detcore lets the backend skip an empty user-return boundary. Only
    /// reverie-kvm reads this answer, and only once Detcore's
    /// `install_signal_control` returned `ToolControlled`, which needs the
    /// backend's process signal control and `--sequentialize-threads`. The
    /// ptrace, DBT, SaBRe and LiteInst backends never ask, so the constant is
    /// fixed for the run on every backend, as the contract requires.
    ///
    /// An empty boundary is one where the thread holds no permit and the
    /// backend shows no pending signal its mask leaves unblocked, no parked
    /// ledger, no signalfd in the process, no dequeue-journal entry of the
    /// thread, and no stored dequeue failure. There the full round trip is
    /// `authorize_signal_boundary`, a selection of nothing, and a `NoHandler`
    /// receipt to `consume_signal_boundary`. Skipping it is unobservable:
    ///
    /// - Authorization publishes nothing. Detcore's own signals (real-timer
    ///   and POSIX-timer expiry, child-exit SIGCHLD) are published under the
    ///   scheduler lock at scheduler time into the backend's pending sets or
    ///   dequeue journal, which make a boundary non-empty, so none is lost.
    /// - Authorization succeeds only for `parked.running`. The only skippable
    ///   boundaries follow a completed Tool callback or a backend-owned
    ///   `rt_sigreturn` (thread entry checks only when a signal is eligible).
    ///   Every request is awaited, and every background release
    ///   (`BlockingExternalIO`, `BlockingRtSigsuspend`, a vfork parent) awaits
    ///   its `BlockedExternalContinue` grant before the callback returns, so
    ///   such a boundary is always inside the thread's own serial grant
    ///   (`holds_serial_grant`). No grant, alarm selection or step2 drain runs
    ///   then: they wait for quiescence, and `are_all_quiesced` waits on the
    ///   running thread's empty request. The control-intent drain
    ///   (`drain_control_intents`) does run on every pass of the turn loop,
    ///   but its intents only finish or resume an observed parked wait. A
    ///   finish runs while that wait's observation permit exists, and in the
    ///   boundary thread's own process that permit makes authorization
    ///   refuse; a resume reads no permit. So no other decision can see the
    ///   short-lived permit, and its release admits no exit: exits wait only
    ///   behind another permit of the process.
    /// - The receipt's cleanup is redundant there. Every grant settles the
    ///   parked request it answers (`settle_parked_grant`), and every request
    ///   the scheduler substitutes for a filed one retires the replaced
    ///   request's ownership (`retire_replaced_parked_request`), so no
    ///   ownership outlives its request. An observed wait's request and
    ///   `Observation` owner, and a `ReturningCaught` owner, always come with
    ///   a permit, which makes the boundary non-empty.
    /// - The rest is bookkeeping no decision reads: `parked.nonce` stays
    ///   monotonic, only smaller, and is never logged; `parked.completed`
    ///   only recognises an exact duplicate receipt.
    ///
    /// The cost is that Detcore's identity checks at authorization (ESRCH,
    /// EINVAL) now run only at non-empty boundaries.
    ///
    /// Held once before, while a backgrounded KVM `rt_sigsuspend` race was
    /// open (<https://github.com/rrnewton/hermit/issues/4360>). That race is
    /// fixed: a KVM `rt_sigsuspend` now runs in its caller's turn, and a
    /// sibling may exit while a thread holds a permit
    /// (<https://github.com/rrnewton/hermit/issues/4367>). Before those fixes
    /// the race fired about as often with this opt-in as without it.
    ///
    /// Re-prove or drop this if Detcore ever holds pending signals, or its
    /// own mask copy, that the backend cannot see and delivers them through
    /// authorization, or if the backend's KVM signal policy moves into
    /// Detcore: the skip trusts the backend's pending sets and mask.
    fn skip_empty_signal_boundaries(&self) -> bool {
        true
    }

    fn authorize_backend_signal_boundary(
        &self,
        task: reverie::SignalTaskIdentity,
    ) -> Result<Option<reverie::SignalDeliveryPermit>, reverie::Error> {
        self.sched.lock().unwrap().authorize_signal_boundary(task)
    }

    async fn on_backend_signal_boundary(
        &self,
        receipt: reverie::SignalBoundaryReceipt,
    ) -> Result<(), reverie::Error> {
        self.sched.lock().unwrap().consume_signal_boundary(receipt)
    }

    fn on_backend_process_retired(
        &self,
        event: reverie::BackendProcessRetirement,
    ) -> Result<(), reverie::Error> {
        let (result, wakes) = {
            let mut sched = self.sched.lock().unwrap();
            let result = sched.consume_process_retirement(event);
            (result, sched.take_signal_failure_wakes())
        };
        for wake in wakes {
            let _ = wake.send(());
        }
        result
    }

    /// A backend whose process exits complete asynchronously reports each
    /// physical exit here; see [`GlobalState::complete_physical_process_exit`].
    fn on_backend_process_exited(&self, pid: i32) {
        if self.cfg.backend.reports_child_exit_publication {
            // The kernel has published this process's exit to its parent. It
            // is not a physical-exit barrier report on this backend.
            let released = self
                .sched
                .lock()
                .unwrap()
                .complete_child_exit_publication(DetPid::from_raw(pid));
            // This branch bypasses `ProcessExitCleanup::complete_locked`: the
            // process's `/proc/<pid>/auxv` vector goes here. A zombie's file
            // is served on the rendered path whether or not its vector is
            // still kept (`Detcore::auxv_kept_by_kernel`), so the host-timed
            // moment of this report changes nothing the guest sees.
            self.process_auxv
                .lock()
                .unwrap()
                .forget_process(DetPid::from_raw(pid));
            if released {
                trace!("[detcore, dpid {}] backend published the child exit", pid);
            }
        } else {
            self.complete_physical_process_exit(pid);
        }
    }

    /// The processes held between their exit grant and their physical exit,
    /// for the backend's watchdog.
    fn backend_pending_process_exits(&self) -> Vec<i32> {
        self.sched
            .lock()
            .unwrap()
            .pending_async_process_exits()
            .into_iter()
            .map(|detpid| detpid.as_raw())
            .collect()
    }

    fn on_fatal_signal_exit(&self, exit: &reverie::FatalSignalExit) {
        if let Some(capture) = &self.cfg.fatal_core_capture {
            crate::fatal_core::capture_and_log(capture, exit);
        }
    }

    fn report_backend_failure(&self, event: reverie::BackendFailure) {
        let (wake, deferred) = {
            let mut sched = self.sched.lock().unwrap();
            (
                sched.report_backend_failure(event),
                sched.take_signal_failure_wakes(),
            )
        };
        for wake in deferred {
            let _ = wake.send(());
        }
        if let Some(wake) = wake {
            // No waiter can begin consuming cleanup until the scheduler has
            // closed its selected transaction under the grant/commit mutex.
            let _ = wake.send(());
        }
    }

    async fn wait_for_backend_failure(&self) {
        let wake = self.sched.lock().unwrap().backend_failure_waiter();
        wake.await
            .expect("GlobalState owns the failure sender until publication");
    }

    async fn on_backend_child_wait_event(
        &self,
        event: reverie::BackendChildWaitEvent,
    ) -> Result<(), reverie::Error> {
        // KVM polls this future through its first suspension before exposing
        // waitability. The helper's first poll performs the complete
        // generation-bound publication under the scheduler mutex, then
        // self-wakes and returns Pending; its second poll consumes the typed
        // result and releases the stage-2 control barrier.
        crate::scheduler::signal_control::ChildExitPublicationFuture::new(self.sched.clone(), event)
            .await
    }

    async fn receive_rpc(&self, from: Tid, gr: Self::Request) -> Self::Response {
        if let GlobalRequest::MappingCapture(request) = &gr.2 {
            return (
                None,
                GlobalResponse::MappingCapture(self.mapping_capture_request(from, gr.1, request)),
            );
        }
        if let GlobalRequest::CheckBrkMappings(check, fact) = &gr.2 {
            return (
                None,
                GlobalResponse::CheckBrkMappings(
                    self.capture_brk_mappings(from, gr.1, *check, *fact),
                ),
            );
        }
        // Intercept even malformed counted envelopes before taking a deadline
        // hint. Native controls belong to the already owned exec cut.
        if native_exec::is_native_exec_control(&gr.2) {
            return self.dispatch_native_exec_control(from, gr);
        }
        // The deadline hint of https://github.com/rrnewton/hermit/issues/4121
        // rides on whichever reply resumes the thread after its grant: an
        // ordinary or parked resource grant, a futex wake, a parent's
        // continuation, thread start, the exec continuation. Clearing on
        // arrival means a reply carries only a hint recorded while its own
        // request was pending (a grant, or a timer the request armed).
        // Only a request whose reply resumes the thread from a grant, or arms
        // a timer, touches the hint: control messages sent on another task's
        // behalf must neither consume nor carry it.
        //
        // Whether the scheduler resumed the thread from a wait while its
        // request was pending rides on the reply too (`GlobalResponse::Granted`),
        // whatever the request: the scheduler marks where it resumes a waiting
        // thread (`Scheduler::granted`: a grant, or a parked wait resumed to
        // observe a signal), so no request kind has to be listed for it. A
        // request answered without either carries no mark.
        let dtid = DetTid::from_raw(from.as_raw());
        let request_mm = gr.1;
        // Root/reloaded image self-registration precedes StartNewThread. Its
        // existing ParentContinue grant must not initialize callback counters
        // or consume the budget which belongs after the startup reset.
        let self_registration = registers_own_image(dtid, &gr.2);
        self.sched.lock().unwrap().take_grant(dtid);
        let (time, response) = if !resumes_from_grant(&gr.2) {
            self.dispatch_rpc(from, gr).await
        } else {
            self.sched.lock().unwrap().take_deadline_hint(dtid);
            let (time, response) = self.dispatch_rpc(from, gr).await;
            let hint = self.sched.lock().unwrap().take_deadline_hint(dtid);
            match hint {
                Some(remaining)
                    if response != GlobalResponse::ThreadExited
                        && response != GlobalResponse::Retired =>
                {
                    (
                        time,
                        GlobalResponse::WithDeadline(remaining, Box::new(response)),
                    )
                }
                _ => (time, response),
            }
        };
        let (granted, boundary_budget, boundary_owner) = {
            let mut scheduler = self.sched.lock().unwrap();
            let granted = scheduler.take_grant(dtid);
            let terminal =
                response == GlobalResponse::ThreadExited || response == GlobalResponse::Retired;
            let mut budget = scheduler
                .boundary_budgets
                .finish(dtid, request_mm, granted && !self_registration, terminal)
                .unwrap_or_else(|error| crate::scheduler::exit_on_scheduler_refusal(&error));
            // Same-leader exec keeps its existing turn; its authenticated
            // image-completion reply carries the new image's expectation
            // without pretending another resource turn was committed.
            if !terminal && matches!(response, GlobalResponse::MarkPastFirstExecve(_)) {
                assert!(
                    budget.is_none(),
                    "exec completion cannot steal an old grant budget"
                );
                budget = scheduler
                    .boundary_budget_for_live_owner(dtid)
                    .unwrap_or_else(|error| crate::scheduler::exit_on_scheduler_refusal(&error));
            }
            let owner = if !terminal
                && !self_registration
                && (granted || matches!(response, GlobalResponse::MarkPastFirstExecve(_)))
            {
                scheduler.boundary_owners.as_ref().map(|owners| {
                    owners
                        .owner(dtid, request_mm)
                        .unwrap_or_else(|error| crate::scheduler::exit_on_scheduler_refusal(&error))
                })
            } else {
                None
            };
            (granted, budget, owner)
        };
        let response = match boundary_budget {
            Some(budget) => GlobalResponse::WithSliceBoundaryBudget(budget, Box::new(response)),
            None => response,
        };
        let response = match boundary_owner {
            Some(owner) => GlobalResponse::WithSliceBoundaryOwner(owner, Box::new(response)),
            None => response,
        };
        if granted
            && response != GlobalResponse::ThreadExited
            && response != GlobalResponse::Retired
        {
            (time, GlobalResponse::Granted(Box::new(response)))
        } else {
            (time, response)
        }
    }
}

impl GlobalState {
    /// Evaluate a brk predicate in the controller's unchanged procfs view.
    /// The scheduler lock retains the caller's grant and physical identity
    /// until all temporary descriptors have closed. The backend fact excludes
    /// native tasks that the scheduler cannot stop; it is not inferred here.
    fn capture_brk_mappings(
        &self,
        from: Tid,
        mm: MmId,
        check: crate::syscalls::memory::BrkMappingCheck,
        fact: reverie::ControllerCaptureFact,
    ) -> Option<bool> {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;

        if !self.cfg.sequentialize_threads {
            return None;
        }
        let sched = self.sched.lock().unwrap();
        let dettid = DetTid::from_raw(from.as_raw());
        let (pid, tid) = sched.controller_capture_task(dettid, mm, fact)?;
        let root = File::open("/proc").ok()?;
        let metadata = root.metadata().ok()?;
        if !metadata.is_dir()
            || metadata.dev() != fact.procfs_device
            || metadata.ino() != fact.procfs_inode
        {
            return None;
        }
        let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: root is live and filesystem has space for the complete result.
        if unsafe { libc::fstatfs(root.as_raw_fd(), filesystem.as_mut_ptr()) } != 0
            || unsafe { filesystem.assume_init() }.f_type as u64
                != reverie::syscalls::linux_abi::PROC_SUPER_MAGIC as u64
        {
            return None;
        }
        // A live nonleader remains addressable by its TID after its leader
        // exits, when the leader's /task directory may no longer be readable.
        let relative = std::ffi::CString::new(format!("{tid}/smaps")).ok()?;
        // SAFETY: both the directory and nul-terminated relative path are live.
        let raw = unsafe {
            libc::openat(
                root.as_raw_fd(),
                relative.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return None;
        }
        // SAFETY: openat returned a new descriptor exclusively owned here.
        let mut smaps_file = unsafe { File::from_raw_fd(raw) };
        let mut smaps = Vec::new();
        smaps_file.read_to_end(&mut smaps).ok()?;
        // External death cannot turn a partial or former task's capture into
        // authority for a replacement. The registered pidfd is still held.
        if sched.controller_capture_task(dettid, mm, fact) != Some((pid, tid)) {
            return None;
        }
        Some(check.allows(&smaps))
        // smaps_file and root close before sched releases the grant guard and
        // before the caller receives the fixed, allocation-free answer.
    }

    /// Prints, once per run, a WARN for each signal whose handler an in-guest
    /// runtime refused and one for a refused alternate stack
    /// (`crate::refused_signal_controls`), whichever process reports it.
    fn announce_refused_signal_controls(&self, actions: u64, alternate_stack: bool) {
        let mut announced = self.announced_refused_signal_controls.lock().unwrap();
        let new_actions = actions & !announced.0;
        announced.0 |= actions;
        let new_stack = alternate_stack && !announced.1;
        announced.1 |= alternate_stack;
        drop(announced);
        for signum in (1..=64).filter(|signum| new_actions & (1 << (signum - 1)) != 0) {
            warn!(
                "{}",
                crate::refused_signal_controls::handler_refused_warning(signum)
            );
        }
        if new_stack {
            warn!(
                "{}",
                crate::refused_signal_controls::alternate_stack_refused_warning()
            );
        }
    }

    async fn dispatch_rpc(
        &self,
        from: Tid,
        gr: <Self as GlobalTool>::Request,
    ) -> <Self as GlobalTool>::Response {
        // This read stays inside the caller's existing grant. In particular,
        // it neither drains logs nor updates clocks or deadline ownership.
        if let GlobalRequest::MappingCapture(request) = &gr.2 {
            return (
                None,
                GlobalResponse::MappingCapture(self.mapping_capture_request(from, gr.1, request)),
            );
        }
        if let GlobalRequest::CheckBrkMappings(check, fact) = &gr.2 {
            return (
                None,
                GlobalResponse::CheckBrkMappings(
                    self.capture_brk_mappings(from, gr.1, *check, *fact),
                ),
            );
        }
        // This entry is also used directly. The strict native cut must precede
        // ordinary forwarding drain/checks and all sender-clock accounting.
        if native_exec::is_native_exec_control(&gr.2) {
            return self.dispatch_native_exec_control(from, gr);
        }
        // A Detcore warning from inside the guest (`crate::guest_log`) carries
        // no logical time, changes no scheduler state, and its answer carries
        // no time back. It is answered before the forwarded-record drain below,
        // whose order state it must not touch: verification never compares a
        // warning's place, and a run's ordered records must not depend on
        // whether it warned. (`receive_rpc` takes no deadline hint for it.)
        if let GlobalRequest::GuestLog(record, unsent_before) = &gr.2 {
            crate::guest_log::emit(record.as_ref(), *unsent_before);
            return (None, GlobalResponse::GuestLog(()));
        }
        // Signal controls an in-guest runtime refused
        // (`crate::refused_signal_controls`): announced once per run, and,
        // like a warning, answered before the forwarded-record drain, with no
        // time either way.
        if let GlobalRequest::AnnounceRefusedSignalControls(actions, alternate_stack) = &gr.2 {
            self.announce_refused_signal_controls(*actions, *alternate_stack);
            return (None, GlobalResponse::AnnounceRefusedSignalControls(()));
        }
        // Records an in-guest Tool forwarded before sending this request are queued,
        // and the sender's are written now if it runs alone (the order rule at
        // `detlog::set_forwarded_source`).
        crate::detlog::drain_forwarded_at_request(from.as_raw());
        type R = GlobalResponse;
        let dtid = DetTid::from_raw(from.into()); // TODO(T78538674): FIXME
        let (guest_time, request_mm, request) = gr;
        let request = match request {
            GlobalRequest::WithForwardedCount(produced, request) => {
                crate::detlog::check_forwarded_count(from.as_raw(), produced);
                *request
            }
            request => request,
        };
        let time_from_guest = guest_time.as_nanos();
        // The thread's own computation and syscalls, excluding clock advances
        // (`DetTime::advance_to`), for `--work-span-report`.
        let work_span_cpu = self.cfg.work_span_report.is_some().then(|| {
            guest_time.user_cpu_time().as_nanos() + guest_time.system_cpu_time().as_nanos()
        });
        if let Some(cpu) = work_span_cpu {
            use crate::scheduler::work_span::Arrival;
            let arrival = match &request {
                GlobalRequest::StartNewThread(child, ..) if *child == dtid => Arrival::StartsThread,
                GlobalRequest::RequestResources(resources, _) if resources.poll_attempt > 0 => {
                    Arrival::Retry(resources)
                }
                GlobalRequest::ReconnectExec { .. } | GlobalRequest::RetireExec { .. } => {
                    Arrival::ExecTransfer
                }
                _ => Arrival::Other,
            };
            self.sched
                .lock()
                .unwrap()
                .work_span_note_arrival(dtid, cpu, arrival);
        }
        // Exec transfer messages authenticate both incarnations before ordinary
        // sender-clock accounting. The carried clock still belongs to the former
        // thread until the successful reconnect commits.
        match &request {
            GlobalRequest::ReconnectExec { former, process } => {
                return self
                    .recv_exec_transfer(from, guest_time, request_mm, *former, *process)
                    .await;
            }
            GlobalRequest::RetireExec { thread, signaled } => {
                return self
                    .recv_retire_exec_transfer(
                        from,
                        guest_time,
                        request_mm,
                        thread.clone(),
                        *signaled,
                    )
                    .await;
            }
            _ => {}
        }
        // A committed SIGKILL victim's requests that return before ordinary
        // admission (https://github.com/rrnewton/hermit/issues/3994). Exec
        // transfers above, the child-exit SIGCHLD control (sent in the
        // sender's own turn, which a committed victim no longer gets, or by
        // the tracer at a signal stop) and the consuming `SignalDequeued` keep
        // their own answers. Shared open file control is in-guest LiteInst
        // transport, never sent on ptrace, where alone a SIGKILL is retired
        // at the commit. Observations and loss notices are still recorded,
        // since dropping one only loses information, but not answered. A
        // SIGALRM ledger change from a thread that is already logically dead
        // would land at a host-chosen time: it is refused, and not answered
        // either.
        let early_victim_request = match &request {
            GlobalRequest::RecordHostInput { .. }
            | GlobalRequest::RecordHostMutation { .. }
            | GlobalRequest::RecordDeterminismLoss(_)
            | GlobalRequest::Sigalrm(_) => self
                .sched
                .lock()
                .unwrap()
                .is_unaccounted_sigkill_victim(dtid),
            _ => false,
        };
        if early_victim_request && matches!(&request, GlobalRequest::Sigalrm(_)) {
            return (None, R::Retired);
        }
        // An observation for `hermit run --verify`, answered before any clock
        // or scheduler accounting: it carries no logical time, changes no
        // scheduler state, and its answer carries no time back.
        if let GlobalRequest::RecordHostInput {
            path,
            syscall,
            identity,
        } = request
        {
            self.host_inputs.lock().unwrap().push(HostInputRecord {
                path,
                dtid: dtid.as_raw() as u64,
                syscall,
                identity,
            });
            if early_victim_request {
                return (None, R::Retired);
            }
            return (None, R::RecordHostInput(()));
        }
        if let GlobalRequest::RecordHostMutation { path } = request {
            self.host_mutations.lock().unwrap().insert(path);
            if early_victim_request {
                return (None, R::Retired);
            }
            return (None, R::RecordHostMutation(()));
        }
        // A determinism loss is recorded where verification reads it, and,
        // like the observations above, moves no clock: whether the host made
        // a guest-side handler lose determinism must not change guest time.
        if let GlobalRequest::RecordDeterminismLoss(reason) = request {
            crate::detlog::write_loss_notice(&reason);
            if early_victim_request {
                return (None, R::Retired);
            }
            return (None, R::RecordDeterminismLoss(()));
        }
        // Control traffic for a shared open file description: like the
        // observations above, it carries no logical time, changes no
        // scheduler state, and its answer carries no time back.
        if let GlobalRequest::SharedOpenFile(control) = request {
            return (
                None,
                R::SharedOpenFile(self.shared_open_file_control(dtid, control)),
            );
        }
        // A SIGALRM ledger control message: like the messages above, it
        // carries no logical time and its answer carries none back. It reads
        // or records ledger state, which changes only at scheduler commits,
        // in the sender's own turn.
        if let GlobalRequest::Sigalrm(control) = request {
            let refused = self.sched.lock().unwrap().sigalrm_control(dtid, control);
            return (None, R::Sigalrm(refused));
        }
        // A child-exit SIGCHLD control message: like the SIGALRM ledger's, it
        // carries no logical time and its answer carries none back. It
        // records an exit status in the sender's own turn, or answers at a
        // signal stop, which no scheduler state waits for.
        if let GlobalRequest::ChildExitSigchld(control) = request {
            let answer = self
                .sched
                .lock()
                .unwrap()
                .child_exit_sigchld_control_for_receiver(dtid, control);
            return (None, R::ChildExitSigchld(answer));
        }
        // A discarded-at-send record message
        // (`scheduler::discarded_at_send`): like the child-exit ledger's, it
        // carries no logical time and its answer carries none back. It records
        // a send in the sender's own turn, or a consumption at a signal stop.
        if let GlobalRequest::DiscardedSignal(control) = request {
            let answer = self
                .sched
                .lock()
                .unwrap()
                .discarded_signal_control(dtid, control);
            return (None, R::DiscardedSignal(answer));
        }
        // A thread's copied signal dispositions
        // (`syscalls::signal_shadow::publish_dispositions`): like the control
        // messages above, it carries no logical time and its answer carries
        // none back. It replaces what the scheduler holds for the sender's
        // process, which the sender's own operations changed while it held
        // the turn.
        if let GlobalRequest::PublishSignalDispositions(generation, dispositions) = request {
            let held = self.sched.lock().unwrap().publish_signal_dispositions(
                dtid,
                request_mm,
                generation,
                dispositions,
            );
            return (None, R::PublishSignalDispositions(held));
        }
        // A thread's mask from Detcore's copy
        // (`syscalls::signal_shadow::publish_mask`): the same kind of message.
        if let GlobalRequest::PublishSignalMask(mask) = request {
            let taken = self
                .sched
                .lock()
                .unwrap()
                .publish_signal_mask(dtid, request_mm, mask);
            return (None, R::PublishSignalMask(taken));
        }
        if let GlobalRequest::PublishRuntimePrivateMemory(facts) = request {
            let accepted = self
                .sched
                .lock()
                .unwrap()
                .publish_runtime_private_memory(dtid, request_mm, facts);
            return (None, R::PublishRuntimePrivateMemory(accepted));
        }
        if let GlobalRequest::SignalDequeued {
            detpid,
            identity,
            dequeue,
        } = &request
        {
            return self
                .recv_signal_dequeued(dtid, request_mm, guest_time, *detpid, *identity, *dequeue)
                .await;
        }

        // A vfork child registers itself while the parent is kernel-blocked.
        // Reopening a former exec worker's TID requires that exact pending
        // parent grant and its address-space lineage, not an ordinary RPC.
        if let GlobalRequest::CreateVforkChildThread(parent, process, child, _, flags, ..) =
            &request
        {
            let mut sched = self.lock_rpc_scheduler(false).await;
            if sched.transferred_exec_tid_requires_registration(dtid) {
                if *child != dtid
                    || !flags.contains(CloneFlags::CLONE_VFORK)
                    || !sched.pending_vfork_registration_matches(
                        *parent,
                        *process,
                        *child,
                        request_mm,
                        flags.contains(CloneFlags::CLONE_VM),
                    )
                {
                    return (None, R::ThreadExited);
                }
                sched.register_reused_transferred_exec_tid(dtid, request_mm);
            }
        }

        // Ptrace may deliver a newborn's startup before the parent's clone
        // callback registers it. A former exec worker's reused Linux TID must
        // wait for that authenticated registration, not inherit the old
        // incarnation or turn a premature refusal into a silent exit(0).
        // No clock or scheduler request is published while it waits.
        if matches!(&request, GlobalRequest::StartNewThread(child, ..) if *child == dtid) {
            loop {
                {
                    let sched = self.lock_rpc_scheduler(false).await;
                    if !sched.transferred_exec_tid_requires_registration(dtid)
                        || sched.thread_is_logically_killed(dtid)
                    {
                        break;
                    }
                }
                yield_once().await;
            }
        }

        let is_deregister = matches!(&request, GlobalRequest::DeregisterThread(_));
        let consuming_cleanup =
            is_deregister || matches!(&request, GlobalRequest::RobustListWakes(_));
        // What a committed SIGKILL victim may still send and be answered
        // (`Scheduler::is_unaccounted_sigkill_victim`): its consuming cleanup,
        // and the failure reports that end a run.
        let answered_for_sigkill_victim = consuming_cleanup
            || matches!(
                &request,
                GlobalRequest::ParkedProtocolFailure(_)
                    | GlobalRequest::UnrecoverableShutdown
                    | GlobalRequest::PolicyRefusal(_)
                    | GlobalRequest::StoppingRefusal(_)
            );

        let (exec_reconnect, is_exec_caller_after_local_mm_swap) = {
            let pending = self.pending_exec_states.lock().unwrap();
            let reconnect = match &request {
                GlobalRequest::CreateChildThread(child, process, _, None, _, _, _)
                    if *child == dtid && *child == *process =>
                {
                    pending.get(process).cloned()
                }
                _ => None,
            };
            let is_exec_caller_after_local_mm_swap = pending.values().any(|state| {
                state.caller == dtid && state.mm.for_exec(state.process) == request_mm
            });
            (reconnect, is_exec_caller_after_local_mm_swap)
        };

        // Retired incarnations cannot publish clocks. A transferred former TID
        // reopens only through fresh child registration; successful exec binds
        // the surviving leader separately. Hold admission through clock
        // accounting so logical teardown cannot linearize between the two.
        let mut tombstoned_deregistration = None;
        {
            let sched = self.lock_rpc_scheduler(consuming_cleanup).await;
            if exec_reconnect.is_none()
                && !is_exec_caller_after_local_mm_swap
                && !sched.rpc_incarnation_matches(dtid, request_mm)
            {
                debug!(
                    "[detcore, dtid {}] rejecting {:?} RPC from retired exec incarnation {:?}",
                    dtid, request, request_mm,
                );
                return if is_deregister {
                    (None, R::DeregisterThread(()))
                } else {
                    (None, R::ThreadExited)
                };
            }
            if let GlobalRequest::DeregisterThread(owner) = &request {
                assert_eq!(
                    owner.dettid, dtid,
                    "deregistration must belong to its sender"
                );
                assert_eq!(owner.mm, request_mm, "deregistration must retain its MmId");
                // DBT can reject StartNewThread before parent registration.
                // Its tombstone still needs the existing final accounting path.
                if !sched.thread_was_registered(dtid) && !sched.thread_is_logically_killed(dtid) {
                    assert!(
                        sched.backend_failed() || !owner.thread_start_entered,
                        "a started thread must have a scheduler registration before deregistration"
                    );
                    // The backend still consumes this constructed ThreadState,
                    // but no guest start/registration happened. Acknowledge its
                    // cleanup without creating a clock, tree entry or admission.
                    return (None, R::DeregisterThread(()));
                }
            }
            let child = match &request {
                GlobalRequest::CreateChildThread(child, ..)
                | GlobalRequest::CreateVforkChildThread(_, _, child, ..) => Some(*child),
                _ => None,
            };
            if sched.thread_is_logically_killed(dtid) && exec_reconnect.is_none() {
                trace!(
                    "[detcore, dtid {}] rejecting RPC after permanent logical-thread removal",
                    dtid
                );
                if let GlobalRequest::DeregisterThread(deregistration) = &request {
                    tombstoned_deregistration = Some(deregistration.clone());
                } else {
                    return (None, R::ThreadExited);
                }
            }
            if !answered_for_sigkill_victim && sched.is_unaccounted_sigkill_victim(dtid) {
                trace!(
                    "[detcore, dtid {}] leaving a SIGKILLed thread's request unanswered",
                    dtid
                );
                return (None, R::Retired);
            }
            if child.is_some_and(|child| sched.thread_is_logically_killed(child))
                && exec_reconnect.is_none()
            {
                trace!(
                    "[detcore, dtid {}] rejecting registration that reuses a tombstoned child TID",
                    dtid
                );
                return (None, R::ThreadExited);
            }

            if let GlobalRequest::ResumeExec(process) = &request
                && (dtid != *process
                    || sched.registered_process(dtid) != Some(*process)
                    || !sched.next_turns.contains_key(&dtid))
            {
                return (None, R::ThreadExited);
            }

            // An admitted ordinary request under the new identity proves the
            // Tool received the transfer acknowledgment and rebound its state.
            self.acknowledge_exec_transfer(dtid, request_mm);

            let is_thread_reconnect = matches!(
                &request,
                GlobalRequest::StartNewThread(child_dettid, ..) if *child_dettid == dtid
            ) && self.global_time.lock().unwrap().contains_thread(dtid);

            // This portion of the time updates "asynchronously", and we can tick it on every rpc:
            // TODO: eventually the vector clock should be in shared memory, and
            // the local clocks should update truly asynchrously.  Therefore it
            // SHOULD be safe to always push through this update on any rpc.
            if tombstoned_deregistration.is_none()
                && exec_reconnect.is_none()
                && !is_thread_reconnect
            {
                self.global_time.lock().unwrap().update_global_time(
                    dtid,
                    time_from_guest,
                    guest_time.inherited_nanos(),
                );
            }
        }
        if let Some(deregistration) = tombstoned_deregistration {
            self.recv_deregister_thread(from, deregistration).await;
            return (None, R::DeregisterThread(()));
        }

        // RPC boilerplate. (Hard to generate systematically now though, because of the
        // time payload piggy-backing on each rpc. Maybe eventually once ticking a
        // threads' own clock happens through shared memory.)
        #[allow(clippy::unit_arg)]
        let resp = match request {
            GlobalRequest::PrepareNativeExec(_)
            | GlobalRequest::SealNativeExecStats { .. }
            | GlobalRequest::IssueNativeExec { .. }
            | GlobalRequest::CancelNativeExec { .. }
            | GlobalRequest::CompleteNativeExec { .. } => {
                unreachable!("native controls handled before ordinary admission")
            }
            GlobalRequest::ReconnectExec { .. } | GlobalRequest::RetireExec { .. } => {
                unreachable!("exec transfer handled before ordinary admission")
            }
            GlobalRequest::WithForwardedCount(..) => {
                unreachable!("a counted request is unwrapped on arrival")
            }
            GlobalRequest::ResumeExec(process) => {
                let mut resources = Resources::new(dtid);
                resources.insert(ResourceID::MemAddrSpace(process), Permission::RW);
                let (response, duration) = self
                    .recv_request_resources(from, process, resources, Some(request_mm))
                    .await;
                match response {
                    SchedulerRpcResult::Continue(status) => R::ResumeExec(status, duration),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            GlobalRequest::SignalDequeued { .. } => {
                unreachable!("consuming path handled before ordinary cancellation")
            }
            GlobalRequest::RecordHostInput { .. } | GlobalRequest::RecordHostMutation { .. } => {
                unreachable!("host-input observation answered before clock accounting")
            }
            GlobalRequest::SharedOpenFile(_) => {
                unreachable!("shared open file control answered before clock accounting")
            }
            GlobalRequest::GuestLog(..) => {
                unreachable!("a guest log record is answered before clock accounting")
            }
            GlobalRequest::AnnounceRefusedSignalControls(..) => {
                unreachable!(
                    "refused signal controls are answered before the forwarded-record drain"
                )
            }
            GlobalRequest::MappingCapture(_) => {
                unreachable!("coherent mapping capture precedes clock accounting")
            }
            GlobalRequest::CheckBrkMappings(..) => {
                unreachable!("mapping capture is answered before clock accounting")
            }
            GlobalRequest::Sigalrm(_) => {
                unreachable!("SIGALRM ledger control answered before clock accounting")
            }
            GlobalRequest::ChildExitSigchld(_) => {
                unreachable!("child-exit SIGCHLD control answered before clock accounting")
            }
            GlobalRequest::DiscardedSignal(_) => {
                unreachable!("discarded-at-send control answered before clock accounting")
            }
            GlobalRequest::RecordDeterminismLoss(_) => {
                unreachable!("determinism loss answered before clock accounting")
            }
            GlobalRequest::PublishSignalDispositions(..) => {
                unreachable!("published dispositions answered before clock accounting")
            }
            GlobalRequest::PublishSignalMask(_) => {
                unreachable!("a published mask is answered before clock accounting")
            }
            GlobalRequest::PublishRuntimePrivateMemory(_) => {
                unreachable!("private-memory publication precedes clock accounting")
            }
            GlobalRequest::ParkedRequest(rs, pid, capability) => {
                let (response, _, stack) = self
                    .recv_resources_with_origin(
                        from,
                        pid,
                        rs,
                        Some(request_mm),
                        RpcOrigin::DirectRequestResources,
                        capability,
                    )
                    .await;
                match response {
                    SchedulerRpcResult::Continue(r) => {
                        Self::with_boundary_stack(R::ParkedRequest(r), stack)
                    }
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            GlobalRequest::ResumeParkedRequest {
                ticket,
                current_site,
            } => {
                self.recv_resume_parked(from, request_mm, ticket, current_site)
                    .await
            }
            GlobalRequest::FinishParkedObservation {
                wait,
                lease,
                site,
                finish,
            } => {
                let ack = Ivar::new();
                let intent = ControlIntent::Finish {
                    wait,
                    lease,
                    site,
                    finish,
                    ack: ack.clone(),
                };
                let posted = self
                    .sched
                    .lock()
                    .unwrap()
                    .post_control(dtid, request_mm, intent);
                if let Err(error) = posted {
                    self.sched.lock().unwrap().fail_parked(dtid, error);
                    return (None, R::ThreadExited);
                }
                R::FinishParkedObservation(ack.await)
            }
            GlobalRequest::ParkedProtocolFailure(error) => {
                self.sched.lock().unwrap().fail_parked(dtid, error);
                return (None, R::ThreadExited);
            }

            GlobalRequest::RequestResources(rs, pid) => {
                let (response, time, stack) = self
                    .recv_resources_with_origin(
                        from,
                        pid,
                        rs,
                        Some(request_mm),
                        RpcOrigin::DirectRequestResources,
                        ControlCapability::None,
                    )
                    .await;
                match self.resource_grant_reply(from, response, time).0 {
                    SchedulerRpcResult::Continue(response) => {
                        Self::with_boundary_stack(R::RequestResources(response), stack)
                    }
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            GlobalRequest::ReleaseResources(rs) => {
                R::ReleaseResources(self.recv_release_resources(from, rs).await)
            }
            GlobalRequest::ReleaseAllResources => {
                R::ReleaseAllResources(self.recv_release_all_resources(from).await)
            }
            GlobalRequest::PolicyRefusal(reason) => {
                self.policy_refusal.lock().unwrap().get_or_insert(reason);
                R::PolicyRefusal(())
            }
            // Acted on even from a committed SIGKILL victim (it is in the
            // victim's answered list, as PolicyRefusal is). Whether a victim
            // gets to file one before it dies is host-timed once 3c.1c and
            // 3c.1e add producers; to be re-examined there.
            GlobalRequest::StoppingRefusal(reason) => {
                crate::scheduler::exit_on_scheduler_refusal(&reason)
            }
            // TODO-HUMAN-REVIEW(PR-643): Review run-wide unsupported-syscall aggregation.
            GlobalRequest::ReportUnsupportedSyscall(name) => {
                let _sched = self.lock_rpc_scheduler(false).await;
                let inserted = self
                    .unsupported_syscalls
                    .lock()
                    .unwrap()
                    .insert(name.clone());
                if inserted
                    && let Some(report) = &self.unsupported_syscall_report_fd
                    && let Err(error) = writeln!(report.lock().unwrap(), "{name}")
                {
                    warn!("failed to append unsupported-syscall report: {error}");
                }
                R::ReportUnsupportedSyscall(())
            }
            GlobalRequest::AnnounceSignalHandlerNotInstalled(signum) => {
                if self
                    .announced_handlers_not_installed
                    .lock()
                    .unwrap()
                    .insert(signum)
                {
                    warn!("{}", signal_handler_not_installed_warning(signum));
                }
                R::AnnounceSignalHandlerNotInstalled(())
            }
            GlobalRequest::PrepareExec(process, mm, fd_blocking) => {
                let mut sched = self.lock_rpc_scheduler(false).await;
                if mm != request_mm
                    || fd_blocking.initialize_inherited_stdio
                    || fd_blocking.initial_input.is_some()
                {
                    return (None, R::ThreadExited);
                }
                if self.cfg.sequentialize_threads && !sched.prepare_exec_teardown(dtid, process, mm)
                {
                    return (None, R::ThreadExited);
                }
                trace!(
                    "[detcore, dtid {}] preparing exec for process {} with mm {:?} and logically blocking descriptors {:?}",
                    dtid, process, mm, fd_blocking,
                );
                self.pending_exec_states.lock().unwrap().insert(
                    process,
                    PendingExecState {
                        caller: dtid,
                        process,
                        mm,
                        fd_blocking: *fd_blocking,
                    },
                );
                R::PrepareExec(())
            }
            GlobalRequest::CancelExec(process) => {
                let mut sched = self.lock_rpc_scheduler(false).await;
                let mut pending = self.pending_exec_states.lock().unwrap();
                if pending
                    .get(&process)
                    .is_some_and(|state| state.caller == dtid)
                {
                    let prepared = pending.remove(&process).unwrap();
                    sched.finish_exec_teardown(prepared.caller, process, prepared.mm, false);
                }
                R::CancelExec(())
            }
            GlobalRequest::MarkPastFirstExecve(
                detpid,
                signal_identity,
                unprepared_exe,
                kernel_image,
            ) => {
                let mut sched = self.lock_rpc_scheduler(false).await;
                // The exec leaves its process's descriptor table
                // (`LockLedger::exec`); `TableExec` names what survived.
                self.lock_ledger.lock().unwrap().exec(detpid);
                // A backend preserving the leader's ThreadState does not use
                // the identity-transfer RPC. This successful-exec edge still
                // commits the frozen sibling cohort in scheduler order.
                let prepared = self.pending_exec_states.lock().unwrap().get(&dtid).cloned();
                if sched.signal_control_installed() {
                    let result = (|| {
                        let identity = signal_identity.ok_or(ProtocolFailure::Identity)?;
                        let pid = sched
                            .registered_process(dtid)
                            .ok_or(ProtocolFailure::Identity)?;
                        let mut pending = self.pending_exec_states.lock().unwrap();
                        if let Some(prepared) = pending.get(&pid) {
                            if prepared.caller != dtid || prepared.process != pid {
                                return Err(ProtocolFailure::Identity);
                            }
                            sched.complete_signal_exec(
                                pid,
                                dtid,
                                prepared.mm,
                                request_mm,
                                identity,
                            )?;
                            pending.remove(&pid);
                        } else {
                            // A backend that re-enters thread-start already bound
                            // the new image. A lifecycle notification alone cannot
                            // authorize a new address space or backend generation.
                            sched
                                .real_timers
                                .validate_task(pid, dtid, request_mm, identity)?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        sched.fail_parked(dtid, error);
                        return (None, R::ThreadExited);
                    }
                }
                // The exec caller still owns its turn: delete process-owned
                // POSIX deadlines before the replacement image can yield or run.
                // Failed exec takes CancelExec and never reaches this point.
                sched.delete_exec_posix_timers(detpid);
                // The exec resets the process's exit signal to SIGCHLD and turns
                // its existing children's notifications into SIGCHLD.
                sched.record_successful_exec(detpid);
                // Reloading backends consumed the preparation in their
                // registration reconnect; state-preserving leaders consume it
                // here after the terminal exec notification was authenticated.
                if let Some(prepared) = &prepared
                    && self.cfg.sequentialize_threads
                    && prepared.caller == dtid
                    && prepared.process == dtid
                    && prepared.mm.for_exec(dtid) == request_mm
                {
                    // Only a reloading backend (DBT) carries owner-death
                    // wakes with its preparation, and it delivered them at
                    // its reconnect; ptrace delivers its own after the exec
                    // (Claude review of
                    // https://github.com/rrnewton/hermit/pull/4352, F5).
                    assert!(
                        prepared.fd_blocking.robust_wakes.is_empty(),
                        "a state-preserving exec carried robust-list wakes to the coordinator"
                    );
                    sched.reconnect_after_exec(ExecReconnect {
                        caller: dtid,
                        new_leader: dtid,
                        detpid: dtid,
                        pre_exec_mm: prepared.mm,
                        post_exec_mm: request_mm,
                        child_tid_addr: 0,
                        reconnect_priority: None,
                    });
                    self.pending_exec_states.lock().unwrap().remove(&dtid);
                    // Native-only lifecycle receipt: the actual same-leader
                    // Mark edge has now installed this ticket's new mm. This
                    // hook grants no turn and performs no clock accounting.
                    if let Some(registry) = self.native_exec_registry.get()
                        && registry
                            .lock()
                            .unwrap()
                            .mark_past_first_exec(dtid, request_mm)
                            .is_err()
                    {
                        return (None, R::ThreadExited);
                    }
                }
                self.past_first_execve.store(true, SeqCst);
                let first_image = !self.initial_descriptor_image_complete.swap(true, SeqCst);
                let mut overrides = self
                    .post_exec_fd_blocking
                    .lock()
                    .unwrap()
                    .remove(&dtid)
                    .unwrap_or_default();
                trace!(
                    "[detcore, dtid {}] restoring logically blocking descriptors after exec: {:?}",
                    dtid, overrides,
                );
                overrides.initialize_inherited_stdio =
                    first_image && self.cfg.backend.tool_shares_guest_descriptor_table;
                // The image the exec ran: a reloading backend carried it in the
                // preparation it reconnected with (above); a state-preserving
                // one left the preparation to this edge. The registered root's
                // first image is the one its launch named, which a backend
                // whose launcher loads the guest (SaBRe) never execs.
                // Without sequentialized threads, sibling execs may prepare
                // concurrently and the preparation this edge consumed may be
                // another thread's, which the launcher check cannot tell
                // apart: no prepared record is kept (a launched run then
                // refuses the link by name).
                if !self.cfg.sequentialize_threads {
                    overrides.exe = None;
                } else if overrides.exe.is_none() {
                    overrides.exe = prepared
                        .filter(|prepared| prepared.caller == dtid)
                        .and_then(|prepared| prepared.fd_blocking.exe)
                        // An exec no Detcore prepared (a backend's copied
                        // vfork child runs it without Detcore): the record the
                        // new image resolved from the path the exec was given.
                        .or(unprepared_exe);
                }
                if self.root_dettid.get() == Some(&dtid)
                    && !self.root_image_marked.swap(true, SeqCst)
                    && let Some(launch) = &self.cfg.launch_image
                {
                    let launched = launch.exe.as_deref().and_then(|exe| {
                        crate::exec_record::ExecRecord::at(std::path::Path::new("/"), exe)
                    });
                    overrides.exe = if launch.launcher.is_some() {
                        // In a run that declares a launcher, the image's own
                        // record (from its `AT_EXECFN`) is the file the
                        // launcher actually ran, where it names one other
                        // than the launcher: it outranks the launch's own
                        // search, which stands where the image gave none.
                        overrides
                            .exe
                            .take()
                            .filter(|record| Some(record.identity()) != self.launcher_identity())
                            .or(launched)
                    } else {
                        launched
                    };
                }
                // A record stands only where the kernel's own link, which the
                // new image read (`kernel_image`), names the recorded file or
                // the launcher the run declared: Detcore's resolution looked the
                // path up in its own view, which a binfmt_misc handler, another
                // filesystem view or a racing exec can make wrong. Where the
                // tool cannot read the link (KVM), the record stands.
                if let Some(kernel) = kernel_image {
                    overrides.exe = overrides.exe.filter(|record| {
                        record.identity() == kernel || self.launcher_identity() == Some(kernel)
                    });
                }
                // The launcher itself is never the guest's image (a link to it
                // resolved, as the kernel's link names it).
                overrides.exe = overrides
                    .exe
                    .filter(|record| Some(record.identity()) != self.launcher_identity());
                {
                    let mut images = self.exec_images.lock().unwrap();
                    match &overrides.exe {
                        Some(record) => images.insert(detpid, record.clone()),
                        None => images.remove(&detpid),
                    };
                }
                if self.cfg.backend.tool_shares_guest_descriptor_table {
                    let mut input = self.inherited_input.lock().unwrap();
                    if input
                        .as_ref()
                        .is_some_and(|input| input.root == Some((dtid, request_mm)))
                    {
                        // Only this registered root/image consumes the launch
                        // seed. A descendant or later exec cannot acquire it.
                        // Drop the temporary reader at this handoff; the actual
                        // root's descriptor remains live until local validation.
                        overrides.initial_input =
                            input.take().map(|input| input.lease.identity().clone());
                    }
                }
                R::MarkPastFirstExecve(Box::new(overrides))
            }
            // Requested by the parent thread:
            GlobalRequest::CreateChildThread(
                dettid,
                parent_detpid,
                ctid,
                flags,
                exit_signal,
                physical_ids,
                priority,
            ) => {
                if let Some(prepared) = &exec_reconnect {
                    let mut sched = self.lock_rpc_scheduler(false).await;
                    let (mut pending, post_exec_mm) = {
                        let mut states = self.pending_exec_states.lock().unwrap();
                        let Some(pending) = states.remove(&parent_detpid) else {
                            return (None, R::ThreadExited);
                        };
                        assert_eq!(&pending, prepared);
                        let post_exec_mm = pending.mm.for_exec(pending.process);
                        (pending, post_exec_mm)
                    };
                    assert_eq!(pending.process, parent_detpid);
                    if let Some((physical_pid, physical_tid)) = physical_ids
                        && let Err(open_error) = sched.register_physical_thread(
                            dettid,
                            post_exec_mm,
                            physical_pid,
                            physical_tid,
                        )
                    {
                        error!(
                            "[detcore, dtid {}] failed to register post-exec host process {} thread {}: {}",
                            dettid, physical_pid, physical_tid, open_error,
                        );
                        return (None, R::ThreadExited);
                    }
                    let retired = sched.reconnect_after_exec(ExecReconnect {
                        caller: pending.caller,
                        new_leader: dettid,
                        detpid: parent_detpid,
                        pre_exec_mm: pending.mm,
                        post_exec_mm,
                        child_tid_addr: ctid,
                        reconnect_priority: priority,
                    });
                    if pending.caller != dettid {
                        self.global_time
                            .lock()
                            .unwrap()
                            .reassign_thread(pending.caller, dettid);
                    }
                    // Linux marked the old thread group's robust-list words
                    // during the exec, which this message from the new image
                    // follows, and the reconnect has retired the threads the
                    // exec killed. Wake one waiter per marked word now, under
                    // the same lock: the scheduler commits no turn between the
                    // exec's grant and this reconnect, and the admissions are
                    // drained in canonical order
                    // (https://github.com/rrnewton/hermit/issues/4268).
                    let robust_wakes = std::mem::take(&mut pending.fd_blocking.robust_wakes);
                    wake_exec_robust_waiters(&mut sched, &robust_wakes, "exec");
                    if !pending.fd_blocking.is_empty() {
                        self.post_exec_fd_blocking
                            .lock()
                            .unwrap()
                            .insert(dettid, pending.fd_blocking);
                    }
                    debug!(
                        "[detcore, dtid {}] reconciled successful exec from caller {}; retired prior identities {:?}",
                        dtid, pending.caller, retired
                    );
                    R::CreateChildThread(Some(post_exec_mm))
                } else {
                    let registration = self
                        .recv_create_child_thread(
                            from,
                            request_mm,
                            ChildRegistration {
                                parent_dettid: DetTid::from_raw(from.into()),
                                parent_detpid,
                                child_dettid: dettid,
                                child_tid_addr: ctid,
                                flags,
                                exit_signal,
                                physical_ids,
                                maybe_priority: priority,
                                parent_is_kernel_blocked: false,
                            },
                        )
                        .await;
                    #[cfg(feature = "physical-mm-test-support")]
                    self.initial_mm_registration_result(dettid, &registration);
                    match registration {
                        SchedulerRpcResult::Continue(()) => R::CreateChildThread(None),
                        SchedulerRpcResult::ThreadExited => R::ThreadExited,
                    }
                }
            }
            // Requested by the vfork child on behalf of its kernel-blocked parent:
            GlobalRequest::CreateVforkChildThread(
                parent_dettid,
                parent_detpid,
                child_dettid,
                ctid,
                flags,
                exit_signal,
                priority,
            ) => match self
                .recv_create_child_thread(
                    from,
                    request_mm,
                    ChildRegistration {
                        parent_dettid,
                        parent_detpid,
                        child_dettid,
                        child_tid_addr: ctid,
                        flags: Some(flags),
                        exit_signal,
                        physical_ids: None,
                        maybe_priority: priority,
                        parent_is_kernel_blocked: true,
                    },
                )
                .await
            {
                SchedulerRpcResult::Continue(()) => R::CreateChildThread(None),
                SchedulerRpcResult::ThreadExited => R::ThreadExited,
            },
            // Requested by the child thread itself:
            GlobalRequest::StartNewThread(dettid, detpid, physical_ids, signal_identity) => {
                match self
                    .recv_start_new_thread(
                        from,
                        dettid,
                        detpid,
                        request_mm,
                        physical_ids,
                        signal_identity,
                    )
                    .await
                {
                    SchedulerRpcResult::Continue(history) => R::StartNewThread(history),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            GlobalRequest::DeregisterThread(deregistration) => {
                R::DeregisterThread(self.recv_deregister_thread(from, deregistration).await)
            }
            GlobalRequest::PublishedGuestSigmask(thread) => R::PublishedGuestSigmask(
                self.lock_rpc_scheduler(false)
                    .await
                    .published_guest_sigmask(thread),
            ),
            GlobalRequest::SetSavedGuestSigmask(mask) => {
                let updated = self
                    .lock_rpc_scheduler(false)
                    .await
                    .set_saved_guest_sigmask(dtid, mask);
                if updated {
                    R::SetSavedGuestSigmask(())
                } else {
                    R::ThreadExited
                }
            }
            GlobalRequest::SetSigtimedwaitSet(set) => {
                let updated = self
                    .lock_rpc_scheduler(false)
                    .await
                    .set_sigtimedwait_set(dtid, set);
                if updated {
                    R::SetSigtimedwaitSet(())
                } else {
                    R::ThreadExited
                }
            }
            GlobalRequest::RecordGuestMarkerSignal => {
                self.lock_rpc_scheduler(false)
                    .await
                    .record_guest_marker_signal();
                R::RecordGuestMarkerSignal(())
            }
            GlobalRequest::OwnProcessSignalInputs => R::OwnProcessSignalInputs(
                self.lock_rpc_scheduler(false)
                    .await
                    .own_process_signal_inputs(dtid),
            ),
            GlobalRequest::SetChildTidAddress(address) => {
                let updated = self
                    .lock_rpc_scheduler(false)
                    .await
                    .set_child_tid_address(dtid, address);
                if updated {
                    R::SetChildTidAddress(())
                } else {
                    R::ThreadExited
                }
            }
            GlobalRequest::FutexAction(dettid, action, futexid, init_read, mask) => R::FutexAction(
                self.recv_futex_action(
                    RpcIncarnation {
                        dettid,
                        mm: request_mm,
                    },
                    action,
                    futexid,
                    init_read,
                    mask,
                )
                .await,
            ),
            GlobalRequest::RobustListWakes(wakes) => {
                R::RobustListWakes(self.recv_robust_list_wakes(dtid, wakes))
            }
            GlobalRequest::DeterminizeInode(ino, observed, sighting, procfs) => {
                R::DeterminizeInode(
                    self.recv_determinize_inode(from, ino, procfs, observed, sighting)
                        .await,
                )
            }
            GlobalRequest::DeterminizeMappingInodes(inodes) => R::DeterminizeMappingInodes(
                self.recv_determinize_mapping_inodes(from, inodes).await,
            ),
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
            GlobalRequest::DeterminizeDevice(dev) => {
                R::DeterminizeDevice(self.recv_determinize_device(from, dev).await)
            }
            GlobalRequest::DeterminizeMountId(raw_mount_id, fallback_order) => {
                R::DeterminizeMountId(
                    self.recv_determinize_mount_id(from, raw_mount_id, fallback_order.as_deref())
                        .await,
                )
            }
            GlobalRequest::ResolveMountinfoIds(mountinfo_order, observation_order) => {
                R::ResolveMountinfoIds(
                    self.recv_resolve_mountinfo_ids(from, &mountinfo_order, &observation_order)
                        .await,
                )
            }
            GlobalRequest::RetireInode(ino) => {
                R::RetireInode(self.recv_retire_inode(from, ino).await)
            }
            GlobalRequest::ForgetRetiredInode(ino, procfs) => {
                R::ForgetRetiredInode(self.recv_forget_retired_inode(from, ino, procfs).await)
            }
            GlobalRequest::DirectorySnapshotMark => {
                let _sched = self.lock_rpc_scheduler(false).await;
                let inodes = self.inodes.lock().unwrap();
                R::DirectorySnapshotMark(SnapshotMark {
                    retirements: inodes.retirements,
                    threads_counted: inodes.threads_counted,
                })
            }
            GlobalRequest::ReadyChannel(channel, wants) => R::ReadyChannel(
                self.lock_rpc_scheduler(false)
                    .await
                    .ready_channel(dtid, channel, wants),
            ),
            GlobalRequest::HandOffTurn => {
                R::HandOffTurn(self.lock_rpc_scheduler(false).await.hand_off_turn(dtid))
            }
            GlobalRequest::RecordFatalSignal => {
                let mut scheduler = self.lock_rpc_scheduler(false).await;
                if let Some(process) = scheduler.registered_process(dtid) {
                    scheduler.record_committed_fatal_signal(process);
                }
                R::RecordFatalSignal(())
            }
            GlobalRequest::TouchFile(ino) => R::TouchFile(self.recv_touch_file(from, ino).await),
            GlobalRequest::SetFileMtime(ino, mtime) => {
                R::SetFileMtime(self.recv_set_file_mtime(from, ino, mtime).await)
            }
            GlobalRequest::GlobalTimeLowerBound => {
                let ns = self.global_time.lock().unwrap().as_nanos();
                R::GlobalTimeLowerBound(ns)
            }
            GlobalRequest::TraceSchedEvent(ev, detpid, command_bootstrap) => {
                match self
                    .recv_trace_schedevent(ev, detpid, request_mm, command_bootstrap)
                    .await
                {
                    SchedulerRpcResult::Continue(response) => R::TraceSchedEvent(response),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#663)
            // TODO-HUMAN-REVIEW(#869)
            GlobalRequest::RegisterAlarm(dpid, dtid, duration, interval, sig) => {
                let now = self.global_time.lock().unwrap().as_nanos();
                // O1: a dequeue earlier in this turn restarted the timer, so
                // the old value reports that restart
                // (https://github.com/rrnewton/hermit/issues/4139).
                self.sched.lock().unwrap().reconcile_alarm(dpid, now);
                let response = match self
                    .recv_register_alarm(
                        dpid,
                        RpcIncarnation {
                            dettid: dtid,
                            mm: request_mm,
                        },
                        now,
                        duration,
                        interval,
                        sig,
                    )
                    .await
                {
                    SchedulerRpcResult::Continue(remaining) => R::RegisterAlarm(remaining),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                };
                self.sched.lock().unwrap().note_arming_hint(dtid, now);
                response
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-841): Review logical alarm query RPC.
            GlobalRequest::AlarmRemaining(dpid) => {
                let now = self.global_time.lock().unwrap().as_nanos();
                let mut sched = self.lock_rpc_scheduler(false).await;
                // O1: reconcile a dequeue earlier in this turn first, and end
                // the caller's slice at a deadline it restarts
                // (https://github.com/rrnewton/hermit/issues/4139).
                if sched.reconcile_alarm(dpid, now).is_some() {
                    sched.note_arming_hint(dtid, now);
                }
                match sched.itimer_snapshot(dpid, now) {
                    Ok(snapshot) => R::AlarmRemaining(snapshot),
                    Err(error) => {
                        sched.fail_parked(dtid, ProtocolFailure::Timer(error));
                        R::ThreadExited
                    }
                }
            }
            // O2: a signalfd read or `rt_sigtimedwait` completion in this
            // turn may have dequeued the process's ITIMER_REAL copy
            // (https://github.com/rrnewton/hermit/issues/4139).
            GlobalRequest::NoteGuestSigalrm(target) => {
                self.sched.lock().unwrap().note_guest_sigalrm(dtid, target);
                R::NoteGuestSigalrm
            }
            GlobalRequest::NoteSigalrmDisposition => {
                self.sched.lock().unwrap().note_sigalrm_disposition(dtid);
                R::NoteGuestSigalrm
            }
            GlobalRequest::ReconcileAlarm(dpid) => {
                let now = self.global_time.lock().unwrap().as_nanos();
                let mut sched = self.sched.lock().unwrap();
                if sched.reconcile_alarm(dpid, now).is_some() {
                    sched.note_arming_hint(dtid, now);
                }
                R::ReconcileAlarm
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#869)
            GlobalRequest::RegisterPosixTimer(
                dpid,
                dtid,
                timer_id,
                deadline,
                interval,
                notification,
            ) => {
                let now = self.global_time.lock().unwrap().as_nanos();
                let response = match self
                    .recv_register_posix_timer(
                        dpid,
                        RpcIncarnation {
                            dettid: dtid,
                            mm: request_mm,
                        },
                        timer_id,
                        deadline,
                        interval,
                        notification,
                    )
                    .await
                {
                    SchedulerRpcResult::Continue(()) => R::RegisterPosixTimer(()),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                };
                self.sched.lock().unwrap().note_arming_hint(dtid, now);
                response
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#663)
            GlobalRequest::ResolveKillTargets(dpid) => {
                let scheduler = self.lock_rpc_scheduler(false).await;
                R::ResolveKillTargets(
                    scheduler.process_signal_targets(dpid),
                    scheduler.exited_process_is_zombie(dpid),
                )
            }
            GlobalRequest::NamesHermitTask(caller, tgid, tid) => {
                R::NamesHermitTask(hermit_task_named(caller, tgid, tid))
            }
            GlobalRequest::HoldTurnForGroupExit(dettid) => R::HoldTurnForGroupExit(
                self.lock_rpc_scheduler(false)
                    .await
                    .hold_turn_for_group_exit(dettid),
            ),
            GlobalRequest::NotifySignalPending(
                dettid,
                SigWrapper(signal),
                target_process,
                scope,
            ) => {
                let (fence, valve) = {
                    let mut scheduler = self.lock_rpc_scheduler(false).await;
                    scheduler.notify_signal_pending(dettid, SigWrapper(signal));
                    let dbt = self.cfg.backend.requires_thread_directed_process_signals;
                    // kill, rt_sigqueueinfo and a pidfd without
                    // PIDFD_SIGNAL_THREAD name a process; tgkill, tkill,
                    // rt_tgsigqueueinfo and a pidfd with PIDFD_SIGNAL_THREAD
                    // name a thread (the notice's SignalScope), whose SIGKILL ends its
                    // whole group only while that thread is alive
                    // (https://github.com/rrnewton/hermit/issues/4362; Claude
                    // review of https://github.com/rrnewton/hermit/pull/4439, R1).
                    let process_directed = scope == SignalScope::Process;
                    let target_process = if signal == libc::SIGKILL && dbt {
                        target_process.or_else(|| scheduler.registered_process(dettid))
                    } else {
                        target_process
                    };
                    let mut fence = None;
                    if signal == libc::SIGKILL {
                        scheduler.record_committed_sigkill(dettid, target_process);
                        if let Some(detpid) = target_process {
                            if dbt {
                                fence =
                                    fence_or_refuse(&scheduler, dettid, detpid, process_directed);
                            }
                            scheduler.note_process_sigkill(dettid, detpid, process_directed);
                        }
                    }
                    (fence, scheduler.sigkill_fence_valve)
                };
                // The sender's kill does not return until the victim process
                // is gone, as ptrace's SIGKILL fence makes it wait: a process
                // pidfd reports exit after the kernel has closed the victim's
                // files and sent its parent SIGCHLD. Only then can the sender,
                // which still holds the serial grant, observe the death, at a
                // point the schedule fixes (Claude review of
                // https://github.com/rrnewton/hermit/pull/4400, F1).
                if let Some((detpid, pidfd)) = fence
                    && let Err(reason) = wait_for_process_exit(pidfd, valve).await
                {
                    crate::scheduler::exit_on_scheduler_refusal(
                        &crate::scheduler::sigkill_retirement::UnfencedSigkill {
                            process: detpid,
                            reason,
                        },
                    );
                }
                R::NotifySignalPending(())
            }
            GlobalRequest::NotifyUnresolvedSigkill(group) => {
                self.lock_rpc_scheduler(false)
                    .await
                    .record_unresolved_sigkill(group);
                R::NotifyUnresolvedSigkill(())
            }
            GlobalRequest::ReserveSigkill(sender, target, uid) => {
                let mut scheduler = self.lock_rpc_scheduler(false).await;
                let reserved = scheduler
                    .registered_process(target)
                    .ok_or(crate::scheduler::sigkill_retirement::SigkillRefusal::NoTarget)
                    .and_then(|process| scheduler.reserve_sigkill(sender, process, uid));
                R::ReserveSigkill(reserved.ok())
            }
            GlobalRequest::ReserveSigkillGroup(sender, pgid, uid) => R::ReserveSigkill(
                self.lock_rpc_scheduler(false)
                    .await
                    .reserve_sigkill_group(sender, pgid, uid)
                    .ok(),
            ),
            GlobalRequest::CommitSigkill(token, sent) => {
                // A send that includes the sender is committed at its Exit
                // grant, before it is sent: its victims die only afterwards,
                // and its sender never returns, so it does not wait.
                let (victims, wait) = {
                    let mut sched = self.lock_rpc_scheduler(false).await;
                    let wait = sent && !sched.sigkill_reservation_includes_sender(token);
                    (sched.commit_sigkill(token, sent), wait)
                };
                // The sender's `kill` returns only once every victim's exit is
                // accounted and published to its parent. A parent that killed
                // its own child then finds the kernel's SIGCHLD pending when
                // `kill` returns, a point the schedule fixes, instead of
                // receiving it at a host-chosen moment in its own code. The
                // wait is bounded: past the valve the run is refused.
                let since = std::time::Instant::now();
                if wait {
                    loop {
                        {
                            let mut sched = self.lock_rpc_scheduler(false).await;
                            if sched.sigkill_settled(&victims) {
                                break;
                            }
                            if since.elapsed() >= sched.sigkill_fence_valve {
                                sched.refuse_unsettled_sigkill(since.elapsed());
                                break;
                            }
                        }
                        yield_once().await;
                    }
                }
                R::CommitSigkill(())
            }
            GlobalRequest::ThreadIsLive(dtid) => {
                R::ThreadIsLive(self.lock_rpc_scheduler(false).await.thread_is_live(dtid))
            }
            GlobalRequest::NamesARetiredExitedLeader(named) => R::NamesARetiredExitedLeader(
                self.lock_rpc_scheduler(false)
                    .await
                    .names_a_retired_exited_leader(named),
            ),
            GlobalRequest::ParentDeathSignal(arm) => {
                let mut sched = self.lock_rpc_scheduler(false).await;
                if let Some(signal) = arm {
                    sched.set_parent_death_signal(dtid, signal);
                }
                R::ParentDeathSignal(sched.parent_death_signal(dtid))
            }
            GlobalRequest::ThreadIdRefused(id) => {
                let (registered, zombie) = {
                    let sched = self.lock_rpc_scheduler(false).await;
                    (
                        sched.thread_was_registered(id),
                        sched.id_is_a_zombie_child_of(id, dtid),
                    )
                };
                let clocked = self.global_time.lock().unwrap().contains_thread(id);
                R::ThreadIdRefused((registered || clocked) && !zombie)
            }
            GlobalRequest::PendingRealTimeExpirations(detpid) => R::PendingRealTimeExpirations(
                self.lock_rpc_scheduler(false)
                    .await
                    .pending_real_time_expirations(detpid),
            ),
            GlobalRequest::RecordHandlerRestoreMask(detpid, unblocked) => {
                self.lock_rpc_scheduler(false)
                    .await
                    .record_handler_restore_unblocked(detpid, unblocked);
                R::RecordHandlerRestoreMask(())
            }
            GlobalRequest::RegisterSignalfd(detpid, mask) => {
                self.lock_rpc_scheduler(false)
                    .await
                    .register_signalfd(detpid, mask);
                R::RegisterSignalfd(())
            }
            GlobalRequest::DescriptorsReplaced(files, first, last) => {
                self.lock_rpc_scheduler(false)
                    .await
                    .note_descriptors_replaced(files, first, last);
                R::DescriptorsReplaced(())
            }
            GlobalRequest::DiscardPendingRealTimeExpirations(detpid, signals) => {
                self.lock_rpc_scheduler(false)
                    .await
                    .discard_pending_real_time_expirations(detpid, signals);
                R::DiscardPendingRealTimeExpirations(())
            }
            GlobalRequest::RecordHostTimedSignals(dtid, scope, signals) => R::HostTimedSignals(
                self.lock_rpc_scheduler(false)
                    .await
                    .record_host_timed_signals(dtid, scope, signals),
            ),
            GlobalRequest::HostTimedSignals(dtid) => R::HostTimedSignals(
                self.lock_rpc_scheduler(false)
                    .await
                    .host_timed_signals(dtid),
            ),
            GlobalRequest::HostTimedAndPrivateSigchld(dtid) => {
                let (host_timed, private) = self
                    .lock_rpc_scheduler(false)
                    .await
                    .host_timed_and_private_sigchld(dtid);
                R::HostTimedAndPrivateSigchld(host_timed, private)
            }
            GlobalRequest::ExactChildWaitState(parent, child) => R::ExactChildWaitState(
                self.lock_rpc_scheduler(false)
                    .await
                    .exact_child_wait_state(parent, child),
            ),
            GlobalRequest::ReadyChildWait(parent, selector) => {
                let sched = self.lock_rpc_scheduler(false).await;
                R::ReadyChildWait((
                    sched.ready_child_wait(parent, selector),
                    sched.has_child_wait_target(parent, selector),
                ))
            }
            GlobalRequest::ConsumeChildWait(parent, child) => R::ConsumeChildWait({
                let mut sched = self.lock_rpc_scheduler(false).await;
                if let Some(cpu) = work_span_cpu
                    && sched.thread_tree.parent_process(&child) == Some(parent)
                {
                    sched.work_span_note_reap(dtid, cpu, parent, child);
                }
                sched.consume_child_wait(parent, child)
            }),
            GlobalRequest::ProcessGroup(process) => R::ProcessGroup(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .process_group(process),
            ),
            GlobalRequest::SetProcessGroup(process, group) => R::SetProcessGroup(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .set_process_group(process, group),
            ),
            GlobalRequest::CreateSession(process) => R::CreateSession(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .create_session(process),
            ),
            GlobalRequest::RegisteredProcessOf(dettid) => R::RegisteredProcessOf(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .process_of(dettid),
            ),
            GlobalRequest::ForgetThreadTimerCopies(process, signo) => {
                self.lock_rpc_scheduler(false)
                    .await
                    .forget_thread_timer_copies(process, signo);
                R::ForgetThreadTimerCopies(())
            }
            GlobalRequest::ExecTimerFlush(process) => R::ExecTimerFlush(
                self.lock_rpc_scheduler(false)
                    .await
                    .take_exec_timer_flush(dtid, process),
            ),
            GlobalRequest::RetireExitedLeader => R::RetireExitedLeader(
                self.lock_rpc_scheduler(false)
                    .await
                    .retire_exited_leader(dtid, request_mm),
            ),
            GlobalRequest::GuestPid(caller, pid) => {
                let sched = self.lock_rpc_scheduler(false).await;
                R::GuestPid(sched.guest_pid(caller, pid))
            }
            GlobalRequest::ProcfsListing(process) => {
                R::ProcfsListing(self.lock_rpc_scheduler(false).await.procfs_listing(process))
            }
            GlobalRequest::ProcfsIds(to_kernel, ids) => R::ProcfsIds(
                self.lock_rpc_scheduler(false)
                    .await
                    .procfs_ids(to_kernel, &ids),
            ),
            GlobalRequest::ProcessImage(process) => {
                let sched = self.lock_rpc_scheduler(false).await;
                // `/proc/<tid>/exe` names a thread's process's image.
                let process = sched
                    .registered_process(DetTid::from_raw(process.as_raw()))
                    .unwrap_or(process);
                R::ProcessImage(
                    self.exec_images
                        .lock()
                        .unwrap()
                        .get(&process)
                        .cloned()
                        .map(Box::new),
                )
            }
            GlobalRequest::RecordProcessAuxv(bytes) => {
                let sched = self.lock_rpc_scheduler(false).await;
                let process = sched.registered_process(dtid).unwrap_or(dtid);
                self.process_auxv.lock().unwrap().record(process, bytes);
                R::RecordProcessAuxv(())
            }
            GlobalRequest::ProcessAuxv(process) => {
                let sched = self.lock_rpc_scheduler(false).await;
                // `/proc/<tid>/auxv` is a thread's process's.
                let process = sched
                    .registered_process(DetTid::from_raw(process.as_raw()))
                    .unwrap_or(process);
                R::ProcessAuxv(self.process_auxv.lock().unwrap().current(process))
            }
            GlobalRequest::MarkChangedRoot => {
                // The sending task's own filesystem group: a thread created
                // without CLONE_FS has its own root.
                let _sched = self.lock_rpc_scheduler(false).await;
                self.process_auxv.lock().unwrap().mark_changed_root(dtid);
                R::MarkChangedRoot(())
            }
            GlobalRequest::RootChanged(task) => {
                // The task the path names: a process's number names its
                // leader thread, whose root Linux's `/proc/<pid>` shows.
                let _sched = self.lock_rpc_scheduler(false).await;
                R::RootChanged(self.process_auxv.lock().unwrap().root_changed(task))
            }
            GlobalRequest::DescriptionLocks(open_file, locks, slots) => {
                let sched = self.lock_rpc_scheduler(false).await;
                let process = sched.registered_process(dtid).unwrap_or(dtid);
                let mut ledger = self.lock_ledger.lock().unwrap();
                match locks {
                    Ok(locks) => ledger.note_locks(process, open_file, locks, slots),
                    Err(reason) => ledger.note_unknown(process, open_file, reason, slots),
                }
                R::DescriptionLocks(())
            }
            GlobalRequest::TableReleased(open_file, remaining) => {
                let sched = self.lock_rpc_scheduler(false).await;
                let process = sched.registered_process(dtid).unwrap_or(dtid);
                self.lock_ledger
                    .lock()
                    .unwrap()
                    .release(process, open_file, remaining);
                R::TableReleased(())
            }
            GlobalRequest::TableExec(surviving) => {
                let sched = self.lock_rpc_scheduler(false).await;
                let process = sched.registered_process(dtid).unwrap_or(dtid);
                self.lock_ledger
                    .lock()
                    .unwrap()
                    .exec_kept(process, &surviving);
                R::TableExec(())
            }
            GlobalRequest::AliasedInode(inode) => {
                let _sched = self.lock_rpc_scheduler(false).await;
                self.lock_ledger.lock().unwrap().alias_inode(inode);
                R::AliasedInode(())
            }
            GlobalRequest::LockLedgerRefused(reason) => {
                let _sched = self.lock_rpc_scheduler(false).await;
                self.lock_ledger.lock().unwrap().refuse(reason);
                R::LockLedgerRefused(())
            }
            GlobalRequest::GuestLockRows(rows) => {
                // A positive owner is the kernel's number for a process in the
                // container's PID namespace, as `/proc` spells it; it is the
                // guest's when it names a process `/proc` lists, in the
                // guest's numbering. Hermit's own init is not listed.
                let sched = self.lock_rpc_scheduler(false).await;
                let owners = rows
                    .iter()
                    .filter_map(|row| i32::try_from(row.owner).ok().filter(|owner| *owner > 0))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let listed = sched.procfs_listing(None).processes;
                let guest_owners = owners
                    .iter()
                    .zip(sched.procfs_ids(false, &owners))
                    .filter(|(_, guest)| {
                        guest.is_some_and(|guest| listed.binary_search(&guest).is_ok())
                    })
                    .map(|(owner, _)| i64::from(*owner))
                    .collect::<BTreeSet<_>>();
                // A table a process that exited used, reaped or not,
                // references nothing (`LockLedger::prune`).
                let mut ledger = self.lock_ledger.lock().unwrap();
                ledger.prune(|process| sched.process_is_alive(process));
                // A referenced description whose locks are unknown could own
                // any OFD row (OFD rows are kept by field match against what a
                // description holds): the snapshot is refused
                // (`LockLedger::unknown`). Flock rows are kept by their taker's
                // pid alone, so for a description that only ever held flocks
                // this refusal is conservative.
                if let Some(reason) = ledger.unknown() {
                    R::GuestLockRows(Err(reason.to_string()))
                } else {
                    let held = ledger.held();
                    R::GuestLockRows(Ok(crate::procfs::locks::guest_lock_rows(
                        &rows,
                        |owner| guest_owners.contains(&owner),
                        &held,
                    )))
                }
            }
            GlobalRequest::AuxvFromKernel(why) => {
                let sched = self.lock_rpc_scheduler(false).await;
                let process = sched.registered_process(dtid).unwrap_or(dtid);
                let mut table = self.process_auxv.lock().unwrap();
                table.forget(process);
                R::AuxvFromKernel(table.first_kernel_file(why))
            }
            GlobalRequest::GroupOrSession(pid, session) => R::GroupOrSession(
                self.lock_rpc_scheduler(false)
                    .await
                    .group_or_session_of(pid, session),
            ),
            GlobalRequest::SetsidDecision(process) => R::SetsidDecision(
                self.lock_rpc_scheduler(false)
                    .await
                    .setsid_decision(process)
                    .map_err(|errno| errno.into_raw()),
            ),
            GlobalRequest::SetpgidDecision(caller, pid, pgid) => R::SetpgidDecision(
                self.lock_rpc_scheduler(false)
                    .await
                    .setpgid_decision(caller, pid, pgid)
                    .map_err(|errno| errno.into_raw()),
            ),
            GlobalRequest::ProcfsTask(named, thread) => {
                let scheduler = self.lock_rpc_scheduler(false).await;
                let existence = scheduler.procfs_task(named, thread);
                let mut answer = self
                    .inodes
                    .lock()
                    .unwrap()
                    .procfs_task_answer(named, thread, existence);
                if existence.0 && !existence.1 {
                    answer.mm = scheduler.thread_mm(thread.unwrap_or(named));
                    answer.runtime_private_memory = answer
                        .mm
                        .and_then(|mm| scheduler.runtime_private_memory(mm));
                }
                R::ProcfsTask(answer)
            }
            GlobalRequest::RenderedFileState(inode, change) => R::RenderedFileState(
                self.inodes
                    .lock()
                    .unwrap()
                    .rendered_file_state(inode, change),
            ),
            GlobalRequest::LocalPid(caller, task) => R::LocalPid(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .pid_namespaces
                    .localize_for(caller, task),
            ),
            GlobalRequest::SignalTargetProcess(named) => R::SignalTargetProcess(
                self.lock_rpc_scheduler(false)
                    .await
                    .signal_target_process(named),
            ),
            GlobalRequest::PosixTimerThread(process, named) => R::PosixTimerThread(
                self.lock_rpc_scheduler(false)
                    .await
                    .posix_timer_thread(process, named),
            ),
            GlobalRequest::SetSigchldDisposition(process, ignored, nocldwait) => {
                R::SetSigchldDisposition(
                    self.lock_rpc_scheduler(false)
                        .await
                        .thread_tree
                        .set_sigchld_disposition(process, ignored, nocldwait),
                )
            }
            GlobalRequest::SetChildSubreaper(process, on) => R::SetChildSubreaper(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .set_child_subreaper(process, on),
            ),
            GlobalRequest::UnrecoverableShutdown => {
                self.force_shutdown_with_error();
                R::UnrecoverableShutdown(())
            }
            GlobalRequest::Network(request) => {
                let _sched = self.lock_rpc_scheduler(false).await;
                let mut engine = self
                    .network
                    .as_ref()
                    .expect("network request without a network trace mode")
                    .lock()
                    .unwrap();
                let now = self.global_time.lock().unwrap().as_nanos();
                R::Network(engine.apply(now, request))
            }
            GlobalRequest::RequestPort(open_file_id) => {
                let _sched = self.lock_rpc_scheduler(false).await;
                let mut mut_used_ports = self.used_ports.lock().unwrap();
                self.update_port_range();
                let total_available =
                    self.port_end_range.load(SeqCst) - self.port_start_range.load(SeqCst);
                let mut index = 0;
                while (*mut_used_ports).contains(&self.next_port.load(SeqCst))
                    && index < total_available
                {
                    self.next_port.fetch_add(1, SeqCst);
                    if self.next_port.load(SeqCst) > self.port_end_range.load(SeqCst) {
                        self.next_port
                            .store(self.port_start_range.load(SeqCst), SeqCst);
                    }
                    index += 1;
                }
                if index == total_available {
                    R::PortFull
                } else {
                    (*mut_used_ports).insert(self.next_port.load(SeqCst));
                    let mut open_file_to_port = self.open_file_to_port.lock().unwrap();
                    open_file_to_port.insert(open_file_id, self.next_port.load(SeqCst));
                    R::RequestPort(self.next_port.load(SeqCst))
                }
            }
            GlobalRequest::AddUsedPort(port, open_file_id) => {
                let _sched = self.lock_rpc_scheduler(false).await;
                let mut used_ports = self.used_ports.lock().unwrap();
                used_ports.insert(port);
                let mut open_file_to_port = self.open_file_to_port.lock().unwrap();
                open_file_to_port.insert(open_file_id, port);
                R::AddUsedPort
            }
            GlobalRequest::ReleasePort(open_file_id) => {
                let _sched = self.lock_rpc_scheduler(false).await;
                let mut used_ports = self.used_ports.lock().unwrap();
                let mut open_file_to_port = self.open_file_to_port.lock().unwrap();
                let port = open_file_to_port.remove(&open_file_id);
                if let Some(port) = port {
                    used_ports.remove(&port);
                }
                R::ReleasePort(port)
            }
        };

        #[cfg(feature = "physical-mm-test-support")]
        if matches!(&resp, R::MarkPastFirstExecve(_)) {
            // Mark's state/input updates are complete and every match-arm
            // mutex guard is out of scope. The ordinary terminal recheck below
            // still follows this checkpoint before any reply can be returned.
            self.initial_mm_checkpoint(initial_mm_probe::Point::PostExec, dtid, request_mm)
                .await;
        }

        // Awaited scheduler operations may have raced logical teardown. Never return their
        // operation-specific response after the sender acquired a permanent tombstone.
        // A SIGKILL committed meanwhile leaves the request unanswered, whatever
        // its operation-specific response, ThreadExited included.
        let (sender_became_terminal, sender_sigkill_retired) =
            if is_deregister || exec_reconnect.is_some() || is_exec_caller_after_local_mm_swap {
                (false, false)
            } else {
                let sched = self.lock_rpc_scheduler(consuming_cleanup).await;
                (
                    sched.thread_is_logically_killed(dtid)
                        || !sched.rpc_incarnation_matches(dtid, request_mm),
                    !answered_for_sigkill_victim && sched.is_unaccounted_sigkill_victim(dtid),
                )
            };
        if sender_sigkill_retired {
            return (None, R::Retired);
        }
        if resp == R::ThreadExited || sender_became_terminal {
            return (None, R::ThreadExited);
        }

        let time_from_sched = self.global_time.lock().unwrap().threads_time(dtid);
        let time_update = match time_from_sched.cmp(&time_from_guest) {
            Ordering::Equal => None,
            Ordering::Less => {
                panic!(
                    "internal error: thread time should never go down, only monotonically up: time in sched {}, thread local time was {}",
                    time_from_sched, time_from_guest
                )
            }
            Ordering::Greater => Some(time_from_sched),
        };
        (time_update, resp)
    }

    async fn recv_resources_with_origin(
        &self,
        from: Tid,
        detpid: DetPid,
        mut rs: Resources,
        request_mm: Option<MmId>,
        rpc: RpcOrigin,
        capability: ControlCapability,
    ) -> (
        SchedulerRpcResult<ResourceReply>,
        Option<LogicalTime>,
        MaybePrintStack,
    ) {
        let dettid = DetTid::from_raw(from.into()); // TODO(T78538674): FIXME
        let mut boundary_stack = None;
        let resp2 = {
            let mut sched = self.lock_rpc_scheduler(false).await;
            if sched.thread_is_logically_killed(dettid)
                || request_mm.is_some_and(|mm| !sched.rpc_incarnation_matches(dettid, mm))
            {
                return (SchedulerRpcResult::ThreadExited, None, None);
            }
            // A host-timed request (a background operation's rejoin) can pass
            // admission just before a SIGKILL commit retires its thread. The
            // handler's response exit turns this into an unanswered request.
            if sched.is_unaccounted_sigkill_victim(dettid) {
                return (SchedulerRpcResult::ThreadExited, None, None);
            }
            let Some(nextturn) = sched.next_turns.get(&dettid).cloned() else {
                panic!(
                    "Detcore internal error: no entry for dettid {} in next_turns during resource request.",
                    dettid
                );
            };
            trace!(
                "[detcore, dtid {}] ResourceRequest, filling request into {}",
                &dettid, &nextturn.req
            );
            if let Some(mm) = request_mm
                && let Err(error) = sched.install_resource_origin(
                    dettid,
                    ResourceOrigin {
                        rpc,
                        mm,
                        control: capability,
                    },
                )
            {
                sched.fail_parked(dettid, error);
                return (SchedulerRpcResult::ThreadExited, None, None);
            }
            if let Some(boundary) = rs.slice_boundary.take() {
                let mm = request_mm.unwrap_or_else(|| {
                    crate::scheduler::exit_on_scheduler_refusal(
                        &"slice-boundary protocol refusal: unauthenticated request",
                    )
                });
                if rs.tid != dettid {
                    crate::scheduler::exit_on_scheduler_refusal(
                        &"slice-boundary protocol refusal: resource owner differs from sender",
                    );
                }
                boundary_stack = sched
                    .accept_slice_boundary(dettid, mm, &boundary)
                    .unwrap_or_else(|error| crate::scheduler::exit_on_scheduler_refusal(&error));
                if boundary_stack.is_some() {
                    self.signal_requested_stacktrace(dettid);
                }
            }
            sched.request_put(&nextturn.req, rs.clone(), &self.global_time);
            nextturn.resp
        };
        trace!(
            "[detcore, dtid {}] waiting on {} for resources: {:?}",
            dettid, &resp2, rs
        );
        let answer = resp2.get().await; // Block on the scheduler allowing our guest to proceed.
        let (result, time) = self
            .finish_resource_response(from, detpid, rs, request_mm, answer)
            .await;
        if matches!(result, SchedulerRpcResult::ThreadExited) {
            (result, time, None)
        } else {
            (result, time, boundary_stack)
        }
    }

    async fn finish_resource_response(
        &self,
        from: Tid,
        detpid: DetPid,
        rs: Resources,
        request_mm: Option<MmId>,
        answer: SchedResponse,
    ) -> (SchedulerRpcResult<ResourceReply>, Option<LogicalTime>) {
        let dettid = DetTid::from_raw(from.as_raw());
        let request_became_stale = {
            let sched = self.lock_rpc_scheduler(false).await;
            sched.thread_is_logically_killed(dettid)
                || request_mm.is_some_and(|mm| !sched.rpc_incarnation_matches(dettid, mm))
        };
        if request_became_stale {
            // `logically_kill_thread` wakes an already-pending request with a
            // signal response.  Treat that wake-up as terminal: otherwise a
            // caller that ignores `ResumeStatus::Signaled` can inject the
            // original syscall after the thread was logically removed.
            // TODO-HUMAN-REVIEW(PR-1023): Review pending SaBRe resource-request cancellation.
            trace!(
                "[detcore, dtid {}] terminating pending request after logical removal",
                dettid
            );
            return (SchedulerRpcResult::ThreadExited, None);
        }
        if matches!(answer, SchedResponse::InGuestFatalAlarm) {
            let mut sched = self.lock_rpc_scheduler(false).await;
            if !self.cfg.sequentialize_threads
                || !sched.in_guest_fatal_alarm_response_matches(dettid, detpid, request_mm, &rs)
            {
                sched.fail_parked(dettid, ProtocolFailure::UnexpectedControl);
                return (SchedulerRpcResult::ThreadExited, None);
            }
            return (
                SchedulerRpcResult::Continue(ResourceReply::Grant(ResumeStatus::InGuestFatalAlarm)),
                None,
            );
        }
        // A control spends only this response transport, not the resource
        // operation. It must precede exit-group or normal grant side effects.
        if let SchedResponse::ObserveSignal(control) = answer {
            return (
                SchedulerRpcResult::Continue(ResourceReply::ObserveSignal(control)),
                None,
            );
        }
        if let Some((true, process, mm)) = rs.exit_identity() {
            info!(
                "Scheduler authorized an exit-group scenario, from dettid {} / detpid {}",
                dettid, detpid
            );
            // Before allowing an `exit_group` to physically proceed, we
            // deregister the other threads in the thread group to reflect the
            // fact that they will not receive further logical turns.
            //
            // We trust the kernel to physically kill them irrespective of what they're
            // blocked on, including us having blocked them in the `futex_waiters` list.
            {
                let mut sched = self.lock_rpc_scheduler(false).await;
                if sched.thread_is_logically_killed(dettid)
                    || request_mm.is_some_and(|mm| !sched.rpc_incarnation_matches(dettid, mm))
                {
                    return (SchedulerRpcResult::ThreadExited, None);
                }
                for tid in sched.thread_tree.my_thread_group(&dettid) {
                    // We don't need to do anything extra for our own thread. That can use the
                    // same mechanics as a normal exit:
                    if tid != dettid {
                        sched.logically_kill_thread(&tid, &process, mm);
                    }
                }
            }
        }

        match answer {
            SchedResponse::ObserveSignal(_) | SchedResponse::InGuestFatalAlarm => {
                self.sched
                    .lock()
                    .unwrap()
                    .fail_parked(dettid, ProtocolFailure::UnexpectedControl);
                (SchedulerRpcResult::ThreadExited, None)
            }
            // In this context, SchedValue
            SchedResponse::Go(Some(schedval)) => {
                trace!(
                    "[dtid {}] resources granted, resuming normally: {:?}",
                    dettid, rs
                );

                let endtime_update = match schedval {
                    // Only syscalls timeout, and they don't need to update guest timeslice end.
                    // Nor does a futex requeue's answer, which a resource grant never carries.
                    SchedValue::TimeOut | SchedValue::Signaled(_) | SchedValue::Requeued { .. } => {
                        None
                    }
                    SchedValue::Value(timeslice) => Some(LogicalTime::from_nanos(timeslice)),
                };
                (
                    SchedulerRpcResult::Continue(ResourceReply::Grant(ResumeStatus::Normal)),
                    endtime_update,
                )
            }
            SchedResponse::GoAlone(schedval) => {
                trace!(
                    "[dtid {}] resources granted, and nothing else can run",
                    dettid
                );
                let endtime_update = match schedval {
                    Some(SchedValue::Value(timeslice)) => Some(LogicalTime::from_nanos(timeslice)),
                    _ => None,
                };
                (
                    SchedulerRpcResult::Continue(ResourceReply::Grant(
                        ResumeStatus::NothingElseCanRun,
                    )),
                    endtime_update,
                )
            }
            SchedResponse::Go(None) => {
                trace!(
                    "[dtid {}] resources granted but no timeslice specified",
                    dettid,
                );
                (
                    SchedulerRpcResult::Continue(ResourceReply::Grant(ResumeStatus::Normal)),
                    None,
                )
            }
            SchedResponse::Signaled(signal) => {
                trace!(
                    "[dtid {}] resources granted but interrupted by signal",
                    dettid,
                );
                (
                    SchedulerRpcResult::Continue(ResourceReply::Grant(ResumeStatus::Signaled(
                        signal,
                    ))),
                    None,
                )
            }
        }
    }

    async fn recv_release_resources(&self, from: Tid, rs: Resources) {
        // TODO(T78627377): add real resource-locking when we enable backgrounding actions.
        trace!("[detcore] Resources released to pid {}: {:?}", from, rs);
    }

    async fn recv_release_all_resources(&self, from: Tid) {
        // TODO(T78627377): add real resource-locking when we enable backgrounding actions.
        trace!("[detcore] All resources held by pid {} released", from);
    }

    /// Global portion of parent-forking-child protocol.  Called by the parent
    /// thread for an ordinary clone, or by the child itself for a vfork whose
    /// parent is blocked inside the kernel (`parent_is_kernel_blocked`).
    async fn recv_create_child_thread(
        &self,
        rpc_sender: Tid,
        request_mm: MmId,
        registration: ChildRegistration,
    ) -> SchedulerRpcResult<()> {
        let ChildRegistration {
            parent_dettid,
            parent_detpid,
            child_dettid,
            child_tid_addr: ctid,
            flags,
            exit_signal,
            physical_ids,
            maybe_priority,
            parent_is_kernel_blocked,
        } = registration;
        #[cfg(feature = "physical-mm-test-support")]
        if flags.is_none() && rpc_sender.as_raw() == child_dettid.as_raw() {
            self.initial_mm_checkpoint(
                initial_mm_probe::Point::BeforeRegistration,
                child_dettid,
                request_mm,
            )
            .await;
        }
        let initial_priority = if let Some(pr) = &self.preemptions_to_replay {
            assert!(maybe_priority.is_none());
            let prio = pr
                .thread_initial_priority(&child_dettid)
                .unwrap_or_else(|| {
                    warn!(
                        "Child thread {} not found in preemption history to replay",
                        child_dettid
                    );
                    DEFAULT_PRIORITY
                });
            if !is_ordinary_priority(prio) {
                panic!(
                    "Read a bad initial_prority from file: {}\nFull file: {}",
                    prio, pr,
                );
            }
            prio
        } else {
            let prio = maybe_priority.expect(
                "create_child_thread must take an initial priority unless replaying preemptions",
            );
            if !is_ordinary_priority(prio) {
                panic!(
                    "recv_create_child_thread received a bad prority argument : {}",
                    prio,
                );
            }
            prio
        };

        {
            let mut sched = self.lock_rpc_scheduler(false).await;
            let sender = DetTid::from_raw(rpc_sender.into());
            if sched.thread_is_logically_killed(sender)
                || !sched.rpc_incarnation_matches(sender, request_mm)
                || sched.thread_is_logically_killed(child_dettid)
            {
                return SchedulerRpcResult::ThreadExited;
            }
            // A process the backend already reported physically gone must not
            // be admitted: the scheduler would wait for a request it can never
            // send. A forked child is its own process.
            let child_detpid =
                if flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_THREAD)) {
                    parent_detpid
                } else {
                    DetPid::from_raw(child_dettid.as_raw())
                };
            if self.cfg.backend.process_exits_complete_asynchronously
                && sched.process_physically_gone(child_detpid)
            {
                drop(sched);
                self.report_backend_failure(reverie::BackendFailure {
                    pid: reverie::Pid::from_raw(child_detpid.as_raw()),
                    tid: Tid::from_raw(child_dettid.as_raw()),
                    phase: "a process was already physically gone when it was registered",
                });
                return SchedulerRpcResult::ThreadExited;
            }

            // A parent-authored child registration carries the parent's MM,
            // which naturally differs from the child's initial MM (including
            // when the parent has execed). It must retain the ordinary clone
            // transaction. Only an already registered, matching parent may
            // take that path; a malformed root self-request still reaches the
            // complete first-image checks below.
            let registered_parent_creation = sender == parent_dettid
                && parent_dettid != child_dettid
                && sched.thread_was_registered(sender);
            let initial_current_root = flags.is_none()
                && request_mm != MmId::initial(child_dettid)
                && !registered_parent_creation;
            if initial_current_root
                && (sender != child_dettid
                    || parent_dettid != child_dettid
                    || parent_detpid != child_dettid
                    || ctid != 0
                    || parent_is_kernel_blocked
                    || request_mm != MmId::initial(child_dettid).for_exec(child_dettid)
                    || self.root_dettid.get().is_some()
                    || self.initial_descriptor_image_complete.load(SeqCst)
                    || self.root_image_marked.load(SeqCst)
                    || self
                        .pending_exec_states
                        .lock()
                        .unwrap()
                        .contains_key(&child_dettid))
            {
                return SchedulerRpcResult::ThreadExited;
            }
            // The local Tool selected this MM synchronously under the genuine
            // initial-image lifecycle before this request began. Record both
            // identities at birth, not in a later Mark response/ack interval.
            if initial_current_root
                && !sched.record_initial_current_image_birth(child_dettid, request_mm)
            {
                return SchedulerRpcResult::ThreadExited;
            }

            if parent_is_kernel_blocked && self.cfg.sequentialize_threads {
                sched.complete_vfork_registration(parent_dettid, child_dettid);
            }

            let child_mm = if initial_current_root {
                request_mm
            } else {
                MmId::for_clone(
                    request_mm,
                    child_dettid,
                    flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_VM)),
                )
            };
            if let Some(owners) = &mut sched.boundary_owners {
                owners
                    .fresh(child_dettid, child_mm)
                    .unwrap_or_else(|error| crate::scheduler::exit_on_scheduler_refusal(&error));
            }
            if !initial_current_root {
                sched.register_reused_transferred_exec_tid(child_dettid, child_mm);
                sched.record_thread_mm(child_dettid, child_mm);
            }
            // The guest's procfs paths name a task by its thread ID in the
            // guest's PID namespace, which is the raw value of its DetTid.
            self.inodes
                .lock()
                .unwrap()
                .count_incarnation(child_dettid.as_raw());

            // A new thread on a reused tid has not exited, and has published
            // no mask.
            sched.forget_committed_exit(child_dettid);
            sched.published_masks.remove(&child_dettid);
            // Don't fill in the request, as the child will do it:
            let _entry = sched
                .next_turns
                .entry(child_dettid)
                .or_insert_with(|| ThreadNextTurn {
                    dettid: child_dettid,
                    child_tid_addr: ctid,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                    protocol: Default::default(),
                });

            {
                let is_group_leader = if let Some(f) = flags {
                    !f.contains(CloneFlags::CLONE_THREAD)
                } else {
                    true // root thread
                };
                sched.thread_tree.add_child_with_wait_metadata(
                    parent_dettid,
                    child_dettid,
                    is_group_leader,
                    flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_PARENT)),
                    exit_signal,
                );
                sched.work_span_note_spawn(parent_dettid, child_dettid);
                if is_group_leader {
                    sched.forget_previous_process_exit(child_dettid);
                    // The child's exit signal is posted to its parent at a
                    // host-timed moment: that pair stops being observable now,
                    // before the child can run (4262 design §3.4).
                    sched.child_ends_timer_observability(child_dettid);
                }
                let new_pid_namespace = is_group_leader
                    && flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_NEWPID));
                if new_pid_namespace {
                    sched.thread_tree.enter_new_pid_namespace(child_dettid);
                    sched.thread_tree.mark_pid_namespace_init(child_dettid);
                }
                // A nested task's pid in each namespace is fixed now, in its
                // creator's turn, before either runs again
                // (https://github.com/rrnewton/hermit/issues/4155).
                let pid_namespaces = &mut sched.thread_tree.pid_namespaces;
                let mut chain = pid_namespaces.chain_of(parent_dettid);
                if new_pid_namespace {
                    chain.push(child_dettid);
                }
                if let Some(reason) = pid_namespaces.untranslatable(parent_dettid) {
                    let reason = format!("its creator {parent_dettid}: {reason}");
                    pid_namespaces.mark_untranslatable(child_dettid, reason);
                } else if chain.is_empty() {
                    pid_namespaces.forget(child_dettid);
                } else {
                    let recorded =
                        crate::scheduler::pid_namespaces::read_nspid(child_detpid, child_dettid)
                            .and_then(|nspid| {
                                pid_namespaces
                                    .record(child_dettid, &chain, &nspid)
                                    .map_err(|error| error.to_string())
                            });
                    if let Err(reason) = recorded {
                        error!(
                            "[detcore, dtid {}] cannot record its pids in its PID namespaces: {}",
                            child_dettid, reason
                        );
                        pid_namespaces.mark_untranslatable(child_dettid, reason);
                    } else if is_group_leader {
                        sched.note_member_of_ended_namespace(child_dettid);
                    }
                }
                // A new task's root: its creator's filesystem group with
                // CLONE_FS, a copy of it without.
                self.process_auxv.lock().unwrap().clone_root(
                    parent_dettid,
                    child_dettid,
                    flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_FS)),
                );
                if is_group_leader && flags.is_some() {
                    // A new process runs its creator's image until it execs.
                    let mut images = self.exec_images.lock().unwrap();
                    match images.get(&parent_detpid).cloned() {
                        Some(image) => images.insert(child_dettid, image),
                        None => images.remove(&child_dettid),
                    };
                    drop(images);
                    self.process_auxv
                        .lock()
                        .unwrap()
                        .inherit(parent_detpid, child_dettid);
                    sched.inherit_real_time_records(parent_detpid, child_dettid);
                    // Its descriptor table: its creator's with CLONE_FILES, a
                    // copy without. Recorded here, in the creator's turn and
                    // before the child can run, so a lock the child takes
                    // first is never overwritten by its own fork
                    // (`procfs::locks::LockLedger::fork`).
                    self.lock_ledger.lock().unwrap().fork(
                        parent_detpid,
                        child_dettid,
                        flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_FILES)),
                    );
                }
                // A thread with a table of its own: the ledger counts its
                // calls as its process's, so that process's table can no
                // longer be followed (`procfs::locks::LockLedger::mark_opaque`).
                if !is_group_leader
                    && flags.is_some_and(|flags| !flags.contains(CloneFlags::CLONE_FILES))
                {
                    self.lock_ledger.lock().unwrap().mark_opaque(parent_detpid);
                }
            }

            if let Some((physical_pid, physical_tid)) = physical_ids {
                let child_is_thread =
                    flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_THREAD));
                let child_detpid = if child_is_thread {
                    parent_detpid
                } else {
                    child_dettid
                };
                if let Err(open_error) = sched.register_physical_thread(
                    child_dettid,
                    child_mm,
                    physical_pid,
                    physical_tid,
                ) {
                    error!(
                        "[detcore, dtid {}] cannot bind host process {} thread {} during child registration: {}",
                        child_dettid, physical_pid, physical_tid, open_error,
                    );
                    sched.logically_kill_thread(&child_dettid, &child_detpid, child_mm);
                    return SchedulerRpcResult::ThreadExited;
                }
            }

            // Bind only the existing root self-registration, after its
            // registration has succeeded. An arbitrary descendant's first
            // completion cannot claim a launcher-created input endpoint.
            if flags.is_none()
                && parent_dettid == child_dettid
                && parent_detpid == child_dettid
                && let Some(input) = self.inherited_input.lock().unwrap().as_mut()
                && input.root.is_none()
            {
                input.root = Some((child_dettid, child_mm));
            }
            // The same root self-registration names the thread whose first
            // image `Config::launch_image` describes.
            if flags.is_none() && parent_dettid == child_dettid && parent_detpid == child_dettid {
                let _ = self.root_dettid.set(child_dettid);
            }

            // Record this thread in deterministic creation order so a
            // happens-before anchor addressed by `spawn_ordinal` resolves to it.
            sched.hb_note_spawn(child_dettid);

            if self.cfg.replay_schedule_from.is_none() {
                // Give the thread an initial priority
                let old_prio = sched.priorities.insert(child_dettid, initial_priority);
                assert!(old_prio.is_none());
            } else {
                // In replay mode the schedule decides the thread's priority
                // (`Scheduler::replay_registration_priority`).
                if sched.replay_registration_priority(child_dettid, initial_priority) {
                    warn!(
                        "[detcore, dtid {}] registered before the replayed schedule switched to it, created by process {}; it takes its starting priority {}",
                        child_dettid, parent_detpid, initial_priority
                    );
                }
            }

            if let Some(pr) = &mut sched.preemption_writer {
                pr.register_thread(child_dettid, initial_priority);
            }

            // Describe *how* the admission side is chosen, but do not resolve it
            // (and in particular do not draw the post-fork PRNG) here: this
            // handler runs on whichever backend worker fielded the RPC, so on an
            // asynchronous backend (e.g. DBT, where the child self-registers
            // outside a scheduler turn) resolving the side now would consume the
            // PRNG draw in host RPC order. `admit_to_run_queue` resolves the
            // intent at the step2 drain -- which under ptrace is post-commit, in
            // schedule order, unchanged. Read its "What this does and does not
            // make deterministic" section before relying on that word: the drain
            // is a deterministic *point* and resolution within one drain is
            // `DetTid`-ordered, but on an asynchronous backend which drain a
            // given admission lands in is not itself schedule-determined unless
            // that admission is anchored (ordinary clone is, via the parent's
            // `ParentContinue`; vfork is, via `step2a`'s barrier). When threads
            // are not sequentialized, or the
            // parent is already kernel-blocked (vfork), the child takes the tail
            // and no PRNG is consumed.
            let intent = if self.cfg.sequentialize_threads && !parent_is_kernel_blocked {
                AdmitIntent::PostFork(self.cfg.runs_post_fork)
            } else {
                AdmitIntent::Fixed(AdmitSide::Back)
            };
            sched.admit_to_run_queue(child_dettid, intent);
            debug!(
                "[detcore] CreateChildThread with dtid {}: admit child via {:?}.",
                child_dettid, intent,
            );
            sched.started_up.try_put(());
        }
        #[cfg(feature = "physical-mm-test-support")]
        if flags.is_none() && rpc_sender.as_raw() == child_dettid.as_raw() {
            // No scheduler mutex lives across the test rendezvous. The real
            // ParentContinue call below is unchanged and has not been issued.
            self.initial_mm_checkpoint(
                initial_mm_probe::Point::RegisteredBeforeGrant,
                child_dettid,
                request_mm,
            )
            .await;
        }
        // The child queue position above determines which equal-priority side
        // gets the first turn when the parent requests ParentContinue.
        // A vfork parent is already blocked by the kernel and is not in the run
        // queue, so it must not issue a ParentContinue request here.
        if self.cfg.sequentialize_threads && !parent_is_kernel_blocked {
            let mut rs = Resources::new(parent_detpid);
            rs.insert(
                ResourceID::ParentContinue {
                    parent: parent_dettid,
                    child: child_dettid,
                },
                Permission::W,
            );
            if matches!(
                self.recv_grant_resources(
                    rpc_sender,
                    parent_detpid,
                    rs,
                    Some(request_mm),
                    RpcOrigin::ParentContinue
                )
                .await
                .0,
                SchedulerRpcResult::ThreadExited
            ) {
                return SchedulerRpcResult::ThreadExited;
            }
        }
        SchedulerRpcResult::Continue(())
    }

    /// Called by the child thread upon startup.
    /// Returns a thread-preemption history for the new guest thread (if --replay-preemptions-from
    /// is used).
    async fn recv_start_new_thread(
        &self,
        from: Tid,
        dettid: DetTid,
        detpid: DetPid,
        request_mm: MmId,
        physical_ids: Option<(i32, i32)>,
        signal_identity: Option<reverie::SignalTaskIdentity>,
    ) -> SchedulerRpcResult<Option<ThreadHistory>> {
        #[cfg(feature = "physical-mm-test-support")]
        self.initial_mm_checkpoint(initial_mm_probe::Point::ThreadStartup, dettid, request_mm)
            .await;
        let mut tries: u64 = 0;
        // TODO: eliminate this loop. Could instead signal with an ivar.
        let response_ivar = loop {
            yield_once().await;
            let mut sched = self.lock_rpc_scheduler(false).await;
            if sched.thread_is_logically_killed(dettid)
                || !sched.rpc_incarnation_matches(dettid, request_mm)
            {
                return SchedulerRpcResult::ThreadExited;
            }
            if self.cfg.backend.process_exits_complete_asynchronously
                && sched.process_physically_gone(detpid)
            {
                drop(sched);
                self.report_backend_failure(reverie::BackendFailure {
                    pid: reverie::Pid::from_raw(detpid.as_raw()),
                    tid: Tid::from_raw(dettid.as_raw()),
                    phase: "a process was already physically gone when it started",
                });
                return SchedulerRpcResult::ThreadExited;
            }
            if self.cfg.backend.requires_thread_directed_process_signals && physical_ids.is_none() {
                error!(
                    "[detcore, dtid {}] backend requires a host thread ID at StartNewThread",
                    dettid,
                );
                sched.logically_kill_thread(&dettid, &detpid, request_mm);
                return SchedulerRpcResult::ThreadExited;
            }
            // The resources that must be held for the fresh thread to run:
            let rsrcs = {
                let mut s = HashMap::new();
                s.insert(ResourceID::MemAddrSpace(detpid), Permission::RW); // TODO(T78055411): track mem aliasing.
                Resources {
                    tid: dettid,
                    resources: s,
                    poll_attempt: 0,
                    fyi: String::new(),
                    signal_interrupt_errno: None,
                    backend_runtime_bootstrap: false,
                    blocked_signal_mask: None,
                    blocked_signal_mask_unknown: false,
                    replay_served_from_log: false,
                    reads_signalfd: false,
                    read_fd: None,
                    polls_guest_state_only: false,
                    waits_until_nothing_else_can_run: false,
                    previous_attempt_made_progress: false,
                    upgraded_from_attempt: 0,
                    polls_channel: None,
                    poll_deadline: None,
                    in_guest_sleep_sigalrm: None,
                    signal_state_task: None,
                    decides_signal_interruption_itself: false,
                    continues_sigreturn: false,
                    private_sigchld_decides: false,
                    slice_boundary: None,
                }
            };
            let nextturn = match sched.next_turns.entry(dettid) {
                Entry::Vacant(_entry) => {
                    // CreateChildThread on the parent hasn't run yet.

                    // TODO: We could try to populate the entry since we get here
                    // first, but currently we lack the information right here to
                    // populate the child_tid_addr field.
                    if tries == 0 {
                        trace!(
                            "[detcore, dtid {}] thread showed up early, no queue entry yet.  Waiting...",
                            dettid
                        );
                    }
                    tries += 1;
                    continue;
                }
                Entry::Occupied(entry) => {
                    trace!(
                        "[detcore, dtid {}] handling StartNewThread rpc.  Found next_turns entry (after {} tries)",
                        from, tries
                    );
                    entry.get().clone()
                }
            };
            if let Some((physical_pid, physical_tid)) = physical_ids
                && let Err(open_error) =
                    sched.register_physical_thread(dettid, request_mm, physical_pid, physical_tid)
            {
                error!(
                    "[detcore, dtid {}] cannot bind host process {} thread {} for exact signal delivery: {}",
                    dettid, physical_pid, physical_tid, open_error,
                );
                sched.logically_kill_thread(&dettid, &detpid, request_mm);
                return SchedulerRpcResult::ThreadExited;
            }
            if sched.signal_control_installed() {
                let binding = signal_identity
                    .ok_or(TimerFailure::Identity)
                    .and_then(|identity| {
                        if from.as_raw() != dettid.as_raw()
                            || sched.registered_process(dettid) != Some(detpid)
                        {
                            return Err(TimerFailure::Identity);
                        }
                        sched.real_timers.bind(detpid, dettid, request_mm, identity)
                    });
                if let Err(error) = binding {
                    sched.fail_parked(dettid, ProtocolFailure::Timer(error));
                    return SchedulerRpcResult::ThreadExited;
                }
            }
            if let Err(error) = sched.install_resource_origin(
                dettid,
                ResourceOrigin {
                    rpc: RpcOrigin::ThreadStart,
                    mm: request_mm,
                    control: ControlCapability::None,
                },
            ) {
                sched.fail_parked(dettid, error);
                return SchedulerRpcResult::ThreadExited;
            }
            sched.request_put(&nextturn.req, rsrcs, &self.global_time);
            break nextturn.resp;
        };
        debug!(
            "[detcore, dtid {}] New thread will now wait for response on {}...",
            &dettid, &response_ivar
        );
        let answer = response_ivar.get().await;
        if matches!(
            answer,
            SchedResponse::ObserveSignal(_) | SchedResponse::InGuestFatalAlarm
        ) {
            self.sched
                .lock()
                .unwrap()
                .fail_parked(dettid, ProtocolFailure::UnexpectedControl);
            return SchedulerRpcResult::ThreadExited;
        }
        let request_became_stale = {
            let sched = self.lock_rpc_scheduler(false).await;
            sched.thread_is_logically_killed(dettid)
                || !sched.rpc_incarnation_matches(dettid, request_mm)
        };
        if request_became_stale {
            return SchedulerRpcResult::ThreadExited;
        }
        info!(
            "[detcore, dtid {}] New thread given go-ahead to proceed via {}",
            &dettid, &response_ivar
        );
        if let Some(pr) = &self.preemptions_to_replay {
            let (history, old_prio) = {
                let mut sched = self.lock_rpc_scheduler(false).await;
                if sched.thread_is_logically_killed(dettid)
                    || !sched.rpc_incarnation_matches(dettid, request_mm)
                {
                    return SchedulerRpcResult::ThreadExited;
                }
                let history = pr.extract_thread_record(&dettid).unwrap_or_else(|| {
                    warn!(
                        "Replaying preemptions, but no record found for thread {}",
                        dettid
                    );
                    ThreadHistory::new()
                });
                let old_prio = sched.priorities.insert(dettid, history.initial_priority());
                (history, old_prio)
            };
            debug!(
                "[replay-preemption] Enqueing new thread at priority {:?} (changed from {:?})",
                history.initial_priority(),
                old_prio,
            );
            SchedulerRpcResult::Continue(Some(history))
        } else {
            SchedulerRpcResult::Continue(None)
        }
    }

    /// Warning: this happens completely asynchronously, whenever the guest exit hook fires.
    /// Its timing is not coordinated by the scheduler.
    async fn recv_deregister_thread(&self, _from: Tid, mut deregistration: ThreadDeregistration) {
        // Keep the same scheduler -> pending-exec lock order as reconnect.
        let mut sched = self.sched.lock().unwrap();
        let ThreadDeregistration {
            dettid, detpid, mm, ..
        } = &deregistration;
        // A fatal signal can tear down the caller after its local state has advanced to the
        // candidate exec image but before the successful reconnect (or failed-exec cancel).
        // Retire that one-shot preparation before scheduler-incarnation admission rejects the
        // candidate image's final cleanup RPC.
        let mut pending = self.pending_exec_states.lock().unwrap();
        if pending.get(detpid).is_some_and(|state| {
            state.caller == *dettid && (*mm == state.mm || *mm == state.mm.for_exec(state.process))
        }) {
            let prepared = pending.remove(detpid).unwrap();
            // Unlike a recoverable failure (CancelExec), the caller's old image
            // deregistered before any reconnect: its group is dying, and Linux
            // walks every robust list of the group, in the exec or in each
            // thread's exit. On DBT this takes a fatal signal during the exec.
            // An exec whose target the kernel cannot load does not get here:
            // DynamoRIO's own exec succeeds, the new image reconnects and its
            // wakes are queued there, and the run then hangs on that image's
            // unretired death (https://github.com/rrnewton/hermit/issues/4362).
            // Hold turns until the kernel has ended the group, as an
            // exit_group's grant does, and wake the prepared waiters now, while
            // the caller's outstanding exec request still withholds every turn;
            // they run at the drain after the hold. A group that outlives this
            // is ended by the exited-thread valve with a named HERMIT_DEADLOCK
            // rather than losing the wakes (Codex review of
            // https://github.com/rrnewton/hermit/pull/4352, P2).
            if !prepared.fd_blocking.robust_wakes.is_empty() {
                sched.hold_turns_until_the_group_ends(*detpid);
                wake_exec_robust_waiters(
                    &mut sched,
                    &prepared.fd_blocking.robust_wakes,
                    "terminal exec",
                );
            }
            sched.finish_exec_teardown(prepared.caller, *detpid, prepared.mm, false);
            // The local caller can have installed its candidate exec MmId
            // before cancellation. It still owns the original registration;
            // consume that exact prepared incarnation rather than discarding
            // cleanup as a stale request after removing the preparation.
            deregistration.mm = prepared.mm;
        }
        drop(pending);

        // Invariant: will only be called when sequentialize-threads is on.
        assert!(self.cfg.sequentialize_threads);
        self.account_deregistered_thread(&mut sched, deregistration);
    }

    /// Consume final statistics and registration while holding admission closed.
    /// Authenticated exec cleanup uses this same once-only accounting path.
    fn account_deregistered_thread(
        &self,
        sched: &mut Scheduler,
        deregistration: ThreadDeregistration,
    ) {
        let ThreadDeregistration {
            dettid,
            detpid,
            mm,
            thread_start_entered: _,
            timeslice_stats,
            syscall_count,
            chaos_epochs,
        } = deregistration;
        if !sched.rpc_incarnation_matches(dettid, mm) {
            debug!(
                "[detcore, dtid {}] ignoring deregistration from retired exec incarnation {:?}",
                dettid, mm,
            );
            return;
        }
        self.post_exec_fd_blocking.lock().unwrap().remove(&dettid);
        // A SIGKILL victim's cleanup is accounted exactly once
        // (`scheduler::sigkill_retirement`), also on a backend that otherwise
        // accepts a repeated callback.
        if !sched.note_deregistration_accounted(dettid)
            || sched.sigkill_receipt_already_counted(dettid)
        {
            trace!(
                "[detcore, dtid {}] acknowledging already-accounted deregistration",
                dettid
            );
            return;
        }
        if let Some(writer) = &mut sched.preemption_writer {
            for transition in chaos_epochs {
                writer.insert_chaos_epoch(dettid, transition);
            }
        }
        sched.record_timeslice_stats(dettid, timeslice_stats);
        sched.record_syscall_count(dettid, syscall_count);
        // A SIGKILL victim is retired at its sender's turn, not here
        // (`scheduler::sigkill_retirement`): its receipt is accounted above
        // and only counted.
        if sched.route_sigkill_receipt(dettid)
            != crate::scheduler::sigkill_retirement::SigkillReceipt::Ordinary
        {
            return;
        }
        sched.retire_deregistered_thread(dettid, detpid, mm);
        trace!(
            "[detcore, dtid {}] thread deregistered, removed from sched structures.",
            dettid
        );
    }

    async fn recv_futex_action(
        &self,
        caller: RpcIncarnation,
        action: FutexAction,
        futexid: FutexID,
        init_read: i32,
        mask: u32,
    ) -> Option<SchedValue> {
        let RpcIncarnation { dettid, mm } = caller;
        trace!("[detcore, dtid {}] Futex action: {:?}", &dettid, action);
        let response_iv = {
            let mut sched = self.lock_rpc_scheduler(false).await;
            if sched.thread_is_logically_killed(dettid)
                || !sched.rpc_incarnation_matches(dettid, mm)
            {
                return Some(SchedValue::Value(nix::errno::Errno::EINTR as u64));
            }
            let Some(resp_iv) = sched
                .next_turns
                .get(&dettid)
                .map(|nextturn| nextturn.resp.clone())
            else {
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-845): Review late RPCs from exit-group siblings.
                trace!(
                    "[detcore, dtid {}] ignoring futex action after logical thread removal",
                    dettid
                );
                return Some(SchedValue::Value(nix::errno::Errno::EINTR as u64));
            };
            match action {
                FutexAction::WaitRequest(maybe_timeout, signal_watch) => {
                    if sched.child_tid_was_cleared(futexid, init_read) {
                        trace!(
                            "[detcore, dtid {}] late wait on cleared child-TID futex {:?}",
                            dettid, futexid
                        );
                        return Some(SchedValue::Value(0));
                    }
                    if let Err(error) = sched.install_resource_origin(
                        dettid,
                        ResourceOrigin {
                            rpc: RpcOrigin::FutexAction,
                            mm,
                            control: ControlCapability::None,
                        },
                    ) {
                        sched.fail_parked(dettid, error);
                        return Some(SchedValue::Value(nix::errno::Errno::EINTR as u64));
                    }
                    sched.sleep_futex_waiter(&dettid, futexid, maybe_timeout, mask, signal_watch);
                    // block on ivar, below
                }
                FutexAction::WaitFinished => {
                    return None;
                }
                FutexAction::WakeRequest(num_threads) => {
                    let num = sched.wake_futex_waiters(dettid, futexid, num_threads, mask);
                    if self.cfg.yields_after_futex_wake(num) {
                        sched.requeue_futex_waker(dettid);
                    }
                    return Some(SchedValue::Value(num));
                }
                FutexAction::WakeFinished(_num_threads) => {
                    return None;
                }
                FutexAction::RequeueRequest(to, nr_wake, nr_requeue) => {
                    let (woken, moved) = sched.requeue_futex_waiters(
                        futexid,
                        to,
                        nr_wake as usize,
                        nr_requeue as usize,
                    );
                    if self.cfg.yields_after_futex_wake(woken) {
                        sched.requeue_futex_waker(dettid);
                    }
                    return Some(SchedValue::Requeued { woken, moved });
                }
            }
            // Blocking on the FUTEX_WAIT here, remove ourselves:
            assert!(sched.run_queue.remove_tid(dettid));
            resp_iv
        };
        // Wait for wake+scheduler response.
        match response_iv.get().await {
            // A futex wait never asks for `GoAlone`.
            SchedResponse::Go(answer) | SchedResponse::GoAlone(answer) => {
                trace!(
                    "[detcore, dtid {}] Unblocked from futex_wait! ({})",
                    &dettid, &response_iv
                );
                answer
            }
            SchedResponse::Signaled(signals) => Some(SchedValue::Signaled(
                signals.iter().flatten().fold(0, |set, signal| {
                    set | crate::scheduler::kernel_signal_bit(signal.0)
                }),
            )),
            SchedResponse::ObserveSignal(_) | SchedResponse::InGuestFatalAlarm => {
                self.sched
                    .lock()
                    .unwrap()
                    .fail_parked(dettid, ProtocolFailure::UnexpectedControl);
                self.wait_for_backend_failure().await;
                futures::future::pending().await
            }
        }
    }

    fn recv_robust_list_wakes(&self, from: DetTid, wakes: Vec<(DetTid, FutexID)>) -> Vec<u64> {
        let mut sched = self.sched.lock().unwrap();
        if sched.retain_sigkill_victim_wakes(from, &wakes) {
            // A SIGKILL victim's batch runs with its cohort; the empty answer
            // has no counts, so the victim logs nothing for it.
            return Vec::new();
        }
        sched.wake_futex_waiters_after_exit(&wakes)
    }

    /// The epoch as a logical time: the virtual mtime of a file first seen
    /// with a non-canonical host mtime.
    fn epoch_logical_time(&self) -> LogicalTime {
        let nanos = self
            .cfg
            .epoch
            .timestamp_nanos_opt()
            .expect("epoch cannot be represented in a timestamp with nanosecond precision")
            as u64;
        LogicalTime::from_nanos(nanos)
    }

    async fn recv_determinize_inode(
        &self,
        from: Tid,
        ino: RawInode,
        procfs: Option<ProcfsName>,
        observed: ObservedMtime,
        sighting: InodeSighting,
    ) -> (DetInode, LogicalTime) {
        let _sched = self.lock_rpc_scheduler(false).await;
        // Here we establish a policy that when we first see a file its mtime is
        // the epoch, unless its host mtime is one of the canonical values in
        // `CANONICAL_FILE_MTIME_SECONDS`, which it keeps.
        let epoch = self.epoch_logical_time();
        let mut pool = self.inodes.lock().unwrap();
        if let Some(name) = procfs {
            let path = pool.procfs_path_key(&name);
            pool.procfs_key(ino, path);
        }
        let (dino, ns) = pool.add_sighted_inode(ino, observed, epoch, sighting);
        drop(pool);
        trace!(
            "[detcore, dtid {}] resolved (raw) inode {:?} to {:?}, mtime {}",
            from, ino, dino, ns
        );
        (dino, ns)
    }

    async fn recv_determinize_mapping_inodes(
        &self,
        from: Tid,
        inodes: Vec<RawInode>,
    ) -> Vec<DetInode> {
        let _sched = self.lock_rpc_scheduler(false).await;
        let epoch = self.epoch_logical_time();
        let mut pool = self.inodes.lock().unwrap();
        let dinos: Vec<DetInode> = inodes
            .iter()
            .map(|ino| pool.add_inode(*ino, ObservedMtime::Unobserved, epoch).0)
            .collect();
        trace!(
            "[detcore, dtid {}] resolved mapping (raw) inodes {:?} to {:?}",
            from, inodes, dinos
        );
        dinos
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
    async fn recv_determinize_device(&self, from: Tid, raw_device: u64) -> u64 {
        let _sched = self.lock_rpc_scheduler(false).await;
        let det_device = self.devices.lock().unwrap().determinize(raw_device);
        trace!(
            "[detcore, dtid {}] resolved (raw) device {} to {}",
            from, raw_device, det_device
        );
        det_device
    }

    async fn recv_determinize_mount_id(
        &self,
        from: Tid,
        raw_mount_id: u64,
        mountinfo_order: Option<&[u64]>,
    ) -> Option<u64> {
        let _sched = self.lock_rpc_scheduler(false).await;
        let virtual_mount_id = self
            .mount_ids
            .lock()
            .unwrap()
            .determinize(raw_mount_id, mountinfo_order);
        trace!(
            "[detcore, dtid {}] resolved fdinfo mount ID {} to {:?}",
            from, raw_mount_id, virtual_mount_id
        );
        virtual_mount_id
    }

    async fn recv_resolve_mountinfo_ids(
        &self,
        from: Tid,
        mountinfo_order: &[u64],
        observation_order: &[u64],
    ) -> Option<Vec<u64>> {
        let _sched = self.lock_rpc_scheduler(false).await;
        let virtual_mount_ids = self
            .mount_ids
            .lock()
            .unwrap()
            .resolve_mountinfo(mountinfo_order, observation_order);
        trace!(
            "[detcore, dtid {}] resolved mountinfo observation order {:?} (canonical {:?}) to {:?}",
            from, observation_order, mountinfo_order, virtual_mount_ids
        );
        virtual_mount_ids
    }

    async fn recv_retire_inode(&self, from: Tid, ino: RawInode) {
        let _sched = self.lock_rpc_scheduler(false).await;
        trace!("[detcore, dtid {}] retire (raw) inode {:?}", from, ino);
        self.inodes.lock().unwrap().retire(ino);
    }

    async fn recv_forget_retired_inode(
        &self,
        from: Tid,
        ino: RawInode,
        procfs: Option<ProcfsName>,
    ) {
        let _sched = self.lock_rpc_scheduler(false).await;
        trace!(
            "[detcore, dtid {}] forget retired (raw) inode {:?}",
            from, ino
        );
        let mut pool = self.inodes.lock().unwrap();
        if let Some(name) = procfs {
            let path = pool.procfs_path_key(&name);
            pool.procfs_key(ino, path);
        }
        pool.forget_retired(ino);
    }

    async fn recv_touch_file(&self, from: Tid, ino: RawInode) {
        let _sched = self.lock_rpc_scheduler(false).await;
        let mtime = if self.cfg.virtualize_time {
            self.global_time.lock().unwrap().as_nanos()
        } else {
            // In this scenario, virtualize_metadata is set and virtualize_time isn't.
            // We virtualize initial mtimes, but update using realtime.
            let dt: DateTime<Utc> = Utc::now();
            let nanos = dt.timestamp_nanos_opt().expect(
                "current time cannot be represented in a timestamp with nanosecond precision",
            ) as u64;
            LogicalTime::from_nanos(nanos)
        };
        trace!(
            "[dtid {}] bumping mtime on file (rawinode {:?}) to {}",
            from, ino, mtime,
        );
        self.set_inode_mtime(ino, mtime);
    }

    /// Set the virtual mtime to the time the guest asked for with utimensat
    /// (or utime/utimes, which are routed through it). Without this, a file's
    /// mtime is the time of its last write, so `tar` extraction, `cp -p` and
    /// `touch -r` could not restore one, and `make` saw files ordered by when
    /// they were unpacked.
    async fn recv_set_file_mtime(&self, from: Tid, ino: RawInode, mtime: LogicalTime) {
        let _sched = self.lock_rpc_scheduler(false).await;
        trace!(
            "[dtid {}] setting mtime on file (rawinode {:?}) to {}",
            from, ino, mtime,
        );
        self.set_inode_mtime(ino, mtime);
    }

    fn set_inode_mtime(&self, ino: RawInode, mtime: LogicalTime) {
        self.inodes.lock().unwrap().set_mtime(ino, mtime);
    }

    fn with_boundary_stack(response: GlobalResponse, stack: MaybePrintStack) -> GlobalResponse {
        match stack {
            Some(path) => GlobalResponse::WithSliceBoundaryStack(path, Box::new(response)),
            None => response,
        }
    }

    // The caller holds the same scheduler lock as the existing event path.
    fn signal_requested_stacktrace(&self, dettid: DetTid) {
        if let Some(sig) = &self.cfg.stacktrace_signal {
            trace!(
                "[dtid {}] signaling thread with {} at the point of stack trace printing.",
                dettid, sig.0
            );
            let tid = Pid::from_raw(dettid.as_raw());
            // TODO(T78538674): virtualize pid/tid:
            // We send a raw signal here and let the guest pick it up WHENEVER it resumes.
            // We don't use the "signal_guest" method because we don't necessarily respect that
            // protocol here.
            // Alarm/timer signals are guest-chosen and may be realtime, which
            // `nix` cannot name; fall through to the raw syscall for those.
            match sig.signal() {
                Some(named) => signal::kill(tid, named).unwrap(),
                None => {
                    // SAFETY: `tid` is a live thread this scheduler owns and
                    // `sig.raw()` is a signal number the guest already supplied.
                    let rc = unsafe { libc::kill(tid.as_raw(), sig.raw()) };
                    assert_eq!(rc, 0, "raw kill of signal {} failed", sig.raw());
                }
            }
        }
    }

    async fn recv_trace_schedevent(
        &self,
        ev: SchedEvent,
        detpid: DetPid,
        request_mm: MmId,
        command_bootstrap: bool,
    ) -> SchedulerRpcResult<TraceSchedEventResponse> {
        let ev = {
            let sched = self.lock_rpc_scheduler(false).await;
            if !sched.rpc_incarnation_matches(ev.dettid, request_mm) {
                return SchedulerRpcResult::ThreadExited;
            }
            // TODO(T124316762): debug address randomization in the tracer and get rid of this hack:
            let ev = {
                if self.past_first_execve.load(SeqCst) {
                    ev
                } else {
                    info!(
                        "Warning: erasing rip of pre-execve sched event! {:?}",
                        SchedEventForLog {
                            event: &ev,
                            command_bootstrap
                        }
                    );
                    SchedEvent {
                        end_rip: None,
                        start_rip: None,
                        ..ev
                    }
                }
            };
            // Future trace_schedevent calls will retain their rip values.
            if ev.op == Op::Syscall(Sysno::execve, SyscallPhase::Prehook) {
                self.past_first_execve.store(true, SeqCst);
            }
            ev
        };

        // Yield this guest thread if needed to follow schedule.
        let result = if self.cfg.replay_schedule_from.is_some() {
            let (consumed, print_stack2) = {
                let mut sched = self.lock_rpc_scheduler(false).await;
                if sched.thread_is_logically_killed(ev.dettid)
                    || !sched.rpc_incarnation_matches(ev.dettid, request_mm)
                {
                    return SchedulerRpcResult::ThreadExited;
                }
                let consumed = sched.consume_schedevent(&ev);
                let print_stack2 = if self.cfg.record_preemptions {
                    sched.record_event(&ev)
                } else {
                    None
                };
                (consumed, print_stack2)
            };
            let ConsumeResult {
                keep_running,
                print_stack,
                event_ix: _,
                timeslice_remaining: mut end_of_timeslice,
            } = consumed;
            trace!(
                "keep_running :{}, end_of_timeslice: {:?}",
                keep_running, end_of_timeslice
            );

            if !keep_running {
                trace!(
                    "[detcore, dtid {}] Thread yielding to follow replay schedule",
                    &ev.dettid,
                );
                let tid = reverie::Tid::from(ev.dettid.as_raw()); // TODO(T78538674): virtualize pid/tid:
                let mut rsrcs = Resources::new(ev.dettid);
                rsrcs.insert(ResourceID::TraceReplay, Permission::RW);
                let (response, timeslice) = self
                    .recv_grant_resources(
                        tid,
                        detpid,
                        rsrcs,
                        Some(request_mm),
                        RpcOrigin::TraceSchedEvent,
                    )
                    .await;
                if response == SchedulerRpcResult::ThreadExited {
                    return SchedulerRpcResult::ThreadExited;
                }
                end_of_timeslice = timeslice;
                trace!(
                    "[detcore, dtid {}] Thread reactivated after yielding for replay schedule",
                    &ev.dettid,
                );
            }

            TraceSchedEventResponse {
                print_stack_strace: print_stack.or(print_stack2),
                timeslice: end_of_timeslice,
            }
        } else {
            let print_stack_strace = {
                let mut sched = self.lock_rpc_scheduler(false).await;
                if sched.thread_is_logically_killed(ev.dettid)
                    || !sched.rpc_incarnation_matches(ev.dettid, request_mm)
                {
                    return SchedulerRpcResult::ThreadExited;
                }
                if self.cfg.record_preemptions {
                    sched.record_event(&ev)
                } else {
                    None
                }
            };
            TraceSchedEventResponse {
                print_stack_strace,
                timeslice: None,
            }
        };

        if result.print_stack_strace.is_some() && self.cfg.stacktrace_signal.is_some() {
            let _sched = self.lock_rpc_scheduler(false).await;
            self.signal_requested_stacktrace(ev.dettid);
        }

        SchedulerRpcResult::Continue(result)
    }

    // Ephemeral port range is in file /proc/sys/net/ipv4/ip_local_port_range"
    // This function reads from the file and returns the range
    // Start of range is at index 0, end of range is at index 1.
    fn read_port_range() -> Vec<u16> {
        let contents = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
            .expect("File should be present");
        let range: Vec<u16> = contents
            .split_whitespace()
            .filter_map(|number| number.parse().ok())
            .collect();
        range
    }

    // Reflect ephemeral port range updated outside of the tracer program internally.
    fn update_port_range(&self) {
        let range = Self::read_port_range();
        self.port_start_range.store(range[0], SeqCst);
        self.port_end_range.store(range[1], SeqCst);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(#869)
    /// Register an alarm (delayed signal delivery) with the global scheduler.
    async fn recv_register_alarm(
        &self,
        detpid: DetPid,
        caller: RpcIncarnation,
        now: LogicalTime,
        duration: LogicalTime,
        interval: LogicalTime,
        sig: SigWrapper,
    ) -> SchedulerRpcResult<(LogicalTime, LogicalTime)> {
        let RpcIncarnation { dettid, mm } = caller;
        let mut sched = self.lock_rpc_scheduler(false).await;
        if sched.thread_is_logically_killed(dettid) || !sched.rpc_incarnation_matches(dettid, mm) {
            return SchedulerRpcResult::ThreadExited;
        }
        match sched.replace_real_timer(detpid, dettid, now, duration, interval, alarm_signal(sig)) {
            Ok(old) => SchedulerRpcResult::Continue(old),
            Err(error) => {
                sched.fail_parked(dettid, ProtocolFailure::Timer(error));
                SchedulerRpcResult::ThreadExited
            }
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// Register, re-arm, or disarm a POSIX timer in the global scheduler.
    /// `notification` is the timer's signal and, for a `SIGEV_THREAD_ID`
    /// timer, the thread it notifies.
    async fn recv_register_posix_timer(
        &self,
        detpid: DetPid,
        caller: RpcIncarnation,
        timer_id: i32,
        deadline: Option<LogicalTime>,
        interval: LogicalTime,
        notification: PosixTimerNotification,
    ) -> SchedulerRpcResult<()> {
        let PosixTimerNotification {
            sig,
            thread,
            value,
            deleted,
        } = notification;
        let RpcIncarnation { dettid, mm } = caller;
        let mut sched = self.lock_rpc_scheduler(false).await;
        if sched.thread_is_logically_killed(dettid) || !sched.rpc_incarnation_matches(dettid, mm) {
            return SchedulerRpcResult::ThreadExited;
        }
        // A process-directed timer prefers the thread that armed it.
        let target = match thread {
            Some(thread) => SignalTarget::Thread(thread),
            None => SignalTarget::Process(dettid),
        };
        sched.note_posix_timer_notification(detpid, timer_id, sig.raw(), value, deleted);
        sched.register_posix_timer(
            detpid,
            target,
            timer_id,
            deadline,
            interval,
            match sig.signal() {
                Some(signal) => signal.into(),
                None => crate::scheduler::timed_waiters::TimerSignal::RefuseRealTime(sig.raw()),
            },
        );
        SchedulerRpcResult::Continue(())
    }
}

/// What a POSIX timer notifies (`GlobalRequest::RegisterPosixTimer`).
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub struct PosixTimerNotification {
    /// The signal.
    pub sig: SigWrapper,
    /// The thread a `SIGEV_THREAD_ID` timer notifies, or `None` for a
    /// process-directed one.
    pub thread: Option<DetTid>,
    /// The `si_value` of its signal.
    pub value: u64,
    /// The timer is being deleted (`timer_delete`), not set.
    pub deleted: bool,
}

/// Identity and final accounting for an asynchronous scheduler deregistration.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub struct ThreadDeregistration {
    pub(crate) dettid: DetTid,
    pub(crate) detpid: DetPid,
    pub(crate) mm: MmId,
    /// Carried by the consuming ThreadState owner, independently of detpid's
    /// delayed initialization and the parent's scheduler registration RPC.
    pub(crate) thread_start_entered: bool,
    pub(crate) timeslice_stats: TimesliceStats,
    pub(crate) syscall_count: u64,
    pub(crate) chaos_epochs: Vec<ChaosEpochTransition>,
}

/// A question about, or a record in, the scheduler's ledger of guest-handled
/// SIGALRMs (`Scheduler::sigalrm_control`), sent by a guest's Detcore in the
/// calling thread's turn before the call reaches the kernel. Signal phase 1
/// refuses each of these with EPERM while it could reach a process that
/// handles SIGALRM.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum SigalrmControl {
    /// A send of SIGALRM to the process of this thread.
    SendTo(DetTid),
    /// A send of SIGALRM to a process the caller cannot name to Detcore: a
    /// pidfd with no recorded target (`pidfd_open` records one).
    SendToUnnamed,
    /// An arming of a kernel producer of SIGALRM that Detcore does not
    /// emulate through its alarm path (a POSIX timer, `F_SETSIG` or
    /// `PR_SET_PDEATHSIG`), or of a file lease, which makes another process's
    /// open wait for the lease break with no scheduler admission. Recorded
    /// when allowed, so a later handler installation can be refused.
    ArmProducer,
    /// An arming of a recurring `ITIMER_REAL` (nonzero interval), which phase 1
    /// does not model for a handling process.
    ArmRecurringTimer,
    /// The caller, in a process that handles SIGALRM, is about to run
    /// synchronous I/O on inherited stdio, which may sleep with no scheduler
    /// admission. Never refused: a SIGALRM already due is recorded as a
    /// determinism loss (design closure 1).
    UnadmittedStdioIo,
    /// Before a guest SIGALRM handler is installed in the caller's process
    /// (phase 1 step I3, design section 6): refused once any guest armed a
    /// kernel producer of SIGALRM, anywhere in the run.
    InstallHandler,
    /// Before the caller's process changes its SIGALRM handler to `SIG_DFL`:
    /// refused while the process has a pending entry, which would take the
    /// default action inside the callback (design section 6).
    HandlerToDefault,
    /// Whether the caller's process has a pending entry, for `rt_sigpending`
    /// ("refused" is true when it has one). Never changes anything.
    PendingEntry,
    /// Records the caller's process's SIGALRM disposition and the caller's
    /// "SIGALRM virtually blocked" bit, after its runtime changed them inside
    /// this turn. Never refused.
    Publish {
        /// Whether the process's SIGALRM disposition is a guest handler.
        handled: bool,
        /// Whether the caller's virtual mask blocks SIGALRM.
        blocked: bool,
    },
    /// Records the caller's "SIGALRM virtually blocked" bit, after its runtime
    /// changed its mask inside this turn. Never refused.
    PublishBlocked(bool),
    /// Whether the caller's process has an entry that is due for the caller
    /// (its SIGALRM not virtually blocked; "refused" is true when it has one).
    /// Never changes anything.
    DueEntry,
    /// Takes the caller's process's entry for delivery to the caller, if it
    /// is still pending, handled and due ("refused" is true when taken), in
    /// the turn of the delivery's `InboundSignal` request (step I4, design
    /// section 4, step 3).
    TakeEntry,
    /// Records a determinism loss for the caller's process, here in the
    /// coordinator, where the verifier reads it: the runtime could not
    /// deliver an entry the caller took (or could not tell the caller's
    /// virtual mask). The caller then stops the process. Never refused.
    DeliveryLost,
    /// Records a determinism loss for the caller's process, here in the
    /// coordinator: the caller delivers an entry before a syscall that has
    /// not run (the I4 addendum's point c), whose handler cannot be shown the
    /// registers Linux would show it (Reverie restores rcx and r11 as the
    /// `syscall` instruction already wrote them). The delivery goes ahead and
    /// the run continues; verification refuses to compare it. Never refused.
    EntryDeliveryUnfaithful,
}

/// Messages to the global object.
///
/// Whether a signal send named a process (kill, rt_sigqueueinfo, a pidfd
/// without `PIDFD_SIGNAL_THREAD`) or one thread (tkill, tgkill,
/// rt_tgsigqueueinfo, a pidfd with `PIDFD_SIGNAL_THREAD`). A SIGKILL ends its
/// group either way while the named thread is alive; for an exited thread only
/// a process-directed one does (Claude re-check of
/// <https://github.com/rrnewton/hermit/pull/4439>, N1).
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum SignalScope {
    /// The send named a process.
    Process,
    /// The send named one thread.
    Thread,
}

/// This is public only so it can be used in the `GlobalTool` trait.
/// It should NOT be used by any client outside of this file.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
#[allow(clippy::enum_variant_names)]
pub enum GlobalRequest {
    /// `request`, sent with the number of records this guest thread produced
    /// for forwarding since its previous counted request, so the coordinator
    /// can check they all arrived (`detlog::check_forwarded_count`). Sent only
    /// by a thread that has produced some (`counted_request`).
    WithForwardedCount(u64, Box<GlobalRequest>),
    /// Lock the resources
    /// Also contains the `DetPid` of the process containing the thread requesting resources.
    RequestResources(Resources, DetPid),
    /// Request resources with the actual backend parked-callback capability.
    ParkedRequest(Resources, DetPid, ControlCapability),
    /// Resume a previously parked request at its actual current callback site.
    ResumeParkedRequest {
        /// Scheduler-issued one-use continuation ticket.
        ticket: ResumeTicket,
        /// Actual callback site supplied by the backend on resumption.
        current_site: reverie::CallbackSignalSite,
    },
    /// Finish the observation belonging to one scheduler-owned parked wait.
    FinishParkedObservation {
        /// Actual pending wait identity.
        wait: ContinuationId,
        /// Backend observation lease belonging to that wait.
        lease: reverie::ParkedObservationLease,
        /// Actual callback site at the observation boundary.
        site: reverie::CallbackSignalSite,
        /// Result of the observation, including an explicit failure.
        finish: ObservationFinish,
    },
    /// Report a terminal violation of the parked continuation protocol.
    ParkedProtocolFailure(ProtocolFailure),
    /// Report an actual signal removed from this process's pending set.
    SignalDequeued {
        /// Deterministic process whose signal was dequeued.
        detpid: DetPid,
        /// Backend identity of the actual signal task.
        identity: reverie::SignalTaskIdentity,
        /// Exact signal dequeue observation.
        dequeue: reverie::SignalDequeue,
    },
    /// Release the locks
    ReleaseResources(Resources),
    /// For convenience, release all the resources held by the current TID.
    ReleaseAllResources,

    // TODO-HUMAN-REVIEW(PR-643): Review this new Detcore global RPC request.
    /// Add a syscall to the run-wide unsupported-use summary.
    ReportUnsupportedSyscall(String),

    /// A guest asked for a disposition of this signal that Detcore answered
    /// with success but did not install. The coordinator announces it once per
    /// signal per run.
    AnnounceSignalHandlerNotInstalled(i32),

    /// The fail-closed policy is ending the run, for this reason. Sent just
    /// before the Tool image exits with `HERMIT_POLICY_REFUSAL_EXIT`, which on
    /// an in-guest backend ends only the guest process: the launcher reads the
    /// reason here to report the refusal (`GlobalState::policy_refusal`).
    PolicyRefusal(String),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1154): Review the SaBRe exec descriptor-status handoff.
    /// Save the caller, address-space identity, and logically blocking descriptors before exec.
    /// A successful backend may reload the Tool or preserve and transfer its thread state.
    /// Boxed, as is `GlobalResponse::MarkPastFirstExecve`'s: inline, the
    /// largest payload sizes every RPC future, and each of the hundreds of
    /// handler futures `handle_syscall_event` awaits holds some, which in a
    /// debug build overflowed the 2 MiB test thread that runs ptrace record
    /// through the library (`record_replay`'s `public_record_*` tests).
    PrepareExec(DetPid, MmId, Box<ExecFdState>),

    /// Clear the saved transition after an exec attempt returns with an error.
    CancelExec(DetPid),

    /// Authenticate the preserved exec caller taking over its process leader's TID.
    ReconnectExec {
        /// Preserved caller before the kernel leader-identity transition.
        former: DetTid,
        /// Continuing process leader identity.
        process: DetPid,
    },
    /// Acquire the replacement's first turn without newborn state initialization.
    ResumeExec(DetPid),
    /// Consume a transferred owner if genuine cancellation interrupts local
    /// rebinding. The Tool copies `signaled` from the backend's exit status;
    /// ordinary exit requires a separately recorded backend failure.
    RetireExec {
        /// Final transferred thread state and accounting.
        thread: ThreadDeregistration,
        /// Whether the backend observed signal termination.
        signaled: bool,
    },

    /// Complete a successful image transition, including the initial image, and
    /// delete the process's POSIX timer deadlines before the new image runs.
    /// The last field is the device and inode of the file the kernel's
    /// `/proc/<pid>/exe` link names for the new image, where the tool can
    /// read it, which the image's record is checked against
    /// (`crate::exec_record`). Before it, in a run that declares a launcher,
    /// the record the new image resolved from the path its exec was given,
    /// for an exec Detcore did not prepare.
    MarkPastFirstExecve(
        DetPid,
        Option<reverie::SignalTaskIdentity>,
        Option<crate::exec_record::ExecRecord>,
        Option<(u64, u64)>,
    ),

    /// The parent is adding a child-thread to the round-robin pool.  Contains the dettid
    /// of the new child and it's starting scheduler priority IF it is available to the caller.
    /// The only scenario where the Priority will be missing is when we're replaying preemptions.
    /// In that case it is the global state that holds the information regarding the new thread's
    /// initial priority.
    CreateChildThread(
        DetTid,
        DetPid,
        usize,
        Option<CloneFlags>,
        libc::c_int,
        Option<(i32, i32)>,
        Option<Priority>,
    ),

    /// A vfork child registering itself while its parent is blocked inside the
    /// kernel. Contains the (real) parent dettid and detpid, the child dettid,
    /// the child TID address, the clone flags, and the starting priority (absent
    /// only when replaying preemptions).
    CreateVforkChildThread(
        DetTid,
        DetPid,
        DetTid,
        usize,
        CloneFlags,
        libc::c_int,
        Option<Priority>,
    ),

    /// New thread is alive and waiting to run its first instruction.  Contains the dettid
    /// and detpid of the new child.
    StartNewThread(
        DetTid,
        DetPid,
        Option<(i32, i32)>,
        Option<reverie::SignalTaskIdentity>,
    ),

    /// Remove a thread from scheduler data structures, guaranteeing that it will
    /// consume no further turns. Carries its final timeslice distribution and any
    /// chaos-epoch transitions not yet flushed by a priority-change commit.
    DeregisterThread(ThreadDeregistration),

    /// Replace the address cleared and woken when the calling thread exits.
    /// A zero address disables the exit-time store and wake.
    SetChildTidAddress(usize),

    /// Record (`Some`) or clear (`None`) the calling thread's own signal mask
    /// while Detcore holds it under a private blocking mask, so the scheduler's
    /// child-exit classification reads the guest's mask, not Detcore's.
    SetSavedGuestSigmask(Option<u64>),

    /// The guest's own signal mask that a thread published while Detcore
    /// holds it under a private blocking mask, if any
    /// (`Scheduler::published_guest_sigmask`), for its `/proc` status.
    PublishedGuestSigmask(DetTid),

    /// Record (`Some`) or clear (`None`) the set the calling thread waits for
    /// while it polls `rt_sigtimedwait`.
    SetSigtimedwaitSet(Option<u64>),

    /// The inputs to a process-directed signal the calling thread sends to its
    /// own process (`Scheduler::own_process_signal_inputs`).
    OwnProcessSignalInputs,

    /// The calling thread sends SIGSTKFLT (`Scheduler::record_guest_marker_signal`).
    RecordGuestMarkerSignal,

    /// Notify scheduler before/after futex action.
    /// The last two arguments are the initial contents of the memory word, and the mask.
    FutexAction(DetTid, FutexAction, FutexID, i32, u32),

    /// Translate nondeterministic to deterministic inode, and report what the
    /// caller observed of the host mtime (see `ObservedMtime`), how it
    /// reached the file (see `InodeSighting`), and, for a procfs entry, its
    /// name (see `procfs_inode::ProcfsName` and `InodePool::procfs_key`).
    DeterminizeInode(RawInode, ObservedMtime, InodeSighting, Option<ProcfsName>),

    /// Translate the inode of every file-backed line of one `/proc/*/maps`
    /// read, in line order, repeats included: one numbering request per line.
    DeterminizeMappingInodes(Vec<RawInode>),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
    /// Translate a nondeterministic (host-assigned) device number to a
    /// deterministic one.
    DeterminizeDevice(u64),

    /// Translate a host-assigned fdinfo mount ID to a run-local identity. The
    /// fallback order is populated only by low-level callers without captured
    /// namespace provenance.
    DeterminizeMountId(u64, Option<Vec<u64>>),

    /// Seed or validate the snapshot's canonical mountinfo identity order
    /// (first field), then return the run-local identity of each entry of the
    /// observation order (second field: the same raw IDs, rows then parents),
    /// numbering unseen entries in that observation order.
    ResolveMountinfoIds(Vec<u64>, Vec<u64>),

    /// The file behind this host inode lost its last name (see
    /// `InodePool::retire`).
    RetireInode(RawInode),

    /// An `open` found this host inode linked or freshly created (see
    /// `InodePool::forget_retired`); for a procfs entry, also its name (see
    /// `procfs_inode::ProcfsName` and `InodePool::procfs_key`).
    ForgetRetiredInode(RawInode, Option<ProcfsName>),

    /// The inode pool's state, for a directory snapshot (see `SnapshotMark`).
    DirectorySnapshotMark,

    /// `--pipe-wake-latency`: an action of the calling thread readied the
    /// pollers of a channel that want this (`Scheduler::ready_channel`).
    ReadyChannel(crate::pipe_wake::ChannelId, crate::pipe_wake::Wants),
    /// `--pipe-wake-latency`: the calling thread's hand-off is due
    /// (`Scheduler::hand_off_turn`).
    HandOffTurn,
    /// The calling thread is delivering a signal whose default action ends
    /// its process (`Scheduler::record_committed_fatal_signal`).
    RecordFatalSignal,

    /// Bump mtime
    TouchFile(RawInode),

    /// Set mtime to a guest-requested time (utimensat and friends).
    SetFileMtime(RawInode, LogicalTime),

    /// Retrieve global time.
    GlobalTimeLowerBound,

    /// Record scheduling event in a total order.
    // Logging provenance only; never serialized into a schedule artifact.
    TraceSchedEvent(SchedEvent, DetPid, bool),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(#869)
    /// Basically performs an alarm syscall, takes a logical duration.
    RegisterAlarm(DetPid, DetTid, LogicalTime, LogicalTime, SigWrapper),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// Register, re-arm, or disarm one POSIX timer, or delete it.
    RegisterPosixTimer(
        DetPid,
        DetTid,
        i32,
        Option<LogicalTime>,
        LogicalTime,
        PosixTimerNotification,
    ),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-841): Review logical alarm query RPC.
    /// Return the logical time remaining on a process's one-shot alarm.
    AlarmRemaining(DetPid),
    /// Reconcile the process's `Inactive` ITIMER_REAL after a call that may
    /// have dequeued its SIGALRM (`Scheduler::reconcile_alarm`).
    ReconcileAlarm(DetPid),
    /// A guest's committed SIGALRM send (rule 4, `Scheduler::note_guest_sigalrm`).
    NoteGuestSigalrm(crate::scheduler::itimer_coalesce::GuestSigalrmTarget),
    /// The caller changed SIGALRM's disposition (rule 5).
    NoteSigalrmDisposition,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Deterministically select a logically exited matching child.
    ReadyChildWait(DetPid, ChildWaitSpec),
    /// Retire a consumed terminal child wait status.
    ConsumeChildWait(DetPid, DetPid),
    /// Query scheduler-owned process-group membership.
    ProcessGroup(DetPid),
    /// Apply a successful setpgid transition.
    SetProcessGroup(DetPid, DetPid),
    /// Apply a successful setsid transition.
    CreateSession(DetPid),
    /// Query live threads before translating process-directed signal delivery,
    /// and the process's exit state (`Scheduler::exited_process_is_zombie`).
    ResolveKillTargets(DetPid),
    /// Whether a pid or tid that a guest task named (with the thread group it
    /// named, if any) is one of Hermit's own tasks; see `hermit_task_named`.
    NamesHermitTask(DetTid, Option<i32>, i32),
    /// A successful kill(2) queued a physical signal for this sole target,
    /// sent with the given scope.
    NotifySignalPending(DetTid, SigWrapper, Option<DetPid>, SignalScope),
    /// The sender, holding the serial grant in its signal-delivery turn,
    /// decided that a default action ends its thread group: its next request,
    /// the group `Exit`, is granted before any other thread runs
    /// (`Scheduler::hold_turn_for_group_exit`).
    HoldTurnForGroupExit(DetTid),
    /// A successful SIGKILL whose victims the scheduler is not told one by
    /// one: a process-group kill (`Some(group)`), or a pidfd whose process
    /// Detcore did not record (`None`).
    NotifyUnresolvedSigkill(Option<DetPid>),
    /// Liveness of one tid, leader or not; see `thread_is_live`.
    ThreadIsLive(DetTid),
    /// The pending real-time timer expirations of a process; see
    /// `pending_real_time_expirations`.
    PendingRealTimeExpirations(DetPid),
    /// Discard pending real-time timer expirations of a process (a kernel
    /// sigset) whose disposition became `SIG_IGN`.
    DiscardPendingRealTimeExpirations(DetPid, u64),
    /// A process created a signalfd for these signals (a kernel sigset).
    RegisterSignalfd(DetPid, u64),
    /// A handler's return in a process restores a mask leaving these
    /// real-time signals (a kernel sigset) unblocked.
    RecordHandlerRestoreMask(DetPid, u64),
    /// Scheduler-owned lifecycle state for a direct child process.
    ExactChildWaitState(DetPid, DetPid),

    /// The container is shutting down.  Exit the scheduler "thread".
    UnrecoverableShutdown,

    /// Request a port for an open file description.
    RequestPort(OpenFileId),

    /// Add a port to the used-port list for an open file description.
    AddUsedPort(u16, OpenFileId),

    /// Release the port when the last alias of its open file description closes.
    ReleasePort(OpenFileId),

    /// Deliver robust-futex wakes collected before exit after the backend has
    /// confirmed that Linux's physical task cleanup completed.
    RobustListWakes(Vec<(DetTid, FutexID)>),
    /// One external network record or replay operation.
    Network(NetworkRequest),

    /// Record that the sending thread, in its syscall number `syscall`, opened
    /// `path` and found the host file `identity` (see
    /// `detcore_model::host_input`). An observation for `hermit run --verify`
    /// only: answered before any clock or scheduler accounting. Appended at
    /// the end, so that adding it left every earlier variant's encoded tag
    /// unchanged.
    RecordHostInput {
        /// Guest pathname observed by the original syscall.
        path: String,
        /// Number of the syscall that observed the input.
        syscall: u64,
        /// Captured actual host file identity.
        identity: HostFileIdentity,
    },

    /// Record that the sending thread rebound `path` in the guest's file
    /// namespace (see `detcore_model::host_input::HostMutationRecord`). An
    /// observation for `hermit run --verify` only, answered like
    /// [`Self::RecordHostInput`]. Appended at the end, so that adding it left
    /// every earlier variant's encoded tag unchanged.
    RecordHostMutation {
        /// Guest pathname whose namespace binding changed.
        path: String,
    },
    /// This thread armed, in its turn, a host-timed source of these signals;
    /// see `record_host_timed_signals`. Appended after `RecordHostMutation`,
    /// so that adding it left every earlier variant's encoded tag unchanged.
    RecordHostTimedSignals(DetTid, HostTimedSignalScope, u64),
    /// The signals no gated wait of this thread ends for; see
    /// `host_timed_signals`. Appended after `RecordHostTimedSignals`, for the
    /// same reason.
    HostTimedSignals(DetTid),
    /// A control message about a shared open file description, answered
    /// before any clock or scheduler accounting (see
    /// `crate::shared_open_files`). Appended after `HostTimedSignals`, for the
    /// same reason.
    SharedOpenFile(OpenFileControl),
    /// A SIGALRM ledger control message, answered before any clock or
    /// scheduler accounting (see `sigalrm_refuses`). Appended after
    /// `SharedOpenFile`, for the same reason.
    Sigalrm(SigalrmControl),
    /// A child-exit `SIGCHLD` control message
    /// (`scheduler::child_exit_sigchld`).
    ChildExitSigchld(crate::scheduler::child_exit_sigchld::ChildExitSigchldControl),
    /// A determinism loss to record for the run (see
    /// `crate::detlog::write_loss_notice`), from a guest-side handler, which
    /// may run in a process whose own record verification never reads.
    /// Answered before any clock or scheduler accounting. Appended after
    /// `ChildExitSigchld`, for the same reason.
    RecordDeterminismLoss(String),
    /// Mirror a successful `prctl(PR_SET_CHILD_SUBREAPER)` by this process,
    /// in the caller's turn (`set_child_subreaper`). Appended after
    /// `RecordDeterminismLoss`, for the same reason.
    SetChildSubreaper(DetPid, bool),
    /// The process Detcore registered `DetTid` in: its own thread group's
    /// leader in the thread tree. Unlike the backend's `Guest::pid`, it names
    /// a raw clone child with a non-SIGCHLD exit signal as its own process.
    /// Appended after `SetChildSubreaper`, for the same reason.
    RegisteredProcessOf(DetTid),
    /// Mirror a successful `rt_sigaction(SIGCHLD, ...)` by this process:
    /// whether the action is `SIG_IGN`, and whether it has `SA_NOCLDWAIT`
    /// (`set_sigchld_disposition`). Appended after `RegisteredProcessOf`, for
    /// the same reason.
    SetSigchldDisposition(DetPid, bool, bool),
    /// The process a signal naming this id goes to
    /// (`Scheduler::signal_target_process`). Appended after
    /// `SetSigchldDisposition`, for the same reason.
    SignalTargetProcess(DetPid),
    /// Reserve the victims of a SIGKILL the sender is about to send to the
    /// process of the given thread, before the physical send
    /// (`scheduler::sigkill_retirement`). Appended after
    /// `RegisteredProcessOf`, for the same reason.
    ReserveSigkill(DetTid, DetTid, u32),
    /// Commit (true: the send succeeded) or unwind a SIGKILL reservation.
    /// Appended after `ReserveSigkill`, for the same reason.
    CommitSigkill(u64, bool),
    /// Reserve the victims of a `kill(-pgid, SIGKILL)`: every live process in
    /// the recorded process group. Appended after `CommitSigkill`, for the
    /// same reason.
    ReserveSigkillGroup(DetTid, DetPid, u32),
    /// The thread a `SIGEV_THREAD_ID` timer of this process names by this id
    /// (`Scheduler::posix_timer_thread`). Appended after
    /// `ReserveSigkillGroup`, for the same reason.
    PosixTimerThread(DetPid, i32),
    /// A disposition change in this process flushed this signal from every
    /// thread (`Scheduler::forget_thread_timer_copies`). Appended after
    /// `PosixTimerThread`, for the same reason.
    ForgetThreadTimerCopies(DetPid, i32),
    /// What a positive pid the given nested thread names, in its own PID
    /// namespace, names (`PidNamespaces::resolve_for`). Appended after
    /// `ReserveSigkillGroup`, for the same reason.
    GuestPid(DetTid, i32),
    /// The pid the given nested thread sees for a task
    /// (`PidNamespaces::localize_for`). Appended after `GuestPid`, for the
    /// same reason.
    LocalPid(DetTid, DetTid),
    /// The POSIX timer copies a successful exec of this process flushes
    /// (`Scheduler::take_exec_timer_flush`), asked by its surviving thread.
    /// Appended after `LocalPid`, for the same reason.
    ExecTimerFlush(DetPid),
    /// The calling thread-group leader is about to complete its own `exit`
    /// while its group lives on (`Scheduler::retire_exited_leader`).
    /// Appended after `ExecTimerFlush`, for the same reason.
    RetireExitedLeader,
    /// A discarded-at-send record message (`scheduler::discarded_at_send`),
    /// answered before any clock or scheduler accounting. Appended after
    /// `RetireExitedLeader`, for the same reason.
    DiscardedSignal(crate::scheduler::discarded_at_send::DiscardedSignalControl),
    /// A `close`, `close_range`, `dup2` or `dup3` released or replaced the
    /// descriptors `first..=last` of the caller's table, named by its identity
    /// (`Scheduler::note_descriptors_replaced`). Appended after
    /// `DiscardedSignal`, for the same reason.
    DescriptorsReplaced(detcore_model::fd::FilesId, u32, u32),
    /// Whether a task of this run had this id and it is not a zombie's pid
    /// (`thread_id_refused`). Appended after `DescriptorsReplaced`, for the
    /// same reason.
    ThreadIdRefused(DetTid),
    /// The processes `/proc` lists, and the threads the `task` directory of
    /// the given process lists (`Scheduler::procfs_listing`). Appended after
    /// `ThreadIdRefused`, for the same reason.
    ProcfsListing(Option<DetPid>),
    /// Arm (`Some`, 0 disarms) or read (`None`) the sender's parent-death
    /// signal that Detcore sends itself (`parent_death_signal`). Appended
    /// after `ProcfsListing`, for the same reason.
    ParentDeathSignal(Option<i32>),

    /// Neutral controls appended without changing any existing wire tag.
    PrepareNativeExec(native_exec::PrepareNativeExec),
    /// Seal the exact versioned runtime bytes before one-use descriptor provision.
    SealNativeExecStats {
        /// Retained prepared transaction ticket.
        ticket: native_exec::ExecTicket,
        /// SHA-256 of the exact runtime envelope bytes.
        runtime_digest: [u8; 32],
        /// Bounded actual runtime envelope, not host-decoded Tool state.
        runtime: Vec<u8>,
    },
    /// Last old-image control after the sealed empty forwarding cut.
    IssueNativeExec {
        /// Successfully provisioned retained transaction ticket.
        ticket: native_exec::ExecTicket,
        /// Independently computed composite Tool/runtime digest.
        state_digest: [u8; 32],
    },
    /// Invalidate a pre-redemption ticket; ordinary CancelExec still follows.
    CancelNativeExec {
        /// Prepared or Issued ticket whose physical exec did not succeed.
        ticket: native_exec::ExecTicket,
    },
    /// Acknowledge the real new-mm lifecycle after MarkPastFirstExecve.
    CompleteNativeExec {
        /// Consumed successor ticket after real lifecycle and frontend checks.
        ticket: native_exec::ExecTicket,
    },
    /// Whether a tid names a thread-group leader retired at its own exit
    /// while its group lives on (`names_a_retired_exited_leader`). Appended
    /// last, for the same reason as the controls above.
    NamesARetiredExitedLeader(DetTid),
    /// The kernel's numbers for guest process or thread numbers (`true`), or
    /// the guest's for kernel numbers (`false`), as `/proc` spells them
    /// (`Scheduler::procfs_ids`). Appended last, for the same reason.
    ProcfsIds(bool, Vec<i32>),
    /// The process group (`false`) or session (`true`) of the task with an id,
    /// as `getpgid` and `getsid` answer it (`Scheduler::group_or_session_of`).
    /// Appended last, for the same reason.
    GroupOrSession(DetPid, bool),
    /// Linux's answer to `setsid()` by a process
    /// (`Scheduler::setsid_decision`), deciding nothing yet. Appended last.
    SetsidDecision(DetPid),
    /// Linux's answer to `setpgid(pid, pgid)` by a process
    /// (`Scheduler::setpgid_decision`), deciding nothing yet. Appended last.
    SetpgidDecision(DetPid, i32, i32),
    /// The image a guest process executes, as Detcore recorded it at the
    /// process's exec (`crate::exec_record`), for its `/proc/<pid>/exe`
    /// link. Appended last.
    ProcessImage(DetPid),
    /// Whether the task a per-process `/proc` file names exists, whether it
    /// has exited unreaped, and the incarnation of its numbers
    /// (`Scheduler::procfs_task`). Appended last.
    ProcfsTask(DetTid, Option<DetTid>),
    /// Change or read the state Detcore keeps on a rendered per-process
    /// `/proc` file's inode (`InodePool::rendered_file_state`). Appended last.
    RenderedFileState(u64, crate::procfs::listing::RenderedFileChange),
    /// The sender's process's signal dispositions as Detcore's copy holds
    /// them, or `None` to withdraw them, with the publication's generation
    /// (`Scheduler::publish_signal_dispositions`). Answered before any clock
    /// or scheduler accounting. Appended last.
    PublishSignalDispositions(
        u64,
        Option<crate::syscalls::signal_shadow::CopiedDispositions>,
    ),
    /// A Detcore warning or error from inside the guest, which the coordinator
    /// emits at the run's own log level (`crate::guest_log`), or none, and how
    /// many of the sender's process could not be sent before it. Answered
    /// before forwarded records are drained and before any clock or scheduler
    /// accounting. Appended last.
    GuestLog(Option<crate::guest_log::GuestLogRecord>, u64),
    /// The sender's mask as Detcore's copy holds it, or `None` to withdraw
    /// it (`Scheduler::publish_signal_mask`). Answered before any clock or
    /// scheduler accounting. Appended last.
    PublishSignalMask(Option<u64>),
    /// The auxiliary vector the sender's process's new image found on its
    /// initial stack (`ProcessAuxvTable::record`). Appended last.
    RecordProcessAuxv(Vec<u8>),
    /// A process's image's auxiliary vector (`ProcessAuxvTable::current`), for
    /// `/proc/<pid>/auxv`. Appended last.
    ProcessAuxv(DetPid),
    /// The sender's process's new image's `/proc/<pid>/auxv` is left to the
    /// kernel beneath (`ProcessAuxvTable::forget`), for the reason given.
    /// Answers whether this is the run's first such image for that reason,
    /// which the sender warns of. Appended last.
    AuxvFromKernel(AuxvFromKernel),
    /// The signals whose new action an in-guest runtime refused, bit `n - 1`
    /// for signal `n`, and whether it refused an alternate signal stack
    /// (`crate::refused_signal_controls`). Answered before the forwarded-record
    /// drain and any clock or scheduler accounting. Appended last.
    AnnounceRefusedSignalControls(u64, bool),
    /// A fixed reply from an external controller while the sender retains its
    /// serial grant. The runtime fact and the scheduler jointly establish that
    /// no guest can observe the temporary procfs descriptor. Appended last.
    CheckBrkMappings(
        crate::syscalls::memory::BrkMappingCheck,
        reverie::ControllerCaptureFact,
    ),
    /// Current image's native ownership facts, authenticated by the RPC MmId.
    /// Answered without granting another task or advancing logical time.
    PublishRuntimePrivateMemory(reverie::RuntimePrivateMemory),
    /// The sender's process changed its root with `chroot`
    /// (`ProcessAuxvTable::mark_changed_root`). Appended last.
    MarkChangedRoot,
    /// Whether the process a number names has changed its root
    /// (`ProcessAuxvTable::root_changed`). Appended last.
    RootChanged(DetPid),
    /// The OFD and flock locks this open file description holds, as its
    /// fdinfo showed them after a call through one of the sender's
    /// descriptors, and how many slots of the sender's descriptor table name
    /// it (`procfs::locks::LockLedger::note_locks`), or why that fdinfo
    /// could not be read (`procfs::locks::LockLedger::note_unknown`).
    /// Appended last.
    DescriptionLocks(
        OpenFileId,
        Result<Vec<crate::procfs::locks::HeldLock>, String>,
        u32,
    ),
    /// How many slots of the sender's descriptor table still name this
    /// description after a close or replacement there
    /// (`procfs::locks::LockLedger::release`). Appended last.
    TableReleased(OpenFileId, u32),
    /// Which rows of a `/proc/locks` snapshot the guest owns
    /// (`procfs::locks::guest_lock_rows`). Appended last.
    GuestLockRows(Vec<crate::procfs::locks::LockRowFacts>),
    /// The descriptions whose locks Detcore read that the sender's process's
    /// table still names after its successful exec, with their slot counts
    /// (`procfs::locks::LockLedger::exec_kept`). Appended last.
    TableExec(Vec<(OpenFileId, u32)>),
    /// A lock or unlock went through a descriptor whose description the
    /// ledger may know under another identity, on the file with this inode
    /// number, or on an unknown one
    /// (`procfs::locks::LockLedger::alias_inode`). Appended last.
    AliasedInode(Option<String>),
    /// The calling thread's host-timed signals and the `SIGCHLD` the schedule
    /// recorded in its private queue (`Scheduler::host_timed_and_private_sigchld`).
    /// Appended last.
    HostTimedAndPrivateSigchld(DetTid),
    /// A lock or unlock through a descriptor whose description the ledger
    /// may know under another identity, whose fdinfo could not be read for
    /// this reason (`procfs::locks::LockLedger::refuse`). Appended last.
    LockLedgerRefused(String),
    /// Contained mapping capture; answered without changing the serial grant.
    MappingCapture(mapping_capture::Request),
    /// The in-guest delivery protocol cannot go on in this run, for this
    /// reason: the whole refusal line, conventionally `hermit refused to
    /// continue the run (NAME): ...` (A5 design D49). The coordinator ends the
    /// whole run on it (`scheduler::exit_on_scheduler_refusal`): it writes the
    /// line and exits with `HERMIT_POLICY_REFUSAL_EXIT`, and where the
    /// scheduler runs as the init of the guest's PID namespace, the kernel
    /// then kills every guest process. Unlike `PolicyRefusal`, which records a
    /// reason while only the refusing Tool image exits, no guest process runs
    /// on after it, and it is never answered. Filed only under
    /// `Config::in_guest_signal_delivery`, whose gate requires that
    /// ownership. Appended last.
    StoppingRefusal(String),
}

/// Responses from the global object
#[allow(missing_docs, clippy::unit_arg)]
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum GlobalResponse {
    /// The scheduler permanently removed this raw TID. Guest-side RPC handling retires
    /// this task without cancelling a live group-exit issuer or resuming the operation.
    ThreadExited,
    /// A reply, with the virtual time that remained before the earliest
    /// pending deadline when the thread was granted the turn it resumes, or
    /// armed a timer (`Scheduler::deadline_remaining_from`). Unwrapped by
    /// `send_and_update_time`, which caps the thread's slice there.
    WithDeadline(Duration, Box<GlobalResponse>),
    /// A reply to a request while which the thread was granted a turn
    /// (`Scheduler::take_grant`): other threads may have run since it was
    /// sent. Unwrapped by `send_and_update_time`, which notes the hand-off
    /// (`ThreadState::note_scheduler_handoff`).
    Granted(Box<GlobalResponse>),
    RequestResources(ResumeStatus),
    ParkedRequest(ResourceReply),
    ResumeParkedRequest(ResourceReply),
    FinishParkedObservation(Result<FinishAck, ProtocolFailure>),
    SignalDequeued {
        ack: Result<DequeueAck, TimerFailure>,
        terminal: bool,
    },
    ReleaseResources(()),
    ReleaseAllResources(()),
    // TODO-HUMAN-REVIEW(PR-643): Review this new Detcore global RPC response.
    ReportUnsupportedSyscall(()),
    AnnounceSignalHandlerNotInstalled(()),
    PolicyRefusal(()),
    PrepareExec(()),
    CancelExec(()),
    ReconnectExec(bool),
    ResumeExec(ResumeStatus, Option<LogicalTime>),
    RetireExec(bool),
    MarkPastFirstExecve(Box<ExecFdState>),
    CreateChildThread(Option<MmId>),
    /// Includes optional preemption points for the new thread.
    StartNewThread(Option<ThreadHistory>),
    DeregisterThread(()),
    SetChildTidAddress(()),
    SetSavedGuestSigmask(()),
    PublishedGuestSigmask(Option<u64>),
    SetSigtimedwaitSet(()),
    OwnProcessSignalInputs(OwnProcessSignalInputs),
    RecordGuestMarkerSignal(()),
    FutexAction(Option<SchedValue>),
    /// Return the mtime as well:
    DeterminizeInode((DetInode, LogicalTime)),
    DeterminizeMappingInodes(Vec<DetInode>),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
    DeterminizeDevice(u64),
    DeterminizeMountId(Option<u64>),
    ResolveMountinfoIds(Option<Vec<u64>>),
    RetireInode(()),
    ForgetRetiredInode(()),
    DirectorySnapshotMark(SnapshotMark),
    ReadyChannel(crate::pipe_wake::ReadyOutcome),
    HandOffTurn(bool),
    RecordFatalSignal(()),
    TouchFile(()),
    SetFileMtime(()),
    GlobalTimeLowerBound(LogicalTime),
    TraceSchedEvent(TraceSchedEventResponse),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(#869)
    RegisterAlarm((LogicalTime, LogicalTime)),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    RegisterPosixTimer(()),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-841): Review logical alarm query RPC.
    ReadyChildWait((Option<DetPid>, bool)),
    ConsumeChildWait(bool),
    ProcessGroup(Option<DetPid>),
    SetProcessGroup(bool),
    CreateSession(bool),
    AlarmRemaining(ItimerSnapshot),
    ReconcileAlarm,
    NoteGuestSigalrm,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    ResolveKillTargets(Vec<DetTid>, Option<bool>),
    NamesHermitTask(bool),
    NotifySignalPending(()),
    HoldTurnForGroupExit(bool),
    NotifyUnresolvedSigkill(()),
    ThreadIsLive(bool),
    PendingRealTimeExpirations(u64),
    DiscardPendingRealTimeExpirations(()),
    RegisterSignalfd(()),
    RecordHandlerRestoreMask(()),
    ExactChildWaitState(ExactChildWaitState),
    // TODO: use void_send_rpc, and remove this bogus response:
    UnrecoverableShutdown(()),

    RequestPort(u16),
    AddUsedPort,
    ReleasePort(Option<u16>),
    PortFull,
    RobustListWakes(Vec<u64>),
    Network(Result<NetworkReply, NetworkEngineError>),
    /// Appended at the end, so that adding it left every earlier variant's
    /// tag unchanged.
    RecordHostInput(()),
    /// Appended at the end, so that adding it left every earlier variant's
    /// tag unchanged.
    RecordHostMutation(()),
    /// Appended after `RecordHostMutation`, so that adding it left every
    /// earlier variant's tag unchanged.
    HostTimedSignals(u64),
    /// Appended after `HostTimedSignals`, for the same reason.
    SharedOpenFile(Result<OpenFileControlReply, SharedOpenFileError>),
    /// Whether the SIGALRM ledger refused the control message. Appended after
    /// `SharedOpenFile`, for the same reason.
    Sigalrm(bool),
    ChildExitSigchld(crate::scheduler::child_exit_sigchld::ChildExitSigchldAnswer),
    /// Appended after `ChildExitSigchld`, for the same reason.
    RecordDeterminismLoss(()),
    /// Appended after `RecordDeterminismLoss`, for the same reason.
    SetChildSubreaper(()),
    /// Appended after `SetChildSubreaper`, for the same reason.
    RegisteredProcessOf(Option<DetPid>),
    /// Appended after `RegisteredProcessOf`, for the same reason.
    SetSigchldDisposition(()),
    /// Appended after `SetSigchldDisposition`, for the same reason.
    SignalTargetProcess(Option<DetPid>),
    /// The reservation token, or none when the send keeps the ordinary path.
    /// Appended after `RegisteredProcessOf`, for the same reason.
    ReserveSigkill(Option<crate::scheduler::sigkill_retirement::SigkillReservation>),
    /// Appended after `ReserveSigkill`, for the same reason.
    CommitSigkill(()),
    /// The sender is a committed SIGKILL victim
    /// (`Scheduler::is_unaccounted_sigkill_victim`): its request is never
    /// answered, and the client waits for the kernel to end it. Unlike
    /// `ThreadExited`, it never tail-injects an exit. Appended after
    /// `CommitSigkill`, for the same reason.
    Retired,
    /// Appended after `Retired`, for the same reason.
    PosixTimerThread(PosixTimerThread),
    /// Appended after `PosixTimerThread`, for the same reason.
    ForgetThreadTimerCopies(()),
    /// Appended after `Retired`, for the same reason.
    GuestPid(crate::scheduler::pid_namespaces::GuestPidAnswer),
    /// Appended after `GuestPid`, for the same reason.
    LocalPid(Result<Option<i32>, String>),
    /// Appended after `LocalPid`, for the same reason.
    ExecTimerFlush(Result<crate::scheduler::ExecTimerFlush, String>),
    /// Appended after `ExecTimerFlush`, for the same reason.
    RetireExitedLeader(bool),
    /// Appended after `RetireExitedLeader`, for the same reason.
    DiscardedSignal(crate::scheduler::discarded_at_send::DiscardedSignalAnswer),
    /// Appended after `DiscardedSignal`, for the same reason.
    DescriptorsReplaced(()),
    /// Appended after `DescriptorsReplaced`, for the same reason.
    ThreadIdRefused(bool),
    /// Appended after `ThreadIdRefused`, for the same reason.
    ProcfsListing(crate::procfs::listing::ListingFacts),
    /// Appended after `ProcfsListing`, for the same reason.
    ParentDeathSignal(i32),

    /// Exact native responses, appended without changing existing wire tags.
    PrepareNativeExec(Result<native_exec::ExecTicket, native_exec::ExecControlError>),
    SealNativeExecStats(Result<[u8; 32], native_exec::ExecControlError>),
    IssueNativeExec(Result<(), native_exec::ExecControlError>),
    CancelNativeExec(Result<(), native_exec::ExecControlError>),
    CompleteNativeExec(Result<(), native_exec::ExecControlError>),
    /// Appended last, for the same reason.
    NamesARetiredExitedLeader(bool),
    /// Appended last, for the same reason.
    ProcfsIds(Vec<Option<i32>>),
    /// Appended last, for the same reason.
    GroupOrSession(Option<DetPid>),
    /// Appended last; the error is an errno.
    SetsidDecision(Result<(), i32>),
    /// The process to move and its new group; the error is an errno. Appended
    /// last.
    SetpgidDecision(Result<(DetPid, DetPid), i32>),
    /// Boxed, to keep the response small. Appended last.
    ProcessImage(Option<Box<crate::exec_record::ExecRecord>>),
    /// Appended last.
    ProcfsTask(crate::procfs::listing::ProcfsTaskAnswer),
    /// Appended last.
    RenderedFileState(crate::procfs::listing::RenderedFileState),
    /// The generation the scheduler holds for the sender's process afterwards
    /// and whether this publication is what it holds
    /// (`Scheduler::publish_signal_dispositions`), or `None` when it took
    /// nothing from this sender. Appended last.
    PublishSignalDispositions(Option<crate::syscalls::signal_shadow::PublicationAnswer>),
    /// Appended last.
    GuestLog(()),
    /// Whether the scheduler took the mask (it takes nothing from a thread it
    /// does not know). Appended last.
    PublishSignalMask(bool),
    /// Appended last.
    RecordProcessAuxv(()),
    /// Appended last.
    ProcessAuxv(Option<ProcessAuxv>),
    /// Whether this is the run's first. Appended last.
    AuxvFromKernel(bool),
    /// Appended last.
    AnnounceRefusedSignalControls(()),
    /// None means the capture authority or mappings were unavailable.
    /// Appended last; this response owns no allocated storage.
    CheckBrkMappings(Option<bool>),
    /// Current image ownership publication was accepted. Appended last.
    PublishRuntimePrivateMemory(bool),
    /// Appended last.
    MarkChangedRoot(()),
    /// Whether the root changed. Appended last.
    RootChanged(bool),
    /// Appended last.
    DescriptionLocks(()),
    /// Appended last.
    TableReleased(()),
    /// One answer per row, in row order, or, while a referenced description's
    /// locks are unknown, why. Appended last.
    GuestLockRows(Result<Vec<bool>, String>),
    /// Appended last.
    TableExec(()),
    /// Appended last.
    AliasedInode(()),
    /// The host-timed signals, then the recorded private `SIGCHLD`. Appended
    /// last.
    HostTimedAndPrivateSigchld(u64, u64),
    /// Appended last.
    LockLedgerRefused(()),
    /// Appended mapping capture response.
    MappingCapture(mapping_capture::Reply),
    /// Typed expectation delivered by the existing grant, appended without
    /// renumbering any legacy wire variant. Not a new resource request.
    WithSliceBoundaryBudget(crate::slice_boundary::grant::Budget, Box<GlobalResponse>),
    /// Actual lifecycle identity on the existing grant, not an expectation
    /// from replay. Appended without renumbering existing wire variants.
    WithSliceBoundaryOwner(
        detcore_model::schedule::boundary::Owner,
        Box<GlobalResponse>,
    ),
    /// Stored-index stack request returned on the annotated operation's own
    /// response, without a mailbox another concurrent RPC could consume.
    WithSliceBoundaryStack(Option<PathBuf>, Box<GlobalResponse>),
}

/// Read only the mapping predicate through a backend's synchronous external
/// channel. No deadline wrapper, log accounting, or async future may allocate
/// or free guest-mm storage after the controller captures the mappings.
pub(crate) fn check_brk_mappings<G, T>(
    guest: &G,
    check: crate::syscalls::memory::BrkMappingCheck,
) -> Option<bool>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let fact = guest.controller_capture_fact()?;
    let state = guest.thread_state();
    let request = (
        state.thread_logical_time.clone(),
        state.mm_id,
        GlobalRequest::CheckBrkMappings(check, fact),
    );
    match guest.try_send_rpc_sync(request).ok()? {
        (None, GlobalResponse::CheckBrkMappings(answer)) => answer,
        _ => None,
    }
}

/// `request`, carrying the number of records this guest thread produced for
/// forwarding since its previous counted request when there are any
/// (`GlobalRequest::WithForwardedCount`). A thread of a backend whose Tool does
/// not forward records never has any, so its requests are unchanged.
pub(crate) fn counted_request(request: GlobalRequest) -> GlobalRequest {
    match crate::detlog::take_forwarded_since_request() {
        0 => request,
        produced => GlobalRequest::WithForwardedCount(produced, Box::new(request)),
    }
}

/// The global request that carries one shared open file control message. It
/// is answered before any clock accounting, so its clock and address-space
/// fields are placeholders. The sender is the thread the backend's transport
/// names for the request; for in-guest LiteInst that is the runtime's own
/// envelope, checked against the connecting process at admission, on a
/// connection the guest cannot write to.
pub fn shared_open_file_request(control: OpenFileControl) -> (DetTime, MmId, GlobalRequest) {
    (
        DetTime::default(),
        MmId::initial(DetTid::from_raw(0)),
        GlobalRequest::SharedOpenFile(control),
    )
}

/// The request [`crate::guest_log`] sends through a runtime's
/// [`crate::guest_log::CoordinatorCall`]: like [`shared_open_file_request`], it
/// carries no clock.
pub(crate) fn guest_log_request(
    record: Option<crate::guest_log::GuestLogRecord>,
    unsent_before: u64,
) -> (DetTime, MmId, GlobalRequest) {
    (
        DetTime::default(),
        MmId::initial(DetTid::from_raw(0)),
        GlobalRequest::GuestLog(record, unsent_before),
    )
}

/// The answer to a [`guest_log_request`].
pub(crate) fn guest_log_reply(
    response: (Option<LogicalTime>, GlobalResponse),
) -> std::io::Result<()> {
    match response {
        (None, GlobalResponse::GuestLog(())) => Ok(()),
        other => Err(std::io::Error::other(format!(
            "unexpected answer to a guest log record: {other:?}"
        ))),
    }
}

/// The answer to a [`shared_open_file_request`].
pub fn shared_open_file_reply(
    response: (Option<LogicalTime>, GlobalResponse),
) -> Result<OpenFileControlReply, SharedOpenFileError> {
    match response {
        (None, GlobalResponse::SharedOpenFile(reply)) => reply,
        other => Err(SharedOpenFileError(format!(
            "unexpected answer to a shared open file control message: {other:?}"
        ))),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the shared warning formatter API.
/// Formats one deterministic warning for a set of unsupported syscall names.
pub fn format_unsupported_syscall_warning(syscalls: &BTreeSet<String>) -> Option<String> {
    if syscalls.is_empty() {
        None
    } else {
        Some(format!(
            "syscalls {} used but not yet supported",
            syscalls.iter().cloned().collect::<Vec<_>>().join(",")
        ))
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1154): Review the SaBRe exec descriptor-status handoff.
/// Notifies the coordinator that `guest` is about to `execve`, recording the
/// pre-exec address space `mm` and any file-descriptor blocking overrides. A
/// backend that handles `execve` outside Detcore's syscall handler must call
/// this before the native syscall so the next image reconnects to the existing
/// scheduler identity and logical clock.
///
/// `robust_wakes` are the owner-death wakes a successful exec makes Linux
/// deliver, from [`Detcore::exec_robust_list_wakes`], where the backend runs
/// the native execve outside Detcore's handler; the coordinator delivers them
/// at the new image's reconnect, or when the caller's thread ends without
/// one, and drops them at [`cancel_exec`]. A backend whose execve Detcore handles stages
/// them in its own thread state instead and passes none.
///
/// `exe` is the image the exec runs, where the caller resolved it
/// ([`crate::exec_record::ExecRecord::resolve`]); the new image's
/// `/proc/<pid>/exe` link answers from it.
pub async fn prepare_exec<G, T>(
    guest: &mut G,
    mm: MmId,
    fd_blocking: ExecFdBlockingOverrides,
    robust_wakes: Vec<(DetTid, FutexID)>,
    exe: Option<crate::exec_record::ExecRecord>,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // An exec replaces the image holding this process's unsent warning count
    // (`crate::guest_log`); every exec path, DBT's native one included, comes
    // here first.
    crate::guest_log::flush_unsent();
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let fd_state = ExecFdState {
        blocking: fd_blocking,
        inherited_reads: guest.thread_state().inherited_reads_for_exec(),
        initialize_inherited_stdio: false,
        initial_input: None,
        robust_wakes,
        open_files: guest
            .thread_state()
            .shared_open_files_for_exec()
            .expect("prepared exec descriptors are no longer live"),
        maps: if guest.config().backend.rediscovers_descriptors_after_exec {
            guest
                .thread_state()
                .maps_for_exec(guest.runtime_private_memory_regions().as_ref())
                .expect("prepared maps descriptors are no longer live")
        } else {
            None
        },
        channels: Default::default(),
        open_file_namespace: None,
        exe,
    };
    // `--pipe-wake-latency` on a backend that rediscovers its descriptors
    // after exec: carry the channels, and reserve the old numbering at every
    // exec (`crate::pipe_wake`).
    let fd_state = if guest.config().pipe_wake_latency_in_effect().is_some()
        && guest.config().backend.rediscovers_descriptors_after_exec
    {
        ExecFdState {
            channels: guest.thread_state().pipe_channels_for_exec(),
            open_file_namespace: Some(guest.thread_state().open_file_namespace_for_exec()),
            ..fd_state
        }
    } else {
        fd_state
    };
    let (_, response) = send_and_update_time(
        guest,
        GlobalRequest::PrepareExec(detpid, mm, Box::new(fd_state)),
    )
    .await;
    assert_eq!(response, GlobalResponse::PrepareExec(()));
}

/// Cancels a matching [`prepare_exec`] after a native exec fails. Call this
/// before returning to the old image, preserving its thread state and address
/// space identity so frozen siblings can resume without an exec transition.
pub async fn cancel_exec<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let (_, response) = send_and_update_time(guest, GlobalRequest::CancelExec(detpid)).await;
    assert_eq!(response, GlobalResponse::CancelExec(()));
}

pub async fn mark_past_first_execve<G, T>(
    guest: &mut G,
    unprepared_exe: Option<crate::exec_record::ExecRecord>,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let controlled =
        guest.signal_control_mode() == reverie::BackendSignalControlMode::ToolControlled;
    let signal_identity = controlled.then(|| guest.signal_task_identity()).flatten();
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    // The device and inode of the file the kernel's `exe` link names for the
    // new image, which the coordinator checks the exec's record against.
    let kernel_image = crate::syscalls::guest_task_procfs(
        &guest.config().backend,
        guest.tid().as_raw(),
        guest
            .thread_state()
            .changed_root
            .load(std::sync::atomic::Ordering::SeqCst),
    )
    .and_then(|task| std::fs::metadata(task.join("exe")).ok())
    .map(|meta| {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    });
    let (_, response) = send_and_update_time(
        guest,
        GlobalRequest::MarkPastFirstExecve(detpid, signal_identity, unprepared_exe, kernel_image),
    )
    .await;
    let mut overrides = match response {
        GlobalResponse::MarkPastFirstExecve(overrides) => *overrides,
        _ => unreachable!(),
    };
    if controlled {
        guest.thread_state_mut().signal_task_identity = signal_identity;
    }
    // The new image's `/proc/<pid>/exe` link answers from its record, or
    // without one is the kernel's.
    guest.thread_state_mut().exe = overrides.exe.take().map(Arc::new);
    if !overrides.is_empty() {
        if let Some(creator) = overrides.open_file_creator() {
            // Every carried description continues the old namespace, including
            // maps-only handoffs and the pipe-wake-latency channel namespace.
            guest.thread_state_mut().set_open_file_creator(creator);
        }
        let dettid = guest.thread_state().dettid;
        let metadata = Arc::clone(&guest.thread_state().file_metadata);
        metadata
            .lock()
            .unwrap()
            .apply_exec_fd_state(
                dettid,
                overrides,
                guest.config().backend.tool_shares_guest_descriptor_table,
            )
            .expect("prepared exec descriptor provenance no longer matches the guest table");
    }
}

/// Report to the global state that this thread, in its current syscall,
/// opened `path` and found the host file `identity`, for `hermit run
/// --verify` (see `detcore_model::host_input`). The answer carries no time.
pub(crate) async fn record_host_input<G, T>(guest: &mut G, path: String, identity: HostFileIdentity)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let syscall = guest.thread_state().stats.syscall_count;
    let (time, response) = send_and_update_time(
        guest,
        GlobalRequest::RecordHostInput {
            path,
            syscall,
            identity,
        },
    )
    .await;
    assert_eq!(
        (time, response),
        (None, GlobalResponse::RecordHostInput(()))
    );
}

/// Report to the global state that this thread rebound `path` in the guest's
/// file namespace, for `hermit run --verify` (see
/// `detcore_model::host_input::HostMutationRecord`). The answer carries no
/// time.
pub(crate) async fn record_host_mutation<G, T>(guest: &mut G, path: String)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (time, response) =
        send_and_update_time(guest, GlobalRequest::RecordHostMutation { path }).await;
    assert_eq!(
        (time, response),
        (None, GlobalResponse::RecordHostMutation(()))
    );
}

/// Record a determinism loss for the run in the global state's process,
/// whose losses verification reads (see `crate::detlog::determinism_loss`),
/// whichever process this handler runs in. The answer carries no time.
pub(crate) async fn report_determinism_loss<G, T>(guest: &mut G, reason: String)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (time, response) =
        send_and_update_time(guest, GlobalRequest::RecordDeterminismLoss(reason)).await;
    assert_eq!(
        (time, response),
        (None, GlobalResponse::RecordDeterminismLoss(()))
    );
}

// TODO-HUMAN-REVIEW(PR-643): Review the guest-to-global unsupported-syscall report path.
pub async fn report_unsupported_syscall<G, T>(guest: &mut G, sysno: Sysno)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) = send_and_update_time(
        guest,
        GlobalRequest::ReportUnsupportedSyscall(sysno.to_string()),
    )
    .await;
    assert_eq!(response, GlobalResponse::ReportUnsupportedSyscall(()));
}

/// The once-per-run WARN for a signal disposition a guest asked for and
/// Detcore answered with success but did not install.
pub(crate) fn signal_handler_not_installed_warning(signum: i32) -> String {
    format!(
        "HERMIT_SIGNAL_HANDLER_NOT_INSTALLED signum={signum} (SIGSTKFLT): the guest set a handler \
         or SIG_IGN for it and rt_sigaction reported success, but Hermit did not install it, \
         because the backend reserves SIGSTKFLT (Reverie's PMU preemption timer, SaBRe's \
         controlled exit). A SIGSTKFLT the guest receives takes the default action. Hermit \
         answers success on purpose so that programs that register it routinely, such as the \
         Go runtime, still start."
    )
}

/// Tells the coordinator the signal controls an in-guest runtime refused
/// (`GlobalRequest::AnnounceRefusedSignalControls`). Like the publications of
/// Detcore's signal-state copy, it is sent directly, not through
/// `send_and_update_time`, and carries no logical time either way.
pub(crate) async fn announce_refused_signal_controls<G, T>(
    guest: &mut G,
    actions: u64,
    alternate_stack: bool,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // Answered before any clock accounting, so the clock and address-space
    // fields are placeholders, as `shared_open_file_request`'s are.
    let (_, response) = guest
        .send_rpc((
            DetTime::default(),
            MmId::initial(DetTid::from_raw(0)),
            GlobalRequest::AnnounceRefusedSignalControls(actions, alternate_stack),
        ))
        .await;
    debug_assert_eq!(response, GlobalResponse::AnnounceRefusedSignalControls(()));
}

/// Tells the coordinator a guest's disposition for `signum` was answered with
/// success but not installed (`GlobalRequest::AnnounceSignalHandlerNotInstalled`).
pub(crate) async fn announce_signal_handler_not_installed<G, T>(guest: &mut G, signum: i32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) = send_and_update_time(
        guest,
        GlobalRequest::AnnounceSignalHandlerNotInstalled(signum),
    )
    .await;
    assert_eq!(
        response,
        GlobalResponse::AnnounceSignalHandlerNotInstalled(())
    );
}

/// Tells the coordinator why the fail-closed policy is ending the run, before
/// this Tool image exits (`GlobalRequest::PolicyRefusal`).
pub(crate) async fn report_policy_refusal<G, T>(guest: &mut G, reason: String)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) = send_and_update_time(guest, GlobalRequest::PolicyRefusal(reason)).await;
    assert!(
        matches!(
            response,
            GlobalResponse::PolicyRefusal(()) | GlobalResponse::ThreadExited
        ),
        "unexpected response to PolicyRefusal: {response:?}"
    );
}

/// Mirrors a successful `set_tid_address(2)` into scheduler-owned exit state.
/// Tells the scheduler the calling thread's own signal mask while Detcore holds
/// it under a private blocking mask (`Some`), or that the guest's mask is back
/// (`None`).
pub(crate) async fn set_saved_guest_sigmask<G, T>(guest: &mut G, mask: Option<u64>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) =
        send_and_update_time(guest, GlobalRequest::SetSavedGuestSigmask(mask)).await;
    assert!(
        matches!(
            response,
            GlobalResponse::SetSavedGuestSigmask(()) | GlobalResponse::ThreadExited
        ),
        "unexpected response to SetSavedGuestSigmask: {response:?}"
    );
}

/// The guest's own signal mask that `thread` published while Detcore holds it
/// under a private blocking mask, if any (`Scheduler::published_guest_sigmask`):
/// the `SigBlk` its `/proc` status reports in place of the kernel's
/// (https://github.com/rrnewton/hermit/issues/4273, R2b-2 stage 2).
pub(crate) async fn published_guest_sigmask<G, T>(guest: &mut G, thread: DetTid) -> Option<u64>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) =
        send_and_update_time(guest, GlobalRequest::PublishedGuestSigmask(thread)).await;
    match response {
        GlobalResponse::PublishedGuestSigmask(mask) => mask,
        GlobalResponse::ThreadExited => None,
        other => panic!("unexpected response to PublishedGuestSigmask: {other:?}"),
    }
}

/// Tells the scheduler the set the calling thread waits for while it polls
/// `rt_sigtimedwait` (`Some`), or that it polls no more (`None`).
pub(crate) async fn set_sigtimedwait_set<G, T>(guest: &mut G, set: Option<u64>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) = send_and_update_time(guest, GlobalRequest::SetSigtimedwaitSet(set)).await;
    assert!(
        matches!(
            response,
            GlobalResponse::SetSigtimedwaitSet(()) | GlobalResponse::ThreadExited
        ),
        "unexpected response to SetSigtimedwaitSet: {response:?}"
    );
}

/// Tells the scheduler the calling thread sends SIGSTKFLT, in its own turn
/// (`Scheduler::record_guest_marker_signal`).
pub(crate) async fn record_guest_marker_signal<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) = send_and_update_time(guest, GlobalRequest::RecordGuestMarkerSignal).await;
    assert!(
        matches!(response, GlobalResponse::RecordGuestMarkerSignal(())),
        "unexpected response to RecordGuestMarkerSignal: {response:?}"
    );
}

/// The inputs to a process-directed signal the calling thread sends to its own
/// process, read in its turn (`Scheduler::own_process_signal_inputs`).
pub(crate) async fn own_process_signal_inputs<G, T>(guest: &mut G) -> OwnProcessSignalInputs
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(guest, GlobalRequest::OwnProcessSignalInputs)
        .await
        .1
    {
        GlobalResponse::OwnProcessSignalInputs(inputs) => inputs,
        response => unreachable!("unexpected response to OwnProcessSignalInputs: {response:?}"),
    }
}

pub(crate) async fn set_child_tid_address<G, T>(guest: &mut G, address: usize)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) =
        send_and_update_time(guest, GlobalRequest::SetChildTidAddress(address)).await;
    assert_eq!(response, GlobalResponse::SetChildTidAddress(()));
}

/// Publishes `dispositions`, publication `generation`, as the calling
/// thread's process's (`Scheduler::publish_signal_dispositions`; `None`
/// withdraws them), and returns the scheduler's answer: the generation it
/// holds afterwards and whether this publication is what it holds, or `None`
/// when it took nothing from this thread. Like the zero-time control messages it carries no logical time and
/// its answer carries none back. It is sent from `send_and_update_time`, not
/// through it, so it neither consumes a grant's deadline nor counts forwarded
/// records.
pub(crate) async fn publish_signal_dispositions<G, T>(
    guest: &mut G,
    generation: u64,
    dispositions: Option<crate::syscalls::signal_shadow::CopiedDispositions>,
) -> Option<crate::syscalls::signal_shadow::PublicationAnswer>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let state = guest.thread_state();
    let message = (
        state.thread_logical_time.clone(),
        state.mm_id,
        GlobalRequest::PublishSignalDispositions(generation, dispositions),
    );
    match guest.send_rpc(message).await.1 {
        GlobalResponse::PublishSignalDispositions(answer) => answer,
        _ => None,
    }
}

/// Whether the scheduler took `mask` as the calling thread's
/// (`Scheduler::publish_signal_mask`; `None` withdraws it). Sent like
/// [`publish_signal_dispositions`].
pub(crate) async fn publish_signal_mask<G, T>(guest: &mut G, mask: Option<u64>) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let state = guest.thread_state();
    let message = (
        state.thread_logical_time.clone(),
        state.mm_id,
        GlobalRequest::PublishSignalMask(mask),
    );
    matches!(
        guest.send_rpc(message).await.1,
        GlobalResponse::PublishSignalMask(true)
    )
}

async fn publish_runtime_private_memory<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some(facts) = guest.runtime_private_memory_regions() else {
        return;
    };
    let state = guest.thread_state();
    let mm = state.mm_id;
    if state.published_runtime_private_memory.as_ref() == Some(&(mm, facts.clone())) {
        return;
    }
    let message = (
        state.thread_logical_time.clone(),
        mm,
        GlobalRequest::PublishRuntimePrivateMemory(facts.clone()),
    );
    if guest.send_rpc(message).await.1 == GlobalResponse::PublishRuntimePrivateMemory(true) {
        guest.thread_state_mut().published_runtime_private_memory = Some((mm, facts));
    }
}

pub async fn send_and_update_time<G, T>(
    guest: &mut G,
    request: GlobalRequest,
) -> (Option<LogicalTime>, GlobalResponse)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // The successful exec callback must bind a surviving worker to its new
    // backend TID before issuing ordinary replacement-image requests. In
    // particular, a missed reconnect must not turn ThreadExited into exit(0).
    // Consuming cancellation and the reconnect itself use their authenticated
    // direct RPC paths instead of this ordinary-request helper.
    assert_eq!(
        DetTid::from_raw(guest.tid().as_raw()),
        guest.thread_state().dettid,
        "replacement image must reconnect before ordinary RPCs"
    );
    // Any request may let another thread run, which may then decide from
    // this thread's process's dispositions or this thread's mask.
    crate::syscalls::signal_shadow::publish_dispositions(guest).await;
    crate::syscalls::signal_shadow::publish_mask(guest).await;
    if !matches!(request, GlobalRequest::StartNewThread(..)) {
        publish_runtime_private_memory(guest).await;
    }
    let mytime = guest.thread_state().thread_logical_time.clone();
    let mm = guest.thread_state().mm_id;
    let continues_sigreturn = request_continues_sigreturn(&request);
    let starts_thread = matches!(request, GlobalRequest::StartNewThread(..));
    let mut resp = guest.send_rpc((mytime, mm, counted_request(request))).await;
    // A grant while the request was pending is a hand-off to the scheduler,
    // the one place every hand-off is noted (`ThreadState::note_scheduler_handoff`).
    if let GlobalResponse::Granted(response) = resp.1 {
        resp.1 = *response;
        guest
            .thread_state_mut()
            .note_scheduler_handoff(continues_sigreturn);
    }
    if let GlobalResponse::WithSliceBoundaryOwner(owner, response) = resp.1 {
        resp.1 = *response;
        crate::slice_boundary::adopt_owner(guest.thread_state_mut(), owner, starts_thread)
            .unwrap_or_else(|error| crate::scheduler::exit_on_scheduler_refusal(&error));
    }
    let boundary_budget = match resp.1 {
        GlobalResponse::WithSliceBoundaryBudget(budget, response) => {
            resp.1 = *response;
            Some(budget)
        }
        _ => None,
    };
    let deadline = match resp.1 {
        GlobalResponse::WithDeadline(remaining, response) => {
            resp.1 = *response;
            Some(remaining)
        }
        _ => None,
    };
    if let GlobalResponse::WithSliceBoundaryStack(path, response) = resp.1 {
        resp.1 = *response;
        write_backtrace(guest, path.as_ref());
    }
    guest.thread_state_mut().last_grant_deadline = deadline;
    cap_timeslice_at_deadline(guest, deadline);
    if resp.1 == GlobalResponse::Retired {
        // A committed SIGKILL victim (`Scheduler::is_unaccounted_sigkill_victim`):
        // this callback never resumes. No error, errno or injection reaches
        // the backend; its terminal owner consumes the real death.
        trace!(
            "[detcore, dtid {}] waiting for the kernel to end a SIGKILLed thread",
            guest.thread_state().dettid
        );
        return std::future::pending().await;
    }
    if resp.1 == GlobalResponse::ThreadExited {
        let dettid = guest.thread_state().dettid;
        // A thread an exit_group killed, on DBT where the exit_group's handler
        // walks the group's robust lists after the grant: its own native exit
        // would let Linux mark and clear the same words before the walk reads
        // them, so it retires only after the walk
        // (https://github.com/rrnewton/hermit/issues/4302).
        guest.thread_state().cancel_exit_group_filing();
        if !guest
            .thread_state()
            .wait_for_exit_group_robust_walk(crate::tool_local::EXIT_GROUP_ROBUST_WALK_WAIT)
        {
            // Retiring now would let this thread's native exit race a walk
            // that may still be running, so the run fails instead.
            panic!(
                "[detcore, dtid {}] EXIT_GROUP_ROBUST_WALK_TIMEOUT: the exit_group's robust-list walk neither finished nor was cancelled within {:?}",
                dettid,
                crate::tool_local::EXIT_GROUP_ROBUST_WALK_WAIT,
            );
        }
        trace!(
            "[detcore, dtid {}] exiting after terminal scheduler cancellation",
            dettid
        );
        // This exact task must never resume, but a sibling may still own the
        // winning exit-group commit. Retire only this task; explicit backend
        // cancellation could cancel that issuer or lose its later status.
        guest.retire_current_thread().await
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review applying the coordinator clock after exec reload.
    if let Some(time) = resp.0 {
        guest
            .thread_state_mut()
            .thread_logical_time
            .advance_to(time);
    }
    if let Some(budget) = boundary_budget {
        let installed = if starts_thread {
            crate::slice_boundary::grant::stage_start(guest.thread_state_mut(), budget)
        } else {
            crate::slice_boundary::grant::install(guest.thread_state_mut(), budget)
        };
        installed.unwrap_or_else(|error| crate::scheduler::exit_on_scheduler_refusal(&error));
    }
    resp
}

/// When the thread resumes after a potentially-blocking scheduler request, is it a normal
/// continuation of execution, or is it because the thread will now execute a signal handler.
/// If the latter, that interrupts logically blocking syscalls that were in progress.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum ResumeStatus {
    Normal,
    Signaled(Option<Vec<SigWrapper>>),
    /// A validated one-shot SIGALRM wake of this exact in-guest sleep.
    InGuestFatalAlarm,
    /// Granted, and nothing else can run (`SchedResponse::GoAlone`).
    NothingElseCanRun,
}

/// Internal result of a scheduler operation. Terminal results become
/// [`GlobalResponse::ThreadExited`] before returning to the guest-side RPC helper.
#[derive(PartialEq, Debug, Eq, Clone)]
enum SchedulerRpcResult<T> {
    Continue(T),
    ThreadExited,
}

/// Wake one waiter per word an exec's owner death marked, for the next
/// deterministic drain (https://github.com/rrnewton/hermit/issues/4268).
/// The INFO record is not logged on DBT, whose coordinator has no tracing
/// subscriber, so there an L2 run compares the guest's output only (Claude
/// review of https://github.com/rrnewton/hermit/pull/4352, F2).
fn wake_exec_robust_waiters(sched: &mut Scheduler, wakes: &[(DetTid, FutexID)], at: &str) {
    if wakes.is_empty() {
        return;
    }
    let counts = sched.wake_futex_waiters_after_exit(wakes);
    for ((owner, futex), count) in wakes.iter().zip(counts) {
        info!(
            "[detcore, dtid {}] robust-list owner death woke {} waiter(s) on futex {:?} at {}",
            owner, count, futex, at,
        );
    }
}

/// A process pidfd for the host process `pid` (`pidfd_open` with no flags),
/// which reports exit only once the whole thread group has exited.
pub(crate) fn open_process_pidfd(pid: i32) -> std::io::Result<std::os::fd::OwnedFd> {
    // SAFETY: pidfd_open takes no pointers.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pidfd_open returned a new descriptor owned by this process.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as libc::c_int) })
}

/// The pidfd that holds a SIGKILL's sender until victim `detpid` is gone, or
/// none when there is nothing left to wait for. The sender is never left
/// unfenced (Codex design review of
/// https://github.com/rrnewton/hermit/issues/4362 part 2, P2-3):
/// - for a kill that named the process (`process_directed`: kill or a pidfd),
///   a zombie whose exit the schedule granted: nothing to wait for
///   (https://github.com/rrnewton/hermit/issues/4411);
/// - otherwise the victim's host process is the one registered for `named`,
///   the thread the kill named, or else, for a kill that named the process,
///   for the victim's lowest registered thread, as when a pidfd names a leader
///   that has exited while its siblings run on
///   (https://github.com/rrnewton/hermit/issues/4411). A thread-directed
///   SIGKILL to an exited thread gets no fallback, because Linux does not end
///   the group for it (Claude review of
///   https://github.com/rrnewton/hermit/pull/4439, R1). With no such
///   thread, the run is refused: whether the victim still has a live thread
///   cannot decide it, because the kill may already have reached the kernel
///   (Claude re-check of https://github.com/rrnewton/hermit/pull/4423, N1);
/// - opening the process pidfd fails with ESRCH (or EINVAL, which some older
///   kernels gave for a reaped process whose pid number lived on as a process
///   group or session id): the process is gone only if that thread's own
///   `PIDFD_THREAD` pidfd, held since it registered, reports that it exited;
///   otherwise the run is refused;
/// - any other open failure, such as EMFILE: the run is refused.
fn sigkill_fence_for(
    scheduler: &Scheduler,
    named: DetTid,
    detpid: DetPid,
    process_directed: bool,
    open: impl Fn(i32) -> std::io::Result<std::os::fd::OwnedFd>,
) -> Result<Option<std::os::fd::OwnedFd>, crate::scheduler::sigkill_retirement::UnfencedSigkill> {
    let unfenced = |reason: String| crate::scheduler::sigkill_retirement::UnfencedSigkill {
        process: detpid,
        reason,
    };
    if process_directed && scheduler.process_exit_granted(detpid) {
        return Ok(None);
    }
    let Some(via) = scheduler.sigkill_victim_registered_thread(named, detpid, process_directed)
    else {
        return Err(unfenced(if process_directed {
            format!(
                "neither the thread it named (dettid {named}) nor any other thread of it has a registered host process"
            )
        } else {
            format!(
                "the thread it named (dettid {named}) has no registered host process, and a SIGKILL to that thread alone does not end its group"
            )
        }));
    };
    let host = scheduler
        .physical_process_of(via)
        .expect("a registered thread has a host process");
    match open(host) {
        Ok(pidfd) => Ok(Some(pidfd)),
        Err(error)
            if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::EINVAL))
                && scheduler.registered_thread_has_exited(via) == Some(true) =>
        {
            Ok(None)
        }
        Err(error) => Err(unfenced(format!(
            "pidfd_open of its host process failed: {error}"
        ))),
    }
}

/// `sigkill_fence_for`, ending the run on its refusal before the sender is
/// answered.
fn fence_or_refuse(
    scheduler: &Scheduler,
    named: DetTid,
    detpid: DetPid,
    process_directed: bool,
) -> Option<(DetPid, std::os::fd::OwnedFd)> {
    match sigkill_fence_for(
        scheduler,
        named,
        detpid,
        process_directed,
        open_process_pidfd,
    ) {
        Ok(fence) => fence.map(|pidfd| (detpid, pidfd)),
        Err(refusal) => crate::scheduler::exit_on_scheduler_refusal(&refusal),
    }
}

/// Wait until the process behind `pidfd` has exited, for at most `valve`;
/// why not, otherwise.
async fn wait_for_process_exit(pidfd: std::os::fd::OwnedFd, valve: Duration) -> Result<(), String> {
    let fd = tokio::io::unix::AsyncFd::with_interest(pidfd, tokio::io::Interest::READABLE)
        .map_err(|error| format!("its pidfd cannot be awaited: {error}"))?;
    match tokio::time::timeout(valve, fd.readable()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("waiting on its pidfd failed: {error}")),
        Err(_) => Err(format!("it was not gone within {valve:?}")),
    }
}

/// Whether the reply to `request` can resume its thread from a scheduler grant
/// (`Scheduler::unblock_guest`), or answers a timer it armed: the requests
/// whose replies carry a deadline hint
/// (<https://github.com/rrnewton/hermit/issues/4121>). Every handler that
/// awaits a grant is reached from one of these.
/// Whether `request` is a signal delivery that continues the thread's previous
/// one (`Resources::continues_sigreturn`), as an ordinary or a parked request.
fn request_continues_sigreturn(request: &GlobalRequest) -> bool {
    match request {
        GlobalRequest::WithForwardedCount(_, request) => request_continues_sigreturn(request),
        GlobalRequest::RequestResources(resources, _)
        | GlobalRequest::ParkedRequest(resources, ..) => resources.continues_sigreturn,
        _ => false,
    }
}

// Only the actual sender's registration is pre-admission. A parent's
// continuation after registering a different child remains an ordinary grant.
fn registers_own_image(sender: DetTid, request: &GlobalRequest) -> bool {
    match request {
        GlobalRequest::WithForwardedCount(_, request) => registers_own_image(sender, request),
        GlobalRequest::CreateChildThread(child, ..) => *child == sender,
        GlobalRequest::CreateVforkChildThread(_, _, child, ..) => *child == sender,
        _ => false,
    }
}

fn resumes_from_grant(request: &GlobalRequest) -> bool {
    match request {
        GlobalRequest::WithForwardedCount(_, request) => resumes_from_grant(request),
        GlobalRequest::RequestResources(..)
        | GlobalRequest::ParkedRequest(..)
        | GlobalRequest::ResumeParkedRequest { .. }
        | GlobalRequest::ResumeExec(..)
        | GlobalRequest::CreateChildThread(..)
        | GlobalRequest::CreateVforkChildThread(..)
        | GlobalRequest::StartNewThread(..)
        | GlobalRequest::FutexAction(..)
        | GlobalRequest::TraceSchedEvent(..)
        | GlobalRequest::RegisterAlarm(..)
        | GlobalRequest::AlarmRemaining(..)
        | GlobalRequest::ReconcileAlarm(..)
        | GlobalRequest::RegisterPosixTimer(..) => true,
        _ => false,
    }
}

/// End this thread's slice no later than the earliest deadline that was
/// pending when it was granted the turn it resumes
/// (<https://github.com/rrnewton/hermit/issues/4121>).
///
/// A deadline is acted on only at a scheduler turn, and a thread running a
/// long slice of cheap syscalls takes none, so without this a sleeper's
/// wake-up waits for the whole slice.
pub(crate) fn cap_timeslice_at_deadline<G, T>(guest: &mut G, deadline: Option<Duration>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some(remaining) = deadline else {
        return;
    };
    let clock_multiplier = guest.config().clock_multiplier.unwrap_or(1.0);
    let schedule_replay = guest.config().replay_schedule_from.is_some();
    // A configuration with an invalid turn cost is refused before any guest
    // runs; zero here only drops the floor.
    let min_slice = Duration::from_nanos(guest.config().scheduler_turn_charge().unwrap_or(0));
    guest.thread_state_mut().cap_timeslice_at_deadline(
        remaining,
        clock_multiplier,
        min_slice,
        schedule_replay,
    );
}

/// Global method RPC to request to control a resource.
///
/// Blocking: future returns only when resources are fully acquired.
pub async fn resource_request<G, T>(guest: &mut G, mut r: Resources) -> ResumeStatus
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // A request made while handling a syscall whose cost the bootstrap window
    // withholds carries that fact to the scheduler, which then withholds the
    // turn's scheduler time too (see `Scheduler::bump_global_time`). Only a
    // backend whose `is_backend_runtime_bootstrap` can return true ever sets it.
    r.backend_runtime_bootstrap |=
        guest.thread_state().in_uncharged_bootstrap_syscall && guest.is_backend_runtime_bootstrap();
    // Any turn of its own is a hand-off under `--pipe-wake-latency`: the
    // thread goes back behind the pollers it readied (`crate::pipe_wake`).
    guest.thread_state_mut().pipe_handoff_due = None;
    if guest.config().sequentialize_threads {
        if let Some(lease) = guest.signal_observation_lease() {
            if let Some(site) = guest.parked_signal_site() {
                return parked::capable_resource_request(
                    guest,
                    r,
                    ControlCapability::PublishOnly { lease, site },
                )
                .await;
            }
            parked::terminate_protocol(guest, ProtocolFailure::Identity).await;
        }
        let dettid = guest.thread_state().dettid;
        let detpid = guest.thread_state().detpid.expect("detpid unset");
        trace!(
            "[detcore, dtid {}] BLOCKING on resource_request rpc... {:?}",
            &dettid, r
        );
        let resp =
            send_and_update_time(guest, GlobalRequest::RequestResources(r.clone(), detpid)).await;
        match resp.1 {
            GlobalResponse::RequestResources(x) => {
                trace!(
                    "[detcore, dtid {}] UNBLOCKED, acquired resources: {:?}",
                    &dettid, r
                );
                x
            }
            _ => unreachable!(),
        }
    } else {
        ResumeStatus::Normal
    }
}

/// Global method RPC to release all held resources.
///
/// Nonblocking: future may return immediately before the central global object has
/// processed the resource release.
pub async fn resource_release_all<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.config().sequentialize_threads {
        let resp = send_and_update_time(guest, GlobalRequest::ReleaseAllResources).await;
        match resp.1 {
            GlobalResponse::ReleaseAllResources(x) => x,
            _ => unreachable!(),
        }
    }
}

/// Global method RPC to allow a new thread to begin execution, called from the child thread.
///
/// Blocking: future returns only when the thread execution is truly ready to proceed.
///
/// Returns: a history of the thread preemptions, for it to play back when --replay-preemptions-from
/// is used.
pub async fn thread_start_request<G, T>(
    cfg: &Config,
    guest: &mut G,
    detpid: DetPid,
) -> Option<ThreadHistory>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    if cfg.sequentialize_threads {
        trace!("[detcore, dtid {}] new thread BLOCKING on rpc...", &dettid);
        let physical_ids = guest
            .thread_state()
            .physical_tid
            .map(|tid| (guest.pid().as_raw(), tid));
        let signal_identity = guest.signal_task_identity();
        guest.thread_state_mut().signal_task_identity = signal_identity;
        let resp = send_and_update_time(
            guest,
            GlobalRequest::StartNewThread(dettid, detpid, physical_ids, signal_identity),
        )
        .await;
        match resp.1 {
            GlobalResponse::StartNewThread(preempts) => {
                // Publish only after the deterministic start grant. Before
                // it, parent registration and this RPC can race in host time.
                publish_runtime_private_memory(guest).await;
                trace!("[detcore, dtid {}] new thread UNBLOCKED (post-rpc)", dettid);
                preempts
            }
            _ => unreachable!(),
        }
    } else {
        None
    }
}

/// Complete the existing startup reset after its grant. In particular, a
/// boundary budget arrives before this reset and is installed only after it;
/// the legacy preemption-history and deadline-cap ordering is unchanged.
pub(crate) fn finish_thread_start<G, T>(cfg: &Config, guest: &mut G, history: Option<ThreadHistory>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let reset = {
        let thread = guest.thread_state_mut();
        thread.preemption_points = history.map(|history| history.into_iter());
        crate::slice_boundary::grant::reset_start(thread, cfg)
            .unwrap_or_else(|error| crate::scheduler::exit_on_scheduler_refusal(&error))
    };
    let deadline = guest.thread_state_mut().last_grant_deadline.take();
    cap_timeslice_at_deadline(guest, deadline);
    crate::slice_boundary::grant::finish_start(guest.thread_state_mut(), reset)
        .unwrap_or_else(|error| crate::scheduler::exit_on_scheduler_refusal(&error));
}

/// Keep only the clone pointer that requests an exit-time clear and wake.
/// SETTID alone requests a birth-time store and must not register an exit wake.
pub(crate) fn child_tid_clear_address(flags: CloneFlags, address: usize) -> usize {
    if flags.contains(CloneFlags::CLONE_CHILD_CLEARTID) {
        address
    } else {
        0
    }
}

/// Global method RPC for the parent to add a child-thread to the round-robin pool.
///
/// Nonblocking: future returning does not guarantee anything about the central scheduler,
/// except that it will eventually give a slot to the child.  Then the protocol is that
/// child will subsequently make a `thread_start_request` to gate the start of its execution.
pub async fn create_child_thread<G, T>(
    guest: &mut G,
    child_dettid: DetTid,
    ctid: usize,
    flags: Option<CloneFlags>,
    exit_signal: libc::c_int,
    physical_ids: Option<(i32, i32)>,
) -> Option<MmId>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // Random (or replayed) starting priority in chaos mode, constant priority otherwise.
    let starting_priority = if guest.config().replay_preemptions_from.is_some() {
        // In preemption replay mode, the initial priority is set on the other
        // side of the rpc, in recv_create_child_thread.
        None
    } else if guest.config().replay_schedule_from.is_some() {
        // FIXME!  Find a cleaner way to make the root thread start off high-priority:
        if child_dettid <= DetTid::from_raw(3) {
            Some(REPLAY_FOREGROUND_PRIORITY)
        } else {
            Some(REPLAY_DEFERRED_PRIORITY)
        }
    } else if guest.config().chaos {
        let entropy = guest
            .thread_state_mut()
            .chaos_prng_next_u64("child_priority");
        if guest.config().chaos_target_races {
            // Targeted chaos: bias a freshly created child to an extreme priority
            // so it either runs before the parent resumes or strictly after it,
            // instead of landing at a uniformly random priority. This maximizes
            // parent/child ordering divergence to surface fork/exec races.
            // Reproducible under `--fuzz-seed`/`--sched-seed`.
            if entropy.is_multiple_of(2) {
                Some(FIRST_PRIORITY)
            } else {
                Some(LAST_PRIORITY)
            }
        } else {
            Some(entropy_to_priority(entropy))
        }
    } else {
        Some(DEFAULT_PRIORITY)
    };

    let detpid = guest.thread_state().detpid.expect("detpid unset");

    let resp = send_and_update_time(
        guest,
        GlobalRequest::CreateChildThread(
            child_dettid,
            detpid,
            ctid,
            flags,
            exit_signal,
            physical_ids,
            starting_priority,
        ),
    )
    .await;
    match resp.1 {
        GlobalResponse::CreateChildThread(x) => x,
        _ => unreachable!(),
    }
}

/// Register a vfork child while its parent is blocked inside `clone(2)`.
///
/// Unlike an ordinary clone, the parent cannot perform this registration
/// because the kernel has blocked it until the child execs or exits. The child
/// therefore registers itself, carrying the inherited parent identity, flags,
/// and starting priority. The starting priority is derived the same way as an
/// ordinary clone so that chaos and replay scheduling stay deterministic.
pub async fn create_vfork_child_thread<G, T>(
    guest: &mut G,
    child_dettid: DetTid,
    vfork: crate::tool_local::PendingVfork,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let starting_priority = if guest.config().replay_preemptions_from.is_some() {
        None
    } else if guest.config().replay_schedule_from.is_some() {
        Some(if child_dettid <= DetTid::from_raw(3) {
            REPLAY_FOREGROUND_PRIORITY
        } else {
            REPLAY_DEFERRED_PRIORITY
        })
    } else if guest.config().chaos {
        Some(entropy_to_priority(vfork.child_priority_entropy.expect(
            "vfork child priority entropy missing in chaos mode",
        )))
    } else {
        // POSIX vfork suspends the parent until the child execs or _exits. Give
        // the vfork child a strictly higher priority (lower number) than the
        // parent's DEFAULT_PRIORITY so the deterministic scheduler always runs
        // the child first, rather than round-robining the parent and child at
        // equal priority (which leaves fork/exec ordering nondeterministic).
        Some(DEFAULT_PRIORITY - 1)
    };

    let resp = send_and_update_time(
        guest,
        GlobalRequest::CreateVforkChildThread(
            vfork.parent_dettid,
            vfork.parent_detpid,
            child_dettid,
            vfork.child_tid_addr,
            vfork.flags,
            vfork.exit_signal,
            starting_priority,
        ),
    )
    .await;
    match resp.1 {
        GlobalResponse::CreateChildThread(_) => (),
        _ => unreachable!(),
    }
}

/// Remove the thread from the scheduler.
///
/// Nonblocking: the future may return immediately, not guaranteeing the changes to the
/// scheduler have been completed.
pub(crate) async fn deregister_thread<R>(
    threads_time: DetTime,
    cfg: &Config,
    reverie: &R,
    thread: ThreadDeregistration,
) where
    // Note, this is called from a context where we DON'T have a full, operable `Guest`.
    R: GlobalRPC<GlobalState>,
{
    if cfg.sequentialize_threads {
        let mm = thread.mm;
        // TODO: void_send_rpc
        let resp = reverie
            .send_rpc((threads_time, mm, {
                // The thread's last count; its slot is free for a later thread.
                let request = counted_request(GlobalRequest::DeregisterThread(thread));
                crate::detlog::release_forwarded_count();
                request
            }))
            .await;
        // We can't update the thread time here.  But it's dead anyway!
        match resp.1 {
            GlobalResponse::DeregisterThread(x) => x,
            _ => unreachable!(),
        }
    }
}

/// Account this physical-exit callback's own clock before it joins a staged
/// robust-list batch. The empty wake request has no scheduler effects, but its
/// ordinary RPC header is admitted and accounted before it is acknowledged.
/// A retired incarnation must not contribute to the completed-exit barrier.
pub(crate) async fn acknowledge_robust_list_exit_time<R>(
    threads_time: DetTime,
    reverie: &R,
    mm: MmId,
) -> bool
where
    R: GlobalRPC<GlobalState>,
{
    let response = reverie
        .send_rpc((threads_time, mm, GlobalRequest::RobustListWakes(Vec::new())))
        .await;
    match response.1 {
        GlobalResponse::RobustListWakes(counts) => {
            assert!(
                counts.is_empty(),
                "an empty exit-clock acknowledgement woke a waiter"
            );
            true
        }
        GlobalResponse::ThreadExited => false,
        _ => unreachable!(),
    }
}

/// Deliver owner-death wakes from an exit callback that no longer has guest
/// memory access. The callback runs only after ptrace has observed physical
/// exit, so Linux's atomic robust-list word update is already complete.
pub(crate) async fn robust_list_wakes_after_exit<R>(
    threads_time: DetTime,
    reverie: &R,
    mm: MmId,
    wakes: Vec<(DetTid, RobustListWake)>,
) -> Vec<u64>
where
    R: GlobalRPC<GlobalState>,
{
    if wakes.is_empty() {
        return Vec::new();
    }
    let response = reverie
        .send_rpc((
            threads_time,
            mm,
            GlobalRequest::RobustListWakes(
                wakes
                    .into_iter()
                    .map(|(owner, wake)| (owner, wake.futex))
                    .collect(),
            ),
        ))
        .await;
    match response.1 {
        GlobalResponse::RobustListWakes(counts) => counts,
        _ => unreachable!(),
    }
}

/// Deliver the owner-death wakes staged at a successful exec's entry. The
/// exec has returned, so Linux has already walked every robust list in the old
/// thread group and changed each owner word atomically.
pub(crate) async fn robust_list_wakes_after_exec<G, T>(
    guest: &mut G,
    wakes: Vec<(DetTid, RobustListWake)>,
) -> Vec<u64>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if wakes.is_empty() {
        return Vec::new();
    }
    let (_, response) = send_and_update_time(
        guest,
        GlobalRequest::RobustListWakes(
            wakes
                .into_iter()
                .map(|(owner, wake)| (owner, wake.futex))
                .collect(),
        ),
    )
    .await;
    match response {
        GlobalResponse::RobustListWakes(counts) => counts,
        _ => unreachable!(),
    }
}

/// Which actions we can take before/after a futex system call.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum FutexAction {
    /// Check in before a FUTEX_WAIT, including an optional timeout and, on a backend
    /// that reports the kernel's signal state, the waiter's mask and where the
    /// scheduler reads its dispositions when it commits a wake. With `None`, as
    /// before, a scheduler-sent signal wakes the waiter and a cross-task signal
    /// does not.
    WaitRequest(Option<LogicalTime>, Option<FutexSignalWatch>),
    /// Check in after a FUTEX_WAIT
    WaitFinished,
    /// Check in before a FUTEX_WAKE, parameterized by the number of threads woken.
    WakeRequest(i32),
    /// Check in after a FUTEX_WAKE, parameterized by the number of threads woken.
    WakeFinished(i32),
    /// Check in for a plain FUTEX_REQUEUE or FUTEX_CMP_REQUEUE of the request's
    /// futex onto the given one, waking up to the first count and moving up to
    /// the second (`Scheduler::requeue_futex_waiters`). Both counts are already
    /// checked to be non-negative, and the futex word already compared for
    /// FUTEX_CMP_REQUEUE. Answers with the number woken plus the number moved,
    /// and is followed by a `WakeFinished`, as a wake is.
    RequeueRequest(FutexID, u32, u32),
}

/// Ask scheduler for permission to proceed before/after futex operation.
/// Returns true if the operation completed normally, and false if it timed out.
pub async fn futex_action<G, T>(
    guest: &mut G,
    futex_action: FutexAction,
    futexid: &FutexID,
    init_read: i32,
    mask: u32,
) -> Option<SchedValue>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    assert!(guest.config().sequentialize_threads);
    let dettid = guest.thread_state().dettid;
    let req = GlobalRequest::FutexAction(dettid, futex_action, *futexid, init_read, mask);
    trace!(
        "BLOCKING on futex_action: sending request to scheduler: {:?}",
        req
    );
    // Update local time from potentially blocking operation:
    let resp = send_and_update_time(guest, req.clone()).await;
    match resp.1 {
        GlobalResponse::FutexAction(answer) => {
            trace!("UNBLOCKING after futex_action. Request was: {:?}", req);
            answer
        }
        _ => unreachable!(),
    }
}

/// track a (possibly new) inode, by returning a deterministic inode.
/// Also return the logical mtime for the inode, though this is only
/// used if `virtualize_metadata` is set.
///
/// For callers that have not read the file's metadata. A stat must use
/// [`determinize_inode_observing_mtime`] so the first-seen mtime policy sees
/// the host mtime.
pub async fn determinize_inode<G, T>(guest: &mut G, inode: RawInode) -> (DetInode, LogicalTime)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    determinize_inode_observing_mtime(
        guest,
        inode,
        None,
        ObservedMtime::Unobserved,
        InodeSighting::Descriptor,
    )
    .await
}

/// [`determinize_inode`] for a directory entry, which names a linked file
/// (see [`InodeSighting::Name`]). `procfs` names a procfs entry (see
/// `procfs_inode::ProcfsName`).
pub async fn determinize_named_inode<G, T>(
    guest: &mut G,
    inode: RawInode,
    procfs: Option<ProcfsName>,
) -> (DetInode, LogicalTime)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    determinize_inode_observing_mtime(
        guest,
        inode,
        procfs,
        ObservedMtime::Unobserved,
        InodeSighting::Name,
    )
    .await
}

/// Like [`determinize_inode`], for a caller that observed the file's host
/// mtime. If the file's virtual mtime is still unresolved, `observed` resolves
/// it: a canonical mtime (see [`CANONICAL_FILE_MTIME_SECONDS`]) is kept, and any
/// other becomes the epoch. `procfs` names a procfs entry (see
/// `procfs_inode::ProcfsName`).
pub async fn determinize_inode_observing_mtime<G, T>(
    guest: &mut G,
    inode: RawInode,
    procfs: Option<ProcfsName>,
    observed: ObservedMtime,
    sighting: InodeSighting,
) -> (DetInode, LogicalTime)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(
        guest,
        GlobalRequest::DeterminizeInode(inode, observed, sighting, procfs),
    )
    .await;
    match resp.1 {
        GlobalResponse::DeterminizeInode(x) => x,
        _ => unreachable!(),
    }
}

/// Determinize the inode of every file-backed line of one `/proc/*/maps` read.
/// `inodes` holds one entry per line, in line order, with repeats: each entry
/// is one numbering request, so how many requests a read makes depends on the
/// guest's mappings alone, never on whether two lines name the same host file
/// (<https://github.com/rrnewton/hermit/issues/2897>). The result is in the
/// same order.
pub async fn determinize_mapping_inodes<G, T>(guest: &mut G, inodes: Vec<RawInode>) -> Vec<DetInode>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::DeterminizeMappingInodes(inodes)).await;
    match resp.1 {
        GlobalResponse::DeterminizeMappingInodes(x) => x,
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
/// Translate a host-assigned device number (`st_dev`) to a deterministic one.
pub async fn determinize_device<G, T>(guest: &mut G, raw_device: u64) -> u64
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::DeterminizeDevice(raw_device)).await;
    match resp.1 {
        GlobalResponse::DeterminizeDevice(x) => x,
        _ => unreachable!(),
    }
}

/// Translate an observed fdinfo mount ID to the shared run-local identity.
pub async fn determinize_mount_id<G, T>(
    guest: &mut G,
    raw_mount_id: u64,
    mountinfo_order: Option<Vec<u64>>,
) -> Option<u64>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(
        guest,
        GlobalRequest::DeterminizeMountId(raw_mount_id, mountinfo_order),
    )
    .await;
    match resp.1 {
        GlobalResponse::DeterminizeMountId(value) => value,
        _ => unreachable!(),
    }
}

/// Seed or validate the run-global mount-ID pool against one mountinfo
/// snapshot's canonical order, and return the run-local identity of each entry
/// of `observation_order`, numbering unseen entries in that order. `None`
/// means validation failed or the two orders name different raw IDs.
pub async fn resolve_mountinfo_identities<G, T>(
    guest: &mut G,
    mountinfo_order: Vec<u64>,
    observation_order: Vec<u64>,
) -> Option<Vec<u64>>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(
        guest,
        GlobalRequest::ResolveMountinfoIds(mountinfo_order, observation_order),
    )
    .await;
    match resp.1 {
        GlobalResponse::ResolveMountinfoIds(value) => value,
        _ => unreachable!(),
    }
}

/// The file behind `inode` lost its last name (see `InodePool::retire`).
pub async fn retire_inode<G, T>(guest: &mut G, inode: RawInode)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::RetireInode(inode)).await;
    match resp.1 {
        GlobalResponse::RetireInode(x) => x,
        _ => unreachable!(),
    }
}

/// The inode pool's state for a directory snapshot about to be taken (see
/// [`SnapshotMark`]).
pub async fn directory_snapshot_mark<G, T>(guest: &mut G) -> SnapshotMark
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::DirectorySnapshotMark).await;
    match resp.1 {
        GlobalResponse::DirectorySnapshotMark(mark) => mark,
        _ => unreachable!(),
    }
}

/// `--pipe-wake-latency`: report that the calling thread's action readied the
/// pollers of `channel` that want `wants` (`Scheduler::ready_channel`).
pub async fn ready_channel<G, T>(
    guest: &mut G,
    channel: crate::pipe_wake::ChannelId,
    wants: crate::pipe_wake::Wants,
) -> crate::pipe_wake::ReadyOutcome
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::ReadyChannel(channel, wants)).await;
    match resp.1 {
        GlobalResponse::ReadyChannel(outcome) => outcome,
        _ => unreachable!(),
    }
}

/// `--pipe-wake-latency`: the calling thread's hand-off is due. Returns
/// whether the scheduler moved it behind the pollers it readied, so that it
/// must now yield (`Scheduler::hand_off_turn`).
pub async fn hand_off_turn<G, T>(guest: &mut G) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::HandOffTurn).await;
    match resp.1 {
        GlobalResponse::HandOffTurn(yields) => yields,
        _ => false,
    }
}

/// Record that the calling thread is delivering a signal whose default action
/// ends its process (`Scheduler::record_committed_fatal_signal`).
pub async fn record_fatal_signal<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let _ = send_and_update_time(guest, GlobalRequest::RecordFatalSignal).await;
}

/// [`determinize_inode`] for an entry of a directory snapshot taken at
/// `mark` (see [`SnapshotMark`]). `procfs` names a procfs entry (see
/// `procfs_inode::ProcfsName`).
pub async fn determinize_listed_inode<G, T>(
    guest: &mut G,
    inode: RawInode,
    procfs: Option<ProcfsName>,
    mark: SnapshotMark,
) -> (DetInode, LogicalTime)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    determinize_inode_observing_mtime(
        guest,
        inode,
        procfs.map(|name| name.in_snapshot(mark.threads_counted)),
        ObservedMtime::Unobserved,
        InodeSighting::Listed(mark.retirements),
    )
    .await
}

/// An `open` found `inode` linked or freshly created (see
/// `InodePool::forget_retired`). `procfs` names a procfs entry (see
/// `procfs_inode::ProcfsName`), so that writes through the new descriptor
/// update the entry that stats number.
pub async fn forget_retired_inode<G, T>(guest: &mut G, inode: RawInode, procfs: Option<ProcfsName>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::ForgetRetiredInode(inode, procfs)).await;
    match resp.1 {
        GlobalResponse::ForgetRetiredInode(x) => x,
        _ => unreachable!(),
    }
}

/// Update the modification time for a file, using its inode.
/// This will set the mtime to a coherent global-time value.
pub async fn touch_file<G, T>(guest: &mut G, inode: RawInode)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::TouchFile(inode)).await;
    match resp.1 {
        GlobalResponse::TouchFile(x) => x,
        _ => unreachable!(),
    }
}

/// Set a file's virtual mtime to a guest-requested time.
pub async fn set_file_mtime<G, T>(guest: &mut G, inode: RawInode, mtime: LogicalTime)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::SetFileMtime(inode, mtime)).await;
    match resp.1 {
        GlobalResponse::SetFileMtime(x) => x,
        _ => unreachable!(),
    }
}

/// Read the global clock, or at least a deterministic lower bound on it.
pub async fn global_time_lower_bound<G, T>(guest: &mut G) -> LogicalTime
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::GlobalTimeLowerBound).await;
    match resp.1 {
        GlobalResponse::GlobalTimeLowerBound(x) => x,
        _ => unreachable!(),
    }
}

/// Take a time observation from the current thread. This extra indirection
/// helps abstract over whether or not we need to use local or global
/// information for this.
pub async fn thread_observe_time<G, T>(guest: &mut G) -> LogicalTime
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    global_time_lower_bound(guest).await
}

/// Writes a structured json backtrace to a given file
fn write_backtrace<G, T>(guest: &mut G, m_path: Option<&PathBuf>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Some(backtrace) = guest.backtrace() {
        if let Some(path) = m_path {
            let file = File::create(path).expect("Failed to open preemption stacktrace log file");
            serde_json::to_writer(file, &backtrace.force_pretty()).unwrap();
        } else {
            crate::util::capped_eprintln!("{}", backtrace.force_pretty());
        }
    } else {
        warn!("Could not read backtrace!");
    }
}

/// Additional instructions to a guest after shed events is consumed by global tool
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub struct TraceSchedEventResponse {
    print_stack_strace: MaybePrintStack,
    timeslice: Option<LogicalTime>,
}

struct SchedEventForLog<'a> {
    event: &'a SchedEvent,
    command_bootstrap: bool,
}

struct CommandBootstrapInstructionPointer(NonZeroUsize);

impl std::fmt::Debug for CommandBootstrapInstructionPointer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::logdiff::host_addr(self.0.get()))
    }
}

impl std::fmt::Debug for SchedEventForLog<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.command_bootstrap {
            return std::fmt::Debug::fmt(self.event, f);
        }
        f.debug_struct("SchedEvent")
            .field("dettid", &self.event.dettid)
            .field("op", &self.event.op)
            .field("count", &self.event.count)
            .field(
                "start_rip",
                &self.event.start_rip.map(CommandBootstrapInstructionPointer),
            )
            .field(
                "end_rip",
                &self.event.end_rip.map(CommandBootstrapInstructionPointer),
            )
            .field("end_time", &self.event.end_time)
            .finish()
    }
}

/// Record an event in the schedule trace, OR check the event on replay.
/// This also prints the backtrace of the schedevent, if indicated.
///
/// Arguments:
/// - tag_end_rip: read the current guest registers to fill in the `end_rip` on the event with the
///   current instruction pointer.
pub async fn trace_schedevent<G, T>(guest: &mut G, ev: SchedEvent, tag_end_rip: bool)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    assert!(guest.config().sequentialize_threads);

    // trace_schedevent is called AFTER the event is complete, and the rip is resting just after it.
    let ev = if tag_end_rip {
        let end_rip = if let Some(r) = ev.end_rip {
            r
        } else {
            // While a SIGALRM delivery's signal pipeline runs at a syscall's
            // completion, the registers are its completed projection.
            let regs = match guest.thread_state().signal_register_projection {
                Some(projection) => projection,
                None => guest.regs().await,
            };
            NonZeroUsize::new(regs.rip.try_into().unwrap()).unwrap()
        };
        SchedEvent {
            end_rip: Some(end_rip),
            ..ev
        }
    } else {
        ev
    };

    if tracing::enabled!(tracing::Level::TRACE)
        && let Some(rip) = ev.end_rip
    {
        let rip_addr = AddrMut::<u16>::from_raw(rip.into()).unwrap();
        // These bytes are diagnostic, not part of the recorded event. A guest
        // may legitimately make its memory unreadable (PR_SET_DUMPABLE), so
        // preserve that observation without unwinding the live run owner.
        let rip_contents = guest.memory().read_value(rip_addr);
        trace!(
            "Tracing sched event, after which rip is {}, next two instruction bytes {:?}",
            rip, rip_contents
        );
    }

    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let command_bootstrap = guest.is_command_bootstrap();
    let resp = send_and_update_time(
        guest,
        GlobalRequest::TraceSchedEvent(ev, detpid, command_bootstrap),
    )
    .await;

    trace!("trace_schedevent result: {:?}", resp);
    match resp {
        (
            _,
            GlobalResponse::TraceSchedEvent(TraceSchedEventResponse {
                print_stack_strace,
                timeslice,
            }),
        ) => {
            if let Some(m_path) = print_stack_strace {
                trace!("[trace_schedevent] writing stacktrace via Reverie...");
                write_backtrace(guest, m_path.as_ref());
            }

            if let Some(timeslice) = timeslice
                && guest.thread_state().past_global_first_execve
            {
                let end_of_timeslice =
                    guest.thread_state().thread_logical_time.as_nanos() + timeslice;
                trace!(
                    "[detcore][dettid {}] setting end_of_timeslice to {:?} as instructed by replayer",
                    guest.thread_state().dettid,
                    end_of_timeslice
                );
                guest.thread_state_mut().end_of_timeslice = Some(end_of_timeslice);
                if guest.config().max_timeslice.is_some() {
                    guest.thread_state_mut().max_timeslice_end = Some(end_of_timeslice);
                }
            }
        }
        _ => {
            unreachable!()
        }
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#663)
// TODO-HUMAN-REVIEW(#869)
/// Register an alarm (delayed signal delivery) with the global scheduler.
/// Returns the logical duration remaining until any previously scheduled alarm.
pub async fn register_alarm<G, T>(
    guest: &mut G,
    duration: LogicalTime,
    interval: LogicalTime,
    sig: Signal,
) -> (LogicalTime, LogicalTime)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(
        guest,
        GlobalRequest::RegisterAlarm(detpid, dettid, duration, interval, SigWrapper::from(sig)),
    )
    .await;
    match resp.1 {
        GlobalResponse::RegisterAlarm(x) => x,
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-841): Review logical alarm query API.
/// Return the logical duration remaining on the process's one-shot alarm.
pub async fn alarm_remaining<G, T>(guest: &mut G) -> ItimerSnapshot
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(guest, GlobalRequest::AlarmRemaining(detpid)).await;
    match resp.1 {
        GlobalResponse::AlarmRemaining(remaining) => remaining,
        _ => unreachable!(),
    }
}

/// Rule 4/5 notifications for the coalescing ITIMER_REAL
/// (<https://github.com/rrnewton/hermit/issues/4139>): a committed guest
/// SIGALRM send, or a change of SIGALRM's disposition.
pub async fn note_sigalrm_event<G, T>(guest: &mut G, request: GlobalRequest)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, request).await;
    match resp.1 {
        GlobalResponse::NoteGuestSigalrm => {}
        _ => unreachable!(),
    }
}

/// O2: after a call that may have dequeued a SIGALRM from this process's
/// shared queue (a signalfd read, an `rt_sigtimedwait` completion), let the
/// scheduler restart an `Inactive` ITIMER_REAL at this point in the turn
/// (<https://github.com/rrnewton/hermit/issues/4139>).
pub async fn reconcile_alarm<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(guest, GlobalRequest::ReconcileAlarm(detpid)).await;
    match resp.1 {
        GlobalResponse::ReconcileAlarm => {}
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#869)
/// Register, re-arm, or disarm a POSIX timer with the global scheduler, or
/// delete it.
pub async fn register_posix_timer<G, T>(
    guest: &mut G,
    timer_id: i32,
    deadline: Option<LogicalTime>,
    interval: LogicalTime,
    notification: PosixTimerNotification,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(
        guest,
        GlobalRequest::RegisterPosixTimer(
            detpid,
            dettid,
            timer_id,
            deadline,
            interval,
            notification,
        ),
    )
    .await;
    match resp.1 {
        GlobalResponse::RegisterPosixTimer(()) => {}
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#663)
/// Return the scheduler's live threads for a positive process ID.
/// Does a live thread with this tid exist, leader or not?
///
/// Distinct from [`resolve_kill_targets`], which models `kill(2)` and therefore
/// only recognises thread-group leaders. Syscalls that resolve a task through
/// `find_task_by_vpid` -- `sched_setattr` among them -- must use this instead,
/// or a non-leader thread reports ESRCH while it is plainly running.
pub async fn thread_is_live<G, T>(guest: &mut G, dettid: DetTid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ThreadIsLive(dettid)).await;
    match response.1 {
        GlobalResponse::ThreadIsLive(live) => live,
        _ => unreachable!(),
    }
}

/// Whether a `clone3` `set_tid` naming `dettid` is refused: a task of this
/// run had that id, and it is not the pid of a zombie child of the caller's
/// process (`Scheduler::id_is_a_zombie_child_of`). "Had" is a registration with
/// the scheduler, which is never removed, or a clock component in the global
/// time (which exec reassignment can move away from an id that stays
/// registered). Such a zombie's pid is left to the kernel, which answers
/// EEXIST, and only the caller's own process can release it. Any other used id is
/// refused, a live task's included: whether a live thread is still alive when
/// the kernel runs the call can depend on host timing (a thread killed
/// without a granted exit deregisters when the host says), and a freed id
/// given to a new task would merge two tasks' state, keyed by that id, in
/// Detcore (`Detcore::clone3_set_tid_refusal`,
/// https://github.com/rrnewton/hermit/issues/4370).
pub(crate) async fn thread_id_refused<G, T>(guest: &mut G, dettid: DetTid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ThreadIdRefused(dettid)).await;
    match response.1 {
        GlobalResponse::ThreadIdRefused(refused) => refused,
        _ => unreachable!(),
    }
}

/// Whether `named` is a thread-group leader the scheduler retired at its own
/// exit while its group lives on, so the kernel shows it a zombie
/// (`Scheduler::names_a_retired_exited_leader`,
/// https://github.com/rrnewton/hermit/issues/4445).
pub(crate) async fn names_a_retired_exited_leader<G, T>(guest: &mut G, named: DetTid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response =
        send_and_update_time(guest, GlobalRequest::NamesARetiredExitedLeader(named)).await;
    match response.1 {
        GlobalResponse::NamesARetiredExitedLeader(retired) => retired,
        _ => unreachable!(),
    }
}

/// Arm (`Some(signal)`, 0 disarms) or read (`None`) the calling task's
/// parent-death signal that Detcore sends itself when its parent thread
/// leaves (`Scheduler::deliver_parent_death_signals`,
/// https://github.com/rrnewton/hermit/issues/4440). Returns the value held
/// after the call.
pub(crate) async fn parent_death_signal<G, T>(guest: &mut G, arm: Option<i32>) -> i32
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ParentDeathSignal(arm)).await;
    match response.1 {
        GlobalResponse::ParentDeathSignal(signal) => signal,
        _ => unreachable!(),
    }
}

/// The real-time signals (a kernel sigset) of `detpid`'s POSIX timers that
/// expired while every thread of the process blocked them, and that are still
/// pending (https://github.com/rrnewton/hermit/issues/4089). Asked in the
/// calling thread's turn, while no timed event can pop, so the answer is a
/// function of the schedule.
pub(crate) async fn pending_real_time_expirations<G, T>(guest: &mut G, detpid: DetPid) -> u64
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response =
        send_and_update_time(guest, GlobalRequest::PendingRealTimeExpirations(detpid)).await;
    match response.1 {
        GlobalResponse::PendingRealTimeExpirations(pending) => pending,
        GlobalResponse::ThreadExited => 0,
        other => unreachable!("unexpected response to PendingRealTimeExpirations: {other:?}"),
    }
}

/// Record that a handler's return in `detpid` restores a mask leaving
/// `unblocked` (real-time signals) unblocked; see
/// `Scheduler::record_handler_restore_unblocked`.
pub(crate) async fn record_handler_restore_mask<G, T>(guest: &mut G, detpid: DetPid, unblocked: u64)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(
        guest,
        GlobalRequest::RecordHandlerRestoreMask(detpid, unblocked),
    )
    .await;
    assert!(
        matches!(
            response.1,
            GlobalResponse::RecordHandlerRestoreMask(()) | GlobalResponse::ThreadExited
        ),
        "unexpected response to RecordHandlerRestoreMask: {:?}",
        response.1
    );
}

/// Record that `detpid` created a signalfd for `mask` (a kernel sigset with a
/// real-time signal); see `Scheduler::register_signalfd`.
pub(crate) async fn register_signalfd<G, T>(guest: &mut G, detpid: DetPid, mask: u64)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::RegisterSignalfd(detpid, mask)).await;
    assert!(
        matches!(
            response.1,
            GlobalResponse::RegisterSignalfd(()) | GlobalResponse::ThreadExited
        ),
        "unexpected response to RegisterSignalfd: {:?}",
        response.1
    );
}

/// Record, under record and replay, that the calling thread's `close`,
/// `close_range`, `dup2` or `dup3` released or replaced its descriptors
/// `first..=last`; see `Scheduler::note_descriptors_replaced`.
pub(crate) async fn descriptors_replaced<G, T>(guest: &mut G, first: u32, last: u32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if !guest.config().recordreplay_modes {
        return;
    }
    let files = guest.thread_state().files_id();
    let response = send_and_update_time(
        guest,
        GlobalRequest::DescriptorsReplaced(files, first, last),
    )
    .await;
    assert!(
        matches!(
            response.1,
            GlobalResponse::DescriptorsReplaced(()) | GlobalResponse::ThreadExited
        ),
        "unexpected response to DescriptorsReplaced: {:?}",
        response.1
    );
}

/// Discard `signals` from `detpid`'s pending real-time timer expirations, as
/// Linux flushes a pending signal whose disposition becomes `SIG_IGN`.
pub(crate) async fn discard_pending_real_time_expirations<G, T>(
    guest: &mut G,
    detpid: DetPid,
    signals: u64,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(
        guest,
        GlobalRequest::DiscardPendingRealTimeExpirations(detpid, signals),
    )
    .await;
    assert!(
        matches!(
            response.1,
            GlobalResponse::DiscardPendingRealTimeExpirations(()) | GlobalResponse::ThreadExited
        ),
        "unexpected response to DiscardPendingRealTimeExpirations: {:?}",
        response.1
    );
}

/// Records, in the calling thread's turn, that it armed a host-timed source of
/// `signals` (a kernel sigset) reaching the processes in `scope`, before the
/// arming call reaches the kernel. Returns the signals the caller's gated waits
/// now hold (`Scheduler::record_host_timed_signals`).
pub async fn record_host_timed_signals<G, T>(
    guest: &mut G,
    scope: HostTimedSignalScope,
    signals: u64,
) -> u64
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let response = send_and_update_time(
        guest,
        GlobalRequest::RecordHostTimedSignals(dettid, scope, signals),
    )
    .await;
    match response.1 {
        GlobalResponse::HostTimedSignals(signals) => signals,
        _ => unreachable!(),
    }
}

/// The signals (a kernel sigset) that no gated wait of the calling thread ends
/// for, because a host-timed source a guest armed can post them to its process
/// (`Scheduler::host_timed_signals`). Read it in the waiting thread's turn.
pub async fn host_timed_signals<G, T>(guest: &mut G) -> u64
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let response = send_and_update_time(guest, GlobalRequest::HostTimedSignals(dettid)).await;
    match response.1 {
        GlobalResponse::HostTimedSignals(signals) => signals,
        _ => unreachable!(),
    }
}

/// The calling thread's host-timed signals (`host_timed_signals`) and the
/// `SIGCHLD` a guest sent it that the schedule recorded in its private queue,
/// in one round trip (`Scheduler::host_timed_and_private_sigchld`). Read it in
/// the waiting thread's turn, before any injection of that turn.
pub(crate) async fn host_timed_and_private_sigchld<G, T>(guest: &mut G) -> (u64, u64)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let response =
        send_and_update_time(guest, GlobalRequest::HostTimedAndPrivateSigchld(dettid)).await;
    match response.1 {
        GlobalResponse::HostTimedAndPrivateSigchld(host_timed, private) => (host_timed, private),
        _ => unreachable!(),
    }
}

/// The scheduler's answer to a child-exit `SIGCHLD` control message from the
/// calling thread (`Scheduler::child_exit_sigchld_control`). It carries no
/// logical time either way.
pub(crate) async fn child_exit_sigchld<G, T>(
    guest: &mut G,
    control: crate::scheduler::child_exit_sigchld::ChildExitSigchldControl,
) -> crate::scheduler::child_exit_sigchld::ChildExitSigchldAnswer
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ChildExitSigchld(control)).await;
    match response.1 {
        GlobalResponse::ChildExitSigchld(answer) => answer,
        _ => unreachable!(),
    }
}

/// The scheduler's answer to a discarded-at-send record message from the
/// calling thread (`Scheduler::discarded_signal_control`). It carries no
/// logical time either way.
pub(crate) async fn discarded_signal<G, T>(
    guest: &mut G,
    control: crate::scheduler::discarded_at_send::DiscardedSignalControl,
) -> crate::scheduler::discarded_at_send::DiscardedSignalAnswer
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::DiscardedSignal(control)).await;
    match response.1 {
        GlobalResponse::DiscardedSignal(answer) => answer,
        _ => unreachable!(),
    }
}

/// Whether the scheduler's SIGALRM ledger refuses `control` for the calling
/// thread (`Scheduler::sigalrm_control`). Asked in the thread's own turn; it
/// carries no logical time either way.
pub async fn sigalrm_refuses<G, T>(guest: &mut G, control: SigalrmControl) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::Sigalrm(control)).await;
    match response.1 {
        GlobalResponse::Sigalrm(refused) => refused,
        _ => unreachable!(),
    }
}

/// EPERM when the scheduler's SIGALRM ledger refuses `control`
/// ([`sigalrm_refuses`]).
pub async fn refuse_sigalrm<G, T>(
    guest: &mut G,
    control: SigalrmControl,
) -> Result<(), nix::errno::Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if sigalrm_refuses(guest, control).await {
        Err(nix::errno::Errno::EPERM)
    } else {
        Ok(())
    }
}

/// Return scheduler-owned lifecycle state for an exact child-process wait.
pub async fn exact_child_wait_state<G, T>(guest: &mut G, child: DetPid) -> ExactChildWaitState
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let response =
        send_and_update_time(guest, GlobalRequest::ExactChildWaitState(parent, child)).await;
    match response.1 {
        GlobalResponse::ExactChildWaitState(state) => state,
        _ => unreachable!(),
    }
}

/// Wait without requesting a scheduler turn for a backend's physical-exit report.
pub async fn await_exact_child_physical_exit<G, T>(
    guest: &mut G,
    child: DetPid,
) -> ExactChildWaitState
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut state = exact_child_wait_state(guest, child).await;
    if matches!(
        state,
        ExactChildWaitState::PhysicalExitPending | ExactChildWaitState::PhysicallyExited
    ) {
        let dettid = guest.thread_state().dettid;
        let mut resources = Resources::new(dettid);
        resources.insert(ResourceID::WaitPhysicalChild(child), Permission::R);
        resources.fyi("wait-child-physical-exit");
        let _ = resource_request(guest, resources).await;
        state = exact_child_wait_state(guest, child).await;
    }
    state
}

/// Park until an exact or any-child process wait has a logical exit to reap.
pub async fn wait_for_child_lifecycle<G, T>(guest: &mut G, spec: ChildWaitSpec) -> ResumeStatus
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let mut resources = Resources::new(dettid);
    resources.insert(ResourceID::WaitChild { parent, spec }, Permission::R);
    resources.fyi("wait-child-lifecycle");
    resource_request(guest, resources).await
}

pub async fn ready_child_wait<G, T>(guest: &mut G, spec: ChildWaitSpec) -> (Option<DetPid>, bool)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let response = send_and_update_time(guest, GlobalRequest::ReadyChildWait(parent, spec)).await;
    match response.1 {
        GlobalResponse::ReadyChildWait(snapshot) => snapshot,
        _ => unreachable!(),
    }
}

pub async fn process_group<G, T>(guest: &mut G, process: DetPid) -> Option<DetPid>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ProcessGroup(process)).await;
    match response.1 {
        GlobalResponse::ProcessGroup(group) => group,
        _ => unreachable!(),
    }
}

/// Linux's answer to `setsid()` by `process`, from Detcore's model
/// (`Scheduler::setsid_decision`).
pub async fn setsid_decision<G, T>(guest: &mut G, process: DetPid) -> Result<(), reverie::Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::SetsidDecision(process)).await;
    match response.1 {
        GlobalResponse::SetsidDecision(decision) => decision.map_err(reverie::Errno::new),
        _ => unreachable!(),
    }
}

/// Linux's answer to `setpgid(pid, pgid)` by `caller`, from Detcore's model
/// (`Scheduler::setpgid_decision`): the process to move and its group.
pub async fn setpgid_decision<G, T>(
    guest: &mut G,
    caller: DetPid,
    pid: i32,
    pgid: i32,
) -> Result<(DetPid, DetPid), reverie::Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response =
        send_and_update_time(guest, GlobalRequest::SetpgidDecision(caller, pid, pgid)).await;
    match response.1 {
        GlobalResponse::SetpgidDecision(decision) => decision.map_err(reverie::Errno::new),
        _ => unreachable!(),
    }
}

/// The process group (`session` false) or session (`session` true) of the
/// task with id `pid` in Detcore's model, as `getpgid` and `getsid` answer it
/// (`Scheduler::group_or_session_of`); `None` where Linux has no such task.
pub async fn group_or_session_of<G, T>(guest: &mut G, pid: DetPid, session: bool) -> Option<DetPid>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::GroupOrSession(pid, session)).await;
    match response.1 {
        GlobalResponse::GroupOrSession(answer) => answer,
        _ => unreachable!(),
    }
}

/// The image guest process `process` executes, as Detcore recorded it at its
/// exec (`crate::exec_record`); `None` without a record, and for a process
/// that has exited.
pub(crate) async fn process_image<G, T>(
    guest: &mut G,
    process: DetPid,
) -> Option<crate::exec_record::ExecRecord>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(guest, GlobalRequest::ProcessImage(process))
        .await
        .1
    {
        GlobalResponse::ProcessImage(image) => image.map(|image| *image),
        _ => unreachable!(),
    }
}

pub async fn set_process_group<G, T>(guest: &mut G, process: DetPid, group: DetPid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response =
        send_and_update_time(guest, GlobalRequest::SetProcessGroup(process, group)).await;
    match response.1 {
        GlobalResponse::SetProcessGroup(updated) => updated,
        _ => unreachable!(),
    }
}

/// The process Detcore registered `dettid` in (`ThreadTree::process_of`), in
/// the calling thread's turn.
pub async fn registered_process_of<G, T>(guest: &mut G, dettid: DetTid) -> Option<DetPid>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::RegisteredProcessOf(dettid)).await;
    match response.1 {
        GlobalResponse::RegisteredProcessOf(process) => process,
        _ => unreachable!(),
    }
}

/// Reserve the victims of the SIGKILL the calling thread is about to send to
/// `target`'s process (`scheduler::sigkill_retirement`). `None` keeps the
/// ordinary path.
pub async fn reserve_sigkill<G, T>(
    guest: &mut G,
    target: DetTid,
) -> Option<crate::scheduler::sigkill_retirement::SigkillReservation>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // Backends that cannot retire at the kill turn send no extra RPC.
    if !crate::scheduler::sigkill_retirement::retires_sigkill_at_the_kill_turn(
        &guest.config().backend,
    ) {
        return None;
    }
    let sender = guest.thread_state().dettid;
    // Read before the send, while the target certainly exists.
    let uid =
        crate::syscalls::real_uid(target.as_raw()).unwrap_or_else(|| unsafe { libc::getuid() });
    let response =
        send_and_update_time(guest, GlobalRequest::ReserveSigkill(sender, target, uid)).await;
    match response.1 {
        GlobalResponse::ReserveSigkill(token) => token,
        _ => unreachable!(),
    }
}

/// Mirror a successful `prctl(PR_SET_CHILD_SUBREAPER, on)` by `process` in
/// the scheduler's thread tree, in the calling thread's turn.
pub async fn set_child_subreaper<G, T>(guest: &mut G, process: DetPid, on: bool)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::SetChildSubreaper(process, on)).await;
    match response.1 {
        GlobalResponse::SetChildSubreaper(()) => {}
        _ => unreachable!(),
    }
}

/// What the `sigev_notify_thread_id` `named` of a `timer_create` by the
/// calling thread names (`Scheduler::posix_timer_thread`).
pub async fn posix_timer_thread<G, T>(guest: &mut G, named: i32) -> PosixTimerThread
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let process = guest.thread_state().detpid.expect("detpid unset");
    let response =
        send_and_update_time(guest, GlobalRequest::PosixTimerThread(process, named)).await;
    match response.1 {
        GlobalResponse::PosixTimerThread(answer) => answer,
        _ => unreachable!(),
    }
}

/// A disposition change in the calling thread's process flushed `signo` from
/// every thread (`Scheduler::forget_thread_timer_copies`).
pub async fn forget_thread_timer_copies<G, T>(guest: &mut G, signo: i32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let process = guest.thread_state().detpid.expect("detpid unset");
    let response = send_and_update_time(
        guest,
        GlobalRequest::ForgetThreadTimerCopies(process, signo),
    )
    .await;
    match response.1 {
        GlobalResponse::ForgetThreadTimerCopies(()) => {}
        _ => unreachable!(),
    }
}

/// The POSIX timer copies the calling thread's successful exec flushes
/// (`Scheduler::take_exec_timer_flush`). Asked in the exec's turn, only in a
/// run that has recorded a timer copy or kept a real-time expiration pending.
pub async fn exec_timer_flush<G, T>(
    guest: &mut G,
) -> Result<crate::scheduler::ExecTimerFlush, String>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let process = guest.thread_state().detpid.expect("detpid unset");
    let response = send_and_update_time(guest, GlobalRequest::ExecTimerFlush(process)).await;
    match response.1 {
        GlobalResponse::ExecTimerFlush(answer) => answer,
        _ => unreachable!(),
    }
}

/// Retire the calling thread-group leader, which has made its last request
/// before completing its own `exit`, if its group lives on
/// (`Scheduler::retire_exited_leader`). Returns whether it was retired.
pub async fn retire_exited_leader<G, T>(guest: &mut G) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::RetireExitedLeader).await;
    match response.1 {
        GlobalResponse::RetireExitedLeader(retired) => retired,
        _ => unreachable!(),
    }
}

/// What the positive pid `pid`, named by the calling thread in its nested PID
/// namespace, names (`PidNamespaces::resolve_for`). Asked only by a thread
/// below the container's namespace (`ThreadState::nested_pid_namespace`).
pub async fn guest_pid<G, T>(
    guest: &mut G,
    pid: i32,
) -> crate::scheduler::pid_namespaces::GuestPidAnswer
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let caller = guest.thread_state().dettid;
    let response = send_and_update_time(guest, GlobalRequest::GuestPid(caller, pid)).await;
    match response.1 {
        GlobalResponse::GuestPid(answer) => answer,
        _ => unreachable!(),
    }
}

/// The processes `/proc` lists and, for `process`, the threads its `task`
/// directory lists, as a thread in the container's PID namespace numbers
/// them (`Scheduler::procfs_listing`).
pub async fn procfs_listing<G, T>(
    guest: &mut G,
    process: Option<DetPid>,
) -> crate::procfs::listing::ListingFacts
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ProcfsListing(process)).await;
    match response.1 {
        GlobalResponse::ProcfsListing(facts) => facts,
        _ => unreachable!(),
    }
}

/// What Detcore's process table says of the task a per-process `/proc` file
/// names (`Scheduler::procfs_task`), with the incarnation of its numbers.
pub async fn procfs_task<G, T>(
    guest: &mut G,
    task: crate::procfs::BoundTask,
) -> crate::procfs::listing::ProcfsTaskAnswer
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let request = GlobalRequest::ProcfsTask(
        DetTid::from_raw(task.process),
        task.thread.map(DetTid::from_raw),
    );
    let response = send_and_update_time(guest, request).await;
    match response.1 {
        GlobalResponse::ProcfsTask(answer) => answer,
        _ => unreachable!(),
    }
}

/// Record the auxiliary vector the sender's process's new image found on
/// its initial stack (`ProcessAuxvTable::record`).
pub(crate) async fn record_process_auxv<G, T>(guest: &mut G, bytes: Vec<u8>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::RecordProcessAuxv(bytes))
        .await
        .1;
    assert_eq!(response, GlobalResponse::RecordProcessAuxv(()));
}

/// The process whose image `/proc/<process>/auxv` describes (a thread's
/// number names its process), and that image's auxiliary vector
/// (`ProcessAuxvTable::current`).
pub(crate) async fn process_auxv<G, T>(guest: &mut G, process: DetPid) -> Option<ProcessAuxv>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(guest, GlobalRequest::ProcessAuxv(process))
        .await
        .1
    {
        GlobalResponse::ProcessAuxv(answer) => answer,
        _ => unreachable!(),
    }
}

/// Record that the sender's process changed its root with `chroot`
/// (`ProcessAuxvTable::mark_changed_root`).
pub(crate) async fn mark_changed_root<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::MarkChangedRoot)
        .await
        .1;
    assert_eq!(response, GlobalResponse::MarkChangedRoot(()));
}

/// Whether the process `task` names has changed its root
/// (`ProcessAuxvTable::root_changed`).
pub(crate) async fn root_changed<G, T>(guest: &mut G, task: DetPid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(guest, GlobalRequest::RootChanged(task))
        .await
        .1
    {
        GlobalResponse::RootChanged(changed) => changed,
        _ => unreachable!(),
    }
}

/// Record the OFD and flock locks `open_file` holds, as its fdinfo showed
/// them after a call through one of the sender's descriptors, and that
/// `slots` slots of the sender's table name it
/// (`procfs::locks::LockLedger::note_locks`), or why that fdinfo could not be
/// read (`procfs::locks::LockLedger::note_unknown`).
pub(crate) async fn note_description_locks<G, T>(
    guest: &mut G,
    open_file: OpenFileId,
    locks: Result<Vec<crate::procfs::locks::HeldLock>, String>,
    slots: u32,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(
        guest,
        GlobalRequest::DescriptionLocks(open_file, locks, slots),
    )
    .await
    .1;
    assert_eq!(response, GlobalResponse::DescriptionLocks(()));
}

/// Record that `remaining` slots of the sender's table still name
/// `open_file` (`procfs::locks::LockLedger::release`).
pub(crate) async fn note_table_released<G, T>(guest: &mut G, open_file: OpenFileId, remaining: u32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::TableReleased(open_file, remaining))
        .await
        .1;
    assert_eq!(response, GlobalResponse::TableReleased(()));
}

/// Record which descriptions whose locks Detcore read the sender's table
/// still names after its successful exec
/// (`procfs::locks::LockLedger::exec_kept`).
pub(crate) async fn note_table_exec<G, T>(guest: &mut G, surviving: Vec<(OpenFileId, u32)>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::TableExec(surviving))
        .await
        .1;
    assert_eq!(response, GlobalResponse::TableExec(()));
}

/// Record that a lock or unlock went through a descriptor whose description
/// the ledger may know under another identity, on the file with inode number
/// `inode` (`None`: unknown) (`procfs::locks::LockLedger::alias_inode`).
pub(crate) async fn note_aliased_inode<G, T>(guest: &mut G, inode: Option<String>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::AliasedInode(inode))
        .await
        .1;
    assert_eq!(response, GlobalResponse::AliasedInode(()));
}

/// Record that every later `/proc/locks` snapshot is refused, for `reason`
/// (`procfs::locks::LockLedger::refuse`).
pub(crate) async fn note_lock_ledger_refused<G, T>(guest: &mut G, reason: String)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::LockLedgerRefused(reason))
        .await
        .1;
    assert_eq!(response, GlobalResponse::LockLedgerRefused(()));
}

/// Which of the rows of a `/proc/locks` snapshot the guest owns, in row order
/// (`procfs::locks::guest_lock_rows`), or why no snapshot can be decided
/// (`procfs::locks::LockLedger::unknown`).
pub(crate) async fn guest_lock_rows<G, T>(
    guest: &mut G,
    rows: Vec<crate::procfs::locks::LockRowFacts>,
) -> Result<Vec<bool>, String>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(guest, GlobalRequest::GuestLockRows(rows))
        .await
        .1
    {
        GlobalResponse::GuestLockRows(kept) => kept,
        _ => unreachable!(),
    }
}

/// Report that the sender's process's new image's `/proc/<pid>/auxv` is left
/// to the kernel beneath for `why`, and learn whether it is the run's first
/// for that reason, which the caller warns of: the guest's tool prints where
/// the run's output shows it, which the global tool's log does not on DBT.
pub(crate) async fn auxv_from_kernel<G, T>(guest: &mut G, why: AuxvFromKernel) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(guest, GlobalRequest::AuxvFromKernel(why))
        .await
        .1
    {
        GlobalResponse::AuxvFromKernel(first) => first,
        _ => unreachable!(),
    }
}

/// Change or read the state Detcore keeps on the rendered per-process `/proc`
/// file numbered `inode` (`InodePool::rendered_file_state`).
pub async fn rendered_file_state<G, T>(
    guest: &mut G,
    inode: u64,
    change: crate::procfs::listing::RenderedFileChange,
) -> crate::procfs::listing::RenderedFileState
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response =
        send_and_update_time(guest, GlobalRequest::RenderedFileState(inode, change)).await;
    match response.1 {
        GlobalResponse::RenderedFileState(state) => state,
        _ => unreachable!(),
    }
}

/// The kernel's number for each guest process or thread number in `ids`
/// (`to_kernel`), or the guest's for each kernel number, as `/proc` spells
/// them (`Scheduler::procfs_ids`).
pub async fn procfs_ids<G, T>(guest: &mut G, to_kernel: bool, ids: Vec<i32>) -> Vec<Option<i32>>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ProcfsIds(to_kernel, ids)).await;
    match response.1 {
        GlobalResponse::ProcfsIds(ids) => ids,
        _ => unreachable!(),
    }
}

/// The pid the calling thread, in its nested PID namespace, sees for `task`
/// (`PidNamespaces::localize_for`): `Ok(None)` when `task` is not visible
/// there.
pub async fn local_pid<G, T>(guest: &mut G, task: DetTid) -> Result<Option<i32>, String>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let caller = guest.thread_state().dettid;
    let response = send_and_update_time(guest, GlobalRequest::LocalPid(caller, task)).await;
    match response.1 {
        GlobalResponse::LocalPid(answer) => answer,
        _ => unreachable!(),
    }
}

/// The process a process-directed signal naming `named` goes to: a live
/// non-leader thread's id names its thread group, as Linux's kill_pid_info
/// does (`Scheduler::signal_target_process`).
pub async fn signal_target_process<G, T>(guest: &mut G, named: DetPid) -> Option<DetPid>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::SignalTargetProcess(named)).await;
    match response.1 {
        GlobalResponse::SignalTargetProcess(process) => process,
        _ => unreachable!(),
    }
}

/// Mirror a successful `rt_sigaction(SIGCHLD, ...)` by `process` in the
/// scheduler's thread tree, in the calling thread's turn.
pub async fn set_sigchld_disposition<G, T>(
    guest: &mut G,
    process: DetPid,
    ignored: bool,
    nocldwait: bool,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(
        guest,
        GlobalRequest::SetSigchldDisposition(process, ignored, nocldwait),
    )
    .await;
    match response.1 {
        GlobalResponse::SetSigchldDisposition(()) => {}
        _ => unreachable!(),
    }
}

/// Reserve the victims of the `kill(-pgid, SIGKILL)` the calling thread is
/// about to send (`Scheduler::reserve_sigkill_group`). Every guest process has
/// the same credentials, so the members share the caller's real uid.
pub async fn reserve_sigkill_group<G, T>(
    guest: &mut G,
    pgid: DetPid,
) -> Option<crate::scheduler::sigkill_retirement::SigkillReservation>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // Backends that cannot retire at the kill turn send no extra RPC.
    if !crate::scheduler::sigkill_retirement::retires_sigkill_at_the_kill_turn(
        &guest.config().backend,
    ) {
        return None;
    }
    let sender = guest.thread_state().dettid;
    let uid = crate::syscalls::real_uid(guest.tid().as_raw())
        .unwrap_or_else(|| unsafe { libc::getuid() });
    let response =
        send_and_update_time(guest, GlobalRequest::ReserveSigkillGroup(sender, pgid, uid)).await;
    match response.1 {
        GlobalResponse::ReserveSigkill(token) => token,
        _ => unreachable!(),
    }
}

/// Commit (`sent`) or unwind a SIGKILL reservation at the sending boundary.
pub async fn commit_sigkill<G, T>(guest: &mut G, token: u64, sent: bool)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::CommitSigkill(token, sent)).await;
    match response.1 {
        GlobalResponse::CommitSigkill(()) => {}
        _ => unreachable!(),
    }
}

pub async fn create_session<G, T>(guest: &mut G, process: DetPid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::CreateSession(process)).await;
    match response.1 {
        GlobalResponse::CreateSession(updated) => updated,
        _ => unreachable!(),
    }
}

pub async fn consume_child_wait<G, T>(guest: &mut G, child: DetPid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let response =
        send_and_update_time(guest, GlobalRequest::ConsumeChildWait(parent, child)).await;
    match response.1 {
        GlobalResponse::ConsumeChildWait(consumed) => consumed,
        _ => unreachable!(),
    }
}

pub async fn resolve_kill_targets<G, T>(guest: &mut G, detpid: DetPid) -> Vec<DetTid>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    resolve_kill_targets_and_exit(guest, detpid).await.0
}

/// The live threads of `detpid`, and its exit state
/// (`Scheduler::exited_process_is_zombie`).
pub async fn resolve_kill_targets_and_exit<G, T>(
    guest: &mut G,
    detpid: DetPid,
) -> (Vec<DetTid>, Option<bool>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ResolveKillTargets(detpid)).await;
    match response.1 {
        GlobalResponse::ResolveKillTargets(targets, zombie) => (targets, zombie),
        _ => unreachable!(),
    }
}

/// Whether `tid`, a pid or tid the calling guest task named (in thread group
/// `tgid`, for the calls that name one), is one of Hermit's own tasks; see
/// [`hermit_task_named`].
pub async fn names_hermit_task<G, T>(guest: &mut G, tgid: Option<i32>, tid: i32) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let caller = guest.thread_state().dettid;
    let response =
        send_and_update_time(guest, GlobalRequest::NamesHermitTask(caller, tgid, tid)).await;
    match response.1 {
        GlobalResponse::NamesHermitTask(named) => named,
        _ => unreachable!(),
    }
}

/// Whether guest task `caller`, naming `tid` (and thread group `tgid`, for the
/// calls that name one), names one of Hermit's own tasks: a thread of the
/// process Detcore's global state runs in (<https://github.com/rrnewton/hermit/issues/4066>).
/// Under ptrace that is Hermit's, the container init, PID 1 of the guest's PID
/// namespace. Under DBT the global state runs in the root guest process, whose
/// guest ids are virtual and are not host tasks descended from it, so the
/// answer there is always no, as for KVM below.
///
/// Hermit's tasks are identified directly, as its own thread group, not as
/// "not a registered guest task": a guest task Detcore has not registered yet
/// (a newborn, before its parent's `clone` returns) is not Hermit's. The
/// thread-group test is `tgkill(hermit, tid, 0)`, which sends nothing.
///
/// Only a caller that is a host process descended from Hermit, in Hermit's
/// own PID namespace, can name Hermit's tasks:
/// - a caller in a nested PID namespace names tasks by that namespace's ids,
///   where Hermit's tasks are not visible;
/// - a backend whose guest tasks are not host tasks (KVM, whose guest ids may
///   equal the ids of Hermit's own threads) never reaches Hermit by a pid;
/// - a caller that is one of Hermit's threads, or whose `/proc` entry cannot
///   be read, is not such a host task: a guest task in a system call is alive.
pub fn hermit_task_named(caller: DetTid, tgid: Option<i32>, tid: i32) -> bool {
    use std::os::unix::fs::MetadataExt;
    let hermit = std::process::id() as i32;
    if tid <= 0 || tgid.is_some_and(|tgid| tgid != hermit) {
        return false;
    }
    // SAFETY: signal 0 to a thread of this process only checks it exists.
    let in_hermits_group =
        tid == hermit || unsafe { libc::syscall(libc::SYS_tgkill, hermit, tid, 0) } == 0;
    if !in_hermits_group || !descends_from(caller.as_raw(), hermit) {
        return false;
    }
    let namespace = |path: String| fs::metadata(path).map(|meta| (meta.dev(), meta.ino()));
    match (
        namespace(format!("/proc/{}/ns/pid", caller.as_raw())),
        namespace("/proc/self/ns/pid".to_owned()),
    ) {
        (Ok(callers), Ok(hermits)) => callers == hermits,
        _ => false,
    }
}

/// Whether host task `task`'s process is a descendant of process `ancestor`,
/// following parent pids in `/proc` (field 4 of `stat`, the process's parent
/// for any of its threads). A task that cannot be read is not.
fn descends_from(task: i32, ancestor: i32) -> bool {
    let parent = |pid: i32| -> Option<i32> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name may hold spaces and parentheses: fields resume
        // after the last ')', as state, ppid.
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let mut pid = task;
    // Bounded by the depth of any real process tree; a reparenting race
    // cannot make it loop.
    for _ in 0..4096 {
        match parent(pid) {
            Some(next) if next == ancestor => return true,
            Some(next) if next > 0 => pid = next,
            _ => return false,
        }
    }
    false
}

/// Tell the scheduler that a SIGKILL it is not told about one victim at a time
/// succeeded: a process-group kill (`Some(group)`), or a pidfd with no recorded
/// process (`None`). See `Scheduler::record_unresolved_sigkill`.
pub async fn notify_unresolved_sigkill<G, T>(guest: &mut G, group: Option<DetPid>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::NotifyUnresolvedSigkill(group)).await;
    match response.1 {
        GlobalResponse::NotifyUnresolvedSigkill(()) => {}
        _ => unreachable!(),
    }
}

/// Tell the scheduler that a successful kill(2) left a physical
/// signal pending for a sole, known target.
/// The `nix` signal a timer will raise.
///
/// Timer registration still models a named signal: `setitimer`/`timer_create`
/// reach here with one, and the timer wheel stores it. Widening that is a
/// separate axis from the notification defect this change fixes, so the
/// conversion is asserted here rather than silently widened — a realtime timer
/// signal would be a NEW capability, not a regression of an existing one.
fn alarm_signal(sig: SigWrapper) -> Signal {
    sig.signal().unwrap_or_else(|| {
        panic!(
            "timer registration received unnameable signal {}",
            sig.raw()
        )
    })
}

/// Ask that this thread's next request, its group `Exit`, be granted before
/// any other thread runs (`GlobalRequest::HoldTurnForGroupExit`). Returns
/// whether the scheduler agreed, which it does only for the thread holding the
/// serial grant.
pub async fn hold_turn_for_group_exit<G, T>(guest: &mut G, dettid: DetTid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::HoldTurnForGroupExit(dettid)).await;
    match response.1 {
        GlobalResponse::HoldTurnForGroupExit(held) => held,
        _ => unreachable!(),
    }
}

pub async fn notify_signal_pending<G, T>(
    guest: &mut G,
    dettid: DetTid,
    signal: SigWrapper,
    target_process: Option<DetPid>,
    scope: SignalScope,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(
        guest,
        GlobalRequest::NotifySignalPending(dettid, signal, target_process, scope),
    )
    .await;
    match response.1 {
        GlobalResponse::NotifySignalPending(()) => {}
        _ => unreachable!(),
    }
}

/// Signal an unrecoverable error that exits the entire container.
/// Such exits are not determinizable (see "quasi-determinism").
///
/// ⚠️ `status` IS NOT A DEFAULTABLE PARAMETER, AND THAT IS THE POINT. This
/// function has four callers and they do not all mean the same thing: three are
/// fail-closed policy refusals (`HERMIT_POLICY_REFUSAL_EXIT`) and one is an
/// operator interrupt (`HERMIT_SIGINT_DEATH_EXIT`). While the status was baked in
/// here, every caller inherited whatever the last edit chose, so the SIGINT path
/// reported "hermit refused this run" for a run hermit did not refuse. Making it
/// an argument turns the meaning into a visible claim at each call site, which is
/// the same rule `scripts/check-exit-status-class.rs` enforces on the test side:
/// a status you can read is worth less than a status that says which channel
/// produced it.
///
/// Adding a caller means choosing; there is deliberately no default to fall into.
pub async fn unrecoverable_shutdown<G, T>(guest: &G, status: i32) -> !
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // The process ends here (`crate::guest_log`).
    crate::guest_log::flush_unsent();
    if cfg!(debug_assertions) {
        let mytime = guest.thread_state().thread_logical_time.clone();
        let mm = guest.thread_state().mm_id;
        // TODO: void_send_rpc
        let _ = guest
            .send_rpc((mytime, mm, GlobalRequest::UnrecoverableShutdown))
            .await;
    }

    // In this scenario a backtrace doesn't really help us.
    //
    // ⚠️ THE STATUS IS THE ONLY THING THAT CROSSES THIS BOUNDARY, SO IT HAS TO
    // CARRY THE MEANING. This is a DELIBERATE, policy-driven shutdown: the run
    // hit an operation it cannot service under a fail-closed configuration and
    // hermit chose to stop. The parent sees only a container child that exited,
    // and its classifier treats an exit it cannot account for as unchosen — so
    // exiting 1 here made `classify_container_result` report
    // `class=container-child-exit` and 125, i.e. "hermit broke", for a shutdown
    // that worked exactly as designed. `HERMIT_POLICY_REFUSAL_EXIT` is the
    // agreed value for "hermit refused"; see its doc for why it is not 1.
    //
    // ⚠️ AND WHICH VALUE IS THE CALLER'S TO SAY, NOT THIS FUNCTION'S. Baking one
    // in here is what made an operator's Ctrl-C report as a policy refusal: the
    // condition differs per caller and only the caller knows it.
    //
    // The RPC above is `cfg!(debug_assertions)`-only, so it cannot be the
    // signal — in a release build the parent would learn nothing.
    std::process::exit(status);
}

#[cfg(test)]
mod tests {
    /// `response` as `receive_rpc` returns it after a turn was granted
    /// (`GlobalResponse::Granted`).
    fn granted(response: super::GlobalResponse) -> super::GlobalResponse {
        super::GlobalResponse::Granted(Box::new(response))
    }

    /// A process reads its current image's vector; a forked child its
    /// creator's until it execs; an exec replaces it; a forgotten process
    /// (gone, or an exec with no recorded vector) has none.
    #[test]
    fn a_changed_root_follows_fork_and_goes_at_exit() {
        use super::ProcessAuxvTable;
        use crate::types::DetPid;
        let (parent, child, other) = (
            DetPid::from_raw(3),
            DetPid::from_raw(4),
            DetPid::from_raw(5),
        );
        let mut table = ProcessAuxvTable::default();
        table.clone_root(DetPid::from_raw(1), parent, false);
        assert!(!table.root_changed(parent));
        table.mark_changed_root(parent);
        assert!(table.root_changed(parent));
        // A new task without CLONE_FS starts with a copy of its creator's
        // root.
        table.clone_root(parent, child, false);
        assert!(table.root_changed(child));
        table.clone_root(other, child, false);
        assert!(!table.root_changed(child), "a reused number starts afresh");
        // An exec keeps the root, so only an exit forgets it.
        table.forget(parent);
        assert!(table.root_changed(parent));
        table.forget_process(parent);
        assert!(!table.root_changed(parent));
    }

    /// Tasks created with CLONE_FS share one root, as Linux's `fs_struct`:
    /// a chroot by the child changes its creator's root too, a task created
    /// without CLONE_FS keeps its own, and a non-leader thread's exec takes
    /// its group to the process's number.
    #[test]
    fn a_root_is_shared_by_a_filesystem_group() {
        use super::ProcessAuxvTable;
        use crate::types::DetPid;
        let pid = DetPid::from_raw;
        let mut table = ProcessAuxvTable::default();
        table.clone_root(pid(1), pid(3), false);
        // A CLONE_FS|SIGCHLD child, and an ordinary fork as an observer.
        table.clone_root(pid(3), pid(4), true);
        table.clone_root(pid(3), pid(5), false);
        table.mark_changed_root(pid(4));
        assert!(
            table.root_changed(pid(3)),
            "the child's chroot is its creator's"
        );
        assert!(
            !table.root_changed(pid(5)),
            "a copied root stays where it was"
        );
        // A thread created without CLONE_FS has a root of its own.
        table.clone_root(pid(5), pid(6), false);
        table.mark_changed_root(pid(6));
        assert!(!table.root_changed(pid(5)));
        // Thread 6's exec gives it process 5's number, and its root.
        table.transfer_root(pid(6), pid(5));
        assert!(table.root_changed(pid(5)));
        // The group outlives a member that exits.
        table.forget_process(pid(4));
        assert!(table.root_changed(pid(3)));
    }

    #[test]
    fn process_auxv_follows_exec_and_fork() {
        use super::ProcessAuxv;
        use super::ProcessAuxvTable;
        use crate::types::DetPid;
        let (parent, child) = (DetPid::from_raw(3), DetPid::from_raw(4));
        let mut table = ProcessAuxvTable::default();
        assert_eq!(table.current(parent), None);
        table.record(parent, vec![1; 16]);
        table.inherit(parent, child);
        assert_eq!(
            table.current(child),
            Some(ProcessAuxv {
                process: child,
                bytes: vec![1; 16]
            })
        );
        table.record(child, vec![2; 32]);
        assert_eq!(
            table.current(child).map(|entry| entry.bytes),
            Some(vec![2; 32])
        );
        assert_eq!(
            table.current(parent).map(|entry| entry.bytes),
            Some(vec![1; 16])
        );
        table.forget(parent);
        assert_eq!(table.current(parent), None);
        // A creator with no vector gives its child none.
        table.inherit(parent, child);
        assert_eq!(table.current(child), None);
        assert_eq!(table.len(), 0);
    }

    /// Hermit's tasks are exactly its own thread group, named from its own PID
    /// namespace (<https://github.com/rrnewton/hermit/issues/4066>). The test
    /// process stands in for Hermit.
    #[test]
    fn only_hermits_own_thread_group_is_hermits() {
        use super::hermit_task_named;
        use crate::types::DetTid;
        let me = std::process::id() as i32;
        // A child process stands in for a guest task.
        let mut guest = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let caller = DetTid::from_raw(guest.id() as i32);
        assert!(hermit_task_named(caller, None, me));
        assert!(hermit_task_named(caller, Some(me), me));
        // One of Hermit's own threads is not a guest task, and names nothing
        // of Hermit's (KVM guest ids can equal them).
        // SAFETY: gettid has no preconditions.
        let own_thread = DetTid::from_raw(unsafe { libc::gettid() });
        assert!(!hermit_task_named(own_thread, None, me));
        let (send, receive) = std::sync::mpsc::channel();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            // SAFETY: as above.
            send.send(unsafe { libc::gettid() }).unwrap();
            let _ = stopped.recv();
        });
        let other_thread = receive.recv().unwrap();
        assert!(hermit_task_named(caller, None, other_thread));
        assert!(hermit_task_named(caller, Some(me), other_thread));
        let guest_pid = guest.id() as i32;
        assert!(!hermit_task_named(caller, None, guest_pid));
        assert!(!hermit_task_named(caller, Some(guest_pid), guest_pid));
        assert!(!hermit_task_named(caller, Some(guest_pid), other_thread));
        assert!(!hermit_task_named(caller, None, 0));
        assert!(!hermit_task_named(caller, None, -1));
        // A caller that is not a host task descended from Hermit names nothing
        // of Hermit's: one that cannot be read, or Hermit's own parent.
        assert!(!hermit_task_named(DetTid::from_raw(i32::MAX), None, me));
        // SAFETY: getppid has no preconditions.
        let parent = DetTid::from_raw(unsafe { libc::getppid() });
        assert!(!hermit_task_named(parent, None, me));
        guest.kill().unwrap();
        guest.wait().unwrap();
        stop.send(()).unwrap();
        thread.join().unwrap();
    }

    /// The DBT SIGKILL fence waits on a process pidfd: a live victim keeps it
    /// waiting to the valve, and a dead one, even unreaped, releases it
    /// (https://github.com/rrnewton/hermit/issues/4362).
    #[test]
    fn the_dbt_sigkill_fence_waits_until_the_victim_process_is_gone() {
        use super::open_process_pidfd;
        use super::wait_for_process_exit;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut victim = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = victim.id() as i32;
        let live = open_process_pidfd(pid).expect("a live child has a pidfd");
        let waited = runtime.block_on(wait_for_process_exit(
            live,
            std::time::Duration::from_millis(50),
        ));
        assert!(waited.unwrap_err().contains("not gone within"));
        let dying = open_process_pidfd(pid).expect("a live child has a pidfd");
        victim.kill().unwrap();
        assert_eq!(
            runtime.block_on(wait_for_process_exit(
                dying,
                std::time::Duration::from_secs(30)
            )),
            Ok(())
        );
        victim.wait().unwrap();
        assert!(open_process_pidfd(pid).is_err());
    }

    /// The DBT SIGKILL fence never leaves the sender unfenced while the victim
    /// may still be running: an open failure refuses, and an ESRCH counts as
    /// the victim gone only once the thread the kill named has exited, as its
    /// registration pidfd shows (Codex design review of
    /// https://github.com/rrnewton/hermit/issues/4362 part 2, P2-3).
    #[test]
    fn the_dbt_sigkill_fence_refuses_rather_than_leave_the_sender_unfenced() {
        use super::sigkill_fence_for;
        use crate::scheduler::Scheduler;
        use crate::types::DetPid;
        use crate::types::DetTid;
        use crate::types::MmId;
        let config = crate::config::Config::default()
            .with_backend(|backend| *backend = reverie::BackendCapabilities::DBT);
        let mut scheduler = Scheduler::new(&config);
        let me = std::process::id() as i32;
        let errno = |code| move |_: i32| Err(std::io::Error::from_raw_os_error(code));
        // A live registered thread: EMFILE and an unconfirmed ESRCH refuse.
        let live = DetTid::from_raw(41);
        let live_process = DetPid::from_raw(41);
        // SAFETY: gettid has no preconditions.
        let my_tid = unsafe { libc::gettid() };
        scheduler
            .register_physical_thread(live, MmId::initial(live_process), me, my_tid)
            .unwrap();
        let refusal = sigkill_fence_for(&scheduler, live, live_process, true, errno(libc::EMFILE))
            .unwrap_err();
        assert_eq!(refusal.process, live_process);
        assert!(refusal.reason.contains("pidfd_open"), "{}", refusal.reason);
        assert!(
            sigkill_fence_for(&scheduler, live, live_process, true, errno(libc::ESRCH)).is_err()
        );
        assert!(
            sigkill_fence_for(&scheduler, live, live_process, true, errno(libc::EINVAL)).is_err()
        );
        assert!(
            sigkill_fence_for(
                &scheduler,
                live,
                live_process,
                true,
                super::open_process_pidfd
            )
            .unwrap()
            .is_some()
        );
        // A registered thread that has exited: an ESRCH means the victim is gone.
        let gone = DetTid::from_raw(43);
        let gone_process = DetPid::from_raw(43);
        let (send, receive) = std::sync::mpsc::channel();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            // SAFETY: as above.
            send.send(unsafe { libc::gettid() }).unwrap();
            let _ = stopped.recv();
        });
        let gone_tid = receive.recv().unwrap();
        scheduler
            .register_physical_thread(gone, MmId::initial(gone_process), me, gone_tid)
            .unwrap();
        stop.send(()).unwrap();
        thread.join().unwrap();
        // pthread_join returns at the CLONE_CHILD_CLEARTID wake, before the
        // kernel marks the thread exited, so its pidfd can turn readable a
        // little later (Claude review of
        // https://github.com/rrnewton/hermit/pull/4423, F1).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while scheduler.registered_thread_has_exited(gone) != Some(true) {
            assert!(
                std::time::Instant::now() < deadline,
                "the joined thread's pidfd never reported its exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            sigkill_fence_for(&scheduler, gone, gone_process, true, errno(libc::EINVAL))
                .unwrap()
                .is_none()
        );
        assert!(
            sigkill_fence_for(&scheduler, gone, gone_process, true, errno(libc::ESRCH))
                .unwrap()
                .is_none()
        );
        assert!(
            sigkill_fence_for(&scheduler, gone, gone_process, true, errno(libc::EMFILE)).is_err()
        );
        // A named thread with no registration refuses, even when the victim
        // seems to have no live thread left: that can be its threads' exits
        // racing this notice after the kill reached the kernel (Claude
        // re-check of https://github.com/rrnewton/hermit/pull/4423, N1).
        let unregistered = DetTid::from_raw(47);
        let empty = DetPid::from_raw(47);
        let refusal = sigkill_fence_for(&scheduler, unregistered, empty, true, errno(libc::EMFILE))
            .unwrap_err();
        assert!(
            refusal.reason.contains("has a registered host process"),
            "{}",
            refusal.reason
        );
        // A zombie whose exit the schedule granted needs no fence
        // (https://github.com/rrnewton/hermit/issues/4411).
        scheduler.grant_exit_for_test(empty);
        assert!(
            sigkill_fence_for(&scheduler, unregistered, empty, true, errno(libc::EMFILE))
                .unwrap()
                .is_none()
        );
        // A named leader that has exited while a sibling runs on: the fence
        // goes through the sibling's registration, and an ESRCH counts as gone
        // only once that sibling has exited
        // (https://github.com/rrnewton/hermit/issues/4411).
        let leader = DetTid::from_raw(51);
        let process = DetPid::from_raw(51);
        let sibling = DetTid::from_raw(52);
        scheduler.thread_tree.add_child(leader, leader, true);
        scheduler.thread_tree.add_child(leader, sibling, false);
        // SAFETY: as above.
        let my_tid = unsafe { libc::gettid() };
        scheduler
            .register_physical_thread(sibling, MmId::initial(process), me, my_tid)
            .unwrap();
        assert!(
            sigkill_fence_for(&scheduler, leader, process, true, super::open_process_pidfd)
                .unwrap()
                .is_some()
        );
        assert!(sigkill_fence_for(&scheduler, leader, process, true, errno(libc::ESRCH)).is_err());
        // A thread-directed SIGKILL (tgkill, tkill, rt_tgsigqueueinfo) to the
        // exited leader gets no fallback and refuses; Linux does not end the
        // group for it (Claude review of
        // https://github.com/rrnewton/hermit/pull/4439, R1). Nor does a
        // thread-directed kill to a zombie skip the fence.
        assert!(
            sigkill_fence_for(
                &scheduler,
                leader,
                process,
                false,
                super::open_process_pidfd
            )
            .is_err()
        );
        assert!(
            sigkill_fence_for(&scheduler, unregistered, empty, false, errno(libc::EMFILE)).is_err()
        );
        // An exited leader whose surviving sibling has since exited too: an
        // ESRCH counts as gone, confirmed through the sibling's own
        // registration pidfd, not the leader's (Claude review of 4439, R2).
        let late_leader = DetTid::from_raw(61);
        let late_process = DetPid::from_raw(61);
        let late_sibling = DetTid::from_raw(62);
        scheduler
            .thread_tree
            .add_child(late_leader, late_leader, true);
        scheduler
            .thread_tree
            .add_child(late_leader, late_sibling, false);
        let (send, receive) = std::sync::mpsc::channel();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            // SAFETY: as above.
            send.send(unsafe { libc::gettid() }).unwrap();
            let _ = stopped.recv();
        });
        let sibling_tid = receive.recv().unwrap();
        scheduler
            .register_physical_thread(late_sibling, MmId::initial(late_process), me, sibling_tid)
            .unwrap();
        stop.send(()).unwrap();
        thread.join().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while scheduler.registered_thread_has_exited(late_sibling) != Some(true) {
            assert!(
                std::time::Instant::now() < deadline,
                "the joined thread's pidfd never reported its exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            sigkill_fence_for(
                &scheduler,
                late_leader,
                late_process,
                true,
                errno(libc::ESRCH)
            )
            .unwrap()
            .is_none()
        );
    }

    /// DBT publishes READY_IMAGE between creating the scheduler future and
    /// polling it, so the announcement must already be logged when
    /// `run_external_scheduler` returns:
    /// <https://github.com/rrnewton/hermit/issues/3463>.
    #[test]
    fn external_scheduler_announces_before_its_future_is_polled() {
        #[derive(Clone, Default)]
        struct Capture(std::sync::Arc<Mutex<Vec<String>>>);
        struct Visitor(String);
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    use std::fmt::Write;
                    write!(self.0, "{value:?}").unwrap();
                }
            }
        }
        impl tracing::Subscriber for Capture {
            fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
                *metadata.level() == tracing::Level::INFO
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let mut visitor = Visitor(String::new());
                event.record(&mut visitor);
                self.0.lock().unwrap().push(visitor.0);
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::init_for_external_scheduler(&config);
        let captured = Capture::default();
        let scheduler = tracing::subscriber::with_default(captured.clone(), || {
            state.run_external_scheduler(std::sync::Arc::new(|_| {}))
        });
        let announcement = "[scheduler] daemon task starting up, waiting for guest thread start..";
        assert_eq!(*captured.0.lock().unwrap(), [announcement]);
        drop(scheduler);
    }

    #[test]
    fn schedule_event_host_markers_require_command_bootstrap_provenance() {
        let event = SchedEvent::branches(DetTid::from_raw(3), 223)
            .with_end_rip(std::num::NonZeroUsize::new(0x1234).unwrap())
            .with_time(LogicalTime::from_nanos(2230));
        let original = serde_json::to_string(&event).unwrap();
        let plain = format!(
            "{:?}",
            super::SchedEventForLog {
                event: &event,
                command_bootstrap: false
            }
        );
        assert_eq!(plain, format!("{event:?}"));
        let marked = format!(
            "{:?}",
            super::SchedEventForLog {
                event: &event,
                command_bootstrap: true
            }
        );
        assert_eq!(
            marked,
            "SchedEvent { dettid: DetPid(3), op: Branch, count: 223, start_rip: None, end_rip: Some(<hostaddr 0x1234>), end_time: Some(LogicalTime(2230)) }"
        );
        assert_eq!(serde_json::to_string(&event).unwrap(), original);
    }

    #[test]
    fn summary_preemption_views_keep_counts_full_report_and_single_flush() {
        let (_config, state, tid, _) = cancellation_test_state();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recording");
        let mut writer = crate::preemptions::PreemptionWriter::new(Some(path.clone()));
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_reprioritization(tid, LogicalTime::from_nanos(100), 10, DEFAULT_PRIORITY, 20);
        let mut scheduler = state.sched.lock().unwrap();
        scheduler.preemption_writer = Some(writer);
        let (summary, info_description) = scheduler
            .generate_partial_run_summary_for_log(Some(&path))
            .unwrap();
        assert!(scheduler.preemption_writer.is_none());
        let recorded = std::fs::read(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&recorded).unwrap();
        assert_eq!(
            parsed["per_thread"][tid.to_string()]["prio_changes"],
            serde_json::json!([[100, DEFAULT_PRIORITY]])
        );
        let count_line = "Record of 1 preemption and reprioritization events:\n";
        assert_eq!(info_description.as_deref(), Some(count_line));
        assert_eq!(
            summary.reprio_descrip.as_deref(),
            Some(format!("{count_line}  (Writing to file {path:?})\n").as_str())
        );
        let full = summary.to_string();
        let json = serde_json::to_vec(&summary).unwrap();
        let info = summary.info(info_description.as_deref()).to_string();
        assert!(info.contains(count_line));
        assert!(!info.contains("Writing to file"));
        assert!(full.contains(&format!("Writing to file {path:?}")));
        // Formatting and another empty summary do not flush the consumed writer again.
        let _ = scheduler.generate_partial_run_summary(None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), recorded);
        assert_eq!(summary.to_string(), full);
        assert_eq!(serde_json::to_vec(&summary).unwrap(), json);
    }

    #[test]
    fn run_summary_records_the_effective_shared_preemption_configuration() {
        // Config::default() is unprepared: launch installs the timer capability.
        // Specify that mechanism fact here independently of the maximum.
        for (config, expected_maximum, expected_timer) in [
            (
                Config::default().with_backend(|backend| {
                    backend.delivers_preemption_timer_events = true;
                }),
                Some(200_000_000),
                true,
            ),
            (
                Config {
                    max_timeslice: None,
                    ..Config::default()
                }
                .with_backend(|backend| {
                    backend.delivers_preemption_timer_events = true;
                }),
                None,
                false,
            ),
            (
                Config {
                    max_timeslice: std::num::NonZeroU64::new(12_340),
                    ..Config::default()
                }
                .with_backend(|backend| {
                    backend.delivers_preemption_timer_events = true;
                }),
                Some(12_340),
                true,
            ),
            (
                Config::default().with_backend(|backend| {
                    backend.delivers_preemption_timer_events = false;
                }),
                Some(200_000_000),
                false,
            ),
            (
                Config {
                    max_timeslice: None,
                    ..Config::default()
                }
                .with_backend(|backend| {
                    backend.delivers_preemption_timer_events = false;
                }),
                None,
                false,
            ),
        ] {
            let summary = GlobalState::for_tests(&config).into_run_summary().unwrap();
            assert_eq!(
                summary.preemption,
                Some(super::PreemptionSummary {
                    max_timeslice_ns: expected_maximum,
                    timer_enabled: Some(expected_timer),
                })
            );
        }
    }

    #[test]
    fn run_summary_info_keeps_semantics_and_debug_retains_bookkeeping() {
        #[derive(Clone)]
        struct Capture(std::sync::Arc<Mutex<Vec<(tracing::Level, String)>>>);
        struct Visitor(String);
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                write!(self.0, "{}={value:?};", field.name()).unwrap();
            }
        }
        impl tracing::Subscriber for Capture {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let mut visitor = Visitor(String::new());
                event.record(&mut visitor);
                self.0
                    .lock()
                    .unwrap()
                    .push((*event.metadata().level(), visitor.0));
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let summary = super::RunSummary {
            sched_turns: 4,
            schedevent_recorded: 11,
            schedevent_replayed: 11,
            schedevent_desynced: 2,
            desync_descrip: Some("two real desyncs\n".into()),
            num_processes: 1,
            num_threads: 1,
            threads_descrip: "[3]".into(),
            syscalls: Some(3),
            virttime_elapsed: 2_512_380,
            virttime_final: 2_512_380,
            timeslice_stats: TimesliceStats {
                count: 1,
                sum_ns: 512_380,
                min_ns: 512_380,
                max_ns: 512_380,
            },
            ..Default::default()
        };
        let description = "Record of 7 preemption and reprioritization events:\n";
        let captured = Capture(Default::default());
        tracing::subscriber::with_default(captured.clone(), || {
            super::log_run_summary(
                "report",
                &summary,
                Some(description),
                Some(std::path::Path::new("/host/recording")),
            );
        });
        let events = captured.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, tracing::Level::INFO);
        assert_eq!(events[1].0, tracing::Level::DEBUG);
        let info = &events[0].1;
        for text in [
            "1 group leaders of 1 thread(s)",
            "3 syscalls",
            "4 turns, recorded 11 events (2 desynced)",
            "two real desyncs",
            "Record of 7 preemption",
            "2_512_380ns",
            "min=512380ns max=512380ns mean=512380ns count=1",
        ] {
            assert!(info.contains(text), "missing semantic field {text}: {info}");
        }
        assert!(!info.contains("/host/recording"));
        assert!(!info.contains("replayed"));
        assert!(events[1].1.contains("replayed_events=11"));
        assert!(events[1].1.contains("/host/recording"));
        let baseline = summary.info(Some(description)).to_string();
        let mutations: [fn(&mut super::RunSummary); 8] = [
            |s| s.sched_turns += 1,
            |s| s.schedevent_recorded += 1,
            |s| s.schedevent_desynced += 1,
            |s| s.num_threads += 1,
            |s| s.syscalls = Some(4),
            |s| s.virttime_elapsed += 1,
            |s| s.timeslice_stats.count += 1,
            |s| s.threads_descrip.push_str(",4"),
        ];
        for mutate in mutations {
            let mut changed = summary.clone();
            mutate(&mut changed);
            assert_ne!(changed.info(Some(description)).to_string(), baseline);
        }
        assert_ne!(
            summary
                .info(Some(
                    "Record of 8 preemption and reprioritization events:\n"
                ))
                .to_string(),
            baseline
        );
    }

    mod backend_failure_tests;
    mod exec_identity_tests;
    mod exec_teardown_tests;
    mod stopping_refusal_tests;
    mod timerless_clock_tests;
    use std::collections::BTreeSet;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::sync::Mutex;
    use std::time::Duration;

    use nix::sys::signal::Signal;
    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Guest;
    use reverie::Tid;
    use reverie::syscalls::CloneFlags;
    use reverie::syscalls::Sysno;

    use super::FutexAction;
    use super::GlobalRequest;
    use super::GlobalResponse;
    use super::GlobalState;
    use super::MountIdPool;
    use super::PendingExecState;
    use super::ResumeStatus;
    use super::RpcIncarnation;
    use super::SchedulerRpcResult;
    use super::SigWrapper;
    use super::ThreadDeregistration;
    use super::TimesliceStats;
    use super::format_unsupported_syscall_warning;
    use crate::Detcore;
    use crate::config::Config;
    use crate::config::RunsPostFork;
    use crate::ivar::Ivar;
    use crate::preemptions::PreemptionRecord;
    use crate::resources::ExternalOpId;
    use crate::resources::Permission;
    use crate::resources::ResourceID;
    use crate::resources::Resources;
    use crate::scheduler::DEFAULT_PRIORITY;
    use crate::scheduler::SchedRequest;
    use crate::scheduler::SchedResponse;
    use crate::scheduler::SchedValue;
    use crate::scheduler::ThreadNextTurn;
    use crate::tool_local::ExecFdBlockingOverrides;
    use crate::tool_local::ExecFdState;
    use crate::types::DetPid;
    use crate::types::DetTid;
    use crate::types::DetTime;
    use crate::types::FutexID;
    use crate::types::LogicalTime;
    use crate::types::MmId;
    use crate::types::Op;
    use crate::types::SchedEvent;

    #[test]
    fn fdinfo_mount_ids_preserve_raw_equivalence_and_distinctness() {
        let mut pool = MountIdPool::from_config(&[10, 20, 30], true, &[]);
        // Numbered in first-observation order, not by row position: 20 is
        // the second captured row but the first ID the guest observes.
        assert_eq!(pool.determinize(20, None), Some(1));

        // Unlisted nsfs, anon_inodefs, and pidfs IDs are distinct even when
        // their descriptors have the same broad Detcore FdType.
        assert_eq!(pool.determinize(700, None), Some(2));
        assert_eq!(pool.determinize(701, None), Some(3));
        assert_eq!(pool.determinize(702, None), Some(4));
        assert_eq!(pool.determinize(701, None), Some(3));
        assert_eq!(pool.determinize(20, None), Some(1));
    }

    #[test]
    fn fdinfo_mount_ids_seed_from_low_level_snapshot_and_refuse_drift() {
        let mut pool = MountIdPool::from_config(&[], false, &[]);
        assert_eq!(pool.determinize(20, Some(&[10, 20, 30])), Some(1));
        assert_eq!(pool.determinize(700, Some(&[10, 20, 30])), Some(2));
        assert_eq!(pool.determinize(20, Some(&[10, 20, 30])), Some(1));
        assert_eq!(pool.determinize(20, Some(&[10, 99, 30])), None);
        assert_eq!(pool.determinize(20, Some(&[10, 20])), None);
        assert_eq!(pool.determinize(20, Some(&[10, 20, 30, 40])), None);
    }

    /// The compat/lsof divergence: a run's namespace starts as a copy of the
    /// host's, so unrelated host mounts (here 25, 26, 27, as one loop mount
    /// propagated to three peers) can change the row count between two runs
    /// of the same guest. The IDs the guest observes must not move.
    #[test]
    fn mount_ids_do_not_depend_on_rows_the_guest_never_observed() {
        let observe = |mut pool: MountIdPool, visible: &[u64]| {
            vec![
                pool.determinize(700, None),
                pool.determinize(701, None),
                pool.determinize(20, None),
            ]
            .into_iter()
            .chain(
                pool.resolve_mountinfo(visible, visible)
                    .unwrap()
                    .into_iter()
                    .map(Some),
            )
            .collect::<Vec<_>>()
        };
        let quiet = observe(
            MountIdPool::from_config(&[10, 20, 30], true, &[]),
            &[10, 30],
        );
        let churned = observe(
            MountIdPool::from_config(&[10, 20, 25, 26, 27, 30], true, &[]),
            &[10, 30],
        );
        assert_eq!(quiet, [Some(1), Some(2), Some(3), Some(4), Some(5)]);
        assert_eq!(quiet, churned);

        // Uncaptured runs validate each fdinfo read against a tracer-side
        // snapshot of the same namespace; the snapshot's size is irrelevant.
        let mut quiet = MountIdPool::from_config(&[], false, &[]);
        let mut churned = MountIdPool::from_config(&[], false, &[]);
        assert_eq!(quiet.determinize(700, Some(&[10, 20, 30])), Some(1));
        assert_eq!(
            churned.determinize(700, Some(&[10, 20, 25, 26, 27, 30])),
            Some(1)
        );
        assert_eq!(quiet.determinize(701, Some(&[10, 20, 30])), Some(2));
        assert_eq!(
            churned.determinize(701, Some(&[10, 20, 25, 26, 27, 30])),
            Some(2)
        );
    }

    #[test]
    fn mountinfo_views_keep_earlier_numbers_and_number_the_rest_in_row_order() {
        let mut pool = MountIdPool::from_config(&[10, 20, 30, 40], true, &[]);
        assert_eq!(pool.determinize(30, None), Some(1));
        assert_eq!(pool.determinize(700, None), Some(2));
        assert_eq!(
            pool.resolve_mountinfo(&[10, 20, 30], &[10, 20, 30]),
            Some(vec![3, 4, 1])
        );
        assert_eq!(
            pool.resolve_mountinfo(&[20, 40], &[20, 40]),
            Some(vec![4, 5])
        );
        // A refused view numbers nothing.
        assert_eq!(pool.resolve_mountinfo(&[40, 10], &[40, 10]), None);
        assert_eq!(pool.resolve_mountinfo(&[10, 99], &[10, 99]), None);
        assert_eq!(pool.determinize(701, None), Some(6));
        assert_eq!(pool.determinize(10, None), Some(3));
    }

    /// One chroot view: row 20 whose parent 10 is outside the view. The
    /// captured namespace may list 10 before 20 or after it, which the guest
    /// cannot see. The view's canonical order follows the capture, but the
    /// numbers must follow the view: row 20 first, then parent 10.
    #[test]
    fn captured_namespace_order_never_decides_mountinfo_numbers() {
        let mut parent_first = MountIdPool::from_config(&[10, 20, 9], true, &[]);
        let mut parent_last = MountIdPool::from_config(&[20, 10], true, &[]);
        assert_eq!(
            parent_first.resolve_mountinfo(&[10, 20], &[20, 10]),
            Some(vec![1, 2])
        );
        assert_eq!(
            parent_last.resolve_mountinfo(&[20, 10], &[20, 10]),
            Some(vec![1, 2])
        );
        assert_eq!(parent_first.determinize(10, None), Some(2));
        assert_eq!(parent_last.determinize(10, None), Some(2));
    }

    #[test]
    fn mountinfo_observation_order_must_name_exactly_the_canonical_ids() {
        for (canonical, observation) in [
            (&[10, 20][..], &[20][..]),
            (&[10, 20][..], &[20, 30][..]),
            (&[10, 20][..], &[20, 20][..]),
            (&[10, 20][..], &[10, 20, 30][..]),
        ] {
            let mut captured = MountIdPool::from_config(&[10, 20, 30], true, &[]);
            assert_eq!(captured.resolve_mountinfo(canonical, observation), None);
            assert_eq!(captured.determinize(30, None), Some(1));

            // A refused uncaptured view must not seed the namespace order.
            let mut uncaptured = MountIdPool::from_config(&[], false, &[]);
            assert_eq!(uncaptured.resolve_mountinfo(canonical, observation), None);
            assert!(matches!(uncaptured, MountIdPool::Uninitialized));
        }
    }

    #[test]
    fn mountinfo_reads_seed_once_and_refuse_later_table_changes() {
        let mut pool = MountIdPool::from_config(&[], false, &[]);
        assert!(pool.validate_mountinfo_order(&[10, 20, 30]));
        assert!(pool.validate_mountinfo_order(&[10, 20, 30]));
        assert!(!pool.validate_mountinfo_order(&[10, 99, 30]));

        let mut empty = MountIdPool::from_config(&[], false, &[]);
        assert!(empty.validate_mountinfo_order(&[]));
        assert!(!empty.validate_mountinfo_order(&[10]));
    }

    #[test]
    fn configured_mountinfo_order_accepts_only_ordered_known_subsets() {
        let mut pool = MountIdPool::from_config(&[10, 20, 30, 40], true, &[]);
        assert!(pool.validate_mountinfo_order(&[10, 30, 40]));
        assert_eq!(pool.determinize(30, None), Some(1));
        assert!(pool.validate_mountinfo_order(&[20, 40]));
        assert!(!pool.validate_mountinfo_order(&[30, 20]));
        assert!(!pool.validate_mountinfo_order(&[10, 10]));
    }

    #[test]
    fn configured_mountinfo_order_refuses_new_namespace_ids() {
        let mut pool = MountIdPool::from_config(&[10, 20, 30, 40], true, &[]);
        assert!(pool.validate_mountinfo_order(&[10, 30, 40]));
        assert!(
            !pool.validate_mountinfo_order(&[10, 99, 40]),
            "a namespace-local mount ID absent from captured provenance must fail closed"
        );
    }

    #[test]
    fn fdinfo_mount_ids_refuse_malformed_configured_provenance() {
        let mut pool = MountIdPool::from_config(&[10, 10], true, &[]);
        assert_eq!(pool.determinize(10, None), None);
        let mut pool = MountIdPool::from_config(&[10], true, &[700, 700]);
        assert_eq!(pool.determinize(10, None), None);
        // An assignment order without a captured namespace has nothing to
        // validate later views against.
        let mut pool = MountIdPool::from_config(&[], false, &[700]);
        assert_eq!(pool.determinize(700, Some(&[10])), None);
        assert!(pool.provenance().is_err());
        // No recording numbers raw 0 unless mountinfo named it: an fdinfo zero
        // is the anonymous class, and a mountinfo zero must pass validation.
        let mut pool = MountIdPool::from_config(&[10], true, &[0]);
        assert_eq!(pool.determinize(10, None), None);
        assert!(pool.provenance().is_err());
        let mut pool = MountIdPool::from_config(&[10, 0], true, &[10, 0]);
        assert_eq!(pool.resolve_mountinfo(&[10, 0], &[10, 0]), Some(vec![1, 2]));
        assert_eq!(pool.determinize(0, None), Some(0));
    }

    #[test]
    fn raw_zero_is_one_mount_identity_without_descriptor_type_partitioning() {
        let mut pool = MountIdPool::from_config(&[10, 0], true, &[]);
        // The pool API intentionally accepts no descriptor type: memfd and
        // every other Linux descriptor reporting raw mnt_id 0 share this one
        // equivalence class, even when mountinfo uses zero as an outside parent
        // ID for its root row.
        assert_eq!(pool.determinize(0, None), Some(0));
        assert_eq!(pool.determinize(20, None), Some(1));
        assert_eq!(pool.determinize(0, None), Some(0));
    }

    #[tokio::test]
    async fn completing_an_unreported_exit_retires_it_and_records_determinism_loss() {
        if !crate::scheduler::exec_teardown_tests::in_isolated_log_test(
            module_path!(),
            "completing_an_unreported_exit_retires_it_and_records_determinism_loss",
        ) {
            return;
        }
        use std::os::unix::process::ExitStatusExt;

        use super::open_process_pidfd;
        use super::wait_for_process_exit;

        struct Reap(std::process::Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Reap(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let physical_pid = child.0.id() as i32;
        let pidfd = open_process_pidfd(physical_pid).expect("a live child has a process pidfd");
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        }
        .with_backend(|backend| {
            backend.process_exits_complete_asynchronously = true;
            backend.needs_killed_thread_rpc_cancellation = true;
        });
        let state = GlobalState::initialize(&config, false);
        let dettid = DetTid::from_raw(41);
        let mm = MmId::initial(dettid);
        assert_ne!(dettid.as_raw(), physical_pid);
        let pending_request = {
            let mut sched = state.sched.lock().unwrap();
            sched.thread_tree.add_child(dettid, dettid, true);
            sched.record_thread_mm(dettid, mm);
            sched
                .register_physical_thread(dettid, mm, physical_pid, physical_pid)
                .unwrap();
            sched.next_turns.insert(
                dettid,
                crate::scheduler::ThreadNextTurn {
                    dettid,
                    child_tid_addr: 0,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                    protocol: Default::default(),
                },
            );
            sched.next_turns[&dettid].req.clone()
        };
        assert!(pending_request.is_empty());
        child.0.kill().unwrap();
        assert!(
            wait_for_process_exit(pidfd, Duration::from_secs(5))
                .await
                .is_ok()
        );

        // Physical readiness authenticates death, but cleanup takes the
        // registered logical process ID and must not reap the parent's child.
        state.complete_physical_process_exit(dettid.as_raw());
        assert!(pending_request.get().await.is_err());
        let sched = state.sched.lock().unwrap();
        assert!(!sched.next_turns.contains_key(&dettid));
        assert!(sched.process_physically_gone(DetPid::from_raw(41)));
        assert!(!sched.process_physically_gone(DetPid::from_raw(physical_pid)));
        drop(sched);
        assert_eq!(
            crate::detlog::determinism_loss().as_deref(),
            Some("process 41 exited without deregistering; its exit was not scheduled")
        );
        let status = child
            .0
            .wait()
            .expect("cleanup must leave the kernel status for the parent");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert_eq!(status.code(), None);
    }

    #[tokio::test]
    async fn stale_process_exit_cannot_clear_replacement_and_normal_exit_adds_no_loss() {
        if !crate::scheduler::exec_teardown_tests::in_isolated_log_test(
            module_path!(),
            "stale_process_exit_cannot_clear_replacement_and_normal_exit_adds_no_loss",
        ) {
            return;
        }
        use nix::fcntl::OFlag;

        use super::open_process_pidfd;
        use super::wait_for_process_exit;
        use crate::fd::DetFd;
        use crate::fd::FdType;
        use crate::shared_open_files::OpenFileControl;
        use crate::types::OpenFileId;

        struct Reap(std::process::Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Reap(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let physical_pid = child.0.id() as i32;
        let pidfd = open_process_pidfd(physical_pid).unwrap();
        let config = Config {
            sequentialize_threads: true,
            backend: reverie::BackendCapabilities::DBT,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let process = DetPid::from_raw(41);
        let old_mm = MmId::initial(process);
        let mm = old_mm.for_exec(process);
        let (old, current) = {
            let mut sched = state.sched.lock().unwrap();
            sched.thread_tree.add_child(process, process, true);
            sched.record_thread_mm(process, old_mm);
            sched.next_turns.insert(
                process,
                ThreadNextTurn {
                    dettid: process,
                    child_tid_addr: 0,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                    protocol: Default::default(),
                },
            );
            sched
                .register_physical_thread(process, old_mm, physical_pid, physical_pid)
                .unwrap();
            let old = sched.arm_process_exit_watch(
                process,
                old_mm,
                physical_pid,
                open_process_pidfd(physical_pid).unwrap(),
            );
            sched.record_thread_mm(process, mm);
            sched
                .register_physical_thread(process, mm, physical_pid, physical_pid)
                .unwrap();
            let current = sched.arm_process_exit_watch(
                process,
                mm,
                physical_pid,
                open_process_pidfd(physical_pid).unwrap(),
            );
            (old, current)
        };
        let pending = PendingExecState {
            caller: process,
            process,
            mm,
            fd_blocking: Default::default(),
        };
        state
            .pending_exec_states
            .lock()
            .unwrap()
            .insert(process, pending.clone());
        state
            .post_exec_fd_blocking
            .lock()
            .unwrap()
            .insert(process, ExecFdState::default());
        let id = OpenFileId::new(process, 0);
        {
            let mut files = state.shared_open_files.lock().unwrap();
            files
                .insert(DetFd::new(3, OFlag::O_RDWR, FdType::Regular, id).model_for_test())
                .unwrap();
            files
                .handle(process, process, OpenFileControl::Take { id })
                .unwrap();
        }
        child.0.kill().unwrap();
        assert!(
            wait_for_process_exit(pidfd, Duration::from_secs(5))
                .await
                .is_ok()
        );
        let cleanup = state.process_exit_cleanup();
        {
            let mut sched = state.sched.lock().unwrap();
            assert_eq!(cleanup.complete_observed_locked(&mut sched, &old), None);
            assert!(sched.next_turns.contains_key(&process));
            assert_eq!(
                state.pending_exec_states.lock().unwrap().get(&process),
                Some(&pending)
            );
            assert!(
                state
                    .post_exec_fd_blocking
                    .lock()
                    .unwrap()
                    .contains_key(&process)
            );
            assert!(state.shared_open_files.lock().unwrap().is_leased(id));
            assert_eq!(crate::detlog::determinism_loss(), None);

            // Model the normal hook winning the same mutex before the
            // readiness callback. Its later physical completion is no loss.
            state.account_deregistered_thread(
                &mut sched,
                ThreadDeregistration {
                    dettid: process,
                    detpid: process,
                    mm,
                    thread_start_entered: true,
                    timeslice_stats: TimesliceStats::default(),
                    syscall_count: 0,
                    chaos_epochs: Vec::new(),
                },
            );
            assert_eq!(
                cleanup.complete_observed_locked(&mut sched, &current),
                Some((false, None))
            );
            assert_eq!(cleanup.complete_observed_locked(&mut sched, &current), None);
        }
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());
        assert!(!state.shared_open_files.lock().unwrap().is_leased(id));
        assert_eq!(crate::detlog::determinism_loss(), None);
        assert!(child.0.wait().unwrap().code().is_none());
    }

    /// An admitted process can die before any registration names it. The
    /// production completion callback must then record it physically gone,
    /// without panicking on the unknown thread group, so that its late
    /// registration is refused (Codex implementation review, HIGH 1).
    #[test]
    fn completing_a_process_that_never_registered_records_it_gone() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        }
        .with_backend(|backend| {
            backend.process_exits_complete_asynchronously = true;
            backend.needs_killed_thread_rpc_cancellation = true;
        });
        let state = GlobalState::initialize(&config, false);
        state.complete_physical_process_exit(57);
        let sched = state.sched.lock().unwrap();
        assert!(sched.process_physically_gone(DetPid::from_raw(57)));
        assert!(!sched.thread_tree.contains(&DetTid::from_raw(57)));
        assert!(!sched.next_turns.contains_key(&DetTid::from_raw(57)));
    }

    /// Detcore lets reverie-kvm skip empty signal boundaries. The answer is
    /// one constant for every configuration, as the Reverie contract requires
    /// that it never change during a run; only reverie-kvm reads it.
    #[test]
    fn detcore_lets_the_backend_skip_empty_signal_boundaries() {
        for (signal_control, sequentialize_threads) in
            [(true, true), (true, false), (false, true), (false, false)]
        {
            let config = Config {
                sequentialize_threads,
                ..Config::default()
            }
            .with_backend(|backend| {
                backend.provides_process_signal_control = signal_control;
            });
            let state = GlobalState::initialize(&config, false);
            assert!(state.skip_empty_signal_boundaries());
        }
    }

    /// Two backends call `GlobalTool::on_backend_process_exited`, for two
    /// different events, and Detcore tells them apart only by capability. The
    /// ptrace tracer (PTRACE and E9PATCH declare
    /// `reports_child_exit_publication`) reports that the kernel published a
    /// leader's exit to its parent: that releases the scheduler's publication
    /// hold and must leave the physical-exit and exec state alone. In-guest
    /// LiteInst (`process_exits_complete_asynchronously`, no publication
    /// reports) reports the process's physical exit: that releases its
    /// physical-exit barrier and clears its exec state, and is not a
    /// publication. Each case starts from the state both paths could touch, so
    /// taking the other path fails it.
    #[test]
    fn backend_exit_reports_take_the_path_the_backend_capabilities_select() {
        use reverie::BackendCapabilities;
        for (name, backend) in [
            ("PTRACE", BackendCapabilities::PTRACE),
            ("E9PATCH", BackendCapabilities::E9PATCH),
            ("LITEINST_IN_GUEST", BackendCapabilities::LITEINST_IN_GUEST),
        ] {
            let publishes = backend.reports_child_exit_publication;
            assert_eq!(publishes, name != "LITEINST_IN_GUEST", "{name}");
            let config = Config {
                sequentialize_threads: true,
                backend,
                ..Config::default()
            };
            let state = GlobalState::initialize(&config, false);
            let detpid = DetPid::from_raw(43);
            let leader = DetTid::from_raw(43);
            state.pending_exec_states.lock().unwrap().insert(
                detpid,
                PendingExecState {
                    caller: leader,
                    process: detpid,
                    mm: MmId::initial(detpid),
                    fd_blocking: Default::default(),
                },
            );
            state.post_exec_fd_blocking.lock().unwrap().insert(
                leader,
                ExecFdState::from([42].into_iter().collect::<ExecFdBlockingOverrides>()),
            );
            {
                let mut sched = state.sched.lock().unwrap();
                sched.install_test_child_exit_publication_hold(detpid);
                // Only a backend whose exits complete asynchronously (or that
                // reports physical exits) installs a physical-exit barrier.
                assert_eq!(
                    sched.begin_physical_process_exit(detpid),
                    !publishes,
                    "{name}"
                );
            }

            <GlobalState as GlobalTool>::on_backend_process_exited(&state, detpid.as_raw());

            let sched = state.sched.lock().unwrap();
            let exec_state_kept = state
                .pending_exec_states
                .lock()
                .unwrap()
                .contains_key(&detpid);
            let fd_blocking_kept = state
                .post_exec_fd_blocking
                .lock()
                .unwrap()
                .contains_key(&leader);
            if publishes {
                // The hold is released, not recorded as an early report.
                assert_eq!(
                    sched.test_child_exit_publication_state(detpid),
                    (false, false),
                    "{name}"
                );
                assert!(!sched.process_physically_gone(detpid), "{name}");
                assert!(exec_state_kept, "{name}");
                assert!(fd_blocking_kept, "{name}");
            } else {
                assert!(sched.pending_async_process_exits().is_empty(), "{name}");
                assert!(sched.process_physically_gone(detpid), "{name}");
                assert!(!exec_state_kept, "{name}");
                assert!(!fd_blocking_kept, "{name}");
                // Not a publication: the hold is neither released nor recorded.
                assert_eq!(
                    sched.test_child_exit_publication_state(detpid),
                    (true, false),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn recorded_assignment_order_rebuilds_the_same_mapping_in_any_replay_order() {
        let mut recording = MountIdPool::from_config(&[], false, &[]);
        assert_eq!(recording.determinize(700, Some(&[10, 20])), Some(1));
        assert_eq!(
            recording.resolve_mountinfo(&[10, 20], &[10, 20]),
            Some(vec![2, 3])
        );
        assert_eq!(recording.determinize(701, None), Some(4));
        let provenance = recording.provenance().unwrap().unwrap();
        assert_eq!(provenance.mountinfo_order, [10, 20]);
        assert_eq!(provenance.assignment_order, [700, 10, 20, 701]);

        // Replay observes the same raw IDs in a different order and still
        // gets every recorded number.
        let mut replay = MountIdPool::from_config(
            &provenance.mountinfo_order,
            true,
            &provenance.assignment_order,
        );
        assert_eq!(replay.determinize(701, None), Some(4));
        assert_eq!(replay.resolve_mountinfo(&[20], &[20]), Some(vec![3]));
        assert_eq!(replay.determinize(10, None), Some(2));
        assert_eq!(replay.determinize(700, None), Some(1));
        assert_eq!(
            replay.provenance().unwrap().unwrap().assignment_order,
            provenance.assignment_order
        );
    }

    #[tokio::test]
    async fn boundary_owner_is_allocated_by_actual_root_registration() {
        for current_image in [false, true] {
            let config = Config {
                sequentialize_threads: false,
                ..Config::default()
            };
            let state = GlobalState::initialize(&config, false);
            state.sched.lock().unwrap().boundary_owners =
                Some(crate::slice_boundary::lifecycle::Registry::default());
            let root = DetTid::from_raw(3);
            let mm = if current_image {
                MmId::initial(root).for_exec(root)
            } else {
                MmId::initial(root)
            };
            let before = {
                let scheduler = state.sched.lock().unwrap();
                (scheduler.turn, scheduler.committed_time)
            };
            assert_eq!(
                state
                    .recv_create_child_thread(
                        Tid::from_raw(root.as_raw()),
                        mm,
                        super::ChildRegistration {
                            parent_dettid: root,
                            parent_detpid: root,
                            child_dettid: root,
                            child_tid_addr: 0,
                            flags: None,
                            exit_signal: libc::SIGCHLD,
                            physical_ids: None,
                            maybe_priority: Some(DEFAULT_PRIORITY),
                            parent_is_kernel_blocked: false,
                        }
                    )
                    .await,
                SchedulerRpcResult::Continue(())
            );
            let scheduler = state.sched.lock().unwrap();
            let owner = scheduler
                .boundary_owners
                .as_ref()
                .unwrap()
                .owner(root, mm)
                .unwrap();
            assert_eq!(owner.tid, root);
            assert_eq!(owner.birth_tid, root);
            assert_eq!(owner.birth_generation, 0);
            assert_eq!(owner.thread_generation, 0);
            assert_eq!(owner.address_space_creator, mm.creator());
            assert_eq!(owner.address_space_generation, mm.generation());
            assert_eq!((scheduler.turn, scheduler.committed_time), before);
        }
    }

    #[tokio::test]
    async fn initial_current_root_registration_is_consistent_and_single_use() {
        let config = Config {
            sequentialize_threads: false,
            ..Config::default()
        }
        .with_backend(|backend| backend.tool_shares_guest_descriptor_table = true);
        let mut state = GlobalState::initialize(&config, false);
        let pipe = reverie::InheritedInput::pipe(0).unwrap();
        state.install_inherited_input(pipe.lease().unwrap());
        let root = DetTid::from_raw(3);
        let initial = MmId::initial(root);
        let current = initial.for_exec(root);
        let registration = || super::ChildRegistration {
            parent_dettid: root,
            parent_detpid: root,
            child_dettid: root,
            child_tid_addr: 0,
            flags: None,
            exit_signal: libc::SIGCHLD,
            physical_ids: None,
            maybe_priority: Some(DEFAULT_PRIORITY),
            parent_is_kernel_blocked: false,
        };
        assert!(matches!(
            state
                .recv_create_child_thread(Tid::from_raw(root.as_raw()), current, registration())
                .await,
            SchedulerRpcResult::Continue(())
        ));
        assert_eq!(state.root_dettid.get(), Some(&root));
        assert_eq!(
            state.inherited_input.lock().unwrap().as_ref().unwrap().root,
            Some((root, current))
        );
        // Completion consumes the launch input at its birth identity; there is
        // no intermediate old-MM registration and no changed Mark response.
        let response = state
            .receive_rpc(
                Tid::from_raw(root.as_raw()),
                (
                    DetTime::new(&config),
                    initial,
                    GlobalRequest::MarkPastFirstExecve(root, None, None, None),
                ),
            )
            .await
            .1;
        assert_eq!(response, GlobalResponse::ThreadExited);
        assert!(state.inherited_input.lock().unwrap().is_some());
        let response = state
            .receive_rpc(
                Tid::from_raw(root.as_raw()),
                (
                    DetTime::new(&config),
                    current,
                    GlobalRequest::MarkPastFirstExecve(root, None, None, None),
                ),
            )
            .await
            .1;
        let GlobalResponse::MarkPastFirstExecve(completion) = response else {
            panic!("{response:?}")
        };
        assert_eq!(completion.initial_input.as_ref(), Some(pipe.identity()));
        assert!(state.inherited_input.lock().unwrap().is_none());
        {
            let sched = state.sched.lock().unwrap();
            assert_eq!(sched.thread_mm(root), Some(current));
            assert!(sched.rpc_incarnation_matches(root, current));
            assert!(!sched.rpc_incarnation_matches(root, initial));
            assert!(sched.thread_was_registered(root));
        }
        assert!(matches!(
            state
                .recv_create_child_thread(Tid::from_raw(root.as_raw()), current, registration())
                .await,
            SchedulerRpcResult::ThreadExited
        ));
        assert_eq!(state.sched.lock().unwrap().thread_mm(root), Some(current));
    }

    #[tokio::test]
    async fn boundary_same_leader_exec_completion_returns_actual_new_owner_without_a_turn() {
        let (config, state, tid, process) = cancellation_test_state();
        install_test_registration(&state, tid, Ivar::new());
        let old_mm = MmId::initial(tid);
        let new_mm = old_mm.for_exec(tid);
        let old_owner = {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.install_test_exec_incarnation(tid, old_mm);
            let mut owners = crate::slice_boundary::lifecycle::Registry::default();
            let owner = owners.fresh(tid, old_mm).unwrap();
            scheduler.boundary_owners = Some(owners);
            owner
        };
        state.pending_exec_states.lock().unwrap().insert(
            process,
            PendingExecState {
                caller: tid,
                process,
                mm: old_mm,
                fd_blocking: Default::default(),
            },
        );
        let before = {
            let scheduler = state.sched.lock().unwrap();
            (scheduler.turn, scheduler.committed_time)
        };
        let response = state
            .receive_rpc(
                Tid::from_raw(tid.as_raw()),
                (
                    DetTime::new(&config),
                    new_mm,
                    GlobalRequest::MarkPastFirstExecve(process, None, None, None),
                ),
            )
            .await;
        let GlobalResponse::WithSliceBoundaryOwner(owner, response) = response.1 else {
            panic!("lost actual exec owner")
        };
        assert!(matches!(*response, GlobalResponse::MarkPastFirstExecve(_)));
        assert_eq!(
            (owner.birth_tid, owner.birth_generation),
            (old_owner.birth_tid, old_owner.birth_generation)
        );
        assert_eq!(owner.thread_generation, old_owner.thread_generation);
        assert_eq!(owner.address_space_generation, new_mm.generation());
        let scheduler = state.sched.lock().unwrap();
        assert_eq!(
            scheduler
                .boundary_owners
                .as_ref()
                .unwrap()
                .owner(tid, new_mm)
                .unwrap(),
            owner
        );
        assert!(
            scheduler
                .boundary_owners
                .as_ref()
                .unwrap()
                .owner(tid, old_mm)
                .is_err()
        );
        assert_eq!((scheduler.turn, scheduler.committed_time), before);
    }

    #[tokio::test]
    async fn initial_current_root_rejects_stale_nonroot_and_wrong_incarnations() {
        let root = DetTid::from_raw(3);
        let other = DetTid::from_raw(4);
        let current = MmId::initial(root).for_exec(root);
        for case in 0..8 {
            let config = Config {
                sequentialize_threads: false,
                ..Config::default()
            };
            let state = GlobalState::initialize(&config, false);
            let mut registration = super::ChildRegistration {
                parent_dettid: root,
                parent_detpid: root,
                child_dettid: root,
                child_tid_addr: 0,
                flags: None,
                exit_signal: libc::SIGCHLD,
                physical_ids: None,
                maybe_priority: Some(DEFAULT_PRIORITY),
                parent_is_kernel_blocked: false,
            };
            let mut sender = root;
            let mut mm = current;
            match case {
                0 => sender = other,
                1 => registration.parent_dettid = other,
                2 => registration.parent_detpid = other,
                3 => registration.child_tid_addr = 8,
                4 => registration.parent_is_kernel_blocked = true,
                5 => mm = current.for_exec(root),
                6 => mm = MmId::initial(other).for_exec(other),
                7 => {
                    // A registered but unrelated sender cannot make a
                    // root-shaped request ordinary parent creation.
                    sender = other;
                    state
                        .sched
                        .lock()
                        .unwrap()
                        .thread_tree
                        .add_child(other, other, true);
                }
                _ => unreachable!(),
            }
            assert!(matches!(
                state
                    .recv_create_child_thread(Tid::from_raw(sender.as_raw()), mm, registration)
                    .await,
                SchedulerRpcResult::ThreadExited
            ));
            assert!(state.root_dettid.get().is_none());
            assert_eq!(state.sched.lock().unwrap().thread_mm(root), None);
        }
    }

    #[tokio::test]
    async fn registered_parent_creation_preserves_the_child_mm_after_initial_image_birth() {
        let parent = DetTid::from_raw(3);
        let child = DetTid::from_raw(4);
        for parent_mm in [
            MmId::initial(parent),
            MmId::initial(parent).for_exec(parent),
        ] {
            for flags in [None, Some(CloneFlags::empty())] {
                let config = Config {
                    sequentialize_threads: false,
                    ..Config::default()
                };
                let state = GlobalState::initialize(&config, false);
                let registration = |child_dettid, flags| super::ChildRegistration {
                    parent_dettid: parent,
                    parent_detpid: parent,
                    child_dettid,
                    child_tid_addr: 0,
                    flags,
                    exit_signal: libc::SIGCHLD,
                    physical_ids: None,
                    maybe_priority: Some(DEFAULT_PRIORITY),
                    parent_is_kernel_blocked: false,
                };
                assert!(matches!(
                    state
                        .recv_create_child_thread(
                            Tid::from_raw(parent.as_raw()),
                            parent_mm,
                            registration(parent, None),
                        )
                        .await,
                    SchedulerRpcResult::Continue(())
                ));
                assert!(matches!(
                    state
                        .recv_create_child_thread(
                            Tid::from_raw(parent.as_raw()),
                            parent_mm,
                            registration(child, flags),
                        )
                        .await,
                    SchedulerRpcResult::Continue(())
                ));
                let sched = state.sched.lock().unwrap();
                assert_eq!(sched.thread_mm(parent), Some(parent_mm));
                assert_eq!(sched.thread_mm(child), Some(MmId::initial(child)));
                assert!(sched.thread_was_registered(parent));
                assert!(sched.thread_was_registered(child));
                assert!(sched.rpc_incarnation_matches(parent, parent_mm));
                assert_eq!(state.root_dettid.get(), Some(&parent));
            }
        }
    }

    /// The global tool's `/proc/<pid>/auxv` rows, driven through its RPC
    /// handlers and the backend's exit reports as a ptrace run drives them:
    /// a forked child (and a CLONE_VM child, sharing the creator's memory
    /// map) inherit the creator's vector; an exec whose vector could not be
    /// recorded drops the child's stale one (`AuxvFromKernel`), and the run
    /// warns only of the first; the exit reports a ptrace backend sends
    /// (child-exit publication, which bypasses the physical-exit cleanup)
    /// leave no row behind, the last process of the shared map included.
    #[tokio::test]
    async fn auxv_rows_follow_fork_failed_exec_and_exit_through_the_handlers() {
        use super::AuxvFromKernel;
        use super::ChildRegistration;

        let config = Config {
            sequentialize_threads: false,
            ..Config::default()
        }
        .with_backend(|backend| backend.reports_child_exit_publication = true);
        let state = GlobalState::initialize(&config, false);
        let root = DetTid::from_raw(3);
        let (forked, shared) = (DetTid::from_raw(4), DetTid::from_raw(5));
        let root_mm = MmId::initial(root);
        assert!(matches!(
            state
                .recv_create_child_thread(
                    Tid::from_raw(root.as_raw()),
                    root_mm,
                    ChildRegistration {
                        parent_dettid: root,
                        parent_detpid: root,
                        child_dettid: root,
                        child_tid_addr: 0,
                        flags: None,
                        exit_signal: libc::SIGCHLD,
                        physical_ids: None,
                        maybe_priority: Some(DEFAULT_PRIORITY),
                        parent_is_kernel_blocked: false,
                    },
                )
                .await,
            SchedulerRpcResult::Continue(())
        ));
        let rpc = |task: DetTid, mm: MmId, request: GlobalRequest| {
            state.receive_rpc(
                Tid::from_raw(task.as_raw()),
                (DetTime::new(&config), mm, request),
            )
        };
        let response = rpc(root, root_mm, GlobalRequest::RecordProcessAuxv(vec![7; 32]))
            .await
            .1;
        assert_eq!(response, GlobalResponse::RecordProcessAuxv(()));
        for (task, flags) in [
            (forked, CloneFlags::empty()),
            (shared, CloneFlags::CLONE_VM),
        ] {
            assert!(matches!(
                state
                    .recv_create_child_thread(
                        Tid::from_raw(root.as_raw()),
                        root_mm,
                        ChildRegistration {
                            parent_dettid: root,
                            parent_detpid: root,
                            child_dettid: task,
                            child_tid_addr: 0,
                            flags: Some(flags),
                            exit_signal: libc::SIGCHLD,
                            physical_ids: None,
                            maybe_priority: Some(DEFAULT_PRIORITY),
                            parent_is_kernel_blocked: false,
                        },
                    )
                    .await,
                SchedulerRpcResult::Continue(())
            ));
        }
        let current = |task: DetTid| {
            rpc(
                root,
                root_mm,
                GlobalRequest::ProcessAuxv(DetPid::from_raw(task.as_raw())),
            )
        };
        for task in [root, forked, shared] {
            let GlobalResponse::ProcessAuxv(Some(entry)) = current(task).await.1 else {
                panic!("{task} has no vector");
            };
            assert_eq!(entry.bytes, vec![7; 32], "{task}");
        }
        assert_eq!(state.process_auxv.lock().unwrap().len(), 3);
        // The forked child's exec finds no plausible vector: its creator's is
        // stale and goes; only the first such exec of the run warns.
        let forked_mm = MmId::initial(forked);
        for first in [true, false] {
            let response = rpc(
                forked,
                forked_mm,
                GlobalRequest::AuxvFromKernel(AuxvFromKernel::Implausible),
            )
            .await
            .1;
            assert_eq!(response, GlobalResponse::AuxvFromKernel(first));
        }
        assert_eq!(current(forked).await.1, GlobalResponse::ProcessAuxv(None));
        assert_eq!(state.process_auxv.lock().unwrap().len(), 2);
        // The exits as a ptrace backend reports them.
        for task in [forked, shared, root] {
            state.on_backend_process_exited(task.as_raw());
        }
        assert_eq!(state.process_auxv.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn inherited_input_belongs_only_to_the_registered_root_initial_image() {
        check_inherited_input_root_ownership(super::ROOT_DETPID, DetTid::from_raw(4)).await;
    }

    #[tokio::test]
    async fn inherited_input_uses_registered_root_identity_instead_of_pid_three() {
        // A descendant with the old numeric root ID must not acquire the lease.
        check_inherited_input_root_ownership(DetTid::from_raw(37), DetTid::from_raw(3)).await;
    }

    async fn check_inherited_input_root_ownership(root: DetTid, child: DetTid) {
        use super::ChildRegistration;

        let config = Config {
            sequentialize_threads: false,
            ..Config::default()
        }
        .with_backend(|backend| backend.tool_shares_guest_descriptor_table = true);
        let mut state = GlobalState::initialize(&config, false);
        let pipe = reverie::InheritedInput::pipe(0).unwrap();
        state.install_inherited_input(pipe.lease().unwrap());
        let worker = DetTid::from_raw(root.as_raw() + 2);
        let root_mm = MmId::initial(root);
        for (parent, task, flags) in [
            (root, root, None),
            (root, child, Some(CloneFlags::empty())),
            (
                root,
                worker,
                Some(CloneFlags::CLONE_VM | CloneFlags::CLONE_SIGHAND | CloneFlags::CLONE_THREAD),
            ),
        ] {
            assert!(matches!(
                state
                    .recv_create_child_thread(
                        Tid::from_raw(parent.as_raw()),
                        root_mm,
                        ChildRegistration {
                            parent_dettid: parent,
                            parent_detpid: root,
                            child_dettid: task,
                            child_tid_addr: 0,
                            flags,
                            exit_signal: libc::SIGCHLD,
                            physical_ids: None,
                            maybe_priority: Some(DEFAULT_PRIORITY),
                            parent_is_kernel_blocked: false,
                        },
                    )
                    .await,
                SchedulerRpcResult::Continue(())
            ));
        }
        assert_eq!(
            state.inherited_input.lock().unwrap().as_ref().unwrap().root,
            Some((root, root_mm))
        );

        // Neither a descendant nor a sibling sharing the root's exact MmId
        // may consume the seed before the root's own completion.
        for (task, detpid, mm) in [
            (child, child, MmId::initial(child)),
            (worker, root, root_mm),
        ] {
            let response = state
                .receive_rpc(
                    Tid::from_raw(task.as_raw()),
                    (
                        DetTime::new(&config),
                        mm,
                        GlobalRequest::MarkPastFirstExecve(detpid, None, None, None),
                    ),
                )
                .await
                .1;
            let GlobalResponse::MarkPastFirstExecve(non_root_state) = response else {
                panic!("{response:?}")
            };
            assert!(non_root_state.initial_input.is_none());
            assert!(state.inherited_input.lock().unwrap().is_some());
        }

        // Nor can a later root image, even with the correct descriptor slot.
        let response = state
            .receive_rpc(
                Tid::from_raw(root.as_raw()),
                (
                    DetTime::new(&config),
                    root_mm.for_exec(root),
                    GlobalRequest::MarkPastFirstExecve(root, None, None, None),
                ),
            )
            .await
            .1;
        // An initial registration has no retired-exec incarnation entry, so
        // the existing RPC layer returns an empty completion here. The launch
        // lease's stricter root/image binding must still refuse the seed.
        assert_eq!(
            response,
            GlobalResponse::MarkPastFirstExecve(Default::default())
        );
        assert!(state.inherited_input.lock().unwrap().is_some());

        let response = state
            .receive_rpc(
                Tid::from_raw(root.as_raw()),
                (
                    DetTime::new(&config),
                    root_mm,
                    GlobalRequest::MarkPastFirstExecve(root, None, None, None),
                ),
            )
            .await
            .1;
        let GlobalResponse::MarkPastFirstExecve(root_state) = response else {
            panic!("{response:?}")
        };
        assert_eq!(root_state.initial_input.as_ref(), Some(pipe.identity()));
        assert!(state.inherited_input.lock().unwrap().is_none());
        let response = state
            .receive_rpc(
                Tid::from_raw(root.as_raw()),
                (
                    DetTime::new(&config),
                    root_mm,
                    GlobalRequest::MarkPastFirstExecve(root, None, None, None),
                ),
            )
            .await
            .1;
        let GlobalResponse::MarkPastFirstExecve(repeated) = response else {
            panic!("{response:?}")
        };
        assert!(repeated.initial_input.is_none());

        // An exec preparation is a return of existing OFD state, not an
        // installation channel for copied launch identities.
        let response = state
            .receive_rpc(
                Tid::from_raw(root.as_raw()),
                (
                    DetTime::new(&config),
                    root_mm,
                    GlobalRequest::PrepareExec(root, root_mm, root_state),
                ),
            )
            .await
            .1;
        assert_eq!(response, GlobalResponse::ThreadExited);
    }

    #[tokio::test]
    async fn failed_root_registration_does_not_bind_inherited_input() {
        for raw_root in [3, 37] {
            let config = Config {
                sequentialize_threads: false,
                ..Config::default()
            }
            .with_backend(|backend| backend.tool_shares_guest_descriptor_table = true);
            let mut state = GlobalState::initialize(&config, false);
            let pipe = reverie::InheritedInput::pipe(0).unwrap();
            state.install_inherited_input(pipe.lease().unwrap());
            let root = DetTid::from_raw(raw_root);
            let root_mm = MmId::initial(root);
            let response = state
                .recv_create_child_thread(
                    Tid::from_raw(raw_root),
                    root_mm,
                    super::ChildRegistration {
                        parent_dettid: root,
                        parent_detpid: root,
                        child_dettid: root,
                        child_tid_addr: 0,
                        flags: None,
                        exit_signal: libc::SIGCHLD,
                        // The real physical-registration boundary rejects
                        // nonpositive IDs before opening any kernel pidfd.
                        physical_ids: Some((-1, -1)),
                        maybe_priority: Some(DEFAULT_PRIORITY),
                        parent_is_kernel_blocked: false,
                    },
                )
                .await;
            assert!(matches!(response, SchedulerRpcResult::ThreadExited));
            let input = state.inherited_input.lock().unwrap();
            let input = input
                .as_ref()
                .expect("failed registration consumed the lease");
            assert!(input.root.is_none(), "failed registration bound the root");
            assert_eq!(input.lease.identity(), pipe.identity());
        }
    }

    fn cancellation_test_state() -> (Config, GlobalState, DetTid, DetPid) {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        }
        .with_backend(|backend| {
            backend.needs_killed_thread_rpc_cancellation = true;
        });
        cancellation_test_state_with_config(config)
    }

    fn cancellation_test_state_with_config(
        config: Config,
    ) -> (Config, GlobalState, DetTid, DetPid) {
        let state = GlobalState::initialize(&config, false);
        let dettid = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, dettid, true);
        // The root inherits its process group from outside the namespace; this
        // test's root leads its own, as after setpgid(0, 0).
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .set_process_group(detpid, detpid);
        (config, state, dettid, detpid)
    }

    fn install_test_registration(state: &GlobalState, dettid: DetTid, request: Ivar<SchedRequest>) {
        let mut scheduler = state.sched.lock().unwrap();
        assert!(!scheduler.thread_is_logically_killed(dettid));
        scheduler.next_turns.insert(
            dettid,
            ThreadNextTurn {
                dettid,
                child_tid_addr: 0,
                req: request,
                resp: Ivar::new(),
                protocol: Default::default(),
            },
        );
        scheduler.priorities.insert(dettid, DEFAULT_PRIORITY);
        scheduler.runqueue_push_back(dettid);
    }

    // These tests stage a validated expectation, but use the real RPC and
    // real committed turn to deliver it. They do not enable replay lookahead
    // or claim callback/exec-lifecycle qualification.
    async fn boundary_producer_roundtrip(
        enabled: bool,
        input: Option<&std::path::Path>,
        output: Option<&std::path::Path>,
    ) -> (Vec<Resources>, LogicalTime, LogicalTime, u64, u64) {
        use detcore_model::schedule::boundary::Entry;
        use detcore_model::schedule::boundary::Phase;
        use detcore_model::schedule::boundary::Record;
        use reverie::Tool;

        use crate::preemptions::boundary_recording::Recording;
        let (config, state, tid, process) = if input.is_some() || output.is_some() {
            cancellation_test_state_with_config(
                Config {
                    sequentialize_threads: true,
                    replay_schedule_from: input.map(std::path::Path::to_path_buf),
                    strict_schedule_boundaries: true,
                    record_preemptions: true,
                    record_preemptions_to: output.map(std::path::Path::to_path_buf),
                    ..Config::default()
                }
                .with_backend(|backend| backend.needs_killed_thread_rpc_cancellation = true),
            )
        } else {
            cancellation_test_state()
        };
        install_test_registration(&state, tid, Ivar::new());
        let mut thread = crate::ThreadState::new(tid, &config, ());
        thread.detpid = Some(process);
        thread.next_timeslice(&config);
        if enabled {
            thread.slice_boundary = Some(crate::slice_boundary::Observations::new(&thread, 0));
            let mut scheduler = state.sched.lock().unwrap();
            let mut owners = crate::slice_boundary::lifecycle::Registry::default();
            owners.fresh(tid, thread.mm_id).unwrap();
            scheduler.boundary_owners = Some(owners);
            scheduler.record_thread_mm(tid, thread.mm_id);
            if input.is_some() {
                let budget = scheduler
                    .boundary_budget_for_live_owner(tid)
                    .unwrap()
                    .unwrap();
                crate::slice_boundary::grant::install(&mut thread, budget).unwrap();
            }
            let mut writer =
                crate::preemptions::PreemptionWriter::new(output.map(std::path::Path::to_path_buf))
                    .with_slice_boundaries()
                    .unwrap();
            writer.register_thread(tid, DEFAULT_PRIORITY);
            scheduler.preemption_writer = Some(writer);
        }
        let mut guest = ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: false,
            ppid: None,
            private_memory: None,
        };
        let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
        let mut turns = Vec::new();
        for _ in 0..2 {
            if let Some(mut observed) = guest.thread.slice_boundary.take() {
                observed
                    .callback(&guest.thread, Entry::Cpuid, Phase::Prehook)
                    .unwrap();
                guest.thread.slice_boundary = Some(observed);
            }
            let mut request = Box::pin(tool.end_timeslice(&mut guest));
            for _ in 0..8 {
                assert!(futures::poll!(&mut request).is_pending());
                if state.sched.lock().unwrap().next_turns[&tid]
                    .req
                    .try_read()
                    .is_some()
                {
                    break;
                }
            }
            let posted = state.sched.lock().unwrap().next_turns[&tid]
                .req
                .try_read()
                .unwrap()
                .unwrap();
            assert!(
                posted.slice_boundary.is_none(),
                "stored/reposted resource must not retain an annotation"
            );
            assert!(posted.upgraded_to_runnable().slice_boundary.is_none());
            let turn = crate::scheduler::do_a_turn_blocking(
                state.sched.clone(),
                state.global_time.clone(),
                &Err(crate::scheduler::SkipTurn),
            )
            .await
            .expect("the original request must commit exactly one turn");
            assert!(turn.slice_boundary.is_none());
            turns.push(turn);
            request.await;
        }
        let mut scheduler = state.sched.lock().unwrap();
        if enabled {
            let writer = scheduler.preemption_writer.take().unwrap();
            let encoded = if let Some(path) = output {
                writer.flush().unwrap();
                std::fs::read_to_string(path).unwrap()
            } else {
                writer.into_string()
            };
            let Recording::Boundaries(recording) = Recording::decode(encoded.as_bytes()).unwrap()
            else {
                panic!("lost typed recording")
            };
            let records = recording.schedule().records();
            assert_eq!(records.len(), 2);
            for (index, record) in records.iter().enumerate() {
                let Record::TimesliceEnd(boundary) = record else {
                    panic!("introduced an ordinary event")
                };
                assert_eq!(boundary.identity.slice, index as u64 + 1);
                assert_eq!(boundary.identity.checkin, index as u64 + 1);
                assert_eq!(boundary.identity.committed_rcbs, 0);
                assert_eq!(boundary.identity.entry, Entry::Cpuid);
            }
            assert_eq!(scheduler.recorded_event_count, 2);
            if input.is_some() {
                assert_eq!(
                    scheduler
                        .generate_partial_run_summary(None)
                        .unwrap()
                        .schedevent_replayed,
                    2
                );
            }
        }
        (
            turns,
            scheduler.committed_time,
            guest.thread.thread_logical_time.as_nanos(),
            scheduler.turn,
            guest.thread.stats.timeslice_count,
        )
    }

    #[tokio::test]
    async fn boundary_actual_reset_and_resource_transport_preserve_original_turns_and_clocks() {
        assert_eq!(
            boundary_producer_roundtrip(true, None, None).await,
            boundary_producer_roundtrip(false, None, None).await
        );
    }

    #[tokio::test]
    async fn boundary_recorded_bytes_replay_two_original_yields_with_exact_clock_and_turn_parity() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.json");
        let replay = dir.path().join("replay.json");
        let recorded = boundary_producer_roundtrip(true, None, Some(&first)).await;
        let replayed = boundary_producer_roundtrip(true, Some(&first), Some(&replay)).await;
        assert_eq!(recorded, replayed);
        assert_eq!(
            std::fs::read(first).unwrap(),
            std::fs::read(replay).unwrap()
        );
    }

    // Exercise the syscall handler and the original resource receiver/turn,
    // rather than calling close_slice or end_timeslice directly. No guest or
    // backend is executed by this fixture. Both timer modes remain supported.
    async fn boundary_sched_yield_handler(
        input: Option<&std::path::Path>,
        typed: bool,
        chaos: bool,
        timers: bool,
        stale_owner: bool,
    ) -> (
        crate::preemptions::boundary_recording::Recording,
        LogicalTime,
        LogicalTime,
        u64,
        u64,
    ) {
        use detcore_model::schedule::boundary::Cause;
        use detcore_model::schedule::boundary::Entry;
        use detcore_model::schedule::boundary::Phase;
        use detcore_model::schedule::boundary::Record;
        use reverie::Tool;

        use crate::preemptions::boundary_recording::Recording;

        let (config, state, tid, process) = cancellation_test_state_with_config(
            Config {
                sequentialize_threads: true,
                replay_schedule_from: input.map(std::path::Path::to_path_buf),
                strict_schedule_boundaries: typed && !chaos && input.is_some(),
                chaos,
                max_timeslice: if timers {
                    Config::default().max_timeslice
                } else {
                    None
                },
                target_timeslice: None,
                ..Config::default()
            }
            .with_backend(|backend| backend.needs_killed_thread_rpc_cancellation = true),
        );
        install_test_registration(&state, tid, Ivar::new());
        let mut thread = crate::ThreadState::new(tid, &config, ());
        thread.detpid = Some(process);
        thread.next_timeslice(&config);
        let initial_slices = thread.stats.timeslice_count;
        {
            let mut scheduler = state.sched.lock().unwrap();
            let mut writer = crate::preemptions::PreemptionWriter::new(None);
            if typed {
                thread.slice_boundary = Some(crate::slice_boundary::Observations::new(&thread, 0));
                let mut owners = crate::slice_boundary::lifecycle::Registry::default();
                owners.fresh(tid, thread.mm_id).unwrap();
                scheduler.boundary_owners = Some(owners);
                scheduler.record_thread_mm(tid, thread.mm_id);
                if let Some(budget) = scheduler.boundary_budget_for_live_owner(tid).unwrap() {
                    crate::slice_boundary::grant::install(&mut thread, budget).unwrap();
                }
                writer = writer.with_slice_boundaries().unwrap();
            }
            writer.register_thread(tid, DEFAULT_PRIORITY);
            scheduler.preemption_writer = Some(writer);
        }
        let mut guest = ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: false,
            ppid: None,
            private_memory: None,
        };
        let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
        for occurrence in 1..=2 {
            if let Some(mut observed) = guest.thread.slice_boundary.take() {
                observed
                    .syscall(&guest.thread, Sysno::sched_yield, Phase::Prehook)
                    .unwrap();
                guest.thread.slice_boundary = Some(observed);
            }
            if stale_owner {
                guest.thread.mm_id = guest.thread.mm_id.for_exec(tid);
            }
            let before_turn = state.sched.lock().unwrap().turn;
            let before_requests = guest.requests.lock().unwrap().len();
            let mut request = Box::pin(
                tool.handle_sched_yield(&mut guest, reverie::syscalls::SchedYield::default()),
            );
            for _ in 0..8 {
                assert!(futures::poll!(&mut request).is_pending());
                if state.sched.lock().unwrap().next_turns[&tid]
                    .req
                    .try_read()
                    .is_some()
                {
                    break;
                }
            }
            let posted = state.sched.lock().unwrap().next_turns[&tid]
                .req
                .try_read()
                .unwrap()
                .unwrap();
            assert!(posted.slice_boundary.is_none());
            assert!(posted.upgraded_to_runnable().slice_boundary.is_none());
            // Record's original strong yield and replay's original weak yield
            // remain distinct. Typed boundary consumption must not replace the
            // replay request with an independent one-turn exclusion.
            let expected = if input.is_none() && !chaos {
                ResourceID::SchedYield
            } else {
                ResourceID::SleepUntil(LogicalTime::from_nanos(0))
            };
            assert_eq!(posted.resources.len(), 1);
            assert_eq!(posted.resources.get(&expected), Some(&Permission::W));
            let turn = crate::scheduler::do_a_turn_blocking(
                state.sched.clone(),
                state.global_time.clone(),
                &Err(crate::scheduler::SkipTurn),
            )
            .await
            .expect("one sched_yield must commit its original turn");
            assert!(turn.slice_boundary.is_none());
            assert_eq!(request.await.unwrap(), 0);
            assert_eq!(state.sched.lock().unwrap().turn, before_turn + 1);
            assert_eq!(guest.requests.lock().unwrap().len(), before_requests + 1);
            if let Some(mut observed) = guest.thread.slice_boundary.take() {
                observed
                    .syscall(&guest.thread, Sysno::sched_yield, Phase::Posthook)
                    .unwrap();
                guest.thread.slice_boundary = Some(observed);
            }
            let closes = typed && !chaos;
            assert_eq!(
                guest.thread.stats.timeslice_count,
                initial_slices + if closes && timers { occurrence } else { 0 }
            );
        }
        let mut scheduler = state.sched.lock().unwrap();
        let recording = Recording::decode(
            scheduler
                .preemption_writer
                .take()
                .unwrap()
                .into_string()
                .as_bytes(),
        )
        .unwrap();
        if let Recording::Boundaries(recording) = &recording {
            let records = recording.schedule().records();
            assert_eq!(records.len(), if chaos { 0 } else { 2 });
            for (index, record) in records.iter().enumerate() {
                let Record::TimesliceEnd(boundary) = record else {
                    panic!("not a boundary")
                };
                assert_eq!(boundary.identity.cause, Cause::ExplicitSchedYield);
                assert_eq!(boundary.identity.phase, Phase::Prehook);
                assert_eq!(
                    boundary.identity.entry,
                    Entry::Syscall {
                        syscall: Sysno::sched_yield,
                        occurrence: index as u64 + 1
                    }
                );
                assert_eq!(boundary.identity.slice, index as u64 + 1);
                assert_eq!(boundary.identity.checkin, index as u64 * 2 + 1);
                assert_eq!(boundary.identity.committed_rcbs, 0);
                assert_eq!(boundary.count, 1);
            }
            assert_eq!(
                scheduler.recorded_event_count,
                u64::try_from(records.len()).unwrap()
            );
            if input.is_some() && !chaos {
                assert_eq!(
                    scheduler
                        .generate_partial_run_summary(None)
                        .unwrap()
                        .schedevent_replayed,
                    2
                );
            }
        } else {
            assert_eq!(scheduler.recorded_event_count, 0);
        }
        (
            recording,
            scheduler.committed_time,
            guest.thread.thread_logical_time.as_nanos(),
            scheduler.turn,
            guest.thread.stats.timeslice_count,
        )
    }

    #[tokio::test]
    async fn boundary_actual_sched_yield_handler_replays_with_and_without_timers() {
        let directory = tempfile::tempdir().unwrap();
        for timers in [false, true] {
            let recorded = boundary_sched_yield_handler(None, true, false, timers, false).await;
            let input = directory.path().join(format!("{timers}.json"));
            recorded.0.write(&input).unwrap();
            let replayed =
                boundary_sched_yield_handler(Some(&input), true, false, timers, false).await;
            // Complete metadata, identities, clocks, turns and reset counts.
            assert_eq!(recorded, replayed);
        }
    }

    #[tokio::test]
    async fn boundary_actual_sched_yield_handler_preserves_legacy_and_chaos_weak_yield() {
        use crate::preemptions::boundary_recording::Recording;
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("legacy.json");
        Recording::Legacy(PreemptionRecord::from_sched_events(vec![]))
            .write(&input)
            .unwrap();
        for timers in [false, true] {
            boundary_sched_yield_handler(Some(&input), false, false, timers, false).await;
        }
        // Even a typed observation state must not intercept the chaos route.
        // A configured maximum makes this the existing WeakYield chaos arm.
        boundary_sched_yield_handler(Some(&input), false, true, true, false).await;
        boundary_sched_yield_handler(Some(&input), true, true, true, false).await;
    }

    async fn boundary_bad_yield_replay(kind: &str) {
        use detcore_model::schedule::boundary::Cause;
        use detcore_model::schedule::boundary::Entry;
        use detcore_model::schedule::boundary::Record;
        use detcore_model::schedule::boundary::Schedule;

        use crate::preemptions::boundary_recording::BoundaryRecording;
        use crate::preemptions::boundary_recording::Recording;

        let recorded = boundary_sched_yield_handler(None, true, false, false, false)
            .await
            .0;
        let Recording::Boundaries(recorded) = recorded else {
            panic!("not typed")
        };
        let mut records = recorded.schedule().records().to_vec();
        records.truncate(1);
        if kind == "missing" {
            records.clear();
        } else if kind != "owner" {
            let Record::TimesliceEnd(boundary) = &mut records[0] else {
                unreachable!()
            };
            match kind {
                "cause" => boundary.identity.cause = Cause::SoftwareCheckin,
                "occurrence" => {
                    boundary.identity.entry = Entry::Syscall {
                        syscall: Sysno::sched_yield,
                        occurrence: 2,
                    }
                }
                _ => panic!("unknown negative fixture"),
            }
        }
        let input = tempfile::tempdir().unwrap();
        let path = input.path().join("bad.json");
        Recording::Boundaries(
            BoundaryRecording::new(
                PreemptionRecord::from_sched_events(vec![]),
                Schedule::new(records).unwrap(),
            )
            .unwrap(),
        )
        .write(&path)
        .unwrap();
        boundary_sched_yield_handler(Some(&path), true, false, false, kind == "owner").await;
        panic!("actual sched_yield handler accepted {kind}");
    }

    #[tokio::test]
    async fn boundary_actual_sched_yield_handler_refuses_missing_marker() {
        if in_child_expecting_refusal(
            "boundary_actual_sched_yield_handler_refuses_missing_marker",
            "Exhausted",
        ) {
            boundary_bad_yield_replay("missing").await;
        }
    }

    #[tokio::test]
    async fn boundary_actual_sched_yield_handler_refuses_wrong_cause() {
        if in_child_expecting_refusal(
            "boundary_actual_sched_yield_handler_refuses_wrong_cause",
            "expected Identity",
        ) {
            boundary_bad_yield_replay("cause").await;
        }
    }

    #[tokio::test]
    async fn boundary_actual_sched_yield_handler_refuses_wrong_occurrence() {
        if in_child_expecting_refusal(
            "boundary_actual_sched_yield_handler_refuses_wrong_occurrence",
            "expected Identity",
        ) {
            boundary_bad_yield_replay("occurrence").await;
        }
    }

    #[tokio::test]
    async fn boundary_actual_sched_yield_handler_refuses_stale_owner() {
        if in_child_expecting_refusal(
            "boundary_actual_sched_yield_handler_refuses_stale_owner",
            "stale observation owner",
        ) {
            boundary_bad_yield_replay("owner").await;
        }
    }

    #[test]
    fn boundary_resource_union_refuses_either_annotated_operand_before_mutation() {
        let tid = DetTid::from_raw(17);
        let config = Config::default();
        let mut thread = crate::ThreadState::new(tid, &config, ());
        let mut observed = crate::slice_boundary::Observations::new(&thread, 0);
        observed
            .callback(
                &thread,
                detcore_model::schedule::boundary::Entry::Cpuid,
                detcore_model::schedule::boundary::Phase::Prehook,
            )
            .unwrap();
        let boundary = observed
            .close_slice(&mut thread, &config, false, None)
            .unwrap()
            .boundary;
        for side in 0..2 {
            let mut left = Resources::new(tid);
            let mut right = Resources::new(tid);
            right.insert(ResourceID::MemAddrSpace(tid), Permission::RW);
            if side == 0 {
                left.slice_boundary = Some(Box::new(boundary.clone()));
            } else {
                right.slice_boundary = Some(Box::new(boundary.clone()));
            }
            let annotated = if side == 0 { &left } else { &right };
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || annotated.upgraded_to_runnable()
                ))
                .is_err()
            );
            let before = left.clone();
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| left.union(&right)))
                    .is_err()
            );
            assert_eq!(left, before);
        }
    }

    async fn boundary_grant_roundtrip(
        route: u8,
        with_budget: bool,
        automatic: bool,
    ) -> (
        Resources,
        LogicalTime,
        LogicalTime,
        u64,
        (Option<LogicalTime>, Option<LogicalTime>, u64),
    ) {
        let input = tempfile::tempdir().unwrap();
        let (config, state, tid, process) = if automatic {
            use detcore_model::schedule::boundary::Boundary;
            use detcore_model::schedule::boundary::Record;
            use detcore_model::schedule::boundary::Schedule;

            use crate::preemptions::boundary_recording::BoundaryRecording;
            use crate::preemptions::boundary_recording::Recording;
            let tid = DetTid::from_raw(17);
            let budget = crate::slice_boundary::grant::tests::budget(tid, MmId::initial(tid), 91);
            let recording = Recording::Boundaries(
                BoundaryRecording::new(
                    PreemptionRecord::from_sched_events(vec![]),
                    Schedule::new(vec![Record::TimesliceEnd(Boundary {
                        identity: budget.expected(),
                        count: 1,
                        start_rip: None,
                        end_rip: None,
                        end_time: None,
                    })])
                    .unwrap(),
                )
                .unwrap(),
            );
            let path = input.path().join("recording.json");
            recording.write(&path).unwrap();
            cancellation_test_state_with_config(
                Config {
                    sequentialize_threads: true,
                    replay_schedule_from: Some(path),
                    strict_schedule_boundaries: true,
                    ..Config::default()
                }
                .with_backend(|backend| backend.needs_killed_thread_rpc_cancellation = true),
            )
        } else {
            cancellation_test_state()
        };
        install_test_registration(&state, tid, Ivar::new());
        let mut thread = crate::ThreadState::new(tid, &config, ());
        thread.detpid = Some(process);
        if route != 1 {
            thread.next_timeslice(&config);
        }
        thread.slice_boundary = Some(crate::slice_boundary::Observations::new(&thread, 0));
        let budget = crate::slice_boundary::grant::tests::budget(tid, thread.mm_id, 91);
        if with_budget {
            let mut scheduler = state.sched.lock().unwrap();
            let mut owners = crate::slice_boundary::lifecycle::Registry::default();
            owners.fresh(tid, thread.mm_id).unwrap();
            scheduler.boundary_owners = Some(owners);
            if automatic {
                scheduler.record_thread_mm(tid, thread.mm_id);
                assert_eq!(
                    scheduler.boundary_budget_for_live_owner(tid).unwrap(),
                    Some(budget)
                );
            } else {
                scheduler
                    .boundary_budgets
                    .queue(thread.mm_id, budget)
                    .unwrap();
            }
            if route == 1 {
                // The real startup reply must initialize from lifecycle
                // authority before it stages the separate expected budget.
                thread.slice_boundary = None;
            }
        }
        let original_slices = thread.stats.timeslice_count;
        let original_maximum = thread.max_timeslice_end;
        let mut guest = ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: false,
            ppid: None,
            private_memory: None,
        };
        let mut request = Box::pin(async {
            match route {
                0 => {
                    let mut resources = Resources::new(tid);
                    resources.insert(ResourceID::MemAddrSpace(process), Permission::RW);
                    assert_eq!(
                        super::resource_request(&mut guest, resources).await,
                        ResumeStatus::Normal
                    );
                }
                1 => assert_eq!(
                    super::thread_start_request(&config, &mut guest, process).await,
                    None
                ),
                2 => {
                    let response =
                        super::send_and_update_time(&mut guest, GlobalRequest::ResumeExec(process))
                            .await;
                    assert_eq!(
                        response.1,
                        GlobalResponse::ResumeExec(ResumeStatus::Normal, None)
                    );
                }
                _ => unreachable!(),
            }
        });
        for _ in 0..8 {
            assert!(futures::poll!(&mut request).is_pending());
            if state.sched.lock().unwrap().next_turns[&tid]
                .req
                .try_read()
                .is_some()
            {
                break;
            }
        }
        assert!(
            state.sched.lock().unwrap().next_turns[&tid]
                .req
                .try_read()
                .is_some()
        );
        let turn = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .expect("the original resource turn must commit");
        request.await;
        if route == 1 {
            // The actual startup continuation runs the reset AFTER the RPC,
            // with its original history/cap order. Its budget must still be
            // pending here, not armed and then erased by next_timeslice.
            assert_eq!(guest.thread.replay_rcb_end, None);
            assert_eq!(guest.thread.stats.timeslice_count, original_slices);
            super::finish_thread_start(&config, &mut guest, None);
        }
        assert_eq!(guest.thread.replay_rcb_end, with_budget.then_some(91));
        assert_eq!(
            guest.thread.stats.timeslice_count,
            original_slices + u64::from(route == 1)
        );
        if route != 1 {
            assert_eq!(guest.thread.max_timeslice_end, original_maximum);
        }
        let scheduler = state.sched.lock().unwrap();
        (
            turn,
            scheduler.committed_time,
            guest.thread.thread_logical_time.as_nanos(),
            scheduler.turn,
            (
                guest.thread.max_timeslice_end,
                guest.thread.end_of_timeslice,
                guest.thread.stats.timeslice_count,
            ),
        )
    }

    #[tokio::test]
    async fn boundary_budget_survives_real_resource_start_and_exec_resume_rpc_without_an_extra_turn()
     {
        for route in 0..3 {
            assert_eq!(
                boundary_grant_roundtrip(route, true, false).await,
                boundary_grant_roundtrip(route, false, false).await
            );
        }
    }

    #[tokio::test]
    async fn boundary_loaded_cursor_delivers_only_through_the_existing_grant_and_start_reset() {
        for route in 0..3 {
            assert_eq!(
                boundary_grant_roundtrip(route, true, true).await,
                boundary_grant_roundtrip(route, false, false).await,
            );
        }
    }

    // Exercise external registration and post-exec through the real global RPC
    // without a backend or guest process. Unexpected guest operations fail.
    struct ExternalRegistrationGuest<'a> {
        global: &'a GlobalState,
        config: &'a Config,
        thread: crate::ThreadState<()>,
        requests: Mutex<Vec<GlobalRequest>>,
        post_exec: bool,
        ppid: Option<reverie::Pid>,
        private_memory: Option<reverie::RuntimePrivateMemory>,
    }

    struct ExternalRegistrationStack;
    struct ExternalRegistrationStackGuard;

    impl Drop for ExternalRegistrationStackGuard {
        fn drop(&mut self) {}
    }

    impl reverie::Stack for ExternalRegistrationStack {
        type StackGuard = ExternalRegistrationStackGuard;

        fn size(&self) -> usize {
            panic!("external registration must not use a guest stack")
        }

        fn capacity(&self) -> usize {
            panic!("external registration must not use a guest stack")
        }

        fn push<'stack, T>(&mut self, _value: T) -> reverie::syscalls::Addr<'stack, T> {
            panic!("external registration must not use a guest stack")
        }

        fn reserve<'stack, T>(&mut self) -> reverie::syscalls::AddrMut<'stack, T> {
            panic!("external registration must not use a guest stack")
        }

        fn commit(self) -> Result<Self::StackGuard, reverie::syscalls::Errno> {
            panic!("external registration must not use a guest stack")
        }
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for ExternalRegistrationGuest<'_> {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            self.requests.lock().unwrap().push(message.2.clone());
            self.global
                .receive_rpc(Tid::from_raw(self.thread.dettid.as_raw()), message)
                .await
        }

        fn config(&self) -> &Config {
            self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for ExternalRegistrationGuest<'_> {
        type Memory = reverie::syscalls::LocalMemory;
        type Stack = ExternalRegistrationStack;

        fn tid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.dettid.as_raw())
        }

        fn pid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.detpid.unwrap().as_raw())
        }

        fn ppid(&self) -> Option<reverie::Pid> {
            self.ppid
        }

        fn runtime_private_memory_regions(&self) -> Option<reverie::RuntimePrivateMemory> {
            self.private_memory.clone()
        }

        fn auxv(&self) -> reverie::Auxv {
            assert!(self.post_exec, "external registration must not read auxv");
            reverie::Auxv::from_entries([])
        }

        fn memory(&self) -> Self::Memory {
            panic!("external registration must not access guest memory")
        }

        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
            &mut self.thread
        }

        fn thread_state(&self) -> &crate::ThreadState<()> {
            &self.thread
        }

        async fn regs(&mut self) -> libc::user_regs_struct {
            assert!(
                self.post_exec,
                "external registration must not read guest registers"
            );
            // SAFETY: every field is an integer register word. Post-exec hooks
            // may log these synthetic registers but never execute them.
            unsafe { std::mem::zeroed() }
        }

        async fn stack(&mut self) -> Self::Stack {
            panic!("external registration must not use a guest stack")
        }

        async fn daemonize(&mut self) {
            panic!("external registration must not daemonize")
        }

        async fn inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> Result<i64, reverie::syscalls::Errno> {
            panic!("external registration must not inject a syscall")
        }

        async fn tail_inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> reverie::Never {
            panic!("external registration unexpectedly retired its live parent")
        }

        fn set_timer(&mut self, _schedule: reverie::TimerSchedule) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn set_timer_precise(
            &mut self,
            _schedule: reverie::TimerSchedule,
        ) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn read_clock(&mut self) -> Result<u64, reverie::Error> {
            panic!("external registration must not read a host clock")
        }
    }

    struct RetirementGuest<'a> {
        global: &'a GlobalState,
        config: &'a Config,
        thread: crate::ThreadState<()>,
        requests: Mutex<Vec<GlobalRequest>>,
        retired: std::sync::atomic::AtomicBool,
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for RetirementGuest<'_> {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            self.requests.lock().unwrap().push(message.2.clone());
            self.global
                .receive_rpc(Tid::from_raw(self.thread.dettid.as_raw()), message)
                .await
        }

        fn config(&self) -> &Config {
            self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for RetirementGuest<'_> {
        type Memory = reverie::syscalls::LocalMemory;
        type Stack = ExternalRegistrationStack;

        fn tid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.dettid.as_raw())
        }

        fn pid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.detpid.unwrap().as_raw())
        }

        fn ppid(&self) -> Option<reverie::Pid> {
            None
        }

        fn memory(&self) -> Self::Memory {
            panic!("external registration must not access guest memory")
        }

        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
            &mut self.thread
        }

        fn thread_state(&self) -> &crate::ThreadState<()> {
            &self.thread
        }

        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("external registration must not read guest registers")
        }

        async fn stack(&mut self) -> Self::Stack {
            panic!("external registration must not use a guest stack")
        }

        async fn daemonize(&mut self) {
            panic!("external registration must not daemonize")
        }

        async fn inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> Result<i64, reverie::syscalls::Errno> {
            panic!("external registration must not inject a syscall")
        }

        async fn retire_current_thread(&mut self) -> reverie::Never {
            self.retired
                .store(true, std::sync::atomic::Ordering::SeqCst);
            futures::future::pending().await
        }

        async fn cancel_current_thread(&mut self) -> reverie::Never {
            panic!("scheduler retirement must not request group cancellation")
        }

        async fn tail_inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> reverie::Never {
            panic!("external registration unexpectedly retired its live parent")
        }

        fn set_timer(&mut self, _schedule: reverie::TimerSchedule) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn set_timer_precise(
            &mut self,
            _schedule: reverie::TimerSchedule,
        ) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn read_clock(&mut self) -> Result<u64, reverie::Error> {
            panic!("external registration must not read a host clock")
        }
    }

    /// While the caller's own process handles SIGALRM, every SIGALRM send and
    /// producer arming the handlers check is refused with EPERM before the
    /// kernel: the guest injects nothing (`RetirementGuest` panics on an
    /// injection) and sends no RPC but the ledger's control message.
    #[tokio::test]
    async fn sigalrm_sends_and_producer_armings_are_refused_before_the_kernel() {
        use reverie::Tool;
        use reverie::syscalls;

        let (config, state, tid, pid) = cancellation_test_state();
        install_test_registration(&state, tid, Ivar::new());
        {
            let mut sched = state.sched.lock().unwrap();
            sched.set_sigalrm_handled(pid, true);
            sched.set_running_for_test(tid);
        }
        let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(tid.as_raw()), None);
        thread.detpid = Some(pid);
        let mut guest = RetirementGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            retired: std::sync::atomic::AtomicBool::new(false),
        };
        let (raw_pid, raw_tid) = (pid.as_raw(), tid.as_raw());
        let eperm = |result: Result<i64, reverie::Error>, what: &str| {
            assert!(
                matches!(result, Err(reverie::Error::Errno(errno)) if errno == reverie::Errno::EPERM),
                "{what}: {result:?}"
            );
        };

        let sigalrm = libc::SIGALRM;
        let tgkill = syscalls::Tgkill::new()
            .with_tgid(raw_pid)
            .with_tid(raw_tid)
            .with_sig(sigalrm);
        eperm(tool.handle_tgkill(&mut guest, tgkill).await, "tgkill");
        let tkill = syscalls::Tkill::new().with_tid(raw_tid).with_sig(sigalrm);
        eperm(tool.handle_tkill(&mut guest, tkill).await, "tkill");
        let kill = syscalls::Kill::new().with_pid(raw_pid).with_sig(sigalrm);
        eperm(tool.handle_kill(&mut guest, kill).await, "kill");
        let queue = syscalls::RtSigqueueinfo::new()
            .with_tgid(raw_pid)
            .with_sig(sigalrm);
        eperm(
            tool.handle_rt_sigqueueinfo(&mut guest, queue).await,
            "rt_sigqueueinfo",
        );
        let tgqueue = syscalls::RtTgsigqueueinfo::new()
            .with_tgid(raw_pid)
            .with_tid(raw_tid)
            .with_sig(sigalrm);
        eperm(
            tool.handle_rt_tgsigqueueinfo(&mut guest, tgqueue).await,
            "rt_tgsigqueueinfo",
        );
        let pdeathsig = syscalls::Prctl::new()
            .with_option(libc::PR_SET_PDEATHSIG)
            .with_arg2(sigalrm as libc::c_ulong);
        eperm(
            tool.handle_prctl(&mut guest, pdeathsig).await,
            "PR_SET_PDEATHSIG",
        );
        // A NULL sigevent names SIGALRM; the id pointer is never written.
        let timer = syscalls::TimerCreate::new()
            .with_clockid(syscalls::ClockId::CLOCK_MONOTONIC)
            .with_timerid(reverie::syscalls::AddrMut::from_raw(0x1000));
        eperm(
            tool.handle_timer_create(&mut guest, timer).await,
            "timer_create",
        );

        let requests = guest.requests.lock().unwrap();
        assert_eq!(requests.len(), 7);
        assert!(
            requests
                .iter()
                .all(|request| matches!(request, GlobalRequest::Sigalrm(_))),
            "{requests:?}"
        );
    }

    /// In a process that handles SIGALRM, signal phase 1's gate refuses what
    /// its table does not place, with EOPNOTSUPP, and allows what it places,
    /// with no RPC either way.
    #[tokio::test]
    async fn sigalrm_phase1_gate_refuses_unplaced_calls() {
        use reverie::Tool;
        use reverie::syscalls::Syscall;
        use reverie::syscalls::SyscallArgs;
        use reverie::syscalls::Sysno;

        let (config, state, tid, pid) = cancellation_test_state();
        install_test_registration(&state, tid, Ivar::new());
        let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(tid.as_raw()), None);
        thread.detpid = Some(pid);
        thread.sigalrm_handled = true;
        let guest = RetirementGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            retired: std::sync::atomic::AtomicBool::new(false),
        };
        let raw = |sysno, a0, a1| Syscall::from_raw(sysno, SyscallArgs::new(a0, a1, 0, 0, 0, 0));

        for (sysno, a0, a1) in [
            (Sysno::rt_sigsuspend, 0x1000, 8),
            (Sysno::signalfd4, usize::MAX, 0x1000),
            (Sysno::epoll_pwait2, 3, 0),
            (Sysno::nanosleep, 0x1000, 0),
            (Sysno::fcntl, 3, libc::F_SETLKW as usize),
            (Sysno::execve, 0x1000, 0),
            // stdin, and a descriptor Detcore does not model.
            (Sysno::read, 0, 0x1000),
            (Sysno::read, 77, 0x1000),
            // stderr, which may be a socket Detcore labels Regular.
            (Sysno::close, 2, 0),
            // No ioctl is placed.
            (Sysno::ioctl, 1, libc::TCGETS as usize),
            // Buffered credentials and native process relations remain outside
            // the four scalar identity queries.
            (Sysno::getresuid, 0x1000, 0x2000),
            (Sysno::getresgid, 0x1000, 0x2000),
            (Sysno::getgroups, 1, 0x1000),
            (Sysno::getppid, 0, 0),
            (Sysno::getpgid, 0, 0),
            (Sysno::getsid, 0, 0),
        ] {
            assert_eq!(
                tool.sigalrm_phase1_gate(&guest, raw(sysno, a0, a1)),
                Some(nix::errno::Errno::EOPNOTSUPP),
                "{sysno}"
            );
        }
        for (sysno, a0, a1) in [
            (Sysno::getpid, 0, 0),
            (Sysno::write, 1, 0x1000),
            (Sysno::getuid, 0, 0),
            (Sysno::geteuid, 0, 0),
            (Sysno::getgid, 0, 0),
            (Sysno::getegid, 0, 0),
        ] {
            assert_eq!(
                tool.sigalrm_phase1_gate(&guest, raw(sysno, a0, a1)),
                None,
                "{sysno}"
            );
        }
        assert!(guest.requests.lock().unwrap().is_empty(), "no RPC");
    }

    /// While any process handles SIGALRM, a file lease is refused before the
    /// kernel: its break would make another process's open wait with no
    /// scheduler admission. Releasing a lease is not refused.
    #[tokio::test]
    async fn a_file_lease_is_refused_while_a_guest_handles_sigalrm() {
        use reverie::Tool;
        use reverie::syscalls;
        use reverie::syscalls::FcntlCmd;

        let (config, state, tid, pid) = cancellation_test_state();
        install_test_registration(&state, tid, Ivar::new());
        {
            let mut sched = state.sched.lock().unwrap();
            sched.set_sigalrm_handled(DetPid::from_raw(99), true);
            sched.set_running_for_test(tid);
        }
        let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(tid.as_raw()), None);
        thread.detpid = Some(pid);
        let mut guest = RetirementGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            retired: std::sync::atomic::AtomicBool::new(false),
        };
        for lease in [libc::F_RDLCK, libc::F_WRLCK] {
            let fcntl = syscalls::Fcntl::new()
                .with_fd(3)
                .with_cmd(FcntlCmd::F_SETLEASE(lease));
            let result = tool.handle_fcntl(&mut guest, fcntl).await;
            assert!(
                matches!(result, Err(reverie::Error::Errno(errno)) if errno == reverie::Errno::EPERM),
                "{lease}: {result:?}"
            );
        }
        let requests = guest.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| matches!(
            request,
            GlobalRequest::Sigalrm(super::SigalrmControl::ArmProducer)
        )));
    }

    /// A committed SIGKILL victim's late ordinary request is never answered
    /// (https://github.com/rrnewton/hermit/issues/3994, design revision 4,
    /// R1). The two late states are a background operation's rejoin posted
    /// after the commit (`BlockedExternalContinue`, through
    /// `resource_request`, as `helpers.rs` posts it once the operation's
    /// outcome is in) and a plain request. In both, the callback stays
    /// pending: no response, no injection (`RetirementGuest` panics on one),
    /// and no `retire_current_thread`, so the backend's terminal owner
    /// consumes the real death. Its consuming cleanup is still answered.
    #[tokio::test]
    async fn a_sigkilled_threads_late_ordinary_request_stays_unanswered() {
        use std::sync::atomic::Ordering;

        use reverie::Tool;

        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let [root, sender, victim] = [1, 2, 3].map(DetPid::from_raw);
        let commit = |state: &GlobalState| {
            let mut sched = state.sched.lock().unwrap();
            let token = sched.reserve_sigkill(sender, victim, 0).unwrap().token;
            assert_eq!(sched.commit_sigkill(token, true).len(), 1);
            assert!(sched.is_unaccounted_sigkill_victim(victim));
        };
        // "filed": the rejoin is admitted and waiting for its turn when the
        // commit happens. "rejoin" and "plain" arrive after the commit.
        for late in ["filed", "rejoin", "plain"] {
            let state = GlobalState::initialize(&config, false);
            {
                let mut sched = state.sched.lock().unwrap();
                sched.thread_tree.add_child(root, root, true);
                sched.thread_tree.add_child(root, sender, true);
                sched.thread_tree.add_child(root, victim, true);
            }
            for tid in [root, sender, victim] {
                install_test_registration(&state, tid, Ivar::new());
                state
                    .sched
                    .lock()
                    .unwrap()
                    .record_thread_mm(tid, MmId::initial(tid));
            }
            if late != "filed" {
                commit(&state);
            }
            let tool: Detcore = Detcore::new(Tid::from_raw(victim.as_raw()), &config);
            let mut thread = tool.init_thread_state(Tid::from_raw(victim.as_raw()), None);
            thread.detpid = Some(victim);
            let mut guest = RetirementGuest {
                global: &state,
                config: &config,
                thread,
                requests: Mutex::new(Vec::new()),
                retired: std::sync::atomic::AtomicBool::new(false),
            };
            {
                let mut rejoin = Resources::new(victim);
                rejoin.insert(
                    ResourceID::BlockedExternalContinue(ExternalOpId::new(victim, 7)),
                    Permission::RW,
                );
                let pending = if late == "plain" {
                    let mut call = std::pin::pin!(super::send_and_update_time(
                        &mut guest,
                        GlobalRequest::GlobalTimeLowerBound
                    ));
                    futures::poll!(call.as_mut()).is_pending()
                } else {
                    let mut call = std::pin::pin!(super::resource_request(&mut guest, rejoin));
                    let mut pending = futures::poll!(call.as_mut()).is_pending();
                    if late == "filed" {
                        commit(&state);
                        for _ in 0..8 {
                            tokio::task::yield_now().await;
                            pending &= futures::poll!(call.as_mut()).is_pending();
                        }
                    }
                    pending
                };
                assert!(pending, "{late}: the callback resumed");
            }
            assert!(
                !guest.retired.load(Ordering::SeqCst),
                "{late}: retire_current_thread tail-injects exit"
            );
            assert_eq!(guest.requests.lock().unwrap().len(), 1, "{late}");
            // The consuming cleanup of the same victim is admitted and answered.
            let (_, ack) = state
                .receive_rpc(
                    Tid::from_raw(victim.as_raw()),
                    (
                        guest.thread.thread_logical_time.clone(),
                        guest.thread.mm_id,
                        GlobalRequest::RobustListWakes(Vec::new()),
                    ),
                )
                .await;
            assert_eq!(ack, GlobalResponse::RobustListWakes(Vec::new()), "{late}");
            // Requests answered before ordinary admission get no answer either:
            // a SIGALRM ledger change is refused, and a loss notice is
            // recorded but not answered.
            for request in [
                GlobalRequest::Sigalrm(super::SigalrmControl::ArmProducer),
                GlobalRequest::RecordDeterminismLoss("victim notice".to_string()),
            ] {
                let (_, response) = state
                    .receive_rpc(
                        Tid::from_raw(victim.as_raw()),
                        (
                            guest.thread.thread_logical_time.clone(),
                            guest.thread.mm_id,
                            request,
                        ),
                    )
                    .await;
                assert_eq!(response, GlobalResponse::Retired, "{late}");
            }
        }
    }

    /// A victim's request admitted before the commit whose response is
    /// granted, and observed by the handler, only after the commit reaches the
    /// handler's single response exit, which must not answer it
    /// (https://github.com/rrnewton/hermit/issues/3994, R1). Without that exit
    /// guard the granted response would resume the callback.
    #[tokio::test]
    async fn a_victims_request_granted_across_the_commit_stays_unanswered() {
        use reverie::Tool;

        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let [root, sender, victim] = [1, 2, 3].map(DetPid::from_raw);
        let state = GlobalState::initialize(&config, false);
        {
            let mut sched = state.sched.lock().unwrap();
            sched.thread_tree.add_child(root, root, true);
            sched.thread_tree.add_child(root, sender, true);
            sched.thread_tree.add_child(root, victim, true);
        }
        for tid in [root, sender, victim] {
            install_test_registration(&state, tid, Ivar::new());
            state
                .sched
                .lock()
                .unwrap()
                .record_thread_mm(tid, MmId::initial(tid));
        }
        let tool: Detcore = Detcore::new(Tid::from_raw(victim.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(victim.as_raw()), None);
        thread.detpid = Some(victim);
        let mut guest = RetirementGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            retired: std::sync::atomic::AtomicBool::new(false),
        };
        let mut rejoin = Resources::new(victim);
        rejoin.insert(
            ResourceID::BlockedExternalContinue(ExternalOpId::new(victim, 7)),
            Permission::RW,
        );
        let response = state.sched.lock().unwrap().next_turns[&victim].resp.clone();
        {
            let mut call = std::pin::pin!(super::resource_request(&mut guest, rejoin));
            // Admitted and filed: the handler waits for the grant.
            assert!(futures::poll!(call.as_mut()).is_pending());
            // The scheduler grants it, and only then does the commit retire the
            // victim, before the handler has looked at the grant.
            response.put(SchedResponse::Go(None));
            {
                let mut sched = state.sched.lock().unwrap();
                let token = sched.reserve_sigkill(sender, victim, 0).unwrap().token;
                assert_eq!(sched.commit_sigkill(token, true).len(), 1);
            }
            for _ in 0..8 {
                tokio::task::yield_now().await;
                assert!(
                    futures::poll!(call.as_mut()).is_pending(),
                    "the granted response resumed a SIGKILLed thread's callback"
                );
            }
        }
        assert!(!guest.retired.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Runs the calling test's body in a child copy of this test binary, and
    /// requires the child to exit with the policy-refusal status and print
    /// `expected`. Returns whether this is the child, which runs the body.
    /// Used where the body ends the process by refusing.
    fn in_child_expecting_refusal(name: &str, expected: &str) -> bool {
        const CHILD: &str = "HERMIT_TEST_REFUSAL_CHILD";
        let test = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
        if std::env::var(CHILD).as_deref() == Ok(test.as_str()) {
            return true;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD, &test)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            "{test}: the child was not refused\n{stderr}"
        );
        assert!(
            stderr.contains(expected),
            "{test}: missing {expected:?}\n{stderr}"
        );
        false
    }

    /// Sends the DBT SIGKILL notice for process 3, from process 1, with a
    /// 20 ms fence valve. Process 3's thread is registered on the host as
    /// `victim`'s, when given, and otherwise only modelled.
    async fn send_dbt_sigkill_notice(victim: Option<i32>) {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        }
        .with_backend(|backend| *backend = reverie::BackendCapabilities::DBT);
        let state = GlobalState::initialize(&config, false);
        let [root, target] = [1, 3].map(DetPid::from_raw);
        {
            let mut sched = state.sched.lock().unwrap();
            sched.thread_tree.add_child(root, root, true);
            sched.thread_tree.add_child(root, target, true);
        }
        for tid in [root, target] {
            install_test_registration(&state, tid, Ivar::new());
            state
                .sched
                .lock()
                .unwrap()
                .record_thread_mm(tid, MmId::initial(tid));
        }
        {
            let mut sched = state.sched.lock().unwrap();
            sched.sigkill_fence_valve = std::time::Duration::from_millis(20);
            if let Some(pid) = victim {
                sched
                    .register_physical_thread(target, MmId::initial(target), pid, pid)
                    .unwrap();
            }
        }
        let rpc = state.receive_rpc(
            Tid::from_raw(root.as_raw()),
            (
                crate::ThreadState::<()>::new(root, &config, ())
                    .thread_logical_time
                    .clone(),
                MmId::initial(root),
                GlobalRequest::NotifySignalPending(
                    target,
                    SigWrapper(libc::SIGKILL),
                    Some(target),
                    super::SignalScope::Process,
                ),
            ),
        );
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), rpc).await;
    }

    /// On DBT, a SIGKILL notice whose victim is not gone within the fence
    /// valve ends the run with the named refusal before the sender is
    /// answered (shortened valve; Claude re-check of
    /// https://github.com/rrnewton/hermit/pull/4423, N3).
    #[tokio::test]
    async fn a_dbt_sigkill_notice_past_its_valve_refuses_before_answering() {
        if !in_child_expecting_refusal(
            "a_dbt_sigkill_notice_past_its_valve_refuses_before_answering",
            "cannot be held until it is gone: it was not gone within",
        ) {
            return;
        }
        // The victim outlives the valve by far, and dies with this child
        // (PR_SET_PDEATHSIG) rather than outliving it (Claude re-check of
        // 4423 at 858d81ec, N5). Its stdio is not this child's, which the
        // parent reads to the end.
        let mut victim = std::process::Command::new("sleep");
        victim
            .arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: prctl is async-signal-safe and touches no shared state.
        unsafe {
            use std::os::unix::process::CommandExt;
            victim.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let mut victim = victim.spawn().unwrap();
        send_dbt_sigkill_notice(Some(victim.id() as i32)).await;
        let _ = victim.kill();
        let _ = victim.wait();
        panic!("the SIGKILL notice answered its sender although the victim was not gone");
    }

    /// On DBT, a SIGKILL notice for a victim with a live thread but no thread
    /// registered on the host ends the run with the named refusal before the
    /// sender is answered, never left unfenced (Claude review of
    /// https://github.com/rrnewton/hermit/pull/4423, F2: the end-to-end form
    /// of this case became a passing run with
    /// https://github.com/rrnewton/hermit/issues/4411's fix).
    #[tokio::test]
    async fn a_dbt_sigkill_notice_with_no_host_registration_refuses_before_answering() {
        if !in_child_expecting_refusal(
            "a_dbt_sigkill_notice_with_no_host_registration_refuses_before_answering",
            "nor any other thread of it has a registered host process",
        ) {
            return;
        }
        send_dbt_sigkill_notice(None).await;
        panic!("the SIGKILL notice answered its sender without a fence");
    }

    /// A returning sender's wait for its victims is bounded by the valve: a
    /// commit whose victim never dies returns once the valve expires and
    /// refuses the run by name, instead of hanging the sender (shortened
    /// valve; the 30 s production bound is the same field).
    #[tokio::test]
    async fn a_sigkill_commit_wait_past_its_valve_refuses_the_run() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let [root, sender, target] = [1, 2, 3].map(DetPid::from_raw);
        {
            let mut sched = state.sched.lock().unwrap();
            sched.thread_tree.add_child(root, root, true);
            sched.thread_tree.add_child(root, sender, true);
            sched.thread_tree.add_child(root, target, true);
        }
        for tid in [root, sender, target] {
            install_test_registration(&state, tid, Ivar::new());
            state
                .sched
                .lock()
                .unwrap()
                .record_thread_mm(tid, MmId::initial(tid));
        }
        let token = {
            let mut sched = state.sched.lock().unwrap();
            sched.sigkill_fence_valve = std::time::Duration::from_millis(20);
            sched.reserve_sigkill(sender, target, 0).unwrap().token
        };
        let rpc = state.receive_rpc(
            Tid::from_raw(sender.as_raw()),
            (
                crate::ThreadState::<()>::new(sender, &config, ())
                    .thread_logical_time
                    .clone(),
                MmId::initial(sender),
                GlobalRequest::CommitSigkill(token, true),
            ),
        );
        let (_, response) = tokio::time::timeout(std::time::Duration::from_secs(10), rpc)
            .await
            .expect("the commit's wait outlived its valve");
        assert_eq!(response, GlobalResponse::CommitSigkill(()));
        let sched = state.sched.lock().unwrap();
        let refusal = sched
            .sigkill_fence_refusal_for_test()
            .expect("no refusal recorded");
        assert!(refusal.waited >= std::time::Duration::from_millis(20));
        assert_eq!(refusal.unaccounted, [target]);
    }

    /// Two victims of one SIGKILL send their owner-death batches through this
    /// real RPC handler, as authenticated registered victims, in either order,
    /// with a step2 between the receipts (3994 design revision 4, R2). Each
    /// batch is acknowledged with no counts and runs only when the cohort is
    /// complete, so the waiters woken, the run-queue order and the fuzz arm's
    /// next draw do not depend on the order, under FIFO and under the fuzz
    /// arm. This covers the retention protocol, not a SIGKILL producer: a
    /// SIGKILL victim stages no batch today
    /// (<https://github.com/rrnewton/hermit/issues/2082>).
    #[tokio::test]
    async fn victims_owner_death_wakes_through_the_handler_do_not_depend_on_receipt_order() {
        async fn run(fuzz: bool, reverse: bool) -> (Vec<Vec<DetTid>>, Vec<DetTid>, u64) {
            let config = Config {
                sequentialize_threads: true,
                fuzz_futexes: fuzz,
                ..Config::default()
            };
            let state = GlobalState::initialize(&config, false);
            let [root, sender, target, worker] = [1, 2, 3, 4].map(DetPid::from_raw);
            let mm = MmId::initial(target);
            let futexes = [0x404100, 0x404200].map(|addr| FutexID::private(mm, addr));
            {
                let mut sched = state.sched.lock().unwrap();
                sched.thread_tree.add_child(root, root, true);
                sched.thread_tree.add_child(root, sender, true);
                sched.thread_tree.add_child(root, target, true);
                sched.thread_tree.add_child(target, worker, false);
            }
            for tid in [root, sender, target, worker] {
                install_test_registration(&state, tid, Ivar::new());
                state
                    .sched
                    .lock()
                    .unwrap()
                    .record_thread_mm(tid, MmId::initial(tid));
            }
            for (futex, waiters) in futexes.iter().zip([[11, 12, 13], [14, 15, 16]]) {
                for raw in waiters {
                    let tid = DetTid::from_raw(raw);
                    let mut sched = state.sched.lock().unwrap();
                    sched.thread_tree.add_child(root, tid, true);
                    sched.next_turns.insert(
                        tid,
                        ThreadNextTurn {
                            dettid: tid,
                            child_tid_addr: 0,
                            req: Ivar::new(),
                            resp: Ivar::new(),
                            protocol: Default::default(),
                        },
                    );
                    sched.priorities.insert(tid, DEFAULT_PRIORITY);
                    sched.sleep_futex_waiter(&tid, *futex, None, u32::MAX, None);
                }
            }
            {
                let mut sched = state.sched.lock().unwrap();
                let token = sched.reserve_sigkill(sender, target, 0).unwrap().token;
                assert_eq!(sched.commit_sigkill(token, true).len(), 2);
            }
            let mut receipts = [(target, futexes[0]), (worker, futexes[1])];
            if reverse {
                receipts.reverse();
            }
            for (victim, futex) in receipts {
                let (_, ack) = state
                    .receive_rpc(
                        Tid::from_raw(victim.as_raw()),
                        (
                            crate::ThreadState::<()>::new(victim, &config, ())
                                .thread_logical_time
                                .clone(),
                            MmId::initial(victim),
                            GlobalRequest::RobustListWakes(vec![(victim, futex)]),
                        ),
                    )
                    .await;
                assert_eq!(
                    ack,
                    GlobalResponse::RobustListWakes(Vec::new()),
                    "a victim's batch is retained, not run at receipt"
                );
                state.sched.lock().unwrap().step2_drain_prefix_for_test();
                // The victim's real consuming deregistration, and a duplicate:
                // the first is accounted once, the second changes nothing.
                let deregistration = ThreadDeregistration {
                    dettid: victim,
                    detpid: target,
                    mm: MmId::initial(victim),
                    thread_start_entered: true,
                    timeslice_stats: TimesliceStats {
                        count: 1,
                        sum_ns: 10,
                        min_ns: 10,
                        max_ns: 10,
                    },
                    syscall_count: 3,
                    chaos_epochs: Vec::new(),
                };
                for _ in 0..2 {
                    let (_, done) = state
                        .receive_rpc(
                            Tid::from_raw(victim.as_raw()),
                            (
                                crate::ThreadState::<()>::new(victim, &config, ())
                                    .thread_logical_time
                                    .clone(),
                                MmId::initial(victim),
                                GlobalRequest::DeregisterThread(deregistration.clone()),
                            ),
                        )
                        .await;
                    assert_eq!(done, GlobalResponse::DeregisterThread(()));
                }
                state.sched.lock().unwrap().step2_drain_prefix_for_test();
            }
            assert!(
                state.sched.lock().unwrap().sigkill_fence_holds(),
                "the victim's exit is not yet published to its parent"
            );
            <GlobalState as GlobalTool>::on_backend_process_exited(&state, target.as_raw());
            state.sched.lock().unwrap().step2_drain_prefix_for_test();
            {
                let sched = state.sched.lock().unwrap();
                assert!(
                    !sched.sigkill_fence_holds(),
                    "every victim is accounted and published"
                );
                for victim in [target, worker] {
                    assert_eq!(
                        sched
                            .timeslice_stats_for_test(victim)
                            .map(|stats| stats.count),
                        Some(1),
                        "victim {victim} accounted exactly once"
                    );
                }
            }
            let mut sched = state.sched.lock().unwrap();
            let still_waiting = futexes
                .iter()
                .map(|futex| sched.futex_waiters_for_test(*futex))
                .collect();
            let queue = sched.run_queue_for_test();
            (still_waiting, queue, sched.next_fuzz_draw_for_test())
        }
        for fuzz in [false, true] {
            let forward = run(fuzz, false).await;
            assert_eq!(forward.0.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2]);
            assert_eq!(forward, run(fuzz, true).await, "fuzz={fuzz}");
        }
    }

    /// A victim with a filed request, whose real consuming deregistration
    /// reaches the handler before the sender's commit (an early receipt, held)
    /// or after it. Both readiness orders give the same retirement: the
    /// callback stays pending with nothing injected (`RetirementGuest` panics
    /// on an injection) and is not retired, the cleanup is accounted once, the
    /// fence is released, and the victim's process is waitable
    /// (https://github.com/rrnewton/hermit/issues/3994, R1).
    #[tokio::test]
    async fn a_victims_receipt_before_or_after_the_commit_retires_it_the_same_way() {
        use reverie::Tool;

        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let [root, sender, victim] = [1, 2, 3].map(DetPid::from_raw);
        for early in [true, false] {
            let state = GlobalState::initialize(&config, false);
            {
                let mut sched = state.sched.lock().unwrap();
                sched.thread_tree.add_child(root, root, true);
                sched.thread_tree.add_child(root, sender, true);
                sched.thread_tree.add_child(root, victim, true);
            }
            for tid in [root, sender, victim] {
                install_test_registration(&state, tid, Ivar::new());
                state
                    .sched
                    .lock()
                    .unwrap()
                    .record_thread_mm(tid, MmId::initial(tid));
            }
            let tool: Detcore = Detcore::new(Tid::from_raw(victim.as_raw()), &config);
            let mut thread = tool.init_thread_state(Tid::from_raw(victim.as_raw()), None);
            thread.detpid = Some(victim);
            let clock = thread.thread_logical_time.clone();
            let mut guest = RetirementGuest {
                global: &state,
                config: &config,
                thread,
                requests: Mutex::new(Vec::new()),
                retired: std::sync::atomic::AtomicBool::new(false),
            };
            let token = state
                .sched
                .lock()
                .unwrap()
                .reserve_sigkill(sender, victim, 0)
                .unwrap()
                .token;
            let deregister = || {
                state.receive_rpc(
                    Tid::from_raw(victim.as_raw()),
                    (
                        clock.clone(),
                        MmId::initial(victim),
                        GlobalRequest::DeregisterThread(ThreadDeregistration {
                            dettid: victim,
                            detpid: victim,
                            mm: MmId::initial(victim),
                            thread_start_entered: true,
                            timeslice_stats: TimesliceStats {
                                count: 1,
                                sum_ns: 10,
                                min_ns: 10,
                                max_ns: 10,
                            },
                            syscall_count: 3,
                            chaos_epochs: Vec::new(),
                        }),
                    ),
                )
            };
            {
                let mut rejoin = Resources::new(victim);
                rejoin.insert(
                    ResourceID::BlockedExternalContinue(ExternalOpId::new(victim, 7)),
                    Permission::RW,
                );
                let mut call = std::pin::pin!(super::resource_request(&mut guest, rejoin));
                assert!(futures::poll!(call.as_mut()).is_pending());
                if early {
                    assert_eq!(deregister().await.1, GlobalResponse::DeregisterThread(()));
                }
                state.sched.lock().unwrap().commit_sigkill(token, true);
                if !early {
                    assert_eq!(deregister().await.1, GlobalResponse::DeregisterThread(()));
                }
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                    assert!(futures::poll!(call.as_mut()).is_pending(), "early={early}");
                }
            }
            assert!(!guest.retired.load(std::sync::atomic::Ordering::SeqCst));
            assert!(
                state.sched.lock().unwrap().sigkill_fence_holds(),
                "early={early}: the exit is not yet published"
            );
            <GlobalState as GlobalTool>::on_backend_process_exited(&state, victim.as_raw());
            let sched = state.sched.lock().unwrap();
            assert!(!sched.sigkill_fence_holds(), "early={early}");
            assert!(sched.process_is_unreaped_zombie(victim), "early={early}");
            assert_eq!(
                sched
                    .timeslice_stats_for_test(victim)
                    .map(|stats| stats.count),
                Some(1),
                "early={early}"
            );
        }
    }

    #[tokio::test]
    async fn thread_exited_uses_natural_retirement_for_killed_stale_and_failed_rpcs() {
        use std::sync::atomic::Ordering;

        use reverie::Tool;

        for cause in ["killed", "stale image", "backend failure", "live"] {
            let (config, state, tid, pid) = cancellation_test_state();
            install_test_registration(&state, tid, Ivar::new());
            let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
            let mut thread = tool.init_thread_state(Tid::from_raw(tid.as_raw()), None);
            thread.detpid = Some(pid);
            thread.thread_logical_time.add_syscall_with_cost(37);
            match cause {
                "killed" => {
                    state
                        .sched
                        .lock()
                        .unwrap()
                        .logically_kill_thread(&tid, &pid, thread.mm_id)
                }
                "stale image" => {
                    state
                        .sched
                        .lock()
                        .unwrap()
                        .install_test_exec_incarnation(tid, thread.mm_id);
                    thread.mm_id = thread.mm_id.for_exec(pid);
                }
                "backend failure" => state.report_backend_failure(reverie::BackendFailure {
                    pid: Tid::from_raw(pid.as_raw()),
                    tid: Tid::from_raw(tid.as_raw()),
                    phase: "natural retirement control",
                }),
                "live" => {}
                _ => unreachable!(),
            }
            let mut guest = RetirementGuest {
                global: &state,
                config: &config,
                thread,
                requests: Mutex::new(Vec::new()),
                retired: std::sync::atomic::AtomicBool::new(false),
            };
            let before = guest.thread.thread_logical_time.as_nanos();
            {
                let mut call = std::pin::pin!(super::send_and_update_time(
                    &mut guest,
                    GlobalRequest::GlobalTimeLowerBound
                ));
                if cause == "live" {
                    assert!(matches!(
                        futures::poll!(call.as_mut()),
                        std::task::Poll::Ready((_, GlobalResponse::GlobalTimeLowerBound(_)))
                    ));
                } else {
                    assert!(futures::poll!(call.as_mut()).is_pending(), "{cause}");
                }
            }
            // A terminal backend failure closes ordinary RPC admission before
            // any ThreadExited response. The backend failure subscription owns
            // cancellation of that pending callback; it must not retire as success.
            assert_eq!(
                guest.retired.load(Ordering::SeqCst),
                matches!(cause, "killed" | "stale image"),
                "{cause}"
            );
            assert_eq!(guest.requests.lock().unwrap().len(), 1);
            assert_eq!(guest.thread.thread_logical_time.as_nanos(), before);
            if cause == "backend failure" {
                assert!(state.sched.lock().unwrap().backend_failed());
                assert!(
                    futures::poll!(std::pin::pin!(state.wait_for_backend_failure())).is_ready()
                );
            }
        }
    }

    /// A5 step 2b: an ordinary request first publishes the caller's process's
    /// copied dispositions, through the real global RPC, when they differ
    /// from what the scheduler holds, and not otherwise; the publication
    /// carries the next generation, and a copy whose actions became unknown
    /// withdraws them. A discard reads the result.
    #[tokio::test]
    async fn an_ordinary_request_first_publishes_changed_dispositions() {
        use reverie::Tool;

        use crate::syscalls::signal_shadow::CopiedDispositions;
        use crate::syscalls::signal_shadow::PublishedDispositions;

        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        }
        .with_backend(|backend| detcore_model::config::set_kernel_signal_facts(backend, true));
        let state = GlobalState::initialize(&config, false);
        let leader = DetTid::from_raw(17);
        let process = DetPid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(leader, leader, true);
        install_test_registration(&state, leader, Ivar::new());
        let tool = <Detcore as Tool>::new(reverie::Pid::from_raw(leader.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(leader.as_raw()), None);
        thread.detpid = Some(process);
        let usr1 = 1 << (libc::SIGUSR1 - 1);
        thread.signal_shadow = Some(crate::signal_state::SignalShadow::seeded(usr1, 0));
        let mut guest = ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: false,
            ppid: None,
            private_memory: None,
        };
        let mutation = |n: u32| GlobalRequest::RecordHostMutation {
            path: format!("/a5-step-2b/{n}"),
        };
        let ignoring = CopiedDispositions {
            ignored: usr1,
            caught: 0,
        };
        let held = |state: &GlobalState| {
            state
                .sched
                .lock()
                .unwrap()
                .published_dispositions
                .get(&process)
                .copied()
        };

        super::send_and_update_time(&mut guest, mutation(1)).await;
        super::send_and_update_time(&mut guest, mutation(2)).await;
        assert_eq!(
            *guest.requests.lock().unwrap(),
            vec![
                GlobalRequest::PublishSignalDispositions(1, Some(ignoring)),
                GlobalRequest::PublishSignalMask(Some(0)),
                mutation(1),
                mutation(2),
            ],
            "published once, before the first request"
        );
        // The scheduler's entry, and the process's record of it.
        let entry = |generation, dispositions| PublishedDispositions {
            generation,
            dispositions,
            ..Default::default()
        };
        let record = |generation, accepted, dispositions| PublishedDispositions {
            generation,
            accepted,
            dispositions,
        };
        assert_eq!(held(&state), Some(entry(1, Some(ignoring))));
        assert_eq!(
            *guest.thread.published_dispositions.lock().unwrap(),
            record(1, 1, Some(ignoring))
        );
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .published_dispositions_for(leader, libc::SIGUSR1)
                .is_some()
        );

        // A clone that shares the actions with a new process withdraws them
        // before it runs (`before_clone`).
        guest.requests.lock().unwrap().clear();
        crate::syscalls::signal_shadow::before_clone(
            &mut guest,
            (libc::CLONE_VM | libc::CLONE_SIGHAND) as u64,
        )
        .await;
        assert_eq!(
            *guest.requests.lock().unwrap(),
            vec![GlobalRequest::PublishSignalDispositions(2, None)]
        );
        assert_eq!(held(&state), Some(entry(2, None)));
        assert_eq!(
            *guest.thread.published_dispositions.lock().unwrap(),
            record(2, 2, None)
        );
        super::send_and_update_time(&mut guest, mutation(3)).await;
        assert_eq!(
            guest.requests.lock().unwrap().len(),
            2,
            "nothing more to withdraw"
        );

        // A thread the scheduler does not know is refused: the record keeps
        // its dispositions, and once known, the next request publishes again.
        guest.thread.signal_shadow = Some(crate::signal_state::SignalShadow::seeded(usr1, 0));
        let mut turn = state.sched.lock().unwrap().next_turns.remove(&leader);
        guest.requests.lock().unwrap().clear();
        super::send_and_update_time(&mut guest, mutation(4)).await;
        assert_eq!(held(&state), Some(entry(2, None)), "refused");
        assert_eq!(
            *guest.thread.published_dispositions.lock().unwrap(),
            record(3, 2, None)
        );
        state
            .sched
            .lock()
            .unwrap()
            .next_turns
            .insert(leader, turn.take().unwrap());
        super::send_and_update_time(&mut guest, mutation(5)).await;
        assert_eq!(held(&state), Some(entry(4, Some(ignoring))));
        assert_eq!(
            *guest.thread.published_dispositions.lock().unwrap(),
            record(4, 4, Some(ignoring))
        );

        // The scheduler holds a generation this process never used (a stale
        // entry): the publication is not taken, the record carries on after
        // that generation, and the thread publishes again at once, before its
        // request goes out, and is taken.
        let stale = entry(
            50,
            Some(CopiedDispositions {
                ignored: 0,
                caught: usr1,
            }),
        );
        state
            .sched
            .lock()
            .unwrap()
            .published_dispositions
            .insert(process, stale);
        *guest.thread.published_dispositions.lock().unwrap() = record(4, 4, None);
        super::send_and_update_time(&mut guest, mutation(6)).await;
        assert_eq!(held(&state), Some(entry(51, Some(ignoring))));
        assert_eq!(
            *guest.thread.published_dispositions.lock().unwrap(),
            record(51, 51, Some(ignoring))
        );
        // The same with a stale entry of the very generation the process uses
        // next, the likeliest shape: not taken, so not mistaken for it.
        state
            .sched
            .lock()
            .unwrap()
            .published_dispositions
            .insert(process, entry(52, stale.dispositions));
        *guest.thread.published_dispositions.lock().unwrap() = record(51, 51, None);
        super::send_and_update_time(&mut guest, mutation(7)).await;
        assert_eq!(held(&state), Some(entry(53, Some(ignoring))));
        assert_eq!(
            *guest.thread.published_dispositions.lock().unwrap(),
            record(53, 53, Some(ignoring))
        );
        assert_eq!(
            *guest.requests.lock().unwrap(),
            vec![
                GlobalRequest::PublishSignalDispositions(3, Some(ignoring)),
                mutation(4),
                GlobalRequest::PublishSignalDispositions(4, Some(ignoring)),
                mutation(5),
                GlobalRequest::PublishSignalDispositions(5, Some(ignoring)),
                GlobalRequest::PublishSignalDispositions(51, Some(ignoring)),
                mutation(6),
                GlobalRequest::PublishSignalDispositions(52, Some(ignoring)),
                GlobalRequest::PublishSignalDispositions(53, Some(ignoring)),
                mutation(7),
            ]
        );
    }

    /// A5 step 2b: an ordinary request first publishes the caller's mask
    /// from Detcore's copy, through the real global RPC, when it differs
    /// from what the scheduler holds, withdraws it inside a temporary-mask
    /// wait, and is refused for a thread the scheduler does not know, which
    /// publishes again once known. The scheduler's reader answers from it.
    #[tokio::test]
    async fn an_ordinary_request_first_publishes_a_changed_mask() {
        use reverie::Tool;

        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        }
        .with_backend(|backend| detcore_model::config::set_kernel_signal_facts(backend, true));
        let state = GlobalState::initialize(&config, false);
        let leader = DetTid::from_raw(17);
        let process = DetPid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(leader, leader, true);
        install_test_registration(&state, leader, Ivar::new());
        let tool = <Detcore as Tool>::new(reverie::Pid::from_raw(leader.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(leader.as_raw()), None);
        thread.detpid = Some(process);
        let usr1 = 1 << (libc::SIGUSR1 - 1);
        thread.signal_shadow = Some(crate::signal_state::SignalShadow::seeded(0, usr1));
        let mut guest = ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: false,
            ppid: None,
            private_memory: None,
        };
        let mutation = |n: u32| GlobalRequest::RecordHostMutation {
            path: format!("/a5-step-2b-mask/{n}"),
        };
        let masks = |guest: &ExternalRegistrationGuest<'_>| -> Vec<GlobalRequest> {
            guest
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| !matches!(request, GlobalRequest::PublishSignalDispositions(..)))
                .cloned()
                .collect()
        };
        let held = |state: &GlobalState| {
            state
                .sched
                .lock()
                .unwrap()
                .published_masks
                .get(&leader)
                .copied()
        };

        super::send_and_update_time(&mut guest, mutation(1)).await;
        super::send_and_update_time(&mut guest, mutation(2)).await;
        // The whole stream: the process's dispositions, then the mask, then
        // the request.
        assert_eq!(
            *guest.requests.lock().unwrap(),
            vec![
                GlobalRequest::PublishSignalDispositions(
                    1,
                    Some(crate::syscalls::signal_shadow::CopiedDispositions {
                        ignored: 0,
                        caught: 0,
                    })
                ),
                GlobalRequest::PublishSignalMask(Some(usr1)),
                mutation(1),
                mutation(2),
            ],
            "published once, before the first request"
        );
        assert_eq!(held(&state), Some(usr1));
        assert_eq!(guest.thread.published_mask, Some(Some(usr1)));
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .published_mask_for(leader, libc::SIGUSR1),
            Some(usr1)
        );

        // Inside a temporary-mask wait the kernel may hold the wait's mask:
        // withdrawn.
        guest
            .thread
            .signal_shadow
            .as_mut()
            .unwrap()
            .thread
            .wait_mask = Some(0);
        guest.requests.lock().unwrap().clear();
        super::send_and_update_time(&mut guest, mutation(3)).await;
        assert_eq!(
            masks(&guest),
            vec![GlobalRequest::PublishSignalMask(None), mutation(3)]
        );
        assert_eq!(held(&state), None);

        // Refused while the scheduler does not know the thread; published
        // again once it does.
        guest
            .thread
            .signal_shadow
            .as_mut()
            .unwrap()
            .thread
            .wait_mask = None;
        let mut turn = state.sched.lock().unwrap().next_turns.remove(&leader);
        guest.requests.lock().unwrap().clear();
        super::send_and_update_time(&mut guest, mutation(4)).await;
        assert_eq!(held(&state), None, "refused");
        assert_eq!(guest.thread.published_mask, Some(None));
        state
            .sched
            .lock()
            .unwrap()
            .next_turns
            .insert(leader, turn.take().unwrap());
        super::send_and_update_time(&mut guest, mutation(5)).await;
        assert_eq!(
            masks(&guest),
            vec![
                GlobalRequest::PublishSignalMask(Some(usr1)),
                mutation(4),
                GlobalRequest::PublishSignalMask(Some(usr1)),
                mutation(5),
            ]
        );
        assert_eq!(held(&state), Some(usr1));

        // An exec that loses the copy: what the scheduler holds is unknown
        // after an exec, so the next request withdraws the mask.
        guest
            .thread
            .signal_shadow
            .as_ref()
            .unwrap()
            .forget_dispositions();
        crate::syscalls::signal_shadow::at_exec(&mut guest);
        assert!(guest.thread.signal_shadow.is_none(), "the copy was lost");
        guest.requests.lock().unwrap().clear();
        super::send_and_update_time(&mut guest, mutation(6)).await;
        assert_eq!(
            masks(&guest),
            vec![GlobalRequest::PublishSignalMask(None), mutation(6)]
        );
        assert_eq!(held(&state), None);
    }

    async fn check_external_child_tid_registration(
        flags: CloneFlags,
        supplied_address: usize,
        expected_address: usize,
    ) {
        use reverie::Tool;

        let config = Config {
            sequentialize_threads: true,
            // This control observes registration before the child starts; use
            // the supported parent-first order for the one turn it drives.
            runs_post_fork: crate::RunsPostFork::Parent,
            ..Config::default()
        }
        .with_backend(|backend| {
            backend.needs_killed_thread_rpc_cancellation = true;
        });
        let state = GlobalState::initialize(&config, false);
        let parent = DetTid::from_raw(17);
        let parent_pid = DetPid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(parent, parent, true);
        install_test_registration(&state, parent, Ivar::new());
        let tool = Detcore::new(reverie::Pid::from_raw(parent.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
        thread.detpid = Some(parent_pid);
        let mut guest = ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: false,
            ppid: None,
            private_memory: None,
        };
        let child = DetTid::from_raw(18);
        let exit_signal = if flags.contains(CloneFlags::CLONE_THREAD) {
            0
        } else {
            libc::SIGCHLD
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut registration = std::pin::pin!(tool.register_external_child(
                &mut guest,
                Tid::from_raw(child.as_raw()),
                supplied_address,
                flags,
                exit_signal,
                None,
            ));
            assert!(futures::poll!(registration.as_mut()).is_pending());
            // The production RPC parks the parent on ParentContinue. Drive
            // that actual scheduler turn instead of pre-filling a response or
            // disabling sequentialization to make registration return.
            let committed = crate::scheduler::do_a_turn_blocking(
                state.sched.clone(),
                state.global_time.clone(),
                &Err(crate::scheduler::SkipTurn),
            )
            .await
            .expect("the parent continuation must commit");
            assert_eq!(committed.tid, parent);
            assert_eq!(
                committed.resources,
                std::collections::HashMap::from([(
                    crate::resources::ResourceID::ParentContinue { parent, child },
                    crate::resources::Permission::W,
                )]),
            );
            registration.await;
        })
        .await
        .expect("external child registration must return without running a guest");

        assert_eq!(guest.thread.clone_flags, None);
        assert_eq!(guest.thread.dettid, parent);
        let mut scheduler = state.sched.lock().unwrap();
        assert_eq!(scheduler.next_turns.len(), 2);
        assert!(scheduler.next_turns.contains_key(&parent));
        assert_eq!(
            scheduler.next_turns[&child].child_tid_addr,
            expected_address
        );
        assert_eq!(
            *guest.requests.lock().unwrap(),
            vec![GlobalRequest::CreateChildThread(
                child,
                parent_pid,
                expected_address,
                Some(flags),
                exit_signal,
                None,
                Some(DEFAULT_PRIORITY),
            )],
            "external registration must send one correctly gated real RPC"
        );
        let child_pid = if flags.contains(CloneFlags::CLONE_THREAD) {
            parent_pid
        } else {
            child
        };
        let child_mm = MmId::for_clone(
            MmId::initial(parent_pid),
            child,
            flags.contains(CloneFlags::CLONE_VM),
        );
        scheduler.logically_kill_thread(&child, &child_pid, child_mm);
        assert!(!scheduler.next_turns.contains_key(&child));
        assert!(scheduler.next_turns.contains_key(&parent));
        assert_eq!(
            scheduler.child_tid_was_cleared(
                FutexID::private(child_mm, supplied_address),
                child.as_raw(),
            ),
            expected_address != 0,
            "exit must use only the registered child-TID address"
        );
        assert!(!scheduler.child_tid_was_cleared(FutexID::private(child_mm, 0), child.as_raw(),));
        assert!(!scheduler.child_tid_was_cleared(
            FutexID::private(child_mm, supplied_address),
            parent.as_raw(),
        ));
    }

    #[tokio::test]
    async fn external_registration_without_child_cleartid_disables_exit_wake() {
        for kind in [
            CloneFlags::empty(),
            CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM | CloneFlags::CLONE_SIGHAND,
        ] {
            for registration in [CloneFlags::empty(), CloneFlags::CLONE_CHILD_SETTID] {
                check_external_child_tid_registration(kind | registration, 0x1234, 0).await;
            }
        }
    }

    #[tokio::test]
    async fn external_registration_with_child_cleartid_preserves_exact_exit_wake() {
        for kind in [
            CloneFlags::empty(),
            CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM | CloneFlags::CLONE_SIGHAND,
        ] {
            for registration in [
                CloneFlags::CLONE_CHILD_CLEARTID,
                CloneFlags::CLONE_CHILD_CLEARTID | CloneFlags::CLONE_CHILD_SETTID,
            ] {
                check_external_child_tid_registration(kind | registration, 0x1234, 0x1234).await;
                check_external_child_tid_registration(kind | registration, 0, 0).await;
            }
        }
    }

    #[tokio::test]
    async fn set_child_tid_address_rpc_updates_and_resets_the_registration() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .next_turns
            .get_mut(&dettid)
            .unwrap()
            .child_tid_addr = 0x1234;

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(detpid),
                    GlobalRequest::SetChildTidAddress(0),
                ),
            )
            .await;

        assert_eq!(response.1, GlobalResponse::SetChildTidAddress(()));
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .next_turns
                .get(&dettid)
                .unwrap()
                .child_tid_addr,
            0
        );
    }

    /// A successful SIGKILL reaches the scheduler's record through the RPCs a
    /// sender makes in its turn: a process named by `kill`, and a SIGKILL with
    /// no named victim; another signal records nothing
    /// (https://github.com/rrnewton/hermit/issues/3904).
    #[tokio::test]
    async fn sigkill_notifications_record_the_doomed_process() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        let notify = |request| {
            state.receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (DetTime::new(&config), MmId::initial(detpid), request),
            )
        };
        let victim = DetPid::from_raw(41);
        assert_eq!(
            notify(GlobalRequest::NotifySignalPending(
                DetTid::from_raw(41),
                SigWrapper(libc::SIGKILL),
                Some(victim),
                super::SignalScope::Process,
            ))
            .await
            .1,
            GlobalResponse::NotifySignalPending(())
        );
        assert_eq!(
            state.sched.lock().unwrap().sigkill_recorded(victim),
            (true, false)
        );
        let other = DetPid::from_raw(43);
        notify(GlobalRequest::NotifySignalPending(
            DetTid::from_raw(43),
            SigWrapper(libc::SIGTERM),
            Some(other),
            super::SignalScope::Process,
        ))
        .await;
        assert_eq!(
            state.sched.lock().unwrap().sigkill_recorded(other),
            (false, false)
        );
        // A process-group kill records every process in the group.
        assert_eq!(
            notify(GlobalRequest::NotifyUnresolvedSigkill(Some(detpid)))
                .await
                .1,
            GlobalResponse::NotifyUnresolvedSigkill(())
        );
        assert_eq!(
            state.sched.lock().unwrap().sigkill_recorded(detpid),
            (true, false)
        );
        assert_eq!(
            notify(GlobalRequest::NotifyUnresolvedSigkill(None)).await.1,
            GlobalResponse::NotifyUnresolvedSigkill(())
        );
        assert_eq!(
            state.sched.lock().unwrap().sigkill_recorded(other),
            (false, true)
        );
    }

    async fn child_start_clock_trajectory(
        start_before_selection: bool,
        child_first: bool,
    ) -> (Vec<LogicalTime>, Vec<DetTid>, bool) {
        let config = Config {
            sequentialize_threads: true,
            runs_post_fork: if child_first {
                RunsPostFork::Child
            } else {
                RunsPostFork::Parent
            },
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let parent = DetTid::from_raw(17);
        let parent_pid = parent;
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(parent, parent, true);
        let child = DetTid::from_raw(parent.as_raw() + 1);
        let parent_mm = MmId::initial(parent_pid);
        let child_mm = MmId::for_clone(parent_mm, child, false);
        install_test_registration(&state, parent, Ivar::new());
        let epoch = DetTime::new(&config).as_nanos();
        let mut parent_clock = DetTime::new(&config);
        parent_clock.advance_to(epoch + LogicalTime::from_nanos(1_001));
        let child_clock = parent_clock.clone_for_child();

        // Polling the real parent RPC publishes both admission and
        // ParentContinue before it waits, just as the clone handler does.
        let mut registration = Box::pin(state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                parent_clock.clone(),
                parent_mm,
                GlobalRequest::CreateChildThread(
                    child,
                    parent_pid,
                    0,
                    Some(CloneFlags::empty()),
                    libc::SIGCHLD,
                    None,
                    Some(DEFAULT_PRIORITY),
                ),
            ),
        ));
        assert!(futures::poll!(&mut registration).is_pending());
        let child_request = state.sched.lock().unwrap().next_turns[&child].req.clone();
        let mut startup = Box::pin(state.receive_rpc(
            Tid::from_raw(child.as_raw()),
            (
                child_clock.clone(),
                child_mm,
                GlobalRequest::StartNewThread(child, child, None, None),
            ),
        ));
        if start_before_selection {
            // recv_start_new_thread yields once before filling its request.
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(child_request.try_read().is_some());
        }

        let skipped = Err(crate::scheduler::SkipTurn);
        let mut turn = Box::pin(crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &skipped,
        ));
        if !start_before_selection && child_first {
            assert!(futures::poll!(&mut turn).is_pending());
            assert_eq!(child_request.to_string(), "<ivar HasWaiter>");
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(child_request.try_read().is_some());
        }
        let first = turn.await.expect("first post-fork turn must commit");
        if !start_before_selection && !child_first {
            assert!(child_request.try_read().is_none());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(child_request.try_read().is_some());
        }
        let child_resource = ResourceID::MemAddrSpace(child);
        let parent_resource = ResourceID::ParentContinue { parent, child };
        let (first_tid, first_resource, first_permission, second_resource, second_permission) =
            if child_first {
                (
                    child,
                    &child_resource,
                    Permission::RW,
                    &parent_resource,
                    Permission::W,
                )
            } else {
                (
                    parent,
                    &parent_resource,
                    Permission::W,
                    &child_resource,
                    Permission::RW,
                )
            };
        assert_eq!(first.tid, first_tid);
        assert_eq!(first.resources.len(), 1);
        assert_eq!(first.resources.get(first_resource), Some(&first_permission));
        if child_first {
            assert_eq!(
                startup.as_mut().await,
                (None, granted(GlobalResponse::StartNewThread(None)))
            );
        } else {
            assert_eq!(
                registration.as_mut().await,
                (None, granted(GlobalResponse::CreateChildThread(None)))
            );
        }
        let first_time = state.sched.lock().unwrap().committed_time;
        assert_eq!(first_time, epoch + LogicalTime::from_nanos(1_001));
        assert_eq!(
            state.global_time.lock().unwrap().threads_time(child),
            child_clock.as_nanos()
        );

        // The selected thread's first new nanosecond and the ordinary scheduler
        // increment must both remain observable on the other thread's turn.
        let (mut first_clock, first_mm) = if child_first {
            (child_clock, child_mm)
        } else {
            (parent_clock, parent_mm)
        };
        first_clock.advance_to(first_clock.as_nanos() + LogicalTime::from_nanos(1));
        let mut resources = Resources::new(first_tid);
        resources.insert(ResourceID::MemAddrSpace(first_tid), Permission::RW);
        let mut work = Box::pin(state.receive_rpc(
            Tid::from_raw(first_tid.as_raw()),
            (
                first_clock,
                first_mm,
                GlobalRequest::RequestResources(resources, first_tid),
            ),
        ));
        assert!(futures::poll!(&mut work).is_pending());
        let second = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Ok(first),
        )
        .await
        .expect("other thread's continuation must commit");
        assert_eq!(second.resources.len(), 1);
        assert_eq!(
            second.resources.get(second_resource),
            Some(&second_permission)
        );
        if child_first {
            assert_eq!(
                registration.await,
                (None, granted(GlobalResponse::CreateChildThread(None)))
            );
        } else {
            assert_eq!(
                startup.await,
                (None, granted(GlobalResponse::StartNewThread(None)))
            );
        }
        let mut scheduler = state.sched.lock().unwrap();
        let next_time = scheduler.committed_time;
        assert_eq!(next_time, epoch + LogicalTime::from_nanos(501_002));
        let queue = scheduler.run_queue.tids().copied().collect();
        let next_random = scheduler.child_runs_first_post_fork(RunsPostFork::Random);
        (vec![first_time, next_time], queue, next_random)
    }

    #[tokio::test]
    async fn child_start_clock_is_independent_of_first_rpc_arrival() {
        for child_first in [true, false] {
            assert_eq!(
                child_start_clock_trajectory(true, child_first).await,
                child_start_clock_trajectory(false, child_first).await,
            );
        }
    }

    #[tokio::test]
    async fn vfork_registration_does_not_charge_inherited_work_before_startup() {
        let (config, state, parent, parent_pid) = cancellation_test_state();
        let child = DetTid::from_raw(parent.as_raw() + 1);
        let mm = MmId::initial(parent_pid);
        install_test_registration(&state, parent, Ivar::new());
        let mut parent_clock = DetTime::new(&config);
        let epoch = parent_clock.as_nanos();
        parent_clock.advance_to(epoch + LogicalTime::from_nanos(1_001));
        let child_clock = parent_clock.clone_for_child();
        let mut resources = Resources::new(parent);
        resources.insert(
            ResourceID::BlockingVfork(ExternalOpId::new(parent, 1)),
            Permission::RW,
        );
        let mut blocking = Box::pin(state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                parent_clock,
                mm,
                GlobalRequest::RequestResources(resources, parent_pid),
            ),
        ));
        assert!(futures::poll!(&mut blocking).is_pending());
        let background = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await;
        assert!(background.is_err());
        assert_eq!(
            blocking.await,
            (
                None,
                granted(GlobalResponse::RequestResources(ResumeStatus::Normal))
            )
        );
        let before_child = state.global_time.lock().unwrap().as_nanos();
        assert_eq!(before_child, epoch + LogicalTime::from_nanos(1_001));

        // Unlike ordinary clone, this first RPC is sent by the child itself.
        let created = state
            .receive_rpc(
                Tid::from_raw(child.as_raw()),
                (
                    child_clock.clone(),
                    mm,
                    GlobalRequest::CreateVforkChildThread(
                        parent,
                        parent_pid,
                        child,
                        0,
                        CloneFlags::CLONE_VFORK | CloneFlags::CLONE_VM,
                        libc::SIGCHLD,
                        Some(DEFAULT_PRIORITY - 1),
                    ),
                ),
            )
            .await;
        assert_eq!(created, (None, GlobalResponse::CreateChildThread(None)));
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_child);
        assert_eq!(
            state.global_time.lock().unwrap().threads_time(child),
            child_clock.as_nanos()
        );

        let mut startup = Box::pin(state.receive_rpc(
            Tid::from_raw(child.as_raw()),
            (
                child_clock,
                mm,
                GlobalRequest::StartNewThread(child, child, None, None),
            ),
        ));
        assert!(futures::poll!(&mut startup).is_pending());
        assert!(futures::poll!(&mut startup).is_pending());
        let first = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &background,
        )
        .await
        .expect("vfork child must receive its first turn");
        assert_eq!(first.tid, child);
        assert_eq!(
            startup.await,
            (None, granted(GlobalResponse::StartNewThread(None)))
        );
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_child);
    }

    #[tokio::test]
    async fn first_nonstartup_rpc_counts_only_new_child_work() {
        let config = Config {
            sequentialize_threads: false,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let parent = DetTid::from_raw(17);
        let child = DetTid::from_raw(18);
        let mut parent_clock = DetTime::new(&config);
        let epoch = parent_clock.as_nanos();
        parent_clock.advance_to(epoch + LogicalTime::from_nanos(101));
        let mut child_clock = parent_clock.clone_for_child();
        let _ = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    parent_clock,
                    MmId::initial(parent),
                    GlobalRequest::GlobalTimeLowerBound,
                ),
            )
            .await;
        child_clock.advance_to(child_clock.as_nanos() + LogicalTime::from_nanos(1));
        let observed = state
            .receive_rpc(
                Tid::from_raw(child.as_raw()),
                (
                    child_clock,
                    MmId::initial(child),
                    GlobalRequest::GlobalTimeLowerBound,
                ),
            )
            .await;
        assert_eq!(
            observed,
            (
                None,
                GlobalResponse::GlobalTimeLowerBound(epoch + LogicalTime::from_nanos(102))
            )
        );
    }

    #[tokio::test]
    async fn exec_reconnect_retains_inherited_work_accounting_across_local_reload() {
        let (config, state, leader, detpid) = cancellation_test_state();
        let ancestor = DetTid::from_raw(leader.as_raw() - 1);
        let worker = DetTid::from_raw(leader.as_raw() + 1);
        let old_mm = MmId::initial(detpid);
        install_test_registration(&state, leader, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(leader, worker, false);
        install_test_registration(&state, worker, Ivar::new());

        let mut ancestor_clock = DetTime::new(&config);
        let epoch = ancestor_clock.as_nanos();
        ancestor_clock.advance_to(epoch + LogicalTime::from_nanos(1_000));
        let mut leader_clock = ancestor_clock.clone_for_child();
        leader_clock.advance_to(epoch + LogicalTime::from_nanos(1_100));
        let mut worker_clock = leader_clock.clone_for_child();
        worker_clock.advance_to(epoch + LogicalTime::from_nanos(1_350));
        for (tid, clock) in [
            (ancestor, ancestor_clock),
            (leader, leader_clock),
            (worker, worker_clock.clone()),
        ] {
            let _ = state
                .receive_rpc(
                    Tid::from_raw(tid.as_raw()),
                    (clock, old_mm, GlobalRequest::GlobalTimeLowerBound),
                )
                .await;
        }
        let total = state.global_time.lock().unwrap().as_nanos();
        assert_eq!(total, epoch + LogicalTime::from_nanos(1_350));
        // A failed exec cancels its pending transfer without changing either
        // inherited component. The next successful attempt must use the same
        // clocks, not charge either component's inherited work again.
        let _ = state
            .receive_rpc(
                Tid::from_raw(worker.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::PrepareExec(detpid, old_mm, Default::default()),
                ),
            )
            .await;
        let cancelled = state
            .receive_rpc(
                Tid::from_raw(worker.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::CancelExec(detpid),
                ),
            )
            .await;
        assert_eq!(cancelled, (None, GlobalResponse::CancelExec(())));
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), total);
        assert_eq!(
            state.global_time.lock().unwrap().threads_time(worker),
            worker_clock.as_nanos()
        );

        let prepared = state
            .receive_rpc(
                Tid::from_raw(worker.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::PrepareExec(detpid, old_mm, Default::default()),
                ),
            )
            .await;
        assert_eq!(prepared, (None, GlobalResponse::PrepareExec(())));

        let mut fresh = DetTime::new(&config);
        let recreated = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    fresh.clone(),
                    MmId::initial(leader),
                    GlobalRequest::CreateChildThread(
                        leader,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(
            recreated,
            (
                Some(worker_clock.as_nanos()),
                GlobalResponse::CreateChildThread(Some(old_mm.for_exec(detpid)))
            )
        );
        // A delayed request from the destroyed image must be rejected before
        // its absolute clock or its inherited metadata reaches accounting.
        let stale = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::RequestResources(Resources::new(leader), detpid),
                ),
            )
            .await;
        assert_eq!(stale, (None, GlobalResponse::ThreadExited));
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), total);

        fresh.advance_to(recreated.0.unwrap());
        assert_eq!(fresh.inherited_nanos(), LogicalTime::ZERO);
        let mut startup = Box::pin(state.receive_rpc(
            Tid::from_raw(leader.as_raw()),
            (
                fresh.clone(),
                old_mm.for_exec(detpid),
                GlobalRequest::StartNewThread(leader, detpid, None, None),
            ),
        ));
        assert!(futures::poll!(&mut startup).is_pending());
        assert!(futures::poll!(&mut startup).is_pending());
        let first = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .expect("replacement leader must run");
        assert_eq!(first.tid, leader);
        assert_eq!(
            startup.await,
            (None, granted(GlobalResponse::StartNewThread(None)))
        );
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), total);

        fresh.advance_to(fresh.as_nanos() + LogicalTime::from_nanos(1));
        let observed = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    fresh.clone(),
                    old_mm.for_exec(detpid),
                    GlobalRequest::GlobalTimeLowerBound,
                ),
            )
            .await;
        assert_eq!(
            observed,
            (
                None,
                GlobalResponse::GlobalTimeLowerBound(total + LogicalTime::from_nanos(1))
            )
        );
        let global = state.global_time.lock().unwrap();
        assert_eq!(global.threads_time(leader), fresh.as_nanos());
        assert!(!global.contains_thread(worker));
    }

    #[test]
    fn live_registration_without_next_turn_is_not_terminal() {
        let (_, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        state.sched.lock().unwrap().next_turns.remove(&dettid);

        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .thread_is_logically_killed(dettid),
            "transient next-turn absence must not imply logical death"
        );

        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .thread_is_logically_killed(dettid),
            "explicit logical death must install a permanent TID tombstone"
        );
    }

    #[tokio::test]
    async fn exec_reconnect_retires_siblings_and_reuses_live_scheduler_and_clock_state() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        let old_mm = MmId::initial(detpid).for_exec(detpid);
        install_test_registration(&state, dettid, Ivar::new());
        let sibling = DetTid::from_raw(dettid.as_raw() + 1);
        let sibling_request = Ivar::new();
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, sibling, false);
        install_test_registration(&state, sibling, sibling_request.clone());
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.next_turns.get_mut(&dettid).unwrap().resp =
                Ivar::full(SchedResponse::Go(None));
            scheduler
                .next_turns
                .get_mut(&sibling)
                .unwrap()
                .child_tid_addr = 0x1234;
        }
        let mut existing_time = DetTime::new(&config);
        existing_time.add_syscall();
        existing_time.add_syscall();
        state.global_time.lock().unwrap().update_global_time(
            dettid,
            existing_time.as_nanos(),
            LogicalTime::ZERO,
        );
        let (global_before, thread_before) = {
            let global_time = state.global_time.lock().unwrap();
            (global_time.as_nanos(), global_time.threads_time(dettid))
        };
        let fresh_local_time = DetTime::new(&config);
        let physical_pid = std::process::id() as i32;
        let physical_tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
        let physical_ids = Some((physical_pid, physical_tid));
        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                caller: dettid,
                process: detpid,
                mm: old_mm,
                fd_blocking: Default::default(),
            },
        );
        state
            .sched
            .lock()
            .unwrap()
            .install_test_exec_incarnation(dettid, old_mm);
        let in_flight_exec_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    existing_time.clone(),
                    old_mm.for_exec(detpid),
                    GlobalRequest::ReportUnsupportedSyscall("exec-in-flight".to_owned()),
                ),
            )
            .await;
        assert_eq!(
            in_flight_exec_response.1,
            GlobalResponse::ReportUnsupportedSyscall(())
        );

        let create_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    fresh_local_time.clone(),
                    MmId::initial(dettid),
                    GlobalRequest::CreateChildThread(
                        dettid,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        physical_ids,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(
            create_response,
            (
                Some(thread_before),
                GlobalResponse::CreateChildThread(Some(old_mm.for_exec(detpid)))
            )
        );
        assert_eq!(
            state.sched.lock().unwrap().physical_thread_identity(dettid),
            Some((old_mm.for_exec(detpid), physical_pid, physical_tid)),
            "post-exec host identity must be installed by CreateChildThread before admission"
        );

        let start_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    fresh_local_time,
                    old_mm.for_exec(detpid),
                    GlobalRequest::StartNewThread(dettid, detpid, physical_ids, None),
                ),
            )
            .await;
        assert_eq!(
            start_response,
            (Some(thread_before), GlobalResponse::StartNewThread(None))
        );

        let scheduler = state.sched.lock().unwrap();
        assert!(!scheduler.thread_is_logically_killed(dettid));
        assert!(scheduler.thread_is_logically_killed(sibling));
        assert_eq!(scheduler.next_turns.len(), 1);
        assert!(matches!(sibling_request.try_read(), Some(Err(_))));
        assert!(
            scheduler.child_tid_was_cleared(FutexID::private(old_mm, 0x1234), sibling.as_raw())
        );
        drop(scheduler);
        let global_time = state.global_time.lock().unwrap();
        assert_eq!(global_time.as_nanos(), global_before);
        assert_eq!(global_time.threads_time(dettid), thread_before);
    }

    /// A waiter in another process, parked on a robust word the exec's caller
    /// owned, for the exec owner-death tests below.
    fn park_exec_robust_waiter(state: &GlobalState, futex: FutexID) -> DetTid {
        park_exec_robust_waiter_as(state, futex, 40)
    }

    /// [`park_exec_robust_waiter`] for the thread `raw`.
    fn park_exec_robust_waiter_as(state: &GlobalState, futex: FutexID, raw: i32) -> DetTid {
        let waiter = DetTid::from_raw(raw);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(waiter, waiter, true);
        install_test_registration(state, waiter, Ivar::new());
        let mut scheduler = state.sched.lock().unwrap();
        scheduler.sleep_futex_waiter(&waiter, futex, None, u32::MAX, None);
        assert!(scheduler.run_queue.remove_tid(waiter));
        waiter
    }

    /// On a backend whose new image reconnects with a fresh runtime (DBT),
    /// the owner-death wakes the exec's caller read before the native exec
    /// travel with the prepared exec, and the reconnect admits their waiters
    /// at the next deterministic drain
    /// (https://github.com/rrnewton/hermit/issues/4268).
    #[tokio::test]
    async fn exec_reconnect_wakes_the_robust_waiters_its_exec_marked() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        let old_mm = MmId::initial(detpid).for_exec(detpid);
        install_test_registration(&state, dettid, Ivar::new());
        let mut existing_time = DetTime::new(&config);
        existing_time.add_syscall();
        state.global_time.lock().unwrap().update_global_time(
            dettid,
            existing_time.as_nanos(),
            LogicalTime::ZERO,
        );
        let futex = FutexID::private(old_mm, 0x4000);
        let waiter = park_exec_robust_waiter(&state, futex);
        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                caller: dettid,
                process: detpid,
                mm: old_mm,
                fd_blocking: ExecFdState {
                    robust_wakes: vec![(dettid, futex)],
                    ..Default::default()
                },
            },
        );
        state
            .sched
            .lock()
            .unwrap()
            .install_test_exec_incarnation(dettid, old_mm);
        assert!(!state.sched.lock().unwrap().admission_pending(waiter));

        let create_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(dettid),
                    GlobalRequest::CreateChildThread(
                        dettid,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(
            create_response.1,
            GlobalResponse::CreateChildThread(Some(old_mm.for_exec(detpid)))
        );
        assert!(state.sched.lock().unwrap().admission_pending(waiter));
        assert!(
            state
                .post_exec_fd_blocking
                .lock()
                .unwrap()
                .get(&dettid)
                .is_none_or(|carried| carried.robust_wakes.is_empty()),
            "the wakes are delivered once, at the reconnect"
        );
    }

    /// An exec that fails before its point of no return (here ENOENT, which
    /// takes CancelExec) marks no robust word, so its prepared wakes are
    /// dropped with the prepared exec and the waiter stays parked. An exec
    /// that ends its group without a reconnect delivers them instead
    /// (`a_terminal_exec_wakes_its_robust_waiters`).
    #[tokio::test]
    async fn a_failed_exec_drops_its_robust_wakes() {
        let (config, state, leader, detpid) = cancellation_test_state();
        install_test_registration(&state, leader, Ivar::new());
        let futex = FutexID::private(MmId::initial(detpid), 0x4000);
        let waiter = park_exec_robust_waiter(&state, futex);
        let clock = DetTime::new(&config);
        let prepared = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    clock.clone(),
                    MmId::initial(leader),
                    GlobalRequest::PrepareExec(
                        detpid,
                        MmId::initial(detpid),
                        Box::new(ExecFdState {
                            robust_wakes: vec![(leader, futex)],
                            ..Default::default()
                        }),
                    ),
                ),
            )
            .await;
        assert_eq!(prepared.1, GlobalResponse::PrepareExec(()));
        let cancelled = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    clock,
                    MmId::initial(leader),
                    GlobalRequest::CancelExec(detpid),
                ),
            )
            .await;
        assert_eq!(cancelled.1, GlobalResponse::CancelExec(()));
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert!(!state.sched.lock().unwrap().admission_pending(waiter));
    }

    /// An exec caller whose old image deregisters before any reconnect (its
    /// group killed during the exec) still had Linux walk the group's robust
    /// lists. The deregistration delivers the prepared wakes and holds turns
    /// until the kernel has ended the group, instead of dropping them with
    /// the preparation (Codex review of
    /// https://github.com/rrnewton/hermit/pull/4352, P2). This builds the
    /// deregistration directly: no guest action is known to reach it on DBT
    /// (an exec whose target cannot be loaded reconnects first, then hangs
    /// the run: <https://github.com/rrnewton/hermit/issues/4362>).
    #[tokio::test]
    async fn a_terminal_exec_wakes_its_robust_waiters() {
        let (config, state, leader, detpid) = cancellation_test_state();
        install_test_registration(&state, leader, Ivar::new());
        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let (exit_tx, exit_rx) = std::sync::mpsc::channel::<()>();
        let host_thread = std::thread::spawn(move || {
            tid_tx
                .send(unsafe { libc::syscall(libc::SYS_gettid) as i32 })
                .unwrap();
            let _ = exit_rx.recv();
        });
        state
            .sched
            .lock()
            .unwrap()
            .register_physical_thread(
                leader,
                MmId::initial(detpid),
                std::process::id() as i32,
                tid_rx.recv().unwrap(),
            )
            .unwrap();
        let futex = FutexID::private(MmId::initial(detpid), 0x4000);
        let waiter = park_exec_robust_waiter(&state, futex);
        // A second waiter on the same word stays parked: one wake per marked
        // word (Claude re-check of
        // https://github.com/rrnewton/hermit/pull/4352, N3).
        let second = park_exec_robust_waiter_as(&state, futex, 41);
        let prepared = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(leader),
                    GlobalRequest::PrepareExec(
                        detpid,
                        MmId::initial(detpid),
                        Box::new(ExecFdState {
                            robust_wakes: vec![(leader, futex)],
                            ..Default::default()
                        }),
                    ),
                ),
            )
            .await;
        assert_eq!(prepared.1, GlobalResponse::PrepareExec(()));
        assert!(!state.sched.lock().unwrap().admission_pending(waiter));

        state
            .recv_deregister_thread(
                reverie::Tid::from_raw(leader.as_raw()),
                ThreadDeregistration {
                    thread_start_entered: true,
                    dettid: leader,
                    detpid,
                    mm: MmId::initial(detpid),
                    timeslice_stats: TimesliceStats::default(),
                    syscall_count: 0,
                    chaos_epochs: Vec::new(),
                },
            )
            .await;
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        {
            let scheduler = state.sched.lock().unwrap();
            assert!(scheduler.admission_pending(waiter));
            assert!(!scheduler.admission_pending(second));
            assert!(scheduler.holds_turns_until_gone(leader));
        }
        exit_tx.send(()).unwrap();
        host_thread.join().unwrap();
    }

    #[tokio::test]
    async fn nonleader_exec_rebinds_caller_to_leader_and_preserves_its_clock() {
        let (config, state, leader, detpid) = cancellation_test_state();
        let worker = DetTid::from_raw(leader.as_raw() + 1);
        let sibling = DetTid::from_raw(leader.as_raw() + 2);
        let old_mm = MmId::initial(detpid).for_exec(detpid);
        let leader_request = Ivar::new();
        let worker_request = Ivar::new();
        let sibling_request = Ivar::new();
        install_test_registration(&state, leader, leader_request.clone());
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.thread_tree.add_child(leader, worker, false);
            scheduler.thread_tree.add_child(leader, sibling, false);
        }
        install_test_registration(&state, worker, worker_request.clone());
        install_test_registration(&state, sibling, sibling_request.clone());
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler
                .next_turns
                .get_mut(&sibling)
                .unwrap()
                .child_tid_addr = 0x5678;
            scheduler
                .timeslices
                .insert(leader, Some(LogicalTime::from_nanos(99)));
            scheduler.install_test_vfork_barrier(leader, sibling);
        }

        let mut leader_clock = DetTime::new(&config);
        leader_clock.add_syscall();
        let mut worker_clock = DetTime::new(&config);
        worker_clock.add_syscall();
        worker_clock.add_syscall();
        worker_clock.add_syscall();
        {
            let mut global_time = state.global_time.lock().unwrap();
            global_time.update_global_time(leader, leader_clock.as_nanos(), LogicalTime::ZERO);
            global_time.update_global_time(worker, worker_clock.as_nanos(), LogicalTime::ZERO);
        }
        let total_before = state.global_time.lock().unwrap().as_nanos();
        let fd_blocking = ExecFdState {
            blocking: [42].into_iter().collect(),
            inherited_reads: std::collections::BTreeMap::from([(
                43,
                crate::fd::InheritedExternalRead {
                    origin: crate::types::OpenFileId::new(leader, 0),
                    dev: 1,
                    inode: 2,
                    rdev: 3,
                    file_type: libc::S_IFCHR,
                    access_mode: libc::O_RDONLY,
                    resource: Some(ResourceID::Device(crate::resources::Device::ContainerStdin)),
                },
            )]),
            initialize_inherited_stdio: false,
            initial_input: None,
            robust_wakes: Vec::new(),
            open_files: None,
            maps: None,
            channels: Default::default(),
            open_file_namespace: None,
            exe: None,
        };
        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                caller: worker,
                process: detpid,
                mm: old_mm,
                fd_blocking: fd_blocking.clone(),
            },
        );
        let fresh_local_time = DetTime::new(&config);

        let create_response = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    fresh_local_time.clone(),
                    MmId::initial(leader),
                    GlobalRequest::CreateChildThread(
                        leader,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(
            create_response,
            (
                Some(worker_clock.as_nanos()),
                GlobalResponse::CreateChildThread(Some(old_mm.for_exec(detpid)))
            )
        );
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert_eq!(
            state.post_exec_fd_blocking.lock().unwrap().get(&leader),
            Some(&fd_blocking)
        );

        let late_old_request = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    leader_clock.clone(),
                    old_mm,
                    GlobalRequest::RequestResources(Resources::new(leader), detpid),
                ),
            )
            .await;
        assert_eq!(late_old_request, (None, GlobalResponse::ThreadExited));
        let admitted_before_fence = state
            .recv_request_resources(
                reverie::Tid::from_raw(leader.as_raw()),
                detpid,
                Resources::new(leader),
                Some(old_mm),
            )
            .await;
        assert_eq!(
            admitted_before_fence,
            (SchedulerRpcResult::ThreadExited, None)
        );
        let late_old_deregister = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    leader_clock.clone(),
                    old_mm,
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        thread_start_entered: true,
                        dettid: leader,
                        detpid,
                        mm: old_mm,
                        timeslice_stats: TimesliceStats::default(),
                        syscall_count: 0,
                        chaos_epochs: Vec::new(),
                    }),
                ),
            )
            .await;
        assert_eq!(
            late_old_deregister,
            (None, GlobalResponse::DeregisterThread(()))
        );
        let duplicate_create = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    fresh_local_time.clone(),
                    MmId::initial(leader),
                    GlobalRequest::CreateChildThread(
                        leader,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(duplicate_create, (None, GlobalResponse::ThreadExited));

        {
            let scheduler = state.sched.lock().unwrap();
            assert!(!scheduler.thread_is_logically_killed(leader));
            assert!(scheduler.thread_is_logically_killed(worker));
            assert!(scheduler.thread_is_logically_killed(sibling));
            assert_eq!(scheduler.next_turns.len(), 1);
            assert!(scheduler.next_turns.contains_key(&leader));
            assert!(!scheduler.timeslices.contains_key(&leader));
            assert!(!scheduler.vfork_barrier_mentions(leader));
            assert!(!scheduler.vfork_barrier_mentions(sibling));
            assert!(matches!(leader_request.try_read(), Some(Err(_))));
            assert!(matches!(worker_request.try_read(), Some(Err(_))));
            assert!(matches!(sibling_request.try_read(), Some(Err(_))));
            assert!(
                scheduler
                    .child_tid_was_cleared(FutexID::private(old_mm, 0x5678), sibling.as_raw(),)
            );
        }

        // Drive the real daemon through step2 rather than manually pre-filling
        // the replacement's response. This drains the destroyed leader's
        // physical removal and the same-raw-TID replacement admission before
        // StartNewThread supplies the fresh image's first request.
        let turn_sched = state.sched.clone();
        let turn_time = state.global_time.clone();
        let turn = tokio::spawn(async move {
            let last: Result<Resources, crate::scheduler::SkipTurn> =
                Err(crate::scheduler::SkipTurn);
            crate::scheduler::do_a_turn_blocking(turn_sched, turn_time, &last).await
        });
        let start_response = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    fresh_local_time,
                    old_mm.for_exec(detpid),
                    GlobalRequest::StartNewThread(leader, detpid, None, None),
                ),
            )
            .await;
        assert!(
            turn.await
                .expect("replacement scheduler turn panicked")
                .is_ok(),
            "replacement leader did not survive the first step2 drain"
        );
        assert_eq!(
            start_response,
            (
                Some(worker_clock.as_nanos()),
                granted(GlobalResponse::StartNewThread(None))
            )
        );
        {
            let scheduler = state.sched.lock().unwrap();
            assert_eq!(
                scheduler
                    .run_queue
                    .tids()
                    .filter(|dettid| **dettid == leader)
                    .count(),
                1
            );
            assert!(!scheduler.run_queue.contains_tid(worker));
            assert!(!scheduler.run_queue.contains_tid(sibling));
        }
        {
            let global_time = state.global_time.lock().unwrap();
            assert_eq!(global_time.as_nanos(), total_before);
            assert_eq!(global_time.threads_time(leader), worker_clock.as_nanos());
            assert!(!global_time.contains_thread(worker));
        }

        let mark_response = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    worker_clock,
                    old_mm.for_exec(detpid),
                    GlobalRequest::MarkPastFirstExecve(detpid, None, None, None),
                ),
            )
            .await;
        assert_eq!(
            mark_response.1,
            GlobalResponse::MarkPastFirstExecve(Box::new(fd_blocking))
        );
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());
    }

    async fn start_external_image(
        tool: &Detcore,
        guest: &mut ExternalRegistrationGuest<'_>,
        expected_grants: &[ResourceID],
    ) {
        use reverie::Tool;

        let state = guest.global;
        let tid = guest.thread.dettid;
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut start = std::pin::pin!(tool.handle_thread_start(guest));
            assert!(futures::poll!(start.as_mut()).is_pending());
            // Initial self-registration requests ParentContinue before
            // StartNewThread. Reload registration has no parent turn.
            // Drive these actual grants, without fabricating responses.
            let grants = async {
                let mut last = Err(crate::scheduler::SkipTurn);
                for expected in expected_grants {
                    let granted = crate::scheduler::do_a_turn_blocking(
                        state.sched.clone(),
                        state.global_time.clone(),
                        &last,
                    )
                    .await
                    .expect("image admission must receive its scheduler grant");
                    assert_eq!(granted.tid, tid);
                    assert!(granted.resources.contains_key(expected));
                    last = Ok(granted);
                }
            };
            let (started, ()) = tokio::join!(start, grants);
            started.unwrap();
        })
        .await
        .expect("image admission must finish without guest instructions");
    }

    // Startup's posthook reads the backend clock even before guest instructions.
    // This dedicated fixture reports exactly zero and counts those reads;
    // the older ExternalRegistrationGuest's host-clock refusal stays intact.
    struct BoundaryAdmissionGuest<'a> {
        inner: ExternalRegistrationGuest<'a>,
        clock_reads: usize,
    }

    impl<'a> std::ops::Deref for BoundaryAdmissionGuest<'a> {
        type Target = ExternalRegistrationGuest<'a>;
        fn deref(&self) -> &Self::Target {
            &self.inner
        }
    }
    impl std::ops::DerefMut for BoundaryAdmissionGuest<'_> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.inner
        }
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for BoundaryAdmissionGuest<'_> {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            self.inner.send_rpc(message).await
        }
        fn config(&self) -> &Config {
            self.inner.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for BoundaryAdmissionGuest<'_> {
        type Memory = reverie::syscalls::LocalMemory;
        type Stack = ExternalRegistrationStack;
        fn tid(&self) -> reverie::Pid {
            self.inner.tid()
        }
        fn pid(&self) -> reverie::Pid {
            self.inner.pid()
        }
        fn ppid(&self) -> Option<reverie::Pid> {
            self.inner.ppid()
        }
        fn memory(&self) -> Self::Memory {
            self.inner.memory()
        }
        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
            &mut self.inner.thread
        }
        fn thread_state(&self) -> &crate::ThreadState<()> {
            &self.inner.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            self.inner.regs().await
        }
        async fn stack(&mut self) -> Self::Stack {
            self.inner.stack().await
        }
        async fn daemonize(&mut self) {
            self.inner.daemonize().await
        }
        async fn inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            syscall: S,
        ) -> Result<i64, reverie::syscalls::Errno> {
            self.inner.inject(syscall).await
        }
        async fn tail_inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            syscall: S,
        ) -> reverie::Never {
            self.inner.tail_inject(syscall).await
        }
        fn set_timer(&mut self, schedule: reverie::TimerSchedule) -> Result<(), reverie::Error> {
            self.inner.set_timer(schedule)
        }
        fn set_timer_precise(
            &mut self,
            schedule: reverie::TimerSchedule,
        ) -> Result<(), reverie::Error> {
            self.inner.set_timer_precise(schedule)
        }
        fn read_clock(&mut self) -> Result<u64, reverie::Error> {
            self.clock_reads += 1;
            Ok(0)
        }
    }

    #[tokio::test]
    async fn boundary_actual_root_admission_keeps_registration_before_start_and_one_reset() {
        use detcore_model::schedule::boundary::Boundary;
        use detcore_model::schedule::boundary::Cause;
        use detcore_model::schedule::boundary::Entry;
        use detcore_model::schedule::boundary::Record;
        use detcore_model::schedule::boundary::Schedule;
        use reverie::Tool;

        use crate::preemptions::boundary_recording::BoundaryRecording;
        use crate::preemptions::boundary_recording::Recording;

        for timers in [true, false] {
            let mut trajectories = Vec::new();
            for mode in 0..3 {
                let temp = tempfile::tempdir().unwrap();
                let root = super::ROOT_DETPID;
                let mut expected =
                    crate::slice_boundary::grant::tests::budget(root, MmId::initial(root), 0)
                        .expected();
                expected.checkin = 3;
                expected.entry = Entry::Cpuid;
                expected.cause = Cause::SoftwareCheckin;
                if !timers {
                    expected.entry = Entry::Syscall {
                        syscall: reverie::syscalls::Sysno::sched_yield,
                        occurrence: 1,
                    };
                    expected.cause = Cause::ExplicitSchedYield;
                }
                let input = temp.path().join("input");
                Recording::Boundaries(
                    BoundaryRecording::new(
                        PreemptionRecord::from_sched_events(vec![]),
                        Schedule::new(vec![Record::TimesliceEnd(Boundary {
                            identity: expected,
                            count: 1,
                            start_rip: None,
                            end_rip: None,
                            end_time: None,
                        })])
                        .unwrap(),
                    )
                    .unwrap(),
                )
                .write(&input)
                .unwrap();
                let config = Config {
                    sequentialize_threads: true,
                    no_rcb_time: true,
                    max_timeslice: if timers {
                        Config::default().max_timeslice
                    } else {
                        None
                    },
                    record_preemptions: mode == 1,
                    strict_schedule_boundaries: mode != 0,
                    record_preemptions_to: (mode == 1).then(|| temp.path().join("output")),
                    replay_schedule_from: (mode == 2).then_some(input),
                    ..Config::default()
                }
                .with_backend(|backend| backend.delivers_preemption_timer_events = false);
                let state = GlobalState::initialize(&config, false);
                let tool = Detcore::new(Tid::from_raw(root.as_raw()), &config);
                let mut guest = BoundaryAdmissionGuest {
                    inner: ExternalRegistrationGuest {
                        global: &state,
                        config: &config,
                        thread: tool.init_thread_state(Tid::from_raw(root.as_raw()), None),
                        requests: Mutex::new(Vec::new()),
                        post_exec: false,
                        ppid: None,
                        private_memory: None,
                    },
                    clock_reads: 0,
                };
                guest.thread.detpid = Some(root);
                assert!(guest.thread.slice_boundary.is_none());
                tokio::time::timeout(Duration::from_secs(2), async {
                    let mut start = std::pin::pin!(tool.handle_thread_start(&mut guest));
                    assert!(futures::poll!(start.as_mut()).is_pending());
                    let grants = async {
                        let first = crate::scheduler::do_a_turn_blocking(
                            state.sched.clone(),
                            state.global_time.clone(),
                            &Err(crate::scheduler::SkipTurn),
                        )
                        .await
                        .unwrap();
                        assert_eq!(first.resources.len(), 1);
                        assert!(first.resources.contains_key(&ResourceID::ParentContinue {
                            parent: root,
                            child: root,
                        }));
                        // Poll the real startup continuation concurrently. Wait for
                        // its next actual request before inspecting the mailbox.
                        let next = state.sched.lock().unwrap().next_turns[&root].req.clone();
                        let pending = next.get().await.unwrap();
                        assert!(
                            pending
                                .resources
                                .contains_key(&ResourceID::MemAddrSpace(root))
                        );
                        if mode == 2 {
                            let mut different = expected;
                            different.committed_rcbs = 1;
                            let schedule = Schedule::new(vec![Record::TimesliceEnd(Boundary {
                                identity: different,
                                count: 1,
                                start_rip: None,
                                end_rip: None,
                                end_time: None,
                            })])
                            .unwrap();
                            let different =
                                crate::slice_boundary::grant::Budget::at(&schedule, 0).unwrap();
                            // A conflicting queue would succeed if ParentContinue had
                            // consumed the pending budget. It must still be present.
                            assert!(
                                state
                                    .sched
                                    .lock()
                                    .unwrap()
                                    .boundary_budgets
                                    .queue(MmId::initial(root), different)
                                    .is_err()
                            );
                        }
                        let second = crate::scheduler::do_a_turn_blocking(
                            state.sched.clone(),
                            state.global_time.clone(),
                            &Ok(first),
                        )
                        .await
                        .unwrap();
                        assert_eq!(second.resources.len(), 1);
                        assert!(
                            second
                                .resources
                                .contains_key(&ResourceID::MemAddrSpace(root))
                        );
                    };
                    let (started, ()) = tokio::join!(start, grants);
                    started.unwrap();
                })
                .await
                .expect("the two original admission grants must finish");
                assert_eq!(guest.thread.stats.timeslice_count, u64::from(timers));
                let requests = guest.requests.lock().unwrap();
                let registration = requests
                    .iter()
                    .position(|r| matches!(r, GlobalRequest::CreateChildThread(..)))
                    .unwrap();
                let start = requests
                    .iter()
                    .position(|r| matches!(r, GlobalRequest::StartNewThread(..)))
                    .unwrap();
                assert!(registration < start);
                if mode != 0 {
                    let observed =
                        serde_json::to_value(guest.thread.slice_boundary.as_ref().unwrap())
                            .unwrap();
                    assert_eq!(observed["start_reset_done"], true);
                    assert_eq!(observed["checkin"], 2);
                    assert_eq!(observed["completed_slices"], 0);
                    assert_eq!(observed["current"][0], "ThreadStart");
                    assert_eq!(observed["current"][1], "Posthook");
                    assert_eq!(observed["pending_start_budget"], serde_json::Value::Null);
                    assert_eq!(observed["expected_budget"].is_null(), mode != 2);
                }
                let sched = state.sched.lock().unwrap();
                trajectories.push((
                    sched.turn,
                    sched.committed_time,
                    guest.thread.thread_logical_time.as_nanos(),
                    guest.thread.stats.timeslice_count,
                    guest.thread.committed_clock_value,
                    guest.thread.scheduler_grants,
                    guest.clock_reads,
                ));
            }
            assert_eq!(trajectories[0], trajectories[1]);
            assert_eq!(trajectories[0], trajectories[2]);
        }
    }

    #[test]
    fn boundary_self_registration_is_not_a_parent_child_or_ordinary_grant() {
        let root = super::ROOT_DETPID;
        let request =
            GlobalRequest::CreateChildThread(root, root, 0, None, libc::SIGCHLD, None, None);
        assert!(super::registers_own_image(root, &request));
        assert!(!super::registers_own_image(DetTid::from_raw(17), &request));
        assert!(super::registers_own_image(
            root,
            &GlobalRequest::WithForwardedCount(4, Box::new(request))
        ));
        assert!(!super::registers_own_image(
            root,
            &GlobalRequest::StartNewThread(root, root, None, None)
        ));
        assert!(!super::registers_own_image(
            root,
            &GlobalRequest::ResumeExec(root)
        ));
        assert!(!super::registers_own_image(
            root,
            &GlobalRequest::GlobalTimeLowerBound
        ));
        let vfork = GlobalRequest::CreateVforkChildThread(
            root,
            root,
            DetTid::from_raw(17),
            0,
            CloneFlags::CLONE_VFORK | CloneFlags::CLONE_VM,
            libc::SIGCHLD,
            None,
        );
        assert!(!super::registers_own_image(root, &vfork));
        assert!(super::registers_own_image(DetTid::from_raw(17), &vfork));

        let mut thread = crate::ThreadState::new(root, &Config::default(), ());
        let owner = crate::slice_boundary::lifecycle::Registry::default()
            .fresh(root, thread.mm_id)
            .unwrap();
        assert!(crate::slice_boundary::adopt_owner(&mut thread, owner, false).is_err());
        assert!(thread.slice_boundary.is_none());
    }

    /// A reloading backend supplies a real in-tree parent for a fresh image.
    /// That image must reconnect its prepared exec before ordinary requests can
    /// publish the replacement native memory owner.
    #[tokio::test]
    async fn a_reloaded_descendant_reconnects_before_publishing_its_new_memory_owner() {
        use reverie::Tool;

        let parent = super::ROOT_DETPID;
        let child = DetTid::from_raw(17);
        for (case, old_mm) in [
            ("first exec", MmId::initial(child)),
            ("later exec", MmId::initial(child).for_exec(child)),
        ] {
            let config = Config {
                sequentialize_threads: true,
                max_timeslice: None,
                no_rcb_time: true,
                ..Config::default()
            }
            .with_backend(|backend| backend.needs_killed_thread_rpc_cancellation = true);
            let state = GlobalState::initialize(&config, false);
            let facts = |epoch| {
                reverie::RuntimePrivateMemory::Ready(reverie::RuntimePrivateMemorySnapshot {
                    owner: [1, 2, 3, epoch],
                    version: 1,
                    regions: vec![reverie::RuntimePrivateMemoryRegion {
                        start: 0x1000,
                        end: 0x2000,
                    }],
                })
            };
            // Seed the already registered descendant's old image. The exec and
            // replacement-image admission below use actual global RPCs and grants.
            {
                let mut sched = state.sched.lock().unwrap();
                sched.thread_tree.add_child(parent, parent, true);
                sched.thread_tree.add_child(parent, child, true);
                assert_eq!(sched.thread_tree.process_of(child), Some(child), "{case}");
                assert_eq!(
                    sched.thread_tree.parent_process(&child),
                    Some(parent),
                    "{case}"
                );
                sched.record_thread_mm(child, old_mm);
                if old_mm != MmId::initial(child) {
                    sched.install_test_exec_incarnation(child, old_mm);
                }
            }
            install_test_registration(&state, child, Ivar::new());
            let tool = Detcore::new(reverie::Pid::from_raw(child.as_raw()), &config);
            let mut old = ExternalRegistrationGuest {
                global: &state,
                config: &config,
                thread: tool.init_thread_state(Tid::from_raw(child.as_raw()), None),
                requests: Mutex::new(Vec::new()),
                post_exec: false,
                ppid: Some(reverie::Pid::from_raw(parent.as_raw())),
                private_memory: Some(facts(1)),
            };
            old.thread.detpid = Some(child);
            old.thread.mm_id = old_mm;
            old.thread.past_global_first_execve = true;
            start_external_image(&tool, &mut old, &[ResourceID::MemAddrSpace(child)]).await;
            super::send_and_update_time(&mut old, GlobalRequest::GlobalTimeLowerBound).await;
            assert_eq!(
                state.sched.lock().unwrap().runtime_private_memory(old_mm),
                Some(facts(1)),
                "{case}"
            );
            super::prepare_exec(&mut old, old_mm, Default::default(), Vec::new(), None).await;
            assert!(
                state
                    .pending_exec_states
                    .lock()
                    .unwrap()
                    .contains_key(&child),
                "{case}"
            );
            // Assert the old image's authority before the successor's first
            // request. The obsolete key need not remain Ready after exec.
            assert_eq!(
                state.sched.lock().unwrap().runtime_private_memory(old_mm),
                Some(facts(1)),
                "{case}: prepared old image"
            );
            let registered_children = state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .get_children(&parent)
                .clone();
            drop(old);

            let mut successor = ExternalRegistrationGuest {
                global: &state,
                config: &config,
                thread: tool.init_thread_state(Tid::from_raw(child.as_raw()), None),
                requests: Mutex::new(Vec::new()),
                post_exec: false,
                ppid: Some(reverie::Pid::from_raw(parent.as_raw())),
                private_memory: Some(facts(2)),
            };
            successor.thread.detpid = Some(child);
            assert!(!successor.is_root_process(), "{case}");
            assert!(!successor.thread.past_global_first_execve, "{case}");
            start_external_image(&tool, &mut successor, &[ResourceID::MemAddrSpace(child)]).await;
            let new_mm = old_mm.for_exec(child);
            assert_eq!(successor.thread.mm_id, new_mm, "{case}");
            assert!(
                state.pending_exec_states.lock().unwrap().is_empty(),
                "{case}"
            );
            super::send_and_update_time(&mut successor, GlobalRequest::GlobalTimeLowerBound).await;
            let sched = state.sched.lock().unwrap();
            assert_eq!(sched.thread_mm(child), Some(new_mm), "{case}");
            assert_eq!(
                sched.thread_tree.get_children(&parent),
                &registered_children,
                "{case}: exec is not a new child birth"
            );
            assert_eq!(sched.thread_tree.process_of(child), Some(child), "{case}");
            assert_eq!(
                sched.thread_tree.parent_process(&child),
                Some(parent),
                "{case}"
            );
            assert_eq!(
                sched.runtime_private_memory(new_mm),
                Some(facts(2)),
                "{case}"
            );
            let requests = successor.requests.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| matches!(request, GlobalRequest::CreateChildThread(..)))
                    .count(),
                1,
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn reloaded_post_exec_restores_prepared_input_aliases_once() {
        use nix::fcntl::OFlag;
        use reverie::Tool;

        let config = Config {
            sequentialize_threads: true,
            max_timeslice: None,
            ..Config::default()
        }
        .with_backend(|backend| {
            backend.needs_killed_thread_rpc_cancellation = true;
            backend.tool_shares_guest_descriptor_table = true;
        });
        let mut state = GlobalState::initialize(&config, false);
        let leader = super::ROOT_DETPID;
        let tool = Detcore::new(reverie::Pid::from_raw(leader.as_raw()), &config);

        // Use the real launch-input owner and its registered-root seed, in a
        // privately owned slot rather than the test harness's stdin.
        let reader: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let fd = reader.as_raw_fd();
        let input = reverie::InheritedInput::pipe(fd).unwrap();
        let identity = input.identity().clone();
        state.install_inherited_input(input.lease().unwrap());
        let (source, _writer) = input.into_reader_and_writer();
        // SAFETY: dup2 replaces only this test's owned destination.
        assert_eq!(unsafe { libc::dup2(source.as_raw_fd(), fd) }, fd);
        drop(source);
        let alias = reader.try_clone().unwrap();
        let cloexec = reader.try_clone().unwrap();
        let alias_fd = alias.as_raw_fd();
        let cloexec_fd = cloexec.as_raw_fd();
        // SAFETY: only the test-owned alias's descriptor flag is changed.
        assert_eq!(unsafe { libc::fcntl(alias_fd, libc::F_SETFD, 0) }, 0);
        let flags = |fd| {
            // SAFETY: both no-argument queries inspect owned descriptors.
            let result = unsafe {
                (
                    libc::fcntl(fd, libc::F_GETFL),
                    libc::fcntl(fd, libc::F_GETFD),
                )
            };
            assert!(result.0 >= 0 && result.1 >= 0);
            result
        };
        let kernel_before = [flags(fd), flags(alias_fd)];
        assert_eq!(
            kernel_before[0].0 & (libc::O_ACCMODE | libc::O_NONBLOCK),
            libc::O_RDONLY
        );
        assert_eq!(flags(cloexec_fd).1 & libc::FD_CLOEXEC, libc::FD_CLOEXEC);

        let mut old = ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread: tool.init_thread_state(Tid::from_raw(leader.as_raw()), None),
            requests: Mutex::new(Vec::new()),
            post_exec: true,
            ppid: None,
            private_memory: None,
        };
        old.thread.detpid = Some(leader);
        start_external_image(
            &tool,
            &mut old,
            &[
                ResourceID::ParentContinue {
                    parent: leader,
                    child: leader,
                },
                ResourceID::MemAddrSpace(leader),
            ],
        )
        .await;
        tool.handle_post_exec(&mut old).await.unwrap();
        assert!(state.inherited_input.lock().unwrap().is_none());
        old.thread.dup_fd(fd, alias_fd, OFlag::empty()).unwrap();
        old.thread.dup_fd(fd, cloexec_fd, OFlag::O_CLOEXEC).unwrap();
        let origin = old.thread.file_metadata.lock().unwrap().file_handles[&fd]
            .inherited_external_read()
            .expect("the actual registered launch seed must establish read provenance");
        assert_eq!(
            (origin.dev, origin.inode, origin.rdev),
            (identity.dev(), identity.inode(), identity.rdev())
        );
        let old_mm = old.thread.mm_id;
        super::prepare_exec(&mut old, old_mm, Default::default(), Vec::new(), None).await;
        {
            let pending = state.pending_exec_states.lock().unwrap();
            let prepared = &pending[&leader].fd_blocking.inherited_reads;
            assert_eq!(
                prepared.keys().copied().collect::<BTreeSet<_>>(),
                BTreeSet::from([fd, alias_fd])
            );
            assert!(prepared.values().all(|entry| entry == &origin));
            assert!(!prepared.contains_key(&cloexec_fd));
        }
        // Model the physical successful-exec boundary: the old Tool state is
        // gone and Linux has closed the CLOEXEC alias. No native exec is claimed.
        drop(old);
        drop(cloexec);
        let mut successor = ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread: tool.init_thread_state(Tid::from_raw(leader.as_raw()), None),
            requests: Mutex::new(Vec::new()),
            post_exec: true,
            ppid: None,
            private_memory: None,
        };
        successor.thread.detpid = Some(leader);
        start_external_image(&tool, &mut successor, &[ResourceID::MemAddrSpace(leader)]).await;
        assert_eq!(successor.thread.mm_id, old_mm.for_exec(leader));
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert_eq!(
            state.post_exec_fd_blocking.lock().unwrap()[&leader]
                .inherited_reads
                .len(),
            2
        );
        assert!(
            !successor
                .thread
                .file_metadata
                .lock()
                .unwrap()
                .file_handles
                .contains_key(&fd)
        );

        // This is the actual Tool callback, not a raw completion-RPC check or
        // a direct metadata restore. Empty auxv deliberately excludes AT_RANDOM.
        tool.handle_post_exec(&mut successor).await.unwrap();
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());
        {
            let metadata = successor.thread.file_metadata.lock().unwrap();
            for slot in [fd, alias_fd] {
                let entry = metadata
                    .file_handles
                    .get(&slot)
                    .expect("post-exec must restore every prepared surviving input alias");
                assert_eq!(
                    entry.inherited_external_read(),
                    Some(origin.clone()),
                    "post-exec must restore transferred origin, not plain pipe discovery"
                );
                assert_eq!(entry.ty(), crate::fd::FdType::Pipe);
                assert!(!entry.is_nonblocking());
                assert!(!entry.physically_nonblocking());
                assert!(!entry.is_cloexec());
                assert_eq!(entry.resource(), origin.resource);
            }
            assert_eq!(
                metadata.file_handles[&fd].open_file_id(),
                metadata.file_handles[&alias_fd].open_file_id()
            );
            assert_eq!(metadata.file_handles[&fd].open_file_alias_count(), 2);
            assert!(!metadata.file_handles.contains_key(&cloexec_fd));
        }
        assert_eq!([flags(fd), flags(alias_fd)], kernel_before);
        let snapshot =
            || serde_json::to_value(&*successor.thread.file_metadata.lock().unwrap()).unwrap();
        let before_rpc = snapshot();
        let response = successor
            .send_rpc((
                successor.thread.thread_logical_time.clone(),
                successor.thread.mm_id,
                GlobalRequest::GlobalTimeLowerBound,
            ))
            .await;
        assert!(matches!(
            response.1,
            GlobalResponse::GlobalTimeLowerBound(_)
        ));
        assert_eq!(
            snapshot(),
            before_rpc,
            "ordinary admitted RPC must leave restored metadata unchanged"
        );
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        let requests = successor.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| matches!(r, GlobalRequest::MarkPastFirstExecve(..)))
                .count(),
            1
        );
        assert!(matches!(
            requests.first(),
            Some(GlobalRequest::CreateChildThread(..))
        ));
        assert!(matches!(
            requests.get(1),
            Some(GlobalRequest::StartNewThread(..))
        ));
        assert!(matches!(
            requests.last(),
            Some(GlobalRequest::GlobalTimeLowerBound)
        ));
    }

    #[tokio::test]
    async fn post_exec_deletes_armed_disarmed_and_non_notifying_timers() {
        use reverie::Tool;

        let config = Config {
            sequentialize_threads: true,
            // This test enters one callback while the caller owns its turn;
            // no guest instructions or PMU timer are needed to drive it.
            max_timeslice: None,
            ..Config::default()
        }
        .with_backend(|backend| {
            backend.needs_killed_thread_rpc_cancellation = true;
        });
        let state = GlobalState::initialize(&config, false);
        let leader = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(leader, leader, true);
        let next_request = Ivar::new();
        install_test_registration(&state, leader, next_request.clone());
        let tool = Detcore::new(reverie::Pid::from_raw(leader.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(leader.as_raw()), None);
        thread.detpid = Some(detpid);
        // The exec's own syscall, intercepted and counted, as a backend that
        // traces the guest counts it before post-exec. (A backend that starts
        // the guest already executing reaches its first post-exec with nothing
        // counted, and post-exec charges the execve itself.)
        thread.stats.count_syscall();
        thread.thread_logical_time.add_syscall_with_cost(137);
        let mut guest = ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: true,
            ppid: None,
            private_memory: None,
        };
        let t = LogicalTime::from_nanos;
        let [periodic, disarmed, silent] = {
            let mut timers = guest.thread.posix_timers.lock().unwrap();
            let periodic = timers.create(Some(libc::SIGUSR2), None, None);
            let disarmed = timers.create(Some(libc::SIGALRM), None, None);
            let silent = timers.create(None, None, None);
            timers.settime(periodic, 50, Some(t(100)), t(0));
            timers.settime(silent, 0, Some(t(200)), t(0));
            [periodic, disarmed, silent]
        };

        // Drive the production callback, including its actual global RPCs.
        // Removing its clear_for_exec call must leave these IDs live and fail.
        // A second successful exec also exercises deletion of an empty table.
        for _ in 0..2 {
            let old_mm = guest.thread.mm_id;
            super::prepare_exec(&mut guest, old_mm, Default::default(), Vec::new(), None).await;
            guest.thread.mm_id = old_mm.for_exec(detpid);
            let before = guest.thread.thread_logical_time.as_nanos();
            tokio::time::timeout(Duration::from_secs(2), tool.handle_post_exec(&mut guest))
                .await
                .expect("post-exec must finish within the caller's scheduler turn")
                .unwrap();
            assert_eq!(guest.thread.thread_logical_time.as_nanos(), before);
            assert!(
                next_request.try_read().is_none(),
                "post-exec unexpectedly yielded"
            );
            let mut timers = guest.thread.posix_timers.lock().unwrap();
            for id in [periodic, disarmed, silent] {
                assert!(!timers.contains(id), "post-exec retained POSIX timer {id}");
                assert_eq!(timers.gettime(id, t(10)), None);
                assert_eq!(timers.settime(id, 0, Some(t(300)), t(10)), None);
                assert_eq!(timers.signal(id), None);
                assert!(!timers.remove(id));
            }
        }
        let mut timers = guest.thread.posix_timers.lock().unwrap();
        let new = timers.create(Some(libc::SIGUSR1), None, None);
        assert_eq!(new, 3);
        assert_eq!(timers.settime(new, 0, Some(t(300)), t(10)), Some((0, 0)));
        assert_eq!(timers.gettime(new, t(20)), Some((280, 0)));
    }

    /// A state like [`cancellation_test_state`]'s for a run that declared
    /// `launcher` as the image the kernel execs in the guest's place.
    fn exec_record_test_state(
        sequentialize_threads: bool,
        launcher: &std::path::Path,
    ) -> (Config, GlobalState, DetTid, DetPid) {
        let config = Config {
            sequentialize_threads,
            launch_image: Some(crate::LaunchImage {
                exe: None,
                launcher: Some(launcher.to_path_buf()),
            }),
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let dettid = DetTid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, dettid, true);
        (config, state, dettid, DetPid::from_raw(17))
    }

    /// An exec's record stands only where the kernel's link, as the new
    /// image read it, names the recorded file or the launcher the run
    /// declared; it is what other processes then read for the process, and a
    /// dropped record leaves them nothing. Without sequentialized threads no
    /// record is kept.
    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // std::fs::canonicalize is the independent oracle
    async fn an_exec_record_stands_only_where_the_kernel_names_it_or_the_launcher() {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let directory = std::fs::canonicalize(directory.path()).unwrap();
        let (image, launcher, other) = (
            directory.join("image"),
            directory.join("launcher"),
            directory.join("other"),
        );
        for file in [&image, &launcher, &other] {
            std::fs::write(file, file.as_os_str().as_encoded_bytes()).unwrap();
        }
        let identity = |path: &std::path::Path| {
            let meta = std::fs::metadata(path).unwrap();
            (meta.dev(), meta.ino())
        };
        let record = crate::exec_record::ExecRecord::at(std::path::Path::new("/"), &image).unwrap();
        for (sequentialize, kernel, kept) in [
            (true, identity(&image), true),
            (true, identity(&launcher), true),
            (true, identity(&other), false),
            (false, identity(&image), false),
        ] {
            let (config, state, leader, detpid) = exec_record_test_state(sequentialize, &launcher);
            install_test_registration(&state, leader, Ivar::new());
            let clock = DetTime::new(&config);
            let mm = MmId::initial(detpid);
            let tid = reverie::Tid::from_raw(leader.as_raw());
            let prepared = ExecFdState {
                exe: Some(record.clone()),
                ..Default::default()
            };
            let response = state
                .receive_rpc(
                    tid,
                    (
                        clock.clone(),
                        mm,
                        GlobalRequest::PrepareExec(detpid, mm, Box::new(prepared)),
                    ),
                )
                .await;
            assert_eq!(response.1, GlobalResponse::PrepareExec(()));
            let response = state
                .receive_rpc(
                    tid,
                    (
                        clock,
                        mm.for_exec(detpid),
                        GlobalRequest::MarkPastFirstExecve(detpid, None, None, Some(kernel)),
                    ),
                )
                .await;
            let GlobalResponse::MarkPastFirstExecve(overrides) = response.1 else {
                panic!("unexpected response {:?}", response.1);
            };
            let expected = kept.then(|| record.clone());
            assert_eq!(overrides.exe, expected, "{sequentialize} {kernel:?}");
            assert_eq!(
                state.exec_images.lock().unwrap().get(&detpid).cloned(),
                expected,
                "{sequentialize} {kernel:?}"
            );
        }
    }

    /// The launch's first image goes to the registered root's first
    /// completed image only: another thread's completion, first or not,
    /// gets none, and the root's later images get none from the launch. The
    /// root's record stands without sequentialized threads too, as it comes
    /// from no preparation.
    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // std::fs::canonicalize is the independent oracle
    async fn the_launch_image_goes_to_the_registered_roots_first_image_only() {
        let directory = tempfile::tempdir().unwrap();
        let directory = std::fs::canonicalize(directory.path()).unwrap();
        let (image, launcher) = (directory.join("image"), directory.join("launcher"));
        for file in [&image, &launcher] {
            std::fs::write(file, file.as_os_str().as_encoded_bytes()).unwrap();
        }
        let record = crate::exec_record::ExecRecord::at(std::path::Path::new("/"), &image).unwrap();
        for sequentialize in [true, false] {
            let config = Config {
                sequentialize_threads: sequentialize,
                launch_image: Some(crate::LaunchImage {
                    exe: Some(image.clone()),
                    launcher: Some(launcher.clone()),
                }),
                ..Config::default()
            };
            let state = GlobalState::initialize(&config, false);
            let (root, other) = (DetTid::from_raw(17), DetTid::from_raw(23));
            for tid in [root, other] {
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .add_child(tid, tid, true);
                install_test_registration(&state, tid, Ivar::new());
            }
            state.root_dettid.set(root).unwrap();
            let clock = DetTime::new(&config);
            let mark = |tid: DetTid| {
                let process = DetPid::from_raw(tid.as_raw());
                (
                    reverie::Tid::from_raw(tid.as_raw()),
                    (
                        clock.clone(),
                        MmId::initial(process),
                        GlobalRequest::MarkPastFirstExecve(process, None, None, None),
                    ),
                )
            };
            for (tid, expected) in [(other, None), (root, Some(&record)), (root, None)] {
                let (from, request) = mark(tid);
                let response = state.receive_rpc(from, request).await;
                let GlobalResponse::MarkPastFirstExecve(overrides) = response.1 else {
                    panic!("unexpected response {:?}", response.1);
                };
                assert_eq!(overrides.exe.as_ref(), expected, "{sequentialize} {tid:?}");
            }
        }
    }

    /// The record a new image resolved from its own `AT_EXECFN` (the
    /// unprepared one) fills in only where no exec was prepared, never
    /// outranks a prepared record, never stands for the launcher, and is
    /// dropped without sequentialized threads, as a prepared one is. For the
    /// registered root's first image it outranks the launch's own search,
    /// which stands where the image gave none or named the launcher; in a run
    /// that declares no launcher, the launch's record stands at the root
    /// whatever the image's exec prepared, as before.
    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // std::fs::canonicalize is the independent oracle
    async fn an_images_own_record_fills_in_and_outranks_only_the_launchs_search() {
        let directory = tempfile::tempdir().unwrap();
        let directory = std::fs::canonicalize(directory.path()).unwrap();
        let (image, other, launcher) = (
            directory.join("image"),
            directory.join("other"),
            directory.join("launcher"),
        );
        for file in [&image, &other, &launcher] {
            std::fs::write(file, file.as_os_str().as_encoded_bytes()).unwrap();
        }
        let record_of = |path: &std::path::Path| {
            crate::exec_record::ExecRecord::at(std::path::Path::new("/"), path)
        };
        let (image_record, other_record, launcher_record) =
            (record_of(&image), record_of(&other), record_of(&launcher));

        // A process other than the root.
        for (sequentialize, prepared, unprepared, expected) in [
            (true, None, other_record.clone(), other_record.clone()),
            (
                true,
                image_record.clone(),
                other_record.clone(),
                image_record.clone(),
            ),
            (true, None, launcher_record.clone(), None),
            (false, None, other_record.clone(), None),
        ] {
            let (config, state, leader, detpid) = exec_record_test_state(sequentialize, &launcher);
            install_test_registration(&state, leader, Ivar::new());
            let clock = DetTime::new(&config);
            let mm = MmId::initial(detpid);
            let tid = reverie::Tid::from_raw(leader.as_raw());
            let preparation = ExecFdState {
                exe: prepared.clone(),
                ..Default::default()
            };
            let response = state
                .receive_rpc(
                    tid,
                    (
                        clock.clone(),
                        mm,
                        GlobalRequest::PrepareExec(detpid, mm, Box::new(preparation)),
                    ),
                )
                .await;
            assert_eq!(response.1, GlobalResponse::PrepareExec(()));
            let response = state
                .receive_rpc(
                    tid,
                    (
                        clock,
                        mm.for_exec(detpid),
                        GlobalRequest::MarkPastFirstExecve(detpid, None, unprepared.clone(), None),
                    ),
                )
                .await;
            let GlobalResponse::MarkPastFirstExecve(overrides) = response.1 else {
                panic!("unexpected response {:?}", response.1);
            };
            assert_eq!(
                overrides.exe, expected,
                "{sequentialize} prepared {prepared:?} unprepared {unprepared:?}"
            );
        }

        // The registered root's first image of a run that declares no
        // launcher (ptrace): the launch's record stands over the one its
        // bootstrap exec prepared.
        {
            let config = Config {
                sequentialize_threads: true,
                launch_image: Some(crate::LaunchImage {
                    exe: Some(image.clone()),
                    launcher: None,
                }),
                ..Config::default()
            };
            let state = GlobalState::initialize(&config, false);
            let root = DetTid::from_raw(17);
            state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .add_child(root, root, true);
            install_test_registration(&state, root, Ivar::new());
            state.root_dettid.set(root).unwrap();
            let process = DetPid::from_raw(root.as_raw());
            let tid = reverie::Tid::from_raw(root.as_raw());
            let clock = DetTime::new(&config);
            let mm = MmId::initial(process);
            let preparation = ExecFdState {
                exe: other_record.clone(),
                ..Default::default()
            };
            let response = state
                .receive_rpc(
                    tid,
                    (
                        clock.clone(),
                        mm,
                        GlobalRequest::PrepareExec(process, mm, Box::new(preparation)),
                    ),
                )
                .await;
            assert_eq!(response.1, GlobalResponse::PrepareExec(()));
            let response = state
                .receive_rpc(
                    tid,
                    (
                        clock,
                        mm.for_exec(process),
                        GlobalRequest::MarkPastFirstExecve(process, None, None, None),
                    ),
                )
                .await;
            let GlobalResponse::MarkPastFirstExecve(overrides) = response.1 else {
                panic!("unexpected response {:?}", response.1);
            };
            assert_eq!(
                overrides.exe, image_record,
                "no launcher: the launch's record"
            );
        }

        // The registered root's first image of a run that declares a
        // launcher.
        for (launch_exe, unprepared, expected) in [
            (Some(&image), other_record.clone(), other_record.clone()),
            (Some(&image), None, image_record.clone()),
            (None, other_record.clone(), other_record.clone()),
            (Some(&image), launcher_record.clone(), image_record.clone()),
        ] {
            let config = Config {
                sequentialize_threads: true,
                launch_image: Some(crate::LaunchImage {
                    exe: launch_exe.cloned(),
                    launcher: Some(launcher.clone()),
                }),
                ..Config::default()
            };
            let state = GlobalState::initialize(&config, false);
            let root = DetTid::from_raw(17);
            state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .add_child(root, root, true);
            install_test_registration(&state, root, Ivar::new());
            state.root_dettid.set(root).unwrap();
            let process = DetPid::from_raw(root.as_raw());
            let response = state
                .receive_rpc(
                    reverie::Tid::from_raw(root.as_raw()),
                    (
                        DetTime::new(&config),
                        MmId::initial(process),
                        GlobalRequest::MarkPastFirstExecve(process, None, unprepared.clone(), None),
                    ),
                )
                .await;
            let GlobalResponse::MarkPastFirstExecve(overrides) = response.1 else {
                panic!("unexpected response {:?}", response.1);
            };
            assert_eq!(
                overrides.exe, expected,
                "launch {launch_exe:?} unprepared {unprepared:?}"
            );
        }
    }

    #[tokio::test]
    async fn only_successful_exec_cancels_posix_deadlines() {
        let (config, state, leader, detpid) = cancellation_test_state();
        install_test_registration(&state, leader, Ivar::new());
        let clock = DetTime::new(&config);
        let mm = MmId::initial(detpid);
        let deadline = LogicalTime::from_nanos(1_000_000);
        {
            let mut sched = state.sched.lock().unwrap();
            sched.register_alarm(
                detpid,
                leader,
                LogicalTime::ZERO,
                deadline,
                LogicalTime::ZERO,
                Signal::SIGALRM,
            );
            sched.register_posix_timer(
                detpid,
                crate::scheduler::timed_waiters::SignalTarget::Process(leader),
                0,
                Some(deadline),
                deadline,
                Signal::SIGUSR2.into(),
            );
        }
        let before: Vec<_> = state
            .sched
            .lock()
            .unwrap()
            .blocked
            .timed_waiters
            .iter()
            .collect();
        for request in [
            GlobalRequest::PrepareExec(detpid, mm, Default::default()),
            GlobalRequest::CancelExec(detpid),
            GlobalRequest::PrepareExec(detpid, mm, Default::default()),
        ] {
            state
                .receive_rpc(
                    reverie::Tid::from_raw(leader.as_raw()),
                    (clock.clone(), mm, request),
                )
                .await;
            assert_eq!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .blocked
                    .timed_waiters
                    .iter()
                    .collect::<Vec<_>>(),
                before
            );
        }

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    clock,
                    mm.for_exec(detpid),
                    GlobalRequest::MarkPastFirstExecve(detpid, None, None, None),
                ),
            )
            .await;
        assert_eq!(
            response.1,
            GlobalResponse::MarkPastFirstExecve(Default::default())
        );
        let sched = state.sched.lock().unwrap();
        assert_eq!(
            sched.blocked.timed_waiters.alarm_state(detpid),
            Some((deadline, LogicalTime::ZERO))
        );
        assert_eq!(sched.blocked.timed_waiters.iter().count(), 1);
    }

    #[tokio::test]
    async fn failed_exec_clears_prepared_state_without_retiring_siblings() {
        let (config, state, leader, detpid) = cancellation_test_state();
        let sibling = DetTid::from_raw(leader.as_raw() + 1);
        install_test_registration(&state, leader, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(leader, sibling, false);
        install_test_registration(&state, sibling, Ivar::new());
        let clock = DetTime::new(&config);

        let prepared = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    clock.clone(),
                    MmId::initial(leader),
                    GlobalRequest::PrepareExec(detpid, MmId::initial(detpid), Default::default()),
                ),
            )
            .await;
        assert_eq!(prepared.1, GlobalResponse::PrepareExec(()));
        assert!(
            state
                .pending_exec_states
                .lock()
                .unwrap()
                .contains_key(&detpid)
        );

        let cancelled = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    clock,
                    MmId::initial(leader),
                    GlobalRequest::CancelExec(detpid),
                ),
            )
            .await;
        assert_eq!(cancelled.1, GlobalResponse::CancelExec(()));
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        {
            let scheduler = state.sched.lock().unwrap();
            assert!(!scheduler.thread_is_logically_killed(leader));
            assert!(!scheduler.thread_is_logically_killed(sibling));
            assert_eq!(scheduler.next_turns.len(), 2);
        }

        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                caller: leader,
                process: detpid,
                mm: MmId::initial(detpid),
                fd_blocking: Default::default(),
            },
        );
        state.post_exec_fd_blocking.lock().unwrap().insert(
            leader,
            ExecFdState::from([42].into_iter().collect::<ExecFdBlockingOverrides>()),
        );
        state
            .recv_deregister_thread(
                reverie::Tid::from_raw(leader.as_raw()),
                ThreadDeregistration {
                    thread_start_entered: true,
                    dettid: leader,
                    detpid,
                    mm: MmId::initial(detpid),
                    timeslice_stats: TimesliceStats::default(),
                    syscall_count: 0,
                    chaos_epochs: Vec::new(),
                },
            )
            .await;
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());

        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                caller: leader,
                process: detpid,
                mm: MmId::initial(detpid),
                fd_blocking: Default::default(),
            },
        );
        state
            .recv_deregister_thread(
                reverie::Tid::from_raw(leader.as_raw()),
                ThreadDeregistration {
                    thread_start_entered: true,
                    dettid: leader,
                    detpid,
                    mm: MmId::initial(detpid).for_exec(detpid),
                    timeslice_stats: TimesliceStats::default(),
                    syscall_count: 0,
                    chaos_epochs: Vec::new(),
                },
            )
            .await;
        assert!(state.pending_exec_states.lock().unwrap().is_empty());

        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                caller: leader,
                process: detpid,
                mm: MmId::initial(detpid),
                fd_blocking: Default::default(),
            },
        );
        state.post_exec_fd_blocking.lock().unwrap().insert(
            leader,
            ExecFdState::from([42].into_iter().collect::<ExecFdBlockingOverrides>()),
        );
        state.complete_physical_process_exit(detpid.as_raw());
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());
    }

    #[test]
    fn completed_unsupported_syscalls_transfer_once_without_duplicates() {
        let mut state = GlobalState::initialize(&Config::default(), false);
        for name in ["restart_syscall", "io_setup", "restart_syscall"] {
            state
                .unsupported_syscalls
                .get_mut()
                .unwrap()
                .insert(name.to_owned());
        }
        assert_eq!(
            state.take_unsupported_syscalls(),
            ["io_setup".to_owned(), "restart_syscall".to_owned()]
                .into_iter()
                .collect()
        );
        assert!(state.take_unsupported_syscalls().is_empty());
    }

    #[test]
    fn unsupported_syscall_report_duplicate_is_close_on_exec() {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // SAFETY: pipe2 initialized both descriptors and transfers ownership.
        let _reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let config = Config {
            unsupported_syscall_report_fd: Some(writer.as_raw_fd()),
            ..Config::default()
        };

        let state = GlobalState::initialize(&config, false);
        let duplicate = state
            .unsupported_syscall_report_fd
            .as_ref()
            .expect("report writer should be duplicated")
            .lock()
            .unwrap();
        let flags = unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping test.
    #[test]
    fn device_pool_remaps_deterministically() {
        use super::DevicePool;

        // Raw device numbers a guest might observe; the procfs/tmpfs ones
        // (large anon-bdev values) are exactly what drifts between runs.
        let raw_root = 0x20; // e.g. a real block device
        let raw_proc_run1 = 3_145_792; // anon bdev in run 1
        let raw_proc_run2 = 3_145_788; // same procfs, different number in run 2

        // Run 1: observe root then proc.
        let mut pool1 = DevicePool::new();
        let root1 = pool1.determinize(raw_root);
        let proc1 = pool1.determinize(raw_proc_run1);
        // Run 2: same observation order, different raw proc number.
        let mut pool2 = DevicePool::new();
        let root2 = pool2.determinize(raw_root);
        let proc2 = pool2.determinize(raw_proc_run2);

        // The synthetic ids depend only on first-observation order, so they are
        // identical across the two runs despite the raw proc number differing.
        assert_eq!(root1, root2);
        assert_eq!(proc1, proc2);

        // Distinct raw devices get distinct ids; ids start at 1 (never 0).
        assert_ne!(root1, proc1);
        assert_eq!(root1, 1);
        assert_eq!(proc1, 2);

        // Re-observing a raw device is stable within a run.
        assert_eq!(pool1.determinize(raw_root), root1);
        assert_eq!(pool1.determinize(raw_proc_run1), proc1);
    }

    #[test]
    fn the_procfs_device_is_never_minted_for_another_device() {
        use super::DevicePool;

        let mut pool = DevicePool::new();
        pool.next_device = crate::procfs_inode::VIRTUAL_PROC_DEVICE - 1;
        let before = pool.determinize(libc::makedev(8, 1));
        let after = pool.determinize(libc::makedev(8, 2));
        assert_eq!(before, crate::procfs_inode::VIRTUAL_PROC_DEVICE - 1);
        assert_eq!(after, crate::procfs_inode::VIRTUAL_PROC_DEVICE + 1);
    }

    #[test]
    fn this_namespaces_procfs_device_is_detcores() {
        use std::os::unix::fs::MetadataExt;

        use super::DevicePool;

        let mut pool = DevicePool::for_this_mount_namespace();
        let proc = std::fs::metadata("/proc").unwrap().dev();
        assert_eq!(
            pool.determinize(proc),
            crate::procfs_inode::VIRTUAL_PROC_DEVICE
        );
        assert_eq!(pool.determinize(libc::makedev(8, 1)), 1);
    }

    #[test]
    fn mountinfo_prepopulation_is_reused_by_later_stat_observations() {
        use super::DevicePool;

        let mountinfo_devices = [libc::makedev(8, 1), libc::makedev(0, 44)];
        let mut pool = DevicePool::new();
        let rendered = mountinfo_devices
            .into_iter()
            .map(|raw| pool.determinize(raw))
            .collect::<Vec<_>>();

        assert_eq!(rendered, [1, 2]);
        assert_eq!(pool.determinize(libc::makedev(0, 44)), rendered[1]);
        assert_eq!(pool.determinize(libc::makedev(8, 1)), rendered[0]);
        assert_eq!(pool.determinize(libc::makedev(259, 7)), 3);
    }

    #[tokio::test]
    async fn late_futex_rpc_after_thread_removal_returns_eintr() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let dettid = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        let response = state
            .recv_futex_action(
                RpcIncarnation {
                    dettid,
                    mm: MmId::initial(detpid),
                },
                FutexAction::WaitRequest(None, None),
                FutexID::private(MmId::initial(detpid), 0x1000),
                0,
                u32::MAX,
            )
            .await;

        assert!(matches!(
            response,
            Some(SchedValue::Value(value)) if value == nix::errno::Errno::EINTR as u64
        ));
    }

    #[tokio::test]
    async fn late_child_tid_wait_after_exit_returns_spurious_wake() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let detpid = DetPid::from_raw(17);
        let child = DetTid::from_raw(18);
        let futex = FutexID::private(MmId::initial(detpid), 0x1000);
        state.sched.lock().unwrap().next_turns.insert(
            detpid,
            ThreadNextTurn {
                dettid: detpid,
                child_tid_addr: 0,
                req: Ivar::new(),
                resp: Ivar::new(),
                protocol: Default::default(),
            },
        );
        state
            .sched
            .lock()
            .unwrap()
            .wake_futex_child_cleartid(futex, child);
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .child_tid_was_cleared(futex, child.as_raw())
        );
        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .child_tid_was_cleared(futex, child.as_raw() + 1)
        );

        let response = state
            .recv_futex_action(
                RpcIncarnation {
                    dettid: detpid,
                    mm: MmId::initial(detpid),
                },
                FutexAction::WaitRequest(None, None),
                futex,
                child.as_raw(),
                u32::MAX,
            )
            .await;

        assert!(matches!(response, Some(SchedValue::Value(0))));
        assert!(state.sched.lock().unwrap().blocked.futex_waiters.is_empty());
    }

    #[tokio::test]
    async fn late_resource_request_after_logical_kill_is_cancelled() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        let mut current_time = DetTime::new(&config);
        current_time.add_syscall();
        state.global_time.lock().unwrap().update_global_time(
            dettid,
            current_time.as_nanos(),
            LogicalTime::ZERO,
        );
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        let (global_before, thread_before) = {
            let global_time = state.global_time.lock().unwrap();
            (global_time.as_nanos(), global_time.threads_time(dettid))
        };
        let mut late_time = current_time;
        late_time.add_syscall();

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    late_time,
                    MmId::initial(dettid),
                    GlobalRequest::RequestResources(Resources::new(dettid), detpid),
                ),
            )
            .await;

        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        assert!(!state.sched.lock().unwrap().next_turns.contains_key(&dettid));
        let global_time = state.global_time.lock().unwrap();
        assert_eq!(global_time.as_nanos(), global_before);
        assert_eq!(global_time.threads_time(dettid), thread_before);
    }

    #[tokio::test]
    async fn duplicate_deregistration_is_acknowledged_without_clock_or_scheduler_mutation() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        let mut current_time = DetTime::new(&config);
        current_time.add_syscall();
        state.global_time.lock().unwrap().update_global_time(
            dettid,
            current_time.as_nanos(),
            LogicalTime::ZERO,
        );
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        let (global_before, thread_before) = {
            let global_time = state.global_time.lock().unwrap();
            (global_time.as_nanos(), global_time.threads_time(dettid))
        };
        let mut late_time = current_time;
        late_time.add_syscall();
        let mut final_stats = TimesliceStats::default();
        final_stats.record(7);

        let first_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    late_time.clone(),
                    MmId::initial(dettid),
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        thread_start_entered: true,
                        dettid,
                        detpid,
                        mm: MmId::initial(detpid),
                        timeslice_stats: final_stats,
                        syscall_count: 17,
                        chaos_epochs: Vec::new(),
                    }),
                ),
            )
            .await;
        assert_eq!(first_response, (None, GlobalResponse::DeregisterThread(())));
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .per_thread_timeslice
                .get(&dettid),
            Some(&final_stats)
        );
        assert_eq!(
            state.sched.lock().unwrap().per_thread_syscalls.get(&dettid),
            Some(&17)
        );

        late_time.add_syscall();
        let duplicate_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    late_time,
                    MmId::initial(dettid),
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        thread_start_entered: true,
                        dettid,
                        detpid,
                        mm: MmId::initial(detpid),
                        timeslice_stats: final_stats,
                        syscall_count: 99,
                        chaos_epochs: Vec::new(),
                    }),
                ),
            )
            .await;
        assert_eq!(
            duplicate_response,
            (None, GlobalResponse::DeregisterThread(()))
        );
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .per_thread_timeslice
                .get(&dettid),
            Some(&final_stats)
        );
        assert_eq!(
            state.sched.lock().unwrap().per_thread_syscalls.get(&dettid),
            Some(&17),
            "a duplicate deregistration must not double-count or replace final accounting"
        );
        let summary = state
            .sched
            .lock()
            .unwrap()
            .generate_partial_run_summary(None)
            .unwrap();
        assert_eq!(summary.syscalls, Some(17));
        assert!(!state.sched.lock().unwrap().next_turns.contains_key(&dettid));
        let global_time = state.global_time.lock().unwrap();
        assert_eq!(global_time.as_nanos(), global_before);
        assert_eq!(global_time.threads_time(dettid), thread_before);
    }

    #[tokio::test]
    async fn child_registration_fails_closed_for_a_tombstoned_tid() {
        let (config, state, parent, detpid) = cancellation_test_state();
        install_test_registration(&state, parent, Ivar::new());
        let child = DetTid::from_raw(18);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(parent, child, false);
        install_test_registration(&state, child, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&child, &detpid, MmId::initial(detpid));

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(parent),
                    GlobalRequest::CreateChildThread(
                        child,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;

        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        let scheduler = state.sched.lock().unwrap();
        assert!(scheduler.thread_is_logically_killed(child));
        assert!(!scheduler.next_turns.contains_key(&child));
        assert!(!scheduler.priorities.contains_key(&child));
        assert!(!state.global_time.lock().unwrap().contains_thread(parent));
    }

    #[tokio::test]
    async fn pending_resource_request_woken_by_logical_kill_is_terminal() {
        let (_, state, dettid, detpid) = cancellation_test_state();
        let request_seen = Ivar::new();
        install_test_registration(&state, dettid, request_seen.clone());

        let request = state.recv_request_resources(
            reverie::Tid::from_raw(dettid.as_raw()),
            detpid,
            Resources::new(dettid),
            None,
        );
        let kill_after_request = async {
            while request_seen.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };

        let (response, ()) = tokio::join!(request, kill_after_request);
        assert_eq!(response, (SchedulerRpcResult::ThreadExited, None));
    }

    #[tokio::test]
    async fn trace_replay_yield_propagates_terminal_scheduler_cancellation() {
        let dettid = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        let next_tid = DetTid::from_raw(18);
        let event = SchedEvent {
            dettid,
            op: Op::OtherInstructions,
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: Some(LogicalTime::from_nanos(1)),
        };
        let next_event = SchedEvent {
            dettid: next_tid,
            end_time: Some(LogicalTime::from_nanos(2)),
            ..event.clone()
        };
        let trace_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            trace_file.path(),
            PreemptionRecord::from_sched_events(vec![event.clone(), next_event]).to_string(),
        )
        .unwrap();
        let config = Config {
            sequentialize_threads: true,
            replay_schedule_from: Some(trace_file.path().to_path_buf()),
            ..Config::default()
        }
        .with_backend(|backend| {
            backend.needs_killed_thread_rpc_cancellation = true;
        });
        let state = GlobalState::initialize(&config, false);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, dettid, true);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, next_tid, false);
        let request_seen = Ivar::new();
        install_test_registration(&state, dettid, request_seen.clone());
        install_test_registration(&state, next_tid, Ivar::new());

        let replay = state.recv_trace_schedevent(event, detpid, MmId::initial(detpid), false);
        let kill_after_replay_yield = async {
            while request_seen.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };
        let (response, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(replay, kill_after_replay_yield)
        })
        .await
        .expect("trace replay cancellation did not terminate the pending scheduler RPC");

        assert_eq!(response, SchedulerRpcResult::ThreadExited);
    }

    #[tokio::test]
    async fn pending_start_request_woken_by_logical_kill_is_terminal() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        let request_seen = Ivar::new();
        install_test_registration(&state, dettid, request_seen.clone());
        let request = state.receive_rpc(
            reverie::Tid::from_raw(dettid.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(dettid),
                GlobalRequest::StartNewThread(dettid, detpid, None, None),
            ),
        );
        let kill_after_request = async {
            while request_seen.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };

        let (response, ()) = tokio::join!(request, kill_after_request);
        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        assert!(!state.sched.lock().unwrap().priorities.contains_key(&dettid));
    }

    #[tokio::test]
    async fn required_physical_thread_id_missing_is_terminal() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        }
        .with_backend(|backend| {
            backend.needs_killed_thread_rpc_cancellation = true;
            backend.requires_thread_directed_process_signals = true;
        });
        let state = GlobalState::initialize(&config, false);
        let dettid = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, dettid, true);
        install_test_registration(&state, dettid, Ivar::new());

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(detpid),
                    GlobalRequest::StartNewThread(dettid, detpid, None, None),
                ),
            )
            .await;

        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        let scheduler = state.sched.lock().unwrap();
        assert!(!scheduler.next_turns.contains_key(&dettid));
    }

    #[tokio::test]
    async fn tombstoned_timer_registration_cannot_mutate_scheduler_state() {
        let (_, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        let now = LogicalTime::from_nanos(100);

        let alarm = state
            .recv_register_alarm(
                detpid,
                RpcIncarnation {
                    dettid,
                    mm: MmId::initial(detpid),
                },
                now,
                LogicalTime::from_nanos(10),
                LogicalTime::ZERO,
                SigWrapper::from(Signal::SIGALRM),
            )
            .await;
        assert_eq!(alarm, SchedulerRpcResult::ThreadExited);

        let posix = state
            .recv_register_posix_timer(
                detpid,
                RpcIncarnation {
                    dettid,
                    mm: MmId::initial(detpid),
                },
                1,
                Some(now + LogicalTime::from_nanos(10)),
                LogicalTime::ZERO,
                super::PosixTimerNotification {
                    sig: SigWrapper::from(Signal::SIGALRM),
                    thread: None,
                    value: 1,
                    deleted: false,
                },
            )
            .await;
        assert_eq!(posix, SchedulerRpcResult::ThreadExited);
        assert!(state.sched.lock().unwrap().blocked.timed_waiters.is_empty());
    }

    #[tokio::test]
    async fn pending_futex_request_woken_by_logical_kill_is_terminal() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        let request = state.receive_rpc(
            reverie::Tid::from_raw(dettid.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(dettid),
                GlobalRequest::FutexAction(
                    dettid,
                    FutexAction::WaitRequest(None, None),
                    FutexID::private(MmId::initial(detpid), 0x1000),
                    0,
                    u32::MAX,
                ),
            ),
        );
        let kill_after_wait = async {
            while state.sched.lock().unwrap().blocked.futex_waiters.is_empty() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };

        let (response, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(request, kill_after_wait)
        })
        .await
        .expect("futex teardown did not wake the blocked RPC");
        assert_eq!(response, (None, GlobalResponse::ThreadExited));
    }

    #[tokio::test]
    async fn parent_continue_propagates_terminal_scheduler_cancellation() {
        let (config, state, parent, detpid) = cancellation_test_state();
        let parent_request = Ivar::new();
        install_test_registration(&state, parent, parent_request.clone());
        let child = DetTid::from_raw(18);
        let physical_pid = std::process::id() as i32;
        let physical_tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
        let request = state.receive_rpc(
            reverie::Tid::from_raw(parent.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(parent),
                GlobalRequest::CreateChildThread(
                    child,
                    detpid,
                    0,
                    None,
                    libc::SIGCHLD,
                    Some((physical_pid, physical_tid)),
                    Some(DEFAULT_PRIORITY),
                ),
            ),
        );
        let kill_after_parent_parks = async {
            while parent_request.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            let mut scheduler = state.sched.lock().unwrap();
            let (registered_mm, registered_pid, registered_tid) = scheduler
                .physical_thread_identity(child)
                .expect("parent registration must install the child pidfd before continuing");
            assert_eq!(
                registered_mm,
                MmId::for_clone(MmId::initial(parent), child, false)
            );
            assert_eq!(
                (registered_pid, registered_tid),
                (physical_pid, physical_tid)
            );
            scheduler.logically_kill_thread(&parent, &detpid, MmId::initial(detpid));
        };

        let (response, ()) = tokio::join!(request, kill_after_parent_parks);
        assert_eq!(response, (None, GlobalResponse::ThreadExited));
    }

    #[test]
    fn unsupported_syscall_warning_is_sorted_and_aggregated() {
        let syscalls = BTreeSet::from([
            "vmsplice".to_owned(),
            "getppid".to_owned(),
            "getppid".to_owned(),
        ]);

        assert_eq!(
            format_unsupported_syscall_warning(&syscalls).as_deref(),
            Some("syscalls getppid,vmsplice used but not yet supported")
        );
        assert_eq!(format_unsupported_syscall_warning(&BTreeSet::new()), None);
    }

    #[tokio::test]
    async fn abnormal_cleanup_cancels_an_unstarted_scheduler() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let mut state = GlobalState::initialize(&config, true);
        state.cancel_internal_scheduler().await;
        let summary_path = None;
        let cleanup = state.clean_up(false, &summary_path);

        assert!(
            matches!(
                tokio::time::timeout(Duration::from_millis(100), cleanup).await,
                Ok(Ok(()))
            ),
            "cleanup waited for a scheduler whose guest never registered"
        );
    }

    #[tokio::test]
    async fn abnormal_cleanup_cancels_a_registered_scheduler() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let mut state = GlobalState::initialize(&config, true);
        let dettid = DetTid::from_raw(1);
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.priorities.insert(dettid, DEFAULT_PRIORITY);
            scheduler.next_turns.insert(
                dettid,
                ThreadNextTurn {
                    dettid,
                    child_tid_addr: 0,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                    protocol: Default::default(),
                },
            );
            scheduler.runqueue_push_back(dettid);
            scheduler.started_up.put(());
        }
        tokio::task::yield_now().await;

        state.cancel_internal_scheduler().await;
        let summary_path = None;
        assert!(
            matches!(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    state.clean_up(false, &summary_path),
                )
                .await,
                Ok(Ok(()))
            ),
            "cleanup waited after cancelling a registered scheduler"
        );
    }

    /// A deterministic inode must be minted from the monotonic counter, never
    /// derived from the host inode's bits. This is the behavioural half of the
    /// guarantee whose static half is `DetInode` being a newtype: even for a
    /// large, realistic host inode the det value stays small, so a leaked host
    /// inode is distinguishable from a genuine det one.
    #[test]
    fn det_inodes_are_minted_not_passed_through() {
        use crate::types::DetInode;

        let mut pool = super::InodePool::new();
        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;

        // 221_742_951 is the value observed leaking into FileContents.
        let host_a = crate::types::RawInode::new(33, 221_742_951);
        let host_b = crate::types::RawInode::new(33, 998_877_665);
        let (a, _) = pool.add_inode(host_a, seen, t);
        let (b, _) = pool.add_inode(host_b, seen, t);

        assert_ne!(
            a.as_raw(),
            host_a.ino,
            "det inode must not be the host inode"
        );
        assert_ne!(
            b.as_raw(),
            host_b.ino,
            "det inode must not be the host inode"
        );
        assert_eq!(
            a,
            crate::consts::DET_INODE_OFFSET,
            "minting starts above the fixed stdio inodes"
        );
        assert_eq!(b, DetInode::mint(9001), "minting is monotonic");

        // Re-determinizing the same host inode is stable, not a fresh mint.
        let (a_again, _) = pool.add_inode(host_a, seen, t);
        assert_eq!(a, a_again, "mapping must be stable per host inode");
    }

    /// <https://github.com/rrnewton/hermit/issues/3840>: a filesystem such as
    /// ext4 or XFS gives a freed inode to a new file, and whether it does
    /// differs between runs. The same guest steps (number a file, remove its
    /// last name, create another file, look it up) must number the new file,
    /// and every file after it, the same way whether or not the host reused
    /// the inode, however the new file is first reached.
    #[test]
    fn a_host_reusing_a_freed_inode_does_not_renumber_the_new_file() {
        use super::InodeSighting::Descriptor;
        use super::InodeSighting::Name;
        use crate::types::DetInode;
        use crate::types::RawInode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        const EXT4: u64 = 2049;
        let old = RawInode::new(EXT4, 12);
        let fresh = RawInode::new(EXT4, 13);
        // `opened` says whether the new file is created by an `open`, which
        // discards a retired mapping, before its first numbering request.
        let steps = |new: RawInode, opened: bool, first: super::InodeSighting| {
            let mut pool = super::InodePool::new();
            let old_number = pool.add_sighted_inode(old, seen, t, Name).0;
            pool.retire(old);
            if opened {
                pool.forget_retired(new);
            }
            let new_number = pool.add_sighted_inode(new, seen, t, first).0;
            let listed = pool.add_sighted_inode(new, seen, t, Name).0;
            let later: Vec<DetInode> = [RawInode::new(EXT4, 40), RawInode::new(EXT4, 41)]
                .into_iter()
                .map(|raw| pool.add_sighted_inode(raw, seen, t, Name).0)
                .collect();
            (old_number, new_number, listed, later)
        };
        for (opened, first) in [(true, Descriptor), (true, Name), (false, Name)] {
            let reused = steps(old, opened, first);
            let not_reused = steps(fresh, opened, first);
            assert_eq!(
                reused, not_reused,
                "opened={opened} first={first:?}: reuse changed a number"
            );
            let (old_number, new_number, listed, _) = reused;
            assert_ne!(old_number, new_number, "the new file took the old number");
            assert_eq!(new_number, listed, "the new file's number is stable");
        }
    }

    /// An unlinked file reached through a descriptor (open, unlink, fstat)
    /// keeps its number and its mtime updates, as on Linux, until a name shows
    /// that the host gave its inode to another file. A path that finds no
    /// link (`/proc/self/fd/N`) is a `Descriptor` sighting.
    #[test]
    fn an_unlinked_file_keeps_its_number_through_its_descriptors() {
        use super::InodeSighting::Descriptor;
        use super::InodeSighting::Name;
        use crate::types::RawInode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let raw = RawInode::new(2049, 12);
        let mut pool = super::InodePool::new();
        let (number, _) = pool.add_sighted_inode(raw, seen, t, Name);
        pool.retire(raw);
        assert_eq!(pool.add_sighted_inode(raw, seen, t, Descriptor).0, number);
        let written = LogicalTime::from_nanos(7_000);
        pool.set_mtime(raw, written);
        assert_eq!(
            pool.add_sighted_inode(raw, seen, t, Descriptor),
            (number, written),
            "a write through the descriptor reached the retired file"
        );
        let (reused, _) = pool.add_sighted_inode(raw, seen, t, Name);
        assert_ne!(reused, number, "a name of a retired inode is another file");
        assert_eq!(
            pool.add_sighted_inode(raw, seen, t, Descriptor).0,
            reused,
            "descriptors now reach the new file"
        );
        // Without a retire, a name finds the live mapping, as before.
        let other = RawInode::new(2049, 99);
        let (live, _) = pool.add_sighted_inode(other, seen, t, Descriptor);
        assert_eq!(pool.add_sighted_inode(other, seen, t, Name).0, live);
    }

    /// A write's mtime update can reach a file before anything numbers it
    /// (`echo x > f`, then `rm f`). The pending mtime must stay with that file,
    /// for a descriptor that still reaches it, and must not reach the next
    /// file the host gives the inode (claude review of
    /// <https://github.com/rrnewton/hermit/pull/3849>, P2-2).
    #[test]
    fn a_pending_mtime_stays_with_its_file_after_the_last_name_goes() {
        use super::InodeSighting::Descriptor;
        use super::InodeSighting::Name;
        use crate::types::RawInode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let old = RawInode::new(2049, 12);
        let fresh = RawInode::new(2049, 13);
        let written = LogicalTime::from_nanos(7_000);
        // The next file is created by an open, or first seen in a listing.
        let next_file = |new: RawInode, opened: bool| {
            let mut pool = super::InodePool::new();
            pool.set_mtime(old, written);
            pool.retire(old);
            if opened {
                pool.forget_retired(new);
            }
            pool.add_sighted_inode(new, seen, t, Name)
        };
        for opened in [true, false] {
            assert_eq!(
                next_file(old, opened),
                next_file(fresh, opened),
                "opened={opened}: the reused inode inherited the removed file's mtime"
            );
        }

        // Through a remaining descriptor, the removed file keeps its mtime,
        // and later writes through it reach it.
        let mut pool = super::InodePool::new();
        pool.set_mtime(old, written);
        pool.retire(old);
        let (number, mtime) = pool.add_sighted_inode(old, seen, t, Descriptor);
        assert_eq!(mtime, written);
        let rewritten = LogicalTime::from_nanos(9_000);
        pool.set_mtime(old, rewritten);
        assert_eq!(
            pool.add_sighted_inode(old, seen, t, Descriptor),
            (number, rewritten)
        );
        // It was numbered as retired: a name is another file.
        let (other, _) = pool.add_sighted_inode(old, seen, t, Name);
        assert_ne!(other, number);
    }

    /// A directory stream snapshots the whole directory at its first
    /// `getdents` and can return an entry after the entry's file lost its
    /// last name, while a descriptor still holds that file. Such an entry is
    /// not a new file, so it must not renumber the held one; an entry from a
    /// snapshot taken after the retirement is (codex review of
    /// <https://github.com/rrnewton/hermit/pull/3849>, P1).
    #[test]
    fn a_stale_directory_entry_does_not_renumber_a_held_file() {
        use super::InodeSighting::Descriptor;
        use super::InodeSighting::Listed;
        use super::InodeSighting::Name;
        use crate::types::RawInode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let raw = RawInode::new(2049, 12);
        let mut pool = super::InodePool::new();
        let (number, _) = pool.add_sighted_inode(raw, seen, t, Name);
        let before = pool.retirements;
        pool.retire(raw);
        assert_eq!(
            pool.add_sighted_inode(raw, seen, t, Listed(before)).0,
            number
        );
        assert_eq!(pool.add_sighted_inode(raw, seen, t, Descriptor).0, number);
        let after = pool.retirements;
        let (reused, _) = pool.add_sighted_inode(raw, seen, t, Listed(after));
        assert_ne!(
            reused, number,
            "a snapshot taken after the retirement shows a new file"
        );
    }

    /// A snapshot entry must name the file that held the inode when the
    /// snapshot was taken, even after a later file the host gave the freed
    /// inode discarded that file's retirement. Codex re-review of
    /// https://github.com/rrnewton/hermit/pull/3849, P1 (Claude re-review
    /// P2-A): number F, take a snapshot, unlink F, create and number G, then
    /// read F's cached entry; reuse and a fresh inode must agree on every
    /// number, whether or not F was numbered before the snapshot.
    #[test]
    fn a_stale_directory_entry_keeps_its_file_after_the_inode_is_reused() {
        use super::InodeSighting::Listed;
        use super::InodeSighting::Name;
        use crate::types::DetInode;
        use crate::types::RawInode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let f = RawInode::new(2049, 12);
        let fresh = RawInode::new(2049, 13);
        let history = |g: RawInode, numbered_first: bool| -> Vec<DetInode> {
            let mut pool = super::InodePool::new();
            let mut numbers = Vec::new();
            if numbered_first {
                numbers.push(pool.add_sighted_inode(f, seen, t, Name).0);
            }
            let snapshot = pool.retirements;
            pool.retire(f);
            pool.forget_retired(g); // G's open.
            numbers.push(pool.add_sighted_inode(g, seen, t, Name).0);
            numbers.push(pool.add_sighted_inode(f, seen, t, Listed(snapshot)).0);
            numbers.push(pool.add_sighted_inode(f, seen, t, Listed(snapshot)).0);
            numbers.push(
                pool.add_sighted_inode(RawInode::new(2049, 40), seen, t, Name)
                    .0,
            );
            numbers
        };
        for numbered_first in [true, false] {
            let reused = history(f, numbered_first);
            assert_eq!(
                reused,
                history(fresh, numbered_first),
                "numbered_first={numbered_first}: host reuse changed a number"
            );
            let cached = reused[reused.len() - 3];
            let g = reused[reused.len() - 4];
            assert_ne!(cached, g, "F's cached entry took G's number");
            assert_eq!(
                cached,
                reused[reused.len() - 2],
                "the cached entry is stable"
            );
        }
    }

    /// A write through a descriptor after the last name of a never-numbered
    /// file went away reaches that file's history record too, so a directory
    /// snapshot that names it after the host reused the inode sees the same
    /// mtime as one that names it without reuse (claude re-check of
    /// https://github.com/rrnewton/hermit/pull/3849).
    #[test]
    fn a_write_after_the_last_name_reaches_the_snapshot_record() {
        use super::InodeSighting::Listed;
        use super::InodeSighting::Name;
        use crate::types::RawInode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let f = RawInode::new(2049, 12);
        let fresh = RawInode::new(2049, 13);
        let cached = |g: RawInode| {
            let mut pool = super::InodePool::new();
            pool.set_mtime(f, LogicalTime::from_nanos(1_000));
            let snapshot = pool.retirements;
            pool.retire(f);
            pool.set_mtime(f, LogicalTime::from_nanos(2_000));
            pool.forget_retired(g);
            pool.add_sighted_inode(g, seen, t, Name);
            pool.add_sighted_inode(f, seen, t, Listed(snapshot))
        };
        assert_eq!(cached(f), cached(fresh));
        assert_eq!(cached(f).1, LogicalTime::from_nanos(2_000));
    }

    /// Ordinary churn reuses one host inode many times. A snapshot lookup
    /// must not scan the inode's whole history (codex re-check of
    /// https://github.com/rrnewton/hermit/pull/3849): a snapshot newer than
    /// the last retirement is answered without examining any record, and an
    /// older one examines about log2 of the history's length, while still
    /// naming the file that held the inode when it was taken.
    #[test]
    fn a_snapshot_lookup_does_not_scan_the_inodes_history() {
        use super::InodeSighting::Listed;
        use super::InodeSighting::Name;
        use crate::types::RawInode;

        const GENERATIONS: usize = 4096;
        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let raw = RawInode::new(2049, 12);
        let mut pool = super::InodePool::new();
        // Each holder is numbered, seen in a snapshot, and retired; the next
        // file the host gives the inode is opened by name.
        let mut held = Vec::with_capacity(GENERATIONS);
        for _ in 0..GENERATIONS {
            pool.forget_retired(raw);
            let number = pool.add_sighted_inode(raw, seen, t, Name).0;
            held.push((pool.retirements, number));
            pool.retire(raw);
        }
        // The current holder, numbered after the last retirement.
        pool.forget_retired(raw);
        let current = pool.add_sighted_inode(raw, seen, t, Name).0;

        pool.listed_probes = 0;
        for _ in 0..1000 {
            let snapshot = pool.retirements;
            assert_eq!(
                pool.add_sighted_inode(raw, seen, t, Listed(snapshot)).0,
                current
            );
        }
        assert_eq!(
            pool.listed_probes, 0,
            "a current snapshot examined history records"
        );

        let bound = (GENERATIONS.ilog2() + 2) as u64;
        for step in (0..GENERATIONS).step_by(37) {
            let (snapshot, number) = held[step];
            pool.listed_probes = 0;
            assert_eq!(
                pool.add_sighted_inode(raw, seen, t, Listed(snapshot)).0,
                number,
                "the snapshot taken while holder {step} held the inode"
            );
            assert!(
                pool.listed_probes <= bound,
                "holder {step}: {} probes for {GENERATIONS} generations",
                pool.listed_probes
            );
        }
    }

    /// Retiring and discarding follow host state (a link count, whether the
    /// host reused an inode), so they must not consume counter values: files
    /// numbered after them get the same numbers either way.
    #[test]
    fn retiring_and_forgetting_consume_no_counter_values() {
        use crate::types::RawInode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let raw = RawInode::new(2049, 12);
        let numbers = |retire: bool| {
            let mut pool = super::InodePool::new();
            pool.add_inode(raw, seen, t);
            if retire {
                pool.retire(raw);
                pool.forget_retired(raw);
                pool.forget_retired(RawInode::new(2049, 77));
            }
            [RawInode::new(2049, 20), RawInode::new(2049, 21)]
                .map(|raw| pool.add_inode(raw, seen, t).0)
        };
        assert_eq!(numbers(true), numbers(false));
    }

    /// Whether a host inode is new to the pool depends on host state the guest
    /// does not control. In https://github.com/rrnewton/hermit/issues/2897,
    /// `sar` and its child `sadc` each opened one file; in one run another
    /// process replaced it between the two opens, so the second open saw a new
    /// host inode. Every later file, on every device, came out one number
    /// higher, and every `getdents64` chunk of a `/sys` listing differed. Only
    /// the file whose host identity differed may change number.
    #[test]
    fn a_host_dependent_new_inode_does_not_renumber_later_files() {
        use crate::types::DetInode;
        use crate::types::RawInode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        const ROOTFS: u64 = 2049;
        const SYSFS: u64 = 22;
        let later_files = |pool: &mut super::InodePool| -> Vec<DetInode> {
            [
                RawInode::new(SYSFS, 76_927),
                RawInode::new(SYSFS, 76_925),
                RawInode::new(SYSFS, 243_324),
                RawInode::new(ROOTFS, 1_311),
                RawInode::new(ROOTFS, 1_312),
            ]
            .into_iter()
            .map(|raw| pool.add_inode(raw, seen, t).0)
            .collect()
        };

        // Both opens find the same host file.
        let mut unchanged = super::InodePool::new();
        unchanged.add_inode(RawInode::new(ROOTFS, 94_136_091), seen, t);
        unchanged.add_inode(RawInode::new(ROOTFS, 94_136_091), seen, t);
        // The file is replaced between the two opens.
        let mut replaced = super::InodePool::new();
        replaced.add_inode(RawInode::new(ROOTFS, 94_136_091), seen, t);
        replaced.add_inode(RawInode::new(ROOTFS, 94_154_661), seen, t);
        assert_eq!(
            later_files(&mut unchanged),
            later_files(&mut replaced),
            "a host-dependent new inode renumbered files minted after it"
        );

        // The converse: two paths that are one host file (a hard link, or an
        // inode the filesystem reused) versus two separate host files.
        let mut linked = super::InodePool::new();
        linked.add_inode(RawInode::new(ROOTFS, 500), seen, t);
        linked.add_inode(RawInode::new(ROOTFS, 500), seen, t);
        let mut separate = super::InodePool::new();
        separate.add_inode(RawInode::new(ROOTFS, 500), seen, t);
        separate.add_inode(RawInode::new(ROOTFS, 501), seen, t);
        assert_eq!(
            later_files(&mut linked),
            later_files(&mut separate),
            "a host-dependent reused inode renumbered files minted after it"
        );
    }

    /// Whether an mtime update reaches the pool depends on host state:
    /// `utimensat` skips it when the target's host identity changed during the
    /// call or the kernel stored an unexpected time. Setting an mtime must not
    /// consume a number, so a skipped update cannot renumber later files; and
    /// a file that gets its mtime before its number keeps that mtime.
    #[test]
    fn an_mtime_update_does_not_number_files() {
        use crate::types::DetInode;
        use crate::types::RawInode;

        let epoch = LogicalTime::from_nanos(0);
        let set = LogicalTime::from_nanos(5_000_000_000);
        let seen = super::ObservedMtime::Unobserved;
        let touched = RawInode::new(2049, 77);
        let later_files = |pool: &mut super::InodePool| -> Vec<DetInode> {
            (1_000..1_003)
                .map(|ino| pool.add_inode(RawInode::new(2049, ino), seen, epoch).0)
                .collect()
        };

        // The update reached the pool, before and after the file was numbered.
        let mut applied = super::InodePool::new();
        applied.set_mtime(touched, set);
        let (_, mtime) = applied.add_inode(touched, seen, epoch);
        assert_eq!(
            mtime, set,
            "a pending mtime is lost when the file is numbered"
        );
        applied.set_mtime(touched, set);
        // The same calls with the updates skipped.
        let mut skipped = super::InodePool::new();
        skipped.add_inode(touched, seen, epoch);
        assert_eq!(
            later_files(&mut applied),
            later_files(&mut skipped),
            "a host-dependent mtime update renumbered later files"
        );
    }

    /// The serial grant follows the scheduler's real transitions: a granted
    /// thread holds it; filing its next request surrenders it; and a grant
    /// that releases the thread into a background call does not confer it,
    /// although that grant is the last one (`parked.running`).
    #[tokio::test]
    async fn the_serial_grant_ends_at_the_next_request_and_is_not_a_background_grant() {
        let (_config, state, tid, _) = cancellation_test_state();
        let first = Ivar::new();
        first.put(Ok(Resources::new(tid)));
        install_test_registration(&state, tid, first);
        assert!(!state.sched.lock().unwrap().holds_serial_grant(tid));

        let grant = |state: &GlobalState| {
            let (chosen, request, response) =
                state.sched.lock().unwrap().select_test_turn().unwrap();
            assert_eq!(chosen, tid);
            crate::scheduler::finish_selected_turn(
                state.sched.clone(),
                state.global_time.clone(),
                chosen,
                request,
                response,
            )
        };
        grant(&state).await.unwrap();
        assert!(state.sched.lock().unwrap().holds_serial_grant(tid));

        // The thread files its next request: a blocking external call.
        let next = state.sched.lock().unwrap().next_turns[&tid].req.clone();
        let mut background = Resources::new(tid);
        background.insert(
            ResourceID::BlockingExternalIO(ExternalOpId::new(tid, 1)),
            Permission::RW,
        );
        next.put(Ok(background));
        assert!(!state.sched.lock().unwrap().holds_serial_grant(tid));

        // The scheduler releases it into the background. A background grant
        // unblocks the thread in step 4 and then skips the rest of the turn.
        assert!(grant(&state).await.is_err());
        let sched = state.sched.lock().unwrap();
        assert!(sched.blocked.external_io_blockers.contains_key(&tid));
        assert!(!sched.holds_serial_grant(tid));
    }

    /// Controller mapping capture authenticates its task and procfs view,
    /// retains the existing serial grant, and returns no logical time.
    #[tokio::test]
    async fn controller_brk_capture_authenticates_the_view_without_spending_the_grant() {
        use std::os::unix::fs::MetadataExt;

        use crate::syscalls::memory::BrkMappingCheck;

        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let tid = DetTid::from_raw(37);
        let mm = MmId::initial(tid);
        let pid = std::process::id() as i32;
        let physical_tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
        let procfs = std::fs::metadata("/proc").unwrap();
        let fact = reverie::ControllerCaptureFact {
            physical_pid: reverie::Pid::from_raw(pid),
            physical_tid: reverie::Pid::from_raw(physical_tid),
            procfs_device: procfs.dev(),
            procfs_inode: procfs.ino(),
        };
        {
            let mut sched = state.sched.lock().unwrap();
            sched.thread_tree.add_child(tid, tid, true);
            sched.record_thread_mm(tid, mm);
            sched.next_turns.insert(
                tid,
                ThreadNextTurn {
                    dettid: tid,
                    child_tid_addr: 0,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                    protocol: Default::default(),
                },
            );
            sched
                .register_physical_thread(tid, mm, pid, physical_tid)
                .unwrap();
            sched.set_running_for_test(tid);
        }
        let before = state.global_time.lock().unwrap().as_nanos();
        // This real smaps capture must deny an overflowing growth, without
        // allocating a guest-visible snapshot or advancing the logical clock.
        let check = BrkMappingCheck::Grow {
            old_end: 0,
            new_end: u64::MAX,
        };
        let answer = state
            .receive_rpc(
                Tid::from_raw(tid.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::CheckBrkMappings(check, fact),
                ),
            )
            .await;
        assert_eq!(
            answer,
            (None, GlobalResponse::CheckBrkMappings(Some(false)))
        );
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before);
        assert!(state.sched.lock().unwrap().holds_serial_grant(tid));
        assert_eq!(
            state.capture_brk_mappings(
                Tid::from_raw(tid.as_raw()),
                mm,
                check,
                reverie::ControllerCaptureFact {
                    procfs_inode: fact.procfs_inode + 1,
                    ..fact
                },
            ),
            None,
        );
        let address = &before as *const _ as u64;
        assert_eq!(
            state.capture_brk_mappings(
                Tid::from_raw(tid.as_raw()),
                mm,
                BrkMappingCheck::Shrink {
                    new_end: address,
                    old_end: address + 1
                },
                fact,
            ),
            Some(true),
        );
        let unsequentialized = GlobalState::initialize(
            &Config {
                sequentialize_threads: false,
                ..config
            },
            false,
        );
        assert_eq!(
            unsequentialized.capture_brk_mappings(Tid::from_raw(tid.as_raw()), mm, check, fact,),
            None
        );
    }

    /// Only the thread holding the serial grant reaches a shared open file
    /// description. Its control messages move no clock, and the death of the
    /// lease holder's process ends the lease. A refused message fails the run.
    #[tokio::test]
    async fn shared_open_file_control_needs_the_serial_grant() {
        use nix::fcntl::OFlag;

        use crate::fd::DetFd;
        use crate::fd::FdType;
        use crate::shared_open_files::OpenFileControl;
        use crate::shared_open_files::OpenFileControlReply;
        use crate::types::OpenFileId;

        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let granted = DetTid::from_raw(21);
        let other = DetTid::from_raw(22);
        let id = OpenFileId::new(granted, 0);
        {
            let mut sched = state.sched.lock().unwrap();
            for tid in [granted, other] {
                sched.thread_tree.add_child(tid, tid, true);
                sched.next_turns.insert(
                    tid,
                    ThreadNextTurn {
                        dettid: tid,
                        child_tid_addr: 0,
                        req: Ivar::new(),
                        resp: Ivar::new(),
                        protocol: Default::default(),
                    },
                );
            }
            sched.set_running_for_test(granted);
        }
        state
            .shared_open_files
            .lock()
            .unwrap()
            .insert(DetFd::new(3, OFlag::O_RDWR, FdType::Regular, id).model_for_test())
            .unwrap();
        let global_before = state.global_time.lock().unwrap().as_nanos();
        let take = |tid: DetTid| {
            state.receive_rpc(
                reverie::Tid::from_raw(tid.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(tid),
                    GlobalRequest::SharedOpenFile(OpenFileControl::Take { id }),
                ),
            )
        };

        assert!(matches!(
            take(granted).await,
            (
                None,
                GlobalResponse::SharedOpenFile(Ok(OpenFileControlReply::Taken { .. }))
            )
        ));
        assert!(state.shared_open_files.lock().unwrap().is_leased(id));
        {
            let global_time = state.global_time.lock().unwrap();
            assert_eq!(global_time.as_nanos(), global_before, "no clock accounting");
            assert!(!global_time.contains_thread(granted));
        }

        state.complete_physical_process_exit(granted.as_raw());
        assert!(!state.shared_open_files.lock().unwrap().is_leased(id));
        assert!(!state.sched.lock().unwrap().backend_failed());

        // A thread that does not hold the grant is refused, takes nothing,
        // and the refusal is a backend failure.
        assert!(matches!(
            take(other).await,
            (None, GlobalResponse::SharedOpenFile(Err(_)))
        ));
        assert!(!state.shared_open_files.lock().unwrap().is_leased(id));
        assert!(state.sched.lock().unwrap().backend_failed());
    }

    /// A SIGALRM ledger control message is answered with no logical time and
    /// moves no clock, and its answer is the ledger's: a send to a process that
    /// handles SIGALRM is refused, one to another process is not.
    #[tokio::test]
    async fn sigalrm_control_is_answered_before_clock_accounting() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let (handler, sender) = (DetTid::from_raw(21), DetTid::from_raw(22));
        {
            let mut sched = state.sched.lock().unwrap();
            for tid in [handler, sender] {
                sched.thread_tree.add_child(tid, tid, true);
                sched.next_turns.insert(
                    tid,
                    ThreadNextTurn {
                        dettid: tid,
                        child_tid_addr: 0,
                        req: Ivar::new(),
                        resp: Ivar::new(),
                        protocol: Default::default(),
                    },
                );
            }
            sched.set_sigalrm_handled(DetPid::from_raw(21), true);
            sched.set_running_for_test(sender);
        }
        let global_before = state.global_time.lock().unwrap().as_nanos();
        let send_to = |target: DetTid| {
            state.receive_rpc(
                reverie::Tid::from_raw(sender.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(sender),
                    GlobalRequest::Sigalrm(super::SigalrmControl::SendTo(target)),
                ),
            )
        };

        assert_eq!(
            send_to(handler).await,
            (None, GlobalResponse::Sigalrm(true))
        );
        assert_eq!(
            send_to(sender).await,
            (None, GlobalResponse::Sigalrm(false))
        );
        let global_time = state.global_time.lock().unwrap();
        assert_eq!(global_time.as_nanos(), global_before, "no clock accounting");
        assert!(!global_time.contains_thread(sender));
    }

    /// A host-input observation, and a report of a path the guest rebound,
    /// belong to the run whose global state receives them. Each is answered
    /// with no logical time, before any clock accounting,
    /// so it cannot move the global clock or the sender's, and the run's
    /// records are written with their end line at normal clean-up. The same
    /// request reaches this state from every backend: the openat handler sends
    /// it over the backend's RPC whether Detcore runs in the tracer or inside
    /// the guest.
    #[tokio::test]
    async fn host_input_observations_are_kept_by_the_run_and_written_at_its_end() {
        use detcore_model::host_input::HostFileIdentity;
        use detcore_model::host_input::HostInputLogEnd;
        use detcore_model::host_input::HostInputRecord;

        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("host-inputs");
        let config = Config {
            sequentialize_threads: true,
            record_host_inputs: true,
            host_input_log: Some(log.clone()),
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let dettid = DetTid::from_raw(17);
        let identity = HostFileIdentity {
            dev: 33,
            ino: 7,
            size: 100,
            mtime_sec: 1_700_000_000,
            mtime_nsec: 5,
        };
        let mut sender_time = DetTime::new(&config);
        sender_time.add_syscall();
        sender_time.add_syscall();
        let global_before = state.global_time.lock().unwrap().as_nanos();
        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    sender_time.clone(),
                    MmId::initial(dettid),
                    GlobalRequest::RecordHostInput {
                        path: "/etc/ld.so.cache".into(),
                        syscall: 12,
                        identity,
                    },
                ),
            )
            .await;
        assert_eq!(response, (None, GlobalResponse::RecordHostInput(())));
        for _ in 0..2 {
            let response = state
                .receive_rpc(
                    reverie::Tid::from_raw(dettid.as_raw()),
                    (
                        sender_time.clone(),
                        MmId::initial(dettid),
                        GlobalRequest::RecordHostMutation {
                            path: "/data/F".into(),
                        },
                    ),
                )
                .await;
            assert_eq!(response, (None, GlobalResponse::RecordHostMutation(())));
        }
        {
            let global_time = state.global_time.lock().unwrap();
            assert_eq!(global_time.as_nanos(), global_before, "no clock accounting");
            assert!(
                !global_time.contains_thread(dettid),
                "the sender's clock was not pushed"
            );
        }
        assert!(!log.exists(), "nothing is written while the run goes on");

        state.clean_up(false, &None).await.unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(
            serde_json::from_str::<HostInputRecord>(lines[0]).unwrap(),
            HostInputRecord {
                path: "/etc/ld.so.cache".into(),
                dtid: 17,
                syscall: 12,
                identity,
            }
        );
        // A path rebound twice is one line.
        assert_eq!(
            serde_json::from_str::<detcore_model::host_input::HostMutationRecord>(lines[1])
                .unwrap()
                .mutated,
            "/data/F"
        );
        assert_eq!(
            serde_json::from_str::<HostInputLogEnd>(lines[2]).unwrap(),
            HostInputLogEnd { records: 2 }
        );
    }

    /// Two files on different filesystems can share an inode number; they
    /// are different files and must get different deterministic inodes, with
    /// first-seen mtimes of their own. Keyed by the number alone they shared
    /// one, so a guest comparing inode numbers took them for the same file,
    /// and the second file reported the first one's mtime.
    #[test]
    fn files_on_different_devices_with_one_inode_number_stay_distinct() {
        use super::ObservedMtime;

        let epoch = LogicalTime::from_secs(1_798_761_600);
        let mut pool = super::InodePool::new();
        // 76927 was /sys/bus/i2c/devices on the host that motivated this.
        let sysfs = crate::types::RawInode::new(23, 76_927);
        let rootfs = crate::types::RawInode::new(33, 76_927);

        let (on_sysfs, sysfs_mtime) = pool.add_inode(sysfs, ObservedMtime::HostSpecific, epoch);
        let (on_rootfs, rootfs_mtime) = pool.add_inode(
            rootfs,
            ObservedMtime::Canonical(LogicalTime::from_secs(1)),
            epoch,
        );
        assert_ne!(on_sysfs, on_rootfs, "different files, different det inodes");
        assert_eq!(sysfs_mtime, epoch);
        assert_eq!(rootfs_mtime, LogicalTime::from_secs(1), "its own mtime");
        assert_eq!(
            pool.add_inode(sysfs, ObservedMtime::Unobserved, epoch).0,
            on_sysfs
        );
        assert_eq!(
            pool.add_inode(rootfs, ObservedMtime::Unobserved, epoch).0,
            on_rootfs
        );
    }

    /// Linux numbers a per-process procfs entry when it builds the entry's
    /// dentry, and the host can evict the dentry and build it again under a
    /// new number. Two `lsof` listings of `/proc/3/fd` found the same numbers
    /// in one `--verify` run and new ones in the other, where the host had
    /// rebuilt the entries between the listings. A rebuilt entry reached by
    /// its path keeps its number, so the guest sees the same numbers either
    /// way.
    #[test]
    fn a_rebuilt_procfs_entry_keeps_its_number() {
        use std::path::PathBuf;

        use crate::procfs_inode::DirectoryInodes;
        use crate::procfs_inode::ProcfsDirectory;
        use crate::types::DetInode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let proc_device = libc::makedev(0, 23);
        let names: [&[u8]; 4] = [b".", b"..", b"0", b"1"];
        // The guest numbers of two listings of `/proc/3/fd`, in which the
        // host reports the inode numbers `first` and then `second`, as
        // `recv_determinize_inode` numbers them.
        let listings = |with_paths: bool, first: [u64; 4], second: [u64; 4]| -> Vec<DetInode> {
            let mut pool = super::InodePool::new();
            let directory = with_paths.then(|| ProcfsDirectory::new(PathBuf::from("/3/fd")));
            let inodes = DirectoryInodes::new(proc_device, directory);
            let mut numbers = Vec::new();
            for host in [first, second] {
                for (name, ino) in names.iter().zip(host) {
                    let (raw, procfs) = inodes.entry_inode(name, ino);
                    if let Some(procfs) = procfs {
                        let path = pool.procfs_path_key(&procfs);
                        pool.procfs_key(raw, path);
                    }
                    let sighting = super::InodeSighting::Listed(0);
                    numbers.push(pool.add_sighted_inode(raw, seen, t, sighting).0);
                }
            }
            numbers
        };
        let cached = [4_100, 4_000, 4_101, 4_102];
        // The open directory pins `.` and `..`; the fd entries were rebuilt.
        let rebuilt = [4_100, 4_000, 4_901, 4_902];
        assert_eq!(
            listings(true, cached, cached),
            listings(true, cached, rebuilt),
            "a host rebuilding procfs dentries changed guest inode numbers"
        );
        // The control: by host inode alone, the rebuilt entries were renumbered.
        assert_ne!(
            listings(false, cached, cached),
            listings(false, cached, rebuilt)
        );
    }

    /// One procfs entry reached by two paths, as `/proc/<pid>/net/tcp` and
    /// `/proc/<pid>/task/<pid>/net/tcp` are, is one host inode, so it keeps
    /// one number however it is reached, and after the host rebuilds it.
    #[test]
    fn procfs_aliases_share_one_number() {
        use std::path::Path;

        use crate::procfs_inode::path_inode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let device = libc::makedev(0, 23);
        let host = |ino| crate::types::RawInode::new(device, ino);
        let net = path_inode(device, Path::new("/3/net/tcp"));
        let task = path_inode(device, Path::new("/3/task/3/net/tcp"));
        let mut pool = super::InodePool::new();
        let mut number = |raw, path| {
            pool.procfs_key(raw, path);
            pool.add_inode(raw, seen, t).0
        };
        let listed = number(host(4_100), net);
        assert_eq!(number(host(4_100), task), listed, "an alias");
        assert_eq!(number(host(4_900), task), listed, "rebuilt, by the alias");
        assert_eq!(number(host(4_901), net), listed, "rebuilt again");
        let other = number(host(4_200), path_inode(device, Path::new("/3/net/udp")));
        assert_ne!(other, listed);
    }

    /// A rebuilt procfs entry first seen without its path is new to the pool,
    /// as it was before paths were named; its path then leads to its new
    /// number, which it keeps across the next rebuild.
    #[test]
    fn a_numbered_host_inode_keeps_its_number_when_its_path_is_named() {
        use std::path::Path;

        use crate::procfs_inode::path_inode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let device = libc::makedev(0, 23);
        let host = |ino| crate::types::RawInode::new(device, ino);
        let path = path_inode(device, Path::new("/3/fd/0"));
        let mut pool = super::InodePool::new();
        pool.procfs_key(host(4_100), path);
        let first = pool.add_inode(host(4_100), seen, t).0;
        let without_path = pool.add_inode(host(4_900), seen, t).0;
        assert_ne!(without_path, first);
        pool.procfs_key(host(4_900), path);
        assert_eq!(pool.add_inode(host(4_900), seen, t).0, without_path);
        pool.procfs_key(host(4_901), path);
        assert_eq!(pool.add_inode(host(4_901), seen, t).0, without_path);
        assert_eq!(pool.add_inode(host(4_100), seen, t).0, first);
    }

    /// A write to a rebuilt procfs entry, which names the host inode only,
    /// sets the mtime of the number the entry kept, whether the descriptor's
    /// open named the path before the write (`procfs_key` then `set_mtime`) or
    /// after it (a pending mtime).
    #[test]
    fn a_write_to_a_rebuilt_procfs_entry_sets_the_mtime_of_its_number() {
        use std::path::Path;

        use crate::procfs_inode::path_inode;

        let t = LogicalTime::from_nanos(0);
        let written = LogicalTime::from_secs(7);
        let seen = super::ObservedMtime::Unobserved;
        let device = libc::makedev(0, 23);
        let host = |ino| crate::types::RawInode::new(device, ino);
        let path = path_inode(device, Path::new("/3/comm"));
        for open_first in [true, false] {
            let mut pool = super::InodePool::new();
            pool.procfs_key(host(4_100), path);
            let (number, mtime) = pool.add_inode(host(4_100), seen, t);
            assert_ne!(mtime, written);
            if open_first {
                pool.procfs_key(host(4_900), path);
                pool.set_mtime(host(4_900), written);
            } else {
                pool.set_mtime(host(4_900), written);
                pool.procfs_key(host(4_900), path);
            }
            assert_eq!(
                pool.add_inode(host(4_100), seen, t),
                (number, written),
                "open_first={open_first}"
            );
            assert_eq!(pool.add_inode(host(4_900), seen, t), (number, written));
        }
    }

    /// A process ID the kernel gives out again names a new process, whose
    /// procfs entries are new files: Linux drops the old process's entries
    /// when it exits. The new process's `/proc/<pid>` takes a number of its
    /// own and keeps it across rebuilds, while a descriptor still open on
    /// the old process's entry keeps the old number.
    #[test]
    fn a_reused_task_id_names_new_procfs_entries() {
        use std::path::Path;

        use crate::procfs_inode::ProcfsName;
        use crate::types::RawInode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let device = libc::makedev(0, 23);
        let host = |ino| RawInode::new(device, ino);
        let process = ProcfsName::new(device, Path::new("/7"));
        let number = |pool: &mut super::InodePool, raw: RawInode| {
            let path = pool.procfs_path_key(&process);
            pool.procfs_key(raw, path);
            pool.add_inode(raw, seen, t).0
        };
        let mut pool = super::InodePool::new();
        pool.count_incarnation(7);
        let first = number(&mut pool, host(4_100));
        assert_eq!(number(&mut pool, host(4_900)), first, "rebuilt");
        pool.count_incarnation(7);
        let second = number(&mut pool, host(5_000));
        assert_ne!(second, first, "a new process took the old one's number");
        assert_eq!(pool.add_inode(host(4_900), seen, t).0, first, "held");
        assert_eq!(number(&mut pool, host(5_001)), second, "rebuilt");
        // A thread's directory also names the thread's incarnation.
        let thread = ProcfsName::new(device, Path::new("/7/task/9/stat"));
        assert_eq!(thread.tasks, [7, 9]);
        let before = pool.procfs_path_key(&thread);
        pool.count_incarnation(9);
        assert_ne!(pool.procfs_path_key(&thread), before);
    }

    /// A pid Detcore gives out again is a new task: each registration of a
    /// task ID counts one more incarnation (`count_incarnation`), which the
    /// `ProcfsTask` answer reports for the process and the thread, and a file
    /// or directory bound to the earlier incarnation no longer names the
    /// task holding the numbers (`ProcfsTaskAnswer::still_names`).
    #[test]
    fn a_reused_pid_is_a_new_incarnation_for_procfs_files() {
        use crate::procfs::TaskIncarnation;
        use crate::types::DetTid;
        let mut pool = super::InodePool::new();
        assert_eq!(pool.incarnation(7), 0, "an ID no task has held");
        pool.count_incarnation(7);
        pool.count_incarnation(9);
        assert_eq!((pool.incarnation(7), pool.incarnation(9)), (1, 1));
        let (seven, nine) = (DetTid::from_raw(7), DetTid::from_raw(9));
        let first = pool.procfs_task_answer(seven, Some(nine), (true, false));
        assert_eq!(
            first.incarnation(),
            TaskIncarnation {
                process: 1,
                thread: Some(1),
            }
        );
        let bound = first.incarnation();
        assert!(first.still_names(bound));
        // The process exits and is reaped; a new process takes pid 7.
        pool.count_incarnation(7);
        let reused = pool.procfs_task_answer(seven, Some(nine), (true, false));
        assert_eq!(reused.process_incarnation, 2);
        assert_eq!(reused.thread_incarnation, Some(1));
        assert!(!reused.still_names(bound), "the file of the old pid 7");
        assert!(reused.still_names(reused.incarnation()));
        // A thread ID given out again is a new thread of the same process.
        pool.count_incarnation(9);
        let thread_reused = pool.procfs_task_answer(seven, Some(nine), (true, false));
        assert!(!thread_reused.still_names(reused.incarnation()));
        // A task that is gone, or a zombie that was reaped, is no task,
        // whatever its incarnation.
        let gone = pool.procfs_task_answer(seven, None, (false, false));
        assert!(!gone.still_names(gone.incarnation()));
        // The process's own file names no thread.
        assert_eq!(gone.thread_incarnation, None);
    }

    /// Reading a rendered file's state inserts nothing; a write keeps one
    /// entry for that inode only.
    #[test]
    fn a_read_of_rendered_file_state_keeps_nothing() {
        use crate::procfs::listing::RenderedFileChange;
        let mut pool = super::InodePool::new();
        let read = pool.rendered_file_state(5, RenderedFileChange::Read);
        assert_eq!(read, Default::default());
        assert!(pool.rendered_file_states.is_empty());
        let written = pool.rendered_file_state(5, RenderedFileChange::Mtime(3, 4));
        assert_eq!(written.mtime, Some((3, 4)));
        let hinted = pool.rendered_file_state(5, RenderedFileChange::WriteHint(2));
        assert_eq!((hinted.mtime, hinted.write_hint), (Some((3, 4)), 2));
        assert_eq!(
            pool.rendered_file_state(6, RenderedFileChange::Read),
            Default::default()
        );
        assert_eq!(pool.rendered_file_states.len(), 1);
        assert_eq!(
            pool.rendered_file_state(5, RenderedFileChange::Read),
            hinted
        );
    }

    /// A directory snapshot returns its entries after it was taken, and a
    /// task it lists can exit, and a new thread take its ID, in between. The
    /// entry still names the task the snapshot listed: an entry of the old
    /// task rebuilt since keeps the old task's number, rather than adopting
    /// the new task's.
    #[test]
    fn a_snapshot_entry_names_the_task_that_held_its_id() {
        use std::path::Path;

        use crate::procfs_inode::ProcfsName;
        use crate::types::RawInode;

        let t = LogicalTime::from_nanos(0);
        let seen = super::ObservedMtime::Unobserved;
        let device = libc::makedev(0, 23);
        let host = |ino| RawInode::new(device, ino);
        let number = |pool: &mut super::InodePool, raw: RawInode, name: &ProcfsName| {
            let path = pool.procfs_path_key(name);
            pool.procfs_key(raw, path);
            pool.add_inode(raw, seen, t).0
        };
        let process = ProcfsName::new(device, Path::new("/7"));
        let mut pool = super::InodePool::new();
        pool.count_incarnation(7);
        let first = number(&mut pool, host(4_100), &process);
        let listed = process.clone().in_snapshot(pool.threads_counted);
        pool.count_incarnation(7);
        let second = number(&mut pool, host(5_000), &process);
        assert_ne!(second, first, "a new process took the old one's number");
        assert_eq!(
            number(&mut pool, host(4_900), &listed),
            first,
            "the listed process's entry, rebuilt"
        );
        // An ID no thread held when the snapshot was taken names no task.
        let unborn = ProcfsName::new(device, Path::new("/8"));
        let listed = unborn.clone().in_snapshot(pool.threads_counted);
        let unsalted = pool.procfs_path_key(&listed);
        assert_eq!(unsalted, pool.procfs_path_key(&unborn));
        pool.count_incarnation(8);
        assert_eq!(pool.procfs_path_key(&listed), unsalted);
        assert_ne!(pool.procfs_path_key(&unborn), unsalted);
    }

    /// Only an exact whole-second 0 or 1 host mtime is canonical
    /// (https://github.com/rrnewton/hermit/issues/3639).
    #[test]
    fn only_exact_canonical_host_mtimes_are_kept() {
        use super::ObservedMtime;

        assert_eq!(
            ObservedMtime::from_host_mtime(1, 0),
            ObservedMtime::Canonical(LogicalTime::from_secs(1))
        );
        assert_eq!(
            ObservedMtime::from_host_mtime(0, 0),
            ObservedMtime::Canonical(LogicalTime::from_secs(0))
        );
        for (secs, nanos) in [(1, 5), (0, 1), (2, 0), (-1, 0), (1_600_000_000, 0)] {
            assert_eq!(
                ObservedMtime::from_host_mtime(secs, nanos),
                ObservedMtime::HostSpecific,
                "{secs}.{nanos:09} is a real timestamp, not a canonical one"
            );
        }
    }

    /// The first-seen policy is applied at the first stat, whichever syscall
    /// minted the inode, and is not re-applied once resolved.
    #[test]
    fn first_seen_mtime_is_resolved_by_the_first_stat() {
        use super::ObservedMtime;

        let epoch = LogicalTime::from_secs(1_798_761_600);
        let canonical = ObservedMtime::Canonical(LogicalTime::from_secs(1));
        let mut pool = super::InodePool::new();

        // An ordinary file reports the epoch.
        let (_, mtime) = pool.add_inode(
            crate::types::RawInode::new(1, 10),
            ObservedMtime::HostSpecific,
            epoch,
        );
        assert_eq!(mtime, epoch);
        // A canonical file keeps its mtime.
        let (_, mtime) = pool.add_inode(crate::types::RawInode::new(1, 11), canonical, epoch);
        assert_eq!(mtime, LogicalTime::from_secs(1));

        // A read or getdents mints the inode first; the later stat decides.
        let (_, mtime) = pool.add_inode(
            crate::types::RawInode::new(1, 12),
            ObservedMtime::Unobserved,
            epoch,
        );
        assert_eq!(mtime, epoch, "an unresolved mtime reads as the epoch");
        let (_, mtime) = pool.add_inode(crate::types::RawInode::new(1, 12), canonical, epoch);
        assert_eq!(mtime, LogicalTime::from_secs(1));

        // Once resolved, a later observation does not change it.
        let (_, mtime) = pool.add_inode(crate::types::RawInode::new(1, 10), canonical, epoch);
        assert_eq!(mtime, epoch);
        let (_, mtime) = pool.add_inode(
            crate::types::RawInode::new(1, 11),
            ObservedMtime::HostSpecific,
            epoch,
        );
        assert_eq!(mtime, LogicalTime::from_secs(1));
    }
}

#[cfg(test)]
mod robust_exit_clock_tests {
    use std::sync::Mutex;
    use std::task::Poll;

    use nix::sys::signal::Signal;
    use reverie::ExitStatus;
    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Tid;
    use reverie::Tool;

    use super::GlobalRequest;
    use super::GlobalResponse;
    use super::GlobalState;
    use crate::Detcore;
    use crate::ThreadState;
    use crate::config::Config;
    use crate::ivar::Ivar;
    use crate::resources::Resources;
    use crate::scheduler::DEFAULT_PRIORITY;
    use crate::scheduler::SkipTurn;
    use crate::scheduler::ThreadNextTurn;
    use crate::tool_local::RobustListExit;
    use crate::tool_local::RobustListWake;
    use crate::types::*;

    #[derive(Debug, PartialEq)]
    struct WakeObservation {
        wakes: Vec<(DetTid, FutexID)>,
        counts: Vec<u64>,
        clocks: serde_json::Value,
        turn: u64,
    }

    #[derive(Debug)]
    struct RpcObservation {
        sender: DetTid,
        kind: &'static str,
        accepted: bool,
        clocks: serde_json::Value,
        queued: Vec<DetTid>,
        waiters: usize,
        turn: u64,
    }

    // This is only a sender-bound transport, as in Reverie's WrappedFrom.
    // Replies, clock accounting, wakes and deregistration come from GlobalState.
    struct ExitRpc<'a> {
        state: &'a GlobalState,
        sender: DetTid,
        observations: &'a Mutex<Vec<WakeObservation>>,
        rpc_observations: &'a Mutex<Vec<RpcObservation>>,
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for ExitRpc<'_> {
        async fn send_rpc(
            &self,
            request: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            let kind = match &request.2 {
                GlobalRequest::RobustListWakes(wakes) if wakes.is_empty() => "empty-wake",
                GlobalRequest::RobustListWakes(_) => "wake",
                GlobalRequest::DeregisterThread(_) => "deregister",
                _ => panic!("unexpected exit RPC: {:?}", request.2),
            };
            let wakes = match &request.2 {
                GlobalRequest::RobustListWakes(wakes) if !wakes.is_empty() => Some(wakes.clone()),
                _ => None,
            };
            let response = self
                .state
                .receive_rpc(Tid::from_raw(self.sender.as_raw()), request)
                .await;
            let clocks = serde_json::to_value(&*self.state.global_time.lock().unwrap()).unwrap();
            {
                let sched = self.state.sched.lock().unwrap();
                self.rpc_observations.lock().unwrap().push(RpcObservation {
                    sender: self.sender,
                    kind,
                    accepted: !matches!(response.1, GlobalResponse::ThreadExited),
                    clocks: clocks.clone(),
                    queued: sched.run_queue.tids().copied().collect(),
                    waiters: sched
                        .blocked
                        .futex_waiters
                        .values()
                        .map(Vec::len)
                        .sum::<usize>(),
                    turn: sched.turn,
                });
            }
            if let Some(wakes) = wakes {
                let GlobalResponse::RobustListWakes(counts) = &response.1 else {
                    panic!("a complete admitted exit batch was refused: {response:?}");
                };
                let clocks =
                    serde_json::to_value(&*self.state.global_time.lock().unwrap()).unwrap();
                let turn = self.state.sched.lock().unwrap().turn;
                self.observations.lock().unwrap().push(WakeObservation {
                    wakes,
                    counts: counts.clone(),
                    clocks,
                    turn,
                });
            }
            response
        }

        fn config(&self) -> &Config {
            &self.state.cfg
        }
    }

    struct Fixture {
        state: GlobalState,
        tool: Detcore,
        owners: [ThreadState<()>; 2],
        initial_clocks: [DetTime; 2],
        waiters: [DetTid; 2],
        peer: DetTid,
        futexes: [FutexID; 2],
        observations: Mutex<Vec<WakeObservation>>,
        rpc_observations: Mutex<Vec<RpcObservation>>,
    }

    impl Fixture {
        fn new(
            reason: RobustListExit,
            equal_clocks: bool,
            empty_owner: Option<usize>,
            needs_rpc_cancellation: bool,
        ) -> Self {
            let config = Config {
                sequentialize_threads: true,
                ..Config::default().with_backend(|backend| {
                    backend.needs_killed_thread_rpc_cancellation = needs_rpc_cancellation
                })
            };
            let state = GlobalState::initialize(&config, false);
            let parent = DetTid::from_raw(1);
            let leader = DetTid::from_raw(17);
            let worker = DetTid::from_raw(18);
            let waiters = [DetTid::from_raw(21), DetTid::from_raw(23)];
            let peer = DetTid::from_raw(25);
            let mm = MmId::initial(leader);
            let mut first = ThreadState::new(leader, &config, ());
            first.detpid = Some(leader);
            let mut second = first.clone();
            second.dettid = worker;
            let mut inherited = DetTime::new(&config);
            inherited.add_syscall_with_cost(1_000);
            let first_initial = inherited.clone_for_child();
            inherited.add_syscall_with_cost(370);
            let second_initial = inherited.clone_for_child();
            first.thread_logical_time = first_initial.clone();
            second.thread_logical_time = second_initial.clone();
            first
                .thread_logical_time
                .add_syscall_with_cost(if equal_clocks { 407 } else { 37 });
            second.thread_logical_time.add_syscall_with_cost(37);
            first.record_robust_list_head(Some(0x404100));
            second.record_robust_list_head(Some(0x404200));
            let object = SharedMemoryObjectId::Anonymous {
                origin: MmId::initial(parent),
                sequence: 1,
            };
            let futexes = [FutexID::shared(object, 0), FutexID::shared(object, 8)];
            first.stage_robust_list_wakes(
                reason,
                vec![
                    (
                        worker,
                        if empty_owner == Some(1) {
                            Vec::new()
                        } else {
                            vec![RobustListWake { futex: futexes[1] }]
                        },
                    ),
                    (
                        leader,
                        if empty_owner == Some(0) {
                            Vec::new()
                        } else {
                            vec![RobustListWake { futex: futexes[0] }]
                        },
                    ),
                ],
            );
            {
                let mut sched = state.sched.lock().unwrap();
                sched.thread_tree.add_child(parent, parent, true);
                sched.thread_tree.add_child(parent, leader, true);
                sched.thread_tree.add_child(leader, worker, false);
                for tid in [waiters[0], waiters[1], peer] {
                    sched.thread_tree.add_child(parent, tid, true);
                }
                for tid in [leader, worker, waiters[0], waiters[1], peer] {
                    sched.priorities.insert(tid, DEFAULT_PRIORITY);
                    sched.next_turns.insert(
                        tid,
                        ThreadNextTurn {
                            dettid: tid,
                            child_tid_addr: 0,
                            req: Ivar::new(),
                            resp: Ivar::new(),
                            protocol: Default::default(),
                        },
                    );
                    sched.install_test_exec_incarnation(
                        tid,
                        if tid == leader || tid == worker {
                            mm
                        } else {
                            MmId::initial(tid)
                        },
                    );
                }
                // Both owners have returned from their last guest turn and
                // retain its empty next request until their exit callbacks.
                // The independent peer is ready, but cannot pass quiescence.
                sched.runqueue_push_back(leader);
                sched.runqueue_push_back(worker);
                sched.runqueue_push_back(peer);
                sched.next_turns[&peer].req.put(Ok(Resources::new(peer)));
                for (waiter, futex) in waiters.into_iter().zip(futexes) {
                    sched.sleep_futex_waiter(&waiter, futex, None, u32::MAX, None);
                }
            }
            {
                let mut time = state.global_time.lock().unwrap();
                for (tid, clock) in [(leader, &first_initial), (worker, &second_initial)] {
                    time.update_global_time(tid, clock.as_nanos(), clock.inherited_nanos());
                }
            }
            let tool = Detcore::new(Tid::from_raw(leader.as_raw()), &config);
            Self {
                state,
                tool,
                owners: [first, second],
                initial_clocks: [first_initial, second_initial],
                waiters,
                peer,
                futexes,
                observations: Mutex::new(Vec::new()),
                rpc_observations: Mutex::new(Vec::new()),
            }
        }

        fn assert_clocks(&self, completed: &[usize]) -> serde_json::Value {
            let time = self.state.global_time.lock().unwrap();
            let snapshot = serde_json::to_value(&*time).unwrap();
            let epoch = DetTime::new(&self.state.cfg).as_nanos();
            let mut expected = epoch;
            for (index, owner) in self.owners.iter().enumerate() {
                let clock = if completed.contains(&index) {
                    &owner.thread_logical_time
                } else {
                    &self.initial_clocks[index]
                };
                assert_eq!(time.threads_time(owner.dettid), clock.as_nanos());
                assert_eq!(
                    snapshot["inherited_time"][owner.dettid.as_raw().to_string()],
                    serde_json::to_value(clock.inherited_nanos()).unwrap()
                );
                expected = expected + (clock.as_nanos() - epoch - clock.inherited_nanos());
            }
            assert_eq!(
                time.as_nanos(),
                expected,
                "only each owner's own uninherited work contributes"
            );
            snapshot
        }

        async fn exit(&self, index: usize, status: ExitStatus) {
            self.exit_thread(self.owners[index].clone(), status).await;
        }

        async fn exit_thread(&self, thread: ThreadState<()>, status: ExitStatus) {
            let rpc = ExitRpc {
                state: &self.state,
                sender: thread.dettid,
                observations: &self.observations,
                rpc_observations: &self.rpc_observations,
            };
            let exit =
                self.tool
                    .on_exit_thread(Tid::from_raw(rpc.sender.as_raw()), &rpc, thread, status);
            let mut exit = std::pin::pin!(exit);
            // On the CLI's current-thread ptrace route, Ready on the first
            // poll excludes a new queued-observer window inside this callback.
            // Multi-thread embeddings still admit concurrent scheduler reads;
            // the RPC observations separately check the actual global actions.
            assert!(
                matches!(futures::poll!(exit.as_mut()), Poll::Ready(Ok(()))),
                "exit callback yielded before its accounting and cleanup completed"
            );
        }

        async fn nonmember_exit(&self, raw_tid: i32) {
            let mut thread = self.owners[0].clone();
            thread.dettid = DetTid::from_raw(raw_tid);
            thread.thread_logical_time = DetTime::new(&self.state.cfg);
            let tid = thread.dettid;
            {
                let mut sched = self.state.sched.lock().unwrap();
                sched
                    .thread_tree
                    .add_child(self.owners[0].dettid, tid, false);
                sched.priorities.insert(tid, DEFAULT_PRIORITY);
                sched.next_turns.insert(
                    tid,
                    ThreadNextTurn {
                        dettid: tid,
                        child_tid_addr: 0,
                        req: Ivar::new(),
                        resp: Ivar::new(),
                        protocol: Default::default(),
                    },
                );
                sched.install_test_exec_incarnation(tid, thread.mm_id);
                sched.runqueue_push_back(tid);
            }
            let before = self.rpc_observations.lock().unwrap().len();
            self.exit_thread(thread, ExitStatus::Exited(0)).await;
            let observations = self.rpc_observations.lock().unwrap();
            assert_eq!(
                observations.len(),
                before + 1,
                "a nonmember sent an acknowledgement or wake RPC"
            );
            assert_eq!(observations[before].sender, tid);
            assert_eq!(observations[before].kind, "deregister");
            assert!(observations[before].accepted);
        }
    }

    #[tokio::test]
    async fn robust_exit_acknowledgements_preserve_global_action_order_and_eligibility() {
        for order in [[0, 1], [1, 0]] {
            for empty_owner in [0, 1] {
                let f = Fixture::new(RobustListExit::ExitGroup, false, Some(empty_owner), false);
                f.nonmember_exit(30).await;
                assert!(f.observations.lock().unwrap().is_empty());
                let queue_before_first: Vec<_> = f
                    .state
                    .sched
                    .lock()
                    .unwrap()
                    .run_queue
                    .tids()
                    .copied()
                    .collect();
                let first_start = f.rpc_observations.lock().unwrap().len();
                f.exit(order[0], ExitStatus::Exited(0)).await;
                let first_clocks = f.assert_clocks(&[order[0]]);
                {
                    let observations = f.rpc_observations.lock().unwrap();
                    let actions = &observations[first_start..];
                    assert_eq!(
                        actions.iter().map(|a| a.kind).collect::<Vec<_>>(),
                        ["empty-wake", "deregister"]
                    );
                    assert!(actions.iter().all(|a| a.accepted
                        && a.sender == f.owners[order[0]].dettid
                        && a.clocks == first_clocks
                        && a.waiters == 2
                        && a.turn == 0));
                    assert_eq!(
                        actions[0].queued, queue_before_first,
                        "empty acknowledgement changed scheduler eligibility"
                    );
                }
                f.nonmember_exit(31).await;
                f.assert_clocks(&[order[0]]);
                assert!(
                    f.observations.lock().unwrap().is_empty(),
                    "a nonmember completed the physical-exit barrier"
                );
                let queue_before_last: Vec<_> = f
                    .state
                    .sched
                    .lock()
                    .unwrap()
                    .run_queue
                    .tids()
                    .copied()
                    .collect();
                let last_start = f.rpc_observations.lock().unwrap().len();
                f.exit(order[1], ExitStatus::Exited(0)).await;
                let final_clocks = f.assert_clocks(&[0, 1]);
                {
                    let observations = f.rpc_observations.lock().unwrap();
                    let actions = &observations[last_start..];
                    assert_eq!(
                        actions.iter().map(|a| a.kind).collect::<Vec<_>>(),
                        ["empty-wake", "wake", "deregister"]
                    );
                    assert!(actions.iter().all(|a| a.accepted
                        && a.sender == f.owners[order[1]].dettid
                        && a.clocks == final_clocks
                        && a.turn == 0));
                    assert_eq!(actions[0].waiters, 2);
                    assert_eq!(actions[1].waiters, 1);
                    assert_eq!(actions[0].queued, queue_before_last);
                    assert_eq!(
                        actions[1].queued, queue_before_last,
                        "wake bypassed deferred admission"
                    );
                }
                f.nonmember_exit(32).await;
                f.assert_clocks(&[0, 1]);
                assert_eq!(f.observations.lock().unwrap().len(), 1);
                let observations = f.rpc_observations.lock().unwrap();
                for owner in &f.owners {
                    assert_eq!(
                        observations
                            .iter()
                            .filter(|a| a.sender == owner.dettid && a.kind == "empty-wake")
                            .count(),
                        1,
                        "each unique matching owner, including an empty-wake owner, must acknowledge before the batch clears"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn robust_exit_callbacks_preserve_owner_clocks_across_arrival_orders() {
        for reason in [
            RobustListExit::ExitGroup,
            RobustListExit::Signal(libc::SIGTERM),
        ] {
            for equal_clocks in [false, true] {
                for empty_owner in [None, Some(0), Some(1)] {
                    let mut results = Vec::new();
                    for order in [[0, 1], [1, 0]] {
                        let f = Fixture::new(reason, equal_clocks, empty_owner, false);
                        let status = match reason {
                            RobustListExit::ExitGroup => ExitStatus::Exited(0),
                            RobustListExit::Signal(_) => {
                                ExitStatus::Signaled(Signal::SIGTERM, false)
                            }
                        };
                        f.exit(order[0], status).await;
                        f.assert_clocks(&[order[0]]);
                        assert!(f.observations.lock().unwrap().is_empty());
                        {
                            let sched = f.state.sched.lock().unwrap();
                            assert_eq!(
                                sched
                                    .blocked
                                    .futex_waiters
                                    .values()
                                    .map(Vec::len)
                                    .sum::<usize>(),
                                2
                            );
                            assert_eq!(sched.turn, 0);
                        }
                        // An actually runnable peer must still wait for the
                        // remaining owner's outstanding next request.
                        {
                            let last = Err(SkipTurn);
                            let turn = crate::scheduler::do_a_turn_blocking(
                                f.state.sched.clone(),
                                f.state.global_time.clone(),
                                &last,
                            );
                            let mut turn = std::pin::pin!(turn);
                            assert!(matches!(futures::poll!(turn.as_mut()), Poll::Pending));
                        }
                        f.exit(order[0], status).await;
                        f.assert_clocks(&[order[0]]);
                        assert!(
                            f.observations.lock().unwrap().is_empty(),
                            "duplicate physical exit released an incomplete group"
                        );
                        f.exit(order[1], status).await;
                        let clocks = f.assert_clocks(&[0, 1]);
                        {
                            let observations = f.observations.lock().unwrap();
                            assert_eq!(observations.len(), 1);
                            let expected: Vec<_> = (0..2)
                                .filter(|i| Some(*i) != empty_owner)
                                .map(|i| (f.owners[i].dettid, f.futexes[i]))
                                .collect();
                            assert_eq!(observations[0].wakes, expected);
                            assert_eq!(observations[0].counts, vec![1; expected.len()]);
                            assert_eq!(
                                observations[0].clocks, clocks,
                                "all owner clocks must be accounted before wake admission"
                            );
                            assert_eq!(observations[0].turn, 0);
                        }
                        f.exit(order[1], status).await;
                        assert_eq!(f.assert_clocks(&[0, 1]), clocks);
                        assert_eq!(
                            f.observations.lock().unwrap().len(),
                            1,
                            "repeated cleanup emitted a second batch"
                        );
                        {
                            let sched = f.state.sched.lock().unwrap();
                            assert!(!sched.run_queue.contains_tid(f.waiters[0]));
                            assert!(!sched.run_queue.contains_tid(f.waiters[1]));
                        }
                        // Drive the real step1/step2 drain and one peer turn;
                        // no test-only scheduler implementation or reply shim.
                        let result = crate::scheduler::do_a_turn_blocking(
                            f.state.sched.clone(),
                            f.state.global_time.clone(),
                            &Err(SkipTurn),
                        )
                        .await;
                        assert!(result.is_ok());
                        let sched = f.state.sched.lock().unwrap();
                        assert_eq!(sched.turn, 1);
                        let queued: Vec<_> = sched.run_queue.tids().copied().collect();
                        for (i, waiter) in f.waiters.iter().enumerate() {
                            assert_eq!(queued.contains(waiter), Some(i) != empty_owner);
                        }
                        assert!(queued.contains(&f.peer));
                        results.push((
                            clocks,
                            queued,
                            std::mem::take(&mut *f.observations.lock().unwrap()),
                        ));
                    }
                    assert_eq!(
                        results[0], results[1],
                        "callback order changed final clocks, typed wake observations or the actual scheduler drain"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn robust_exit_callbacks_keep_rejected_or_mismatched_batches_incomplete() {
        for rejection in ["old-mm", "tombstone", "wrong-signal", "normal-exit"] {
            let f = Fixture::new(
                RobustListExit::Signal(libc::SIGTERM),
                false,
                None,
                rejection == "tombstone",
            );
            let rejected = f.owners[0].dettid;
            let mm = f.owners[0].mm_id;
            f.exit(1, ExitStatus::Signaled(Signal::SIGTERM, false))
                .await;
            let replacement_request = Ivar::new();
            if rejection == "old-mm" {
                let mut sched = f.state.sched.lock().unwrap();
                sched.install_test_exec_incarnation(rejected, mm.for_exec(rejected));
                sched.next_turns.insert(
                    rejected,
                    ThreadNextTurn {
                        dettid: rejected,
                        child_tid_addr: 0,
                        req: replacement_request.clone(),
                        resp: Ivar::new(),
                        protocol: Default::default(),
                    },
                );
                let mut replacement_time = f.owners[0].thread_logical_time.clone();
                replacement_time.add_syscall_with_cost(500);
                f.state.global_time.lock().unwrap().update_global_time(
                    rejected,
                    replacement_time.as_nanos(),
                    replacement_time.inherited_nanos(),
                );
            } else if rejection == "tombstone" {
                // Use the real cancelling backend gate; keep its matching Mm.
                let mut sched = f.state.sched.lock().unwrap();
                sched.logically_kill_thread(&rejected, &rejected, mm);
            }
            let before = serde_json::to_value(&*f.state.global_time.lock().unwrap()).unwrap();
            let status = match rejection {
                "wrong-signal" => ExitStatus::Signaled(Signal::SIGKILL, false),
                "normal-exit" => ExitStatus::Exited(0),
                _ => ExitStatus::Signaled(Signal::SIGTERM, false),
            };
            f.exit(0, status).await;
            assert!(
                f.observations.lock().unwrap().is_empty(),
                "{rejection} released a group"
            );
            if rejection == "old-mm" || rejection == "tombstone" {
                assert_eq!(
                    serde_json::to_value(&*f.state.global_time.lock().unwrap()).unwrap(),
                    before,
                    "rejected acknowledgement changed clock state"
                );
            }
            let sched = f.state.sched.lock().unwrap();
            assert_eq!(
                sched
                    .blocked
                    .futex_waiters
                    .values()
                    .map(Vec::len)
                    .sum::<usize>(),
                2,
                "rejected group lost a real waiter"
            );
            assert!(
                f.waiters
                    .iter()
                    .all(|tid| !sched.run_queue.contains_tid(*tid))
            );
            if rejection == "old-mm" {
                assert!(sched.rpc_incarnation_matches(rejected, mm.for_exec(rejected)));
                assert_eq!(
                    sched.next_turns[&rejected].req, replacement_request,
                    "old cleanup destroyed replacement registration"
                );
            }
        }
    }

    #[tokio::test]
    #[should_panic(expected = "Attempted to update tid 17 time")]
    async fn robust_exit_clock_ack_still_refuses_a_backwards_owner_sample() {
        let f = Fixture::new(RobustListExit::ExitGroup, false, None, false);
        let owner = &f.owners[0];
        let mut later = owner.thread_logical_time.clone();
        later.add_syscall_with_cost(1);
        f.state.global_time.lock().unwrap().update_global_time(
            owner.dettid,
            later.as_nanos(),
            later.inherited_nanos(),
        );
        f.exit(0, ExitStatus::Exited(0)).await;
    }

    #[tokio::test]
    async fn backend_failure_preserves_consuming_robust_exit_clock_accounting() {
        for order in [[0, 1], [1, 0]] {
            let f = Fixture::new(RobustListExit::ExitGroup, false, None, false);
            let selected = {
                let mut sched = f.state.sched.lock().unwrap();
                sched.next_turns.get_mut(&f.peer).unwrap().req = Ivar::new();
                sched.select_test_turn().unwrap()
            };
            let responses = {
                let sched = f.state.sched.lock().unwrap();
                f.waiters.map(|tid| sched.next_turns[&tid].resp.clone())
            };
            let mut turn = std::pin::pin!(crate::scheduler::finish_selected_turn(
                f.state.sched.clone(),
                f.state.global_time.clone(),
                selected.0,
                selected.1,
                selected.2,
            ));
            assert!(futures::poll!(turn.as_mut()).is_pending());
            f.state.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(17),
                tid: Tid::from_raw(18),
                phase: "native robust cleanup control",
            });
            for index in order {
                f.exit(index, ExitStatus::Exited(0)).await;
            }
            assert!(matches!(futures::poll!(turn.as_mut()), Poll::Ready(Err(_))));
            f.assert_clocks(&[0, 1]);
            let observations = f.observations.lock().unwrap();
            assert_eq!(observations.len(), 1, "one complete batch");
            assert_eq!(observations[0].counts, vec![1, 1]);
            assert!(
                responses
                    .iter()
                    .all(|response| response.try_read().is_none())
            );
            let mut sched = f.state.sched.lock().unwrap();
            assert_eq!(sched.turn, 0);
            for owner in &f.owners {
                assert!(!sched.next_turns.contains_key(&owner.dettid));
                assert!(!sched.note_deregistration_accounted(owner.dettid));
            }
        }
    }
}
