/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The process-local portion of the Detcore Reverie-tool.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::os::fd::BorrowedFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

use detcore_model::pedigree::Pedigree;
use detcore_model::summary::TimesliceStats;
use detcore_model::syscall_histogram::SyscallHistogram;
use nix::fcntl::OFlag;
use nix::sys::stat;
use nix::unistd::Pid;
use rand::Rng as _;
use rand::RngExt as _;
use rand::SeedableRng;
use rand_distr::Distribution;
use rand_distr::Exp;
use rand_pcg::Pcg64Mcg;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
use tracing::debug;
use tracing::info;

use crate::config::Config;
use crate::detlog;
use crate::fd::*;
use crate::memory::MemoryMetadata;
use crate::preemptions::ThreadHistoryIterator;
use crate::record_or_replay::NoopTool;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::ChaosEpochTransition;
use crate::resources::Device;
use crate::resources::ExternalOpId;
use crate::resources::InstructionOffset;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::Priority;
use crate::stat::*;
use crate::types::*;

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
    /// Socket-only sequence used for backend-independent socket cookies.
    #[serde(default)]
    next_socket_open_file_sequence: u64,
    /// Random-device-only sequence that names each random-device open file's
    /// stream (`crate::fd::RandomDeviceStream`).
    #[serde(default)]
    next_random_device_sequence: u64,
    /// A random-device or timerfd OFD from this table has been promoted.
    /// Retain this history after its final close so exec cannot reuse a
    /// canonical identity. (Serialized under its earlier name, which the
    /// field had while only random devices were promoted.)
    #[serde(default, rename = "has_promoted_random_files")]
    has_promoted_shared_files: bool,
    /// A description from this table has kept its record's sequence in
    /// Detcore's global state (`ProcfsFile::share_sequence`, at a fork or a
    /// send), keyed by its identity: so an exec on a backend that rebuilds
    /// the table carries this table's identity numbering, and no description
    /// of the new image takes an identity whose sequence another process may
    /// still hold (`ThreadState::procfs_records_for_exec`). Kept after the
    /// description closes, and by `fork_for` and `for_exec`.
    #[serde(default)]
    has_shared_procfs_sequences: bool,
    /// Track what file handles actually point to (e.g. after dup2).
    /// This includes both the identifying resource (usually inode) and the deterministic file handle.
    /// Deserialization restores aliasing (`crate::fd::intern_open_files`).
    #[serde(deserialize_with = "deserialize_interned_file_handles")]
    pub(crate) file_handles: HashMap<RawFd, DetFd>,
    /// Blocking accepts waiting in this table, each as (operation, listener):
    /// a `close` of a listener here is refused while an accept waits on it
    /// (`crate::syscalls::accept_in_turn`). Waits never cross a fork or an
    /// exec: a fork's child and an exec'd task start with none, and each wait
    /// removes only its own entry, from the table it registered in.
    // TODO-HUMAN-REVIEW(PR-3908)
    #[serde(skip)]
    accept_waits: Vec<(u64, RawFd)>,
    /// The next accept-wait operation number in this table.
    #[serde(skip)]
    next_accept_wait: u64,
    /// Whether the session renders `/proc/cpuinfo`, so that a memfd named as
    /// its placeholder may be one when this table rediscovers descriptors
    /// without a guest at hand (an in-guest backend's image after `exec`):
    /// the session's own answer (`Detcore::rendered_files`), taken at the
    /// image's first guest callback (`decide_cpuinfo_placeholders`), and kept
    /// by `fork_for` and `for_exec`. `None` until then, and when absent from
    /// an older serialized table. Detcore makes no such placeholder in a
    /// session that does not render the file, so there a memfd of that name
    /// stays an ordinary memfd.
    #[serde(default)]
    cpuinfo_placeholders: Option<bool>,
    /// Descriptors rediscovered before [`Self::cpuinfo_placeholders`] was
    /// decided whose memfd is named as a placeholder for a file rendered only
    /// where `/proc/cpuinfo` is (the file itself, or a task's `arch_status`):
    /// kept as the ordinary descriptors they look like until the session's
    /// answer says whether they are placeholders
    /// (`decide_cpuinfo_placeholders`).
    #[serde(default)]
    pending_cpuinfo_placeholders: Vec<RawFd>,
    /// The run's `--proc` level, so that a memfd named as a placeholder for
    /// a file the level omits stays the memfd it is when this table
    /// discovers it (`discover_fd_from_current_process`): Detcore makes no
    /// placeholder for such a file, whose open fails with ENOENT. Noted as
    /// the table is made for a thread (`ThreadState::new`) and again at each
    /// image's first guest callback (`decide_cpuinfo_placeholders`), kept by
    /// `fork_for` and `for_exec`, and serialized with the table, so a table
    /// carried across a fork or an `exec` knows it before it rediscovers a
    /// descriptor. `None` until noted (a table no thread made) and when absent
    /// from an older serialized table, where no name is refused for its
    /// level.
    #[serde(default)]
    proc_level: Option<detcore_model::config::ProcLevel>,
    /// Descriptors open in this table that Detcore keeps no description of,
    /// with whether each is close-on-exec: a received anonymous inode, or a
    /// received descriptor whose facts could not be taken
    /// (`Detcore::record_received_descriptors`). Listed in the table's
    /// descriptor directory (`syscalls::files::descriptor_directory`), so a
    /// close-all loop reaches them; forgotten when the number is closed or
    /// replaced.
    #[serde(default)]
    present_unmodelled: BTreeMap<RawFd, bool>,
    /// Whether a receive may have installed descriptors in this table whose
    /// numbers Detcore does not know (a receive whose layout could not be
    /// trusted, one on the background path, or one without sequentialized
    /// threads): then a listing of the table cannot come from Detcore's
    /// model.
    #[serde(default)]
    unaccounted_receipts: bool,
    /// The capacity Linux's descriptor table has (`max_fds`): 64
    /// (`NR_OPEN_DEFAULT`) until a descriptor at or above it is installed,
    /// then the next power of two above the highest one installed
    /// (`expand_files`, `alloc_fdtable`); a fork's copy is sized from the
    /// descriptors open at the fork (`dup_fd`, `sane_fdtable_size`); `exec`
    /// keeps the table. The end of a descriptor directory's listing.
    #[serde(default = "default_descriptor_capacity")]
    descriptor_capacity: u64,
    /// Whether the exec that made this table dropped a close-on-exec
    /// description whose locks Detcore follows (`DetFd::is_lock_tracked`):
    /// the post-exec handler then names the surviving ones to
    /// `procfs::locks::LockLedger` even where none survived, so that the
    /// ledger releases a placeholder `flock` the exec closed
    /// (`LockLedger::exec_kept`).
    #[serde(default)]
    exec_dropped_lock_tracked: bool,
    /// Descriptors queued over `SCM_RIGHTS` that Linux released in the
    /// current call (the queue of a socket whose last reference this table
    /// dropped, `FileMetadata::release_detfd`, or what a receive or a purge
    /// handed over), until the call completes and releases what they held
    /// (`Detcore::settle_released_rights`). Never serialized, and never
    /// copied.
    #[serde(skip)]
    released_rights: ReleasedRights,
    /// The processes whose threads hold this table, and how many threads of
    /// each, counted by guest calls alone: its first holder; each thread or
    /// process made with `CLONE_FILES`, counted as its creator's `clone`
    /// makes it, before either runs on; and each holder's exit, a thread's
    /// `exit` counting one out and an `exit_group` its whole process
    /// ([`Self::leave`]). The count of thread states that share the table
    /// cannot say which exit is its last: the backend drops an exited
    /// thread's state at no particular point of the schedule. Empty for a
    /// table whose holders are not known (one carried across a snapshot),
    /// which no exit releases.
    #[serde(default)]
    holders: BTreeMap<DetPid, usize>,
    /// The table the exec that made this one replaced, and the exec'ing
    /// process (`Self::keep_replaced_table`): kept from the exec's entry
    /// (`Detcore::handle_execveat`) until the new image's first callback,
    /// where that process leaves the replaced table, and where it was the
    /// table's last holder, the table's descriptors go, the close-on-exec ones
    /// as Linux closes them at the exec
    /// (`ThreadState::release_exec_dropped_descriptors`). A failed exec drops
    /// this table, and the replaced one with it. Never serialized, and never
    /// copied.
    #[serde(skip)]
    exec_replaced: ReplacedTable,
    /// The open file descriptions a successful file-backed `mmap` through a
    /// descriptor of this table mapped (`Detcore::note_flock_mapping`): a
    /// mapping keeps its description, and so a `flock` lock it holds, after
    /// its last descriptor goes (`vm_file`). Copied by a fork, whose child
    /// inherits the mappings, and dropped by an exec, which replaces them.
    /// Never cleared by `munmap`, so it names every description the process
    /// may still map. Serialized under no older name.
    #[serde(default)]
    mapped_open_files: BTreeSet<OpenFileId>,
}

/// The table an exec replaced, and the exec'ing process
/// ([`FileMetadata::exec_replaced`]). A copy of the table that holds it
/// starts with none: the exec's own table leaves the replaced one, once.
#[derive(Default)]
struct ReplacedTable(Option<(Arc<Mutex<FileMetadata>>, DetPid)>);

impl Clone for ReplacedTable {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for ReplacedTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ReplacedTable")
            .field(&self.0.is_some())
            .finish()
    }
}

/// What the death of a process by a fatal signal did with one of the tables
/// its threads held ([`FileMetadata::release_at_death`]).
#[derive(Debug)]
pub(crate) enum TableAtDeath {
    /// No holder is left, and every descriptor went: what the queues of the
    /// sockets whose last reference went held, for the death to release
    /// (`Detcore::settle_released_rights`'s rule).
    Released(Vec<SentDescriptor>),
    /// Another process still holds the table, which keeps every descriptor.
    Kept,
    /// The table's holders are not known, or do not name the process, so
    /// whether its death released the table cannot be decided.
    Undecided,
}

/// The descriptors of messages queued to sockets a table released
/// ([`FileMetadata::released_rights`]). A copy of the table, for a fork or an
/// exec, starts with none: what one table released is released once.
#[derive(Debug, Default)]
struct ReleasedRights(Vec<SentDescriptor>);

impl Clone for ReleasedRights {
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// `NR_OPEN_DEFAULT` on 64-bit Linux.
fn default_descriptor_capacity() -> u64 {
    64
}

/// Deserializes a descriptor table with its aliases restored, so a table
/// carried across fork (or any other state migration) keeps one open file
/// description object per `OpenFileId`.
fn deserialize_interned_file_handles<'de, D>(
    deserializer: D,
) -> Result<HashMap<RawFd, DetFd>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut handles = <HashMap<RawFd, DetFd> as Deserialize>::deserialize(deserializer)?;
    crate::fd::intern_open_files(&mut handles);
    Ok(handles)
}

/// A descriptor identity held across an awaited syscall.
///
/// The clone retains the source open-file description until the operation
/// finishes. `files_id` binds it to the table in which the syscall began.
#[derive(Debug)]
pub(crate) struct CapturedDetFd {
    files_id: FilesId,
    detfd: DetFd,
}

/// A captured descriptor could not be installed into the current descriptor
/// table. The capture is returned so the caller can release any resource held
/// alive solely by the in-flight operation.
#[derive(Debug)]
pub(crate) struct CapturedDetFdInstallError {
    pub(crate) expected_files_id: FilesId,
    pub(crate) actual_files_id: FilesId,
    returned_fd: RawFd,
    pub(crate) captured: CapturedDetFd,
}

/// Cleanup required after the kernel created a descriptor but Detcore could not
/// install its captured open-file-description identity in the current table.
#[derive(Debug)]
pub(crate) struct CapturedDetFdInstallCleanup {
    /// The exact kernel-returned descriptor that must be closed.
    pub(crate) close_fd: RawFd,
    /// A scheduler resource whose final modeled OFD reference was the capture.
    pub(crate) release_open_file: Option<OpenFileId>,
    /// Where that capture was a receiving socket's last reference, the
    /// messages queued to it, which go with it as at a close
    /// ([`FileMetadata::release_detfd`]), for the call to release
    /// (`ThreadState::defer_released_rights`).
    pub(crate) released_rights: Vec<SentDescriptor>,
}

impl CapturedDetFdInstallError {
    /// Abandon the failed capture and preserve both cleanup obligations.
    pub(crate) fn into_cleanup(self) -> CapturedDetFdInstallCleanup {
        let last = self.captured.detfd.is_last_reference();
        let release_open_file = last.then(|| self.captured.detfd.open_file_id());
        let released_rights = if last {
            self.captured.detfd.take_all_queued_rights()
        } else {
            Vec::new()
        };
        drop(self.captured);
        CapturedDetFdInstallCleanup {
            close_fd: self.returned_fd,
            release_open_file,
            released_rights,
        }
    }
}

/// Whether a pidfd target is proven to use the caller's exact descriptor table.
///
/// Linux pidfds name a specific task, and `pidfd_open(2)` accepts a thread-group
/// leader. Equal TGIDs alone are not enough: a `CLONE_THREAD` child may omit
/// `CLONE_FILES` and therefore have a different `files_struct`. Restrict the
/// modeled path to the leader itself, where `target == getpid() == gettid()`
/// proves that the task named by the pidfd and the caller are identical.
pub(crate) fn pidfd_getfd_targets_calling_task(
    target: Option<DetPid>,
    current_tgid: DetPid,
    current_tid: DetTid,
) -> bool {
    target == Some(current_tgid) && current_tid == current_tgid
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1154): Review SaBRe exec descriptor-status handoff state.
/// Descriptor numbers that Detcore keeps physically nonblocking while presenting them as
/// blocking to the guest. SaBRe carries this narrow status set across an exec plugin reload.
pub type ExecFdBlockingOverrides = BTreeSet<RawFd>;

/// Metadata carried by the existing exec preparation/completion exchange.
/// Descriptor numbers are authorized by that exchange's process/image fence,
/// not by rediscovering whichever object later occupies a numeric stdio slot.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecFdState {
    pub(crate) blocking: ExecFdBlockingOverrides,
    pub(crate) inherited_reads: BTreeMap<RawFd, crate::fd::InheritedExternalRead>,
    /// Set only by the global first-image completion, never by a preparation.
    pub(crate) initialize_inherited_stdio: bool,
    /// Offered only to the registered root's initial image by its launcher.
    #[serde(default)]
    pub(crate) initial_input: Option<reverie::InheritedInputIdentity>,
    /// The owner-death wakes a successful exec makes Linux deliver, read
    /// before the exec where the backend runs the native execve outside
    /// Detcore's handler (DBT). The coordinator delivers them at the new
    /// image's reconnect, or when the caller's thread ends without one (an
    /// exec that ends its group), and drops them with an exec cancelled before
    /// its point of no return (<https://github.com/rrnewton/hermit/issues/4268>).
    #[serde(default)]
    pub(crate) robust_wakes: Vec<(DetTid, FutexID)>,
    /// Promoted random-device handles and allocator state of a reloaded tool.
    /// Nonrandom descriptors retain their existing exec restoration paths.
    #[serde(default)]
    pub(crate) open_files: Option<ExecOpenFiles>,
    /// Maps descriptions surviving a reload retain their opened target image,
    /// aliases and any cached bytes. Kernel EOF must not erase this authority.
    #[serde(default)]
    pub(crate) maps: Option<ExecMapsFiles>,
    /// On a backend that rebuilds its descriptor table after exec: the
    /// surviving descriptors of per-process files that follow their record's
    /// sequence, with their open files' identities, models and sequences, and
    /// the old table's identity numbering, where it has either
    /// (`ThreadState::procfs_records_for_exec`).
    #[serde(default)]
    pub(crate) procfs_records: Option<ExecProcfsRecords>,
    /// Under `--pipe-wake-latency`, on a backend that rediscovers its
    /// descriptors after exec: the channel end each surviving descriptor is,
    /// reattached after rediscovery (`crate::pipe_wake`).
    #[serde(default)]
    pub(crate) channels: BTreeMap<RawFd, crate::pipe_wake::ChannelEnd>,
    /// Under `--pipe-wake-latency`, on such a backend, at every exec: the old
    /// table's open-file creator and next sequence, reserved before any
    /// rediscovery, so that no description of the new image reuses an id a
    /// channel elsewhere may still carry.
    #[serde(default)]
    pub(crate) open_file_namespace: Option<(DetTid, u64)>,
    /// The old table's count of random-device opens. A backend that
    /// rebuilds its descriptor table after exec starts the new one from it,
    /// so no open after the exec reuses a stream's name
    /// (`crate::fd::RandomDeviceStream`).
    #[serde(default)]
    pub(crate) random_device_sequence: u64,
    /// The image the exec runs, as Detcore resolved it from the exec's
    /// arguments before the exec ([`crate::exec_record`]), or for the
    /// registered root's first image the one its launch named
    /// (`Config::launch_image`), once the coordinator checked it against the
    /// kernel's link. `None` leaves the new image without a record.
    #[serde(default)]
    pub(crate) exe: Option<crate::exec_record::ExecRecord>,
}

/// Written out, not derived, so that the `--pipe-wake-latency` fields are
/// printed only when set: an exec without the option traces as before.
impl std::fmt::Debug for ExecFdState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ExecFdState");
        debug
            .field("blocking", &self.blocking)
            .field("inherited_reads", &self.inherited_reads)
            .field(
                "initialize_inherited_stdio",
                &self.initialize_inherited_stdio,
            )
            .field("initial_input", &self.initial_input)
            .field("robust_wakes", &self.robust_wakes)
            .field("open_files", &self.open_files);
        if let Some(maps) = &self.maps {
            // Native authority tokens and kernel descriptor identities are
            // transport-only; they must not enter canonical guest logs.
            debug.field("maps_survivors", &maps.handles.len());
        }
        if let Some(records) = &self.procfs_records {
            debug.field("procfs_record_survivors", &records.handles.len());
        }
        if !self.channels.is_empty() {
            debug.field("channels", &self.channels);
        }
        if let Some(namespace) = &self.open_file_namespace {
            debug.field("open_file_namespace", namespace);
        }
        if self.random_device_sequence != 0 {
            debug.field("random_device_sequence", &self.random_device_sequence);
        }
        if let Some(exe) = &self.exe {
            debug.field("exe", &exe.exe);
        }
        debug.finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecOpenFiles {
    pub(crate) creator: DetTid,
    next_sequence: u64,
    next_socket_sequence: u64,
    handles: BTreeMap<RawFd, ExecOpenFile>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecMapsFiles {
    creator: DetTid,
    next_sequence: u64,
    handles: BTreeMap<RawFd, ExecMapsFile>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExecMapsFile {
    identity: ExecOpenFile,
    procfs: crate::procfs::ProcfsFile,
    flags: i32,
    path: Option<std::path::PathBuf>,
}

/// The descriptors of per-process files that follow their record's sequence
/// (`ProcfsFile::follows_record_sequence`) surviving an exec on a backend
/// that rebuilds its descriptor table after exec, and the old table's open
/// file creator and next identity (`ThreadState::procfs_records_for_exec`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecProcfsRecords {
    creator: DetTid,
    next_sequence: u64,
    handles: BTreeMap<RawFd, ExecProcfsRecord>,
}

/// One surviving descriptor of [`ExecProcfsRecords`]: its open file's
/// identity, authenticated against the descriptor after the exec, its model,
/// its record's sequence whole (the model does not serialize where the
/// sequence is, `ProcfsFile::read_pos`), its status flags and path, and
/// whether that sequence is one Detcore cannot know
/// (`DetFd::record_sequence_unknown`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExecProcfsRecord {
    identity: ExecOpenFile,
    procfs: crate::procfs::ProcfsFile,
    sequence: crate::procfs::ProcfsSequence,
    flags: i32,
    path: Option<std::path::PathBuf>,
    sequence_unknown: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExecOpenFile {
    id: OpenFileId,
    dev: u64,
    ino: u64,
    rdev: u64,
    file_type: u32,
    access: i32,
}

impl std::fmt::Debug for ExecOpenFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecOpenFile")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl ExecOpenFile {
    fn capture(fd: RawFd, id: OpenFileId) -> Result<Self, Errno> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // Runtime-owned read-only inspection of this process's descriptor table.
        let result = unsafe {
            crate::util::raw_syscall(
                libc::SYS_fstat,
                [fd as u64, (&raw mut stat) as u64, 0, 0, 0, 0],
            )
        };
        if result < 0 {
            return Err(Errno::new(-result as i32));
        }
        let flags = unsafe {
            crate::util::raw_syscall(
                libc::SYS_fcntl,
                [fd as u64, libc::F_GETFL as u64, 0, 0, 0, 0],
            )
        };
        if flags < 0 {
            return Err(Errno::new(-flags as i32));
        }
        Ok(Self {
            id,
            dev: stat.st_dev,
            ino: stat.st_ino,
            rdev: stat.st_rdev,
            file_type: stat.st_mode & libc::S_IFMT,
            access: flags as i32 & libc::O_ACCMODE,
        })
    }
}

impl ExecFdState {
    pub(crate) fn open_file_creator(&self) -> Option<DetTid> {
        self.open_files
            .as_ref()
            .map(|files| files.creator)
            .or_else(|| self.open_file_namespace.map(|(creator, _)| creator))
            .or_else(|| self.maps.as_ref().map(|maps| maps.creator))
            .or_else(|| self.procfs_records.as_ref().map(|records| records.creator))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.blocking.is_empty()
            && self.inherited_reads.is_empty()
            && !self.initialize_inherited_stdio
            && self.initial_input.is_none()
            && self.open_files.is_none()
            && self.maps.is_none()
            && self.procfs_records.is_none()
            && self.channels.is_empty()
            && self.open_file_namespace.is_none()
            && self.random_device_sequence == 0
            && self.exe.is_none()
    }
}

impl From<ExecFdBlockingOverrides> for ExecFdState {
    fn from(blocking: ExecFdBlockingOverrides) -> Self {
        Self {
            blocking,
            ..Self::default()
        }
    }
}

/// The wakes an exec's walk of each owner's list found, in delivery order:
/// owners in walk order, each owner's words in futex-key order.
pub(crate) fn flatten_exec_robust_list_wakes(
    wakes: Vec<(DetTid, Vec<RobustListWake>)>,
) -> Vec<(DetTid, RobustListWake)> {
    wakes
        .into_iter()
        .flat_map(|(owner, mut wakes)| {
            wakes.sort_by_key(|wake| format!("{:?}", wake.futex));
            wakes.into_iter().map(move |wake| (owner, wake))
        })
        .collect()
}

/// A single POSIX per-process interval timer created by `timer_create(2)`.
///
/// Detcore records arming against virtual time and schedules supported
/// `SIGEV_SIGNAL` notifications through the deterministic scheduler.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#869)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PosixTimer {
    /// Reload interval for periodic timers, in nanoseconds (0 => one-shot).
    interval_ns: u64,
    /// Absolute virtual-time deadline of the next expiration, or `None` when the
    /// timer is disarmed (`it_value == 0`).
    deadline: Option<LogicalTime>,
    /// Signal number configured by `timer_create`, or `None` for notifications
    /// that Detcore cannot deliver through its scheduler.
    signal: Option<i32>,
    /// The thread a `SIGEV_THREAD_ID` timer notifies, or `None` for a
    /// process-directed one.
    #[serde(default)]
    thread: Option<DetTid>,
    /// The `si_value` of its signal: `sigev_value`, or `sival_int = id`
    /// for a timer created with no `sigevent` (`do_timer_create`).
    #[serde(default)]
    value: u64,
    /// The CPU clock the timer was created on (`crate::cpu_clock`), if any.
    /// Its armings count down on the virtual clock like any other timer's,
    /// an absolute one made relative to the clock's reading.
    #[serde(default)]
    cpu_clock: Option<crate::cpu_clock::CpuTimerClock>,
}

/// The set of POSIX timers owned by a *process*.
///
/// Timers are shared among all threads of a process and, per POSIX, are **not**
/// inherited across `fork(2)`. Detcore therefore shares this table on
/// `CLONE_THREAD` and starts a fresh, empty table for every new process (see
/// `init_thread_state`). Successful exec deletes the timers as well.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PosixTimers {
    /// Deterministic id allocator. Kernel `timer_t`s are opaque, so we hand out
    /// ids as 0, 1, 2, ... in creation order to keep them reproducible.
    next_id: i32,
    timers: HashMap<i32, PosixTimer>,
}

impl PosixTimers {
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// Allocate a new (disarmed) timer, returning its deterministic id.
    /// `thread` is the thread a `SIGEV_THREAD_ID` timer notifies, and
    /// `value` its `sigev_value`, or `None` for a timer created with no
    /// `sigevent`, whose `si_value` is its id. `cpu_clock` is the CPU clock
    /// the timer was created on, if any.
    pub(crate) fn create(
        &mut self,
        signal: Option<i32>,
        thread: Option<DetTid>,
        value: Option<u64>,
        cpu_clock: Option<crate::cpu_clock::CpuTimerClock>,
    ) -> i32 {
        let id = self.next_id;
        self.next_id += 1;
        self.timers.insert(
            id,
            PosixTimer {
                interval_ns: 0,
                deadline: None,
                signal,
                thread,
                value: value.unwrap_or(id as u32 as u64),
                cpu_clock,
            },
        );
        id
    }

    /// The CPU clock timer `id` was created on, if it exists and was.
    pub(crate) fn cpu_clock(&self, id: i32) -> Option<crate::cpu_clock::CpuTimerClock> {
        self.timers.get(&id).and_then(|timer| timer.cpu_clock)
    }

    /// The `si_value` of timer `id`'s signal.
    pub(crate) fn value(&self, id: i32) -> Option<u64> {
        self.timers.get(&id).map(|timer| timer.value)
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
        let old = (
            remaining_ns(timer.deadline, timer.interval_ns, now),
            timer.interval_ns,
        );
        timer.interval_ns = interval_ns;
        timer.deadline = deadline;
        Some(old)
    }

    /// Report the current `(remaining_ns, interval_ns)` for `timer_gettime`, or
    /// `None` if the id is unknown.
    pub(crate) fn gettime(&self, id: i32, now: LogicalTime) -> Option<(u64, u64)> {
        let timer = self.timers.get(&id)?;
        Some((
            remaining_ns(timer.deadline, timer.interval_ns, now),
            timer.interval_ns,
        ))
    }

    /// Whether a timer with this id currently exists.
    pub(crate) fn contains(&self, id: i32) -> bool {
        self.timers.contains_key(&id)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    pub(crate) fn signal(&self, id: i32) -> Option<Option<i32>> {
        self.timers.get(&id).map(|timer| timer.signal)
    }

    /// The thread timer `id` notifies, if it is a `SIGEV_THREAD_ID` timer.
    pub(crate) fn thread(&self, id: i32) -> Option<DetTid> {
        self.timers.get(&id).and_then(|timer| timer.thread)
    }

    /// Remove a timer; returns whether it existed.
    pub(crate) fn remove(&mut self, id: i32) -> bool {
        self.timers.remove(&id).is_some()
    }

    /// Linux deletes all POSIX timers on successful exec. Keep the process's
    /// ID allocator, but invalidate armed, disarmed and non-notifying timers.
    pub(crate) fn clear_for_exec(&mut self) {
        self.timers.clear();
    }
}

/// One virtualized resource limit, represented in the `prlimit64` ABI's units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResourceLimit {
    pub(crate) current: u64,
    pub(crate) maximum: u64,
}

/// Deterministic resource limits owned by one guest process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResourceLimits {
    limits: Vec<ResourceLimit>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        let unlimited = ResourceLimit {
            current: libc::RLIM64_INFINITY,
            maximum: libc::RLIM64_INFINITY,
        };
        let mut limits = vec![unlimited; reverie::syscalls::linux_abi::RLIMIT_RTTIME as usize + 1];
        limits[reverie::syscalls::linux_abi::RLIMIT_STACK as usize] = ResourceLimit {
            current: 8 * 1024 * 1024,
            maximum: libc::RLIM64_INFINITY,
        };
        limits[reverie::syscalls::linux_abi::RLIMIT_NOFILE as usize] = ResourceLimit {
            current: 1_048_576,
            maximum: 1_048_576,
        };
        Self { limits }
    }
}

/// The limit every guest process starts with for `resource`, as
/// `(current, maximum)` in the `prlimit64` ABI's units, or `None` for an
/// invalid resource number.
///
/// A backend whose guest reads a limit before Detcore runs answers it with
/// this. SaBRe is one: its guest's dynamic loader and libc startup run before
/// the plugin is initialized, and libc sizes default thread stacks from
/// `RLIMIT_STACK` there.
pub fn initial_resource_limit(resource: u32) -> Option<(u64, u64)> {
    ResourceLimits::default()
        .get(resource)
        .map(|limit| (limit.current, limit.maximum))
}

impl ResourceLimits {
    /// Return a limit when `resource` is a valid Linux resource number.
    pub(crate) fn get(&self, resource: u32) -> Option<ResourceLimit> {
        self.limits.get(resource as usize).copied()
    }

    /// Replace a previously validated resource limit.
    pub(crate) fn set(&mut self, resource: u32, limit: ResourceLimit) {
        self.limits[resource as usize] = limit;
    }
}

/// Nanoseconds remaining until `deadline` relative to `now`, saturating at 0.
/// A disarmed timer (`None`) or an elapsed one-shot reports 0. Periodic timers
/// advance arithmetically to their next virtual deadline.
fn remaining_ns(deadline: Option<LogicalTime>, interval_ns: u64, now: LogicalTime) -> u64 {
    match deadline {
        Some(d) if d > now => d.as_nanos() - now.as_nanos(),
        Some(d) if interval_ns != 0 => {
            let elapsed = now.as_nanos() - d.as_nanos();
            interval_ns - (elapsed % interval_ns)
        }
        Some(_) => 0,
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
    /// Delegate to the record/replay tool without collapsing a tool failure
    /// into a guest-visible errno. Syscall errors remain [`Error::Errno`], while
    /// failures of the record/replay machinery retain their non-errno variant.
    pub(crate) async fn record_or_replay_preserving_tool_errors<G, S>(
        &self,
        guest: &mut G,
        syscall: S,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
        S: Into<Syscall>,
    {
        preserve_record_or_replay_error(
            self.translated_record_or_replay(guest, syscall.into())
                .await,
        )?
        .map_err(Error::from)
    }

    /// The record-or-replay tool's handling of `syscall`, with a `/proc`
    /// path that names a process or thread by the guest's number reaching
    /// the kernel's directory for it (DBT; see
    /// `syscalls::files::procfs_numbers`).
    async fn translated_record_or_replay<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        // Only a call that looks up a path, on a thread whose `/proc` numbers
        // are translated (DBT), can have anything to translate;
        // `procfs_translated_call` answers `None` at once for every other
        // call. Asking first keeps its futures, and their size, out of every
        // forwarded syscall's future, and the box is allocated only for a
        // call that then reads its path from the guest anyway.
        let translated = if Self::may_translate_procfs_call(guest, &syscall) {
            Box::pin(Self::procfs_translated_call(guest, &syscall)).await?
        } else {
            None
        };
        let call = translated
            .as_ref()
            .map_or(syscall, |translated| translated.call);
        crate::reserved_input::before_native_call(guest, call)?;
        // A self-sentinel `pidfd_send_signal` the executing kernel spells
        // differently from the guest: the model and the record or replay
        // tool see the guest's call, and only its injection is respelled.
        // Boxed, so the adapter's futures stay out of every other forwarded
        // call's future.
        let result = if let Some(respelling) = guest.thread_state().pidfd_respelling {
            Box::pin(self.respelled_record_or_replay(guest, respelling, call)).await
        } else if crate::record_or_replay::injects_only::<T>() {
            // What the tool's handler would have done (`NoopTool`), here:
            // the same single injection of the same call, awaited at the
            // same point, with its errno converted the same way.
            guest.inject(call).await.map_err(Error::from)
        } else {
            self.record_or_replay
                .handle_syscall_event(&mut guest.into_guest(), call)
                .await
        };
        if let Some(translated) = translated {
            Box::pin(Self::finish_translated_call(guest, translated)).await;
        }
        result
    }

    /// What [`Self::translated_record_or_replay`] does with `call`, through
    /// the respelling adapter, which lives in this future and so in its box.
    async fn respelled_record_or_replay<G: Guest<Self>>(
        &self,
        guest: &mut G,
        respelling: crate::pidfd_self::PhysicalRespelling,
        call: Syscall,
    ) -> Result<i64, Error> {
        let mut physical = crate::pidfd_self::PhysicalSpelling::new(guest, respelling);
        if crate::record_or_replay::injects_only::<T>() {
            physical.inject(call).await.map_err(Error::from)
        } else {
            self.record_or_replay
                .handle_syscall_event(&mut physical.into_guest(), call)
                .await
        }
    }

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
        self.translated_record_or_replay(guest, syscall.into())
            .await
            // TODO: Get rid of this and make this whole function use the Error type.
            .map_err(|err| err.into_errno().unwrap())
    }
}

fn preserve_record_or_replay_error(
    result: Result<i64, Error>,
) -> Result<Result<i64, Errno>, Error> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(Error::Errno(error)) => Ok(Err(error)),
        Err(error) => Err(error),
    }
}

/// Linux reports a positive byte count when a later guest-visible write error
/// follows partial progress. An error in the record/replay tool is different:
/// replay itself failed, so it must remain fatal even after earlier bytes were
/// accepted.
pub(crate) fn finish_partial_record_or_replay_write(
    written: i64,
    error: Error,
) -> Result<i64, Error> {
    match error {
        Error::Errno(_) if written > 0 => Ok(written),
        error => Err(error),
    }
}

#[cfg(test)]
mod record_or_replay_error_tests {
    use super::*;

    #[test]
    fn errno_remains_a_guest_syscall_result() {
        let result = preserve_record_or_replay_error(Err(Error::Errno(Errno::EBADF)))
            .expect("guest errno must not abort the tool");
        assert_eq!(result, Err(Errno::EBADF));
    }

    #[test]
    fn tool_failure_remains_an_outer_error() {
        let failure =
            preserve_record_or_replay_error(Err(Error::Tool(anyhow::anyhow!("restore failed"))))
                .expect_err("tool failure must abort replay");
        assert!(matches!(failure.into_errno(), Err(Error::Tool(_))));
    }

    #[test]
    fn partial_write_suppresses_only_guest_errno() {
        assert_eq!(
            finish_partial_record_or_replay_write(7, Error::Errno(Errno::EPIPE)).unwrap(),
            7
        );

        let io_failure = finish_partial_record_or_replay_write(
            7,
            Error::Io(std::io::Error::from_raw_os_error(libc::ENOSPC)),
        )
        .expect_err("host replay-output failure must not become a partial guest write");
        assert!(matches!(io_failure.into_errno(), Err(Error::Io(_))));

        let tool_failure = finish_partial_record_or_replay_write(
            7,
            Error::Tool(anyhow::anyhow!("replay endpoint disappeared")),
        )
        .expect_err("tool failure must not become a partial guest write");
        assert!(matches!(tool_failure.into_errno(), Err(Error::Tool(_))));

        let guest_failure = finish_partial_record_or_replay_write(0, Error::Errno(Errno::EPIPE))
            .expect_err("zero-progress guest failure must remain guest-visible");
        assert_eq!(guest_failure.into_errno().unwrap(), Errno::EPIPE);
    }
}

impl FileMetadata {
    /// create an empty file metadata
    pub(crate) fn new(owner: DetTid) -> Self {
        FileMetadata {
            files_id: FilesId::initial(owner),
            next_open_file_sequence: 0,
            next_socket_open_file_sequence: 0,
            next_random_device_sequence: 0,
            has_promoted_shared_files: false,
            has_shared_procfs_sequences: false,
            file_handles: HashMap::new(),
            accept_waits: Vec::new(),
            next_accept_wait: 0,
            cpuinfo_placeholders: None,
            pending_cpuinfo_placeholders: Vec::new(),
            proc_level: None,
            present_unmodelled: BTreeMap::new(),
            unaccounted_receipts: false,
            descriptor_capacity: default_descriptor_capacity(),
            exec_dropped_lock_tracked: false,
            released_rights: ReleasedRights::default(),
            holders: BTreeMap::from([(owner, 1)]),
            exec_replaced: ReplacedTable::default(),
            mapped_open_files: BTreeSet::new(),
        }
    }

    /// Note that descriptor `fd` was installed in this table: it is no
    /// longer an unmodelled one, and the table grows to hold it as Linux's
    /// does ([`Self::descriptor_capacity`]).
    fn note_installed(&mut self, fd: RawFd) {
        self.present_unmodelled.remove(&fd);
        self.grow_to_hold(fd);
    }

    /// Grow the table to hold descriptor `fd`, as Linux's `expand_files`
    /// does ([`Self::descriptor_capacity`]): for an installed descriptor, and
    /// for the target of a `dup2` or `dup3` that then fails because its old
    /// descriptor is not open, which `ksys_dup3` expands the table for first
    /// (<https://github.com/torvalds/linux/blob/v6.17/fs/file.c#L1408-L1415>).
    fn grow_to_hold(&mut self, fd: RawFd) {
        if let Ok(fd) = u64::try_from(fd)
            && fd >= self.descriptor_capacity
        {
            self.descriptor_capacity = (fd + 1)
                .checked_next_power_of_two()
                .unwrap_or(u64::MAX / 2)
                .max(default_descriptor_capacity());
        }
    }

    /// Set the close-on-exec flag of descriptor `fd` if it is one this table
    /// lists without modelling ([`Self::present_unmodelled`]); whether it is.
    fn set_unmodelled_cloexec(&mut self, fd: RawFd, cloexec: bool) -> bool {
        match self.present_unmodelled.get_mut(&fd) {
            Some(flag) => {
                *flag = cloexec;
                true
            }
            None => false,
        }
    }

    /// The descriptors open in this table that it knows of, ascending:
    /// those it models and those it does not ([`Self::present_unmodelled`]).
    fn descriptor_numbers(&self) -> Vec<RawFd> {
        let mut numbers: Vec<RawFd> = self
            .file_handles
            .keys()
            .copied()
            .chain(self.present_unmodelled.keys().copied())
            .collect();
        numbers.sort_unstable();
        numbers.dedup();
        numbers
    }

    /// The capacity a fork's copy of this table gets: `dup_fd` sizes it from
    /// the highest descriptor open, rounded up to a multiple of
    /// `BITS_PER_LONG` (`sane_fdtable_size`), and allocates a larger table
    /// only past `NR_OPEN_DEFAULT`, to the next power of two
    /// (`alloc_fdtable`).
    fn forked_descriptor_capacity(&self) -> u64 {
        let open = self
            .descriptor_numbers()
            .last()
            .and_then(|fd| u64::try_from(*fd).ok())
            .map_or(0, |fd| (fd + 1).next_multiple_of(64));
        if open <= default_descriptor_capacity() {
            default_descriptor_capacity()
        } else {
            open.checked_next_power_of_two().unwrap_or(u64::MAX / 2)
        }
    }

    /// Note the run's `--proc` level ([`Self::proc_level`]).
    pub(crate) fn note_proc_level(&mut self, level: detcore_model::config::ProcLevel) {
        self.proc_level = Some(level);
    }

    /// The placeholder `found` names, unless it names a file outside the
    /// run's `--proc` level ([`Self::proc_level`]), for which Detcore makes
    /// no placeholder.
    fn within_proc_level(
        &self,
        found: Option<(&'static std::path::Path, Vec<u8>)>,
    ) -> Option<(&'static std::path::Path, Vec<u8>)> {
        found.filter(|(path, _)| {
            self.proc_level.is_none_or(|level| {
                !crate::syscalls::rendered_procfs::named_file_outside_level(path, level)
            })
        })
    }

    /// Take the session's answer to whether it renders `/proc/cpuinfo`
    /// (`rendered`: `Detcore::rendered_files(guest).cpuinfo`), at the first
    /// guest callback of this image, before guest code runs. A descriptor
    /// rediscovered before it whose memfd is named as a placeholder for a
    /// file rendered only where `/proc/cpuinfo` is (the file itself, or a
    /// task's `arch_status`) becomes the placeholder it is where the session
    /// renders the file, and stays an ordinary memfd where it does not. Later
    /// answers of the same session are the same and change nothing.
    pub(crate) fn decide_cpuinfo_placeholders(&mut self, rendered: bool) -> Result<(), Errno> {
        if self.cpuinfo_placeholders.is_some() {
            return Ok(());
        }
        self.cpuinfo_placeholders = Some(rendered);
        let pending = std::mem::take(&mut self.pending_cpuinfo_placeholders);
        if !rendered {
            return Ok(());
        }
        for fd in pending {
            if !self.file_handles.contains_key(&fd) {
                continue;
            }
            let raw_stat =
                stat::fstat(unsafe { BorrowedFd::borrow_raw(fd) }).map_err(|_| Errno::last())?;
            let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if let Some((path, snapshot)) =
                self.within_proc_level(placeholder_behind(fd, crate::procfs::RenderedFiles::ALL))
            {
                self.adopt_placeholder(fd, &raw_stat, fd_flags, path, snapshot)?;
            }
        }
        Ok(())
    }

    /// Register a blocking accept waiting on `listener` and return the
    /// operation number that removes it again.
    pub(crate) fn add_accept_wait(&mut self, listener: RawFd) -> u64 {
        let operation = self.next_accept_wait;
        self.next_accept_wait += 1;
        self.accept_waits.push((operation, listener));
        operation
    }

    /// Remove the accept wait registered as `operation`, if it is here.
    pub(crate) fn remove_accept_wait(&mut self, operation: u64) {
        self.accept_waits
            .retain(|(waiting, _)| *waiting != operation);
    }

    /// The lowest descriptor in `fds` that a blocking accept waits on.
    pub(crate) fn waited_listener_in(&self, fds: std::ops::RangeInclusive<u32>) -> Option<RawFd> {
        self.accept_waits
            .iter()
            .map(|(_, listener)| *listener)
            .filter(|listener| u32::try_from(*listener).is_ok_and(|fd| fds.contains(&fd)))
            .min()
    }

    fn allocate_open_file_id(&mut self, creator: DetTid, ty: FdType) -> OpenFileId {
        if ty == FdType::Socket {
            let id = OpenFileId::new_socket(creator, self.next_socket_open_file_sequence);
            self.next_socket_open_file_sequence += 1;
            id
        } else {
            let id = OpenFileId::new(creator, self.next_open_file_sequence);
            self.next_open_file_sequence += 1;
            id
        }
    }

    fn count_open_files_at_paths(&self, paths: &[&Path]) -> usize {
        self.file_handles
            .values()
            .filter(|fd| {
                fd.path()
                    .is_some_and(|path| paths.iter().any(|candidate| path == *candidate))
            })
            .map(DetFd::open_file_id)
            .collect::<BTreeSet<_>>()
            .len()
    }

    fn has_loopback_peer(&self) -> bool {
        self.file_handles.values().any(DetFd::is_loopback_peer)
    }

    // TODO-HUMAN-REVIEW(#2373)
    /// True when `vfork` must be refused because some open file description
    /// either holds a lock or carries a cached claim that may now be wrong.
    ///
    /// ⚠️ NEVER-OBSERVED IS DELIBERATELY NOT UNSAFE. The previous form refused
    /// on `!= Some(None)`, which also caught descriptors Detcore had simply
    /// never observed. stdin, stdout and stderr are marked unobserved at
    /// container setup because they predate Detcore, so that form refused
    /// `vfork` for EVERY guest that reached it -- measured: fds 0, 1 and 2 were
    /// the sole reason `run_dbt_virtualizes_process_identities` was refused,
    /// and that guest never calls `flock` at all. A guard that cannot pass is
    /// not conservative, it is broken.
    ///
    /// The hazard is a STALE CACHE: a copied runtime mutating a lock that this
    /// side still has a claim about. A description Detcore never observed
    /// carries no claim, so there is nothing about it that can go stale.
    ///
    /// ⚠️ AN INVALIDATED DESCRIPTION WITH NO GRANT THROUGH IT IS NOT UNSAFE
    /// EITHER. Every ordinary fork invalidates every description the process
    /// created, so refusing on invalidation alone refused `vfork` in every
    /// process that had forked once while holding any descriptor at all --
    /// measured: a pipe, a fork whose child exits, then `vfork` was refused,
    /// and that refusal stopped every DBT `make -j4` cell in run 1, although
    /// `make` never calls `flock`. An invalidated description is refused only
    /// when this process's Detcore granted a lock through it
    /// (`DetFd::flock_may_have_been_granted`), before or after the fork.
    ///
    /// The invariant is "no process holds a known mode for an admitted
    /// description", not "an admitted description holds no lock": a sibling
    /// may have locked it through its own split copy of the model, and the
    /// copied child may change that lock natively. That is Linux behaviour
    /// and no Detcore flock cache can go stale from it. The routes by which
    /// a copied child could reach a description another process DOES hold a
    /// claim about, or change an OFD lock the `/proc/locks` ledger records,
    /// are refused in the copied child itself, whatever the
    /// unsupported-syscall policy: the run ends by name under the default
    /// fail-closed policy and the call answers an errno without executing
    /// under `--allow-unsupported-syscalls` (`pidfd_getfd`, `recvmsg` and
    /// `recvmmsg`, which can import a description over `SCM_RIGHTS`, and OFD
    /// `fcntl` lock changes: `reverie_dbt_runtime_copied_syscall` in
    /// detcore-dbt). A
    /// native flock change can leave the ledger's recorded flock stale, but
    /// that record admits no `/proc/locks` row: flock rows are kept only by
    /// their taker's pid (`procfs::locks`).
    fn has_unsafe_vfork_flock_state(&self) -> bool {
        self.file_handles.values().any(|detfd| {
            matches!(detfd.known_flock_mode(), Some(Some(_)))
                || (detfd.flock_mode_may_be_stale() && detfd.flock_may_have_been_granted())
        })
    }

    fn has_unsafe_vfork_ofd_state(&self) -> bool {
        self.file_handles
            .values()
            .any(|detfd| detfd.may_hold_ofd_lock())
    }

    fn forget_flock_modes(&self) {
        for detfd in self.file_handles.values() {
            detfd.forget_local_flock_mode_at_fork();
        }
    }

    /// Another table takes a copy of this one (a fork, or a thread made
    /// without `CLONE_FILES`): every description the lock ledger does not
    /// follow yet (`DetFd::is_lock_tracked`) now has references it never saw
    /// taken (`DetFd::mark_local_untracked_references_escaped`), without
    /// contacting a promoted description's shared model, which the flock
    /// model takes as escaped (`DetFd::is_shared`). The ledger copies the
    /// references of the descriptions it follows (`LockLedger::fork`).
    fn mark_untracked_references_escaped(&self) {
        for detfd in self.file_handles.values() {
            detfd.mark_local_untracked_references_escaped();
        }
    }

    // TODO-HUMAN-REVIEW(#2373)
    /// Fork the file table for `child`.
    ///
    /// Local flock caches retain the existing conservative invalidation.
    /// A promoted description has one canonical model, so copying a table
    /// neither invalidates that model nor contacts it before child admission.
    /// Same-process aliases keep their lock; a separately hosted runtime
    /// resets copied locks at its physical child boundary.
    pub(crate) fn fork_for(&self, child: DetTid) -> Self {
        self.forget_flock_modes();
        self.mark_untracked_references_escaped();
        Self {
            files_id: FilesId::forked(child),
            next_open_file_sequence: self.next_open_file_sequence,
            next_socket_open_file_sequence: self.next_socket_open_file_sequence,
            next_random_device_sequence: self.next_random_device_sequence,
            has_promoted_shared_files: self.has_promoted_shared_files,
            has_shared_procfs_sequences: self.has_shared_procfs_sequences,
            file_handles: self.file_handles.clone(),
            accept_waits: Vec::new(),
            next_accept_wait: 0,
            cpuinfo_placeholders: self.cpuinfo_placeholders,
            pending_cpuinfo_placeholders: self.pending_cpuinfo_placeholders.clone(),
            proc_level: self.proc_level,
            present_unmodelled: self.present_unmodelled.clone(),
            unaccounted_receipts: self.unaccounted_receipts,
            descriptor_capacity: self.forked_descriptor_capacity(),
            exec_dropped_lock_tracked: false,
            released_rights: ReleasedRights::default(),
            holders: BTreeMap::from([(child, 1)]),
            exec_replaced: ReplacedTable::default(),
            mapped_open_files: self.mapped_open_files.clone(),
        }
    }

    pub(crate) fn for_exec(&self, task: DetTid) -> Self {
        Self {
            files_id: self.files_id.for_exec(task),
            next_open_file_sequence: self.next_open_file_sequence,
            next_socket_open_file_sequence: self.next_socket_open_file_sequence,
            next_random_device_sequence: self.next_random_device_sequence,
            has_promoted_shared_files: self.has_promoted_shared_files,
            has_shared_procfs_sequences: self.has_shared_procfs_sequences,
            file_handles: self
                .file_handles
                .iter()
                .filter_map(|(&fd, detfd)| (!detfd.is_cloexec()).then_some((fd, detfd.clone())))
                .collect(),
            accept_waits: Vec::new(),
            next_accept_wait: 0,
            cpuinfo_placeholders: self.cpuinfo_placeholders,
            pending_cpuinfo_placeholders: self.pending_cpuinfo_placeholders.clone(),
            proc_level: self.proc_level,
            present_unmodelled: self
                .present_unmodelled
                .iter()
                .filter(|(_, cloexec)| !**cloexec)
                .map(|(fd, cloexec)| (*fd, *cloexec))
                .collect(),
            unaccounted_receipts: self.unaccounted_receipts,
            descriptor_capacity: self.descriptor_capacity,
            exec_dropped_lock_tracked: self
                .file_handles
                .values()
                .any(|detfd| detfd.is_cloexec() && detfd.is_lock_tracked()),
            released_rights: ReleasedRights::default(),
            holders: BTreeMap::from([(task, 1)]),
            exec_replaced: ReplacedTable::default(),
            mapped_open_files: BTreeSet::new(),
        }
    }

    /// A thread of `process` made with `CLONE_FILES` holds this table too
    /// ([`Self::holders`]); a table whose holders are not known stays so.
    pub(crate) fn join_holder(&mut self, process: DetPid) {
        if !self.holders.is_empty() {
            *self.holders.entry(process).or_insert(0) += 1;
        }
    }

    /// One thread of `process` holds this table, and nothing else does
    /// ([`Self::holders`]): a copy made for it, by a `clone` without
    /// `CLONE_FILES` or by an `exec`.
    pub(crate) fn hold_alone(&mut self, process: DetPid) {
        self.holders = BTreeMap::from([(process, 1)]);
    }

    /// A thread of `process` that holds this table exits, with its whole
    /// process where `whole_process` (an `exit_group`, which ends every
    /// thread of it). Returns whether that left the table to no holder: then
    /// it goes with this exit, as Linux releases a table with its last
    /// holder's exit (`exit_files`). Never for a table whose holders are not
    /// known, nor for a process that does not hold it.
    pub(crate) fn leave(&mut self, process: DetPid, whole_process: bool) -> bool {
        let Some(count) = self.holders.get_mut(&process) else {
            return false;
        };
        if whole_process || *count <= 1 {
            self.holders.remove(&process);
        } else {
            *count -= 1;
        }
        self.holders.is_empty()
    }

    /// `process` has ended, by the default action of a fatal signal, as the
    /// terminal receipt of its death says (`GlobalState::release_tables_at_death`),
    /// and this is one of the tables its threads held. Every thread of it
    /// leaves the table at once ([`Self::leave`]); where that leaves the
    /// table to no holder, every descriptor of it goes, in order, as Linux
    /// releases a table with its last holder's exit (`exit_files`), and a
    /// socket whose last reference goes takes its queue ([`Self::release_detfd`]).
    /// Where another process still holds the table (`CLONE_FILES` without
    /// `CLONE_THREAD`), it keeps every descriptor. Where its holders are not
    /// known, or do not name `process`, nothing is decided.
    pub(crate) fn release_at_death(&mut self, process: DetPid) -> TableAtDeath {
        if !self.holders.contains_key(&process) {
            return TableAtDeath::Undecided;
        }
        if !self.leave(process, true) {
            return TableAtDeath::Kept;
        }
        self.release_every_descriptor();
        TableAtDeath::Released(self.take_released_rights())
    }

    /// Keep `replaced`, the table an exec of `process` replaces with this
    /// one, until the new image's first callback ([`Self::exec_replaced`]).
    pub(crate) fn keep_replaced_table(
        &mut self,
        replaced: Arc<Mutex<FileMetadata>>,
        process: DetPid,
    ) {
        self.exec_replaced = ReplacedTable(Some((replaced, process)));
    }

    /// Read the live table incarnation for an independently owned exec cut.
    pub fn native_exec_files_id(&self) -> FilesId {
        self.files_id
    }

    /// Validate before and after preparation: `for_exec` intentionally drops
    /// waits and CLOEXEC-only descriptions, so checking only its result would
    /// lose evidence about their skipped runtime capabilities.
    pub(crate) fn validate_native_exec_cut(
        &self,
        expected: FilesId,
        prepared: bool,
    ) -> Result<(), crate::native_exec::NativeExecCutError> {
        use crate::native_exec::NativeExecCutError;

        if self.files_id != expected {
            return Err(NativeExecCutError::Identity("files_id"));
        }
        if !self.accept_waits.is_empty() {
            return Err(NativeExecCutError::LiveCapability("accept_waits"));
        }
        if prepared && self.next_accept_wait != 0 {
            return Err(NativeExecCutError::PreparedState("next_accept_wait"));
        }
        // The native final cut repeats this check after freezing counters.
        // Compare prior descriptors directly so validation neither allocates a
        // scratch map nor changes the carried Tool heap. The state transport
        // separately bounds this first, quiescent single-thread domain.
        for (index, (&slot, fd)) in self.file_handles.iter().enumerate() {
            if slot < 0 || slot != fd.fd {
                return Err(NativeExecCutError::Descriptor("slot number"));
            }
            if prepared && fd.is_cloexec() {
                return Err(NativeExecCutError::PreparedState("CLOEXEC slot"));
            }
            let (id, owner) = fd.validate_native_exec_runtime_state()?;
            for (_, previous) in self.file_handles.iter().take(index) {
                let (previous_id, previous_owner) =
                    previous.validate_native_exec_runtime_state()?;
                if (id == previous_id) != (owner == previous_owner) {
                    return Err(NativeExecCutError::Descriptor("OFD alias identity"));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn exec_blocking_overrides(&self) -> ExecFdBlockingOverrides {
        self.file_handles
            .iter()
            .filter_map(|(&fd, detfd)| {
                (!detfd.is_cloexec() && detfd.physically_nonblocking() && !detfd.is_nonblocking())
                    .then_some(fd)
            })
            .collect()
    }

    pub(crate) fn apply_exec_blocking_overrides(
        &mut self,
        owner: DetTid,
        overrides: ExecFdBlockingOverrides,
    ) {
        for fd in overrides {
            tracing::trace!(
                "[detcore, dtid {}] restoring descriptor {} as logically blocking after exec",
                owner,
                fd,
            );
            match self.discover_fd_from_current_process(owner, fd) {
                Ok(()) => {
                    self.with_detfd(fd, |detfd| detfd.set_logical_nonblocking(false))
                        .expect("a just-discovered descriptor must remain registered");
                }
                Err(error) => {
                    tracing::warn!(
                        "[detcore, dtid {}] unable to restore inherited descriptor {} after exec: {}",
                        owner,
                        fd,
                        error,
                    );
                }
            }
        }
    }

    fn inherited_reads_for_exec(&self) -> BTreeMap<RawFd, crate::fd::InheritedExternalRead> {
        self.file_handles
            .iter()
            .filter(|(_, detfd)| !detfd.is_cloexec())
            .filter_map(|(&fd, detfd)| {
                detfd
                    .inherited_external_read()
                    .map(|terminal| (fd, terminal))
            })
            .collect()
    }

    /// Called only in the actual shared guest descriptor table, after the
    /// global state confirms this is the container's first image. TCGETS is
    /// read-only: neither terminal settings nor OFD flags are changed.
    fn discover_inherited_terminals(&mut self, descriptors: impl IntoIterator<Item = RawFd>) {
        for fd in descriptors {
            let Some(detfd) = self.file_handles.get(&fd) else {
                continue;
            };
            let Some(stat) = detfd.stat() else { continue };
            if stat.mode & libc::S_IFMT != libc::S_IFCHR {
                continue;
            }
            let mut settings: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: TCGETS writes only to this live, adequately sized buffer.
            if unsafe { libc::ioctl(fd, libc::TCGETS, &raw mut settings) } != 0 {
                continue;
            }
            detfd.set_inherited_external_read(crate::fd::InheritedExternalRead {
                origin: detfd.open_file_id(),
                dev: stat.dev,
                inode: stat.inode,
                rdev: stat.rdev,
                file_type: libc::S_IFCHR,
                access_mode: detfd.status_flags() & libc::O_ACCMODE,
                resource: detfd.resource(),
            });
        }
    }

    /// Called only in the actual shared guest descriptor table, after the
    /// global state confirms this is the container's first image, beside
    /// `discover_inherited_terminals`. The guest has not run yet, so a pipe
    /// end the first image holds came from outside the run: a readable one is
    /// an inherited external read, as a terminal or a launcher-bound input is
    /// (https://github.com/rrnewton/hermit/issues/4447), and its reads take
    /// the same external-input path theirs do. That holds even when the image
    /// also holds the pipe's write end: such a pipe was not made by Detcore's
    /// `pipe2`, so it is not physically nonblocking and cannot take the
    /// internal-pipe path, whose invariant (`record_or_replay_blocking`) a
    /// blocking read of it would otherwise trip. The decision reads only what
    /// discovery recorded for the descriptor (its type, the kernel's stat and
    /// its access mode), so no later probe can change it; a description a
    /// launcher already bound keeps its binding. Inherited descriptors above 2
    /// are not discovered here.
    fn discover_inherited_pipes(&mut self, descriptors: impl IntoIterator<Item = RawFd>) {
        for fd in descriptors {
            let Some(detfd) = self.file_handles.get(&fd) else {
                continue;
            };
            let access_mode = detfd.status_flags() & libc::O_ACCMODE;
            if detfd.ty() != FdType::Pipe
                || detfd.is_socketpair_endpoint()
                || access_mode == libc::O_WRONLY
                || detfd.inherited_external_read().is_some()
            {
                continue;
            }
            let Some(stat) = detfd.stat() else { continue };
            detfd.set_inherited_external_read(crate::fd::InheritedExternalRead {
                origin: detfd.open_file_id(),
                dev: stat.dev,
                inode: stat.inode,
                rdev: stat.rdev,
                file_type: libc::S_IFIFO,
                access_mode,
                resource: detfd.resource(),
            });
        }
    }

    /// Restore only external-read descriptions proven to survive the prepared exec. Unknown
    /// descriptors and guest-created PTYs are not promoted by rediscovery.
    fn restore_inherited_reads(
        &mut self,
        owner: DetTid,
        inherited_reads: BTreeMap<RawFd, crate::fd::InheritedExternalRead>,
    ) -> Result<(), Errno> {
        let mut aliases: HashMap<OpenFileId, DetFd> = HashMap::new();
        for (fd, terminal) in inherited_reads {
            self.discover_fd_from_current_process(owner, fd)?;
            let detfd = self.file_handles.get_mut(&fd).ok_or(Errno::EBADF)?;
            let stat =
                stat::fstat(unsafe { BorrowedFd::borrow_raw(fd) }).map_err(|_| Errno::last())?;
            if stat.st_mode & libc::S_IFMT != terminal.file_type
                || (stat.st_dev, stat.st_ino, stat.st_rdev)
                    != (terminal.dev, terminal.inode, terminal.rdev)
                || detfd.status_flags() & libc::O_ACCMODE != terminal.access_mode
            {
                return Err(Errno::EIO);
            }
            // Do not repeat TCGETS here: a hung-up terminal may return EIO
            // from that query while still being the inherited open file.
            if let Some(alias) = aliases.get(&terminal.origin) {
                if detfd.status_flags() != alias.status_flags() {
                    return Err(Errno::EIO);
                }
                detfd.share_open_file_with(alias);
            } else {
                detfd.set_resource(terminal.resource.clone());
                detfd.set_inherited_external_read(terminal.clone());
                aliases.insert(terminal.origin, detfd.clone());
            }
        }
        Ok(())
    }

    fn install_inherited_input(
        &mut self,
        owner: DetTid,
        input: reverie::InheritedInputIdentity,
    ) -> Result<(), Errno> {
        let fd = input.guest_fd();
        self.discover_fd_from_current_process(owner, fd)?;
        let stat = stat::fstat(unsafe { BorrowedFd::borrow_raw(fd) }).map_err(|_| Errno::last())?;
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let detfd = self.file_handles.get(&fd).ok_or(Errno::EBADF)?;
        if !input.matches(&stat, flags) || detfd.ty() != FdType::Pipe {
            return Err(Errno::EIO);
        }
        detfd.set_inherited_external_read(crate::fd::InheritedExternalRead {
            origin: detfd.open_file_id(),
            dev: input.dev(),
            inode: input.inode(),
            rdev: input.rdev(),
            file_type: libc::S_IFIFO,
            access_mode: input.access_mode(),
            resource: detfd.resource(),
        });
        Ok(())
    }

    pub(crate) fn apply_exec_fd_state(
        &mut self,
        owner: DetTid,
        state: ExecFdState,
        shared_guest_table: bool,
    ) -> Result<(), Errno> {
        if shared_guest_table && let Some(maps) = &state.maps {
            // The existing exec exchange authorizes these survivors. Validate
            // every slot before installing any carried maps authority, and
            // reserve its old ID space before lazy discovery allocates IDs.
            for (&fd, survivor) in &maps.handles {
                if ExecOpenFile::capture(fd, survivor.identity.id)? != survivor.identity {
                    return Err(Errno::EIO);
                }
            }
            if state
                .open_files
                .as_ref()
                .is_some_and(|files| files.creator != maps.creator)
                || state
                    .open_file_namespace
                    .is_some_and(|(creator, _)| creator != maps.creator)
            {
                return Err(Errno::EIO);
            }
            self.next_open_file_sequence = self.next_open_file_sequence.max(maps.next_sequence);
        }
        if shared_guest_table && let Some(records) = &state.procfs_records {
            // The same for the per-process files carried with their record's
            // sequence, and their table's numbering, reserved before any
            // rediscovery allocates an identity whose sequence another
            // process may still hold.
            for (&fd, survivor) in &records.handles {
                if ExecOpenFile::capture(fd, survivor.identity.id)? != survivor.identity {
                    return Err(Errno::EIO);
                }
            }
            if state
                .open_files
                .as_ref()
                .is_some_and(|files| files.creator != records.creator)
                || state
                    .open_file_namespace
                    .is_some_and(|(creator, _)| creator != records.creator)
                || state
                    .maps
                    .as_ref()
                    .is_some_and(|maps| maps.creator != records.creator)
            {
                return Err(Errno::EIO);
            }
            self.next_open_file_sequence = self.next_open_file_sequence.max(records.next_sequence);
            self.has_shared_procfs_sequences = true;
        }
        // Legacy discovery also allocates IDs. Reserve the complete old
        // namespace before it can create any nonrandom descriptions, including
        // same-leader exec where the creator did not change.
        let owner = if shared_guest_table && let Some(open_files) = &state.open_files {
            self.next_open_file_sequence =
                self.next_open_file_sequence.max(open_files.next_sequence);
            self.next_socket_open_file_sequence = self
                .next_socket_open_file_sequence
                .max(open_files.next_socket_sequence);
            open_files.creator
        } else if shared_guest_table
            && let Some((creator, next_sequence)) = state.open_file_namespace
        {
            // `--pipe-wake-latency`: no new description may take an id that a
            // carried channel names (`crate::pipe_wake`).
            self.next_open_file_sequence = self.next_open_file_sequence.max(next_sequence);
            creator
        } else if shared_guest_table && let Some(maps) = &state.maps {
            maps.creator
        } else if shared_guest_table && let Some(records) = &state.procfs_records {
            records.creator
        } else {
            owner
        };
        if shared_guest_table {
            self.next_random_device_sequence = self
                .next_random_device_sequence
                .max(state.random_device_sequence);
        }
        self.apply_exec_blocking_overrides(owner, state.blocking);
        if !shared_guest_table {
            return Ok(());
        }
        if let Some(input) = state.initial_input {
            self.install_inherited_input(owner, input)?;
        }
        if state.initialize_inherited_stdio {
            self.discover_inherited_terminals(0..=2);
            self.discover_inherited_pipes(0..=2);
        } else {
            self.restore_inherited_reads(owner, state.inherited_reads)?;
        }
        if let Some(open_files) = state.open_files {
            let mut handles = HashMap::new();
            for (fd, survivor) in open_files.handles {
                if ExecOpenFile::capture(fd, survivor.id)? != survivor {
                    return Err(Errno::EIO);
                }
                handles.insert(fd, DetFd::shared_survivor(fd, survivor.id));
            }
            crate::fd::intern_open_files(&mut handles);
            // Replace only the authenticated random-device and timerfd survivors. Pipes,
            // sockets and other descriptors retain the legacy restoration above;
            // in particular, do not reconnect an old socket's port association.
            self.file_handles.extend(handles);
            self.has_promoted_shared_files = true;
        }
        // Last, so that no restoration above can replace a description after
        // its channel is set: every carried channel end, discovering a
        // descriptor not yet rediscovered (one the guest left `O_NONBLOCK`).
        for (fd, end) in state.channels {
            match self.discover_fd_from_current_process(owner, fd) {
                Ok(()) => {
                    self.with_detfd(fd, |detfd| detfd.set_channel(end))
                        .expect("a just-discovered descriptor must remain registered");
                }
                Err(error) => {
                    tracing::warn!(
                        "[detcore, dtid {}] descriptor {} carried a pipe channel across exec \
                         but cannot be rediscovered; it keeps none: {}",
                        owner,
                        fd,
                        error,
                    );
                }
            }
        }
        if let Some(maps) = state.maps {
            let mut handles = HashMap::new();
            for (fd, survivor) in maps.handles {
                let raw_stat = stat::fstat(unsafe { BorrowedFd::borrow_raw(fd) })
                    .map_err(|error| Errno::new(error as i32))?;
                let handle = DetFd::new(
                    fd,
                    OFlag::from_bits_truncate(survivor.flags),
                    FdType::Regular,
                    survivor.identity.id,
                )
                .with_stat(DetStat::from(raw_stat));
                if let Some(path) = survivor.path {
                    handle.set_path(path);
                }
                handle.set_procfs(survivor.procfs);
                handle.mark_flock_mode_unobserved();
                handles.insert(fd, handle);
            }
            // Aliases retain one old snapshot/cursor. Independent opens of the
            // same proc inode remain separate because their IDs differ.
            crate::fd::intern_open_files(&mut handles);
            self.file_handles.extend(handles);
        }
        if let Some(records) = state.procfs_records {
            self.restore_procfs_records(records)?;
        }
        Ok(())
    }

    /// Restore the per-process files carried across exec with their open
    /// files' identities and record sequences (`ExecProcfsRecords`), already
    /// authenticated against the descriptors (`apply_exec_fd_state`): the
    /// new image's table names the same open files as every other process
    /// holding them, takes and stores a sequence a fork or a send moved to
    /// Detcore's global state, and keeps one it alone held. A rendered file's
    /// placeholder is registered again, under the open file it belongs to,
    /// as its open registered it (`rendered_procfs::register_placeholder_open`).
    fn restore_procfs_records(&mut self, records: ExecProcfsRecords) -> Result<(), Errno> {
        let mut handles = HashMap::new();
        let mut placeholders = Vec::new();
        for (fd, survivor) in records.handles {
            let raw_stat = stat::fstat(unsafe { BorrowedFd::borrow_raw(fd) })
                .map_err(|error| Errno::new(error as i32))?;
            let handle = DetFd::new(
                fd,
                OFlag::from_bits_truncate(survivor.flags),
                FdType::Regular,
                survivor.identity.id,
            )
            .with_stat(DetStat::from(raw_stat));
            if let Some(path) = survivor.path {
                handle.set_path(path);
            }
            let mut procfs = survivor.procfs;
            procfs.adopt_sequence(survivor.sequence);
            if let Some(rendered) = procfs.opened_path() {
                placeholders.push((fd, (survivor.identity.dev, survivor.identity.ino), rendered));
            }
            handle.set_procfs(procfs);
            handle.mark_flock_mode_unobserved();
            if survivor.sequence_unknown {
                handle.mark_record_sequence_unknown();
            }
            handles.insert(fd, handle);
        }
        crate::fd::intern_open_files(&mut handles);
        self.file_handles.extend(handles);
        for (fd, identity, rendered) in placeholders {
            crate::syscalls::rendered_procfs::register_placeholder(
                identity.0, identity.1, rendered,
            );
            let handle = &self.file_handles[&fd];
            crate::syscalls::rendered_procfs::register_placeholder_open(
                identity,
                handle.open_file_handle(),
                handle.open_file_id(),
                Vec::new(),
            );
        }
        Ok(())
    }

    pub(crate) fn open_files_closed_on_exec(&self, table_is_shared: bool) -> Vec<OpenFileId> {
        if table_is_shared {
            return Vec::new();
        }

        let mut open_files = HashMap::new();
        for detfd in self.file_handles.values() {
            let id = detfd.open_file_id();
            let total_aliases = detfd.open_file_alias_count();
            // A copy in flight keeps the description open, whatever holds the
            // copy now (`DetFd::is_last_reference`).
            let in_flight = detfd.copies_in_flight() > 0;
            let entry = open_files
                .entry(id)
                .or_insert((0, total_aliases, !in_flight));
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
    ///
    /// `aliases`: for each of the three, the lowest of them that is one open
    /// file description with it, where the launcher knows
    /// (`Config::standard_stream_aliases`); only `procfs::locks::LockLedger`
    /// reads the groups.
    fn setup_stdio(mut self, _pid: Pid, owner: DetTid, aliases: Option<[u8; 3]>) -> Self {
        // guest stdio can be a pipe, which make things difficult
        // hence use a dummy stat here.
        // The raw syscall, not libc's interposable fstat: this runs inside the
        // guest under in-guest LiteInst (see crate::util::raw_syscall).
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat writes only into `metadata`.
        let status = unsafe {
            crate::util::raw_syscall(libc::SYS_fstat, [0, (&raw mut metadata) as u64, 0, 0, 0, 0])
        };
        if let Err(error) = crate::util::raw_zero_result(status) {
            panic!(
                "fstat of the guest's stdin failed (errno {})",
                error.raw_os_error().unwrap_or(0)
            );
        }
        let stat: DetStat = metadata.into();
        let stdin = DetFd::new(
            0,
            OFlag::empty(),
            FdType::Regular,
            self.allocate_open_file_id(owner, FdType::Regular),
        )
        .with_stat(stat)
        .with_resource(ResourceID::Device(Device::ContainerStdin));
        let stdout = DetFd::new(
            1,
            OFlag::empty(),
            FdType::Regular,
            self.allocate_open_file_id(owner, FdType::Regular),
        )
        .with_stat(stat)
        .with_resource(ResourceID::Device(Device::ContainerStdout));
        let stderr = DetFd::new(
            2,
            OFlag::empty(),
            FdType::Regular,
            self.allocate_open_file_id(owner, FdType::Regular),
        )
        .with_stat(stat)
        .with_resource(ResourceID::Device(Device::ContainerStderr));

        // These descriptors existed before Detcore began observing the guest,
        // so they may already carry flock state that we cannot query.
        stdin.mark_flock_mode_unobserved();
        stdout.mark_flock_mode_unobserved();
        stderr.mark_flock_mode_unobserved();

        // Which of the three are one open file description, as the launcher
        // found with kcmp(KCMP_FILE) on the descriptors it handed the guest
        // (`Config::standard_stream_aliases`): it may pass one description on
        // several (`<>file >&0`, `>log 2>&1`), each of which Detcore models
        // with an identity of its own, and the lock ledger then takes the
        // group as the one description it is. Where the launcher does not
        // know, each stream is a description of its own (a named residual).
        if let Some(aliases) = aliases {
            let streams = [&stdin, &stdout, &stderr];
            for (later, earlier) in aliases.iter().enumerate() {
                let earlier = usize::from(*earlier);
                if earlier < later {
                    let group = streams[earlier]
                        .stdio_alias_group()
                        .unwrap_or_else(|| streams[earlier].open_file_id());
                    streams[earlier].set_stdio_alias_group(group);
                    streams[later].set_stdio_alias_group(group);
                }
            }
        }

        self.add_detfd(stdin);
        self.add_detfd(stdout);
        self.add_detfd(stderr);

        self
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review SaBRe on-demand inherited descriptor discovery.
    fn discover_fd_from_current_process(&mut self, owner: DetTid, fd: RawFd) -> Result<(), Errno> {
        if self.file_handles.contains_key(&fd) {
            return Ok(());
        }

        let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        let status_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if fd_flags == -1 || status_flags == -1 {
            return Err(Errno::last());
        }
        let raw_stat =
            stat::fstat(unsafe { BorrowedFd::borrow_raw(fd) }).map_err(|_| Errno::last())?;
        // An anonymous inode has no file type; its link names what it is.
        let link = (raw_stat.st_mode & libc::S_IFMT == 0)
            .then(|| own_fd_link(fd))
            .flatten();
        self.add_discovered_fd(
            owner,
            fd,
            fd_flags,
            status_flags,
            &raw_stat,
            link.as_deref(),
        )?;
        // A placeholder for a procfs file Detcore renders, which the backend
        // rediscovers after `exec`: the file again, at the position the old
        // image left and with the snapshot it took, as a backend that keeps
        // Detcore's state across `exec` has them (`syscalls::files::rendered_procfs`).
        // The snapshot is the one its name carries (`procfs::placeholder_name`).
        // Only an empty regular file with no link can be one.
        let may_be_placeholder = raw_stat.st_mode & libc::S_IFMT == libc::S_IFREG
            && raw_stat.st_nlink == 0
            && raw_stat.st_size == 0;
        // A `/proc/cpuinfo` placeholder only once the session's answer says
        // it renders the file (`decide_cpuinfo_placeholders`); before that
        // answer, one waits for it as the ordinary descriptor it looks like.
        // So does a placeholder for a task's `arch_status`, which Detcore
        // renders exactly where it renders `/proc/cpuinfo`.
        let decided = self.cpuinfo_placeholders;
        // A process's `mountinfo` or `mounts` has a placeholder only in a
        // session with a container mount table, the only one that renders it.
        let files = crate::procfs::RenderedFiles {
            cpuinfo: decided == Some(true),
            mounts: true,
            // Rendered only where the tool runs outside the guest (KVM), which
            // never rediscovers its descriptors.
            locks: false,
            address_space: false,
        };
        let found = self.within_proc_level(
            may_be_placeholder
                .then(|| placeholder_behind(fd, files))
                .flatten(),
        );
        // Before the answer, `files` differs from every rendered file only in
        // the files rendered where `/proc/cpuinfo` is: a placeholder it does
        // not find but every rendered file does is one of those.
        if found.is_none()
            && decided.is_none()
            && may_be_placeholder
            && self
                .within_proc_level(placeholder_behind(fd, crate::procfs::RenderedFiles::ALL))
                .is_some()
        {
            self.pending_cpuinfo_placeholders.push(fd);
        }
        if let Some((path, snapshot)) = found {
            if self.adopt_placeholder(fd, &raw_stat, fd_flags, path, snapshot)? {
                return Ok(());
            }
        }
        // A fixed procfs file the kernel opened (through a symbolic link),
        // which the old image read from Detcore's rendering: the file again,
        // at the position Detcore kept in the kernel
        // (`DetFd::procfs_keeps_kernel_position`), so the new image does not
        // read the host's file. procfs files report size 0.
        else if raw_stat.st_mode & libc::S_IFMT == libc::S_IFREG
            && raw_stat.st_size == 0
            && let Some(procfs) = fixed_procfs_behind(fd)
        {
            // SAFETY: lseek only reads the descriptor's position.
            let offset = unsafe {
                crate::util::raw_syscall(
                    libc::SYS_lseek,
                    [fd as u64, 0, libc::SEEK_CUR as u64, 0, 0, 0],
                )
            };
            let offset = usize::try_from(offset).unwrap_or(0);
            let path = procfs
                .rendered_path(crate::procfs::RenderedFiles::WITHOUT_CPUINFO)
                .expect("a fixed procfs file has a path");
            self.with_detfd(fd, |detfd| {
                detfd.set_path(path);
                detfd.set_procfs(procfs.clone());
                detfd.set_procfs_offset(offset);
            })?;
        }
        // A per-process file that follows its record's sequence, which the
        // kernel opened in another process and sent here over `SCM_RIGHTS`:
        // where that sequence is and the record it buffered belong to the open
        // file, which every holder shares and the sender's Detcore keeps,
        // and which this one cannot know, so every read and seek through it
        // is refused by name (`Detcore::refuse_unknown_record_sequence`; on
        // DBT, through its runtime's exit path; on SaBRe, where a refusal that
        // ended the run would strand it, with EOPNOTSUPP), so no read reaches
        // the kernel's file. Before, it read the kernel's file, where the
        // sender's captures had left it (end of file), unsanitized (Claude's
        // narrow check of the kvm lane's slice N3, round 5; on SaBRe until its
        // final check, which saw the host's own statm). One the exec that
        // made this image kept is carried with its sequence instead
        // (`ExecProcfsRecords`).
        else if let Some(path) =
            own_fd_link(fd).and_then(|link| kernel_record_file_link(&raw_stat, &link))
        {
            self.with_detfd(fd, |detfd| {
                detfd.set_path(path.clone());
                detfd.mark_record_sequence_unknown();
            })?;
        }
        if let Some(resource) = stdio_resource(fd) {
            self.with_detfd(fd, |detfd| detfd.set_resource(resource.clone()))?;
        }
        Ok(())
    }

    /// Record descriptor `fd`, whose creation Detcore did not see, from the
    /// kernel's facts about it: its descriptor flags `fd_flags` (`F_GETFD`),
    /// status flags `status_flags` (`F_GETFL`) and `raw_stat` (`fstat`). Its
    /// type is the file's (pipe, socket, random device or regular), or, for
    /// an anonymous inode whose descriptor link (`link`) names an inotify
    /// instance, inotify; its
    /// flags are the kernel's (an observed `O_NONBLOCK` stays guest-visible:
    /// the provenance is unknown), and its lock state is unobserved. Shared by
    /// the in-guest discovery ([`Self::discover_fd_from_current_process`])
    /// and by the record of a descriptor received over `SCM_RIGHTS`
    /// (`Detcore::record_received_descriptors`), so both make the same
    /// description from the same facts.
    fn add_discovered_fd(
        &mut self,
        owner: DetTid,
        fd: RawFd,
        fd_flags: libc::c_int,
        status_flags: libc::c_int,
        raw_stat: &libc::stat,
        link: Option<&[u8]>,
    ) -> Result<(), Errno> {
        let file_type = stat::SFlag::from_bits_truncate(raw_stat.st_mode);
        let ty = if raw_stat.st_mode & libc::S_IFMT == 0
            && link == Some(crate::syscalls::inotify::INOTIFY_LINK)
        {
            // Its reads renumber rename cookies (`crate::syscalls::inotify`).
            FdType::Inotify
        } else if file_type.contains(stat::SFlag::S_IFIFO) {
            FdType::Pipe
        } else if file_type.contains(stat::SFlag::S_IFSOCK) {
            FdType::Socket
        } else if status_flags & libc::O_PATH == 0
            && crate::fd::is_random_device(raw_stat.st_mode, raw_stat.st_rdev)
        {
            // Linux's random devices by device number, as an open classifies
            // them (`Detcore::handle_openat`).
            FdType::Rng
        } else {
            FdType::Regular
        };
        let mut flags = OFlag::from_bits_truncate(status_flags);
        let physically_nonblocking = flags.contains(OFlag::O_NONBLOCK);
        // Discovered descriptors have unknown provenance, so an observed
        // O_NONBLOCK bit must remain guest-visible. Detcore-created scheduler
        // pipes are registered when created and do not reach this fallback.
        if fd_flags & libc::FD_CLOEXEC != 0 {
            flags.insert(OFlag::O_CLOEXEC);
        }
        self.add_fd(owner, fd, flags, ty, Some((*raw_stat).into()))?;
        // This newly discovered description predates our observation. Its
        // kernel lock may be held, but no cached lock claim existed to go stale.
        self.with_detfd(fd, |detfd| detfd.mark_flock_mode_unobserved())?;
        if ty == FdType::Rng {
            let stream = crate::fd::RandomDeviceStream::discovered(owner, fd);
            self.with_detfd(fd, |detfd| detfd.set_random_device(stream))?;
        }
        if ty == FdType::Pipe && physically_nonblocking {
            self.with_detfd(fd, |detfd| detfd.set_physically_nonblocking())?;
        }
        Ok(())
    }

    /// Make descriptor `fd`, already in this table, the placeholder for the
    /// rendered file `path` that its memfd (`raw_stat`, descriptor flags
    /// `fd_flags`) is, with the snapshot its name carries. Returns whether it
    /// joined another descriptor's open file (an alias), whose state it then
    /// shares.
    ///
    /// Otherwise the open file is rebuilt. A per-process file that follows
    /// its record's sequence (`ProcfsFile::follows_record_sequence`) needs
    /// more than its placeholder keeps: where its sequence is and the record
    /// it buffered, which Linux keeps in the one open file every holder
    /// shares. Where this process's registry names the open file the
    /// placeholder belongs to (this process, or an ancestor before a fork,
    /// opened it: `rendered_procfs::placeholder_open_id`), the descriptor
    /// takes that open file's identity, and its sequence from Detcore's global
    /// state, where every holder keeps it once the open file has left a
    /// process (`ThreadState::share_procfs_sequences` at a fork,
    /// `Detcore::share_sent_record_sequences` at a send). Otherwise (a
    /// descriptor another process sent to one that never held the open file)
    /// the sequence cannot be known, and every read and seek through it is
    /// refused by name (`DetFd::mark_record_sequence_unknown`), on DBT through
    /// its runtime's exit path, and on SaBRe, where a refusal that ended the
    /// run would strand it, with EOPNOTSUPP. An open file
    /// surviving `exec` is carried with its sequence instead
    /// (`ExecProcfsRecords`), before this rediscovery could reach it.
    fn adopt_placeholder(
        &mut self,
        fd: RawFd,
        raw_stat: &libc::stat,
        fd_flags: libc::c_int,
        path: &'static std::path::Path,
        snapshot: Vec<u8>,
    ) -> Result<bool, Errno> {
        crate::syscalls::rendered_procfs::register_placeholder(
            raw_stat.st_dev,
            raw_stat.st_ino,
            path,
        );
        // Every open makes its own placeholder, so another descriptor on
        // the same one is the same open file: one cursor, as before.
        if let Some(alias) = self.rendered_alias(fd, raw_stat.st_dev, raw_stat.st_ino) {
            let mut alias = alias.with_fd(fd);
            alias.set_cloexec(fd_flags & libc::FD_CLOEXEC != 0);
            self.add_detfd(alias);
            return Ok(true);
        }
        // SAFETY: lseek only reads the descriptor's position.
        let offset = unsafe {
            crate::util::raw_syscall(
                libc::SYS_lseek,
                [fd as u64, 0, libc::SEEK_CUR as u64, 0, 0, 0],
            )
        };
        let offset = usize::try_from(offset).unwrap_or(0);
        let procfs = crate::procfs::ProcfsFile::from_rendered_path(path)
            .expect("a rendered path is a procfs file")
            .into_rendered();
        let mut procfs = if snapshot.is_empty() {
            procfs
        } else {
            procfs.with_rendered_snapshot(snapshot)
        };
        let follows = procfs.follows_record_sequence();
        let joined = follows
            .then(|| {
                crate::syscalls::rendered_procfs::placeholder_open_id((
                    raw_stat.st_dev,
                    raw_stat.st_ino,
                ))
            })
            .flatten();
        if joined.is_some() {
            procfs.share_sequence();
            self.has_shared_procfs_sequences = true;
        }
        self.with_detfd(fd, |detfd| {
            if let Some(id) = joined {
                detfd.rebind_open_file_id(id);
            }
            detfd.set_path(path);
            detfd.set_procfs(procfs.clone());
            detfd.set_procfs_offset(offset);
            if follows && joined.is_none() {
                detfd.mark_record_sequence_unknown();
            }
        })?;
        Ok(false)
    }

    /// Another descriptor of this process open on the placeholder `(dev,
    /// ino)`, which `fd` is too.
    fn rendered_alias(&self, fd: RawFd, dev: u64, ino: u64) -> Option<DetFd> {
        self.file_handles
            .iter()
            .filter(|(other, detfd)| **other != fd && detfd.procfs_rendered_path().is_some())
            .find(|(other, _)| {
                stat::fstat(unsafe { BorrowedFd::borrow_raw(**other) })
                    .is_ok_and(|stat| stat.st_dev == dev && stat.st_ino == ino)
            })
            .map(|(_, detfd)| detfd.clone())
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
        self.note_installed(fd);
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
        let id = self.allocate_open_file_id(creator, ty);
        let detfd = DetFd::new(fd, flags, ty, id).with_stat(stat);
        self.add_detfd(detfd);
        Ok(())
    }

    /// Make `fd`, which `creator` just opened, a random device that reads a
    /// stream named by the opener and the table's count of random-device
    /// opens (`crate::fd::RandomDeviceStream`).
    fn classify_random_device(&mut self, creator: DetTid, fd: RawFd) -> Result<(), Errno> {
        let stream = crate::fd::RandomDeviceStream {
            creator,
            sequence: self.next_random_device_sequence,
        };
        self.with_detfd(fd, |detfd| detfd.set_random_device(stream))?;
        self.next_random_device_sequence += 1;
        Ok(())
    }

    /// The access trace's note of descriptor `fd` changing by `op`: the
    /// open file it names now, and for an open, the path it was opened by.
    fn slot_note(&self, fd: RawFd, op: FdNoteOp) -> crate::access_trace::FdNote {
        use detcore_model::access_trace::FdOp;
        let detfd = self.file_handles.get(&fd);
        let op = match op {
            FdNoteOp::Open => FdOp::Open {
                path: detfd
                    .and_then(DetFd::path)
                    .map(|path| path.to_string_lossy().into_owned()),
                kind: detfd
                    .and_then(DetFd::stat)
                    .and_then(|stat| detcore_model::access_trace::FileKind::of_mode(stat.mode)),
            },
            FdNoteOp::Dup(source) => FdOp::Dup { source },
        };
        crate::access_trace::FdNote {
            slot: detcore_model::fd::FdSlot {
                files: self.files_id,
                fd,
            },
            open_file: detfd.map(DetFd::open_file_id),
            op,
            released: None,
        }
    }

    /// The access trace's note that `fd` is about to be released, with what
    /// it named; `None` when it names nothing Detcore models.
    fn close_note(&self, fd: RawFd) -> Option<crate::access_trace::FdNote> {
        let detfd = self.file_handles.get(&fd)?;
        Some(crate::access_trace::FdNote {
            slot: detcore_model::fd::FdSlot {
                files: self.files_id,
                fd,
            },
            open_file: Some(detfd.open_file_id()),
            op: detcore_model::access_trace::FdOp::Close,
            released: Some(crate::access_trace::FdInfo::of(detfd)),
        })
    }

    /// remove a rawfd
    fn remove_fd(&mut self, fd: RawFd) -> Option<OpenFileId> {
        self.present_unmodelled.remove(&fd);
        let detfd = self.file_handles.remove(&fd)?;
        self.release_detfd(detfd)
    }

    /// `detfd`, gone from this table: the description it names, if this was
    /// its last reference ([`DetFd::is_last_reference`]: no other modeled
    /// descriptor, no copy in flight), for the caller to release its port. A
    /// socket's queued `SCM_RIGHTS` messages go with its last reference, as
    /// Linux purges a released socket's receive queue (`unix_release_sock`),
    /// and wait in [`Self::released_rights`] for the call to release what
    /// they held (`Detcore::settle_released_rights`). A description with a
    /// copy in flight stays, port and queue included, until a guest call
    /// receives or releases the copy.
    fn release_detfd(&mut self, detfd: DetFd) -> Option<OpenFileId> {
        detfd.is_last_reference().then(|| {
            self.released_rights
                .0
                .extend(detfd.take_all_queued_rights());
            detfd.open_file_id()
        })
    }

    /// Let go of every descriptor of this table, in order, as its last
    /// holder's exit does (`ThreadState::release_table_at_exit`), or the
    /// exec that replaced it (`ThreadState::release_exec_dropped_descriptors`):
    /// a socket whose last reference goes takes its queue
    /// ([`Self::release_detfd`]). No port is released here.
    fn release_every_descriptor(&mut self) {
        let mut fds: Vec<RawFd> = self.file_handles.keys().copied().collect();
        fds.sort_unstable();
        for fd in fds {
            let _ = self.remove_fd(fd);
        }
    }

    /// What closes in this table took from receiving sockets' queues
    /// ([`Self::release_detfd`]).
    fn take_released_rights(&mut self) -> Vec<SentDescriptor> {
        std::mem::take(&mut self.released_rights.0)
    }

    /// Remove every modeled descriptor in an inclusive close_range interval.
    fn remove_fd_range(&mut self, first: u32, last: u32) -> Vec<OpenFileId> {
        self.present_unmodelled
            .retain(|fd, _| !(*fd >= 0 && first <= *fd as u32 && *fd as u32 <= last));
        let mut descriptors: Vec<_> = self
            .file_handles
            .keys()
            .copied()
            .filter(|fd| *fd >= 0 && first <= *fd as u32 && *fd as u32 <= last)
            .collect();
        descriptors.sort_unstable();
        descriptors
            .into_iter()
            .filter_map(|fd| self.remove_fd(fd))
            .collect()
    }

    /// dup raw fds.
    fn dup_fd(
        &mut self,
        oldfd: RawFd,
        newfd: RawFd,
        flags: OFlag,
    ) -> Result<Option<OpenFileId>, Errno> {
        if oldfd == newfd {
            if self.present_unmodelled.contains_key(&oldfd) {
                return Ok(None);
            }
            self.with_detfd(oldfd, |_| ())?;
            return Ok(None);
        }
        // A copy of a descriptor Detcore keeps no description of is another
        // such descriptor, which the kernel has installed.
        if !self.file_handles.contains_key(&oldfd) && self.present_unmodelled.contains_key(&oldfd) {
            self.note_installed(newfd);
            self.present_unmodelled
                .insert(newfd, flags.contains(OFlag::O_CLOEXEC));
            let replaced = self.file_handles.remove(&newfd);
            return Ok(replaced.and_then(|detfd| self.release_detfd(detfd)));
        }

        let detfd = self.with_detfd(oldfd, |old_detfd| {
            old_detfd.clone().with_fd(newfd).with_fd_flags(flags)
        })?;
        self.note_installed(newfd);
        let replaced = self.file_handles.insert(newfd, detfd);
        Ok(replaced.and_then(|detfd| self.release_detfd(detfd)))
    }

    /// Capture the exact open-file description currently named by `fd`.
    fn capture_fd(&mut self, fd: RawFd) -> Result<CapturedDetFd, Errno> {
        let detfd = self.with_detfd(fd, |detfd| detfd.clone())?;
        Ok(CapturedDetFd {
            files_id: self.files_id,
            detfd,
        })
    }

    /// Install a previously captured open-file description as `newfd`.
    ///
    /// Unlike [`Self::dup_fd`], this never resolves the source fd number again.
    /// It succeeds only if the destination still belongs to the descriptor
    /// table in which the source was captured.
    fn install_captured_fd(
        &mut self,
        captured: CapturedDetFd,
        newfd: RawFd,
        flags: OFlag,
    ) -> Result<Option<OpenFileId>, CapturedDetFdInstallError> {
        if captured.files_id != self.files_id {
            return Err(CapturedDetFdInstallError {
                expected_files_id: captured.files_id,
                actual_files_id: self.files_id,
                returned_fd: newfd,
                captured,
            });
        }

        let detfd = captured.detfd.with_fd(newfd).with_fd_flags(flags);
        self.note_installed(newfd);
        let replaced = self.file_handles.insert(newfd, detfd);
        Ok(replaced.and_then(|detfd| self.release_detfd(detfd)))
    }

    /// Drop a capture that was not installed and report whether it retained the
    /// final reference to the open-file description.
    fn abandon_captured_fd(&mut self, captured: CapturedDetFd) -> Option<OpenFileId> {
        self.release_detfd(captured.detfd)
    }
}

/// How a descriptor slot changed, for [`FileMetadata::slot_note`].
#[derive(Clone, Copy)]
enum FdNoteOp {
    Open,
    Dup(RawFd),
}

fn stdio_resource(fd: RawFd) -> Option<ResourceID> {
    match fd {
        0 => Some(ResourceID::Device(Device::ContainerStdin)),
        1 => Some(ResourceID::Device(Device::ContainerStdout)),
        2 => Some(ResourceID::Device(Device::ContainerStderr)),
        _ => None,
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
        assert_eq!(timers.create(None, None, None, None), 0);
        assert_eq!(timers.create(Some(libc::SIGALRM), None, None, None), 1);
        assert_eq!(timers.create(None, None, None, None), 2);
    }

    #[test]
    fn settime_reports_previous_arming_and_remaining_uses_virtual_clock() {
        let mut timers = PosixTimers::default();
        let id = timers.create(None, None, None, None);

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
        let id = timers.create(None, None, None, None);
        timers.settime(id, 0, Some(t(100)), t(0));
        // Re-arm at t=30 (70ns remained) with a periodic 50ns timer.
        let old = timers
            .settime(id, 50, Some(t(200)), t(30))
            .expect("known id");
        assert_eq!(old, (70, 0));
        assert_eq!(timers.gettime(id, t(30)), Some((170, 50)));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    #[test]
    fn periodic_remaining_advances_past_each_deadline() {
        let mut timers = PosixTimers::default();
        let id = timers.create(Some(libc::SIGALRM), None, None, None);
        timers.settime(id, 50, Some(t(100)), t(0));

        assert_eq!(timers.gettime(id, t(100)), Some((50, 50)));
        assert_eq!(timers.gettime(id, t(125)), Some((25, 50)));
        assert_eq!(timers.gettime(id, t(150)), Some((50, 50)));
        assert_eq!(timers.signal(id), Some(Some(libc::SIGALRM)));
    }

    #[test]
    fn disarm_and_unknown_ids() {
        let mut timers = PosixTimers::default();
        let id = timers.create(None, None, None, None);
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
        let id = timers.create(None, None, None, None);
        assert!(timers.contains(id));
        assert!(timers.remove(id));
        assert!(!timers.contains(id));
        // Deleting again fails.
        assert!(!timers.remove(id));
    }
}

#[cfg(test)]
mod resource_limits_tests {
    use super::*;

    #[test]
    fn defaults_are_fixed_and_cover_linux_resources() {
        let limits = ResourceLimits::default();
        assert_eq!(
            limits.get(reverie::syscalls::linux_abi::RLIMIT_STACK),
            Some(ResourceLimit {
                current: 8 * 1024 * 1024,
                maximum: libc::RLIM64_INFINITY,
            })
        );
        assert_eq!(
            limits.get(reverie::syscalls::linux_abi::RLIMIT_NOFILE),
            Some(ResourceLimit {
                current: 1_048_576,
                maximum: 1_048_576,
            })
        );
        assert_eq!(
            limits.get(reverie::syscalls::linux_abi::RLIMIT_CORE),
            Some(ResourceLimit {
                current: libc::RLIM64_INFINITY,
                maximum: libc::RLIM64_INFINITY,
            })
        );
        assert_eq!(
            limits.get(reverie::syscalls::linux_abi::RLIMIT_RTTIME + 1),
            None
        );
    }

    #[test]
    fn cloned_process_state_changes_independently() {
        let parent = ResourceLimits::default();
        let mut child = parent.clone();
        let lowered = ResourceLimit {
            current: 1024,
            maximum: 1_048_576,
        };
        child.set(reverie::syscalls::linux_abi::RLIMIT_NOFILE, lowered);

        assert_eq!(
            child.get(reverie::syscalls::linux_abi::RLIMIT_NOFILE),
            Some(lowered)
        );
        assert_eq!(
            parent.get(reverie::syscalls::linux_abi::RLIMIT_NOFILE),
            Some(ResourceLimit {
                current: 1_048_576,
                maximum: 1_048_576,
            })
        );
    }
}

#[cfg(test)]
mod file_metadata_tests {
    mod inherited_input;
    use std::os::fd::AsRawFd;

    use super::*;
    use crate::pipe_wake::ChannelEnd;
    use crate::pipe_wake::ChannelId;

    /// Run the test named `case` in an isolated child process, returning
    /// true when this invocation is the parent and the caller must return
    /// at once. Tests here key model tables by this process's real
    /// descriptor numbers; under plain `cargo test` a sibling thread can
    /// be handed a number this test is using as a model-only alias, or
    /// reuse a number this test just closed, so the model observes a
    /// descriptor that is not its own. A child process has no sibling
    /// threads allocating descriptors.
    fn isolated_child(case: &str) -> bool {
        const CHILD: &str = "HERMIT_FILE_METADATA_CHILD";
        if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new(case)) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", case, "--nocapture", "--test-threads=1"])
            .env(CHILD, case)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{output:?}"
        );
        true
    }

    #[test]
    fn maps_exec_survivors_keep_opened_target_aliases_and_cursor() {
        use crate::procfs::BoundTask;
        use crate::procfs::MapsTarget;
        use crate::procfs::ProcfsFile;
        use crate::procfs::ProcfsSnapshotContext;
        use crate::procfs::TaskIncarnation;

        let owner = DetTid::from_raw(9021);
        let config = Config::default();
        let mut parent = ThreadState::new(owner, &config, ());
        let maps = std::fs::File::open("/proc/self/maps").unwrap();
        let alias = maps.try_clone().unwrap();
        let closed = maps.try_clone().unwrap();
        let unread = std::fs::File::open("/proc/self/maps").unwrap();
        let fd = maps.as_raw_fd();
        let alias_fd = alias.as_raw_fd();
        let closed_fd = closed.as_raw_fd();
        let unread_fd = unread.as_raw_fd();
        let target = MapsTarget {
            task: BoundTask {
                process: 9021,
                thread: None,
            },
            incarnation: TaskIncarnation {
                process: 1,
                thread: None,
            },
            mm: MmId::initial(owner),
            private: reverie::RuntimePrivateMemorySnapshot {
                owner: [1, 2, 3, 4],
                version: 7,
                regions: vec![reverie::RuntimePrivateMemoryRegion {
                    start: 0x1000,
                    end: 0x2000,
                }],
            },
        };
        for number in [fd, unread_fd] {
            parent
                .add_fd(
                    number,
                    OFlag::O_RDONLY | OFlag::O_NONBLOCK,
                    FdType::Regular,
                    None,
                )
                .unwrap();
            let mut procfs = ProcfsFile::from_path(Path::new("/proc/self/maps")).unwrap();
            procfs.bind_maps_target(target.clone());
            parent
                .with_detfd(number, |handle| {
                    handle.set_path("/proc/self/maps");
                    handle.set_procfs(procfs.clone());
                })
                .unwrap();
        }
        parent.dup_fd(fd, alias_fd, OFlag::empty()).unwrap();
        parent.dup_fd(fd, closed_fd, OFlag::O_CLOEXEC).unwrap();
        parent
            .with_detfd(fd, |handle| {
                handle.initialize_procfs(
                    b"3000-4000 rw-p 00000000 00:00 0 \n".to_vec(),
                    ProcfsSnapshotContext::default(),
                );
                handle.commit_procfs_read(0, 7);
            })
            .unwrap();
        assert!(
            parent.maps_for_exec(None).unwrap().is_none(),
            "exec maps carry requires an advertised private-memory provider"
        );
        let transfer = parent
            .maps_for_exec(Some(&reverie::RuntimePrivateMemory::Unavailable))
            .unwrap()
            .unwrap();
        let ready_transfer = parent
            .maps_for_exec(Some(&reverie::RuntimePrivateMemory::Ready(
                target.private.clone(),
            )))
            .unwrap()
            .unwrap();
        assert!(ready_transfer == transfer);
        let original_preview = parent
            .with_detfd(fd, |h| h.preview_procfs(3))
            .unwrap()
            .unwrap();
        assert!(!transfer.handles.contains_key(&closed_fd));
        assert_eq!(
            transfer.handles[&fd].identity.id,
            transfer.handles[&alias_fd].identity.id
        );
        assert_ne!(
            transfer.handles[&fd].identity.id,
            transfer.handles[&unread_fd].identity.id
        );
        let old_id = transfer.handles[&fd].identity.id;
        let old_unread_id = transfer.handles[&unread_fd].identity.id;
        let next_sequence = transfer.next_sequence;
        let state = ExecFdState {
            maps: Some(transfer),
            ..ExecFdState::default()
        };
        assert!(!state.is_empty());
        // Exercise the actual serialized exec preparation/completion payload.
        let state: ExecFdState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        drop(state.clone()); // cancelled preparation leaves the original model alone
        assert_eq!(
            parent
                .with_detfd(fd, |h| h.procfs_position())
                .unwrap()
                .unwrap()
                .0,
            7
        );
        let receiving_tid = DetTid::from_raw(9022);
        let mut successor_thread = ThreadState::new(receiving_tid, &config, ());
        // The same creator choice used by the real exec-completion handler.
        assert_eq!(state.open_file_creator(), Some(owner));
        successor_thread.set_open_file_creator(state.open_file_creator().unwrap());
        let mut successor = successor_thread.metadata();
        successor
            .apply_exec_fd_state(receiving_tid, state.clone(), true)
            .unwrap();
        assert!(
            successor.file_handles.contains_key(&fd),
            "exec must retain maps provenance before lazy discovery"
        );
        assert!(!successor.file_handles.contains_key(&closed_fd));
        for number in [fd, alias_fd, unread_fd] {
            assert_eq!(
                successor.file_handles[&number].procfs_maps_target(),
                Some(target.clone())
            );
            assert!(successor.file_handles[&number].is_nonblocking());
        }
        assert_eq!(successor.file_handles[&fd].open_file_id(), old_id);
        assert_eq!(
            successor.file_handles[&unread_fd].open_file_id(),
            old_unread_id
        );
        assert_eq!(successor.file_handles[&fd].procfs_position().unwrap().0, 7);
        assert!(successor.file_handles[&unread_fd].procfs_needs_snapshot());
        let (offset, bytes) = successor.file_handles[&alias_fd].preview_procfs(3).unwrap();
        assert_eq!((offset, bytes.clone()), original_preview);
        successor.file_handles[&alias_fd].commit_procfs_read(offset, bytes.len());
        assert_eq!(successor.file_handles[&fd].procfs_position().unwrap().0, 10);
        assert_eq!(
            parent
                .with_detfd(fd, |h| h.procfs_position())
                .unwrap()
                .unwrap()
                .0,
            7
        );
        assert!(successor.next_open_file_sequence >= parent.metadata().next_open_file_sequence);
        drop(successor);
        let next = tempfile::tempfile().unwrap();
        successor_thread
            .add_fd(next.as_raw_fd(), OFlag::O_RDWR, FdType::Regular, None)
            .unwrap();
        assert_eq!(
            successor_thread
                .with_detfd(next.as_raw_fd(), |h| h.open_file_id())
                .unwrap(),
            OpenFileId::new(owner, next_sequence),
        );
        // A reused numeric slot cannot acquire the old target's authority.
        let replacement = tempfile::tempfile().unwrap();
        assert_eq!(
            unsafe { libc::dup2(replacement.as_raw_fd(), alias_fd) },
            alias_fd
        );
        assert_eq!(
            FileMetadata::new(owner).apply_exec_fd_state(owner, state, true),
            Err(Errno::EIO)
        );
    }

    #[test]
    fn shared_exec_survivors_keep_aliases_and_allocator_after_closed_high_ids() {
        crate::fd::install_test_shared_open_file_channel();
        let creator = DetTid::from_raw(9011);
        let leader = DetTid::from_raw(9012);
        let config = Config::default();
        let mut parent = ThreadState::new(creator, &config, ());
        parent.set_open_file_creator(creator);
        parent.metadata().file_handles.clear();
        let file = std::fs::File::open("/dev/urandom").unwrap();
        let alias = nix::unistd::dup(&file).unwrap();
        let distinct = std::fs::File::open("/dev/urandom").unwrap();
        let cloexec = nix::unistd::dup(&file).unwrap();
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (fd, alias_fd, distinct_fd, closed_fd, socket_fd) = (
            file.as_raw_fd(),
            alias.as_raw_fd(),
            distinct.as_raw_fd(),
            cloexec.as_raw_fd(),
            socket.as_raw_fd(),
        );
        for fd in [fd, alias_fd, distinct_fd, socket_fd] {
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
        }
        parent
            .add_fd(fd, OFlag::O_RDONLY, FdType::Rng, None)
            .unwrap();
        parent.classify_random_device(fd).unwrap();
        parent.dup_fd(fd, alias_fd, OFlag::empty()).unwrap();
        parent.dup_fd(fd, closed_fd, OFlag::O_CLOEXEC).unwrap();
        parent
            .add_fd(distinct_fd, OFlag::O_RDONLY, FdType::Rng, None)
            .unwrap();
        parent.classify_random_device(distinct_fd).unwrap();
        parent
            .add_fd(socket_fd, OFlag::O_RDWR, FdType::Socket, None)
            .unwrap();
        let first_id = parent.metadata().file_handles[&fd].open_file_id();
        let distinct_id = parent.metadata().file_handles[&distinct_fd].open_file_id();
        let old_socket_id = parent.metadata().file_handles[&socket_fd].open_file_id();
        assert_ne!(
            first_id, distinct_id,
            "same inode does not identify one OFD"
        );
        parent.metadata().file_handles[&fd]
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 0);
                Ok(7)
            })
            .unwrap();
        // Closed and CLOEXEC descriptions still consume allocator identities.
        let high = parent
            .metadata()
            .allocate_open_file_id(creator, FdType::Rng);
        let high_socket = parent
            .metadata()
            .allocate_open_file_id(creator, FdType::Socket);
        parent.promote_shared_open_files().unwrap();
        assert!(!parent.metadata().file_handles[&socket_fd].is_shared());
        let transfer = parent.shared_open_files_for_exec().unwrap().unwrap();
        assert_eq!(transfer.creator, creator);
        assert!(!transfer.handles.contains_key(&closed_fd));
        assert!(!transfer.handles.contains_key(&socket_fd));
        assert_eq!(transfer.handles[&fd].id, transfer.handles[&alias_fd].id);
        // Dropping a cancelled preparation cannot change any cursor.
        drop(transfer.clone());
        parent.metadata().file_handles[&fd]
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 7);
                Ok(0)
            })
            .unwrap();

        // A genuinely replaced descriptor must not acquire a survivor's ID.
        // This owns only a duplicate, never one of the test process's stdio FDs.
        let replacement = tempfile::tempfile().unwrap();
        assert_eq!(
            unsafe { libc::dup2(replacement.as_raw_fd(), alias_fd) },
            alias_fd
        );
        assert_eq!(
            FileMetadata::new(leader).apply_exec_fd_state(
                leader,
                ExecFdState {
                    open_files: Some(transfer.clone()),
                    ..ExecFdState::default()
                },
                true,
            ),
            Err(Errno::EIO),
        );
        assert_eq!(unsafe { libc::dup2(fd, alias_fd) }, alias_fd);

        let mut successor = ThreadState::new(leader, &config, ());
        // Preserve the new image's existing nonrandom association. The handover
        // must not restore the old socket ID used by ReleasePort bookkeeping.
        successor
            .add_fd(socket_fd, OFlag::O_RDWR, FdType::Socket, None)
            .unwrap();
        let new_socket_id = successor.metadata().file_handles[&socket_fd].open_file_id();
        assert_ne!(old_socket_id, new_socket_id);
        let stdio_ids: Vec<_> = (0..=2)
            .map(|fd| successor.metadata().file_handles[&fd].open_file_id())
            .collect();
        successor.set_open_file_creator(transfer.creator);
        successor
            .metadata()
            .apply_exec_fd_state(
                creator,
                ExecFdState {
                    open_files: Some(transfer),
                    random_device_sequence: parent.random_device_sequence_for_exec(),
                    ..ExecFdState::default()
                },
                true,
            )
            .unwrap();
        assert!(!successor.metadata().file_handles.contains_key(&closed_fd));
        assert_eq!(
            successor.metadata().file_handles[&socket_fd].open_file_id(),
            new_socket_id
        );
        assert!(!successor.metadata().file_handles[&socket_fd].is_shared());
        assert_eq!(
            (0..=2)
                .map(|fd| successor.metadata().file_handles[&fd].open_file_id())
                .collect::<Vec<_>>(),
            stdio_ids
        );
        successor.metadata().file_handles[&alias_fd]
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 7);
                Ok(5)
            })
            .unwrap();
        parent.metadata().file_handles[&fd]
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 12);
                Ok(0)
            })
            .unwrap();
        successor.metadata().file_handles[&distinct_fd]
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 0);
                Ok(0)
            })
            .unwrap();
        successor.remove_fd(fd);
        successor
            .add_fd(fd, OFlag::O_RDONLY, FdType::Rng, None)
            .unwrap();
        successor.classify_random_device(fd).unwrap();
        let reopened = successor.metadata().file_handles[&fd].open_file_id();
        assert_ne!(reopened, first_id);
        assert_ne!(reopened, high);
        assert_eq!(
            reopened,
            OpenFileId::new(creator, successor.metadata().next_open_file_sequence - 1)
        );
        // A survivor keeps its stream, and the exec exchange carries the count
        // of random-device opens, so the new image's first open reads a new one.
        let stream_of = |fd| successor.metadata().file_handles[&fd].random_device_stream();
        let named = |sequence| crate::fd::RandomDeviceStream { creator, sequence };
        assert_eq!(stream_of(alias_fd), named(0));
        assert_eq!(stream_of(distinct_fd), named(1));
        assert_eq!(stream_of(fd), named(2));
        assert_ne!(
            successor
                .metadata()
                .allocate_open_file_id(creator, FdType::Socket),
            high_socket
        );
        successor.promote_shared_open_files().unwrap();
        let second = successor.shared_open_files_for_exec().unwrap().unwrap();
        assert_eq!(
            second.creator, creator,
            "nonleader creator survives repeated exec"
        );
        let mut again = FileMetadata::new(leader);
        again
            .apply_exec_fd_state(
                creator,
                ExecFdState {
                    open_files: Some(second),
                    ..ExecFdState::default()
                },
                true,
            )
            .unwrap();
        assert_eq!(again.file_handles[&fd].open_file_id(), reopened);
        again.file_handles[&alias_fd]
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 12);
                Ok(0)
            })
            .unwrap();

        // No surviving RNG descriptor does not mean its old identities are free.
        for fd in [fd, alias_fd, distinct_fd] {
            successor.remove_fd(fd);
        }
        let empty = successor.shared_open_files_for_exec().unwrap().unwrap();
        assert!(empty.handles.is_empty());
        let next_sequence = empty.next_sequence;
        let mut after_close = ThreadState::new(leader, &config, ());
        after_close.set_open_file_creator(empty.creator);
        after_close
            .metadata()
            .apply_exec_fd_state(
                creator,
                ExecFdState {
                    open_files: Some(empty),
                    ..ExecFdState::default()
                },
                true,
            )
            .unwrap();
        after_close
            .add_fd(fd, OFlag::O_RDONLY, FdType::Rng, None)
            .unwrap();
        assert_eq!(
            after_close.metadata().file_handles[&fd].open_file_id(),
            OpenFileId::new(creator, next_sequence)
        );
        // The canonical store still owns all old models: reusing an ID refuses.
        after_close.promote_shared_open_files().unwrap();
    }

    #[test]
    fn shared_random_promotion_leaves_nonrandom_tables_local() {
        let state = ThreadState::new(DetTid::from_raw(9013), &Config::default(), ());
        let regular = tempfile::tempfile().unwrap();
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        state
            .add_fd(regular.as_raw_fd(), OFlag::O_RDWR, FdType::Regular, None)
            .unwrap();
        state
            .add_fd(socket.as_raw_fd(), OFlag::O_RDWR, FdType::Socket, None)
            .unwrap();
        let before: HashMap<_, _> = state
            .metadata()
            .file_handles
            .iter()
            .map(|(&fd, handle)| (fd, handle.open_file_id()))
            .collect();
        state.promote_shared_open_files().unwrap();
        assert!(state.shared_open_files_for_exec().unwrap().is_none());
        for (fd, id) in before {
            let metadata = state.metadata();
            let handle = &metadata.file_handles[&fd];
            assert!(!handle.is_shared());
            assert_eq!(handle.open_file_id(), id);
        }
    }

    #[test]
    fn shared_exec_restores_allocator_before_legacy_pipe_discovery() {
        crate::fd::install_test_shared_open_file_channel();
        let owner = DetTid::from_raw(9014);
        let config = Config::default();
        let parent = ThreadState::new(owner, &config, ());
        let random = std::fs::File::open("/dev/random").unwrap();
        let fd = random.as_raw_fd();
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
        parent
            .add_fd(fd, OFlag::O_RDONLY, FdType::Rng, None)
            .unwrap();
        parent.promote_shared_open_files().unwrap();
        let transfer = parent.shared_open_files_for_exec().unwrap().unwrap();
        let random_id = transfer.handles[&fd].id;
        let old_next = transfer.next_sequence;
        let (pipe, _writer) = nix::unistd::pipe2(OFlag::O_NONBLOCK).unwrap();
        let pipe_fd = pipe.as_raw_fd();
        let successor = ThreadState::new(owner, &config, ());
        successor
            .metadata()
            .apply_exec_fd_state(
                owner,
                ExecFdState {
                    blocking: [pipe_fd].into_iter().collect(),
                    open_files: Some(transfer),
                    ..ExecFdState::default()
                },
                true,
            )
            .unwrap();
        let mut metadata = successor.metadata();
        let pipe_id = metadata.file_handles[&pipe_fd].open_file_id();
        assert_eq!(pipe_id, OpenFileId::new(owner, old_next));
        assert_ne!(pipe_id, random_id);
        assert_eq!(metadata.file_handles[&pipe_fd].ty(), FdType::Pipe);
        assert!(!metadata.file_handles[&pipe_fd].is_nonblocking());
        assert!(metadata.file_handles[&pipe_fd].physically_nonblocking());
        assert!(!metadata.file_handles[&pipe_fd].is_shared());
        assert_eq!(metadata.file_handles[&fd].open_file_id(), random_id);
        assert_ne!(
            metadata.allocate_open_file_id(owner, FdType::Regular),
            pipe_id
        );
        // Interning must never merge the unrelated restored pipe and RNG.
        crate::fd::intern_open_files(&mut metadata.file_handles);
        assert_eq!(metadata.file_handles[&pipe_fd].ty(), FdType::Pipe);
        assert_eq!(metadata.file_handles[&fd].ty(), FdType::Rng);
    }

    /// The channel end of a pipe made through `ThreadState::add_fd`, as
    /// `handle_pipe2` stores it on the read end under `--pipe-wake-latency`.
    fn pipe_with_channel(
        state: &ThreadState<()>,
    ) -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd, ChannelEnd) {
        let (reader, writer) = nix::unistd::pipe2(OFlag::empty()).unwrap();
        state
            .add_fd(reader.as_raw_fd(), OFlag::O_RDONLY, FdType::Pipe, None)
            .unwrap();
        let id = state.metadata().file_handles[&reader.as_raw_fd()].open_file_id();
        let end = ChannelEnd {
            channel: ChannelId::Pipe(id),
            reads: true,
            writes: false,
        };
        state
            .with_detfd(reader.as_raw_fd(), |detfd| detfd.set_channel(end))
            .unwrap();
        (reader, writer, end)
    }

    /// A channel lives on the open file description, so a `dup` and a forked
    /// child's copy of the table carry it with no copy step, while a
    /// descriptor Detcore discovers afresh, as it does one it did not create
    /// (one received over `SCM_RIGHTS`), has none
    /// (<https://github.com/rrnewton/hermit/issues/4380>).
    /// The capacity a descriptor directory's end follows is Linux's: it
    /// grows to the next power of two above an installed descriptor, keeps
    /// it when the descriptor closes, and a fork's copy is sized from what
    /// is open (measured: `dup2(0, 100); close(100)` ends at 130, and a
    /// fork's child after it at 66).
    #[test]
    fn the_descriptor_capacity_follows_linuxs_allocation() {
        let owner = DetTid::from_raw(9031);
        let mut table = FileMetadata::new(owner);
        assert_eq!(table.descriptor_capacity, 64);
        table
            .add_fd(owner, 100, OFlag::empty(), FdType::Regular, None)
            .unwrap();
        assert_eq!(table.descriptor_capacity, 128);
        table.remove_fd(100);
        assert_eq!(table.descriptor_capacity, 128);
        assert_eq!(
            table.fork_for(DetTid::from_raw(9032)).descriptor_capacity,
            64
        );
        table
            .add_fd(owner, 70, OFlag::empty(), FdType::Regular, None)
            .unwrap();
        assert_eq!(
            table.fork_for(DetTid::from_raw(9033)).descriptor_capacity,
            128
        );
        assert_eq!(table.for_exec(owner).descriptor_capacity, 128);
    }

    /// A `dup2` or `dup3` to 100 whose old descriptor is not open grows the
    /// table to 128 slots, as `ksys_dup3` expands it before it looks the old
    /// descriptor up (measured: `dup2(-1, 100)` fails with EBADF and a
    /// listing then ends at 130); a target below the capacity changes
    /// nothing.
    #[test]
    fn a_failed_dup_grows_the_table_for_its_target() {
        let owner = DetTid::from_raw(9041);
        let mut table = FileMetadata::new(owner);
        table.grow_to_hold(10);
        assert_eq!(table.descriptor_capacity, 64);
        table.grow_to_hold(100);
        assert_eq!(table.descriptor_capacity, 128);
        assert!(table.descriptor_numbers().is_empty());
        table.grow_to_hold(-1);
        assert_eq!(table.descriptor_capacity, 128);
    }

    /// A descriptor listed without modelling keeps the close-on-exec flag
    /// `F_SETFD`, `FIOCLEX` and `FIONCLEX` give it, which decides whether it
    /// survives `exec`; any other descriptor is not one.
    #[test]
    fn an_unmodelled_descriptor_keeps_its_close_on_exec_changes() {
        let owner = DetTid::from_raw(9042);
        let mut table = FileMetadata::new(owner);
        table.present_unmodelled.insert(5, false);
        table.present_unmodelled.insert(6, true);
        assert!(table.set_unmodelled_cloexec(5, true));
        assert!(table.set_unmodelled_cloexec(6, false));
        assert!(!table.set_unmodelled_cloexec(7, true));
        let after = table.for_exec(owner);
        assert!(!after.present_unmodelled.contains_key(&5));
        assert_eq!(after.present_unmodelled.get(&6), Some(&false));
    }

    /// A descriptor open in the table that Detcore does not model (a
    /// received anonymous inode) is listed until it is closed or replaced,
    /// survives `exec` unless it is close-on-exec, and a `dup` of it is
    /// another such descriptor.
    #[test]
    fn an_unmodelled_descriptor_is_listed_until_it_goes() {
        let owner = DetTid::from_raw(9034);
        let mut table = FileMetadata::new(owner);
        table
            .add_fd(owner, 0, OFlag::empty(), FdType::Regular, None)
            .unwrap();
        table.present_unmodelled.insert(5, false);
        table.present_unmodelled.insert(6, true);
        assert_eq!(table.descriptor_numbers(), vec![0, 5, 6]);
        assert_eq!(table.for_exec(owner).descriptor_numbers(), vec![0, 5]);
        assert_eq!(table.dup_fd(5, 9, OFlag::O_CLOEXEC).unwrap(), None);
        assert_eq!(table.present_unmodelled.get(&9), Some(&true));
        table.remove_fd(5);
        table
            .add_fd(owner, 6, OFlag::empty(), FdType::Regular, None)
            .unwrap();
        assert_eq!(table.descriptor_numbers(), vec![0, 6, 9]);
        assert!(!table.present_unmodelled.contains_key(&6));
        table.remove_fd_range(0, 100);
        assert!(table.descriptor_numbers().is_empty());
    }

    /// The close that drops a socket's last descriptor takes the messages
    /// queued to it, for the call to release what they held
    /// (`FileMetadata::release_detfd`), as Linux purges a released socket's
    /// receive queue; one that leaves another descriptor of it takes
    /// nothing, and a copy of the table has nothing to release.
    #[test]
    fn the_last_close_of_a_socket_takes_its_queue() {
        let owner = DetTid::from_raw(9043);
        let mut table = FileMetadata::new(owner);
        for (fd, ty) in [
            (3, FdType::Socket),
            (4, FdType::Socket),
            (5, FdType::Regular),
        ] {
            table.add_fd(owner, fd, OFlag::empty(), ty, None).unwrap();
        }
        let left = table.with_detfd(3, |detfd| detfd.clone()).unwrap();
        let right = table.with_detfd(4, |detfd| detfd.clone()).unwrap();
        left.pair_sockets(&right, false);
        let carried = table
            .with_detfd(5, |detfd| detfd.in_flight_open_file(true))
            .unwrap();
        assert!(matches!(
            left.queue_rights_to_peer(
                vec![SentDescriptor {
                    file: (1, 5),
                    open_file: Some(carried),
                    lock_copy: None,
                    suspect: false,
                }],
                1
            ),
            Ok(())
        ));
        drop((left, right));
        table.dup_fd(4, 6, OFlag::empty()).unwrap();
        assert_eq!(table.remove_fd(4), None, "another descriptor of it remains");
        assert!(table.take_released_rights().is_empty());
        let id = table.with_detfd(6, |detfd| detfd.open_file_id()).unwrap();
        assert_eq!(table.remove_fd(6), Some(id), "its last descriptor");
        let copy = table.clone();
        assert_eq!(table.take_released_rights().len(), 1, "its queue, taken");
        assert!(
            copy.released_rights.0.is_empty(),
            "a copy of the table releases nothing"
        );
    }

    /// A table let go at its holder's exit takes the queue of each socket
    /// whose last reference it held, so what those queues carried is
    /// released by that exit (`ThreadState::release_table_at_exit`), whenever
    /// the backend tears the exited thread's state down: here a socket the
    /// parent sent and closed, queued to a socket only the child held, which
    /// is then the last reference to it.
    #[test]
    fn a_table_let_go_at_exit_takes_its_last_sockets_queues() {
        let parent_id = DetTid::from_raw(9044);
        let mut parent = FileMetadata::new(parent_id);
        for fd in [3, 4] {
            parent
                .add_fd(parent_id, fd, OFlag::empty(), FdType::Socket, None)
                .unwrap();
        }
        let left = parent.with_detfd(3, |detfd| detfd.clone()).unwrap();
        let right = parent.with_detfd(4, |detfd| detfd.clone()).unwrap();
        left.pair_sockets(&right, false);
        drop(right);
        let mut child = parent.fork_for(DetTid::from_raw(9045));
        assert_eq!(parent.remove_fd(4), None, "the child holds the receiver");
        assert_eq!(child.remove_fd(3), None, "the parent holds the sender");
        parent
            .add_fd(parent_id, 5, OFlag::empty(), FdType::Socket, None)
            .unwrap();
        let carried = parent
            .with_detfd(5, |detfd| detfd.in_flight_open_file(true))
            .unwrap();
        let sent_id = parent.with_detfd(5, |detfd| detfd.open_file_id()).unwrap();
        assert!(matches!(
            left.queue_rights_to_peer(
                vec![SentDescriptor {
                    file: (1, 5),
                    open_file: Some(carried),
                    lock_copy: None,
                    suspect: false,
                }],
                1
            ),
            Ok(())
        ));
        assert_eq!(parent.remove_fd(5), None, "a copy is in flight");
        child.release_every_descriptor();
        assert!(child.file_handles.is_empty());
        let released: Vec<_> = child
            .take_released_rights()
            .into_iter()
            .filter_map(|sent| {
                sent.open_file
                    .and_then(crate::fd::InFlightOpenFile::release)
            })
            .map(|(id, ty, _)| (id, ty))
            .collect();
        assert_eq!(
            released,
            [(sent_id, FdType::Socket)],
            "the exit released the queued socket, whose last reference it was"
        );
        drop(left);
    }

    /// The exit that leaves a table to no holder lets it go
    /// (`FileMetadata::leave`), as guest calls alone count its holders: a
    /// thread's `exit` counts one out and an `exit_group` its whole process;
    /// a process made with `CLONE_FILES` holds the table too; a copy made for
    /// a fork or an exec has one holder of its own. A table whose holders are
    /// not known, or a process that does not hold it, never goes this way.
    #[test]
    fn a_table_goes_with_the_exit_that_leaves_it_no_holder() {
        let process = DetTid::from_raw(9050);
        let mut table = FileMetadata::new(process);
        table.join_holder(process);
        assert!(!table.leave(process, false), "one thread of two exits");
        assert!(table.leave(process, false), "and then the other");

        let mut table = FileMetadata::new(process);
        table.join_holder(process);
        table.join_holder(process);
        assert!(table.leave(process, true), "an exit_group ends all three");

        let sharer = DetTid::from_raw(9051);
        let mut table = FileMetadata::new(process);
        table.join_holder(sharer);
        assert!(
            !table.leave(process, true),
            "a process made with CLONE_FILES"
        );
        assert!(
            !table.leave(process, false),
            "a process that left holds none"
        );
        assert!(table.leave(sharer, false));

        let mut copy = FileMetadata::new(process).fork_for(sharer);
        assert!(
            copy.leave(sharer, false),
            "a fork's copy, its child's alone"
        );
        let mut copy = FileMetadata::new(process).for_exec(sharer);
        copy.hold_alone(process);
        assert!(!copy.leave(sharer, false));
        assert!(copy.leave(process, false), "an exec's copy, its process's");

        let mut unknown = FileMetadata::new(process);
        unknown.holders.clear();
        unknown.join_holder(process);
        assert!(!unknown.leave(process, true), "holders not known");
    }

    /// An exec leaves the table it replaces, all of the exec'ing process's
    /// threads at once (`ThreadState::release_exec_dropped_descriptors`), and
    /// drops that table's descriptors only where it was the last holder:
    /// Linux copies a table another process still holds (`unshare_files`),
    /// which keeps every descriptor, and that process's own exit then lets the
    /// table go, which the exec'ing process no longer counts against. A table
    /// whose holders are not known is left as it is (Codex's notes on K8-h
    /// reb7: the exec'ing process stayed a holder of the shared table, so the
    /// other process's exit never let it go).
    #[test]
    fn an_exec_leaves_a_table_another_process_still_holds() {
        let process = DetTid::from_raw(9052);
        let sharer = DetTid::from_raw(9053);
        let exec_of = |table: FileMetadata| {
            let replaced = Arc::new(Mutex::new(table));
            let mut new = replaced.lock().unwrap().for_exec(process);
            new.hold_alone(process);
            new.keep_replaced_table(Arc::clone(&replaced), process);
            let state = ThreadState::new(process, &Config::default(), ());
            *state.file_metadata.lock().unwrap() = new;
            let released = state.release_exec_dropped_descriptors();
            (replaced, released)
        };

        let mut shared = FileMetadata::new(process);
        shared.join_holder(process);
        shared.join_holder(sharer);
        shared
            .add_fd(process, 3, OFlag::O_CLOEXEC, FdType::Regular, None)
            .unwrap();
        let (shared, released) = exec_of(shared);
        assert!(!released, "a process made with CLONE_FILES still holds it");
        let mut shared = shared.lock().unwrap();
        assert_eq!(shared.file_handles.len(), 1, "it keeps every descriptor");
        assert_eq!(shared.holders, BTreeMap::from([(sharer, 1)]));
        assert!(shared.leave(sharer, true), "its exit lets the table go");

        let mut unknown = FileMetadata::new(process);
        unknown.holders.clear();
        unknown
            .add_fd(process, 3, OFlag::O_CLOEXEC, FdType::Regular, None)
            .unwrap();
        let (unknown, released) = exec_of(unknown);
        assert!(!released);
        assert_eq!(
            unknown.lock().unwrap().file_handles.len(),
            1,
            "holders not known"
        );
    }

    /// The exec of a process whose table only it holds lets go of that
    /// table's descriptors at the new image's first callback
    /// (`ThreadState::release_exec_dropped_descriptors`): here a receiving
    /// socket whose last descriptor, in the child, is close-on-exec, queued a
    /// description the parent sent and closed, so the exec releases the copy
    /// in flight, as Linux releases it when the exec closes the socket
    /// (Claude's narrow check of K8-h reb6, finding 1: before, the copy, and
    /// with it the lock ledger's count of its locks, stayed for the rest of
    /// the run). The replaced table names nothing afterwards, and a second
    /// call releases nothing.
    #[test]
    fn an_exec_releases_what_its_close_on_exec_receiving_socket_held() {
        let parent_id = DetTid::from_raw(9054);
        let child_id = DetTid::from_raw(9055);
        let mut parent = FileMetadata::new(parent_id);
        parent
            .add_fd(parent_id, 3, OFlag::empty(), FdType::Socket, None)
            .unwrap();
        parent
            .add_fd(parent_id, 4, OFlag::O_CLOEXEC, FdType::Socket, None)
            .unwrap();
        let left = parent.with_detfd(3, |detfd| detfd.clone()).unwrap();
        let right = parent.with_detfd(4, |detfd| detfd.clone()).unwrap();
        left.pair_sockets(&right, false);
        drop(right);
        let mut child = parent.fork_for(child_id);
        assert_eq!(parent.remove_fd(4), None, "the child holds the receiver");
        assert_eq!(child.remove_fd(3), None, "the parent holds the sender");
        parent
            .add_fd(parent_id, 5, OFlag::empty(), FdType::Regular, None)
            .unwrap();
        let carried = parent
            .with_detfd(5, |detfd| detfd.in_flight_open_file(true))
            .unwrap();
        let sent_id = parent.with_detfd(5, |detfd| detfd.open_file_id()).unwrap();
        assert!(matches!(
            left.queue_rights_to_peer(
                vec![SentDescriptor {
                    file: (1, 5),
                    open_file: Some(carried),
                    lock_copy: Some(sent_id),
                    suspect: false,
                }],
                1
            ),
            Ok(())
        ));
        assert_eq!(parent.remove_fd(5), None, "a copy is in flight");

        child.join_holder(child_id);
        let replaced = Arc::new(Mutex::new(child));
        let mut new = replaced.lock().unwrap().for_exec(child_id);
        new.hold_alone(child_id);
        assert!(
            new.file_handles.is_empty(),
            "the receiver was close-on-exec"
        );
        new.keep_replaced_table(Arc::clone(&replaced), child_id);
        let state = ThreadState::new(child_id, &Config::default(), ());
        *state.file_metadata.lock().unwrap() = new;

        assert!(state.release_exec_dropped_descriptors());
        let released: Vec<_> = state
            .take_released_rights()
            .into_iter()
            .map(|sent| {
                (
                    sent.lock_copy,
                    sent.open_file
                        .and_then(crate::fd::InFlightOpenFile::release)
                        .map(|(id, ty, _)| (id, ty)),
                )
            })
            .collect();
        assert_eq!(
            released,
            [(Some(sent_id), Some((sent_id, FdType::Regular)))],
            "the exec released the copy in flight, its description's last reference"
        );
        assert!(
            replaced.lock().unwrap().file_handles.is_empty(),
            "the replaced table names nothing"
        );
        assert!(!state.release_exec_dropped_descriptors(), "released once");
        drop(left);
    }

    /// The death of a process by a fatal signal's default action, which its
    /// terminal receipt reports (`GlobalState::release_tables_at_death`), lets
    /// go of a table only that process held (`FileMetadata::release_at_death`):
    /// here a receiving socket whose last descriptor the child holds queued
    /// a description the parent sent and closed, so the death releases the
    /// copy in flight, the description's last reference, as Linux's
    /// `exit_files` releases it before the parent can wait (Codex's narrow
    /// check of K8-h reb8: before, KVM listed its lock after the child died
    /// by its own SIGTERM, abort() or SIGSEGV). Every thread of the process
    /// leaves at once. A table another process still holds keeps every
    /// descriptor, and a table whose holders are not known, or do not name
    /// the process, is left undecided.
    #[test]
    fn a_fatal_signals_death_releases_a_table_only_its_process_held() {
        let parent_id = DetTid::from_raw(9056);
        let child_id = DetTid::from_raw(9057);
        let sharer = DetTid::from_raw(9058);
        let mut parent = FileMetadata::new(parent_id);
        parent
            .add_fd(parent_id, 3, OFlag::empty(), FdType::Socket, None)
            .unwrap();
        parent
            .add_fd(parent_id, 4, OFlag::empty(), FdType::Socket, None)
            .unwrap();
        let left = parent.with_detfd(3, |detfd| detfd.clone()).unwrap();
        let right = parent.with_detfd(4, |detfd| detfd.clone()).unwrap();
        left.pair_sockets(&right, false);
        drop(right);
        let mut child = parent.fork_for(child_id);
        child.hold_alone(child_id);
        assert_eq!(parent.remove_fd(4), None, "the child holds the receiver");
        assert_eq!(child.remove_fd(3), None, "the parent holds the sender");
        parent
            .add_fd(parent_id, 5, OFlag::empty(), FdType::Regular, None)
            .unwrap();
        let carried = parent
            .with_detfd(5, |detfd| detfd.in_flight_open_file(true))
            .unwrap();
        let sent_id = parent.with_detfd(5, |detfd| detfd.open_file_id()).unwrap();
        assert!(matches!(
            left.queue_rights_to_peer(
                vec![SentDescriptor {
                    file: (1, 5),
                    open_file: Some(carried),
                    lock_copy: Some(sent_id),
                    suspect: false,
                }],
                1
            ),
            Ok(())
        ));
        assert_eq!(parent.remove_fd(5), None, "a copy is in flight");

        // A second thread of the child holds the table too.
        child.join_holder(child_id);
        let TableAtDeath::Released(rights) = child.release_at_death(child_id) else {
            panic!("the child alone held its table");
        };
        assert_eq!(
            crate::syscalls::scm_rights::settle(rights),
            crate::syscalls::scm_rights::Settled {
                lock_copies: vec![sent_id],
                unfollowable_copies: Vec::new(),
                ports: Vec::new(),
            },
            "the death released the copy in flight, its description's last reference"
        );
        assert!(child.file_handles.is_empty(), "every descriptor went");
        assert!(child.holders.is_empty(), "both threads left");
        assert!(
            matches!(child.release_at_death(child_id), TableAtDeath::Undecided),
            "released once"
        );

        let mut shared = FileMetadata::new(child_id);
        shared.join_holder(sharer);
        shared
            .add_fd(child_id, 3, OFlag::empty(), FdType::Regular, None)
            .unwrap();
        assert!(matches!(
            shared.release_at_death(child_id),
            TableAtDeath::Kept
        ));
        assert_eq!(shared.file_handles.len(), 1, "it keeps every descriptor");
        assert_eq!(shared.holders, BTreeMap::from([(sharer, 1)]));

        let mut unknown = FileMetadata::new(child_id);
        unknown.holders.clear();
        unknown
            .add_fd(child_id, 3, OFlag::empty(), FdType::Regular, None)
            .unwrap();
        assert!(matches!(
            unknown.release_at_death(child_id),
            TableAtDeath::Undecided
        ));
        assert_eq!(unknown.file_handles.len(), 1, "holders not known");
        let mut another = FileMetadata::new(sharer);
        another
            .add_fd(sharer, 3, OFlag::empty(), FdType::Regular, None)
            .unwrap();
        assert!(matches!(
            another.release_at_death(child_id),
            TableAtDeath::Undecided
        ));
        assert_eq!(another.file_handles.len(), 1, "not the process's table");
        drop(left);
    }

    #[test]
    fn a_pipe_channel_follows_its_description_through_dup_and_fork() {
        if isolated_child(
            "tool_local::file_metadata_tests::a_pipe_channel_follows_its_description_through_dup_and_fork",
        ) {
            return;
        }
        let owner = DetTid::from_raw(9018);
        let mut parent = FileMetadata::new(owner);
        let (reader, _writer) = nix::unistd::pipe2(OFlag::empty()).unwrap();
        let fd = reader.as_raw_fd();
        parent
            .add_fd(owner, fd, OFlag::empty(), FdType::Pipe, None)
            .unwrap();
        let end = ChannelEnd {
            channel: ChannelId::Pipe(parent.with_detfd(fd, |detfd| detfd.open_file_id()).unwrap()),
            reads: true,
            writes: false,
        };
        parent
            .with_detfd(fd, |detfd| detfd.set_channel(end))
            .unwrap();
        let alias = fd + 100;
        parent.dup_fd(fd, alias, OFlag::empty()).unwrap();
        assert_eq!(
            parent.with_detfd(alias, |detfd| detfd.channel()).unwrap(),
            Some(end)
        );
        let mut child = parent.fork_for(DetTid::from_raw(9019));
        for fd in [fd, alias] {
            assert_eq!(
                child.with_detfd(fd, |detfd| detfd.channel()).unwrap(),
                Some(end)
            );
        }
        let (received, _peer) = nix::unistd::pipe2(OFlag::empty()).unwrap();
        parent
            .discover_fd_from_current_process(owner, received.as_raw_fd())
            .unwrap();
        assert_eq!(
            parent
                .with_detfd(received.as_raw_fd(), |detfd| detfd.channel())
                .unwrap(),
            None
        );
    }

    /// A descriptor received over `SCM_RIGHTS` on a backend whose tool does
    /// not share the guest's table is recorded from injected facts
    /// (`add_discovered_fd`); an in-guest backend discovers the same
    /// descriptor in process. Both make the same description: type, status
    /// flags, close-on-exec, physical nonblocking and an unobserved lock.
    #[test]
    fn a_received_descriptor_is_recorded_as_the_in_guest_discovery_records_it() {
        let owner = DetTid::from_raw(9031);
        let facts = |fd: RawFd| {
            // SAFETY: fcntl and fstat only read this test's descriptor.
            let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0);
            (fd_flags, status, stat)
        };
        let shape = |metadata: &FileMetadata, fd: RawFd| {
            let detfd = &metadata.file_handles[&fd];
            (
                detfd.ty(),
                detfd.status_flags(),
                detfd.is_cloexec(),
                detfd.physically_nonblocking(),
                detfd.is_nonblocking(),
                detfd.known_flock_mode(),
                detfd.flock_may_have_been_granted(),
                detfd.stat().map(|stat| stat.raw_inode()),
            )
        };
        let (pipe_read, _pipe_write) =
            nix::unistd::pipe2(OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).unwrap();
        let (socket, _peer) = nix::sys::socket::socketpair(
            nix::sys::socket::AddressFamily::Unix,
            nix::sys::socket::SockType::Stream,
            None,
            nix::sys::socket::SockFlag::empty(),
        )
        .unwrap();
        let file = std::fs::File::open("/proc/self/exe").unwrap();
        let random = std::fs::File::open("/dev/urandom").unwrap();
        // SAFETY: inotify_init1 takes no pointer; the descriptor is owned here.
        let inotify = unsafe {
            <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(libc::inotify_init1(
                libc::IN_NONBLOCK | libc::IN_CLOEXEC,
            ))
        };
        let cases = [
            (pipe_read.as_raw_fd(), FdType::Pipe),
            (socket.as_raw_fd(), FdType::Socket),
            (file.as_raw_fd(), FdType::Regular),
            (random.as_raw_fd(), FdType::Rng),
            (inotify.as_raw_fd(), FdType::Inotify),
        ];
        for (fd, ty) in cases {
            let mut recorded = FileMetadata::new(owner);
            let (fd_flags, status, stat) = facts(fd);
            // As the receipt reads it: only an anonymous inode's link.
            let link = (stat.st_mode & libc::S_IFMT == 0)
                .then(|| super::own_fd_link(fd))
                .flatten();
            recorded
                .add_discovered_fd(owner, fd, fd_flags, status, &stat, link.as_deref())
                .unwrap();
            let mut discovered = FileMetadata::new(owner);
            discovered
                .discover_fd_from_current_process(owner, fd)
                .unwrap();
            assert_eq!(shape(&recorded, fd), shape(&discovered, fd), "fd {fd}");
            assert_eq!(recorded.file_handles[&fd].ty(), ty, "fd {fd}");
            // Recorded unobserved: no lock claim, and one may be held.
            assert_eq!(recorded.file_handles[&fd].known_flock_mode(), None);
        }
        let pipe = pipe_read.as_raw_fd();
        let mut recorded = FileMetadata::new(owner);
        let (fd_flags, status, stat) = facts(pipe);
        recorded
            .add_discovered_fd(owner, pipe, fd_flags, status, &stat, None)
            .unwrap();
        let detfd = &recorded.file_handles[&pipe];
        // The kernel's flags are what the record knows: O_NONBLOCK stays
        // visible and physical, and FD_CLOEXEC is the slot's.
        assert!(detfd.is_cloexec() && detfd.is_nonblocking() && detfd.physically_nonblocking());
    }

    /// Under `--pipe-wake-latency` a rediscovering exec carries each surviving
    /// descriptor's channel end and the old numbering. The same leader, whose
    /// new image restarts its open-file sequence, rediscovers pipe A with A's
    /// channel; a pipe B it then makes, past every id the old table allocated,
    /// has another channel value. A descriptor left `O_NONBLOCK`, outside the
    /// blocking set, gets its channel too
    /// (https://github.com/rrnewton/hermit/issues/4380).
    #[test]
    fn a_rediscovering_exec_keeps_pipe_channels_apart() {
        let leader = DetTid::from_raw(9015);
        let config = Config::default();
        let before = ThreadState::new(leader, &config, ());
        let (a_reader, _a_writer, a_end) = pipe_with_channel(&before);
        assert_eq!(
            unsafe { libc::fcntl(a_reader.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            0
        );
        let channels = before.pipe_channels_for_exec();
        assert_eq!(channels, BTreeMap::from([(a_reader.as_raw_fd(), a_end)]));
        let namespace = before.open_file_namespace_for_exec();
        assert_eq!(
            namespace,
            (leader, before.metadata().next_open_file_sequence)
        );

        let after = ThreadState::new(leader, &config, ());
        after
            .metadata()
            .apply_exec_fd_state(
                leader,
                ExecFdState {
                    channels,
                    open_file_namespace: Some(namespace),
                    ..ExecFdState::default()
                },
                true,
            )
            .unwrap();
        assert_eq!(
            after
                .with_detfd(a_reader.as_raw_fd(), |detfd| detfd.channel())
                .unwrap(),
            Some(a_end),
            "the carried channel survives rediscovery"
        );
        let (b_reader, _b_writer, b_end) = pipe_with_channel(&after);
        assert_ne!(b_end.channel, a_end.channel);
        let metadata = after.metadata();
        assert_ne!(
            metadata.file_handles[&b_reader.as_raw_fd()].open_file_id(),
            metadata.file_handles[&a_reader.as_raw_fd()].open_file_id()
        );
    }

    /// A channel held by another process is kept apart too: the old image
    /// made pipe A, a child holds it, and the exec carries no channel at all;
    /// the old numbering is still reserved, so a pipe the new image makes
    /// cannot take A's id.
    #[test]
    fn an_exec_carrying_no_channel_still_reserves_the_old_numbering() {
        let leader = DetTid::from_raw(9016);
        let config = Config::default();
        let before = ThreadState::new(leader, &config, ());
        let (a_reader, _a_writer, a_end) = pipe_with_channel(&before);
        let namespace = before.open_file_namespace_for_exec();
        drop(a_reader);

        let after = ThreadState::new(leader, &config, ());
        let state = ExecFdState {
            open_file_namespace: Some(namespace),
            ..ExecFdState::default()
        };
        assert!(
            !state.is_empty(),
            "a namespace-only state survives reconnect"
        );
        after
            .metadata()
            .apply_exec_fd_state(leader, state, true)
            .unwrap();
        let (_b_reader, _b_writer, b_end) = pipe_with_channel(&after);
        assert_ne!(b_end.channel, a_end.channel);
    }

    /// With the option off the exec state carries neither field: nothing is
    /// reserved, the new image numbers as a fresh one does, and the state's
    /// trace prints as it did before the fields existed. A channel-only state
    /// is not empty.
    #[test]
    fn an_exec_without_the_pipe_wake_latency_is_unchanged() {
        let leader = DetTid::from_raw(9017);
        let config = Config::default();
        let after = ThreadState::new(leader, &config, ());
        let state = ExecFdState::default();
        assert!(state.is_empty());
        assert_eq!(
            format!("{state:?}"),
            "ExecFdState { blocking: {}, inherited_reads: {}, \
             initialize_inherited_stdio: false, initial_input: None, \
             robust_wakes: [], open_files: None }"
        );
        after
            .metadata()
            .apply_exec_fd_state(leader, state, true)
            .unwrap();
        let (reader, _writer, _) = pipe_with_channel(&after);
        let fresh = ThreadState::new(leader, &config, ());
        let (fresh_reader, _fresh_writer, _) = pipe_with_channel(&fresh);
        assert_eq!(
            after.metadata().file_handles[&reader.as_raw_fd()].open_file_id(),
            fresh.metadata().file_handles[&fresh_reader.as_raw_fd()].open_file_id()
        );
        let channel_only = ExecFdState {
            channels: BTreeMap::from([(
                reader.as_raw_fd(),
                ChannelEnd {
                    channel: ChannelId::Pipe(OpenFileId::new(leader, 0)),
                    reads: true,
                    writes: false,
                },
            )]),
            ..ExecFdState::default()
        };
        assert!(!channel_only.is_empty());
        assert!(format!("{channel_only:?}").contains("channels: {"));

        // A description prints its channel only when it has one.
        let (plain, _plain_writer) = nix::unistd::pipe2(OFlag::empty()).unwrap();
        after
            .add_fd(plain.as_raw_fd(), OFlag::O_RDONLY, FdType::Pipe, None)
            .unwrap();
        let printed = format!("{:?}", after.metadata().file_handles[&plain.as_raw_fd()]);
        assert!(!printed.contains(" channel:"), "{printed}");
        assert!(printed.contains("timerfd: None }"), "{printed}");
        let printed = format!("{:?}", after.metadata().file_handles[&reader.as_raw_fd()]);
        assert!(
            printed.contains("timerfd: None, channel: ChannelEnd {"),
            "{printed}"
        );
    }

    fn terminal_pair() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
        use std::os::fd::FromRawFd;
        let (mut master, mut slave) = (-1, -1);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        unsafe {
            (
                std::os::fd::OwnedFd::from_raw_fd(master),
                std::os::fd::OwnedFd::from_raw_fd(slave),
            )
        }
    }

    #[test]
    fn inherited_terminal_discovery_keeps_kernel_flags_and_nonterminal_types() {
        let owner = DetTid::from_raw(9);
        let (_master, slave) = terminal_pair();
        let regular = tempfile::tempfile().unwrap();
        let zero = std::fs::File::open("/dev/zero").unwrap();
        let null = std::fs::File::open("/dev/null").unwrap();
        let (pipe, _writer) = nix::unistd::pipe().unwrap();
        let fds = [
            slave.as_raw_fd(),
            regular.as_raw_fd(),
            zero.as_raw_fd(),
            null.as_raw_fd(),
            pipe.as_raw_fd(),
        ];
        let before: Vec<_> = fds
            .iter()
            .map(|fd| unsafe {
                (
                    libc::fcntl(*fd, libc::F_GETFL),
                    libc::fcntl(*fd, libc::F_GETFD),
                )
            })
            .collect();
        let mut metadata = FileMetadata::new(owner);
        for fd in fds {
            metadata
                .discover_fd_from_current_process(owner, fd)
                .unwrap();
        }
        metadata.discover_inherited_terminals(fds);
        for (index, fd) in fds.into_iter().enumerate() {
            let detfd = &metadata.file_handles[&fd];
            assert_eq!(detfd.inherited_external_read().is_some(), index == 0);
            assert_eq!(
                detfd.ty(),
                if index == 4 {
                    FdType::Pipe
                } else {
                    FdType::Regular
                }
            );
            assert_eq!(
                unsafe {
                    (
                        libc::fcntl(fd, libc::F_GETFL),
                        libc::fcntl(fd, libc::F_GETFD),
                    )
                },
                before[index]
            );
        }
    }

    /// A memfd named and sealed as a `/proc/cpuinfo` placeholder, met by a
    /// table that rediscovers descriptors before the session's answer (an
    /// in-guest backend's image after `exec`), is a placeholder only once the
    /// session says it renders the file. With CPUID virtualized but not
    /// intercepted (SaBRe), `Detcore::rendered_files` says it does not, and
    /// the memfd stays the ordinary memfd it is (Codex and
    /// deterministic-scheduling review of
    /// https://github.com/rrnewton/hermit/issues/4213).
    #[test]
    fn a_cpuinfo_placeholder_waits_for_the_sessions_answer() {
        let owner = DetTid::from_raw(9);
        let forged = || {
            // SAFETY: the name is NUL-terminated; the result is checked.
            let raw = unsafe {
                libc::memfd_create(
                    c"hermit-procfs:/proc/cpuinfo@".as_ptr(),
                    libc::MFD_ALLOW_SEALING,
                )
            };
            assert!(
                raw >= 0,
                "memfd_create: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: `raw` is a descriptor this test just made and owns.
            let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
            let seals = crate::syscalls::rendered_procfs::PLACEHOLDER_SEALS;
            assert_eq!(unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, seals) }, 0);
            fd
        };
        let ordinary = |metadata: &FileMetadata, fd: RawFd| {
            metadata.file_handles[&fd].procfs_rendered_path().is_none()
        };
        // The session does not render /proc/cpuinfo (CPUID virtualized by
        // the configuration, but the backend delivers no CPUID to Detcore).
        let fd = forged();
        let mut metadata = FileMetadata::new(owner);
        metadata
            .discover_fd_from_current_process(owner, fd.as_raw_fd())
            .unwrap();
        assert!(ordinary(&metadata, fd.as_raw_fd()));
        metadata.decide_cpuinfo_placeholders(false).unwrap();
        assert!(ordinary(&metadata, fd.as_raw_fd()));
        // Once decided, a later descriptor is not one either.
        let later = forged();
        metadata
            .discover_fd_from_current_process(owner, later.as_raw_fd())
            .unwrap();
        assert!(ordinary(&metadata, later.as_raw_fd()));
        // A later answer of the same session changes nothing.
        metadata.decide_cpuinfo_placeholders(true).unwrap();
        assert!(ordinary(&metadata, fd.as_raw_fd()));
        // The session renders it: the waiting descriptor becomes the
        // placeholder, and so is one met after the answer.
        let mut metadata = FileMetadata::new(owner);
        metadata
            .discover_fd_from_current_process(owner, fd.as_raw_fd())
            .unwrap();
        assert!(ordinary(&metadata, fd.as_raw_fd()));
        metadata.decide_cpuinfo_placeholders(true).unwrap();
        assert_eq!(
            metadata.file_handles[&fd.as_raw_fd()].procfs_rendered_path(),
            Some(std::path::Path::new("/proc/cpuinfo"))
        );
        let after = forged();
        metadata
            .discover_fd_from_current_process(owner, after.as_raw_fd())
            .unwrap();
        assert_eq!(
            metadata.file_handles[&after.as_raw_fd()].procfs_rendered_path(),
            Some(std::path::Path::new("/proc/cpuinfo"))
        );
    }

    /// A task's `arch_status` is rendered exactly where `/proc/cpuinfo` is,
    /// so a memfd named and sealed as its placeholder, met before the
    /// session's answer, waits for that answer as a `/proc/cpuinfo`
    /// placeholder does, and becomes the placeholder it is once the session
    /// renders the files (Claude review of the kvm lane's slice N3: it was
    /// neither adopted nor queued, and stayed an ordinary memfd).
    #[test]
    fn an_arch_status_placeholder_waits_for_the_sessions_answer() {
        let owner = DetTid::from_raw(9);
        let stand_in = || {
            // SAFETY: the name is NUL-terminated; the result is checked.
            let raw = unsafe {
                libc::memfd_create(
                    c"hermit-procfs:/proc/3/arch_status#1@".as_ptr(),
                    libc::MFD_ALLOW_SEALING,
                )
            };
            assert!(
                raw >= 0,
                "memfd_create: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: `raw` is a descriptor this test just made and owns.
            let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
            let seals = crate::syscalls::rendered_procfs::PLACEHOLDER_SEALS;
            assert_eq!(unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, seals) }, 0);
            fd
        };
        // The session does not render the files: it stays an ordinary memfd.
        let fd = stand_in();
        let mut metadata = FileMetadata::new(owner);
        metadata
            .discover_fd_from_current_process(owner, fd.as_raw_fd())
            .unwrap();
        assert!(
            metadata.file_handles[&fd.as_raw_fd()]
                .procfs_rendered_path()
                .is_none()
        );
        metadata.decide_cpuinfo_placeholders(false).unwrap();
        assert!(
            metadata.file_handles[&fd.as_raw_fd()]
                .procfs_rendered_path()
                .is_none()
        );
        // The session renders them: the waiting descriptor becomes the
        // task's arch_status.
        let mut metadata = FileMetadata::new(owner);
        metadata
            .discover_fd_from_current_process(owner, fd.as_raw_fd())
            .unwrap();
        assert!(
            metadata.file_handles[&fd.as_raw_fd()]
                .procfs_rendered_path()
                .is_none()
        );
        metadata.decide_cpuinfo_placeholders(true).unwrap();
        assert_eq!(
            metadata.file_handles[&fd.as_raw_fd()].procfs_rendered_path(),
            Some(std::path::Path::new("/proc/3/arch_status"))
        );
    }

    /// A memfd named and sealed as the placeholder of a file the run's
    /// `--proc` level omits stays the ordinary memfd it is when a table
    /// discovers it: Detcore makes no placeholder for such a file, whose open
    /// fails with ENOENT (Claude review of the kvm lane's slice N3, round 2).
    /// At a level that has the file it is the placeholder, as before.
    #[test]
    fn a_memfd_with_a_stand_in_name_outside_the_proc_level_stays_a_memfd() {
        use detcore_model::config::ProcLevel;
        let owner = DetTid::from_raw(9);
        // SAFETY: the name is NUL-terminated; the result is checked.
        let raw = unsafe {
            libc::memfd_create(
                c"hermit-procfs:/proc/diskstats@".as_ptr(),
                libc::MFD_ALLOW_SEALING,
            )
        };
        assert!(
            raw >= 0,
            "memfd_create: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `raw` is a descriptor this test just made and owns.
        let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        let seals = crate::syscalls::rendered_procfs::PLACEHOLDER_SEALS;
        assert_eq!(unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, seals) }, 0);
        let diskstats = std::path::Path::new("/proc/diskstats");
        assert!(crate::syscalls::rendered_procfs::named_file_outside_level(
            diskstats,
            ProcLevel::Minimal
        ));
        assert!(!crate::syscalls::rendered_procfs::named_file_outside_level(
            diskstats,
            ProcLevel::Standard
        ));
        let mut noted = FileMetadata::new(owner);
        noted.note_proc_level(ProcLevel::Minimal);
        // Carried through serialization, as a table is across a fork or an
        // exec on an in-guest backend, before it rediscovers a descriptor.
        let mut minimal: FileMetadata =
            serde_json::from_slice(&serde_json::to_vec(&noted.for_exec(owner)).unwrap()).unwrap();
        minimal
            .discover_fd_from_current_process(owner, fd.as_raw_fd())
            .unwrap();
        assert!(
            minimal.file_handles[&fd.as_raw_fd()]
                .procfs_rendered_path()
                .is_none()
        );
        let mut standard = FileMetadata::new(owner);
        standard.note_proc_level(ProcLevel::Standard);
        standard
            .discover_fd_from_current_process(owner, fd.as_raw_fd())
            .unwrap();
        assert_eq!(
            standard.file_handles[&fd.as_raw_fd()].procfs_rendered_path(),
            Some(diskstats)
        );
    }

    #[test]
    fn inherited_terminal_aliases_survive_fork_serialization_and_exec_handoff() {
        use std::os::fd::FromRawFd;
        let owner = DetTid::from_raw(9);
        let (_master, slave) = terminal_pair();
        let fd = slave.as_raw_fd();
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        assert!(duplicate >= 0);
        let duplicate = unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicate) };
        let alias = duplicate.as_raw_fd();
        // Make this alias survive the real descriptor boundary as the model does.
        assert_eq!(unsafe { libc::fcntl(alias, libc::F_SETFD, 0) }, 0);
        let mut metadata = FileMetadata::new(owner);
        metadata
            .discover_fd_from_current_process(owner, fd)
            .unwrap();
        metadata.file_handles[&fd].set_resource(ResourceID::Device(Device::ContainerStdin));
        metadata.discover_inherited_terminals([fd]);
        metadata.dup_fd(fd, alias, OFlag::empty()).unwrap();
        metadata.dup_fd(fd, 100_000, OFlag::O_CLOEXEC).unwrap();
        let child = metadata.fork_for(DetTid::from_raw(10));
        let encoded = serde_json::to_vec(&child).unwrap();
        let child: FileMetadata = serde_json::from_slice(&encoded).unwrap();
        let state = ExecFdState {
            inherited_reads: child.inherited_reads_for_exec(),
            ..Default::default()
        };
        assert_eq!(
            state.inherited_reads.len(),
            2,
            "CLOEXEC alias is not transferred"
        );
        assert_eq!(state.inherited_reads[&fd], state.inherited_reads[&alias]);
        let mut restored = FileMetadata::new(owner);
        restored
            .apply_exec_fd_state(
                owner,
                serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap(),
                true,
            )
            .unwrap();
        assert_eq!(
            restored.file_handles[&fd].open_file_id(),
            restored.file_handles[&alias].open_file_id()
        );
        assert_eq!(
            restored.file_handles[&alias].resource(),
            Some(ResourceID::Device(Device::ContainerStdin))
        );
        let flags = unsafe { libc::fcntl(alias, libc::F_GETFL) };
        assert_eq!(
            unsafe { libc::fcntl(alias, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        restored.file_handles[&alias].set_status_flags(flags | libc::O_NONBLOCK);
        assert!(restored.file_handles[&fd].is_nonblocking());
        assert!(restored.file_handles[&fd].physically_nonblocking());
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFL) },
            flags | libc::O_NONBLOCK
        );
        let mut byte = 0_u8;
        assert_eq!(unsafe { libc::read(fd, (&raw mut byte).cast(), 1) }, -1);
        assert_eq!(Errno::last(), Errno::EAGAIN);
    }

    #[test]
    fn guest_created_terminal_and_reused_descriptor_do_not_gain_inherited_provenance() {
        let owner = DetTid::from_raw(9);
        let (_master, slave) = terminal_pair();
        let fd = slave.as_raw_fd();
        let mut guest_created = FileMetadata::new(owner);
        guest_created
            .discover_fd_from_current_process(owner, fd)
            .unwrap();
        guest_created.dup_fd(fd, 0, OFlag::empty()).unwrap();
        assert!(guest_created.inherited_reads_for_exec().is_empty());
        let mut fresh = FileMetadata::new(owner);
        fresh
            .apply_exec_fd_state(owner, ExecFdState::default(), true)
            .unwrap();
        fresh.discover_fd_from_current_process(owner, fd).unwrap();
        assert!(fresh.file_handles[&fd].inherited_external_read().is_none());

        guest_created.discover_inherited_terminals([fd]);
        let state = ExecFdState {
            inherited_reads: guest_created.inherited_reads_for_exec(),
            ..Default::default()
        };
        // Remove the synthetic slot; only the actual owned fd enters this check.
        let state = ExecFdState {
            inherited_reads: state
                .inherited_reads
                .into_iter()
                .filter(|(number, _)| *number == fd)
                .collect(),
            ..Default::default()
        };
        guest_created.remove_fd(fd);
        let zero = std::fs::File::open("/dev/zero").unwrap();
        assert_eq!(unsafe { libc::dup2(zero.as_raw_fd(), fd) }, fd);
        let mut reused = FileMetadata::new(owner);
        assert_eq!(
            reused.apply_exec_fd_state(owner, state, true),
            Err(Errno::EIO)
        );
        assert!(reused.file_handles[&fd].inherited_external_read().is_none());
    }

    #[test]
    fn on_demand_discovery_finds_a_live_descriptor() {
        let owner = DetTid::from_raw(9);
        let file = tempfile::NamedTempFile::new().expect("create test descriptor");
        let fd = file.as_raw_fd();
        assert_eq!(unsafe { libc::flock(fd, libc::LOCK_SH | libc::LOCK_NB) }, 0);
        let mut metadata = FileMetadata::new(owner);

        assert_eq!(metadata.with_detfd(fd, |_| ()), Err(Errno::EBADF));
        metadata
            .discover_fd_from_current_process(owner, fd)
            .expect("live descriptor should be discovered");
        assert_eq!(
            metadata
                .with_detfd(fd, |detfd| detfd.ty())
                .expect("discovered descriptor should be tracked"),
            FdType::Regular
        );
        assert_eq!(
            metadata
                .with_detfd(fd, |detfd| detfd.known_flock_mode())
                .expect("discovered descriptor should be tracked"),
            None,
            "a live descriptor may already hold a flock that Detcore did not observe"
        );
        assert!(
            !metadata
                .with_detfd(fd, |detfd| detfd.flock_mode_may_be_stale())
                .expect("discovered descriptor should be tracked"),
            "first discovery must not invent a previously known lock state"
        );
        assert!(
            !metadata.has_unsafe_vfork_flock_state(),
            "never-observed history is distinct from a held or invalidated cached claim"
        );

        // The metadata repair must not query by converting or dropping the
        // inherited lock. A separate open must still be excluded by it.
        let separate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(file.path())
            .expect("open an independent file description");
        assert_eq!(
            unsafe { libc::flock(separate.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1
        );
        assert_eq!(Errno::last(), Errno::EWOULDBLOCK);

        metadata
            .with_detfd(fd, |detfd| detfd.set_flock_mode(None))
            .expect("discovered descriptor should remain tracked");
        assert_eq!(metadata.file_handles[&fd].known_flock_mode(), None);
        assert!(!metadata.file_handles[&fd].flock_mode_may_be_stale());
    }

    #[test]
    fn discovery_classifies_random_devices_by_device_number() {
        let owner = DetTid::from_raw(9);
        let random = std::fs::File::open("/dev/random").unwrap();
        let urandom = std::fs::File::open("/dev/urandom").unwrap();
        let null = std::fs::File::open("/dev/null").unwrap();
        let path_only = nix::fcntl::open(
            "/dev/urandom",
            OFlag::O_PATH | OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .unwrap();
        let mut metadata = FileMetadata::new(owner);
        for (fd, expected) in [
            (random.as_raw_fd(), FdType::Rng),
            (urandom.as_raw_fd(), FdType::Rng),
            // Another character device, and an `O_PATH` descriptor of a
            // random device, which reads nothing, stay `Regular`.
            (null.as_raw_fd(), FdType::Regular),
            (path_only.as_raw_fd(), FdType::Regular),
        ] {
            metadata
                .discover_fd_from_current_process(owner, fd)
                .expect("live descriptor should be discovered");
            assert_eq!(metadata.file_handles[&fd].ty(), expected, "fd {fd}");
        }
        // A discovered random device is named by its descriptor, apart from
        // the count of opens, which discovery leaves alone.
        for fd in [random.as_raw_fd(), urandom.as_raw_fd()] {
            assert_eq!(
                metadata.file_handles[&fd].random_device_stream(),
                crate::fd::RandomDeviceStream::discovered(owner, fd)
            );
        }
        assert_eq!(metadata.next_random_device_sequence, 0);
    }

    #[test]
    fn rediscovery_preserves_known_and_invalidated_flock_history() {
        let owner = DetTid::from_raw(9);
        let file = tempfile::tempfile().expect("create test descriptor");
        let fd = file.as_raw_fd();
        let mut metadata = FileMetadata::new(owner);
        metadata
            .add_fd(owner, fd, OFlag::O_RDWR, FdType::Regular, None)
            .expect("register an observed open");
        let description = metadata.file_handles[&fd].open_file_id();
        assert_eq!(metadata.file_handles[&fd].known_flock_mode(), Some(None));
        metadata.file_handles[&fd].set_flock_mode(Some(libc::LOCK_SH));

        metadata
            .discover_fd_from_current_process(owner, fd)
            .expect("rediscover an existing descriptor");
        assert_eq!(metadata.file_handles[&fd].open_file_id(), description);
        assert_eq!(
            metadata.file_handles[&fd].known_flock_mode(),
            Some(Some(libc::LOCK_SH))
        );
        assert!(metadata.has_unsafe_vfork_flock_state());

        metadata.file_handles[&fd].forget_flock_mode();
        metadata
            .discover_fd_from_current_process(owner, fd)
            .expect("rediscover an invalidated descriptor");
        assert_eq!(metadata.file_handles[&fd].open_file_id(), description);
        assert_eq!(metadata.file_handles[&fd].known_flock_mode(), None);
        assert!(metadata.file_handles[&fd].flock_mode_may_be_stale());
        assert!(metadata.file_handles[&fd].flock_may_have_been_granted());
        assert!(metadata.has_unsafe_vfork_flock_state());
    }

    /// The converse of the case above: a description Detcore created, never
    /// locked through Detcore, then invalidated by a fork (a `make` job pipe)
    /// has no Detcore claim in any process that a copied `vfork` child's
    /// native lock call could make stale, so it must not refuse `vfork`.
    #[test]
    fn invalidated_description_never_locked_does_not_refuse_vfork() {
        let owner = DetTid::from_raw(9);
        let mut metadata = FileMetadata::new(owner);
        metadata
            .add_fd(owner, 3, OFlag::empty(), FdType::Pipe, None)
            .expect("register a pipe end");
        assert!(!metadata.file_handles[&3].flock_may_have_been_granted());

        let child = metadata.fork_for(DetTid::from_raw(10));
        for table in [&metadata, &child] {
            let detfd = &table.file_handles[&3];
            assert_eq!(detfd.known_flock_mode(), None);
            assert!(detfd.flock_mode_may_be_stale());
            assert!(!detfd.flock_may_have_been_granted());
            assert!(
                !table.has_unsafe_vfork_flock_state(),
                "an invalidated description with no grant through it holds no Detcore claim"
            );
        }

        // A grant after the invalidation is still a grant: the mode stays
        // unknown, but the description may now hold a lock.
        metadata.file_handles[&3].set_flock_mode(Some(libc::LOCK_EX));
        assert_eq!(metadata.file_handles[&3].known_flock_mode(), None);
        assert!(metadata.file_handles[&3].flock_may_have_been_granted());
        assert!(metadata.has_unsafe_vfork_flock_state());
    }

    /// A serialized description that predates `flock_ever_granted` reads it
    /// as true in a self-describing format, so an invalidated description
    /// from such a table still refuses `vfork`.
    #[test]
    fn a_description_serialized_without_the_grant_fact_reads_it_as_granted() {
        let owner = DetTid::from_raw(9);
        let mut metadata = FileMetadata::new(owner);
        metadata
            .add_fd(owner, 3, OFlag::empty(), FdType::Pipe, None)
            .expect("register a pipe end");
        let child = metadata.fork_for(DetTid::from_raw(10));
        assert!(!child.has_unsafe_vfork_flock_state());
        let encoded = serde_json::to_string(&child).expect("serialize the table");
        assert_eq!(encoded.matches("\"flock_ever_granted\":false").count(), 1);
        let without = encoded.replace("\"flock_ever_granted\":false,", "");
        assert_ne!(without, encoded, "the field must have been removed");
        let restored: FileMetadata = serde_json::from_str(&without).expect("deserialize the table");
        assert!(restored.file_handles[&3].flock_mode_may_be_stale());
        assert!(restored.file_handles[&3].flock_may_have_been_granted());
        assert!(restored.has_unsafe_vfork_flock_state());
    }

    /// A lock granted before a fork travels with the description: both
    /// tables refuse `vfork` after the fork. The fact is deliberately sticky,
    /// so a lock released before the fork still counts: the guard stays a
    /// single monotone fact rather than a second lock-state cache.
    #[test]
    fn lock_granted_before_fork_still_refuses_vfork_after_it() {
        let owner = DetTid::from_raw(9);
        let mut metadata = FileMetadata::new(owner);
        metadata
            .add_fd(owner, 3, OFlag::empty(), FdType::Regular, None)
            .expect("register descriptor");
        metadata.file_handles[&3].set_flock_mode(Some(libc::LOCK_SH));
        metadata.file_handles[&3].set_flock_mode(None);
        assert_eq!(metadata.file_handles[&3].known_flock_mode(), Some(None));
        assert!(
            !metadata.has_unsafe_vfork_flock_state(),
            "a known-unlocked description is safe while its mode is known"
        );

        let child = metadata.fork_for(DetTid::from_raw(10));
        for table in [&metadata, &child] {
            assert!(table.file_handles[&3].flock_may_have_been_granted());
            assert!(table.has_unsafe_vfork_flock_state());
        }
    }

    /// A table carried through serialization (in-guest LiteInst's fork) keeps
    /// its aliases: two descriptors that shared one open file description
    /// before still share one object after, while a separate open stays
    /// separate and descriptor flags stay per descriptor.
    #[test]
    fn a_deserialized_table_keeps_dup_aliases_as_one_open_file() {
        let owner = DetTid::from_raw(9);
        let mut metadata = FileMetadata::new(owner);
        metadata
            .add_fd(owner, 3, OFlag::empty(), FdType::Rng, None)
            .expect("register descriptor");
        metadata
            .dup_fd(3, 4, OFlag::O_CLOEXEC)
            .expect("dup the descriptor");
        metadata
            .add_fd(owner, 5, OFlag::empty(), FdType::Rng, None)
            .expect("register a separate open");
        let encoded = serde_json::to_string(&metadata).expect("serialize the table");
        let restored: FileMetadata = serde_json::from_str(&encoded).expect("deserialize the table");
        let three = restored.file_handles[&3].clone();
        let four = restored.file_handles[&4].clone();
        let five = restored.file_handles[&5].clone();
        assert_eq!(three.open_file_id(), four.open_file_id());
        assert_ne!(three.open_file_id(), five.open_file_id());
        // One object again: a read through one alias moves the other's cursor.
        four.advance_random_device_offset(50);
        assert_eq!(three.random_device_offset(), 50);
        assert_eq!(five.random_device_offset(), 0);
        // The table plus the two local handles above.
        assert_eq!(three.open_file_alias_count(), 4);
        // Descriptor flags stay per descriptor.
        assert!(!three.is_cloexec());
        assert!(four.is_cloexec());
    }

    /// Each open of a random device reads a stream of its own, named by its
    /// opener and the table's count of earlier random-device opens; a dup
    /// alias shares it. Other opens leave the count alone. Fork and exec
    /// carry it, and so does the exec exchange of a backend that rebuilds its
    /// table, so no later open in the table or a copy of it reuses a name.
    #[test]
    fn each_random_device_open_reads_its_own_stream() {
        use crate::fd::RandomDeviceStream;
        let owner = DetTid::from_raw(9);
        let thread = DetTid::from_raw(11);
        let stream = |table: &mut FileMetadata, fd| {
            table
                .with_detfd(fd, |detfd| detfd.random_device_stream())
                .expect("registered descriptor")
        };
        let open = |table: &mut FileMetadata, creator, fd| {
            table
                .add_fd(creator, fd, OFlag::empty(), FdType::Regular, None)
                .expect("register the open");
            table
                .classify_random_device(creator, fd)
                .expect("classify it");
        };
        let mut metadata = FileMetadata::new(owner);
        open(&mut metadata, owner, 3);
        metadata.dup_fd(3, 4, OFlag::empty()).expect("dup it");
        metadata
            .add_fd(owner, 5, OFlag::empty(), FdType::Regular, None)
            .expect("open a regular file");
        open(&mut metadata, thread, 6);
        assert_eq!(
            metadata.classify_random_device(thread, 7),
            Err(Errno::EBADF)
        );
        let child = DetTid::from_raw(10);
        let mut forked = metadata.fork_for(child);
        open(&mut forked, child, 7);
        let mut exec = metadata.for_exec(owner);
        open(&mut exec, owner, 7);
        let mut rebuilt = FileMetadata::new(owner);
        rebuilt
            .apply_exec_fd_state(
                owner,
                ExecFdState {
                    random_device_sequence: 2,
                    ..ExecFdState::default()
                },
                true,
            )
            .expect("apply the exec exchange");
        open(&mut rebuilt, owner, 3);

        let named = |creator, sequence| RandomDeviceStream { creator, sequence };
        assert_eq!(stream(&mut metadata, 3), named(owner, 0));
        assert_eq!(stream(&mut metadata, 4), named(owner, 0));
        assert_eq!(stream(&mut metadata, 6), named(thread, 1));
        assert_eq!(stream(&mut forked, 3), named(owner, 0));
        assert_eq!(stream(&mut forked, 7), named(child, 2));
        assert_eq!(stream(&mut exec, 7), named(owner, 2));
        assert_eq!(stream(&mut rebuilt, 3), named(owner, 2));
    }

    #[test]
    fn fork_makes_inherited_flock_state_unknown_in_parent_and_child() {
        let owner = DetTid::from_raw(9);
        let child = DetTid::from_raw(10);
        let mut metadata = FileMetadata::new(owner);
        metadata
            .add_fd(owner, 3, OFlag::empty(), FdType::Regular, None)
            .expect("register descriptor");
        metadata
            .with_detfd(3, |detfd| detfd.set_flock_mode(Some(libc::LOCK_SH)))
            .expect("registered descriptor");

        let mut child_metadata = metadata.fork_for(child);
        assert_eq!(
            metadata
                .with_detfd(3, |detfd| detfd.known_flock_mode())
                .expect("parent descriptor should remain tracked"),
            None,
            "parent cache must not become stale when the child changes the shared open file description"
        );
        assert_eq!(
            child_metadata
                .with_detfd(3, |detfd| detfd.known_flock_mode())
                .expect("child descriptor should be inherited"),
            None,
            "child starts with the same deliberately unknown shared state"
        );
    }

    #[test]
    fn exec_preserves_known_flock_mode_for_surviving_descriptors() {
        let owner = DetTid::from_raw(9);
        let mut metadata = FileMetadata::new(owner);
        metadata
            .add_fd(owner, 3, OFlag::empty(), FdType::Regular, None)
            .expect("register descriptor");
        metadata
            .with_detfd(3, |detfd| detfd.set_flock_mode(Some(libc::LOCK_SH)))
            .expect("registered descriptor");

        let mut after_exec = metadata.for_exec(DetTid::from_raw(10));
        assert_eq!(
            after_exec
                .with_detfd(3, |detfd| detfd.known_flock_mode())
                .expect("non-cloexec descriptor should survive"),
            Some(Some(libc::LOCK_SH)),
            "exec must preserve known flock state for a surviving open file description"
        );
    }

    #[test]
    fn discovered_stdio_uses_container_wide_resources() {
        let owner = DetTid::from_raw(9);
        let mut metadata = FileMetadata::new(owner);

        metadata
            .discover_fd_from_current_process(owner, libc::STDOUT_FILENO)
            .expect("live stdout should be discovered");

        assert_eq!(
            metadata
                .with_detfd(libc::STDOUT_FILENO, |detfd| detfd.resource())
                .expect("discovered stdout should be tracked"),
            Some(ResourceID::Device(Device::ContainerStdout))
        );

        // Observe real metadata transitions without closing the test process's
        // stdio. The narrow repair preserves slot identities for inherited
        // resources and leaves aliases above fd 2 in the ordinary inode pool.
        use crate::syscalls::deterministic_stdio_inode_for_resource;

        let inherited_stat = metadata
            .with_detfd(libc::STDOUT_FILENO, |detfd| detfd.stat().unwrap())
            .unwrap();
        metadata.dup_fd(1, 7, OFlag::empty()).unwrap();
        assert_eq!(
            metadata.with_detfd(7, |detfd| detfd.resource()).unwrap(),
            Some(ResourceID::Device(Device::ContainerStdout))
        );
        assert_eq!(
            metadata
                .with_detfd(7, |detfd| {
                    deterministic_stdio_inode_for_resource(7, detfd.resource())
                })
                .unwrap(),
            None
        );
        for fd in 0..=2 {
            metadata.remove_fd(fd);
            let mut ordinary = inherited_stat;
            ordinary.inode = 123 + fd as u64;
            metadata
                .add_fd(owner, fd, OFlag::empty(), FdType::Regular, Some(ordinary))
                .unwrap();
            metadata.dup_fd(fd, 8, OFlag::empty()).unwrap();
            for ordinary_fd in [fd, 8] {
                assert_eq!(
                    metadata
                        .with_detfd(ordinary_fd, |detfd| {
                            deterministic_stdio_inode_for_resource(ordinary_fd, detfd.resource())
                        })
                        .unwrap(),
                    None
                );
            }
            assert_eq!(
                metadata
                    .with_detfd(fd, |detfd| detfd.open_file_id())
                    .unwrap(),
                metadata
                    .with_detfd(8, |detfd| detfd.open_file_id())
                    .unwrap()
            );
            metadata.dup_fd(7, fd, OFlag::empty()).unwrap();
            assert_eq!(
                metadata
                    .with_detfd(fd, |detfd| {
                        deterministic_stdio_inode_for_resource(fd, detfd.resource())
                    })
                    .unwrap(),
                Some(DetInode::mint(1000 + fd as u64)),
                "inherited streams retain the existing numeric-slot outcome"
            );
        }
        metadata.dup_fd(7, 9, OFlag::O_CLOEXEC).unwrap();
        let child = metadata.fork_for(DetTid::from_raw(10));
        let after_exec = child.for_exec(DetTid::from_raw(10));
        assert!(!after_exec.file_handles.contains_key(&9));
        assert!(after_exec.file_handles.contains_key(&7));
    }

    #[test]
    fn discovered_pipe_preserves_inherited_nonblocking() {
        let owner = DetTid::from_raw(9);
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) },
            0
        );
        let mut metadata = FileMetadata::new(owner);

        metadata
            .discover_fd_from_current_process(owner, fds[0])
            .expect("live pipe should be discovered");
        let flags = metadata
            .with_detfd(fds[0], |detfd| {
                (detfd.is_nonblocking(), detfd.physically_nonblocking())
            })
            .expect("discovered pipe should be tracked");

        assert_eq!(flags, (true, true));
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    #[test]
    fn exec_handoff_restores_scheduler_pipe_as_logically_blocking() {
        let owner = DetTid::from_raw(9);
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) },
            0
        );
        let mut before_exec = FileMetadata::new(owner);
        before_exec
            .add_fd(owner, fds[0], OFlag::empty(), FdType::Pipe, None)
            .expect("scheduler pipe should be registered");
        before_exec
            .with_detfd(fds[0], |detfd| detfd.set_physically_nonblocking())
            .expect("scheduler pipe should remain registered");

        let overrides = before_exec.exec_blocking_overrides();
        assert_eq!(overrides, BTreeSet::from([fds[0]]));

        let mut after_exec = FileMetadata::new(owner);
        after_exec.apply_exec_blocking_overrides(owner, overrides);
        assert_eq!(
            after_exec
                .with_detfd(fds[0], |detfd| {
                    (detfd.is_nonblocking(), detfd.physically_nonblocking())
                })
                .expect("inherited pipe should be rediscovered"),
            (false, true)
        );

        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

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

        let initial_timestamp = LogicalTime::from_nanos(1_234_567_890);
        parent
            .with_detfd(3, |fd| fd.set_socket_receive_timestamp(initial_timestamp))
            .expect("parent socket should accept a receive timestamp");

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
        assert_eq!(
            child
                .with_detfd(3, |fd| fd.socket_receive_timestamp())
                .expect("forked fd should retain its receive timestamp"),
            Some(initial_timestamp)
        );
        let child_timestamp = LogicalTime::from_nanos(2_345_678_901);
        child
            .with_detfd(3, |fd| fd.set_socket_receive_timestamp(child_timestamp))
            .expect("child socket should update the shared receive timestamp");
        assert_eq!(
            parent
                .with_detfd(4, |fd| fd.socket_receive_timestamp())
                .expect("parent duplicate should see the child update"),
            Some(child_timestamp)
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
    fn pidfd_getfd_target_requires_the_calling_leader() {
        let leader = DetPid::from_raw(31);
        assert!(pidfd_getfd_targets_calling_task(
            Some(leader),
            leader,
            DetTid::from_raw(31),
        ));
        assert!(!pidfd_getfd_targets_calling_task(
            Some(leader),
            leader,
            DetTid::from_raw(32),
        ));
        assert!(!pidfd_getfd_targets_calling_task(
            Some(DetPid::from_raw(32)),
            leader,
            DetTid::from_raw(31),
        ));
    }

    #[test]
    fn captured_fd_installs_when_the_descriptor_table_is_unchanged() {
        let owner = DetTid::from_raw(32);
        let mut metadata = FileMetadata::new(owner);
        metadata
            .add_fd(owner, 3, OFlag::empty(), FdType::Regular, None)
            .expect("source should be inserted");
        let source_id = metadata
            .with_detfd(3, |fd| fd.open_file_id())
            .expect("source should exist");
        let captured = metadata.capture_fd(3).expect("source should be captured");

        assert_eq!(
            metadata
                .install_captured_fd(captured, 4, OFlag::O_CLOEXEC)
                .expect("an unchanged table should accept the captured alias"),
            None
        );
        assert_eq!(
            metadata
                .with_detfd(4, |fd| (fd.open_file_id(), fd.is_cloexec()))
                .expect("captured alias should be installed"),
            (source_id, true)
        );
    }

    #[test]
    fn failed_captured_fd_install_preserves_cleanup_obligations() {
        let owner = DetTid::from_raw(36);
        let mut original = FileMetadata::new(owner);
        original
            .add_fd(owner, 3, OFlag::empty(), FdType::Socket, None)
            .expect("source should be inserted");
        let source_id = original
            .with_detfd(3, |fd| fd.open_file_id())
            .expect("source should exist");
        let captured = original.capture_fd(3).expect("source should be captured");
        assert_eq!(
            original.remove_fd(3),
            None,
            "the capture defers final-OFD cleanup while the syscall is in flight"
        );

        let mut replacement_table = FileMetadata::new(DetTid::from_raw(37));
        let failure = replacement_table
            .install_captured_fd(captured, 41, OFlag::O_CLOEXEC)
            .expect_err("a capture must not cross descriptor-table identity");
        assert_ne!(failure.expected_files_id, failure.actual_files_id);
        let cleanup = failure.into_cleanup();
        assert_eq!(cleanup.close_fd, 41);
        assert_eq!(cleanup.release_open_file, Some(source_id));
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
    fn close_range_removes_selected_slots_and_releases_final_aliases() {
        let owner = DetTid::from_raw(35);
        let mut metadata = FileMetadata::new(owner);
        metadata
            .add_fd(owner, 3, OFlag::empty(), FdType::Regular, None)
            .expect("source should be inserted");
        metadata
            .dup_fd(3, 4, OFlag::empty())
            .expect("duplicate should be inserted");
        metadata
            .add_fd(owner, 100, OFlag::empty(), FdType::Regular, None)
            .expect("high fd should be inserted");
        let high_id = metadata
            .with_detfd(100, |fd| fd.open_file_id())
            .expect("high fd should exist");

        assert_eq!(metadata.remove_fd_range(4, 100), [high_id]);
        assert!(metadata.with_detfd(3, |_| ()).is_ok());
        assert_eq!(metadata.with_detfd(4, |_| ()), Err(Errno::EBADF));
        assert_eq!(metadata.with_detfd(100, |_| ()), Err(Errno::EBADF));
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

    /// A rendered per-process file `spelling` (`/proc/<pid>/<name>`) bound to
    /// a task, as a rendered open makes one.
    fn rendered_record_file(spelling: &str) -> crate::procfs::ProcfsFile {
        let mut file = crate::procfs::ProcfsFile::from_path(Path::new(spelling)).unwrap();
        file.bind_task(3, 5);
        file.set_task_incarnation(crate::procfs::TaskIncarnation {
            process: 1,
            thread: None,
        });
        file.into_rendered()
    }

    /// A per-process file that follows its record's sequence survives an
    /// exec on a backend that rebuilds its descriptor table with its open
    /// file's identity, its model and its sequence (the cursor, where the
    /// sequence is, the buffered record), aliases kept and a close-on-exec
    /// copy dropped, and with the old table's numbering: so the new image
    /// continues the record where every holder's sequence is (Claude's
    /// narrow check of the kvm lane's slice N3, round 5: before, the new image
    /// rebuilt it with no sequence) and allocates no identity a sequence in
    /// Detcore's global state may still hold (Codex's, round 5: before, it
    /// numbered from 0 again), also where the only such description closed
    /// at the exec after a fork shared its sequence. A survivor whose sequence
    /// Detcore cannot know stays so, and a slot reused since the preparation
    /// is refused (EIO).
    #[test]
    fn record_sequence_exec_survivors_keep_identity_sequence_and_numbering() {
        if isolated_child(
            "tool_local::file_metadata_tests::record_sequence_exec_survivors_keep_identity_sequence_and_numbering",
        ) {
            return;
        }
        use crate::procfs::ProcfsFile;
        use crate::procfs::ProcfsSnapshotContext;

        let owner = DetTid::from_raw(9031);
        let config = Config::default();
        let mut parent = ThreadState::new(owner, &config, ());
        let statm = std::fs::File::open("/proc/self/statm").unwrap();
        let alias = statm.try_clone().unwrap();
        let closed = statm.try_clone().unwrap();
        let received = std::fs::File::open("/proc/self/statm").unwrap();
        let (fd, alias_fd, closed_fd, received_fd) = (
            statm.as_raw_fd(),
            alias.as_raw_fd(),
            closed.as_raw_fd(),
            received.as_raw_fd(),
        );
        for number in [fd, received_fd] {
            parent
                .add_fd(number, OFlag::O_RDONLY, FdType::Regular, None)
                .unwrap();
            parent
                .with_detfd(number, |handle| {
                    handle.set_path("/proc/self/statm");
                    handle
                        .set_procfs(ProcfsFile::from_path(Path::new("/proc/self/statm")).unwrap());
                })
                .unwrap();
        }
        parent.dup_fd(fd, alias_fd, OFlag::empty()).unwrap();
        parent.dup_fd(fd, closed_fd, OFlag::O_CLOEXEC).unwrap();
        let record = b"1 2 3 4 5 6 7\n".to_vec();
        parent
            .with_detfd(fd, |handle| {
                handle.initialize_procfs(record.clone(), ProcfsSnapshotContext::default());
                handle.commit_procfs_read(0, 4);
                handle.procfs_note_read_end(4);
            })
            .unwrap();
        parent
            .with_detfd(received_fd, |handle| handle.mark_record_sequence_unknown())
            .unwrap();
        // A fork marks the sequences shared.
        parent.share_procfs_sequences();
        let old_id = parent.with_detfd(fd, |h| h.open_file_id()).unwrap();
        let sequence = parent
            .with_detfd(fd, |h| h.procfs_sequence())
            .unwrap()
            .unwrap();
        let records = parent.procfs_records_for_exec().unwrap().unwrap();
        assert!(!records.handles.contains_key(&closed_fd));
        assert_eq!(
            records.handles[&fd].identity.id,
            records.handles[&alias_fd].identity.id
        );
        assert!(records.handles[&received_fd].sequence_unknown);
        let next_sequence = records.next_sequence;
        let state = ExecFdState {
            procfs_records: Some(records),
            ..ExecFdState::default()
        };
        assert!(!state.is_empty());
        assert_eq!(state.open_file_creator(), Some(owner));
        // The serialized exec preparation and completion payload.
        let state: ExecFdState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        let receiving = DetTid::from_raw(9032);
        let mut successor_thread = ThreadState::new(receiving, &config, ());
        // The creator choice the exec-completion handler makes.
        successor_thread.set_open_file_creator(state.open_file_creator().unwrap());
        let mut successor = successor_thread.metadata();
        successor
            .apply_exec_fd_state(receiving, state.clone(), true)
            .unwrap();
        assert!(!successor.file_handles.contains_key(&closed_fd));
        for number in [fd, alias_fd] {
            let handle = &successor.file_handles[&number];
            assert_eq!(handle.open_file_id(), old_id);
            assert_eq!(handle.procfs_shared_sequence(), Some(old_id));
            assert_eq!(handle.procfs_sequence(), Some(sequence.clone()));
            assert!(!handle.record_sequence_unknown());
        }
        assert!(successor.file_handles[&received_fd].record_sequence_unknown());
        // One open file: a read through the alias moves the other's cursor.
        successor.file_handles[&alias_fd].commit_procfs_read(4, 2);
        assert_eq!(successor.file_handles[&fd].procfs_position().unwrap().0, 6);
        assert!(successor.next_open_file_sequence >= next_sequence);
        assert!(successor.has_shared_procfs_sequences);
        drop(successor);
        let next = tempfile::tempfile().unwrap();
        successor_thread
            .add_fd(next.as_raw_fd(), OFlag::O_RDWR, FdType::Regular, None)
            .unwrap();
        assert_eq!(
            successor_thread
                .with_detfd(next.as_raw_fd(), |h| h.open_file_id())
                .unwrap(),
            OpenFileId::new(owner, next_sequence),
        );
        // A table whose shared description closed at the exec still carries
        // its numbering.
        let lone = ThreadState::new(owner, &config, ());
        let cloexec = std::fs::File::open("/proc/self/statm").unwrap();
        lone.add_fd(cloexec.as_raw_fd(), OFlag::O_CLOEXEC, FdType::Regular, None)
            .unwrap();
        lone.with_detfd(cloexec.as_raw_fd(), |handle| {
            handle.set_procfs(ProcfsFile::from_path(Path::new("/proc/self/statm")).unwrap());
        })
        .unwrap();
        assert!(lone.procfs_records_for_exec().unwrap().is_none());
        lone.share_procfs_sequences();
        let numbering = lone.procfs_records_for_exec().unwrap().unwrap();
        assert!(numbering.handles.is_empty());
        assert_eq!(
            numbering.next_sequence,
            lone.metadata().next_open_file_sequence
        );
        assert!(numbering.next_sequence > 0);
        // A reused numeric slot cannot take the old open file's identity.
        let replacement = tempfile::tempfile().unwrap();
        assert_eq!(
            unsafe { libc::dup2(replacement.as_raw_fd(), alias_fd) },
            alias_fd
        );
        assert_eq!(
            FileMetadata::new(owner).apply_exec_fd_state(owner, state, true),
            Err(Errno::EIO)
        );
    }

    /// A descriptor's link names a per-process file that follows its record's
    /// sequence as the kernel opens one only for an empty regular file with a
    /// link (never a placeholder, which has none) whose link names a task's
    /// `stat`, `statm`, `io`, `arch_status` or `cgroup`: not its `comm`, which
    /// the kernel's own open file serves, nor any other file.
    #[test]
    fn a_kernel_record_file_link_names_only_a_sequence_file() {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        stat.st_mode = libc::S_IFREG | 0o444;
        stat.st_nlink = 1;
        for name in ["stat", "statm", "io", "arch_status", "cgroup"] {
            let link = format!("/proc/12/{name}");
            assert_eq!(
                kernel_record_file_link(&stat, link.as_bytes()),
                Some(std::path::PathBuf::from(&link)),
                "{name}"
            );
        }
        assert_eq!(
            kernel_record_file_link(&stat, b"/proc/12/task/13/statm"),
            Some(std::path::PathBuf::from("/proc/12/task/13/statm"))
        );
        for link in [
            "/proc/12/comm",
            "/proc/12/status",
            "/proc/12/maps",
            "/proc/uptime",
            "/tmp/statm",
        ] {
            assert_eq!(
                kernel_record_file_link(&stat, link.as_bytes()),
                None,
                "{link}"
            );
        }
        let mut placeholder = stat;
        placeholder.st_nlink = 0;
        assert_eq!(
            kernel_record_file_link(&placeholder, b"/proc/12/statm"),
            None
        );
        let mut sized = stat;
        sized.st_size = 1;
        assert_eq!(kernel_record_file_link(&sized, b"/proc/12/statm"), None);
        let mut directory = stat;
        directory.st_mode = libc::S_IFDIR | 0o555;
        assert_eq!(kernel_record_file_link(&directory, b"/proc/12/statm"), None);
    }

    /// A rendered per-process file that follows its record's sequence keeps
    /// that sequence in Detcore's global state once a send passes its
    /// descriptor over `SCM_RIGHTS` (`ThreadState::share_sent_procfs_sequence`),
    /// as a fork's copies do: marked, under its open file's identity, and
    /// noted in the table's history. Not a file the kernel opened (a receiver
    /// cannot name its open file), not one whose sequence Detcore cannot know
    /// here, and not a file that follows no record's sequence.
    #[test]
    fn a_sent_rendered_record_file_shares_its_sequence() {
        let owner = DetTid::from_raw(9033);
        let config = Config::default();
        let state = ThreadState::new(owner, &config, ());
        state.metadata().file_handles.clear();
        for (fd, spelling, rendered) in [
            (40, "/proc/7/statm", true),
            (41, "/proc/7/statm", false),
            (42, "/proc/7/statm", true),
            (43, "/proc/7/auxv", true),
        ] {
            state
                .add_fd(fd, OFlag::O_RDONLY, FdType::Regular, None)
                .unwrap();
            let procfs = if rendered {
                rendered_record_file(spelling)
            } else {
                crate::procfs::ProcfsFile::from_path(Path::new(spelling)).unwrap()
            };
            state
                .with_detfd(fd, |handle| handle.set_procfs(procfs.clone()))
                .unwrap();
        }
        state
            .with_detfd(42, |handle| handle.mark_record_sequence_unknown())
            .unwrap();
        let id = state
            .with_detfd(40, |handle| handle.open_file_id())
            .unwrap();
        assert!(!state.metadata().has_shared_procfs_sequences);
        assert_eq!(state.share_sent_procfs_sequence(40), Some(id));
        assert_eq!(
            state
                .with_detfd(40, |handle| handle.procfs_shared_sequence())
                .unwrap(),
            Some(id)
        );
        assert!(state.metadata().has_shared_procfs_sequences);
        // Sent again: the same identity, the mark kept.
        assert_eq!(state.share_sent_procfs_sequence(40), Some(id));
        for fd in [41, 42, 43, 44] {
            assert_eq!(state.share_sent_procfs_sequence(fd), None, "{fd}");
        }
        assert_eq!(
            state
                .with_detfd(41, |handle| handle.procfs_shared_sequence())
                .unwrap(),
            None
        );
        assert_eq!(state.rendered_descriptors(), vec![40, 42, 43]);
    }
}

/// Various measurements of one guest thread's execution. This is useful for printing
/// context in logs as we go and printing a final summary.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct ThreadStats {
    /// A simple count of how many syscalls have been handled on this thread.
    pub syscall_count: u64,

    /// This thread's syscall entries by number, with the outcome of each
    /// completed one (`crate::syscall_histogram`). Its total is
    /// `syscall_count`. Handed to the scheduler when the thread deregisters.
    pub syscall_histogram: SyscallHistogram,

    /// A Detcore refusal policy answered the current syscall
    /// (`crate::syscall_histogram::mark_refused`). Cleared at each syscall
    /// entry.
    pub syscall_refused: bool,

    /// This thread's happens-before counting state: per (syscall, fd, futex
    /// operation) a syscall-occurrence anchor names, how many of this thread's
    /// syscall entries matched, and the origin of each relative anchor whose
    /// base this thread reached (`HappensBeforeProgram::count_syscall_occurrences`).
    /// Empty unless the run has such an anchor; empty for every new thread,
    /// like `syscall_count`.
    pub hb_occurrences: detcore_model::happens_before::HbThreadCounters,

    /// The posthook happens-before anchors the current syscall entry reached
    /// (`Anchor::fires_at_posthook`): counted at its prehook, they check in
    /// with the scheduler when the call completes. Empty between syscalls.
    pub hb_pending_posthook: Vec<String>,

    /// How many register-file samples this thread has CONSIDERED for `--detlog-regs`.
    ///
    /// This is the cadence index, and it exists because no pre-existing counter is a clean
    /// zero-based count of control points: the logged syscall ordinal starts at 2, and
    /// `syscall_count` also counts points this sampler never reaches. Keying the cadence on either
    /// let a short guest whose points never landed on a multiple of N emit ZERO samples while the
    /// run still reported PASS -- a spot-tier green backed by nothing. Counting the samples
    /// themselves makes index 0 the first control point of every thread, so a spot-tier run always
    /// samples at least once.
    pub regs_sample_index: u64,

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

    /// Distribution (min/max/sum/count) of completed timeslice durations for this
    /// thread, in virtual nanoseconds. A slice's duration is the delta of
    /// `thread_logical_time` between two consecutive `next_timeslice` resets.
    pub timeslice_stats: TimesliceStats,

    /// The per-thread logical time (virtual ns) at which the current timeslice
    /// began. `None` until the first slice is opened. Used to compute the
    /// duration of a slice when the next reset occurs.
    pub timeslice_start_ns: Option<LogicalTime>,
}

impl ThreadStats {
    /// Create a new thread stats with zero counters.
    pub fn new() -> Self {
        Default::default()
    }

    /// Count one entry of `sysno`.
    pub fn count_syscall(&mut self, sysno: Sysno) {
        self.syscall_count += 1;
        self.timeslice_syscall_count += 1;
        self.syscall_histogram.enter(sysno);
        self.syscall_refused = false;
    }

    /// Increment the count of signals.
    pub fn count_signal(&mut self) {
        self.signal_count += 1;
        self.timeslice_signal_count += 1;
    }

    /// Reset counters for a new timeslice.
    /// Increases the count of completed timeslices.
    pub(crate) fn reset_timeslice(&mut self) {
        self.timeslice_syscall_count = 0;
        self.timeslice_signal_count = 0;
        self.timeslice_count += 1;
    }

    /// Close the final, in-progress timeslice at thread exit, recording its
    /// virtual-ns duration. This captures short-lived or I/O-bound threads that
    /// exit (or block until exit) before ever exhausting a slice, so they still
    /// contribute one sample. Idempotent: consumes `timeslice_start_ns`.
    pub fn close_final_timeslice(&mut self, now: LogicalTime) {
        if let Some(start) = self.timeslice_start_ns.take()
            && now >= start
        {
            self.timeslice_stats.record((now - start).as_nanos());
        }
    }
}

/// Information inherited by a `CLONE_VFORK` child so it can register itself
/// while its parent is blocked inside the kernel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingVfork {
    pub parent_dettid: DetTid,
    /// The parent's `BlockingVfork` operation, which identifies this vfork's
    /// scheduler barrier.
    pub op: ExternalOpId,
    pub parent_detpid: DetPid,
    pub child_tid_addr: usize,
    pub flags: CloneFlags,
    pub exit_signal: libc::c_int,
    pub child_priority_entropy: Option<u64>,
    /// With `CLONE_NEWUSER`: the creator's effective user ID may not have
    /// been its real one, read before the clone
    /// (`Detcore::user_namespace_owner_may_differ`). The child notes it for
    /// the namespace the clone made when it registers itself, once the clone
    /// has succeeded.
    #[serde(default)]
    pub user_namespace_owner_may_differ: bool,
}

// TODO-HUMAN-REVIEW(#797): Review process-wide logical CPU aggregation.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct ProcessCpuSnapshot {
    pub user: LogicalTime,
    pub system: LogicalTime,
    pub children_user: LogicalTime,
    pub children_system: LogicalTime,
}

/// Runtime-only readiness for one child birth. A retired or replaced token can
/// never be made ready again; cancelled waiters leave only weak references.
#[derive(Debug)]
pub(crate) struct ChildCpuPublication(Mutex<ChildCpuPublicationState>);

#[derive(Debug)]
enum ChildCpuPublicationState {
    Pending(Vec<std::sync::Weak<futures::task::AtomicWaker>>),
    Published,
    Invalidated,
}

impl ChildCpuPublication {
    fn pending() -> Arc<Self> {
        Arc::new(Self(Mutex::new(ChildCpuPublicationState::Pending(
            Vec::new(),
        ))))
    }

    fn finish(&self, published: bool) {
        let waiters = {
            let mut state = self.0.lock().expect("child CPU publication mutex poisoned");
            if matches!(*state, ChildCpuPublicationState::Invalidated)
                || published && matches!(*state, ChildCpuPublicationState::Published)
            {
                return;
            }
            let next = if published {
                ChildCpuPublicationState::Published
            } else {
                ChildCpuPublicationState::Invalidated
            };
            match std::mem::replace(&mut *state, next) {
                ChildCpuPublicationState::Pending(waiters) => waiters,
                _ => Vec::new(),
            }
        };
        // Do not call an arbitrary task waker under either accounting lock.
        for waiter in waiters.into_iter().filter_map(|waiter| waiter.upgrade()) {
            waiter.wake();
        }
    }

    async fn wait(&self) -> bool {
        let waiter = Arc::new(futures::task::AtomicWaker::new());
        {
            let mut state = self.0.lock().expect("child CPU publication mutex poisoned");
            match &mut *state {
                ChildCpuPublicationState::Pending(waiters) => {
                    waiters.retain(|waiter| waiter.strong_count() != 0);
                    waiters.push(Arc::downgrade(&waiter));
                }
                ChildCpuPublicationState::Published => return true,
                ChildCpuPublicationState::Invalidated => return false,
            }
        }
        futures::future::poll_fn(|cx| {
            waiter.register(cx.waker());
            let state = self.0.lock().expect("child CPU publication mutex poisoned");
            match *state {
                ChildCpuPublicationState::Pending(_) => std::task::Poll::Pending,
                ChildCpuPublicationState::Published => std::task::Poll::Ready(true),
                ChildCpuPublicationState::Invalidated => std::task::Poll::Ready(false),
            }
        })
        .await
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct ProcessCpuTime {
    snapshot: ProcessCpuSnapshot,
    exited_children: BTreeMap<DetPid, ProcessCpuSnapshot>,
    /// Each thread's own (user, system) time as of its last accounting, which
    /// another thread's CPU clock of it reads (`crate::cpu_clock`). An entry
    /// leaves once its thread has exited and left the scheduler, unless the
    /// thread leads the process: a leader that exits before its threads stays
    /// a zombie whose clock still reads its final time, as on Linux.
    #[serde(default)]
    threads: BTreeMap<DetTid, (LogicalTime, LogicalTime)>,
    // Runtime ownership, not record-format state. ThreadState clones retain the
    // same parent accounting Arc; notification tokens also retain their identity.
    #[serde(skip)]
    child_cpu_publications: BTreeMap<DetPid, Arc<ChildCpuPublication>>,
}

impl ProcessCpuTime {
    fn validate_native_exec_cut(&self) -> Result<(), crate::native_exec::NativeExecCutError> {
        if !self.child_cpu_publications.is_empty() {
            return Err(crate::native_exec::NativeExecCutError::LiveCapability(
                "child_cpu_publications",
            ));
        }
        Ok(())
    }

    fn add_thread_delta(&mut self, user: LogicalTime, system: LogicalTime) {
        self.snapshot.user = self.snapshot.user + user;
        self.snapshot.system = self.snapshot.system + system;
    }

    /// Record `thread`'s own (user, system) time, which its process total now
    /// includes.
    fn record_thread(&mut self, thread: DetTid, own: (LogicalTime, LogicalTime)) {
        self.threads.insert(thread, own);
    }

    /// `thread`'s own (user, system) time as of its last accounting. A thread
    /// that has not been accounted yet has not run, and has used none.
    fn thread_time(&self, thread: DetTid) -> (LogicalTime, LogicalTime) {
        self.threads
            .get(&thread)
            .copied()
            .unwrap_or((LogicalTime::ZERO, LogicalTime::ZERO))
    }

    /// `thread` exited: its time stays in the process total, as an exited
    /// thread's stays in Linux's (`thread_group_cputime`).
    fn forget_thread(&mut self, thread: DetTid) {
        self.threads.remove(&thread);
    }

    fn record_exited_child(&mut self, pid: DetPid, child: ProcessCpuSnapshot) {
        self.exited_children
            .entry(pid)
            .and_modify(|previous| {
                previous.user = previous.user.max(child.user);
                previous.system = previous.system.max(child.system);
                previous.children_user = previous.children_user.max(child.children_user);
                previous.children_system = previous.children_system.max(child.children_system);
            })
            .or_insert(child);
    }

    fn reap_child(&mut self, pid: DetPid) {
        let Some(child) = self.exited_children.remove(&pid) else {
            return;
        };
        self.snapshot.children_user =
            self.snapshot.children_user + child.user + child.children_user;
        self.snapshot.children_system =
            self.snapshot.children_system + child.system + child.children_system;
    }

    fn prepare_child(&mut self, pid: DetPid) {
        self.exited_children.remove(&pid);
    }
}

/// Guest-visible logical clock shared by an entire process tree.
///
/// The scheduler's raw logical time already includes the configured epoch and
/// is the clock used to judge absolute deadlines. Guest time must therefore
/// track it directly: subtracting a per-process or per-exec origin can put a
/// newly computed absolute deadline in the scheduler's past. The shared floor
/// preserves monotonicity if a backend supplies a stale local observation.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct GuestClock {
    now: LogicalTime,
}

/// One modeled robust-futex wake that must wait until Linux has completed the
/// corresponding task-exit cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RobustListWake {
    pub(crate) futex: FutexID,
}

/// The physical-exit condition under which staged robust-futex wakes become
/// safe to deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RobustListExit {
    ExitGroup,
    Signal(i32),
}

/// Wakes collected while guest memory is still readable, keyed by the owner
/// whose physical exit makes them safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingRobustListWakes {
    pub(crate) reason: RobustListExit,
    pub(crate) wakes: Vec<RobustListWake>,
}

impl PendingRobustListWakes {
    fn matches_exit(&self, exit_signal: Option<i32>) -> bool {
        match self.reason {
            RobustListExit::ExitGroup => true,
            RobustListExit::Signal(expected) => exit_signal == Some(expected),
        }
    }
}

/// How long a thread an exit_group killed waits for its robust-list walk
/// ([`ThreadState::wait_for_exit_group_robust_walk`]).
pub(crate) const EXIT_GROUP_ROBUST_WALK_WAIT: std::time::Duration =
    std::time::Duration::from_secs(10);

/// A registered robust list: its owner and head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RobustListOwner {
    pub(crate) dettid: DetTid,
    pub(crate) head: usize,
}

/// The robust-list walk an exec still owes, where Detcore makes the walk
/// itself (`OwnerDeathPath::Detcore`): set at the exec's entry
/// (`Detcore::handle_execveat`) and taken where the exec can no longer fail
/// (`Tool::handle_exec_commit`, `Detcore::commit_exec_robust_lists`).
/// The exec's entry has already given the thread the new image's state by
/// then, so this keeps what the walk needs from the old image: every
/// registration to walk, the caller's included, and the shared-mapping model
/// and address-space identity that name the futex keys of the walk's wakes.
#[derive(Clone)]
pub(crate) struct ExecRobustCommit {
    pub(crate) owners: Vec<RobustListOwner>,
    pub(crate) memory_metadata: Arc<Mutex<MemoryMetadata>>,
    pub(crate) mm_id: MmId,
}

/// Robust-list registrations and staged wakes shared by one Linux thread
/// group. A forked process starts with an empty instance; `CLONE_THREAD`
/// members share it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct RobustListProcessState {
    heads: BTreeMap<DetTid, usize>,
    /// Threads whose own exit Detcore has already walked on DBT or KVM
    /// ([`crate::syscalls::threads`]'s `OwnerDeathPath::Barrier` and
    /// `OwnerDeathPath::Detcore`).
    #[serde(default)]
    tool_walked: BTreeSet<DetTid>,
    /// An exit_group on DBT or KVM (`OwnerDeathPath::Barrier`,
    /// `OwnerDeathPath::Detcore`) has been filed: an exiting thread keeps its
    /// head here for that exit_group's walk
    /// ([`ThreadState::drop_robust_list_head_at_exit`]).
    #[serde(default)]
    exit_group_filed: bool,
    /// The threads whose filed exit_group is still pending: filed, and
    /// neither granted nor cancelled by the thread's own death.
    #[serde(default)]
    exit_group_filers: BTreeSet<DetTid>,
    /// The thread whose filed exit_group was granted and is walking the
    /// group's lists ([`ThreadState::begin_exit_group_robust_walk`]).
    #[serde(default)]
    exit_group_walker: Option<DetTid>,
    /// That exit_group's walk has finished
    /// ([`ThreadState::finish_exit_group_robust_walk`]).
    #[serde(default)]
    exit_group_walked: bool,
    pending: BTreeMap<DetTid, PendingRobustListWakes>,
    matched_physical_exits: BTreeMap<DetTid, DetTime>,
    /// Wakes read at an `execve`'s entry, delivered only if it succeeds.
    exec_pending: Vec<(DetTid, RobustListWake)>,
}

impl GuestClock {
    fn observe(&mut self, raw: LogicalTime) -> LogicalTime {
        self.now = self.now.max(raw);
        self.now
    }
}

/// Linux's initial per-task timer slack, in nanoseconds.
///
/// Detcore uses the kernel default rather than inheriting the launcher's
/// physical timer slack.  Guest writes are kept in virtual per-thread state so
/// they cannot perturb the host-timed waits Detcore still uses internally.
pub(crate) const DEFAULT_TIMER_SLACK_NS: u64 = 50_000;

fn default_timer_slack_ns() -> u64 {
    DEFAULT_TIMER_SLACK_NS
}

/// The most deliveries in a row that continue their thread's own
/// `rt_sigreturn` (`ThreadState::consecutive_continuations`). A chosen,
/// bounded preference budget: as many as Linux has standard signals, so any
/// set of distinct standard signals pending when a run begins is delivered
/// before other threads run. A cap of 1 cut short a second pending signal
/// and restarted the cycle <https://github.com/rrnewton/hermit/issues/4505>
/// fixes (Claude re-check of https://github.com/rrnewton/hermit/pull/4509).
/// It is not a promise that every backlog drains first: realtime signals
/// queue several instances and a periodic timer can re-arm during a run
/// (v6.12 kernel/signal.c:1070 and :695). Reaching the cap only drops the
/// preference; no signal is lost, and the next delivery waits its turn.
pub(crate) const MAX_SIGRETURN_CONTINUATIONS: u8 = 32;

/// What a signal delivery knows before anything of it can hand the thread
/// to the scheduler (`Detcore::inbound_stop`).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct InboundStop {
    /// The stop follows a reported `rt_sigreturn` (`returned_from_sigreturn`).
    pub(crate) reported: bool,
    /// The thread's scheduler grants at the stop.
    pub(crate) grants_at_stop: u64,
}

/// What a delivery's turn leaves for what the granted delivery settles
/// (`Detcore::inbound_turn`, `Detcore::inbound_delivered`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct InboundTurn {
    /// The delivering thread.
    pub(crate) dettid: DetTid,
    /// The thread's signal count before this delivery.
    pub(crate) mycount: u64,
    /// Actual callback identity retained through an announcement's await.
    #[serde(default)]
    pub(crate) boundary_token: Option<crate::preemptions::callback_history::Token>,
}

/// One delivery announced to `Tool::announce_signal_event` and not yet
/// settled.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SignalAnnouncementState {
    /// The announcement's token.
    pub(crate) token: u64,
    /// The raw signal.
    pub(crate) signal: i32,
    /// What its turn left for its commit.
    pub(crate) turn: InboundTurn,
}

/// The guest tasks of a run that are live or not yet reaped, and the clones
/// still making one: every task of every guest process, which the binder rule
/// reads (`Detcore::binder_other_tasks`). A binder call runs off the guest's
/// turn alone only while its caller is the run's one task.
///
/// One record is shared by every task of the run, as the guest clock is. A
/// task joins it at the entry of the clone that makes it (or, where a backend
/// runs the clone natively, at that backend's report of it,
/// `Detcore::register_external_child`), and leaves it at the wait that reaps
/// its process, or, for a thread that ends by its own `exit` without leading
/// its process, at the commit of that exit's grant in its own turn, after
/// which a sequentialized thread never runs again (on a backend that reports
/// the exit complete afterwards). Every change is at a logical event in
/// Hermit's turn order, never at a host-timed report, so a run and its verify
/// pass find the same tasks; the report only delays an off-turn call until
/// the kernel's last writes for the retired thread are done
/// (`LiveTasks::unreported`). While a caller finds itself the only task, no
/// other task exists to change the record, so the answer cannot change under
/// it; while it finds others, each of them is live, unreaped or being made,
/// so it can only be too many.
#[derive(Debug, Default)]
pub(crate) struct LiveTasks {
    /// Each task, by thread ID, with its process, from the return of the
    /// clone that made it, or the run's start for its first task, until the
    /// wait that reaps its process; a thread that does not lead its process
    /// and ends by its own `exit` leaves at the commit of that exit's grant
    /// (`LiveTasks::commit_thread_exit`). A thread that an exec, an
    /// `exit_group` or a signal ends, and a leader that exits while its
    /// threads run on, a zombie, stay until the reap.
    tasks: BTreeMap<DetTid, DetPid>,
    /// How many clones each task has entered without returning: each may
    /// have made a child that already runs, as a vfork's child does while its
    /// parent waits, which `tasks` holds only once its clone returns. A clone
    /// whose caller dies before it returns stays counted.
    creating: BTreeMap<DetTid, usize>,
    /// The tasks in `tasks` whose own `exit`, or their process's
    /// `exit_group`, was granted: still counted, and named as exited in a
    /// refusal. Only names; never shortens the count.
    exited: BTreeSet<DetTid>,
    /// The threads retired at the commit of their own `exit`
    /// (`LiveTasks::commit_thread_exit`), each with its process, whose exit
    /// the backend has not yet reported complete. Until it has, the kernel may
    /// still mark the futex words of the thread's robust list and store 0 to
    /// its `clear_child_tid` word (`mm_release`), anywhere in the memory it
    /// shared. They no longer count, which was decided in turn order, but an
    /// off-turn binder call is issued only once none is left
    /// (`Detcore::run_binder_call_with`), so those writes are done before the call
    /// can read or write a buffer. The report is Detcore's `on_exit_thread`,
    /// which a backend whose kernel runs the exit
    /// (`BackendCapabilities::runs_exit_robust_list`) calls only after the
    /// thread's final exit status; a wait that reaps the thread's process
    /// forgets its entry, since its exit is then complete whether or not a
    /// report came.
    unreported: BTreeMap<DetTid, DetPid>,
    /// Set where this record was not carried here: a thread state read back
    /// from its serialized form, which does not hold it. Then which tasks
    /// are live is unknown, and no task is taken to be alone.
    unknown: bool,
    /// Set once a backend reported a task it cloned natively without first
    /// announcing the clone (`LiveTasks::end_reported_clone`): the child may
    /// have run unnamed, so from then on which tasks are live is unknown,
    /// and no task is taken to be alone.
    unannounced_clone: bool,
}

impl LiveTasks {
    /// The record of a run whose first task, `tid`, leads process `pid`.
    pub(crate) fn of_first_task(tid: DetTid, pid: DetPid) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            tasks: BTreeMap::from([(tid, pid)]),
            ..Self::default()
        }))
    }

    /// A record that knows no task, for a thread state read back from its
    /// serialized form (`LiveTasks::unknown`).
    pub(crate) fn unknown() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            unknown: true,
            ..Self::default()
        }))
    }

    /// `creator` enters a clone, before the child can exist.
    pub(crate) fn begin_clone(&mut self, creator: DetTid) {
        *self.creating.entry(creator).or_default() += 1;
    }

    /// `creator`'s backend reported `child`, of process `pid`, a task it made
    /// by running the clone natively (`Detcore::register_external_child`).
    /// Where the backend counted that clone from its native entry
    /// (`ThreadState::begin_native_clone`), this ends it and names the child.
    /// Where it did not, the child may have run unnamed before this report,
    /// so which tasks are live is unknown from here (`Self::unknown`) and no
    /// task is taken to be alone again: an application backend that runs
    /// native clones calls `begin_native_clone` before each one.
    pub(crate) fn end_reported_clone(&mut self, creator: DetTid, child: DetTid, pid: DetPid) {
        if !self.creating.contains_key(&creator) {
            self.unannounced_clone = true;
        }
        self.end_clone(creator, Some((child, pid)));
    }

    /// `creator`'s clone returned, having made `child` (its thread ID and
    /// process) or nothing.
    pub(crate) fn end_clone(&mut self, creator: DetTid, child: Option<(DetTid, DetPid)>) {
        if let std::collections::btree_map::Entry::Occupied(mut entry) =
            self.creating.entry(creator)
        {
            *entry.get_mut() -= 1;
            if *entry.get() == 0 {
                entry.remove();
            }
        }
        if let Some((tid, pid)) = child {
            self.tasks.insert(tid, pid);
        }
    }

    /// A wait reaped process `pid`: none of its tasks is left.
    pub(crate) fn reaped(&mut self, pid: DetPid) {
        let exited = &mut self.exited;
        self.tasks.retain(|tid, process| {
            let kept = *process != pid;
            if !kept {
                exited.remove(tid);
            }
            kept
        });
        // A reaped process's threads have all finished exiting, reported or
        // not, so none of them can hold a binder call back any longer.
        self.unreported.retain(|_, process| *process != pid);
    }

    /// Task `tid` had its own `exit`, or its process's `exit_group`, granted.
    /// It still counts, named as exited, until the grant is committed
    /// (`Self::commit_thread_exit`) or a wait reaps its process.
    pub(crate) fn note_exit(&mut self, tid: DetTid) {
        if self.tasks.contains_key(&tid) {
            self.exited.insert(tid);
        }
    }

    /// Task `tid`, of process `pid`, committed the grant of its own `exit`
    /// in its turn, the last thing it does there: under sequentialized
    /// threads it never runs again. A thread that does not lead its process
    /// leaves the record here, its exit awaiting the backend's report
    /// (`Self::unreported`). A leader stays, as the zombie it is, until a
    /// wait reaps its process. Returns whether the thread left.
    pub(crate) fn commit_thread_exit(&mut self, tid: DetTid, pid: DetPid) -> bool {
        if tid.as_raw() == pid.as_raw() || !self.exited.remove(&tid) {
            return false;
        }
        self.tasks.remove(&tid);
        self.unreported.insert(tid, pid);
        true
    }

    /// The backend reported task `tid`'s exit complete: the kernel has done
    /// every write it makes for it (`Self::unreported`).
    pub(crate) fn exit_reported(&mut self, tid: DetTid) {
        self.unreported.remove(&tid);
    }

    /// Whether a thread retired at its exit's commit still awaits its
    /// backend's report (`Self::unreported`).
    #[cfg(test)]
    pub(crate) fn exits_unreported(&self) -> bool {
        !self.unreported.is_empty()
    }

    /// A thread retired at its exit's commit whose report is still awaited,
    /// the lowest-numbered, to name in a refusal.
    pub(crate) fn first_unreported(&self) -> Option<DetTid> {
        self.unreported.keys().next().copied()
    }

    /// Process `pid`'s `exit_group` was granted, which ends each of its
    /// tasks; they stay counted until a wait reaps it, as `note_exit`.
    pub(crate) fn note_process_exit(&mut self, pid: DetPid) {
        for (tid, process) in &self.tasks {
            if *process == pid {
                self.exited.insert(*tid);
            }
        }
    }

    /// The tasks besides `caller`, named for a refusal, or `None` where
    /// `caller` is the run's only task.
    pub(crate) fn others(&self, caller: DetTid) -> Option<String> {
        if self.unknown {
            return Some(
                "which tasks are live is unknown (this thread's state was read back from its \
                 serialized form)"
                    .to_string(),
            );
        }
        if self.unannounced_clone {
            return Some(
                "which tasks are live is unknown (a backend reported a task it cloned natively \
                 without announcing the clone first)"
                    .to_string(),
            );
        }
        let mut named: Vec<String> = self
            .tasks
            .iter()
            .filter(|(tid, _)| **tid != caller)
            .map(|(tid, pid)| {
                let task = if tid.as_raw() == pid.as_raw() {
                    format!("task {tid}")
                } else {
                    format!("task {tid} of process {pid}")
                };
                if self.exited.contains(tid) {
                    format!("{task}, which exited and is not reaped yet")
                } else {
                    task
                }
            })
            .collect();
        named.extend(self.creating.iter().map(|(creator, clones)| {
            if *clones == 1 {
                format!("a task being created by task {creator}")
            } else {
                format!("{clones} tasks being created by task {creator}")
            }
        }));
        if named.is_empty() && self.tasks.contains_key(&caller) {
            return None;
        }
        if named.is_empty() {
            named.push("tasks whose number this record does not hold".to_string());
        }
        const SHOWN: usize = 4;
        let more = named.len().saturating_sub(SHOWN);
        named.truncate(SHOWN);
        let mut list = named.join(", ");
        if more > 0 {
            list.push_str(&format!(" and {more} more"));
        }
        Some(list)
    }
}

#[cfg(test)]
mod live_tasks_tests {
    use super::*;

    /// A task counts among the run's tasks from the entry of the clone that
    /// makes it, before it exists, until a wait reaps its process: an exited
    /// task, and an exited thread of the caller's own process, stay counted
    /// and are named as exited; a failed clone counts only while it runs. A
    /// caller the record does not hold, or a record read back from its
    /// serialized form, is never alone.
    #[test]
    fn a_task_counts_from_its_clone_s_entry_until_a_wait_reaps_its_process() {
        let one = DetTid::from_raw(1);
        let record = LiveTasks::of_first_task(one, one);
        let mut tasks = record.lock().unwrap();
        assert_eq!(tasks.others(one), None);
        tasks.begin_clone(one);
        assert_eq!(
            tasks.others(one).as_deref(),
            Some("a task being created by task 1")
        );
        tasks.end_clone(one, None);
        assert_eq!(tasks.others(one), None);
        tasks.begin_clone(one);
        tasks.end_clone(one, Some((DetTid::from_raw(2), DetPid::from_raw(2))));
        assert_eq!(tasks.others(one).as_deref(), Some("task 2"));
        tasks.note_process_exit(DetPid::from_raw(2));
        assert_eq!(
            tasks.others(one).as_deref(),
            Some("task 2, which exited and is not reaped yet")
        );
        tasks.reaped(DetPid::from_raw(2));
        assert_eq!(tasks.others(one), None);
        tasks.begin_clone(one);
        tasks.end_clone(one, Some((DetTid::from_raw(3), one)));
        tasks.note_exit(DetTid::from_raw(3));
        assert_eq!(
            tasks.others(one).as_deref(),
            Some("task 3 of process 1, which exited and is not reaped yet")
        );
        assert!(tasks.others(DetTid::from_raw(9)).is_some());
        drop(tasks);
        assert!(LiveTasks::unknown().lock().unwrap().others(one).is_some());
    }

    /// A thread that does not lead its process and leaves by its own `exit`
    /// counts while its exit is granted but not committed, and not once the
    /// grant is committed, though its exit then awaits the backend's report
    /// until it comes: a process that joined its helper is alone again. A
    /// leader is never retired at its exit, and a process that exited counts
    /// until a wait reaps it.
    #[test]
    fn a_thread_leaves_at_its_exit_s_commit_and_a_zombie_at_its_reap() {
        let one = DetTid::from_raw(1);
        let record = LiveTasks::of_first_task(one, one);
        let mut tasks = record.lock().unwrap();
        tasks.begin_clone(one);
        tasks.end_clone(one, Some((DetTid::from_raw(2), one)));
        // A commit with no grant retires nothing.
        assert!(!tasks.commit_thread_exit(DetTid::from_raw(2), one));
        tasks.note_exit(DetTid::from_raw(2));
        assert_eq!(
            tasks.others(one).as_deref(),
            Some("task 2 of process 1, which exited and is not reaped yet")
        );
        assert!(tasks.commit_thread_exit(DetTid::from_raw(2), one));
        assert_eq!(tasks.others(one), None);
        assert!(tasks.exits_unreported());
        tasks.exit_reported(DetTid::from_raw(2));
        assert!(!tasks.exits_unreported());
        // A leader whose threads run on stays a zombie until the reap.
        tasks.note_exit(one);
        assert!(!tasks.commit_thread_exit(one, one));
        // A child process that exited counts until a wait reaps it.
        tasks.begin_clone(one);
        tasks.end_clone(one, Some((DetTid::from_raw(3), DetTid::from_raw(3))));
        tasks.note_exit(DetTid::from_raw(3));
        assert!(!tasks.commit_thread_exit(DetTid::from_raw(3), DetTid::from_raw(3)));
        assert!(
            tasks
                .others(DetTid::from_raw(9))
                .is_some_and(|others| others.contains("task 3, which exited"))
        );
        tasks.reaped(DetTid::from_raw(3));
        assert!(
            !tasks
                .others(DetTid::from_raw(9))
                .is_some_and(|others| others.contains("task 3"))
        );
    }

    /// A wait that reaps a process forgets the unreported exits of its
    /// threads retired at their exit's commit: they have all finished, so a
    /// report that never arrives for them cannot hold back a later binder
    /// call. Another process's unreported exit is kept.
    #[test]
    fn a_reaped_process_forgets_its_threads_unreported_exits() {
        let one = DetTid::from_raw(1);
        let record = LiveTasks::of_first_task(one, one);
        let mut tasks = record.lock().unwrap();
        // Process 3, and thread 5 of it; thread 2 of process 1.
        tasks.begin_clone(one);
        tasks.end_clone(one, Some((DetTid::from_raw(3), DetTid::from_raw(3))));
        tasks.begin_clone(DetTid::from_raw(3));
        tasks.end_clone(
            DetTid::from_raw(3),
            Some((DetTid::from_raw(5), DetTid::from_raw(3))),
        );
        tasks.begin_clone(one);
        tasks.end_clone(one, Some((DetTid::from_raw(2), one)));
        for (thread, process) in [(5, 3), (2, 1)] {
            tasks.note_exit(DetTid::from_raw(thread));
            assert!(tasks.commit_thread_exit(DetTid::from_raw(thread), DetTid::from_raw(process)));
        }
        assert_eq!(tasks.first_unreported(), Some(DetTid::from_raw(2)));
        tasks.reaped(DetTid::from_raw(3));
        assert_eq!(tasks.first_unreported(), Some(DetTid::from_raw(2)));
        tasks.exit_reported(DetTid::from_raw(2));
        assert!(!tasks.exits_unreported());
        // Reaped first, then reported: the report finds nothing to clear.
        tasks.begin_clone(one);
        tasks.end_clone(one, Some((DetTid::from_raw(6), DetTid::from_raw(6))));
        tasks.begin_clone(DetTid::from_raw(6));
        tasks.end_clone(
            DetTid::from_raw(6),
            Some((DetTid::from_raw(7), DetTid::from_raw(6))),
        );
        tasks.note_exit(DetTid::from_raw(7));
        assert!(tasks.commit_thread_exit(DetTid::from_raw(7), DetTid::from_raw(6)));
        tasks.reaped(DetTid::from_raw(6));
        assert!(!tasks.exits_unreported());
        tasks.exit_reported(DetTid::from_raw(7));
        assert!(!tasks.exits_unreported());
    }
}

/// The Detcore per-thread state.
#[derive(Serialize, Deserialize, Clone)]
pub struct ThreadState<T> {
    /// The deterministic thread ID of the this thread.
    pub dettid: DetTid,
    /// The deterministic process ID of the this thread.
    pub detpid: Option<DetTid>,

    /// Whether the backend entered the thread-start callback. Construction
    /// alone does not imply that this thread was admitted to the scheduler.
    pub(crate) thread_start_entered: bool,

    /// The parent's `BlockingVfork` operation, if a `CLONE_VFORK` call
    /// created this thread. A vfork child that is torn down before its
    /// scheduler registration completes reports it in its deregistration, so
    /// the barrier of exactly that vfork ends
    /// ([`crate::scheduler::Scheduler::record_unregistered_vfork_child_exit`]).
    pub(crate) vfork_origin: Option<ExternalOpId>,

    /// Host thread ID supplied by a backend whose scheduler identity is
    /// virtual. `None` keeps the existing direct-ID behavior for other
    /// backends.
    #[serde(default)]
    pub physical_tid: Option<i32>,

    /// Backend lifetime retained for consuming signal effects after retirement.
    #[serde(default)]
    pub(crate) signal_task_identity: Option<reverie::SignalTaskIdentity>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1063): Review backend-supplied open-file creator identity.
    /// Stable identity used when allocating deterministic open-file descriptions.
    #[serde(default)]
    pub(crate) open_file_creator: Option<DetTid>,

    /// The input geometry of the `recvmsg` or `recvmmsg` this thread is
    /// running, while Detcore needs it to name the descriptors it receives:
    /// `Some` from the handler's entry, re-read before each attempt of the
    /// internal polling loop so that it is the geometry of the attempt that
    /// succeeds (`retry_nonblocking_syscall_helper`), taken when the call
    /// returns. Never carried across a fork or a migration.
    #[serde(skip)]
    pub(crate) receive_geometry: Option<Option<crate::syscalls::scm_rights::ReceiveGeometry>>,

    /// The open file description a `flock` this thread is waiting in took at
    /// its entry, as Linux's call holds the `struct file` its descriptor
    /// named then (`fdget` in `sys_flock`): `Some` while the call waits in
    /// Detcore's flock model, so that its grant is mirrored on that
    /// description and noted in its cache even when a sibling closed or
    /// replaced the descriptor meanwhile (`Detcore::flock_wait`). Never
    /// carried across a fork or a migration.
    #[serde(skip)]
    pub(crate) flock_waiting_description: Option<crate::fd::DetFd>,

    /// Retained metadata for the vectored call's observers. Original entry
    /// regions preserve the existing outbound and exit-dependency overlays.
    /// When buffer logging is observed, generic readv/recvmsg also retain a
    /// separate complete-geometry result from the successful actual attempt;
    /// their buffer observer hashes returned guest bytes without an overlay.
    /// Set as internal polling ends and taken as the call returns. A failed
    /// final capture cannot certify an earlier attempt's geometry.
    #[serde(skip)]
    pub(crate) entry_import: Option<Box<crate::syscalls::entry_import::EntryImport>>,

    /// The `sendmsg` or `sendmmsg` this thread is running, while its
    /// `SCM_RIGHTS` descriptors matter to the `/proc` directories Detcore
    /// serves on placeholders: `Some` from the handler's inspection
    /// (`Detcore::inspect_placeholder_send`), read before each attempt of the
    /// internal polling loop, which refuses by name an attempt whose messages
    /// pass other placeholders than were inspected
    /// (`retry_nonblocking_syscall_helper`), taken when the call returns.
    /// Never carried across a fork or a migration.
    #[serde(skip)]
    pub(crate) send_inspection: Option<crate::syscalls::SendInspection>,
    /// The inotify read this thread is running, while the bytes queued on
    /// its instance before the attempt that completes say what that attempt
    /// took (`crate::syscalls::inotify::QueueCount`): `Some` from the read
    /// handler's entry, measured again before each attempt of the internal
    /// polling loop (`retry_nonblocking_syscall_helper`), so that it is the
    /// count before the attempt that completed, and taken when the call
    /// returns. Never carried across a fork or a migration.
    #[serde(skip)]
    pub(crate) inotify_queue: Option<crate::syscalls::inotify::QueueCount>,
    /// The call this thread runs in parts (a readv reading on into a later
    /// iovec, `Detcore::readv_inotify`), set around the whole call: whether
    /// the part now running follows one that transferred bytes, and a signal
    /// an earlier part completed with pending
    /// (`crate::syscalls::helpers::CallParts`). Never carried across a fork
    /// or a migration.
    #[serde(skip)]
    pub(crate) call_parts: Option<crate::syscalls::helpers::CallParts>,
    /// The syscall in progress, while the run records an access trace: the
    /// descriptors it named and the slot changes it made
    /// (`crate::access_trace`). Never carried across a fork or a migration.
    #[serde(skip)]
    pub(crate) traced_call: crate::access_trace::TracedCall,

    /// The thread's hardware watchpoints (`--watch`, `crate::watch`). Never
    /// carried to another thread: debug registers are not inherited.
    #[serde(skip)]
    pub(crate) watch: crate::watch::WatchThread,

    /// Linux memory address space shared by tasks created with `CLONE_VM`.
    pub mm_id: MmId,

    /// Last acknowledged ownership publication. A new mm or changed provider
    /// result is published before ordinary RPC can transfer the turn.
    #[serde(skip)]
    pub(crate) published_runtime_private_memory: Option<(MmId, reverie::RuntimePrivateMemory)>,

    /// The CPU accounting this thread last published to the global state
    /// (`tool_global::GlobalRequest::WithCpuTime`), which its requests carry
    /// again only once it has changed.
    #[serde(skip)]
    pub(crate) published_cpu: Option<crate::tool_global::CpuPublication>,

    /// Whether this thread has made sure, at a clone, that the global state
    /// knows its process's guest-visible parent
    /// (`Detcore::record_external_parent_at_clone`).
    #[serde(default)]
    pub(crate) external_parent_checked: bool,

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

    /// Registration metadata for a child whose parent cannot resume until the
    /// backend finishes the child. The child consumes this in
    /// `handle_thread_start`; the parent clears its copy when injection returns.
    pub pending_vfork: Option<PendingVfork>,

    /// The ID this thread's restarted clone-family call took when Detcore
    /// chooses the IDs (`GuestIds::Chosen`), and the call it is for: the call
    /// comes back after the signal that interrupted it, and keeps its ID
    /// (`Detcore::plan_clone_id`). Any other call retires it.
    pub(crate) restarted_clone_id: Option<crate::syscalls::chosen_ids::RestartedCloneId>,

    /// The open file description whose record sequence this thread took from
    /// Detcore's global state for the read, positioned read or seek it is
    /// handling, which stores it back when it ends
    /// (`Detcore::take_procfs_sequence`); `None` outside one. The handler it
    /// runs again sees it and takes nothing more.
    #[serde(skip)]
    pub(crate) procfs_sequence_taken: Option<OpenFileId>,

    /// Shared file metadata among all threads in the same process.
    /// Initialized for new threads (shared or fresh), and then overwritten again on `execve`.
    pub file_metadata: Arc<Mutex<FileMetadata>>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review backend-gated live descriptor discovery state.
    /// Whether the tool runs in the guest's process and shares its descriptor
    /// table, so a descriptor detcore has not seen yet may be inspected live
    /// (copied from `BackendCapabilities::tool_shares_guest_descriptor_table`).
    #[serde(default)]
    pub(crate) tool_shares_guest_descriptor_table: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-2150): Review the per-thread timer-slack state
    // and the creator-current-as-child-default inheritance rule.
    /// Virtualized Linux timer slack for this thread.  Both prctl(2) and the
    /// top-level `/proc/<tid>/timerslack_ns` interface use these fields; the
    /// physical tracee value remains untouched.
    #[serde(default = "default_timer_slack_ns")]
    pub(crate) timer_slack_ns: u64,
    /// Value restored by a zero timer-slack write.  Linux gives a new task the
    /// creating thread's current value as both its current and its default.
    #[serde(default = "default_timer_slack_ns")]
    pub(crate) default_timer_slack_ns: u64,

    /// This thread lives below the container's PID namespace: it, or an
    /// ancestor, was created with `CLONE_NEWPID`. Its pids are those of its
    /// own namespace, which Detcore translates
    /// (`scheduler::pid_namespaces`); at depth 0 nothing is asked.
    #[serde(default)]
    pub(crate) nested_pid_namespace: bool,

    /// This task's root directory may not be the container's: a task it shares
    /// its root with (`CLONE_FS`), or one it inherited the root from, changed
    /// it with `chroot`. Detcore then leaves whether a procfs file exists to
    /// the kernel (`rendered_procfs`). Shared like the root itself: every task
    /// that shares the creator's filesystem information shares this flag.
    #[serde(default)]
    pub(crate) changed_root: Arc<std::sync::atomic::AtomicBool>,

    /// This thread's working and root directories may not be its
    /// thread-group leader's: it, or the thread it was created by, was
    /// created as a thread without `CLONE_FS`, so it holds filesystem
    /// information of its own. Its process's `/proc/<pid>/cwd` and
    /// `/proc/<pid>/root` links, which Linux reads from the leader, are then
    /// not the thread's own `getcwd` (`Detcore::readlink_own_task_link`). A
    /// leader, a new process, and a thread after its exec (which makes it
    /// the leader) hold their own.
    #[serde(default)]
    pub(crate) fs_apart_from_leader: bool,

    /// The image this thread's process executes ([`crate::exec_record`]),
    /// which answers the process's `/proc/<pid>/exe` link on KVM while it
    /// names the image exactly (`Detcore::readlink_own_task_link`), and in a
    /// launched run (`Detcore::launched_exe`); `None` leaves the link to the
    /// path it took before. A new task starts with its creator's, and a
    /// successful exec replaces it (`tool_global::mark_past_first_execve`).
    #[serde(default)]
    pub(crate) exe: Option<Arc<crate::exec_record::ExecRecord>>,

    /// The effective user and group ids Linux gives Detcore's virtual root at
    /// the exec this thread has in flight, as Detcore decided them from the
    /// image's set-id bits before the exec (`Detcore::exec_effective_ids`);
    /// anything but (0, 0) is an exec Linux runs in secure-execution mode.
    /// Taken at the new image's first callback, which compares it with the
    /// image's own `AT_SECURE` (`crate::secure_exec::outcome`) and applies
    /// `commit_creds`'s rule (`crate::commit_creds`), and dropped when the
    /// exec fails. `None` where nothing decided it.
    #[serde(skip)]
    pub(crate) exec_effective_ids: Option<(u32, u32)>,

    /// This thread's permitted capability set when its exec in flight
    /// started, read with an injected `capget` only where Detcore then held a
    /// parent-death signal for it (`holds_parent_death_signal`), the one thing
    /// `commit_creds`'s comparison at the new image's first callback can
    /// clear. Dropped when the exec fails.
    #[serde(skip)]
    pub(crate) exec_permitted_before: Option<u64>,

    /// Whether Detcore holds a nonzero parent-death signal for this thread,
    /// as the scheduler last answered it (`tool_global::parent_death_signal`,
    /// through which every change to it goes). Only the thread itself changes
    /// its own value (Linux's `pdeath_signal` is per task: arming, a
    /// credential change and an exec are all the task's own acts, and a new
    /// task starts with none), so the answer stays current between calls.
    #[serde(skip)]
    pub(crate) holds_parent_death_signal: bool,

    /// The identity of the descriptor table this thread's process holds:
    /// the one its thread-group leader holds now, which procfs reads for
    /// the process's descriptor links (`/proc/self/fd/N`, `/proc/<pid>/fd/N`,
    /// `/dev/fd/N`). A new process sets it to the table it starts with (its
    /// creator's under `CLONE_FILES`, a copy otherwise), and an exec to the
    /// table exec gives it; `unshare` is refused, so a leader's table
    /// changes at nothing else. A thread (`CLONE_THREAD`) copies its
    /// creator's. A plain copy suffices: it changes only where the group has
    /// one thread (a new process, a successful exec), and a failed exec
    /// restores the exec'ing thread's own copy. `None` where it is not
    /// known, which only a state serialized before this field restores.
    #[serde(default)]
    pub(crate) process_files: Option<FilesId>,

    /// POSIX per-process timers created via `timer_create(2)`. Shared among the
    /// threads of a process (`CLONE_THREAD`) and not inherited across `fork`.
    pub(crate) posix_timers: Arc<Mutex<PosixTimers>>,

    /// Resource limits shared by threads and copied when a new process forks.
    pub(crate) resource_limits: Arc<Mutex<ResourceLimits>>,

    /// Logical CPU accounting shared by all threads in this process.
    pub(crate) process_cpu_time: Arc<Mutex<ProcessCpuTime>>,

    /// Guest-visible logical clock shared by every task in this process tree.
    #[serde(default)]
    pub(crate) guest_clock: Arc<Mutex<GuestClock>>,

    /// The `AF_UNIX` connections waiting for an `accept`, shared by every
    /// task in this process tree, as the guest clock is
    /// (`crate::syscalls::scm_rights::UnixConnections`). It names
    /// descriptions by handles that reach them only in the tool process that
    /// serves every guest (ptrace, KVM), so it is never serialized.
    #[serde(skip)]
    pub(crate) unix_connections: Arc<Mutex<crate::syscalls::scm_rights::UnixConnections>>,

    /// The run's live and unreaped tasks, shared by every task of the run, as
    /// the guest clock is (`LiveTasks`). Never serialized: a thread state
    /// read back from that form knows no task, and none is taken to be
    /// alone.
    #[serde(skip, default = "LiveTasks::unknown")]
    pub(crate) live_tasks: Arc<Mutex<LiveTasks>>,

    /// Parent process accounting notified when this process leader exits.
    pub(crate) parent_process_cpu_time: Option<Arc<Mutex<ProcessCpuTime>>>,

    /// The process accounting that holds each child process's birth-owned
    /// final-CPU publication (`ProcessCpuTime::child_cpu_publications`),
    /// shared by every task in this process tree, as the guest clock is. A
    /// child's publication starts in its creator's accounting
    /// (`prepare_child_cpu_publication`). When an exit re-parents the child
    /// (`Scheduler::reparent_orphans`), the process whose wait the scheduler
    /// selects it for takes the publication over
    /// (`adopt_child_cpu_publication`), and the child's exit records into
    /// whichever accounting holds it then
    /// (`record_exited_child_process_cpu_time`). Only the backends whose
    /// waits Detcore completes from these publications (`emulates_child_waits`)
    /// register children here.
    #[serde(skip)]
    pub(crate) child_cpu_owners: Arc<Mutex<BTreeMap<DetPid, Arc<Mutex<ProcessCpuTime>>>>>,

    /// Actual sequential KVM child's birth-owned publication capability.
    #[serde(skip)]
    pub(crate) parent_cpu_publication: Option<Arc<ChildCpuPublication>>,

    /// The record of a wait this thread's last syscall ended with
    /// `ERESTART_RESTARTBLOCK`, for the `restart_syscall` that resumes it
    /// (`syscalls::helpers::RestartBlock`). The next syscall takes it
    /// (`Detcore::handle_syscall_event`).
    #[serde(skip)]
    pub(crate) restart_block: Option<crate::syscalls::helpers::RestartBlock>,

    /// What a signal that ended this thread's current syscall's wait through
    /// `ERESTARTSYS`'s protocol left for the return to user mode
    /// (`syscalls::helpers::WokenWait`): set where the wait ended, settled by
    /// the syscall handler's last step
    /// (`syscalls::helpers::settle_woken_wait_at_return`), and checked
    /// against the deliveries at that return
    /// (`syscalls::helpers::check_woken_wait_delivery`). Each syscall starts
    /// with it clear (`Detcore::handle_syscall_event`).
    #[serde(skip)]
    pub(crate) woken_wait: crate::syscalls::helpers::WokenWait,

    /// The calling thread's working directory in the guest's view, read for
    /// the syscall it is running where the tool cannot read it from host
    /// procfs (`procfs_inode::tool_can_read_guest_cwd`: KVM): its path where
    /// an injected `getcwd` answered one, else, where `getcwd` rendered none
    /// (a removed directory, or one whose path is longer than `PATH_MAX`),
    /// the directories the syscall's spellings climb to out of it with `..`
    /// (`syscalls::SyscallCwd`); `None` until it is read, at most
    /// once per syscall (`Detcore::prepare_guest_cwd`). Each syscall starts
    /// without it (`Detcore::handle_syscall_event`), so a later syscall never
    /// decides by a directory read before it. Never carried across a fork or
    /// a migration.
    #[serde(skip)]
    pub(crate) syscall_cwd: Option<crate::syscalls::SyscallCwd>,

    /// Whether this thread's process has a guest handler for SIGALRM, so that
    /// signal phase 1's table (`sigalrm_phase1`) gates its syscalls. Set by the
    /// runtime's handler admission (phase 1 step I3), in the same turn it
    /// publishes the disposition to the scheduler's ledger; until then only
    /// tests set it.
    pub(crate) sigalrm_handled: bool,

    /// Detcore's own copy of this thread's signal dispositions and masks
    /// (audit step A5: recorded from the calls that change them, checked
    /// against the kernel where it can be read, and read by the decisions
    /// step 2 converted). `None` until the first image seeds it, and wherever it
    /// cannot be seeded: `None` means unknown, never default actions.
    #[serde(default)]
    pub(crate) signal_shadow: Option<crate::signal_state::SignalShadow>,

    /// Whether this thread, or the thread that created it, lost its signal
    /// state copy after a divergence: a lost copy is never seeded again.
    #[serde(default)]
    pub(crate) signal_shadow_lost: bool,

    /// What this thread was doing at its last guest event, for checking a
    /// signal delivery against the kernel's syscall registers (A5 step 3.2,
    /// `signal_shadow::check_delivery`): the syscall it entered, the result
    /// Detcore finished it with, a handler frame just approved, or no
    /// syscall. `None` is unknown (a new thread or image), and the check then
    /// takes the registers' own answer.
    #[serde(skip)]
    pub(crate) delivery_interruption: Option<reverie::Interruption>,

    /// Under the in-guest delivery gate, what ended the current syscall's wait
    /// for a signal (A5 step 3c.1c, design D40): set by the path that ends
    /// the wait, and taken by the restart checkpoint straight after the
    /// dispatch `match` (`Detcore::wait_end_checkpoint`), which turns the
    /// wait's restart code into the guest's result. `None` on every other
    /// call, and always with the gate off.
    #[serde(skip)]
    pub(crate) wait_signal_end: Option<crate::wait_signal_end::WaitSignalEnd>,

    /// While this thread's `pidfd_send_signal` handler runs on a self
    /// sentinel that the executing kernel spells differently from the guest:
    /// the guest's call and its physical form, which
    /// `translated_record_or_replay` injects in its place
    /// (`pidfd_self::ArmedRespelling`). `None` on every other call, for every
    /// new thread and after an exec.
    #[serde(skip)]
    pub(crate) pidfd_respelling: Option<crate::pidfd_self::PhysicalRespelling>,

    /// With `delivery_interruption` `AfterFrame`, the handler address the
    /// kernel's frame jumps to: a further delivery at the same exit, before
    /// any guest instruction, stops with the instruction pointer there.
    #[serde(skip)]
    pub(crate) delivery_frame_handler: u64,

    /// With `delivery_interruption` `AfterFrame`, the signal that frame
    /// delivers, which Linux puts in the handler's first argument register
    /// (`signal_shadow::at_frame_entry`).
    #[serde(skip)]
    pub(crate) delivery_frame_signal: i32,

    /// With `delivery_interruption` `AfterFrame`, `orig_ax` at that frame's
    /// delivery stop, which Linux keeps until the thread next enters the
    /// kernel (`signal_shadow::at_frame_entry`).
    #[serde(skip)]
    pub(crate) delivery_frame_orig_ax: i64,

    /// This thread's counts of checked signal deliveries (A5 step 3.2),
    /// logged when it exits.
    #[serde(skip)]
    pub(crate) delivery_counters: crate::syscalls::signal_shadow::DeliveryCounters,

    /// The process's record of its dispositions publications (A5 step 2b,
    /// `signal_shadow::publish_dispositions`): what the scheduler holds, the
    /// latest generation used, and the latest one the scheduler took. Shared
    /// by the threads of a process, which publish one entry.
    #[serde(default)]
    pub(crate) published_dispositions:
        Arc<Mutex<crate::syscalls::signal_shadow::PublishedDispositions>>,

    /// What the scheduler holds as this thread's mask from Detcore's copy
    /// (A5 step 2b, `signal_shadow::publish_mask`): `Some(None)` nothing,
    /// `Some(Some(mask))` that mask; `None` when that is unknown (after an
    /// exec), so the next request publishes or withdraws.
    #[serde(default)]
    pub(crate) published_mask: Option<Option<u64>>,

    /// The SIGALRM blocked bit this thread last published to the scheduler's
    /// ledger (signal phase 1). A handler's return changes the virtual mask
    /// without a syscall; the next syscall's entry compares the runtime's
    /// virtual bit with this one and publishes a change.
    pub(crate) sigalrm_blocked_published: bool,

    /// The real-time signals this thread's process has already recorded as
    /// unblocked by a handler's restored mask
    /// (`Scheduler::handler_restore_unblocked`), so a delivery adds nothing
    /// new without asking the kernel or the scheduler.
    pub(crate) handler_restore_rt_reported: u64,

    /// While Detcore runs its signal pipeline for a SIGALRM it delivers at a
    /// syscall's completion: the registers as that completion leaves them
    /// (`completed_syscall_projection`), which the pipeline reads instead of
    /// the backend's, whose context still shows the syscall's entry. Never
    /// written back.
    #[serde(skip)]
    pub(crate) signal_register_projection: Option<libc::user_regs_struct>,

    /// Per-thread checkpoints used to add only new work to the process totals.
    pub(crate) last_accounted_user_time: LogicalTime,
    pub(crate) last_accounted_system_time: LogicalTime,

    /// Absolute logical-clock values at this thread's creation. Child clocks
    /// inherit the parent's absolute position for scheduler ordering, but Linux
    /// `RUSAGE_THREAD` starts accounting at the calling thread's own creation.
    pub(crate) thread_cpu_start_user_time: LogicalTime,
    pub(crate) thread_cpu_start_system_time: LogicalTime,

    /// pseudo random number state
    pub prng: Pcg64Mcg,

    /// One initial-image auxv write completed by an authenticated backend
    /// before libc initialization. Consumed by the first post-exec callback;
    /// normal construction and child derivation never manufacture this fact.
    #[serde(default)]
    /// The initial image whose AT_RANDOM a backend wrote before post-exec, the
    /// bytes it wrote and its getrandom fills, until post-exec emits their
    /// records.
    pub(crate) initialized_random_auxv: Option<(
        crate::random::InitialImage,
        [u8; 16],
        Vec<crate::random::EarlyRequest>,
    )>,

    /// RNG to drive chaos scheduling decisions, separate from other (guest) RNG.
    pub chaos_prng: Pcg64Mcg,

    /// logical time, measuring progress of this thread and only this thread.
    pub thread_logical_time: DetTime,

    /// the last RCB clock value committed to `thread_logical_time`
    pub committed_clock_value: u64,

    /// Syscalls this thread made in the current backend-runtime bootstrap
    /// window without charging their cost to `thread_logical_time`. Zero
    /// outside a window; see [`ThreadState::charge_syscall_time`].
    #[serde(default)]
    pub(crate) uncharged_bootstrap_syscalls: u32,

    /// True from the moment [`ThreadState::charge_syscall_time`] leaves the
    /// syscall this thread is handling uncharged until that syscall's handler
    /// finishes. Resource requests made meanwhile through
    /// `tool_global::resource_request` are marked
    /// [`Resources::backend_runtime_bootstrap`], so the scheduler withholds
    /// their turn time as well, except for an IO-polling retry, which still
    /// advances it. Requests that bypass `resource_request` (direct futex and
    /// thread-lifecycle requests) are not marked and advance it as usual.
    /// Cleared at the start of every handler and before the syscall handler's
    /// post-hook.
    #[serde(default)]
    pub(crate) in_uncharged_bootstrap_syscall: bool,

    /// Set when this thread may have passed descriptors over `SCM_RIGHTS` on
    /// a socket that is not guest-held. Requests and the consuming exit
    /// carry it to the scheduler. It stays set for this owner's lifetime:
    /// cancellation can drop a request before its publication is received.
    #[serde(default)]
    pub(crate) passed_descriptors_out_unreported: bool,

    /// Set once a resource request of this thread carried
    /// `Resources::passes_descriptors_out`, after which the scheduler trusts
    /// no guest-held mark for the rest of the run, so this thread need not
    /// report a pass-out again (`Detcore::passes_descriptors_out_now`).
    #[serde(default)]
    pub(crate) passed_descriptors_out_reported: bool,

    /// Set by the nonblocking retry loop when the call it completed returned
    /// a result Detcore rewrote for the guest
    /// (`NonblockableSyscall::normalize_nonblocking_result`): a retried
    /// `connect` whose `EISCONN` reads as success. Such an attempt saw only
    /// that the socket was connected, by an earlier attempt or by another
    /// thread's connect of the same open file, not where to; `handle_connect`
    /// clears it before the call and reads it after.
    #[serde(default)]
    pub(crate) nonblocking_result_normalized: bool,

    /// Set by the syscall handler when a `FUTEX_WAKE` woke a waiter and
    /// [`Config::yields_after_futex_wake`] holds, so that the
    /// syscall's post-hook takes a yield turn. A logically killed waker gets
    /// `EINTR` (4) as its wake count and may set this although the scheduler
    /// did not requeue it; that thread is already being torn down. The other
    /// way round, the scheduler requeues the waker without a yield following
    /// when the wake is the robust-list owner-death wake of an exiting thread
    /// (which never sets this and skips the post-hook), and when the syscall
    /// handler returns an error after the wake and skips the post-hook. In
    /// both the thread is retiring or the run is failing, and the requeue
    /// alone only reorders the run queue deterministically. Cleared at the
    /// start of every handler and by that post-hook.
    #[serde(default)]
    pub(crate) yield_after_futex_wake: bool,
    /// Under `--pipe-wake-latency` (`crate::pipe_wake`): the thread time at
    /// which this thread hands the turn to the pollers its actions readied,
    /// set at the readying action that the scheduler says starts a window
    /// (`crate::pipe_wake::ReadyOutcome`), checked at syscall boundaries, and
    /// cleared by the hand-off or by a scheduler request of its own. Always
    /// `None` with the option unset.
    #[serde(default)]
    pub(crate) pipe_handoff_due: Option<LogicalTime>,
    /// A signal this thread sent to its own multi-threaded process, which it
    /// must dequeue before another thread runs: until its own delivery stop
    /// for that signal no timeslice of it ends, at the target, at the PMU
    /// maximum or in a timer callback (<https://github.com/rrnewton/hermit/issues/4034>).
    #[serde(default)]
    pub(crate) own_signal_turn_hold: Option<libc::c_int>,
    /// Set when one of this thread's signal deliveries that runs a handler is
    /// granted (`delivery_runs_handler`), cleared at the thread's next
    /// grant by the scheduler, whatever request it came through
    /// (`note_scheduler_handoff`). A signal stop that finds it set
    /// comes before this thread made any scheduling point since that
    /// delivery: in particular the
    /// delivery Linux makes, on the return from that handler's
    /// `rt_sigreturn`, of a signal the restored mask unblocks, or a second
    /// delivery on the same return to user space. Its turn continues this
    /// thread's (`Resources::continues_sigreturn`,
    /// <https://github.com/rrnewton/hermit/issues/4505>). It needs no report
    /// of the `rt_sigreturn` itself, so a recording and every replay of it
    /// (one a gdbserver serves too, where the backend reports no sigreturn)
    /// mark the same deliveries. Where the reports are reliable, a run also
    /// continues after a reported `rt_sigreturn` (`returned_from_sigreturn`).
    ///
    /// Where this differs from Linux. On Linux a signal the restored mask
    /// unblocks is delivered on the return from `rt_sigreturn`
    /// (`SYSCALL_DEFINE0(rt_sigreturn)`, v6.12 arch/x86/kernel/signal_64.c:246)
    /// by `arch_do_signal_or_restart` (arch/x86/kernel/signal.c:333, called
    /// from kernel/entry/common.c:111) whatever the handler did before it
    /// returned. Under `hermit record` and every replay, where no report is
    /// read, a handler that took a scheduler turn (a syscall that files a
    /// scheduler request, such as `sched_yield` or a write to a device;
    /// `getpid` files none) makes its next delivery an ordinary one, which
    /// waits behind the other runnable threads. The guest can observe which
    /// thread runs next: a sibling may run, and read or write shared memory,
    /// between the handler's return and that delivery, and a thread whose
    /// handler yields while a sibling re-signals it at every turn progresses
    /// only as fast as it did before continuations existed. It cannot observe
    /// a different signal, mask, handler, `siginfo` or restart decision: the
    /// delivery is still made before the thread runs any code of its own, as
    /// if the thread were preempted at the rescheduling point that precedes
    /// signal work on the same exit (common.c:101-102), which Linux permits.
    /// It is deterministic, and the same in a recording and its replays.
    ///
    /// A delivery that runs no handler (`SIG_IGN`, or a default action that
    /// ignores) does not set it. Linux dequeues the next pending signal on
    /// the same exit after discarding one (`get_signal`, v6.12
    /// kernel/signal.c:2815-2816 and 2831-2832), so here too the stop that
    /// follows is ordered as if preempted at that exit, with the same
    /// consequence: only which thread runs next changes. Continuing it
    /// would let a thread that dequeues an ignored `ITIMER_REAL` copy run
    /// on to the end of its slice instead of waiting at its next dequeue,
    /// so the following expiry's copy stays queued while a sibling runs a
    /// whole slice, and the timer, which re-arms at the dequeue, reads 0
    /// for that slice (`itimer_mixed_masks_keep_the_timer_running`).
    #[serde(default)]
    pub(crate) delivered_since_request: bool,
    /// Set when the backend reports this thread's `rt_sigreturn`
    /// (`Tool::handle_sigreturn`), cleared by its next event of any other
    /// kind (`pre_handler_hook`). A signal stop that finds it set is the
    /// delivery Linux makes on that return to user space, and it continues
    /// the thread's turn even when the handler took a scheduler turn and so
    /// cleared `delivered_since_request`: without it, a reader whose handler
    /// calls `sched_yield` while a sibling re-signals it at every turn did
    /// not get past its handler within 900 s, where the report trigger alone
    /// took about 32,000 sends
    /// (<https://github.com/rrnewton/hermit/issues/4505>). It is read only
    /// where `signal_shadow::observes_sigreturns` holds, which it never does
    /// under `hermit record` or any replay (their signal-state copy is off,
    /// and a gdbserver turns the reports off), so a recording and every
    /// replay of it decide continuations by `delivered_since_request` alone.
    #[serde(default)]
    pub(crate) returned_from_sigreturn: bool,
    /// A delivery the signal-state copy recorded (`record_checked_delivery`)
    /// whose handler entry Detcore has not yet recorded with the scheduler
    /// (`record_delivery_entry` at the waiter's next syscall entry).
    #[serde(default)]
    pub(crate) delivery_entered: bool,
    /// Deliveries announced to `Tool::announce_signal_event` and neither
    /// committed nor withdrawn yet, with what each one's turn left for its
    /// commit. A backend settles every announcement before it returns to
    /// guest code, so this is empty at any other point.
    #[serde(default)]
    pub(crate) signal_announcements: Vec<SignalAnnouncementState>,
    /// The token the next announcement gets.
    #[serde(default)]
    pub(crate) next_signal_announcement: u64,
    /// How many of this thread's latest scheduler requests in a row were
    /// deliveries marked `Resources::continues_sigreturn`; any other request
    /// resets it. A delivery is marked only while it is below
    /// [`MAX_SIGRETURN_CONTINUATIONS`]. That cap is what stops a handler that
    /// raises its own signal from holding the CPU for ever, in every mode: a
    /// timeslice end did not interrupt such a run in practice (7,773
    /// deliveries in 15 s without one), and under `--max-timeslice=disabled`
    /// there is none.
    #[serde(default)]
    pub(crate) consecutive_continuations: u8,
    /// How many times the scheduler has granted this thread a turn
    /// (`note_scheduler_handoff`). A signal stop compares it before and after
    /// its own awaits, so a grant among them ends the delivery window too.
    #[serde(default)]
    pub(crate) scheduler_grants: u64,
    /// Whether this thread has recorded the determinism loss of a binder call
    /// that ran an effect off the turn (`Detcore::admit_host_timed_binder_call`).
    /// Kept per thread, so that each run that makes such a call records it:
    /// a per-process latch would let a second run in the same Hermit process
    /// go unmarked.
    #[serde(default)]
    pub(crate) host_timed_binder_loss_recorded: bool,
    /// Whether this thread has recorded the determinism loss of a binder call
    /// that ran off the turn while the run had another task
    /// (`Detcore::admit_shared_binder_call`), per thread as above.
    #[serde(default)]
    pub(crate) shared_binder_loss_recorded: bool,
    /// Whether this thread has recorded the determinism loss of descriptors a
    /// binder read delivered off the turn that Detcore does not record
    /// (`Detcore::take_binder_receipts`), per thread as above.
    #[serde(default)]
    pub(crate) unrecorded_binder_loss_recorded: bool,

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

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Exact per-thread PMU RCB target for the active preemption-replay slice.
    /// `None` selects the legacy logical-time deadline path.
    #[serde(default)]
    pub replay_rcb_end: Option<u64>,

    /// In a preemption replay, the logical time of the last priority change
    /// point this thread filed (`ThreadState::file_replay_change_point`).
    // Native-exec admission rejects live replay state; supported snapshots
    // retain their existing positional layout.
    #[serde(skip)]
    pub last_replay_change_point: Option<LogicalTime>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Deterministic chaos epoch this thread was in at its last `next_timeslice`.
    /// Used only to detect epoch transitions for `CHAOSEPOCH` logging; the epoch
    /// itself is recomputed each slice from `thread_logical_time`. Sentinel
    /// `u64::MAX` guarantees the first slice always logs its initial epoch.
    #[serde(default = "chaos_epoch_sentinel")]
    pub chaos_epoch: u64,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Exact multiplier currently used to convert this thread's RCBs to virtual time.
    #[serde(default)]
    pub chaos_slowdown_factor: RcbTimeMultiplier,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// True when live chaos configuration or a replay artifact supplies the factor.
    #[serde(default)]
    pub chaos_slowdown_active: bool,

    /// This thread claimed a `SIGCHLD` for a sleep (`SleepSignal::Claimed`),
    /// and the claimed copy's delivery stop has not come yet. That stop places
    /// the delivery (`ChildExitSigchldControl::ClaimedDelivery`) just before
    /// it asks for its turn, so the mark the scheduler keeps is bound to that
    /// copy. A system call, or a copy the ledger drops, clears this once
    /// SIGCHLD is no longer pending for the thread (the claimed copy was never
    /// delivered to it; `syscalls::signal::claim_still_pending`).
    #[serde(default)]
    pub(crate) claimed_sigchld: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Transitions waiting to be committed into the preemption artifact.
    #[serde(default)]
    pub pending_chaos_epochs: Vec<ChaosEpochTransition>,

    /// Absolute deadline enforced by the PMU-backed `--max-timeslice` timer. This is separate from
    /// `end_of_timeslice` so syscall-heavy workloads can use a shorter, cheap target deadline.
    pub max_timeslice_end: Option<LogicalTime>,

    /// The post-crash bound, once the run has switched to it
    /// (`crate::post_crash`): this thread's slices are bounded by it from
    /// its next slice on, instead of by `max_timeslice`. Learned from the
    /// scheduler's answer to a post-crash step or from a reply that resumes
    /// the thread from a grant.
    #[serde(default)]
    pub post_crash_bound: Option<std::num::NonZeroU64>,

    /// The crash signal whose caught delivery armed the post-crash switch for
    /// this thread, until a stop of the thread finds the handler returned
    /// (`crate::post_crash::note_stop`).
    #[serde(default)]
    pub crash_armed: Option<i32>,
    /// The slice length that replaces `--max-timeslice` while this thread
    /// replays a record whose slices ran without that bound
    /// (`Scheduler::uncapped_backstop`), as the replayer last said; `None` for
    /// every other run and once the record has run out.
    #[serde(default)]
    pub uncapped_backstop: Option<LogicalTime>,

    /// The deadline hint carried by the last scheduler reply this thread
    /// received (`ThreadState::cap_timeslice_at_deadline`). Applied when the
    /// reply arrives, and again by the two paths that then install a fresh
    /// slice end: thread start and the exec continuation.
    pub last_grant_deadline: Option<Duration>,

    /// Track what our last timer was set for, just to double check that RCB timers are behaving
    /// as expected and see if we went over.  (For exmaple, this behaves badly if threads are not
    /// pinned and our we migrate between cores.)
    pub last_rcb_timer: Option<u64>,

    /// Whether `last_rcb_timer` represents the maximum deadline rather than a manual interrupt.
    #[serde(default)]
    pub last_rcb_timer_is_max: bool,

    /// Whether the scheduler's last reply to this thread was a grant that
    /// found it the only thread able to run before it next enters Detcore
    /// (`GlobalResponse::WithLoneGrant`). Every reply without that mark
    /// clears it.
    #[serde(default)]
    pub lone_grant: bool,

    /// While the thread runs alone (`Config::replays_lone_preemptions`): the
    /// branch count, in `committed_clock_value` units, at which its current
    /// timeslice ends. No timer stops the guest there; the thread's next
    /// entry into Detcore processes every such end at or before the count it
    /// reached, in order (`Detcore::catch_up_lone_slice_ends`).
    #[serde(default)]
    pub lone_slice_end: Option<u64>,

    /// While such an end is processed: its branch count, which the clock
    /// update reads in place of the hardware counter.
    #[serde(default)]
    pub replaying_lone_slice_end: Option<u64>,

    /// While such an end is processed: it is taken with the stop of the
    /// check-in that follows it at its own branch count, because a replay
    /// does not allow it without a stop
    /// (`Detcore::catch_up_lone_slice_ends`). Its recorded event carries
    /// that stop's RIP instead of a `stopless` mark, so a re-record below
    /// `STOPLESS_SLICE_ENDS_FORMAT` keeps the end visible.
    #[serde(default)]
    pub lone_slice_end_at_check_in: bool,

    /// Set when a grant while such an end was processed did not find the
    /// thread alone, which the scheduler's predicate rules out; taken by
    /// `Detcore::catch_up_lone_slice_ends`, which reports it.
    #[serde(default)]
    pub lone_replay_unsound: bool,

    /// In a schedule replay: the branch count, in `committed_clock_value`
    /// units, of the thread's next recorded timeslice end when the recording
    /// took that end without a stop (`SchedEvent::stopless`), as the
    /// replayer's latest reply to this thread names it (`ReplayedSliceEnd`;
    /// every TraceSchedEvent reply sets it, `None` when it names no such
    /// end). Only that end may be taken without a stop in the replay; at any
    /// other the recording stopped, and its RIP is checked.
    #[serde(default)]
    pub replayed_stopless_end: Option<u64>,
    /// Where `last_rcb_timer` was armed when it was armed past its branch
    /// target (`--chaos-instruction-offset`): it may then retire up to that
    /// many more conditional branches than `last_rcb_timer` says, one per
    /// step.
    #[serde(default)]
    pub armed_instruction_offset: Option<InstructionOffset>,

    /// The steps past its branch target this chaos timeslice's timer is armed
    /// for (`--chaos-instruction-offset`), `0` for none.
    #[serde(default)]
    pub slice_instruction_offset: u64,

    /// Where the recorded timeslice being replayed had its timer armed, if
    /// `--chaos-instruction-offset` delivered it past its branch target.
    #[serde(default)]
    pub replay_instruction_offset: Option<InstructionOffset>,

    /// How many instruction offsets this thread has drawn: the position of
    /// its next draw in its own stream (`chaos_instruction_offset_draw`).
    #[serde(default)]
    pub chaos_instruction_offset_draws: u64,
    /// Under a schedule replay, the slice end `next_timeslice` installed as
    /// the backstop, `--max-timeslice` after the slice began; see
    /// `restart_replay_backstop`. Installing a slice end the schedule decides
    /// (`install_schedule_deadline`) clears it, even when that deadline falls
    /// on the same nanosecond: a recorded slice end can coincide with the
    /// backstop, and it is still the schedule's. It is also the backstop only
    /// while it is still `end_of_timeslice`.
    #[serde(default)]
    pub replay_backstop: Option<LogicalTime>,

    /// Are we past the global moment when the guest's first execve of its root binary completes
    /// (with a successful exit code).
    pub(crate) past_global_first_execve: bool,

    /// Guest address of this thread's `struct robust_list_head`, as last
    /// registered by a successful `set_robust_list(2)`.
    ///
    /// Linux clears `task->robust_list` in `copy_process()` and in `execve`, so
    /// this is never inherited: every thread re-registers its own head (glibc
    /// does so in `start_thread`). Detcore replays the kernel's
    /// `exit_robust_list()` walk from this address when the thread exits; see
    /// `crate::syscalls::robust_list`.
    #[serde(default)]
    pub(crate) robust_list_head: Option<usize>,

    /// Per-thread registrations are mirrored here while their shared address
    /// space is readable, so a group exit can walk every dying sibling's list.
    #[serde(default)]
    pub(crate) robust_list_process: Arc<Mutex<RobustListProcessState>>,

    /// The robust-list walk this thread's exec in flight still owes
    /// ([`ExecRobustCommit`]), from the exec's entry to its point of no
    /// return; `None` otherwise.
    #[serde(skip)]
    pub(crate) exec_robust_commit: Option<ExecRobustCommit>,

    /// Staged boundary protocol state. Runtime creation/exec does not enable
    /// it yet; legacy state keeps `None`. Native transfer cannot discard it.
    #[serde(default)]
    pub(crate) slice_boundary: Option<crate::slice_boundary::Observations>,
    #[serde(default)]
    pub(crate) callback_history: crate::preemptions::callback_history::State,

    /// Under `--replay-schedule-from`, whether the replayed schedule is over
    /// for this thread: it ran out, or the replay released it
    /// (`Scheduler::schedule_replay_over`). The thread learns it from the
    /// reply to its next traced event, and a new thread inherits it from the
    /// thread that created it. From then on its slices end as in a run
    /// without a schedule (`follows_replayed_schedule`).
    #[serde(default)]
    pub(crate) schedule_replay_over: bool,
}

/// We cannot assume that the record_or_replay "subtool" is Debug, so it is handy to be able to
/// print the Detcore threadstate alone.
impl<T> std::fmt::Debug for ThreadState<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadState")
            .field("dettid", &self.dettid)
            .field("detpid", &self.detpid)
            .field("physical_tid", &self.physical_tid)
            .field("mm_id", &self.mm_id)
            .field("memory_metadata", &self.memory_metadata)
            .field("stats", &self.stats)
            .field("clone_flags", &self.clone_flags)
            .field("file_metadata", &self.file_metadata)
            .field("process_files", &self.process_files)
            .field("posix_timers", &self.posix_timers)
            .field("resource_limits", &self.resource_limits)
            .field("process_cpu_time", &self.process_cpu_time)
            .field("prng", &self.prng)
            .field("chaos_prng", &self.chaos_prng)
            .field("thread_logical_time", &self.thread_logical_time)
            .field("committed_clock_value", &self.committed_clock_value)
            .field("end_of_timeslice", &self.end_of_timeslice)
            .field("replay_rcb_end", &self.replay_rcb_end)
            .field("chaos_epoch", &self.chaos_epoch)
            .field("chaos_slowdown_factor", &self.chaos_slowdown_factor)
            .field("chaos_slowdown_active", &self.chaos_slowdown_active)
            .field("max_timeslice_end", &self.max_timeslice_end)
            .field("post_crash_bound", &self.post_crash_bound)
            .field("crash_armed", &self.crash_armed)
            .field("uncapped_backstop", &self.uncapped_backstop)
            .field("last_rcb_timer", &self.last_rcb_timer)
            .field("last_rcb_timer_is_max", &self.last_rcb_timer_is_max)
            .finish()
    }
}

impl<T> Default for ThreadState<T> {
    fn default() -> Self {
        unreachable!()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1151)
/// Sentinel for `ThreadState::chaos_epoch` before the first `next_timeslice`.
/// `u64::MAX` can never equal a real epoch (`current_ns / N`), so the first
/// chaos slice always emits its `CHAOSEPOCH` transition.
pub(crate) fn chaos_epoch_sentinel() -> u64 {
    u64::MAX
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

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1149)
/// RR-style stable per-thread slowdown factor for chaos scheduling.
///
/// Returns the multiplier applied to a thread's *mean* chaos timeslice length.
/// A factor `> 1.0` means the thread is preempted less often (runs "faster"
/// between preemptions — a slower relative wall-clock for its peers), `< 1.0`
/// means it is preempted more often. The factor is drawn log-uniformly from
/// `[1/max_factor, max_factor]`, so slow and fast are symmetric in log-space
/// and `1.0` is the geometric center.
///
/// The value is a **pure, deterministic function** of `(sched_seed, dettid)`:
/// it depends on no run-order, no wall-clock, and no shared PRNG state, so it
/// is stable for a thread across the whole run and reproducible under a fixed
/// seed (unlike the per-timeslice `chaos_prng` draw, which is redrawn every
/// slice and averages out over a long run). Threads are perturbed by a fixed
/// constant so the factor stream differs from other seed-derived streams (e.g.
/// `post_fork_prng`) that also start from `sched_seed`.
///
/// `max_factor <= 1.0` disables the spread and returns `1.0` (nominal) for
/// every thread; callers validate `max_factor >= 1.0`.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1151)
/// `epoch` selects a deterministic chaos EPOCH: the factor is redrawn per
/// (thread, epoch) so a thread's bias changes in deterministic phases across a
/// long run instead of staying fixed. `epoch == 0` reproduces the epoch-less
/// per-thread-slowdown value EXACTLY (the epoch term is `0` and cancels out of
/// the mix), so enabling epochs never perturbs the first epoch's behavior.
pub(crate) fn chaos_per_thread_slowdown_factor(
    sched_seed: u64,
    dettid: DetTid,
    epoch: u64,
    max_factor: f64,
) -> RcbTimeMultiplier {
    // `<=` (rather than `!(max_factor > 1.0)`) keeps clippy's partial-ord lint
    // happy; validate_invariants already rejects non-finite factors upstream.
    if max_factor <= 1.0 {
        return RcbTimeMultiplier::ONE;
    }
    // Mix the seed with the (stable) deterministic thread id to give each
    // thread its own point in the factor distribution. The salt keeps this
    // stream distinct from other sched_seed-derived streams.
    const SLOWDOWN_SALT: u64 = 0x736c_6f77_646f_776e; // "slowdown"
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    // Fold the epoch into the mix with its own golden-ratio multiplier. At
    // `epoch == 0` this term is 0, leaving `mixed` identical to the epoch-less
    // factor; each successive epoch decorrelates the draw deterministically.
    const EPOCH_GOLDEN: u64 = 0xbf58_476d_1ce4_e5b9;
    let mixed = sched_seed
        ^ SLOWDOWN_SALT
        ^ ((dettid.as_raw() as u32 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
        ^ epoch.wrapping_mul(EPOCH_GOLDEN);
    let mut prng = Pcg64Mcg::seed_from_u64(mixed);
    // u in [0,1); map to exponent in [-1, 1) then factor = max_factor^exp,
    // i.e. a log-uniform draw over [1/max_factor, max_factor).
    let u: f64 = prng.random::<f64>();
    let exponent = 2.0 * u - 1.0;
    RcbTimeMultiplier::from_f64(max_factor.powf(exponent))
}

/// The `draw`-th instruction offset of thread `dettid` under
/// `--chaos-instruction-offset=max`: uniform in `0..=max`.
///
/// A pure function of `(sched_seed, dettid, draw)`, in a stream of its own,
/// as `chaos_per_thread_slowdown_factor` is: drawing it takes nothing from the
/// thread's `chaos_prng`, so every timeslice length and priority a seed draws
/// is the same with the option on as with it off.
pub(crate) fn chaos_instruction_offset_draw(
    sched_seed: u64,
    dettid: DetTid,
    draw: u64,
    max: u64,
) -> u64 {
    const OFFSET_SALT: u64 = 0x6f66_6673_6574_7321; // "offsets!"
    const DRAW_GOLDEN: u64 = 0xbf58_476d_1ce4_e5b9;
    let mixed = sched_seed
        ^ OFFSET_SALT
        ^ ((dettid.as_raw() as u32 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
        ^ draw.wrapping_mul(DRAW_GOLDEN);
    Pcg64Mcg::seed_from_u64(mixed).random_range(0..=max)
}

impl<T> ThreadState<T> {
    /// Account one hand-off of this thread to the scheduler for a grant, in
    /// which other threads may run: it ends the window in which a signal stop
    /// continues the thread's last delivery (`delivered_since_request`), and
    /// a request that is itself such a continuation (`continues_sigreturn`)
    /// counts toward `consecutive_continuations`, which any other resets.
    /// Called by `tool_global::send_and_update_time` when the reply says the
    /// thread was granted a turn while its request was pending
    /// (`GlobalResponse::Granted`). The scheduler marks every point where it
    /// resumes a waiting thread (`Scheduler::granted`: a grant, or a parked
    /// wait resumed to observe a signal before its grant, whose nested signal
    /// hook then runs), so every hand-off is noted
    /// whatever the request (an ordinary or parked request, a futex wait, a
    /// parent's continuation after a fork, a schedule-replay trace, an exec's
    /// continuation), and an RPC answered without a grant is none.
    pub(crate) fn note_scheduler_handoff(&mut self, continues_sigreturn: bool) {
        self.scheduler_grants = self.scheduler_grants.wrapping_add(1);
        self.delivered_since_request = false;
        self.consecutive_continuations = if continues_sigreturn {
            self.consecutive_continuations.saturating_add(1)
        } else {
            0
        };
    }

    /// What `observe_guest_clock(raw)` would return, without raising the
    /// clock's floor: the guest clock as of a raw observation.
    pub(crate) fn peek_guest_clock(&self, raw: LogicalTime) -> LogicalTime {
        let clock = self.guest_clock.lock().expect("guest clock mutex poisoned");
        clock.now.max(raw)
    }

    /// Inspect the live per-thread cut. Physical task count, childlessness,
    /// coordinator leases and signal dispositions need their own admission;
    /// neither serde nor these lock probes establish those facts.
    pub(crate) fn validate_native_exec_cut(
        &self,
        binding: &crate::native_exec::NativeExecCutBinding,
        config: &Config,
    ) -> Result<(), crate::native_exec::NativeExecCutError> {
        use crate::native_exec::NativeExecCutError;
        use crate::native_exec::try_native_exec_lock;

        if self.dettid != binding.dettid
            || self.detpid != Some(binding.detpid)
            || self.physical_tid != Some(binding.physical_tid)
        {
            return Err(NativeExecCutError::Identity("ThreadState IDs"));
        }
        if !self.thread_start_entered || !self.past_global_first_execve {
            return Err(NativeExecCutError::PreparedState("initial lifecycle"));
        }
        if self.tool_shares_guest_descriptor_table
            != config.backend.tool_shares_guest_descriptor_table
        {
            return Err(NativeExecCutError::Identity("descriptor-table capability"));
        }
        if self.nested_pid_namespace {
            return Err(NativeExecCutError::LiveCapability("nested PID namespace"));
        }
        if self.signal_task_identity.is_some() {
            return Err(NativeExecCutError::LiveCapability("signal task identity"));
        }
        if self.parent_process_cpu_time.is_some() || self.parent_cpu_publication.is_some() {
            return Err(NativeExecCutError::LiveCapability("parent CPU ownership"));
        }
        if self.restart_block.is_some() || self.signal_register_projection.is_some() {
            return Err(NativeExecCutError::LiveCapability("restart/projection"));
        }
        if self.claimed_sigchld {
            return Err(NativeExecCutError::LiveCapability("claimed SIGCHLD"));
        }
        if !self.signal_announcements.is_empty() {
            return Err(NativeExecCutError::LiveCapability(
                "unsettled signal announcement",
            ));
        }
        if self.clone_flags.is_some() || self.pending_vfork.is_some() {
            return Err(NativeExecCutError::LiveCapability("clone/vfork"));
        }
        if self.own_signal_turn_hold.is_some()
            || self.sigalrm_handled
            || self.handler_restore_rt_reported != 0
        {
            return Err(NativeExecCutError::LiveCapability("guest signal handler"));
        }
        if self.initialized_random_auxv.is_some() {
            return Err(NativeExecCutError::LiveCapability("initial random handoff"));
        }
        if self.uncharged_bootstrap_syscalls != 0
            || self.in_uncharged_bootstrap_syscall
            || self.yield_after_futex_wake
        {
            return Err(NativeExecCutError::LiveCapability(
                "bootstrap/yield transient",
            ));
        }
        if self.preemption_points.is_some()
            || self.slice_boundary.is_some()
            || !self.interrupt_at.is_empty()
            || self.replay_rcb_end.is_some()
            || self.last_replay_change_point.is_some()
            || self.last_rcb_timer.is_some()
            || self.last_rcb_timer_is_max
            || self.max_timeslice_end.is_some()
            || self.chaos_slowdown_active
            || !self.pending_chaos_epochs.is_empty()
        {
            return Err(NativeExecCutError::LiveCapability("RCB/replay/chaos state"));
        }
        let user = self.thread_logical_time.user_cpu_time();
        let system = self.thread_logical_time.system_cpu_time();
        if self.thread_cpu_start_user_time > self.last_accounted_user_time
            || self.last_accounted_user_time > user
            || self.thread_cpu_start_system_time > self.last_accounted_system_time
            || self.last_accounted_system_time > system
        {
            return Err(NativeExecCutError::PreparedState(
                "CPU accounting checkpoints",
            ));
        }
        try_native_exec_lock(&self.process_cpu_time, "process_cpu_time")?
            .validate_native_exec_cut()?;
        // Do not call observe/accounting routines: pending deltas and the
        // process-tree guest floor are continuing state, not a new epoch.
        drop(try_native_exec_lock(&self.guest_clock, "guest_clock")?);
        drop(try_native_exec_lock(
            &self.resource_limits,
            "resource_limits",
        )?);
        drop(try_native_exec_lock(&self.posix_timers, "posix_timers")?);
        Ok(())
    }

    pub(crate) fn validate_old_native_exec_robust_state(
        &self,
    ) -> Result<(), crate::native_exec::NativeExecCutError> {
        use crate::native_exec::NativeExecCutError;
        let process = crate::native_exec::try_native_exec_lock(
            &self.robust_list_process,
            "robust_list_process",
        )?;
        if process.heads.len() != usize::from(self.robust_list_head.is_some())
            || process.heads.get(&self.dettid).copied() != self.robust_list_head
            || !process.pending.is_empty()
            || !process.matched_physical_exits.is_empty()
            || !process.exec_pending.is_empty()
            || !process.tool_walked.is_empty()
            || process.exit_group_filed
            || !process.exit_group_filers.is_empty()
            || process.exit_group_walker.is_some()
            || process.exit_group_walked
        {
            return Err(NativeExecCutError::LiveCapability(
                "old robust process state",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_prepared_native_exec_cut(
        &self,
        binding: &crate::native_exec::NativeExecCutBinding,
    ) -> Result<(), crate::native_exec::NativeExecCutError> {
        use crate::native_exec::NativeExecCutError;
        use crate::native_exec::try_native_exec_lock;

        if self.mm_id != binding.prepared_mm() {
            return Err(NativeExecCutError::Identity("prepared mm"));
        }
        try_native_exec_lock(&self.file_metadata, "prepared file_metadata")?
            .validate_native_exec_cut(binding.prepared_files(), true)?;
        if !try_native_exec_lock(&self.memory_metadata, "memory_metadata")?
            .is_fresh_for_native_exec()
        {
            return Err(NativeExecCutError::PreparedState("memory_metadata"));
        }
        let process = try_native_exec_lock(&self.robust_list_process, "robust_list_process")?;
        if self.robust_list_head.is_some()
            || !process.heads.is_empty()
            || !process.pending.is_empty()
            || !process.matched_physical_exits.is_empty()
            || !process.tool_walked.is_empty()
            || process.exit_group_filed
            || !process.exit_group_filers.is_empty()
            || process.exit_group_walker.is_some()
            || process.exit_group_walked
        {
            return Err(NativeExecCutError::PreparedState("robust process state"));
        }
        // These wakes belong to the old image. Preserve their order and keys
        // for the actual successful post-exec hook; do not rebase or consume.
        for (owner, wake) in &process.exec_pending {
            if *owner != binding.dettid
                || matches!(wake.futex, FutexID::Private { mm, .. } if mm != binding.old_mm)
            {
                return Err(NativeExecCutError::PreparedState("staged robust wake"));
            }
        }
        Ok(())
    }

    pub(crate) fn observe_guest_clock(&self, raw: LogicalTime) -> LogicalTime {
        self.guest_clock
            .lock()
            .expect("guest clock mutex poisoned")
            .observe(raw)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1060): Review backend-stable child RNG reseeding.
    /// Replaces the host-TID-derived child streams with a backend-provided,
    /// deterministic identity before the thread enters its start hook.
    pub fn reseed_child_rngs(&mut self, parent: &Self, entropy: u128) {
        self.prng = thread_rng_from_parent_entropy("USER RAND", &parent.prng, entropy);
        self.chaos_prng = thread_rng_from_parent_entropy("CHAOSRAND", &parent.chaos_prng, entropy);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review SaBRe orphan-thread memory identity recovery.
    /// Repair a thread state that a remote backend had to initialize without
    /// access to its parent's state.
    pub(crate) fn recover_process_mm_id(&mut self, detpid: DetPid) -> bool {
        if self.dettid == detpid || self.mm_id != MmId::initial(self.dettid) {
            return false;
        }

        self.mm_id = MmId::initial(detpid);
        true
    }

    pub(crate) fn account_process_cpu_time(&mut self) {
        let user = self.thread_logical_time.user_cpu_time();
        let system = self.thread_logical_time.system_cpu_time();
        let user_delta = user - self.last_accounted_user_time;
        let system_delta = system - self.last_accounted_system_time;
        let own = self.thread_cpu_time();
        let mut process = self
            .process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned");
        process.add_thread_delta(user_delta, system_delta);
        process.record_thread(self.dettid, own);
        drop(process);
        self.last_accounted_user_time = user;
        self.last_accounted_system_time = system;
    }

    /// At the exit of thread `exiting` of process `process`, once its final
    /// time is accounted and it has left the scheduler: drop this thread's
    /// entry from its process's per-thread table unless it leads the
    /// process. A thread other than the leader is gone for good, and its id
    /// names no thread's clock any more. A leader that exits before its
    /// threads stays a zombie whose clock reads its final time
    /// (`ProcessCpuTime::threads`).
    pub(crate) fn release_thread_cpu_time_at_exit(&self, exiting: DetTid, process: DetPid) {
        if exiting != process {
            self.process_cpu_time
                .lock()
                .expect("process CPU time mutex poisoned")
                .forget_thread(self.dettid);
        }
    }

    /// Another thread of this process's own (user, system) time, as of that
    /// thread's last accounting, which happens at each of its syscalls and
    /// each RCB commit: with threads sequentialized, where it last stopped.
    pub(crate) fn sibling_cpu_time(&self, thread: DetTid) -> (LogicalTime, LogicalTime) {
        self.process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned")
            .thread_time(thread)
    }

    /// This thread's own (user, system) time and its process's accounting
    /// as of this thread's last accounting, which happens at each of its
    /// syscalls and each RCB commit; read without accounting anything now,
    /// so the read changes no state (`tool_global::with_cpu_publication`).
    pub(crate) fn accounted_cpu_times(&self) -> ((LogicalTime, LogicalTime), ProcessCpuSnapshot) {
        let process = self
            .process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned");
        (process.thread_time(self.dettid), process.snapshot)
    }

    /// The own (user, system) time of `thread`, this thread or another of
    /// its process: this thread's now ([`Self::thread_cpu_time`]), another's
    /// as of its last accounting ([`Self::sibling_cpu_time`]).
    pub(crate) fn task_cpu_time(&self, thread: DetTid) -> (LogicalTime, LogicalTime) {
        if thread == self.dettid {
            self.thread_cpu_time()
        } else {
            self.sibling_cpu_time(thread)
        }
    }

    /// Decide whether the syscall this thread is making now charges its cost
    /// to `thread_logical_time`. `in_backend_runtime_bootstrap` is the
    /// backend's `Guest::is_backend_runtime_bootstrap()` answer for this
    /// thread at this syscall.
    ///
    /// Outside a bootstrap window every syscall is charged, and the
    /// per-window count of uncharged syscalls goes back to zero. Inside a
    /// window a syscall that observes virtual time is always charged; any
    /// other syscall is left uncharged until
    /// [`syscall_time::MAX_UNCHARGED_BOOTSTRAP_SYSCALLS`] of them have been,
    /// and every syscall after that in the same window is charged. Reaching
    /// the cap logs one info line for the window.
    ///
    /// [`syscall_time::MAX_UNCHARGED_BOOTSTRAP_SYSCALLS`]: crate::syscall_time::MAX_UNCHARGED_BOOTSTRAP_SYSCALLS
    pub(crate) fn charge_syscall_time(
        &mut self,
        in_backend_runtime_bootstrap: bool,
        sysno: Sysno,
    ) -> bool {
        if !in_backend_runtime_bootstrap {
            if self.uncharged_bootstrap_syscalls != 0 {
                debug!(
                    "[dtid {}] backend runtime bootstrap window left {} syscalls uncharged",
                    self.dettid, self.uncharged_bootstrap_syscalls
                );
                self.uncharged_bootstrap_syscalls = 0;
            }
            return true;
        }
        if crate::syscall_time::observes_virtual_time(sysno) {
            return true;
        }
        if self.uncharged_bootstrap_syscalls < crate::syscall_time::MAX_UNCHARGED_BOOTSTRAP_SYSCALLS
        {
            self.uncharged_bootstrap_syscalls += 1;
            if self.uncharged_bootstrap_syscalls
                == crate::syscall_time::MAX_UNCHARGED_BOOTSTRAP_SYSCALLS
            {
                // Once per window: the count stays at the cap from here on.
                info!(
                    "[dtid {}] backend runtime bootstrap window reached its cap of {} uncharged syscalls; every further syscall in this window is charged",
                    self.dettid,
                    crate::syscall_time::MAX_UNCHARGED_BOOTSTRAP_SYSCALLS
                );
            }
            return false;
        }
        true
    }

    pub(crate) fn process_cpu_time(&mut self) -> ProcessCpuSnapshot {
        self.account_process_cpu_time();
        self.process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned")
            .snapshot
    }

    /// This thread's own logical (user, system) CPU time.
    ///
    /// `getrusage(RUSAGE_THREAD)` needs the per-thread counters, not the process aggregate
    /// that [`Self::process_cpu_time`] returns; for a multithreaded guest the two differ and
    /// reporting the aggregate would over-report every thread. Reads the same
    /// `thread_logical_time` counters that `account_process_cpu_time` folds into the process
    /// total, minus the absolute clock inherited when this thread was created.
    pub(crate) fn thread_cpu_time(&self) -> (LogicalTime, LogicalTime) {
        (
            self.thread_logical_time.user_cpu_time() - self.thread_cpu_start_user_time,
            self.thread_logical_time.system_cpu_time() - self.thread_cpu_start_system_time,
        )
    }

    pub(crate) fn record_exited_child_process_cpu_time(&mut self, pid: DetPid) -> bool {
        self.account_process_cpu_time();
        // The accounting that holds this process's publication now: a
        // re-parented process's new parent took it over
        // (`adopt_child_cpu_publication`). The registry stays locked through
        // the record, so a takeover cannot pass between the lookup and it.
        let owners = Arc::clone(&self.child_cpu_owners);
        let owners = owners.lock().expect("child CPU owners mutex poisoned");
        let holder = owners.get(&pid).cloned();
        let Some(parent) = holder.as_ref().or(self.parent_process_cpu_time.as_ref()) else {
            return true;
        };
        let child = self
            .process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned")
            .snapshot;
        {
            let mut parent = parent
                .lock()
                .expect("parent process CPU time mutex poisoned");
            if let Some(publication) = &self.parent_cpu_publication
                && !parent
                    .child_cpu_publications
                    .get(&pid)
                    .is_some_and(|current| Arc::ptr_eq(current, publication))
            {
                return false;
            }
            parent.record_exited_child(pid, child);
        }
        // No waker runs under the registry's lock either.
        drop(owners);
        if let Some(publication) = &self.parent_cpu_publication {
            publication.finish(true);
        }
        true
    }

    /// Called once at the process child's ThreadState birth. The later external
    /// registration's legacy prepare_child call must not replace this token.
    pub(crate) fn prepare_child_cpu_publication(&self, pid: DetPid) -> Arc<ChildCpuPublication> {
        let publication = ChildCpuPublication::pending();
        let previous = {
            let mut owners = self
                .child_cpu_owners
                .lock()
                .expect("child CPU owners mutex poisoned");
            let mut parent = self
                .process_cpu_time
                .lock()
                .expect("process CPU time mutex poisoned");
            parent.prepare_child(pid);
            owners.insert(pid, Arc::clone(&self.process_cpu_time));
            parent
                .child_cpu_publications
                .insert(pid, Arc::clone(&publication))
        };
        if let Some(previous) = previous {
            previous.finish(false);
        }
        publication
    }

    /// Take over `pid`'s final-CPU publication, and the snapshot already
    /// recorded for it, from the accounting that holds it
    /// (`child_cpu_owners`), when this process does not hold it: an exit
    /// re-parented `pid` to this process, and the scheduler selected it for
    /// this process's wait. Linux adds a reaped child's times to its reaper's
    /// (`wait_task_zombie`), whoever created it. The choice of parent is the
    /// scheduler's; this only moves the record to the waiting process.
    fn adopt_child_cpu_publication(&self, pid: DetPid) {
        let mut owners = self
            .child_cpu_owners
            .lock()
            .expect("child CPU owners mutex poisoned");
        let Some(holder) = owners.get(&pid).cloned() else {
            return;
        };
        if Arc::ptr_eq(&holder, &self.process_cpu_time) {
            return;
        }
        let (publication, snapshot) = {
            let mut holder = holder.lock().expect("process CPU time mutex poisoned");
            let Some(publication) = holder.child_cpu_publications.remove(&pid) else {
                return;
            };
            (publication, holder.exited_children.remove(&pid))
        };
        {
            let mut own = self
                .process_cpu_time
                .lock()
                .expect("process CPU time mutex poisoned");
            own.child_cpu_publications.insert(pid, publication);
            if let Some(snapshot) = snapshot {
                own.exited_children.insert(pid, snapshot);
            }
        }
        owners.insert(pid, Arc::clone(&self.process_cpu_time));
    }

    pub(crate) async fn wait_for_child_cpu_publication(&self, pid: DetPid) -> bool {
        self.adopt_child_cpu_publication(pid);
        let publication = {
            let parent = self
                .process_cpu_time
                .lock()
                .expect("process CPU time mutex poisoned");
            let Some(publication) = parent.child_cpu_publications.get(&pid) else {
                return false;
            };
            Arc::clone(publication)
        };
        // Subscription and publication share the token mutex. Registration
        // followed by a state recheck cannot lose a publication wake. Neither
        // this mutex nor the accounting mutex survives a Pending return.
        if !publication.wait().await {
            return false;
        }
        let parent = self
            .process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned");
        parent
            .child_cpu_publications
            .get(&pid)
            .is_some_and(|current| Arc::ptr_eq(current, &publication))
            && parent.exited_children.contains_key(&pid)
    }

    pub(crate) fn has_exited_child_process_cpu_time(&self, pid: DetPid) -> bool {
        self.process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned")
            .exited_children
            .contains_key(&pid)
    }

    /// A backend that runs a clone natively (an application backend, through
    /// `hermit::app::backend_hook`) is about to: from here until its report
    /// of the child (`Detcore::register_external_child`), the clone may have
    /// made a task the run's record does not name, so it counts as one
    /// (`LiveTasks::begin_clone`). Such a backend calls this before every
    /// native clone; a report of a clone it did not announce leaves the
    /// record unknown, and binder never alone (`LiveTasks::end_reported_clone`).
    /// A native clone that fails, and so is never reported, stays counted.
    pub fn begin_native_clone(&self) {
        self.live_tasks
            .lock()
            .expect("live tasks mutex poisoned")
            .begin_clone(self.dettid);
    }

    pub(crate) fn reap_child_process_cpu_time(&mut self, pid: DetPid) {
        // Every reap of a child's termination comes here, in the waiter's
        // turn.
        self.live_tasks
            .lock()
            .expect("live tasks mutex poisoned")
            .reaped(pid);
        self.account_process_cpu_time();
        let publication = {
            let mut parent = self
                .process_cpu_time
                .lock()
                .expect("process CPU time mutex poisoned");
            // Preserve legacy missing-snapshot behavior. The KVM waitid path
            // requires the snapshot before its irreversible completion.
            let had_snapshot = parent.exited_children.contains_key(&pid);
            parent.reap_child(pid);
            if had_snapshot {
                parent.child_cpu_publications.remove(&pid)
            } else {
                None
            }
        };
        if publication.is_some() {
            let mut owners = self
                .child_cpu_owners
                .lock()
                .expect("child CPU owners mutex poisoned");
            if owners
                .get(&pid)
                .is_some_and(|holder| Arc::ptr_eq(holder, &self.process_cpu_time))
            {
                owners.remove(&pid);
            }
        }
        if let Some(publication) = publication {
            publication.finish(false);
        }
    }

    pub(crate) fn prepare_child_process_cpu_time(&self, pid: DetPid) {
        let mut parent = self
            .process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned");
        // External registration may repeat preparation after ThreadState birth.
        // Only that birth replaces a KVM token; registration must preserve both
        // a pending token and any already-published snapshot for this birth.
        if !parent.child_cpu_publications.contains_key(&pid) {
            parent.prepare_child(pid);
        }
    }

    /// Create a fresh new thread state from nothing.  In practice this is only used for the thread
    /// state of the root thread of the container.
    pub fn new(pid: DetPid, cfg: &Config, record_or_replay: T) -> Self {
        crate::detlog::root_seed_records(pid.as_raw(), cfg.rng_seed(), cfg.sched_seed());
        let thread_logical_time = DetTime::new(cfg);
        let last_accounted_user_time = thread_logical_time.user_cpu_time();
        let last_accounted_system_time = thread_logical_time.system_cpu_time();
        let file_metadata = if cfg.backend.tool_shares_guest_descriptor_table {
            let mut metadata = FileMetadata::new(pid);
            metadata.note_proc_level(cfg.proc_level);
            for fd in 0..=2 {
                match metadata.discover_fd_from_current_process(pid, fd) {
                    Ok(()) | Err(Errno::EBADF) => {}
                    Err(error) => panic!("unable to discover guest stdio {fd}: {error}"),
                }
            }
            metadata
        } else {
            let mut metadata =
                FileMetadata::new(pid).setup_stdio(pid.into(), pid, cfg.standard_stream_aliases);
            metadata.note_proc_level(cfg.proc_level);
            metadata
        };
        let process_files = Some(file_metadata.files_id);
        ThreadState {
            dettid: pid,
            detpid: None, // Initialized later.
            thread_start_entered: false,
            vfork_origin: None, // The root thread.
            physical_tid: None,
            signal_task_identity: None,
            open_file_creator: None,
            receive_geometry: None,
            flock_waiting_description: None,
            entry_import: None,
            send_inspection: None,
            inotify_queue: None,
            call_parts: None,
            traced_call: Default::default(),
            watch: Default::default(),
            mm_id: MmId::initial(pid),
            memory_metadata: Arc::new(Mutex::new(MemoryMetadata::new())),
            pedigree: Pedigree::new(), // Root thread.
            stats: ThreadStats::new(),
            file_metadata: Arc::new(Mutex::new(file_metadata)),
            tool_shares_guest_descriptor_table: cfg.backend.tool_shares_guest_descriptor_table,
            timer_slack_ns: DEFAULT_TIMER_SLACK_NS,
            default_timer_slack_ns: DEFAULT_TIMER_SLACK_NS,
            nested_pid_namespace: false,
            changed_root: Default::default(),
            fs_apart_from_leader: false,
            exe: None,
            exec_effective_ids: None,
            exec_permitted_before: None,
            holds_parent_death_signal: false,
            process_files,
            posix_timers: Arc::new(Mutex::new(PosixTimers::default())),
            resource_limits: Arc::new(Mutex::new(ResourceLimits::default())),
            process_cpu_time: Arc::new(Mutex::new(ProcessCpuTime::default())),
            guest_clock: Arc::new(Mutex::new(GuestClock::default())),
            unix_connections: Default::default(),
            live_tasks: LiveTasks::of_first_task(pid, pid),
            parent_process_cpu_time: None,
            child_cpu_owners: Default::default(),
            parent_cpu_publication: None,
            restart_block: None,
            woken_wait: Default::default(),
            syscall_cwd: None,
            sigalrm_handled: false,
            signal_shadow: None,
            signal_shadow_lost: false,
            delivery_interruption: None,
            wait_signal_end: None,
            pidfd_respelling: None,
            delivery_frame_handler: 0,
            delivery_frame_signal: 0,
            delivery_frame_orig_ax: 0,
            delivery_counters: Default::default(),
            published_mask: Some(None),
            published_dispositions: Arc::default(),
            sigalrm_blocked_published: false,
            handler_restore_rt_reported: 0,
            signal_register_projection: None,
            last_accounted_user_time,
            last_accounted_system_time,
            thread_cpu_start_user_time: last_accounted_user_time,
            thread_cpu_start_system_time: last_accounted_system_time,
            clone_flags: None,
            pending_vfork: None,
            restarted_clone_id: None,
            procfs_sequence_taken: None,
            // For the root thread, we initialize from the seed in the config:
            prng: crate::random::root_prng(cfg.rng_seed()),
            initialized_random_auxv: None,
            chaos_prng: Pcg64Mcg::seed_from_u64(cfg.sched_seed()),
            thread_logical_time,
            committed_clock_value: 0,
            uncharged_bootstrap_syscalls: 0,
            in_uncharged_bootstrap_syscall: false,
            passed_descriptors_out_unreported: false,
            passed_descriptors_out_reported: false,
            nonblocking_result_normalized: false,
            yield_after_futex_wake: false,
            pipe_handoff_due: None,
            own_signal_turn_hold: None,
            delivered_since_request: false,
            returned_from_sigreturn: false,
            delivery_entered: false,
            signal_announcements: Vec::new(),
            next_signal_announcement: 0,
            consecutive_continuations: 0,
            scheduler_grants: 0,
            host_timed_binder_loss_recorded: false,
            shared_binder_loss_recorded: false,
            unrecorded_binder_loss_recorded: false,
            end_of_timeslice: None, // Temporary/bogus.
            replay_rcb_end: None,
            last_replay_change_point: None,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1151)
            chaos_epoch: chaos_epoch_sentinel(),
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1151)
            chaos_slowdown_factor: RcbTimeMultiplier::ONE,
            chaos_slowdown_active: false,
            claimed_sigchld: false,
            pending_chaos_epochs: Vec::new(),
            max_timeslice_end: None,
            post_crash_bound: None,
            crash_armed: None,
            uncapped_backstop: None,
            last_grant_deadline: None,
            published_runtime_private_memory: None,
            published_cpu: None,
            external_parent_checked: false,
            last_rcb_timer: None,
            last_rcb_timer_is_max: false,
            lone_grant: false,
            lone_slice_end: None,
            replaying_lone_slice_end: None,
            lone_slice_end_at_check_in: false,
            lone_replay_unsound: false,
            replayed_stopless_end: None,
            armed_instruction_offset: None,
            slice_instruction_offset: 0,
            replay_instruction_offset: None,
            chaos_instruction_offset_draws: 0,
            replay_backstop: None,
            record_or_replay,
            preemption_points: None,
            past_global_first_execve: false,
            interrupt_at: cfg.interrupts_for_thread(pid),
            robust_list_head: None,
            robust_list_process: Arc::new(Mutex::new(RobustListProcessState::default())),
            exec_robust_commit: None,
            slice_boundary: None,
            callback_history: Default::default(),
            schedule_replay_over: false,
        }
    }

    /// Select the current image before its first registration, only from the
    /// authenticated initial-image Tool hook. No registered identity is rekeyed.
    pub(crate) fn bind_initial_current_image(&mut self, tid: DetTid) -> Result<(), Errno> {
        if tid != self.dettid
            || self.detpid.is_some()
            || self.thread_start_entered
            || self.past_global_first_execve
            || self.mm_id != MmId::initial(tid)
            || self.clone_flags.is_some()
            || self.pending_vfork.is_some()
            || self.pedigree.raw() != Pedigree::new().raw()
            || self.published_runtime_private_memory.is_some()
        {
            return Err(Errno::EPROTO);
        }
        self.mm_id = self.mm_id.for_exec(tid);
        Ok(())
    }

    /// Apply a required, authenticated initial-image random handoff to the
    /// normal root. Errors are tool initialization failures, never guest errno.
    pub fn apply_initial_random_state(
        &mut self,
        bytes: &[u8],
        config: &Config,
        image: crate::random::InitialImage,
    ) -> Result<(), Errno> {
        if self.initialized_random_auxv.is_some()
            || self.past_global_first_execve
            || self.dettid.as_raw() != image.pid
            || self.clone_flags.is_some()
            || self.pedigree.raw() != Pedigree::new().raw()
        {
            return Err(Errno::EPROTO);
        }
        let (prng, at_random_value, early_requests) =
            crate::random::decode_initial_state(bytes, config, image)?;
        self.prng = prng;
        self.initialized_random_auxv = Some((image, at_random_value, early_requests));
        Ok(())
    }

    /// Consume the early auxv write's fact, returning the bytes it wrote and
    /// the requests served before post-exec, in order, or `None` when no
    /// backend wrote AT_RANDOM before post-exec.
    pub(crate) fn complete_initial_random_auxv(
        &mut self,
        pointer: Option<usize>,
    ) -> Result<Option<crate::random::EarlyRandomRecords>, Errno> {
        let Some((image, _, _)) = self.initialized_random_auxv.as_ref() else {
            return Ok(None);
        };
        if pointer != Some(image.at_random) || self.dettid.as_raw() != image.pid {
            return Err(Errno::EPROTO);
        }
        let (_, at_random_value, early_requests) =
            self.initialized_random_auxv.take().expect("checked above");
        Ok(Some((at_random_value, early_requests)))
    }

    pub(crate) fn record_robust_list_head(&mut self, head: Option<usize>) {
        self.robust_list_head = head;
        let mut process = self
            .robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned");
        if let Some(head) = head {
            process.heads.insert(self.dettid, head);
        } else {
            process.heads.remove(&self.dettid);
        }
    }

    /// Record that Detcore has walked this thread's own robust list at its
    /// exit (`OwnerDeathPath::Barrier`), so a later exit_group of its group
    /// does not walk the list again ([`exit_group_robust_list_owners`]).
    pub(crate) fn mark_robust_list_walked_by_tool(&self) {
        self.robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned")
            .tool_walked
            .insert(self.dettid);
    }

    /// The same for every owner a group death walked
    /// (`OwnerDeathPath::Detcore`): each thread's death walks its list once,
    /// however many paths of the one death reach a walk.
    pub(crate) fn mark_robust_lists_walked_by_tool(&self, owners: &[RobustListOwner]) {
        self.robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned")
            .tool_walked
            .extend(owners.iter().map(|owner| owner.dettid));
    }

    /// Note that an exit_group on DBT or KVM (`OwnerDeathPath::Barrier`,
    /// `OwnerDeathPath::Detcore`; on KVM also a SIGKILL a thread sends its own
    /// process) has been filed. From now on a thread the exit_group's grant kills waits for its
    /// walk before its native exit ([`Self::wait_for_exit_group_robust_walk`]),
    /// and a thread of the group that exits keeps its head registered for the
    /// walk, so the walk sees the registrations current at the grant even if
    /// an exit hook runs first. A registration, replacement or unregistration
    /// a thread makes still takes effect.
    ///
    /// Nothing clears the filing: the thread group ends with the exit_group.
    /// The one exception is an exec granted first, which kills the filer.
    /// Where exec keeps this shared state for the new image, killed threads'
    /// old heads would then stay listed for a later exit_group. DBT and KVM
    /// file. On DBT the exec replaces the in-guest runtime's memory, this
    /// state included; modeling exec on DBT
    /// (<https://github.com/rrnewton/hermit/issues/4268>) must keep it so. On
    /// KVM a successful exec gives the new image a state of its own
    /// ([`Self::start_robust_list_process_for_exec`]).
    pub(crate) fn file_exit_group_for_robust_lists(&self) {
        let mut process = self
            .robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned");
        process.exit_group_filed = true;
        process.exit_group_filers.insert(self.dettid);
    }

    /// Note that this thread's exit_group was granted and that it now walks
    /// the group's lists. Only this thread does not wait for the walk: any
    /// other thread, including one that filed an exit_group of its own and
    /// was killed by this one's grant, waits.
    pub(crate) fn begin_exit_group_robust_walk(&self) {
        self.robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned")
            .exit_group_walker = Some(self.dettid);
    }

    /// Note that this thread was killed, so any exit_group it filed can never
    /// be granted. Once every filing is cancelled this way with none granted
    /// (an exec ended the group first), no walk will come, and the threads
    /// waiting for one are released ([`Self::wait_for_exit_group_robust_walk`]).
    pub(crate) fn cancel_exit_group_filing(&self) {
        self.robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned")
            .exit_group_filers
            .remove(&self.dettid);
    }

    /// Note that the filed exit_group's walk has finished, releasing the
    /// threads it killed ([`Self::wait_for_exit_group_robust_walk`]).
    pub(crate) fn finish_exit_group_robust_walk(&self) {
        self.robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned")
            .exit_group_walked = true;
    }

    /// Where an exit_group of this thread's group is filed and this thread is
    /// not walking it, wait until that exit_group's walk has finished, or until
    /// every filing has been cancelled with none granted, at most `bound`.
    /// Returns false if the bound expired first, which the caller treats as a
    /// failure of the run, never as permission to go on.
    ///
    /// A thread the exit_group kills retires with a native exit. On DBT that
    /// exit first rewrites the thread's robust-list owner words to its host
    /// TID and then lets Linux mark them, which would race the walk: the walk
    /// could find a word that no longer names its owner, and could not tell
    /// whose it was. Waiting makes the walk see every word as the guest left
    /// it. That includes a thread that filed an exit_group of its own and was
    /// killed by another's grant; only the thread whose exit_group was granted
    /// ([`Self::begin_exit_group_robust_walk`]) does not wait. The wait is on
    /// host time, but nothing guest-visible depends on how long it takes: the
    /// thread is already logically dead. A walk that never comes, because an
    /// exec killed every filer first, is recognised when the last filing is
    /// cancelled ([`Self::cancel_exit_group_filing`]). Only DBT and KVM file an
    /// exit_group. On DBT each guest thread polls its own callbacks, and on KVM
    /// each guest thread's callbacks run on a host worker of its own, so the
    /// blocking wait holds up only this thread. On KVM no native exit marks a
    /// word, so the wait is not needed there for the walk's reads, only
    /// harmless: the walk's registrations are kept by the filing itself.
    pub(crate) fn wait_for_exit_group_robust_walk(&self, bound: std::time::Duration) -> bool {
        let started = std::time::Instant::now();
        loop {
            {
                let process = self
                    .robust_list_process
                    .lock()
                    .expect("robust-list process state mutex poisoned");
                if !process.exit_group_filed
                    || process.exit_group_walked
                    || process.exit_group_walker == Some(self.dettid)
                    || (process.exit_group_walker.is_none() && process.exit_group_filers.is_empty())
                {
                    return true;
                }
            }
            if started.elapsed() >= bound {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }

    /// The exit hook's counterpart of `record_robust_list_head(None)`: the
    /// thread is gone, so its own head is cleared, but while an exit_group is
    /// filed ([`Self::file_exit_group_for_robust_lists`]) its registration
    /// stays in the group's index.
    pub(crate) fn drop_robust_list_head_at_exit(&mut self) {
        self.robust_list_head = None;
        let mut process = self
            .robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned");
        if !process.exit_group_filed {
            process.heads.remove(&self.dettid);
        }
    }

    /// The registrations an exit_group walks on DBT or KVM
    /// (`OwnerDeathPath::Barrier`, `OwnerDeathPath::Detcore`), read at its
    /// grant: every registration current then, including
    /// those of threads the grant killed, less any thread whose own exit
    /// Detcore already replayed, so no list is walked twice.
    pub(crate) fn robust_list_owners_for_exit_group(&self) -> Vec<RobustListOwner> {
        let walked = self
            .robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned")
            .tool_walked
            .clone();
        self.robust_list_owners()
            .into_iter()
            .filter(|owner| !walked.contains(&owner.dettid))
            .collect()
    }

    /// [`Self::robust_list_heads`] as [`RobustListOwner`]s.
    pub(crate) fn robust_list_owners(&self) -> Vec<RobustListOwner> {
        let process = self
            .robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned");
        process
            .heads
            .iter()
            .map(|(&dettid, &head)| RobustListOwner { dettid, head })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn robust_list_heads(&self) -> Vec<(DetTid, usize)> {
        self.robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned")
            .heads
            .iter()
            .map(|(&tid, &head)| (tid, head))
            .collect()
    }

    pub(crate) fn stage_robust_list_wakes(
        &self,
        reason: RobustListExit,
        wakes: Vec<(DetTid, Vec<RobustListWake>)>,
    ) {
        let mut process = self
            .robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned");
        process.pending = wakes
            .into_iter()
            .map(|(owner, wakes)| (owner, PendingRobustListWakes { reason, wakes }))
            .collect();
        process.matched_physical_exits.clear();
    }

    pub(crate) fn has_matching_robust_list_exit(&self, exit_signal: Option<i32>) -> bool {
        self.robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned")
            .pending
            .get(&self.dettid)
            .is_some_and(|pending| pending.matches_exit(exit_signal))
    }

    pub(crate) fn take_robust_list_wakes_after_exit(
        &self,
        exit_signal: Option<i32>,
        exit_time: DetTime,
    ) -> Option<(DetTime, Vec<(DetTid, RobustListWake)>)> {
        let mut process = self
            .robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned");
        if !process.exit_group_filed {
            process.heads.remove(&self.dettid);
        }
        let pending = process.pending.get(&self.dettid)?;
        if !pending.matches_exit(exit_signal) {
            return None;
        }

        process
            .matched_physical_exits
            .insert(self.dettid, exit_time);
        if process
            .pending
            .keys()
            .any(|owner| !process.matched_physical_exits.contains_key(owner))
        {
            return None;
        }

        let request_time = process
            .matched_physical_exits
            .values()
            .max()
            .expect("a completed robust-list exit group has a physical-exit time")
            .clone();
        process.matched_physical_exits.clear();
        let pending = std::mem::take(&mut process.pending);
        let mut ready = Vec::new();
        for (owner, mut pending) in pending {
            pending
                .wakes
                .sort_by_key(|wake| format!("{:?}", wake.futex));
            ready.extend(pending.wakes.into_iter().map(|wake| (owner, wake)));
        }
        Some((request_time, ready))
    }

    /// Hold the owner-death wakes a successful `execve` makes Linux deliver.
    ///
    /// Past exec's point of no return, `de_thread` kills every other thread,
    /// and each one's exit runs `futex_exit_release` with its own TID; then
    /// `exec_mm_release` runs `futex_exec_release` for the caller itself
    /// (kernel/fork.c `exit_mm_release`/`exec_mm_release`, kernel/futex/core.c
    /// `futex_cleanup`, Linux 7.1.3). Both walk the robust list with
    /// `exit_robust_list` before the old address space goes away. A caller that
    /// is not the group leader already holds the leader's TID by then
    /// (`exchange_tids` in `de_thread`), so its own list is walked with the
    /// thread group ID. The lists can only be read before the old address
    /// space goes, so the wakes are staged here for
    /// `take_robust_list_wakes_for_exec`: at the exec's entry where Linux's
    /// exec walks the lists (ptrace), and at the exec's point of no return
    /// where Detcore walks them itself (KVM,
    /// `Detcore::commit_exec_robust_lists`). A failed exec delivers none.
    pub(crate) fn stage_robust_list_wakes_for_exec(
        &self,
        wakes: Vec<(DetTid, Vec<RobustListWake>)>,
    ) {
        let mut process = self
            .robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned");
        process.exec_pending = flatten_exec_robust_list_wakes(wakes);
    }

    /// Take the wakes staged for an exec (`stage_robust_list_wakes_for_exec`):
    /// to deliver them once it has succeeded, or to drop them when it failed
    /// and left the old image running.
    pub(crate) fn take_robust_list_wakes_for_exec(&self) -> Vec<(DetTid, RobustListWake)> {
        std::mem::take(
            &mut self
                .robust_list_process
                .lock()
                .expect("robust-list process state mutex poisoned")
                .exec_pending,
        )
    }

    /// Clear the robust-list head for a candidate `execve` image, returning the
    /// previous value so a *failed* exec can put it back.
    ///
    /// Linux clears `task->robust_list` when `execve` succeeds, and doing the
    /// same is load-bearing rather than tidy: a head recorded against the old
    /// address space is a wild pointer in the new one, and consulting it at
    /// thread exit would walk unrelated guest memory and write
    /// `FUTEX_OWNER_DIED` into whatever happened to look like a futex word.
    pub(crate) fn take_robust_list_for_exec(&mut self) -> Option<usize> {
        let previous = self.robust_list_head.take();
        self.robust_list_process
            .lock()
            .expect("robust-list process state mutex poisoned")
            .heads
            .remove(&self.dettid);
        previous
    }

    /// Restore the head after an `execve` that failed and left the old address
    /// space intact.
    pub(crate) fn restore_robust_list_after_failed_exec(&mut self, previous: Option<usize>) {
        self.record_robust_list_head(previous);
    }

    /// Give the new image of a successful exec a robust-list state of its own
    /// (`Detcore::exec_starts_a_new_robust_list_process`). The exec'ing thread
    /// is its process's only thread now and has registered no list yet; the
    /// threads the exec killed keep the old state for their exit hooks. Called
    /// after the exec's staged wakes were taken
    /// ([`Self::take_robust_list_wakes_for_exec`]).
    pub(crate) fn start_robust_list_process_for_exec(&mut self) {
        self.robust_list_head = None;
        self.robust_list_process = Arc::new(Mutex::new(RobustListProcessState::default()));
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

    /// Record that the address space allocated or applied a protection key
    /// (`MemoryMetadata::protection_keys`).
    pub(crate) fn note_protection_keys(&self) {
        self.memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .protection_keys = true;
    }

    /// Whether the address space has allocated or applied a protection key.
    pub(crate) fn may_use_protection_keys(&self) -> bool {
        self.memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .protection_keys
    }

    /// Record that a call that sets mapping flags the backend's memory map
    /// does not list reached the backend for a mapping that map lists
    /// (`MemoryMetadata::mapping_flags_changed`).
    pub(crate) fn note_mapping_flags_changed(&self) {
        self.memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .mapping_flags_changed = true;
    }

    /// Whether such a call reached the backend for the address space.
    pub(crate) fn mapping_flags_may_have_changed(&self) -> bool {
        self.memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .mapping_flags_changed
    }

    /// Record that a call that discards pages reached the backend for a
    /// mapping the backend's memory map lists
    /// (`MemoryMetadata::pages_discarded`).
    pub(crate) fn note_pages_discarded(&self) {
        self.memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .pages_discarded = true;
    }

    /// Whether such a call reached the backend for the address space.
    /// Test-only now: its last production reader was the rendered
    /// `numa_maps`, which the no-NUMA platform no longer serves; the
    /// record itself still counts against native-exec freshness.
    #[cfg(test)]
    pub(crate) fn pages_may_have_been_discarded(&self) -> bool {
        self.memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .pages_discarded
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
        keep_source: bool,
    ) {
        self.memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .remap(old_start, old_len, new_start, new_len, keep_source);
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
            signal_interrupt_errno: None,
            backend_runtime_bootstrap: false,
            blocked_signal_mask: None,
            blocked_signal_mask_unknown: false,
            replay_served_from_log: false,
            reads_signalfd: false,
            read_fd: None,
            polls_guest_state_only: false,
            polls_a_named_fifo: false,
            relies_on_guest_held_marks: false,
            passes_descriptors_out: false,
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
            guest_signal_mask: None,
            callback_checkpoint: None,
            priority_row: None,
            slice_expired: false,
            zero_length_sleep: false,
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

    /// The descriptors this thread's table holds, ascending, modelled or not
    /// (`FileMetadata::descriptor_numbers`), its capacity
    /// (`FileMetadata::descriptor_capacity`), and whether a receive may have
    /// installed descriptors Detcore does not know
    /// (`FileMetadata::unaccounted_receipts`).
    pub(crate) fn descriptor_table(&self) -> (Vec<RawFd>, u64, bool) {
        let metadata = self.metadata();
        (
            metadata.descriptor_numbers(),
            metadata.descriptor_capacity,
            metadata.unaccounted_receipts,
        )
    }

    /// Record that descriptor `fd`, close-on-exec or not, is open in this
    /// thread's table though Detcore keeps no description of it
    /// (`FileMetadata::present_unmodelled`).
    pub(crate) fn note_unmodelled_descriptor(&self, fd: RawFd, cloexec: bool) {
        let mut metadata = self.metadata();
        if !metadata.file_handles.contains_key(&fd) {
            metadata.note_installed(fd);
            metadata.present_unmodelled.insert(fd, cloexec);
        }
    }

    /// Grow this thread's table to hold descriptor `fd`, as a `dup2` or
    /// `dup3` to it of a descriptor that is not open does on Linux
    /// (`FileMetadata::grow_to_hold`): `newfd` below `RLIMIT_NOFILE`, as
    /// Detcore keeps it for the process.
    pub(crate) fn note_failed_dup_target(&self, fd: RawFd) {
        let limit = self
            .resource_limits
            .lock()
            .unwrap()
            .get(reverie::syscalls::linux_abi::RLIMIT_NOFILE)
            .map_or(0, |limit| limit.current);
        if u64::try_from(fd).is_ok_and(|fd| fd < limit) {
            self.metadata().grow_to_hold(fd);
        }
    }

    /// Set the close-on-exec flag of descriptor `fd` if Detcore lists it
    /// without modelling it (`FileMetadata::present_unmodelled`), after the
    /// kernel changed it (`F_SETFD`, `FIOCLEX`, `FIONCLEX`); whether it is
    /// one.
    pub(crate) fn set_unmodelled_cloexec(&self, fd: RawFd, cloexec: bool) -> bool {
        self.metadata().set_unmodelled_cloexec(fd, cloexec)
    }

    /// Record that a receive may have installed descriptors in this thread's
    /// table whose numbers Detcore does not know
    /// (`FileMetadata::unaccounted_receipts`).
    pub(crate) fn note_unaccounted_receipt(&self) {
        self.metadata().unaccounted_receipts = true;
    }

    /// Identity of this thread's descriptor table.
    pub(crate) fn files_id(&self) -> detcore_model::fd::FilesId {
        self.metadata().files_id
    }

    /// Whether this thread holds the descriptor table its process holds
    /// ([`ThreadState::process_files`]), the one procfs reads for the
    /// process's descriptor links. `false` where that is not known.
    pub(crate) fn holds_process_files(&self) -> bool {
        self.process_files == Some(self.files_id())
    }

    /// Make the descriptor table this thread holds its process's, for a new
    /// group: after a successful exec the exec'ing thread is its process's
    /// only thread, and leads it, whether or not it led it before.
    pub(crate) fn adopt_process_files(&mut self) {
        self.process_files = Some(self.files_id());
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
        let mut metadata = self.metadata();
        metadata.add_fd(
            self.open_file_creator.unwrap_or(self.dettid),
            fd,
            flags,
            ty,
            stat,
        )?;
        if self.traced_call.is_active() {
            self.traced_call
                .note(metadata.slot_note(fd, FdNoteOp::Open));
        }
        Ok(())
    }

    /// Bring the open note [`Self::add_fd`] made for descriptor `fd` up to
    /// date once the open's handler has set the descriptor's path, which it
    /// does after the install: the note then names the file's path as well
    /// as its kind.
    pub(crate) fn note_opened_path(&self, fd: RawFd) {
        if self.traced_call.is_active() {
            let note = self.metadata().slot_note(fd, FdNoteOp::Open);
            self.traced_call.replace_open_note(note);
        }
    }

    /// Record descriptor `fd`, which Linux installed by a route Detcore did not
    /// model as it ran (`SCM_RIGHTS`), from the kernel's facts about it, as an
    /// in-guest backend records a descriptor it discovers
    /// (`FileMetadata::add_discovered_fd`).
    pub(crate) fn add_discovered_fd(
        &self,
        fd: RawFd,
        fd_flags: libc::c_int,
        status_flags: libc::c_int,
        raw_stat: &libc::stat,
        link: Option<&[u8]>,
    ) -> Result<(), Errno> {
        let mut metadata = self.metadata();
        metadata.add_discovered_fd(
            self.open_file_creator.unwrap_or(self.dettid),
            fd,
            fd_flags,
            status_flags,
            raw_stat,
            link,
        )?;
        if self.traced_call.is_active() {
            self.traced_call
                .note(metadata.slot_note(fd, FdNoteOp::Open));
        }
        Ok(())
    }

    /// Make `fd`, which this thread just opened, a random device
    /// (`FileMetadata::classify_random_device`).
    pub(crate) fn classify_random_device(&self, fd: RawFd) -> Result<(), Errno> {
        self.metadata()
            .classify_random_device(self.open_file_creator.unwrap_or(self.dettid), fd)
    }

    /// This table's count of random-device opens, for the exec exchange
    /// (`ExecFdState::random_device_sequence`).
    pub(crate) fn random_device_sequence_for_exec(&self) -> u64 {
        self.metadata().next_random_device_sequence
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1063): Review backend-supplied open-file creator identity.
    /// Overrides the task identity embedded in subsequently allocated open-file descriptions.
    pub fn set_open_file_creator(&mut self, creator: DetTid) {
        self.open_file_creator = Some(creator);
    }

    /// This process's timerfds that Detcore keeps on virtual time
    /// (`crate::fd::VirtualTimerfd`), one descriptor per open file
    /// description, lowest descriptor number first, so callers visit them in
    /// a deterministic order.
    pub(crate) fn virtual_timerfds(&self) -> Vec<(RawFd, DetFd)> {
        let mut timerfds: Vec<(RawFd, DetFd)> = self
            .metadata()
            .file_handles
            .iter()
            .filter(|(_, detfd)| detfd.ty() == FdType::Timerfd && detfd.virtual_timerfd().is_some())
            .map(|(&fd, detfd)| (fd, detfd.clone()))
            .collect();
        timerfds.sort_unstable_by_key(|(fd, _)| *fd);
        let mut descriptions = Vec::with_capacity(timerfds.len());
        timerfds.retain(|(_, detfd)| {
            let id = detfd.open_file_id();
            let first = !descriptions.contains(&id);
            descriptions.push(id);
            first
        });
        timerfds
    }

    /// Whether this thread's descriptor table holds a signalfd, as Detcore
    /// records its descriptors.
    pub(crate) fn holds_a_signalfd(&self) -> bool {
        self.metadata()
            .file_handles
            .values()
            .any(|detfd| matches!(detfd.ty(), FdType::Signalfd))
    }

    /// Whether this thread's descriptor table holds a procfs file Detcore
    /// renders (`syscalls::files::rendered_procfs`).
    pub(crate) fn holds_a_rendered_procfs_file(&self) -> bool {
        self.metadata()
            .file_handles
            .values()
            .any(|detfd| detfd.procfs_rendered_path().is_some())
    }

    /// Get a mutable reference of `DetFd` from a raw file descriptor, and
    /// run mutable function `f` on it (`&mut DetFd`).
    pub fn with_detfd<F, U>(&self, fd: RawFd, f: F) -> Result<U, Errno>
    where
        F: FnMut(&mut DetFd) -> U,
    {
        let mut metadata = self.metadata();
        if self.tool_shares_guest_descriptor_table {
            metadata.discover_fd_from_current_process(
                self.open_file_creator.unwrap_or(self.dettid),
                fd,
            )?;
        }
        metadata.with_detfd(fd, f)
    }

    /// Preserve logical blocking for scheduler-managed descriptors when exec
    /// replaces an in-process tool and its descriptor metadata.
    pub fn exec_blocking_overrides(&self) -> ExecFdBlockingOverrides {
        self.metadata().exec_blocking_overrides()
    }

    /// Promote the OFDs whose model must stay one copy -- random devices
    /// (their cursor) and timerfds (their virtual-time expirations,
    /// `crate::fd::VirtualTimerfd`) -- before a backend physically copies or
    /// replaces its in-process Tool. The shared model owns their full state;
    /// this does not promote sockets or other OFDs. The record sequences of
    /// per-process files move to Detcore's global state on their own
    /// (`share_procfs_sequences`, first). Call only while this task
    /// owns its serial grant, before the native lifecycle operation can expose
    /// a child/new image. Ptrace's host-side Arc needs no promotion. See
    /// <https://github.com/rrnewton/hermit/issues/4381>.
    pub fn promote_shared_open_files(&self) -> Result<(), crate::fd::SharedOpenFileError> {
        self.share_procfs_sequences();
        let mut metadata = self.metadata();
        let mut fds: Vec<_> = metadata.file_handles.keys().copied().collect();
        fds.sort_unstable();
        for fd in fds {
            let handle = &metadata.file_handles[&fd];
            if matches!(handle.ty(), FdType::Rng | FdType::Timerfd) {
                handle.promote()?;
                // Keep history even if every promoted descriptor later closes.
                metadata.has_promoted_shared_files = true;
            }
        }
        Ok(())
    }

    /// Before a backend whose tool runs in each guest process physically
    /// copies this process (a fork) or replaces its image: every description
    /// of a per-process file that follows its record's sequence keeps that
    /// sequence in Detcore's global state from then on, so the copies a fork
    /// makes share it as Linux's processes share the one `struct file` and
    /// `seq_file` (`ProcfsFile::share_sequence`, `Detcore::take_procfs_sequence`).
    /// Local and RPC-free: the first operation after the fork, in either
    /// process, finds no sequence there yet and keeps its copy, which is the
    /// one the fork made. Runs in the caller's turn, before the copy exists
    /// (`promote_shared_open_files`, and `Detcore::handle_clone_family` for
    /// SaBRe, whose runtime promotes nothing).
    pub fn share_procfs_sequences(&self) {
        let mut metadata = self.metadata();
        let mut shared = false;
        for handle in metadata.file_handles.values() {
            shared |= handle.share_procfs_sequence();
        }
        metadata.has_shared_procfs_sequences |= shared;
    }

    /// Before a send passes descriptor `fd` over `SCM_RIGHTS`, where the tool
    /// runs in each guest process: a rendered per-process file that follows
    /// its record's sequence keeps that sequence in Detcore's global state
    /// from then on, as a fork's copies do (`share_procfs_sequences`). The
    /// identity it is kept under, which the caller then takes and stores
    /// (`Detcore::share_sent_record_sequences`). `None` for any other
    /// descriptor, and for one whose sequence Detcore cannot know here
    /// (`DetFd::record_sequence_unknown`). Local and RPC-free.
    pub(crate) fn share_sent_procfs_sequence(&self, fd: RawFd) -> Option<OpenFileId> {
        let mut metadata = self.metadata();
        let handle = metadata.file_handles.get(&fd)?;
        if handle.is_shared()
            || handle.record_sequence_unknown()
            || handle.procfs_rendered_path().is_none()
            || !handle.share_procfs_sequence()
        {
            return None;
        }
        let id = handle.procfs_shared_sequence();
        metadata.has_shared_procfs_sequences |= id.is_some();
        id
    }

    /// The descriptors of this thread's table open on a rendered procfs file
    /// (`DetFd::procfs_rendered_path`), lowest first. A shared model (a
    /// random device, a timerfd) is never one, and is passed over without a
    /// lease.
    pub(crate) fn rendered_descriptors(&self) -> Vec<RawFd> {
        let metadata = self.metadata();
        let mut fds: Vec<RawFd> = metadata
            .file_handles
            .iter()
            .filter(|(_, detfd)| !detfd.is_shared() && detfd.procfs_rendered_path().is_some())
            .map(|(&fd, _)| fd)
            .collect();
        fds.sort_unstable();
        fds
    }

    /// Rebuild only process-local locks after a physical process copy. This
    /// is RPC-free and precedes the child's first scheduler admission. Ordinary
    /// same-process table clones retain their original shared locks.
    pub fn reset_copied_open_file_locks(&self) {
        let mut metadata = self.metadata();
        let mut handles = metadata
            .file_handles
            .iter()
            .map(|(&fd, handle)| (fd, handle.fork_shared_handle()))
            .collect();
        crate::fd::intern_open_files(&mut handles);
        metadata.file_handles = handles;
    }

    /// Under `--pipe-wake-latency`: the channel end each descriptor that
    /// survives exec is.
    pub(crate) fn pipe_channels_for_exec(&self) -> BTreeMap<RawFd, crate::pipe_wake::ChannelEnd> {
        self.metadata()
            .file_handles
            .iter()
            .filter(|(_, detfd)| !detfd.is_cloexec())
            .filter_map(|(&fd, detfd)| detfd.channel().map(|end| (fd, end)))
            .collect()
    }

    /// The open-file creator and next sequence of this thread's table.
    pub(crate) fn open_file_namespace_for_exec(&self) -> (DetTid, u64) {
        (
            self.open_file_creator.unwrap_or(self.dettid),
            self.metadata().next_open_file_sequence,
        )
    }

    pub(crate) fn shared_open_files_for_exec(&self) -> Result<Option<ExecOpenFiles>, Errno> {
        let metadata = self.metadata();
        if !metadata.has_promoted_shared_files {
            return Ok(None);
        }
        let mut handles = BTreeMap::new();
        for (&fd, handle) in &metadata.file_handles {
            if !handle.is_shared() || handle.is_cloexec() {
                continue;
            }
            assert!(
                matches!(handle.ty(), FdType::Rng | FdType::Timerfd),
                "only random and timerfd OFDs are promoted here"
            );
            handles.insert(fd, ExecOpenFile::capture(fd, handle.open_file_id())?);
        }
        Ok(Some(ExecOpenFiles {
            creator: self.open_file_creator.unwrap_or(self.dettid),
            next_sequence: metadata.next_open_file_sequence,
            next_socket_sequence: metadata.next_socket_open_file_sequence,
            handles,
        }))
    }

    /// Before an exec on a backend that rebuilds its descriptor table after
    /// exec: each surviving descriptor of a per-process file that follows its
    /// record's sequence, with its open file's identity, model and sequence
    /// (`DetFd::procfs_record_for_exec`), so the new image's table names the
    /// same open file as every other process holding it, and a read there
    /// continues where the open file's sequence is (before, the new image
    /// rebuilt it from its placeholder with no sequence: an exec'd child's
    /// read moved the shared position while the other holder kept its own
    /// sequence, and on DBT and SaBRe that holder's next read ran show where
    /// Linux continues the record, Claude's narrow check of the kvm lane's
    /// slice N3, round 5); and this table's open file creator and next
    /// identity, so no description of the new image takes an identity whose
    /// sequence another process may still hold in Detcore's global state
    /// (before, the new image numbered its descriptions from 0 again, Codex's
    /// narrow check, round 5). `None` where the table has no such descriptor
    /// and never shared a sequence (`FileMetadata::has_shared_procfs_sequences`).
    pub(crate) fn procfs_records_for_exec(&self) -> Result<Option<ExecProcfsRecords>, Errno> {
        let metadata = self.metadata();
        let mut handles = BTreeMap::new();
        for (&fd, handle) in &metadata.file_handles {
            if handle.is_cloexec() || handle.is_shared() {
                continue;
            }
            let Some(procfs) = handle.procfs_record_for_exec() else {
                continue;
            };
            let sequence = procfs.sequence();
            handles.insert(
                fd,
                ExecProcfsRecord {
                    identity: ExecOpenFile::capture(fd, handle.open_file_id())?,
                    procfs,
                    sequence,
                    flags: handle.status_flags(),
                    path: handle.path(),
                    sequence_unknown: handle.record_sequence_unknown(),
                },
            );
        }
        if handles.is_empty() && !metadata.has_shared_procfs_sequences {
            return Ok(None);
        }
        Ok(Some(ExecProcfsRecords {
            creator: self.open_file_creator.unwrap_or(self.dettid),
            next_sequence: metadata.next_open_file_sequence,
            handles,
        }))
    }

    pub(crate) fn maps_for_exec(
        &self,
        private_memory: Option<&reverie::RuntimePrivateMemory>,
    ) -> Result<Option<ExecMapsFiles>, Errno> {
        // An unavailable advertised provider still owns saved maps bindings.
        // Backends without a provider retain their existing exec behavior.
        if private_memory.is_none() {
            return Ok(None);
        }
        let metadata = self.metadata();
        let mut handles = BTreeMap::new();
        for (&fd, handle) in &metadata.file_handles {
            if handle.is_cloexec() {
                continue;
            }
            if let Some(procfs) = handle.maps_for_exec() {
                handles.insert(
                    fd,
                    ExecMapsFile {
                        identity: ExecOpenFile::capture(fd, handle.open_file_id())?,
                        procfs,
                        flags: handle.status_flags(),
                        path: handle.path(),
                    },
                );
            }
        }
        Ok((!handles.is_empty()).then_some(ExecMapsFiles {
            creator: self.open_file_creator.unwrap_or(self.dettid),
            next_sequence: metadata.next_open_file_sequence,
            handles,
        }))
    }

    pub(crate) fn inherited_reads_for_exec(
        &self,
    ) -> BTreeMap<RawFd, crate::fd::InheritedExternalRead> {
        self.metadata().inherited_reads_for_exec()
    }

    /// Descriptors in this table that Detcore manages as internal pipes: typed
    /// `Pipe`, physically nonblocking and logically blocking, which is how
    /// `handle_pipe2` leaves a guest-created pipe (and how exec restores it).
    /// On ptrace, a pipe inherited from the host is not in this set: stdio is
    /// typed `Regular`, and another inherited FIFO is physically nonblocking
    /// only when it is logically nonblocking too. On a backend whose tool
    /// shares the guest's descriptor table, a readable pipe inherited on fds
    /// 0-2 is an inherited external read (`discover_inherited_pipes`), so it is
    /// not here either, and a guest `F_SETFL` leaves its kernel status flags
    /// as the guest set them (`keeps_physically_nonblocking`).
    /// Sorted, so callers probe them in a deterministic order.
    pub(crate) fn scheduler_managed_pipe_fds(&self) -> Vec<RawFd> {
        let mut fds: Vec<RawFd> = self
            .metadata()
            .file_handles
            .iter()
            .filter(|(_, detfd)| {
                detfd.ty() == FdType::Pipe
                    && detfd.inherited_external_read().is_none()
                    && detfd.physically_nonblocking()
                    && !detfd.is_nonblocking()
            })
            .map(|(&fd, _)| fd)
            .collect();
        fds.sort_unstable();
        fds
    }

    /// Whether a descriptor of this table other than `fd` names, through a
    /// guest-held description (`DetFd::is_guest_held`), the object at host
    /// device `dev` and inode `inode`: the same socket end, or either end of
    /// the same pipe. Only descriptions whose host `fstat` Detcore keeps can
    /// match.
    pub(crate) fn names_guest_held_object(&self, fd: RawFd, dev: u64, inode: u64) -> bool {
        self.metadata().file_handles.iter().any(|(&other, detfd)| {
            other != fd
                && detfd.is_guest_held()
                && detfd
                    .stat()
                    .is_some_and(|stat| stat.dev == dev && stat.inode == inode)
        })
    }

    /// Mark every listener in this table eligible for accept in turn as passed
    /// over `SCM_RIGHTS` (`DetFd::set_exported`), for a message whose
    /// descriptors Detcore could not read.
    // TODO-HUMAN-REVIEW(PR-3908)
    pub(crate) fn mark_accept_in_turn_listeners_exported(&self) {
        for detfd in self.metadata().file_handles.values() {
            if detfd.is_accept_in_turn_eligible() {
                detfd.set_exported();
            }
        }
    }

    /// Every socket of this table may have been passed to a table that holds
    /// it as an open file of its own: no pair's queues hold every message
    /// any more (`DetFd::give_up_pair_completeness`).
    pub(crate) fn give_up_every_pair_completeness(&self) {
        for detfd in self.metadata().file_handles.values() {
            detfd.give_up_pair_completeness();
        }
    }

    /// Descriptors in this table that name a network record/replay channel,
    /// sorted.
    pub(crate) fn network_channel_fds(&self) -> Vec<RawFd> {
        let mut fds: Vec<RawFd> = self
            .metadata()
            .file_handles
            .iter()
            .filter(|(_, detfd)| detfd.is_network_channel())
            .map(|(&fd, _)| fd)
            .collect();
        fds.sort_unstable();
        fds
    }

    pub(crate) fn count_open_files_at_paths(&self, paths: &[&Path]) -> usize {
        self.metadata().count_open_files_at_paths(paths)
    }

    /// Whether this task owns a socket that attempted a loopback connection.
    pub(crate) fn has_loopback_peer(&self) -> bool {
        self.metadata().has_loopback_peer()
    }

    /// Whether some open file description holds a flock mode Detcore knows,
    /// or was invalidated by a process copy after a flock was granted through
    /// it (`FileMetadata::has_unsafe_vfork_flock_state`).
    pub fn has_unsafe_vfork_flock_state(&self) -> bool {
        self.metadata().has_unsafe_vfork_flock_state()
    }

    /// Whether a description in this table may hold an OFD lock. A copied
    /// `vfork` child (DBT) runs its lock calls natively, so the
    /// `procfs::locks::LockLedger` entry of such a description could go stale
    /// and admit a host's identical `/proc/locks` row.
    pub fn has_unsafe_vfork_ofd_state(&self) -> bool {
        self.metadata().has_unsafe_vfork_ofd_state()
    }

    /// A new process can change every inherited open file description through
    /// a separately hosted tool state, so neither side may keep treating its
    /// cached flock mode as authoritative after fork.
    pub fn forget_flock_modes(&self) {
        self.metadata().forget_flock_modes();
    }

    /// After a successful clone that gave another table a copy of this one:
    /// the descriptions the lock ledger does not follow have references it
    /// never saw taken, in this process's view too where the copy is a
    /// separate tool state (`FileMetadata::mark_untracked_references_escaped`).
    pub fn mark_untracked_references_escaped(&self) {
        self.metadata().mark_untracked_references_escaped();
    }

    /// A successful file-backed `mmap` through a descriptor of this table
    /// mapped `open_file` (`FileMetadata::mapped_open_files`).
    pub(crate) fn note_mapped_open_file(&self, open_file: OpenFileId) {
        self.metadata().mapped_open_files.insert(open_file);
    }

    /// Whether this process may map `open_file`
    /// (`FileMetadata::mapped_open_files`).
    pub(crate) fn maps_open_file(&self, open_file: OpenFileId) -> bool {
        self.metadata().mapped_open_files.contains(&open_file)
    }

    /// remove a rawfd
    pub fn remove_fd(&self, fd: RawFd) -> Option<OpenFileId> {
        let mut metadata = self.metadata();
        if self.traced_call.is_active()
            && let Some(note) = metadata.close_note(fd)
        {
            self.traced_call.note(note);
        }
        metadata.remove_fd(fd)
    }

    /// This thread is exiting, with its whole process where `whole_process`
    /// (an `exit_group`), and the scheduler has granted the exit: it no longer
    /// holds its descriptor table (`FileMetadata::leave`). Where no thread
    /// holds the table any more, as guest calls alone count its holders
    /// (`FileMetadata::holders`), the table goes with this exit, as Linux
    /// releases it with its last holder's exit (`exit_files`): every
    /// descriptor of it is let go now, in this thread's turn, before the
    /// native call that ends it (`Detcore::release_table_at_exit`), so that
    /// whether another table's later close is a
    /// description's last reference depends on this call and not on when the
    /// backend tears thread states down. A socket whose last reference goes
    /// takes its queue, for the exit to release
    /// ([`Self::take_released_rights`]), as Linux purges a released socket's
    /// receive queue. The descriptions themselves keep their ports, as an
    /// exiting process's sockets always have. Returns whether the table was
    /// let go; a table that another thread still holds is kept.
    pub(crate) fn release_table_at_exit(&self, whole_process: bool) -> bool {
        let Some(process) = self.detpid else {
            return false;
        };
        let mut metadata = self.metadata();
        if !metadata.leave(process, whole_process) {
            return false;
        }
        metadata.release_every_descriptor();
        true
    }

    /// The new image's first callback, after an exec that replaced a table
    /// (`FileMetadata::keep_replaced_table`): the exec'ing process leaves it,
    /// all its threads at once (`FileMetadata::leave`), as Linux's exec kills
    /// the process's other threads (`de_thread`) and gives the process a table
    /// of its own where another process still holds the old one
    /// (`unshare_files`). Where that leaves the table to no holder, every
    /// descriptor of it goes now, in order. The close-on-exec ones go as Linux
    /// closes them at the exec (`do_close_on_exec`); the others, which this
    /// thread's new table holds too, release nothing by going. So the replaced
    /// table names no description any more, however long the backend keeps
    /// it, and whether a later close is a description's last reference
    /// depends on the exec, not on when the backend drops that table. Where
    /// another process still holds it (`CLONE_FILES` without `CLONE_THREAD`),
    /// that process keeps every descriptor, and the table goes with that
    /// process's own exit or exec, which the exec'ing process no longer
    /// counts against. A socket whose last reference goes takes its queue,
    /// for the exec to release ([`Self::take_released_rights`]), as Linux
    /// purges a released socket's receive queue (`unix_release_sock`). No
    /// port is released here: `Detcore::handle_execveat` releases those of the
    /// descriptions the exec closes before the exec runs, as before. Returns
    /// whether that left anything to release.
    pub(crate) fn release_exec_dropped_descriptors(&self) -> bool {
        let Some((replaced, process)) = self.metadata().exec_replaced.0.take() else {
            return false;
        };
        let released = {
            let mut replaced = replaced.lock().expect("file metadata mutex poisoned");
            if !replaced.leave(process, true) {
                return false;
            }
            replaced.release_every_descriptor();
            replaced.take_released_rights()
        };
        if released.is_empty() {
            return false;
        }
        self.defer_released_rights(released);
        true
    }

    /// What the current call made Linux release from receiving sockets'
    /// queues, for the call to release when it completes
    /// (`Detcore::settle_released_rights`): what closes in this thread's
    /// table took (`FileMetadata::release_detfd`), and what receives and
    /// purges handed over ([`Self::defer_released_rights`]).
    pub(crate) fn take_released_rights(&self) -> Vec<SentDescriptor> {
        self.metadata().take_released_rights()
    }

    /// Whether the current call holds descriptors to release
    /// ([`Self::take_released_rights`]).
    pub(crate) fn holds_released_rights(&self) -> bool {
        !self.metadata().released_rights.0.is_empty()
    }

    /// Hold `released`, descriptors queued to a socket that Linux has just
    /// released with their message, for the current call to release when it
    /// completes ([`Self::take_released_rights`]).
    pub(crate) fn defer_released_rights(&self, released: impl IntoIterator<Item = SentDescriptor>) {
        self.metadata().released_rights.0.extend(released);
    }

    /// Remove every modeled descriptor in an inclusive close_range interval.
    pub(crate) fn remove_fd_range(&self, first: u32, last: u32) -> Vec<OpenFileId> {
        let mut metadata = self.metadata();
        if self.traced_call.is_active() {
            let mut released: Vec<RawFd> = metadata
                .file_handles
                .keys()
                .copied()
                .filter(|fd| *fd >= 0 && first <= *fd as u32 && *fd as u32 <= last)
                .collect();
            released.sort_unstable();
            for note in released
                .into_iter()
                .filter_map(|fd| metadata.close_note(fd))
            {
                self.traced_call.note(note);
            }
        }
        metadata.remove_fd_range(first, last)
    }

    /// A descriptor of this thread's table that names the open file
    /// description `open_file`: `preferred` where it does, else the lowest
    /// that does (`Detcore::flock_wait`).
    pub(crate) fn slot_naming(&self, open_file: OpenFileId, preferred: RawFd) -> Option<RawFd> {
        let handles = &self.metadata().file_handles;
        if handles
            .get(&preferred)
            .is_some_and(|detfd| detfd.open_file_id() == open_file)
        {
            return Some(preferred);
        }
        handles
            .iter()
            .filter(|(_, detfd)| detfd.open_file_id() == open_file)
            .map(|(fd, _)| *fd)
            .min()
    }

    /// How many slots of this thread's table name the open file description
    /// `open_file` (`procfs::locks::LockLedger`).
    pub(crate) fn slots_naming(&self, open_file: OpenFileId) -> u32 {
        let count = self
            .metadata()
            .file_handles
            .values()
            .filter(|detfd| detfd.open_file_id() == open_file)
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// Whether the exec that made this thread's table dropped a close-on-exec
    /// description whose locks Detcore follows (`FileMetadata::exec_dropped_lock_tracked`).
    pub(crate) fn exec_dropped_lock_tracked(&self) -> bool {
        self.metadata().exec_dropped_lock_tracked
    }

    /// Whether this thread's table holds a descriptor Detcore serves as a
    /// `/proc` directory on a placeholder (`DetFd::placeholder`).
    pub(crate) fn holds_placeholder(&self) -> bool {
        self.metadata()
            .file_handles
            .values()
            .any(|detfd| detfd.placeholder().is_some())
    }

    /// The lowest numbered descriptor of this thread's table that names the
    /// open file description `open_file`, if any.
    pub(crate) fn lowest_slot_naming(&self, open_file: OpenFileId) -> Option<RawFd> {
        self.metadata()
            .file_handles
            .iter()
            .filter(|(_, detfd)| detfd.open_file_id() == open_file)
            .map(|(&fd, _)| fd)
            .min()
    }

    /// The descriptions in this thread's table whose locks
    /// `procfs::locks::LockLedger` follows, each with how many slots name it,
    /// in description order.
    pub(crate) fn lock_tracked_slots(&self) -> Vec<(OpenFileId, u32)> {
        let mut slots: BTreeMap<OpenFileId, u32> = BTreeMap::new();
        for detfd in self.metadata().file_handles.values() {
            if detfd.is_lock_tracked() {
                *slots.entry(detfd.open_file_id()).or_default() += 1;
            }
        }
        slots.into_iter().collect()
    }

    /// The descriptors Detcore's table holds for this thread, in order.
    pub(crate) fn open_fds(&self) -> Vec<RawFd> {
        let mut fds: Vec<RawFd> = self.metadata().file_handles.keys().copied().collect();
        fds.sort_unstable();
        fds
    }

    /// dup raw fds.
    pub fn dup_fd(
        &mut self,
        oldfd: RawFd,
        newfd: RawFd,
        flags: OFlag,
    ) -> Result<Option<OpenFileId>, Errno> {
        let mut metadata = self.metadata();
        if self.tool_shares_guest_descriptor_table {
            metadata.discover_fd_from_current_process(
                self.open_file_creator.unwrap_or(self.dettid),
                oldfd,
            )?;
        }
        let replaced = (self.traced_call.is_active() && oldfd != newfd)
            .then(|| metadata.close_note(newfd))
            .flatten();
        let released = metadata.dup_fd(oldfd, newfd, flags)?;
        if self.traced_call.is_active() && oldfd != newfd {
            if let Some(note) = replaced {
                self.traced_call.note(note);
            }
            self.traced_call
                .note(metadata.slot_note(newfd, FdNoteOp::Dup(oldfd)));
        }
        Ok(released)
    }

    /// Atomically validate a pidfd target and capture its source descriptor.
    ///
    /// Both lookups occur under the one descriptor-table mutex. The caller must
    /// hold the normal serialized Tool turn until the nonblocking kernel call
    /// returns and the captured alias is installed.
    pub(crate) fn capture_pidfd_getfd_source(
        &self,
        pidfd: RawFd,
        targetfd: RawFd,
        current_tgid: DetPid,
        current_tid: DetTid,
    ) -> Result<CapturedDetFd, Errno> {
        let mut metadata = self.metadata();
        if self.tool_shares_guest_descriptor_table {
            metadata.discover_fd_from_current_process(
                self.open_file_creator.unwrap_or(self.dettid),
                pidfd,
            )?;
        }
        let (is_pidfd, target) = metadata.with_detfd(pidfd, |detfd| {
            (matches!(detfd.ty(), FdType::Pidfd), detfd.pidfd_target())
        })?;
        if !is_pidfd {
            return Err(Errno::EBADF);
        }
        if !pidfd_getfd_targets_calling_task(target, current_tgid, current_tid) {
            return Err(Errno::EOPNOTSUPP);
        }
        if self.tool_shares_guest_descriptor_table {
            metadata.discover_fd_from_current_process(
                self.open_file_creator.unwrap_or(self.dettid),
                targetfd,
            )?;
        }
        metadata.capture_fd(targetfd)
    }

    /// Install an alias from a pre-syscall capture without resolving the source
    /// fd slot a second time.
    pub(crate) fn install_captured_fd(
        &mut self,
        captured: CapturedDetFd,
        newfd: RawFd,
        flags: OFlag,
    ) -> Result<Option<OpenFileId>, CapturedDetFdInstallError> {
        let mut metadata = self.metadata();
        let replaced = self
            .traced_call
            .is_active()
            .then(|| metadata.close_note(newfd))
            .flatten();
        let released = metadata.install_captured_fd(captured, newfd, flags)?;
        if self.traced_call.is_active() {
            if let Some(note) = replaced {
                self.traced_call.note(note);
            }
            self.traced_call
                .note(metadata.slot_note(newfd, FdNoteOp::Open));
        }
        Ok(released)
    }

    /// Install `detfd`, which refers to an open file description Detcore
    /// models, as descriptor `detfd.fd` of this table: a rendered procfs
    /// descriptor that reached the description over `SCM_RIGHTS`, adopted
    /// when it is received (`Detcore::record_received_descriptors`) or when
    /// the guest first names it (`Detcore::adopt_received_placeholders`).
    pub(crate) fn install_alias(&self, detfd: DetFd) {
        let fd = detfd.fd;
        let mut metadata = self.metadata();
        metadata.add_detfd(detfd);
        if self.traced_call.is_active() {
            self.traced_call
                .note(metadata.slot_note(fd, FdNoteOp::Open));
        }
    }

    /// The descriptors among `fds` this thread's table names, as the access
    /// trace needs them (`crate::access_trace::FdInfo`).
    pub(crate) fn access_trace_fds(
        &self,
        fds: &[RawFd],
    ) -> BTreeMap<RawFd, crate::access_trace::FdInfo> {
        let metadata = self.metadata();
        fds.iter()
            .filter_map(|fd| {
                metadata
                    .file_handles
                    .get(fd)
                    .map(|detfd| (*fd, crate::access_trace::FdInfo::of(detfd)))
            })
            .collect()
    }

    /// Abandon an uninstalled capture and identify a deferred final-OFD release.
    pub(crate) fn abandon_captured_fd(&self, captured: CapturedDetFd) -> Option<OpenFileId> {
        self.metadata().abandon_captured_fd(captured)
    }

    /// get thread prng, note this rng is deterministic and should not be used
    /// for crypto.
    pub fn thread_prng(&mut self) -> &mut Pcg64Mcg {
        &mut self.prng
    }

    /// Whether this thread's slices still follow a replayed schedule
    /// (`--replay-schedule-from`): they end where the schedule says, and no
    /// pending deadline or PMU maximum ends them. False without a schedule,
    /// and once the schedule is over for this thread
    /// (`schedule_replay_over`): its slices then end as in a run without a
    /// schedule, which `hermit analyze` assumes of a replayed prefix's
    /// continuation.
    pub(crate) fn follows_replayed_schedule(&self, cfg: &Config) -> bool {
        cfg.replay_schedule_from.is_some() && !self.schedule_replay_over
    }

    /// Whether a trapped instruction (`rdtsc`, `cpuid`) should end the current
    /// timeslice.
    ///
    /// By default it ends an expired target, as a syscall does. A trapped
    /// instruction can sit inside a user-space spinlock critical section,
    /// though, and a thread that yields there leaves every thread that wants
    /// the lock spinning until the PMU maximum preempts it. QEMU's
    /// `cpu_get_ticks` reads the host TSC under its `vm_clock_lock` spinlock,
    /// so a multi-threaded TCG vCPU that yielded at every expired target while
    /// reading the TSC held that lock at every yield, and the other vCPU never
    /// acquired it. With `--target-timeslice-syscalls-only`, a trapped
    /// instruction ends the slice only at the PMU maximum, as a branch would.
    /// With no PMU maximum it is the only way out of a busy-wait on the TSC,
    /// and it still ends the expired target. Replay of recorded preemptions or
    /// schedules keeps its recorded boundaries.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn instruction_trap_ends_timeslice(&self, cfg: &Config) -> bool {
        let replaying = self.preemption_points.is_some() || self.follows_replayed_schedule(cfg);
        match self.max_timeslice_end {
            Some(max_end) if cfg.target_timeslice_syscalls_only && !replaying => {
                self.thread_logical_time.as_nanos() >= max_end
            }
            _ => self.timeslice_expired(),
        }
    }

    /// End the current slice no later than `remaining` from now: the virtual
    /// time that was left before the earliest pending deadline when this
    /// thread was granted its turn.
    ///
    /// Global time advances exactly as this thread's clock does while it runs
    /// alone, so its slice then ends, and it takes a turn, once global time has
    /// passed that deadline.
    ///
    /// The slice keeps at least `min_slice`, one scheduler turn charge, and
    /// at least one RCB's worth of time. Every cap-induced slice end is a turn
    /// charged that much, so a thread runs at least as long as the turn it
    /// pays for: the virtual time the cap adds is at most the thread's own
    /// work. With a shorter floor, a sibling sleeping just over one turn
    /// charge made each slice about 1 us of work per 1-1.5 ms of virtual time.
    ///
    /// Only the software slice end moves, which takes effect at the thread's
    /// next check-in: a syscall, signal or timer event. The PMU maximum keeps
    /// its value, so a thread that makes no syscalls still runs to it: a
    /// precise PMU stop at every deadline costs about 70 ms of host time each,
    /// and a fast timer would make them constant
    /// (<https://github.com/rrnewton/hermit/issues/4121>). A schedule replay follows
    /// its recorded turns, and a preemption replay its recorded slice ends,
    /// neither capped. Past the last recorded point a replay's slice end is
    /// `LogicalTime::MAX` (its PMU maximum stays bounded), and the cap may
    /// lower it; that need not reproduce the record's own slice ends.
    pub(crate) fn cap_timeslice_at_deadline(
        &mut self,
        remaining: Duration,
        clock_multiplier: f64,
        min_slice: Duration,
        schedule_replay: bool,
    ) {
        let recorded_slice =
            self.preemption_points.is_some() && self.stats.last_recorded_slice.is_none();
        if recorded_slice || schedule_replay {
            return;
        }
        let Some(end) = self.end_of_timeslice else {
            return;
        };
        let one_rcb =
            (NANOS_PER_RCB * clock_multiplier * self.rcb_time_multiplier().as_f64()).ceil() as u64;
        let current = self.thread_logical_time.as_nanos();
        let cap = current
            + remaining
                .max(min_slice)
                .max(Duration::from_nanos(one_rcb + 1));
        if cap < end {
            debug!(
                "[dtid {}] end of slice moved from {} to {}, the earliest pending deadline (current {})",
                self.dettid, end, cap, current
            );
            self.end_of_timeslice = Some(cap);
        }
    }

    /// The recorded priority-history row whose end the current slice waits
    /// for, under `--replay-preemptions-from`; `None` without a history.
    /// Derived from the history's own cursor: rows are installed as slice
    /// ends in order, and the final slice follows the last.
    pub(crate) fn active_priority_row(
        &self,
    ) -> Option<crate::preemptions::callback_history::ActiveRow> {
        use crate::preemptions::callback_history::ActiveRow;
        let history = self.preemption_points.as_ref()?;
        Some(if self.stats.last_recorded_slice.is_some() {
            ActiveRow::Exhausted
        } else {
            match history.installed_rows() {
                0 => ActiveRow::BeforeFirst,
                installed => ActiveRow::Row(installed - 1),
            }
        })
    }

    /// The active row's callback checkpoint, when the row has one and it has
    /// not been consumed (`Config::annotated_priority_replay`).
    pub(crate) fn pending_checkpoint(
        &self,
    ) -> Option<&crate::preemptions::callback_history::Checkpoint> {
        use crate::preemptions::callback_history::ActiveRow;
        let ActiveRow::Row(row) = self.active_priority_row()? else {
            return None;
        };
        if !self.callback_history.annotates(row) {
            return None;
        }
        self.callback_history.remaining().next()
    }

    /// The post-hook's margin of one retired conditional branch, in logical
    /// time at this thread's current clock and chaos slowdown: a PMU
    /// deadline closer than this ends the slice at once.
    pub(crate) fn pmu_epsilon(&self, cfg: &Config) -> Duration {
        Duration::from_nanos((NANOS_PER_RCB * self.clock_multiplier(cfg)).ceil() as u64)
    }

    /// Logical nanoseconds per nanosecond of retired-branch time for this
    /// thread now: the configured clock multiplier times its chaos slowdown.
    pub(crate) fn clock_multiplier(&self, cfg: &Config) -> f64 {
        cfg.clock_multiplier.unwrap_or(1.0) * self.rcb_time_multiplier().as_f64()
    }

    /// Files a priority change point at `time` in a preemption replay, or
    /// returns the thread's last one when `time` is not after it.
    ///
    /// A replay that reproduces its record files each thread's change points
    /// at the record's times, which are strictly increasing (the loaded
    /// record is validated), so it never gets the error. A change point at or
    /// before the last one is a thread that left the record: a slice closed
    /// with no time in it, as when the slice before ran onto the next
    /// recorded end's branch count while that end's time was still ahead and
    /// the thread's time did not move (the adjacent zero-RCB slices of
    /// `end_timeslice_with_checkpoint`). Filed, such a change point was never
    /// granted, and the replay stalled until `--timeout`. Outside a
    /// preemption replay nothing is checked or kept, and nor is it for an
    /// edited record (`--replay-edited-preemptions`), which keeps a slice
    /// whose RCB target is the thread's count and ends it at once
    /// (`next_edited_slice`): its change points need not advance.
    pub(crate) fn file_replay_change_point(
        &mut self,
        cfg: &Config,
        time: LogicalTime,
    ) -> Result<(), LogicalTime> {
        if cfg.replay_preemptions_from.is_none() || cfg.replay_edited_preemptions {
            return Ok(());
        }
        if let Some(last) = self.last_replay_change_point.filter(|last| time <= *last) {
            return Err(last);
        }
        self.last_replay_change_point = Some(time);
        Ok(())
    }

    /// Runs the rest of a preemption replay's thread as its final slice: no
    /// recorded end governs it any more, and only the run's own PMU maximum
    /// (if any) bounds its slices, as when the record runs out. For a thread
    /// that left its record ([`Self::file_replay_change_point`]), whose
    /// remaining recorded ends no longer describe it.
    pub(crate) fn abandon_replayed_slices(&mut self, cfg: &Config) {
        self.stats.last_recorded_slice = Some(self.stats.timeslice_count);
        self.end_of_timeslice = Some(LogicalTime::MAX);
        self.replay_rcb_end = None;
        let now = self.thread_logical_time.as_nanos();
        self.max_timeslice_end = cfg
            .max_timeslice
            .map(|max_timeslice| now + Duration::from_nanos(u64::from(max_timeslice)));
        self.last_rcb_timer = None;
        self.last_rcb_timer_is_max = false;
    }

    /// Whether this thread has consumed its current logical timeslice.
    pub(crate) fn timeslice_expired(&self) -> bool {
        if let Some(replay_rcb_end) = self.replay_rcb_end {
            return self.committed_clock_value >= replay_rcb_end;
        }
        let current_time = self.thread_logical_time.as_nanos();
        self.end_of_timeslice
            .is_some_and(|end_of_timeslice| current_time >= end_of_timeslice)
    }

    /// Take the instruction offset the ending slice's timer was armed with
    /// (`--chaos-instruction-offset`), for the change point that records it:
    /// only if the thread reached the branch target, so that the timer, or a
    /// check-in among its steps, ended the slice. A slice that ended at a
    /// check-in before the target records no offset; its replay ends it at
    /// that check-in, as any such slice.
    pub(crate) fn take_ended_slice_offset(&mut self) -> Option<InstructionOffset> {
        let committed = self.committed_clock_value;
        self.armed_instruction_offset
            .take()
            .filter(|offset| committed >= offset.rcb_target)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Current RCB virtual-time multiplier, including recorded replay state.
    pub(crate) fn rcb_time_multiplier(&self) -> RcbTimeMultiplier {
        if self.chaos_slowdown_active {
            self.chaos_slowdown_factor
        } else {
            RcbTimeMultiplier::ONE
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    pub(crate) fn take_pending_chaos_epochs(&mut self) -> Vec<ChaosEpochTransition> {
        std::mem::take(&mut self.pending_chaos_epochs)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    fn install_chaos_epoch(&mut self, transition: ChaosEpochTransition, record: bool) {
        self.chaos_epoch = transition.epoch;
        self.chaos_slowdown_factor = transition.factor;
        self.chaos_slowdown_active = true;
        if record {
            self.pending_chaos_epochs.push(transition);
        }
        detlog!(
            "[dtid {}] CHAOSEPOCH => epoch = {}, factor = {}, logical_time = {}",
            self.dettid,
            transition.epoch,
            transition.factor.as_f64(),
            transition.logical_time
        );
    }

    /// Refresh the slowdown at a consumed slice boundary without choosing a
    /// deadline, drawing a priority or resetting slice statistics. A precise
    /// priority-history replay uses its previous recorded endpoint, even when
    /// the PMU checked in one RCB early. A schedule callback supplies its actual
    /// boundary instead of a future deadline already installed by its cursor.
    /// Ambient slowdown always follows the current elapsed logical time.
    pub(crate) fn refresh_chaos_epoch(&mut self, cfg: &Config, recorded_boundary: LogicalTime) {
        let current_ns = self.thread_logical_time.as_nanos();
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1151)
        // Redraw only at scheduler commit boundaries, keyed to elapsed
        // deterministic logical time. Replay artifacts take precedence and
        // restore the exact recorded Q32 factor even without ambient flags.
        let replay_has_epochs = self
            .preemption_points
            .as_ref()
            .is_some_and(ThreadHistoryIterator::has_chaos_epochs);
        if replay_has_epochs {
            // Precise PMU replay deliberately checks in up to one RCB
            // before the recorded logical boundary. The new slice still
            // begins at that recorded boundary, so install its factor
            // against the prior slice's exact target rather than the
            // slightly-early observed clock. Otherwise a short epoch can
            // be skipped and the following RCBs receive the wrong weight.
            let transition = self
                .preemption_points
                .as_mut()
                .and_then(|history| history.advance_chaos_epoch(recorded_boundary));
            if let Some(transition) = transition {
                self.install_chaos_epoch(transition, false);
            }
        } else if cfg.chaos && cfg.chaos_per_thread_slowdown {
            let elapsed_ns = self.thread_logical_time.without_starting().as_nanos();
            let epoch = elapsed_ns
                .checked_div(cfg.chaos_epoch_length_ns)
                .unwrap_or(0);
            let factor = chaos_per_thread_slowdown_factor(
                cfg.sched_seed(),
                self.dettid,
                epoch,
                cfg.chaos_slowdown_max_factor,
            );
            if epoch != self.chaos_epoch {
                self.install_chaos_epoch(
                    ChaosEpochTransition {
                        logical_time: current_ns,
                        epoch,
                        factor,
                    },
                    true,
                );
            } else {
                self.chaos_slowdown_factor = factor;
                self.chaos_slowdown_active = true;
            }
        } else {
            self.chaos_slowdown_factor = RcbTimeMultiplier::ONE;
            self.chaos_slowdown_active = false;
            self.pending_chaos_epochs.clear();
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    /// `bound`, a configured slice bound, raised to the post-crash bound once
    /// the run has switched to it (`crate::post_crash`). A bound already
    /// above it is kept.
    pub(crate) fn after_post_crash(&self, bound: std::num::NonZeroU64) -> std::num::NonZeroU64 {
        self.post_crash_bound
            .map_or(bound, |post_crash| post_crash.max(bound))
    }

    /// Choose the logical target and PMU maximum deadlines for the next timeslice.
    ///
    /// Effects:
    /// - Sets `end_of_timeslice` for the new timeslice.
    /// - Sets `max_timeslice_end` when PMU-backed preemption is enabled.
    /// - Resets the statistics for the timeslice.
    ///
    /// Returns: an optional new priority.
    pub fn next_timeslice(&mut self, cfg: &Config) -> Option<Priority> {
        self.replay_backstop = None;
        let logical_timeslice = cfg
            .target_timeslice
            .or(cfg.max_timeslice)
            .map(|bound| self.after_post_crash(bound));
        if let Some(timeout_ns) = logical_timeslice {
            let current_ns = self.thread_logical_time.as_nanos();

            let recorded_boundary = self.end_of_timeslice.unwrap_or(current_ns);
            self.refresh_chaos_epoch(cfg, recorded_boundary);

            let mut result = None;
            self.replay_rcb_end = None;
            self.slice_instruction_offset = 0;
            self.replay_instruction_offset = None;
            let replay_controls_deadline =
                self.preemption_points.is_some() || self.follows_replayed_schedule(cfg);

            // How far ahead an edited record's slice without an RCB target
            // must end to be replayed (`next_edited_slice`): the post-hook's
            // one-RCB margin at this slice's clock, which ends a closer slice
            // at once. Without a PMU maximum there is no such margin, so a
            // slice is kept whenever it ends ahead, as in a faithful replay.
            let margin = if cfg.max_timeslice.is_some() {
                self.pmu_epsilon(cfg)
            } else {
                Duration::from_nanos(1)
            };
            // Preemption-point replay from recorded --chaos configuration.
            if let Some(thi) = &mut self.preemption_points {
                if self.stats.last_recorded_slice.is_none() {
                    // We have not tapped out the recording yet.
                    let current_rcbs = self.committed_clock_value;
                    // For an edited record, skip the recorded slices that
                    // already ended (`next_edited_slice`). The chaos epoch
                    // advanced only to the previous slice's end (above), so
                    // a slice after skipped ones keeps that epoch's
                    // slowdown: deterministic, if stale. At a thread's start
                    // the scheduler has already applied the record's first
                    // priority, which then holds until the kept slice ends
                    // (the startup reset files no scheduler request).
                    let next = if cfg.replay_edited_preemptions {
                        let (next, skipped) =
                            next_edited_slice(thi, current_ns, current_rcbs, margin);
                        if skipped > 0 {
                            debug!(
                                "[dtid {}] edited record: skipped {} recorded slices that ended before {} (RCB {})",
                                self.dettid, skipped, current_ns, current_rcbs
                            );
                        }
                        next
                    } else {
                        thi.next_with_offset()
                    };
                    if let Some((end_time, prio, replay_rcb_end, offset)) = next {
                        debug!(
                            "[dtid {}] next timeslice (T{}), set by recording to {:?} (current {}), priority {}",
                            self.dettid,
                            self.stats.timeslice_count + 1,
                            end_time,
                            current_ns,
                            prio
                        );
                        if let Some(target_rcbs) = replay_rcb_end
                            && target_rcbs < current_rcbs
                        {
                            panic!(
                                "Cannot set RCB end of timeslice to {} for thread {}, when current RCB count is already {}. \
                                 An edited record, not the recording of this run's schedule, needs \
                                 --replay-edited-preemptions.",
                                target_rcbs, self.dettid, current_rcbs
                            )
                        }
                        let exact_deadline_is_now =
                            replay_rcb_end.is_some_and(|target| target == current_rcbs);
                        // An edited record keeps a slice whose end has passed
                        // while its RCB target is ahead: the target governs.
                        let rcb_target_governs =
                            cfg.replay_edited_preemptions && replay_rcb_end.is_some();
                        if end_time <= current_ns && !exact_deadline_is_now && !rcb_target_governs {
                            // Records that store their epoch are reconciled with
                            // this run's epoch before any guest starts (see
                            // `adopt_replayed_schedule_epoch` in hermit-cli), so
                            // reaching this with such a record means the replay
                            // really diverged. A record without an epoch cannot
                            // be checked up front; say what usually causes it.
                            panic!(
                                "Cannot set end of timeslice to {} for thread {}, when current thread logical time is already {}. \
                                 The replayed preemption record's timeslice ends are absolute virtual times; if it was \
                                 recorded under a different virtual-time epoch, replay it with that --epoch \
                                 (https://github.com/rrnewton/hermit/issues/3413). An edited record, not the \
                                 recording of this run's schedule, needs --replay-edited-preemptions.",
                                end_time, self.dettid, current_ns
                            )
                        }
                        self.end_of_timeslice = Some(end_time);
                        self.replay_rcb_end = replay_rcb_end;
                        // An offset is only meaningful against the exact RCB
                        // end it was recorded with.
                        self.replay_instruction_offset =
                            offset.filter(|_| replay_rcb_end.is_some());
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
            } else if !cfg.chaos_draws_schedule() {
                if self.follows_replayed_schedule(cfg) {
                    if cfg.no_rcb_time && cfg.max_timeslice.is_some() {
                        let length = self
                            .max_slice_length(cfg)
                            .expect("schedule replay with PMU requires a maximum");
                        self.end_of_timeslice = Some(current_ns + Duration::from_nanos(length));
                    } else {
                        // The schedule's events and callback checkpoints decide
                        // the slice end. A cooperative target without a PMU
                        // maximum does not manufacture a replay backstop.
                        debug!(
                            "[dtid {}] next timeslice (T{}), in replay mode setting timeslice to max (current time {})",
                            self.dettid,
                            self.stats.timeslice_count + 1,
                            current_ns
                        );
                        self.end_of_timeslice = Some(LogicalTime::MAX);
                    }
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
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-1151)
                // The slowdown changes RCB-to-virtual-time progression. Converting
                // the sampled virtual duration back to RCBs with the SAME factor
                // keeps the deadline and guest-visible clock internally consistent.
                let slowdown = self.rcb_time_multiplier().as_f64();
                let nanos_per_rcb = NANOS_PER_RCB * cfg.clock_multiplier.unwrap_or(1.0) * slowdown;
                let target_timeout_rcbs = u64::from(timeout_ns) as f64 / nanos_per_rcb;
                if self.chaos_slowdown_active {
                    detlog!(
                        "[dtid {}] CHAOSSLOWDOWN => factor = {}, virtual ns/rcb = {}",
                        self.dettid,
                        slowdown,
                        nanos_per_rcb
                    );
                }
                let next_rcbs: u64 = if cfg.chaos {
                    // `--chaos-timeslice-range`: this slice's mean is drawn
                    // log-uniformly from the range, from the same seeded chaos
                    // stream, so slices of every scale in it mix in one run. A
                    // schedule replay draws none: its slices end where the
                    // record says, and the drawn ends would only add PMU
                    // check-ins. A per-run timeslice (`--chaos-timeslice-draw=run`,
                    // or one a replayed record states) replaces the draw too,
                    // and so does the post-crash switch: a switched process's
                    // slices are drawn around its post-crash bound
                    // (`after_post_crash`), the coarse scale the switch is for,
                    // never from the range again.
                    let target_timeout_rcbs = match cfg.chaos_timeslice_range {
                        Some(range)
                            if !replay_controls_deadline
                                && cfg.chaos_timeslice.is_none()
                                && self.post_crash_bound.is_none() =>
                        {
                            range.log_uniform(self.chaos_prng.random::<f64>()) / nanos_per_rcb
                        }
                        _ => target_timeout_rcbs,
                    };
                    // Average frequency of preemptions per nanosecond:
                    let lambda = 1.0 / target_timeout_rcbs;
                    let exp = Exp::new(lambda).unwrap();
                    // Add one to prevent generating a zero time slice:
                    let rcbs = 1 + exp.sample(&mut self.chaos_prng) as u64;
                    detlog!("[dtid {}] CHAOSRAND => next_rcbs = {}", self.dettid, rcbs);
                    if cfg.chaos_instruction_offset > 0 {
                        let offset = chaos_instruction_offset_draw(
                            cfg.sched_seed(),
                            self.dettid,
                            self.chaos_instruction_offset_draws,
                            cfg.chaos_instruction_offset,
                        );
                        self.chaos_instruction_offset_draws += 1;
                        self.slice_instruction_offset = offset;
                        detlog!(
                            "[dtid {}] CHAOSRAND => instruction_offset = {}",
                            self.dettid,
                            offset
                        );
                    }
                    rcbs
                } else {
                    target_timeout_rcbs as u64
                };
                assert!(next_rcbs > 0);
                self.last_rcb_timer = None;
                self.end_of_timeslice = Some(
                    current_ns
                        + Duration::from_nanos((next_rcbs as f64 * nanos_per_rcb).ceil() as u64),
                );
                debug!(
                    "[dtid {}] next timeslice (T{}) chosen as {} rcbs, end of slice = {} (current {})",
                    self.dettid,
                    self.stats.timeslice_count + 1,
                    next_rcbs,
                    self.end_of_timeslice.unwrap(),
                    current_ns
                );
            }

            // Past an edited record's last recorded slice, a restored chaos
            // slowdown can make one RCB longer than the configured maximum
            // (the configuration checks that maximum at the ambient
            // slowdown): keep at least the post-hook's one-RCB margin, or it
            // would end this slice at once and find itself past its maximum
            // again.
            let past_an_edited_record = cfg.replay_edited_preemptions
                && self.preemption_points.is_some()
                && self.end_of_timeslice == Some(LogicalTime::MAX);
            let configured_max_end = self.max_slice_length(cfg).map(|length| {
                let length = Duration::from_nanos(length);
                current_ns
                    + if past_an_edited_record {
                        length.max(margin)
                    } else {
                        length
                    }
            });
            self.max_timeslice_end = if replay_controls_deadline {
                if cfg.max_timeslice.is_some() {
                    // A replay history uses `LogicalTime::MAX` after its last
                    // recorded preemption. Keep periodic PMU check-ins bounded
                    // instead of trying to program that sentinel as an RCB
                    // timer, which the kernel rejects with EINVAL.
                    self.end_of_timeslice
                        .filter(|end| *end != LogicalTime::MAX)
                        .or(configured_max_end)
                } else {
                    None
                }
            } else if cfg.target_timeslice.is_none() {
                match (self.end_of_timeslice, configured_max_end) {
                    (Some(logical_end), Some(configured_end)) => {
                        Some(logical_end.min(configured_end))
                    }
                    (_, configured_end) => configured_end,
                }
            } else {
                configured_max_end
            };

            if let (Some(target_end), Some(max_end)) =
                (self.end_of_timeslice, self.max_timeslice_end)
                && target_end > max_end
            {
                self.end_of_timeslice = Some(max_end);
            }
            if cfg.replay_schedule_from.is_some() && self.preemption_points.is_none() {
                self.replay_backstop = self.end_of_timeslice;
            }

            self.last_rcb_timer = None;
            self.last_rcb_timer_is_max = false;
            self.armed_instruction_offset = None;
            self.reset_timeslice_stats(current_ns);
            result
        } else {
            self.end_of_timeslice = None;
            self.replay_rcb_end = None;
            self.max_timeslice_end = None;
            self.last_rcb_timer = None;
            self.last_rcb_timer_is_max = false;
            self.armed_instruction_offset = None;
            self.slice_instruction_offset = 0;
            self.replay_instruction_offset = None;
            None
        }
    }

    /// Take what the reply to a traced event says about the replayed
    /// schedule (`Scheduler::trace_replay_reply`): whether it is over
    /// (`schedule_replay_over`), and the replay backstop
    /// (`learn_uncapped_backstop`), in that order, so a backstop never
    /// outlives the schedule it came with.
    pub(crate) fn hear_schedule_replay_reply(
        &mut self,
        schedule_over: bool,
        backstop: Option<LogicalTime>,
        bound: Option<std::num::NonZeroU64>,
        schedule_replay: bool,
    ) {
        if schedule_over {
            self.schedule_replay_over = true;
        }
        self.learn_uncapped_backstop(backstop, bound, schedule_replay);
    }

    /// Take the replay backstop the replayer sent with a traced event
    /// (`Scheduler::uncapped_backstop`). Only a thread that still follows the
    /// schedule keeps one (`follows_replayed_schedule`): once the schedule is
    /// over the thread's slices end as in a run without a schedule, whatever
    /// a reply carried. A thread's first slice begins before it traces
    /// anything, so it began under the plain bound; when the backstop first
    /// arrives, a slice end that came from that bound moves out to the
    /// backstop, counted from the slice's start, as if the slice had begun
    /// with it.
    pub(crate) fn learn_uncapped_backstop(
        &mut self,
        backstop: Option<LogicalTime>,
        bound: Option<std::num::NonZeroU64>,
        schedule_replay: bool,
    ) {
        let following = schedule_replay && !self.schedule_replay_over;
        let backstop = backstop.filter(|_| following);
        let first = self.uncapped_backstop.is_none() && backstop.is_some();
        self.uncapped_backstop = backstop;
        if !first {
            return;
        }
        if let (Some(start), Some(end), Some(max_end), Some(length)) = (
            self.stats.timeslice_start_ns,
            self.end_of_timeslice,
            self.max_timeslice_end,
            self.slice_length_within(bound, following),
        ) && end == max_end
        {
            let backstop_end = start + Duration::from_nanos(length);
            if backstop_end > end {
                self.end_of_timeslice = Some(backstop_end);
                self.max_timeslice_end = Some(backstop_end);
                self.last_rcb_timer = None;
                self.last_rcb_timer_is_max = false;
            }
        }
    }

    /// The longest a slice runs before the `--max-timeslice` bound ends it, in
    /// nanoseconds of the thread's logical time: the bound itself, except
    /// while the thread follows the replayed schedule of a record whose
    /// slices ran without it (`uncapped_backstop`,
    /// `follows_replayed_schedule`), where only the record ends a slice. A
    /// recorded slice can be far longer than the bound there (one that
    /// replayed a preemption record ends its slices where that record says),
    /// and ending it at the bound would leave the record. Once the schedule is
    /// over the bound holds again, as in a run without a schedule.
    pub(crate) fn max_slice_length(&self, cfg: &Config) -> Option<u64> {
        // The post-crash bound (main's `after_post_crash`) and the uncapped
        // backstop each replace the configured bound with the larger of the
        // two, so the effective maximum is the largest of the three.
        self.slice_length_within(
            cfg.max_timeslice.map(|bound| self.after_post_crash(bound)),
            self.follows_replayed_schedule(cfg),
        )
    }

    fn slice_length_within(
        &self,
        bound: Option<std::num::NonZeroU64>,
        following: bool,
    ) -> Option<u64> {
        let bound = bound?.get();
        Some(
            self.uncapped_backstop
                .filter(|_| following)
                .map_or(bound, |backstop| backstop.as_nanos().max(bound)),
        )
    }

    /// Restart a schedule replay's backstop deadline from now, at a recorded
    /// slice end that a callback checkpoint replays without closing the slice
    /// (an administrative turn).
    ///
    /// A schedule replay ends a slice where its record does. Beyond that it
    /// keeps the slice end `next_timeslice` installs, `--max-timeslice` after
    /// the slice began, as the recorded run did (`replay_backstop`). The
    /// recorded run began a new slice at this checkpoint, so its next slice
    /// end comes at most `--max-timeslice` after it. A backstop counted from
    /// the slice the administrative turn kept would end the replayed slice
    /// early, at a point the record does not have, and the extra scheduler
    /// turn would move the virtual clock the guest reads. Any other slice end,
    /// a deadline the replayed schedule set above all, stays.
    pub(crate) fn restart_replay_backstop(&mut self, cfg: &Config) {
        let Some(max_timeslice) = cfg.max_timeslice else {
            return;
        };
        if self.end_of_timeslice.is_none() || self.end_of_timeslice != self.replay_backstop {
            return;
        }
        let end = self.thread_logical_time.as_nanos()
            + Duration::from_nanos(u64::from(self.after_post_crash(max_timeslice)));
        self.end_of_timeslice = Some(end);
        self.max_timeslice_end = Some(end);
        self.replay_backstop = Some(end);
        self.last_rcb_timer = None;
        self.last_rcb_timer_is_max = false;
    }

    /// Install a slice end the replayed schedule decides: the replayer's
    /// deadline for this thread's next recorded event (`trace_schedevent`), the
    /// deadline an exec's continuation resumes, or the no-RCB-time fallback
    /// past a replayed maximum. It bounds the PMU maximum too when one is
    /// configured, and it is no backstop, whatever its value
    /// (`replay_backstop`).
    pub(crate) fn install_schedule_deadline(&mut self, deadline: LogicalTime, max_timeslice: bool) {
        self.end_of_timeslice = Some(deadline);
        if max_timeslice {
            self.max_timeslice_end = Some(deadline);
        }
        self.replay_backstop = None;
    }

    /// Close the current logical-timeslice statistics without selecting a new
    /// preemption point. Used when preemption replay reaches a deterministic
    /// guest sched_yield boundary.
    pub fn reset_timeslice_for_explicit_yield(&mut self) {
        let current_ns = self.thread_logical_time.as_nanos();
        self.reset_timeslice_stats(current_ns);
    }

    fn reset_timeslice_stats(&mut self, current_ns: LogicalTime) {
        if let Some(start) = self.stats.timeslice_start_ns
            && current_ns >= start
        {
            self.stats
                .timeslice_stats
                .record((current_ns - start).as_nanos());
        }
        self.stats.timeslice_start_ns = Some(current_ns);
        self.stats.reset_timeslice();
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

/// A recorded slice as `ThreadHistoryIterator::next_with_offset` yields it: its end, priority, RCB
/// target and where its timer was armed past that target.
type RecordedSlice = (
    LogicalTime,
    Priority,
    Option<u64>,
    Option<InstructionOffset>,
);

/// The first recorded slice of an edited record (`--replay-edited-preemptions`)
/// still ahead of a thread at logical time `now` and committed PMU count
/// `rcbs`, and how many recorded slices before it were skipped.
///
/// An edited record (`hermit analyze` knocks preemptions out of a recording)
/// changes when threads run, so a thread can reach recorded slices that have
/// already ended. Those no longer apply. A slice is ahead when its RCB target
/// is, since that target governs where there is one (its end may have
/// passed), and otherwise when it ends at least `margin` ahead
/// ([`ThreadState::next_timeslice`] passes the post-hook's one-RCB margin), so
/// a recorded one-RCB slice that starts where the previous one ended is kept.
/// Only the baseline search's knock-outs keep RCB targets; minimization's
/// records skip by time alone. The returned target is the kept slice's own,
/// or none when its end governs.
fn next_edited_slice(
    history: &mut ThreadHistoryIterator,
    now: LogicalTime,
    rcbs: u64,
    margin: Duration,
) -> (Option<RecordedSlice>, usize) {
    let mut skipped = 0;
    loop {
        match history.next_with_offset() {
            None => return (None, skipped),
            Some((end, prio, Some(target), offset)) if target >= rcbs => {
                return (Some((end, prio, Some(target), offset)), skipped);
            }
            Some((end, prio, _, offset)) if end >= now + margin => {
                return (Some((end, prio, None, offset)), skipped);
            }
            Some(_) => skipped += 1,
        }
    }
}

#[cfg(test)]
mod traced_call_tests {
    use detcore_model::access_trace::FdOp;

    use super::*;

    /// While a traced call is in progress, the descriptor table notes each
    /// slot it opens, dups onto and closes, in order: a dup2 onto an open
    /// number closes it first, and a close_range lists every slot it
    /// releases with the open file it held. Without a traced call nothing is
    /// noted.
    #[test]
    fn a_traced_call_notes_the_slots_it_changes() {
        let mut state = ThreadState::<()>::new(DetTid::from_raw(3), &Config::default(), ());
        for fd in [10, 11] {
            state.add_fd(fd, OFlag::O_RDWR, FdType::Pipe, None).unwrap();
        }
        let id = |state: &ThreadState<()>, fd| state.with_detfd(fd, |d| d.open_file_id()).unwrap();
        let (ten, eleven) = (id(&state, 10), id(&state, 11));
        state.remove_fd(12);
        assert!(
            state.traced_call.take().is_none(),
            "no traced call, no notes"
        );

        state.traced_call.begin(BTreeMap::new());
        state.dup_fd(10, 11, OFlag::empty()).unwrap();
        state.remove_fd_range(10, u32::MAX);
        let notes = state.traced_call.take().expect("a traced call").notes;
        let slot = |fd| detcore_model::fd::FdSlot {
            files: state.files_id(),
            fd,
        };
        let summary: Vec<_> = notes
            .iter()
            .map(|note| {
                (
                    note.slot,
                    note.open_file,
                    note.op.clone(),
                    note.released.as_ref().map(|r| (r.open_file, r.ty)),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    slot(11),
                    Some(eleven),
                    FdOp::Close,
                    Some((eleven, FdType::Pipe))
                ),
                (slot(11), Some(ten), FdOp::Dup { source: 10 }, None),
                (slot(10), Some(ten), FdOp::Close, Some((ten, FdType::Pipe))),
                (slot(11), Some(ten), FdOp::Close, Some((ten, FdType::Pipe))),
            ]
        );
    }
}

#[cfg(test)]
mod timeslice_tests {
    use std::num::NonZeroU64;

    use super::*;
    use crate::DEFAULT_PRIORITY;
    use crate::preemptions::ThreadHistory;

    /// Once an exit_group is filed on DBT (`OwnerDeathPath::Barrier`), an
    /// exiting thread's head stays registered for the exit_group's walk, but
    /// an unregistration still counts, a registration made after the filing
    /// is seen, and a thread whose own exit Detcore replayed is left out
    /// (https://github.com/rrnewton/hermit/issues/4238).
    #[test]
    fn an_exit_group_walks_the_registrations_current_at_its_grant() {
        let owner = |tid: i32, head: usize| RobustListOwner {
            dettid: DetTid::from_raw(tid),
            head,
        };
        let mut caller = ThreadState::<()>::new(DetTid::from_raw(3), &Config::default(), ());
        let sibling = |tid: i32| {
            let mut state = caller.clone();
            state.dettid = DetTid::from_raw(tid);
            state
        };
        let (mut killed, mut exited, mut late, mut replaced, mut unregistered) =
            (sibling(4), sibling(5), sibling(6), sibling(7), sibling(8));
        caller.record_robust_list_head(Some(0x3000));
        killed.record_robust_list_head(Some(0x4000));
        exited.record_robust_list_head(Some(0x5000));
        replaced.record_robust_list_head(Some(0x7000));
        unregistered.record_robust_list_head(Some(0x8000));

        caller.file_exit_group_for_robust_lists();
        // Before the grant: thread 5 makes its own exit, which Detcore
        // replays; thread 6 registers; thread 7 replaces its head; thread 8
        // unregisters.
        exited.mark_robust_list_walked_by_tool();
        exited.drop_robust_list_head_at_exit();
        late.record_robust_list_head(Some(0x6000));
        replaced.record_robust_list_head(Some(0x7700));
        unregistered.record_robust_list_head(None);
        // The grant kills thread 4, whose exit hook runs before the walk.
        killed.drop_robust_list_head_at_exit();
        assert_eq!(
            killed.take_robust_list_wakes_after_exit(None, DetTime::default()),
            None
        );

        assert_eq!(
            caller.robust_list_owners_for_exit_group(),
            vec![
                owner(3, 0x3000),
                owner(4, 0x4000),
                owner(6, 0x6000),
                owner(7, 0x7700)
            ]
        );
    }

    /// A thread an exit_group killed waits for that exit_group's walk before
    /// its native exit, including one that filed an exit_group of its own,
    /// whichever of two competing filings is granted; the walker does not
    /// wait, nor does a group with no filing.
    #[test]
    fn a_killed_thread_waits_for_the_exit_group_walk() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;
        const BOUND: std::time::Duration = EXIT_GROUP_ROBUST_WALK_WAIT;

        for granted_first_filer in [true, false] {
            let first = ThreadState::<()>::new(DetTid::from_raw(3), &Config::default(), ());
            let mut second = first.clone();
            second.dettid = DetTid::from_raw(4);
            // Nothing filed: no wait.
            assert!(second.wait_for_exit_group_robust_walk(BOUND));

            first.file_exit_group_for_robust_lists();
            second.file_exit_group_for_robust_lists();
            let (walker, killed) = if granted_first_filer {
                (first, second)
            } else {
                (second, first)
            };
            walker.begin_exit_group_robust_walk();
            assert!(walker.wait_for_exit_group_robust_walk(BOUND));

            let released = Arc::new(AtomicBool::new(false));
            let entered = Arc::new(AtomicBool::new(false));
            let waiter = {
                let released = Arc::clone(&released);
                let entered = Arc::clone(&entered);
                std::thread::spawn(move || {
                    killed.cancel_exit_group_filing();
                    entered.store(true, Ordering::SeqCst);
                    let finished = killed.wait_for_exit_group_robust_walk(BOUND);
                    (finished, released.load(Ordering::SeqCst))
                })
            };
            // The killed thread is in the wait before the walk finishes, so the
            // check below is not passed by a waiter that started late (Claude
            // review of https://github.com/rrnewton/hermit/pull/4277, N6).
            while !entered.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(
                !waiter.is_finished(),
                "the killed filer did not wait for the walk"
            );
            released.store(true, Ordering::SeqCst);
            walker.finish_exit_group_robust_walk();
            assert_eq!(
                waiter.join().unwrap(),
                (true, true),
                "the killed filer went on before the walk finished (first filer granted: {granted_first_filer})"
            );
        }
    }

    /// A granted walk that has not finished keeps its killed threads waiting
    /// to the bound, which is reported, not taken as completion.
    #[test]
    fn an_unfinished_walk_is_never_released_by_the_bound() {
        let walker = ThreadState::<()>::new(DetTid::from_raw(3), &Config::default(), ());
        let mut killed = walker.clone();
        killed.dettid = DetTid::from_raw(4);
        walker.file_exit_group_for_robust_lists();
        walker.begin_exit_group_robust_walk();
        killed.cancel_exit_group_filing();
        assert!(!killed.wait_for_exit_group_robust_walk(std::time::Duration::from_millis(20)));
    }

    /// When every filing is cancelled by its filer's death and none was
    /// granted (an exec ended the group first), no walk will come, and the
    /// killed threads are released at once.
    #[test]
    fn a_wait_ends_when_every_filing_is_cancelled() {
        let filer = ThreadState::<()>::new(DetTid::from_raw(3), &Config::default(), ());
        let mut other = filer.clone();
        other.dettid = DetTid::from_raw(4);
        filer.file_exit_group_for_robust_lists();
        other.cancel_exit_group_filing();
        assert!(!other.wait_for_exit_group_robust_walk(std::time::Duration::from_millis(20)));
        filer.cancel_exit_group_filing();
        assert!(other.wait_for_exit_group_robust_walk(std::time::Duration::from_millis(20)));
    }

    /// Without a filed exit_group an exiting thread's head leaves the index.
    #[test]
    fn an_exit_drops_its_head_when_no_exit_group_is_filed() {
        let mut state = ThreadState::<()>::new(DetTid::from_raw(4), &Config::default(), ());
        state.record_robust_list_head(Some(0x4000));
        state.drop_robust_list_head_at_exit();
        assert!(state.robust_list_heads().is_empty());
        assert_eq!(state.robust_list_head, None);
    }

    /// A stale robust-list head must never survive into a new address space.
    /// `run_robust_list_owner_death` walks from whatever this field holds, so if
    /// `execve` left the old image's head here, thread exit would follow a wild
    /// pointer through the *new* image and write `FUTEX_OWNER_DIED` into
    /// whatever happened to sit at the computed word address.
    #[test]
    fn a_successful_exec_clears_the_robust_list_head() {
        let mut state = ThreadState::<()>::new(DetTid::from_raw(3), &Config::default(), ());
        assert_eq!(state.robust_list_head, None, "a fresh thread has no list");

        state.record_robust_list_head(Some(0x7ffff7bff920));
        assert_eq!(
            state.take_robust_list_for_exec(),
            Some(0x7ffff7bff920),
            "the previous head is handed back for the failure path"
        );
        assert_eq!(
            state.robust_list_head, None,
            "the candidate exec image starts with no robust list, as after copy_process"
        );
        assert!(
            state.robust_list_heads().is_empty(),
            "the old address-space registration must also leave the shared index"
        );
    }

    /// `execve` returns only on failure, and then the old address space -- and
    /// so the old head -- is still valid.
    #[test]
    fn a_failed_exec_restores_the_robust_list_head() {
        let mut state = ThreadState::<()>::new(DetTid::from_raw(3), &Config::default(), ());
        state.record_robust_list_head(Some(0x404100));

        let saved = state.take_robust_list_for_exec();
        state.restore_robust_list_after_failed_exec(saved);

        assert_eq!(state.robust_list_head, Some(0x404100));
        assert_eq!(
            state.robust_list_heads(),
            vec![(DetTid::from_raw(3), 0x404100)],
            "a failed exec keeps the old address-space registration indexed"
        );
    }

    fn two_owner_robust_list_state() -> (
        ThreadState<()>,
        ThreadState<()>,
        RobustListWake,
        RobustListWake,
        RobustListWake,
    ) {
        let first_owner = DetTid::from_raw(3);
        let second_owner = DetTid::from_raw(4);
        let mm = MmId::initial(first_owner);
        let lower = RobustListWake {
            futex: FutexID::private(mm, 0x4040),
        };
        let middle = RobustListWake {
            futex: FutexID::private(mm, 0x5050),
        };
        let higher = RobustListWake {
            futex: FutexID::private(mm, 0x6060),
        };
        let mut first = ThreadState::<()>::new(first_owner, &Config::default(), ());
        let mut second = first.clone();
        second.dettid = second_owner;
        first.record_robust_list_head(Some(0x404100));
        second.record_robust_list_head(Some(0x404200));
        first.stage_robust_list_wakes(
            RobustListExit::Signal(libc::SIGTERM),
            vec![
                (second_owner, vec![higher]),
                (first_owner, vec![middle, lower]),
            ],
        );
        (first, second, lower, middle, higher)
    }

    #[test]
    fn group_robust_list_wakes_wait_for_every_owner_and_sort_the_request() {
        let (first, second, lower, middle, higher) = two_owner_robust_list_state();
        let mut first_time = DetTime::default();
        first_time.add_syscall();
        let mut second_time = first_time.clone();
        second_time.add_syscall();

        assert_eq!(
            second.take_robust_list_wakes_after_exit(Some(libc::SIGTERM), second_time.clone()),
            None,
            "the first physical exit must not release part of the group"
        );
        assert_eq!(
            first.take_robust_list_wakes_after_exit(Some(libc::SIGTERM), first_time),
            Some((
                second_time,
                vec![
                    (DetTid::from_raw(3), lower),
                    (DetTid::from_raw(3), middle),
                    (DetTid::from_raw(4), higher),
                ],
            )),
            "the final matching exit releases one owner/futex-sorted request"
        );
    }

    #[test]
    fn group_robust_list_wake_request_is_independent_of_exit_arrival_order() {
        let release = |reverse: bool| {
            let (first, second, _, _, _) = two_owner_robust_list_state();
            let first_time = DetTime::default();
            let mut second_time = first_time.clone();
            second_time.add_syscall();
            if reverse {
                assert_eq!(
                    second.take_robust_list_wakes_after_exit(
                        Some(libc::SIGTERM),
                        second_time.clone(),
                    ),
                    None
                );
                first.take_robust_list_wakes_after_exit(Some(libc::SIGTERM), first_time)
            } else {
                assert_eq!(
                    first.take_robust_list_wakes_after_exit(Some(libc::SIGTERM), first_time),
                    None
                );
                second.take_robust_list_wakes_after_exit(Some(libc::SIGTERM), second_time)
            }
        };

        assert_eq!(release(false), release(true));
    }

    #[test]
    fn mismatched_or_nonfatal_exit_does_not_release_group_robust_list_wakes() {
        let (first, second, _, _, _) = two_owner_robust_list_state();
        assert_eq!(
            first.take_robust_list_wakes_after_exit(Some(libc::SIGKILL), DetTime::default()),
            None
        );
        assert_eq!(
            second.take_robust_list_wakes_after_exit(Some(libc::SIGTERM), DetTime::default()),
            None,
            "one matching owner cannot release a group whose peer exited for another reason"
        );

        let (first, second, _, _, _) = two_owner_robust_list_state();
        assert_eq!(
            first.take_robust_list_wakes_after_exit(None, DetTime::default()),
            None
        );
        assert_eq!(
            second.take_robust_list_wakes_after_exit(Some(libc::SIGTERM), DetTime::default()),
            None,
            "a normal exit must not satisfy a staged fatal-signal exit"
        );
    }

    #[test]
    fn regular_file_opens_do_not_shift_socket_cookie_identity() {
        let owner = DetTid::from_raw(3);
        let mut files = FileMetadata::new(owner);
        let first = files.allocate_open_file_id(owner, FdType::Socket);
        for _ in 0..4 {
            files.allocate_open_file_id(owner, FdType::Regular);
        }
        let second = files.allocate_open_file_id(owner, FdType::Socket);

        assert_eq!(first.deterministic_socket_cookie(), 3_u64 << 32);
        assert_eq!(second.deterministic_socket_cookie(), (3_u64 << 32) | 1);
    }

    #[test]
    fn guest_clock_tracks_raw_logical_time_without_lag() {
        let epoch = LogicalTime::from_secs(1_000);
        let mut clock = GuestClock::default();

        assert_eq!(
            clock.observe(epoch + Duration::from_nanos(41_000_000)),
            epoch + Duration::from_nanos(41_000_000)
        );
        assert_eq!(
            clock.observe(epoch + Duration::from_nanos(41_025_000)),
            epoch + Duration::from_nanos(41_025_000)
        );
        // A stale backend-local sample cannot move the process-tree clock back.
        assert_eq!(
            clock.observe(epoch + Duration::from_nanos(41_010_000)),
            epoch + Duration::from_nanos(41_025_000)
        );
    }

    #[test]
    fn guest_clock_absolute_deadline_stays_ahead_of_committed_time() {
        let committed_time = LogicalTime::from_secs(1_000) + Duration::from_millis(250);
        let mut clock = GuestClock::default();
        let guest_now = clock.observe(committed_time);
        let deadline = guest_now + Duration::from_millis(100);

        assert_eq!(guest_now, committed_time);
        assert!(deadline > committed_time);
    }

    #[test]
    fn guest_clock_process_tree_shares_one_monotonic_domain() {
        let epoch = LogicalTime::from_secs(1_000);
        let root = Arc::new(Mutex::new(GuestClock::default()));
        let forked_child = Arc::clone(&root);

        assert!(Arc::ptr_eq(&root, &forked_child));
        assert_eq!(
            root.lock().unwrap().observe(epoch + Duration::from_secs(1)),
            epoch + Duration::from_secs(1)
        );
        assert_eq!(
            forked_child
                .lock()
                .unwrap()
                .observe(epoch + Duration::from_secs(2)),
            epoch + Duration::from_secs(2)
        );

        // Exec retains the same clock object and does not rebase elapsed time.
        let execed_child = Arc::clone(&forked_child);
        assert!(Arc::ptr_eq(&root, &execed_child));
        assert_eq!(
            execed_child
                .lock()
                .unwrap()
                .observe(epoch + Duration::from_secs(9)),
            epoch + Duration::from_secs(9)
        );
    }

    #[test]
    fn initial_current_image_is_selected_once_before_registration() {
        let tid = DetTid::from_raw(3);
        let mut state = ThreadState::new(tid, &Config::default(), ());
        assert_eq!(state.mm_id, MmId::initial(tid)); // No fact: old default.
        state.physical_tid = Some(tid.as_raw());
        let physical_tid = state.physical_tid;
        state.bind_initial_current_image(tid).unwrap();
        let current = MmId::initial(tid).for_exec(tid);
        assert_eq!(state.mm_id, current);
        assert_eq!(state.physical_tid, physical_tid);
        assert!(!state.thread_start_entered);
        assert!(state.detpid.is_none());
        assert_eq!(state.bind_initial_current_image(tid), Err(Errno::EPROTO));
        assert_eq!(state.mm_id, current);
    }

    #[test]
    fn initial_current_image_rejects_nonfresh_or_other_thread_state() {
        let tid = DetTid::from_raw(3);
        for case in 0..7 {
            let mut state = ThreadState::new(tid, &Config::default(), ());
            let requested = if case == 0 { DetTid::from_raw(4) } else { tid };
            match case {
                1 => state.detpid = Some(tid),
                2 => state.thread_start_entered = true,
                3 => state.past_global_first_execve = true,
                4 => state.mm_id = state.mm_id.for_exec(tid),
                5 => state.clone_flags = Some(CloneFlags::empty()),
                6 => state.mm_id = MmId::initial(DetTid::from_raw(4)),
                _ => {}
            }
            let before = state.mm_id;
            assert_eq!(
                state.bind_initial_current_image(requested),
                Err(Errno::EPROTO)
            );
            assert_eq!(state.mm_id, before);
        }
    }

    #[test]
    fn unparented_thread_recovers_process_memory_identity() {
        let detpid = DetPid::from_raw(4);
        let dettid = DetTid::from_raw(7);
        let mut state = ThreadState::new(dettid, &Config::default(), ());

        assert!(state.recover_process_mm_id(detpid));
        assert_eq!(state.mm_id, MmId::initial(detpid));
        assert!(!state.recover_process_mm_id(detpid));
    }

    #[test]
    fn inherited_thread_keeps_existing_memory_identity() {
        let detpid = DetPid::from_raw(4);
        let dettid = DetTid::from_raw(7);
        let inherited_mm = MmId::initial(detpid).for_exec(detpid);
        let mut state = ThreadState::new(dettid, &Config::default(), ());
        state.mm_id = inherited_mm;

        assert!(!state.recover_process_mm_id(detpid));
        assert_eq!(state.mm_id, inherited_mm);
    }

    #[test]
    fn backend_can_override_open_file_creator_identity() {
        let host_tid = DetTid::from_raw(10_003);
        let virtual_tid = DetTid::from_raw(3);
        let mut state = ThreadState::new(host_tid, &Config::default(), ());

        assert_eq!(state.open_file_creator, None);
        state.set_open_file_creator(virtual_tid);
        assert_eq!(state.open_file_creator, Some(virtual_tid));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1149)
    #[test]
    fn chaos_per_thread_slowdown_factor_is_stable_and_deterministic() {
        let seed = 0xdead_beef_u64;
        let max_factor: f64 = 10.0;
        // Deterministic: same (seed, dettid) -> identical factor, every call.
        for raw in 1..=64 {
            let tid = DetTid::from_raw(raw);
            let a = chaos_per_thread_slowdown_factor(seed, tid, 0, max_factor);
            let b = chaos_per_thread_slowdown_factor(seed, tid, 0, max_factor);
            assert_eq!(
                a, b,
                "factor must be a pure function of (seed, dettid, epoch)"
            );
            // Log-uniform in [1/R, R].
            let a = a.as_f64();
            assert!(
                a >= 1.0 / max_factor - 1e-9 && a <= max_factor + 1e-9,
                "factor {} out of [1/{max_factor}, {max_factor}] for tid {raw}",
                a
            );
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1149)
    #[test]
    fn chaos_per_thread_slowdown_factor_varies_across_threads_and_seeds() {
        let max_factor = 10.0;
        // Different threads (same seed) get a spread of factors, not all equal.
        let factors: Vec<f64> = (1..=32)
            .map(|raw| {
                chaos_per_thread_slowdown_factor(1234, DetTid::from_raw(raw), 0, max_factor)
                    .as_f64()
            })
            .collect();
        let first = factors[0];
        assert!(
            factors.iter().any(|&f| (f - first).abs() > 1e-6),
            "per-thread factors should differ across threads"
        );
        // Different seeds give a different factor for the same thread.
        let tid = DetTid::from_raw(7);
        let f_a = chaos_per_thread_slowdown_factor(1, tid, 0, max_factor).as_f64();
        let f_b = chaos_per_thread_slowdown_factor(2, tid, 0, max_factor).as_f64();
        assert!(
            (f_a - f_b).abs() > 1e-12,
            "different seeds should yield different factors for the same thread"
        );
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1149)
    #[test]
    fn chaos_per_thread_slowdown_factor_disabled_when_max_factor_at_most_one() {
        // max_factor <= 1.0 disables the spread: every thread is nominal (1.0).
        for raw in 1..=16 {
            let tid = DetTid::from_raw(raw);
            assert_eq!(
                chaos_per_thread_slowdown_factor(99, tid, 0, 1.0),
                RcbTimeMultiplier::ONE
            );
            assert_eq!(
                chaos_per_thread_slowdown_factor(99, tid, 0, 0.5),
                RcbTimeMultiplier::ONE
            );
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn chaos_epoch_zero_reproduces_epochless_factor() {
        // Enabling epochs must never perturb the FIRST epoch: epoch 0 has to
        // yield exactly the value the epoch-less #1149 code produced (the epoch
        // term folds to 0 in the mix).
        use rand::RngExt as _;
        use rand::SeedableRng as _;
        let max_factor: f64 = 10.0;
        for raw in 1..=64 {
            let tid = DetTid::from_raw(raw);
            for &seed in &[0u64, 1, 7, 0xdead_beef, u64::MAX] {
                let epochless = {
                    // Reconstruct the exact epoch-less mix inline to pin the
                    // invariant independent of the production function body.
                    const SLOWDOWN_SALT: u64 = 0x736c_6f77_646f_776e;
                    let mixed = seed
                        ^ SLOWDOWN_SALT
                        ^ ((tid.as_raw() as u32 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
                    let mut prng = Pcg64Mcg::seed_from_u64(mixed);
                    let u: f64 = prng.random::<f64>();
                    max_factor.powf(2.0 * u - 1.0)
                };
                assert_eq!(
                    chaos_per_thread_slowdown_factor(seed, tid, 0, max_factor),
                    RcbTimeMultiplier::from_f64(epochless),
                    "epoch 0 must reproduce the epoch-less factor for seed {seed}, tid {raw}"
                );
            }
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn chaos_epoch_factor_varies_deterministically_across_epochs() {
        let max_factor = 10.0;
        let seed = 0x1234_5678_u64;
        let tid = DetTid::from_raw(3);
        // Each epoch draws a fresh factor for the same thread; the sequence is
        // a pure function of (seed, dettid, epoch), hence replayable.
        let factors: Vec<f64> = (0..16)
            .map(|epoch| chaos_per_thread_slowdown_factor(seed, tid, epoch, max_factor).as_f64())
            .collect();
        // Purity: recomputing any epoch yields the identical value.
        for (epoch, &f) in factors.iter().enumerate() {
            assert_eq!(
                chaos_per_thread_slowdown_factor(seed, tid, epoch as u64, max_factor).as_f64(),
                f,
                "factor must be pure in epoch"
            );
            // Stays in the log-uniform range.
            assert!(f >= 1.0 / max_factor - 1e-9 && f <= max_factor + 1e-9);
        }
        // The factor actually changes across epochs (not a constant stream).
        let first = factors[0];
        assert!(
            factors.iter().any(|&f| (f - first).abs() > 1e-6),
            "per-epoch factors should differ across epochs"
        );
    }

    fn cpu_snapshot(
        user: u64,
        system: u64,
        children_user: u64,
        children_system: u64,
    ) -> ProcessCpuSnapshot {
        ProcessCpuSnapshot {
            user: LogicalTime::from_nanos(user),
            system: LogicalTime::from_nanos(system),
            children_user: LogicalTime::from_nanos(children_user),
            children_system: LogicalTime::from_nanos(children_system),
        }
    }

    #[test]
    fn child_cpu_time_is_hidden_until_reap() {
        let pid = DetPid::from_raw(2);
        let mut parent = ProcessCpuTime::default();
        parent.record_exited_child(pid, cpu_snapshot(10, 20, 3, 4));

        assert_eq!(parent.snapshot.children_user, LogicalTime::ZERO);
        assert_eq!(parent.snapshot.children_system, LogicalTime::ZERO);

        parent.reap_child(pid);
        assert_eq!(parent.snapshot.children_user, LogicalTime::from_nanos(13));
        assert_eq!(parent.snapshot.children_system, LogicalTime::from_nanos(24));
    }

    #[test]
    fn reaping_nonexited_child_does_not_change_accounting() {
        let pid = DetPid::from_raw(2);
        let mut parent = ProcessCpuTime::default();

        parent.reap_child(pid);
        assert_eq!(parent.snapshot.children_user, LogicalTime::ZERO);
        assert_eq!(parent.snapshot.children_system, LogicalTime::ZERO);
    }

    #[test]
    fn child_cpu_time_uses_final_thread_snapshot_and_drops_reaped_state() {
        let pid = DetPid::from_raw(2);
        let mut parent = ProcessCpuTime::default();

        parent.record_exited_child(pid, cpu_snapshot(10, 20, 3, 4));
        parent.record_exited_child(pid, cpu_snapshot(12, 25, 4, 5));

        parent.reap_child(pid);
        assert_eq!(parent.snapshot.children_user, LogicalTime::from_nanos(16));
        assert_eq!(parent.snapshot.children_system, LogicalTime::from_nanos(30));
        assert!(parent.exited_children.is_empty());

        parent.reap_child(pid);
        assert_eq!(parent.snapshot.children_user, LogicalTime::from_nanos(16));
        assert_eq!(parent.snapshot.children_system, LogicalTime::from_nanos(30));
    }

    #[tokio::test]
    async fn child_cpu_publication_replacement_rejects_stale_owner_and_snapshot() {
        use std::future::Future;
        use std::task::Context;

        let cfg = Config::default();
        let child = DetPid::from_raw(7);
        let mut parent = ThreadState::new(DetPid::from_raw(3), &cfg, ());
        assert!(!parent.wait_for_child_cpu_publication(child).await);
        let before = serde_json::to_value(&*parent.process_cpu_time.lock().unwrap()).unwrap();
        let first = parent.prepare_child_cpu_publication(child);
        let mut old_child = ThreadState::new(child, &cfg, ());
        old_child.parent_process_cpu_time = Some(Arc::clone(&parent.process_cpu_time));
        old_child.parent_cpu_publication = Some(Arc::clone(&first));
        let mut old_wait = Box::pin(parent.wait_for_child_cpu_publication(child));
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(old_wait.as_mut().poll(&mut cx).is_pending());
        let second = parent.prepare_child_cpu_publication(child);
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(!old_wait.await);
        assert!(!old_child.record_exited_child_process_cpu_time(child));
        assert!(!parent.has_exited_child_process_cpu_time(child));
        // Neither runtime notification state nor host wakers enter recordings.
        {
            let cpu = parent.process_cpu_time.lock().unwrap();
            assert_eq!(serde_json::to_value(&*cpu).unwrap(), before);
            let clone = cpu.clone();
            assert!(Arc::ptr_eq(
                clone.child_cpu_publications.get(&child).unwrap(),
                &second
            ));
            let restored: ProcessCpuTime = serde_json::from_value(before).unwrap();
            assert!(restored.child_cpu_publications.is_empty());
        }

        let mut current_child = ThreadState::new(child, &cfg, ());
        current_child.parent_process_cpu_time = Some(Arc::clone(&parent.process_cpu_time));
        current_child.parent_cpu_publication = Some(Arc::clone(&second));
        assert!(current_child.record_exited_child_process_cpu_time(child));
        parent.prepare_child_process_cpu_time(child);
        assert!(parent.has_exited_child_process_cpu_time(child));
        assert!(parent.wait_for_child_cpu_publication(child).await);
        // ThreadState clones retain the same process identity.
        assert!(Arc::ptr_eq(
            current_child
                .clone()
                .parent_cpu_publication
                .as_ref()
                .unwrap(),
            &second
        ));
        parent.reap_child_process_cpu_time(child);
        assert!(!parent.wait_for_child_cpu_publication(child).await);
        assert!(!current_child.record_exited_child_process_cpu_time(child));
        assert!(!parent.has_exited_child_process_cpu_time(child));
    }

    /// An exit re-parents a child whose final-CPU publication its creator
    /// holds; the reaper whose wait the scheduler selects it for takes the
    /// publication over, with the snapshot an exited child already recorded,
    /// and reaps the child's times into its own, as Linux's
    /// `wait_task_zombie` adds them to the reaper's. A child that exits after
    /// the takeover records into the reaper too.
    #[tokio::test]
    async fn a_reparented_childs_cpu_publication_moves_to_the_reaper_that_waits() {
        use std::future::Future;
        use std::task::Context;

        let cfg = Config::default();
        for exits_first in [true, false] {
            let child = DetPid::from_raw(7);
            let creator = ThreadState::new(DetPid::from_raw(5), &cfg, ());
            let mut reaper = ThreadState::new(DetPid::from_raw(3), &cfg, ());
            reaper.child_cpu_owners = Arc::clone(&creator.child_cpu_owners);
            let token = creator.prepare_child_cpu_publication(child);
            let mut orphan = ThreadState::new(child, &cfg, ());
            orphan.child_cpu_owners = Arc::clone(&creator.child_cpu_owners);
            orphan.parent_process_cpu_time = Some(Arc::clone(&creator.process_cpu_time));
            orphan.parent_cpu_publication = Some(Arc::clone(&token));
            orphan.process_cpu_time.lock().unwrap().snapshot = cpu_snapshot(10, 20, 3, 4);
            if exits_first {
                assert!(orphan.record_exited_child_process_cpu_time(child));
                assert!(reaper.wait_for_child_cpu_publication(child).await);
            } else {
                let mut wait = Box::pin(reaper.wait_for_child_cpu_publication(child));
                let mut cx = Context::from_waker(futures::task::noop_waker_ref());
                assert!(wait.as_mut().poll(&mut cx).is_pending());
                assert!(orphan.record_exited_child_process_cpu_time(child));
                assert!(wait.await);
            }
            assert!(
                reaper.has_exited_child_process_cpu_time(child),
                "{exits_first}"
            );
            {
                let creator = creator.process_cpu_time.lock().unwrap();
                assert!(!creator.child_cpu_publications.contains_key(&child));
                assert!(!creator.exited_children.contains_key(&child));
            }
            let recorded = reaper.process_cpu_time.lock().unwrap().exited_children[&child];
            assert!(recorded.user >= LogicalTime::from_nanos(10));
            reaper.reap_child_process_cpu_time(child);
            let after = reaper.process_cpu_time.lock().unwrap().snapshot;
            assert_eq!(after.children_user, recorded.user + recorded.children_user);
            assert_eq!(
                after.children_system,
                recorded.system + recorded.children_system
            );
            assert!(
                !creator
                    .child_cpu_owners
                    .lock()
                    .unwrap()
                    .contains_key(&child)
            );
        }
    }

    fn nz(value: u64) -> Option<NonZeroU64> {
        NonZeroU64::new(value)
    }

    /// The cap of hermit#4121 only lowers the software slice end, keeps one
    /// RCB of room (10 ns at the default clock) plus one nanosecond, and
    /// leaves the PMU maximum alone.
    #[test]
    fn a_deadline_cap_lowers_the_slice_end_and_keeps_one_rcb() {
        let config = Config {
            max_timeslice: nz(200_000_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(3), &config, ());
        state.next_timeslice(&config);
        let now = state.thread_logical_time.as_nanos();
        let end = state.end_of_timeslice.unwrap();
        assert_eq!(end, now + Duration::from_nanos(200_000_000));
        assert_eq!(state.max_timeslice_end, Some(end));

        state.cap_timeslice_at_deadline(
            Duration::from_nanos(5_000_000),
            1.0,
            Duration::ZERO,
            false,
        );
        let capped = now + Duration::from_nanos(5_000_000);
        assert_eq!(state.end_of_timeslice, Some(capped));
        assert_eq!(state.max_timeslice_end, Some(end));

        // A later deadline never raises the end.
        state.cap_timeslice_at_deadline(
            Duration::from_nanos(9_000_000),
            1.0,
            Duration::ZERO,
            false,
        );
        assert_eq!(state.end_of_timeslice, Some(capped));

        // A deadline already due leaves one RCB and a nanosecond.
        state.cap_timeslice_at_deadline(Duration::ZERO, 1.0, Duration::ZERO, false);
        assert_eq!(state.end_of_timeslice, Some(now + Duration::from_nanos(11)));
        assert_eq!(state.max_timeslice_end, Some(end));
    }

    /// A schedule replay of a record whose slices ran without the bound
    /// ends a slice at the replay backstop instead of `--max-timeslice` while
    /// the replayer sends one. A thread's first slice, begun under the plain
    /// bound before its first traced event, moves out to the backstop when it
    /// first arrives; a later one moves nothing, and once the record runs out
    /// (no backstop) the next slice is bounded as before. Outside a schedule
    /// replay the backstop is ignored.
    #[test]
    fn an_uncapped_replay_ends_slices_at_its_backstop_while_the_record_lasts() {
        let bound = std::num::NonZeroU64::new(100_000);
        let config = Config {
            max_timeslice: bound,
            replay_schedule_from: Some(std::path::PathBuf::from("record.json")),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(3), &config, ());
        state.next_timeslice(&config);
        let start = state.thread_logical_time.as_nanos();
        assert_eq!(
            state.max_timeslice_end,
            Some(start + Duration::from_nanos(100_000))
        );
        let backstop = LogicalTime::from_nanos(3_000_000);
        state.learn_uncapped_backstop(Some(backstop), bound, true);
        let moved = start + Duration::from_nanos(3_000_000);
        assert_eq!(state.end_of_timeslice, Some(moved));
        assert_eq!(state.max_timeslice_end, Some(moved));
        assert_eq!(state.max_slice_length(&config), Some(3_000_000));
        // A backstop that arrives again moves nothing.
        state.end_of_timeslice = Some(start + Duration::from_nanos(7));
        state.learn_uncapped_backstop(Some(backstop), bound, true);
        assert_eq!(
            state.end_of_timeslice,
            Some(start + Duration::from_nanos(7))
        );
        // A backstop shorter than the bound never shortens a slice.
        state.learn_uncapped_backstop(Some(LogicalTime::from_nanos(10)), bound, true);
        assert_eq!(state.max_slice_length(&config), Some(100_000));
        // The record ran out.
        state.learn_uncapped_backstop(None, bound, true);
        assert_eq!(state.uncapped_backstop, None);
        state.next_timeslice(&config);
        let now = state.thread_logical_time.as_nanos();
        assert_eq!(
            state.max_timeslice_end,
            Some(now + Duration::from_nanos(100_000))
        );
        // Not a schedule replay.
        let plain = Config {
            max_timeslice: bound,
            ..Default::default()
        };
        state.uncapped_backstop = Some(backstop);
        assert_eq!(state.max_slice_length(&plain), Some(100_000));
    }

    /// A replay backstop never outlives the replayed schedule. A reply that
    /// says the schedule is over leaves the thread no backstop, even one that
    /// carries a backstop, as a reply built from state read before another
    /// thread ended the schedule did; and a thread whose schedule is over ends
    /// its slices at the bound even while it holds one. The next ordinary
    /// slice after the schedule is bounded, and a backstop that first arrives
    /// with the end moves no slice end.
    #[test]
    fn a_backstop_never_outlives_the_replayed_schedule() {
        let bound = std::num::NonZeroU64::new(100_000);
        let config = Config {
            max_timeslice: bound,
            replay_schedule_from: Some(std::path::PathBuf::from("record.json")),
            ..Default::default()
        };
        let backstop = LogicalTime::from_nanos(3_000_000);
        let mut state = ThreadState::new(DetPid::from_raw(3), &config, ());
        state.next_timeslice(&config);
        state.hear_schedule_replay_reply(false, Some(backstop), bound, true);
        assert_eq!(state.max_slice_length(&config), Some(3_000_000));
        // The reply that ends the schedule, carrying a stale backstop.
        state.hear_schedule_replay_reply(true, Some(backstop), bound, true);
        assert!(state.schedule_replay_over);
        assert_eq!(state.uncapped_backstop, None);
        state.next_timeslice(&config);
        let now = state.thread_logical_time.as_nanos();
        assert_eq!(
            state.max_timeslice_end,
            Some(now + Duration::from_nanos(100_000))
        );
        state.uncapped_backstop = Some(backstop);
        assert_eq!(state.max_slice_length(&config), Some(100_000));
        // A thread whose first backstop comes with the end of the schedule.
        let mut late = ThreadState::new(DetPid::from_raw(4), &config, ());
        late.next_timeslice(&config);
        let (end, max_end) = (late.end_of_timeslice, late.max_timeslice_end);
        late.hear_schedule_replay_reply(true, Some(backstop), bound, true);
        assert_eq!(late.uncapped_backstop, None);
        assert_eq!(
            (late.end_of_timeslice, late.max_timeslice_end),
            (end, max_end)
        );
    }

    /// The cap never ends a slice sooner than one scheduler turn charge
    /// (`min_slice`) after it is applied, so the turn it causes costs no more
    /// virtual time than the work the thread did.
    #[test]
    fn a_deadline_cap_keeps_at_least_one_turn_charge() {
        let config = Config {
            max_timeslice: nz(200_000_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(3), &config, ());
        state.next_timeslice(&config);
        let now = state.thread_logical_time.as_nanos();
        let turn = Duration::from_nanos(500_000);
        state.cap_timeslice_at_deadline(Duration::from_nanos(1_000), 1.0, turn, false);
        assert_eq!(state.end_of_timeslice, Some(now + turn));
        state.cap_timeslice_at_deadline(Duration::ZERO, 1.0, turn, false);
        assert_eq!(state.end_of_timeslice, Some(now + turn));
        state.cap_timeslice_at_deadline(Duration::from_nanos(700_000), 1.0, turn, false);
        assert_eq!(state.end_of_timeslice, Some(now + turn));
    }

    /// Schedule replay follows its recorded turns: the cap leaves its slices
    /// alone.
    #[test]
    fn schedule_replay_takes_no_deadline_cap() {
        let config = Config {
            max_timeslice: nz(200_000_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(3), &config, ());
        state.next_timeslice(&config);
        let end = state.end_of_timeslice;
        state.cap_timeslice_at_deadline(Duration::from_nanos(5_000), 1.0, Duration::ZERO, true);
        assert_eq!(state.end_of_timeslice, end);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn constant_slowdown_is_the_single_epoch_case() {
        let config = Config {
            chaos: true,
            chaos_per_thread_slowdown: true,
            chaos_epoch_length_ns: 0,
            target_timeslice: nz(10_000),
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(3), &config, ());
        state.next_timeslice(&config);
        let first = state.take_pending_chaos_epochs().pop().unwrap();
        assert_eq!(first.epoch, 0);
        assert_eq!(state.chaos_epoch, 0);

        state
            .thread_logical_time
            .add_rcbs_with_multiplier(10_000, first.factor);
        state.next_timeslice(&config);
        assert_eq!(state.chaos_epoch, 0);
        assert_eq!(state.chaos_slowdown_factor, first.factor);
        assert!(state.take_pending_chaos_epochs().is_empty());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn epoch_redraw_uses_elapsed_logical_time_at_commit_boundaries() {
        let config = Config {
            chaos: true,
            chaos_per_thread_slowdown: true,
            chaos_epoch_length_ns: 100,
            target_timeslice: nz(10_000),
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let tid = DetPid::from_raw(5);
        let mut state = ThreadState::new(tid, &config, ());
        state.next_timeslice(&config);
        let first = state.pending_chaos_epochs[0];

        state
            .thread_logical_time
            .add_rcbs_with_multiplier(1_000, first.factor);
        let expected_epoch =
            state.thread_logical_time.without_starting().as_nanos() / config.chaos_epoch_length_ns;
        assert!(expected_epoch > 0);

        state.next_timeslice(&config);
        let transitions = state.take_pending_chaos_epochs();
        assert_eq!(transitions.len(), 2);
        assert_eq!(transitions[0], first);
        let redraw = transitions[1];
        assert_eq!(redraw.epoch, expected_epoch);
        assert_eq!(
            redraw.factor,
            chaos_per_thread_slowdown_factor(
                config.sched_seed(),
                tid,
                expected_epoch,
                config.chaos_slowdown_max_factor,
            )
        );
        assert!(redraw.logical_time > first.logical_time);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn replay_installs_recorded_epoch_without_ambient_chaos_flags() {
        let config = Config {
            target_timeslice: nz(10_000),
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let transition = ChaosEpochTransition {
            logical_time: LogicalTime::ZERO,
            epoch: 7,
            factor: RcbTimeMultiplier::from_f64(3.25),
        };
        let mut state = ThreadState::new(DetPid::from_raw(5), &config, ());
        state.preemption_points = Some(
            ThreadHistory::new()
                .with_chaos_epochs(vec![transition])
                .into_iter(),
        );

        state.next_timeslice(&config);

        assert!(state.chaos_slowdown_active);
        assert_eq!(state.chaos_epoch, transition.epoch);
        assert_eq!(state.chaos_slowdown_factor, transition.factor);
        assert!(state.take_pending_chaos_epochs().is_empty());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn replay_installs_boundary_epoch_when_pmu_checks_in_early() {
        let config = Config {
            target_timeslice: nz(10_000),
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(5), &config, ());
        let now = state.thread_logical_time.as_nanos();
        let first = ChaosEpochTransition {
            logical_time: now,
            epoch: 0,
            factor: RcbTimeMultiplier::from_f64(2.0),
        };
        let second = ChaosEpochTransition {
            logical_time: now + Duration::from_nanos(100),
            epoch: 1,
            factor: RcbTimeMultiplier::from_f64(3.0),
        };
        let history = ThreadHistory::new()
            .with_prio_changes(vec![
                (now + Duration::from_nanos(100), DEFAULT_PRIORITY),
                (now + Duration::from_nanos(200), DEFAULT_PRIORITY),
            ])
            .with_preemption_rcbs(vec![4, 8])
            .with_chaos_epochs(vec![first, second]);
        state.preemption_points = Some(history.into_iter());

        state.next_timeslice(&config);
        assert_eq!(state.chaos_slowdown_factor, first.factor);
        assert_eq!(
            state.end_of_timeslice,
            Some(now + Duration::from_nanos(100))
        );

        // Four 2x RCBs advance to 80ns: within one RCB of the exact 100ns
        // boundary, matching the early check-in path in post_handler_hook.
        state
            .thread_logical_time
            .add_rcbs_with_multiplier(4, first.factor);
        state.committed_clock_value = 4;
        assert!(state.timeslice_expired());
        state.next_timeslice(&config);

        assert_eq!(
            state.thread_logical_time.as_nanos(),
            now + Duration::from_nanos(80)
        );
        assert_eq!(state.chaos_epoch, second.epoch);
        assert_eq!(state.chaos_slowdown_factor, second.factor);
        assert_eq!(
            state.end_of_timeslice,
            Some(now + Duration::from_nanos(200))
        );
    }

    /// A thread at 500 ns of logical time (50 RCBs at multiplier 1) and PMU
    /// count 10, replaying `history` from its start with `config`: returns
    /// the thread, the priority its next slice gets, and the time it chose
    /// that slice at.
    fn replay_from_500ns(
        config: &Config,
        history: ThreadHistory,
    ) -> (ThreadState<()>, Option<Priority>, LogicalTime) {
        let mut state = ThreadState::new(DetPid::from_raw(5), config, ());
        state.preemption_points = Some(history.into_iter());
        state
            .thread_logical_time
            .add_rcbs_with_multiplier(50, RcbTimeMultiplier::ONE);
        state.committed_clock_value = 10;
        let now = state.thread_logical_time.as_nanos();
        let prio = state.next_timeslice(config);
        (state, prio, now)
    }

    fn edited_replay_config() -> Config {
        Config {
            target_timeslice: nz(10_000),
            max_timeslice: nz(100_000),
            replay_edited_preemptions: true,
            ..Default::default()
        }
    }

    /// With --replay-edited-preemptions, recorded slices that ended before
    /// the thread got to them are skipped: the first slice still ahead
    /// applies, with its own end, priority and RCB target, and the slice
    /// after it follows as recorded. Slices whose RCB target has passed, or
    /// that end within the post-hook's one-RCB margin, count as ended.
    #[test]
    fn an_edited_record_skips_the_slices_that_already_ended() {
        let config = edited_replay_config();
        let start = ThreadState::new(DetPid::from_raw(5), &config, ())
            .thread_logical_time
            .as_nanos();
        let at = |ns: u64| start + Duration::from_nanos(ns);
        let history = ThreadHistory::new()
            .with_prio_changes(vec![
                (at(100), DEFAULT_PRIORITY + 1),
                // RCB target behind, end within one RCB (10 ns) of 500 ns.
                (at(505), DEFAULT_PRIORITY + 2),
                (at(900), DEFAULT_PRIORITY + 3),
                (at(1300), DEFAULT_PRIORITY + 4),
            ])
            .with_preemption_rcbs(vec![4, 9, 90, 130]);
        let (mut state, prio, _) = replay_from_500ns(&config, history);
        assert_eq!(prio, Some(DEFAULT_PRIORITY + 3));
        assert_eq!(state.end_of_timeslice, Some(at(900)));
        assert_eq!(state.replay_rcb_end, Some(90));

        state
            .thread_logical_time
            .add_rcbs_with_multiplier(40, RcbTimeMultiplier::ONE);
        state.committed_clock_value = 90;
        assert!(state.timeslice_expired());
        assert_eq!(state.next_timeslice(&config), Some(DEFAULT_PRIORITY + 4));
        assert_eq!(state.end_of_timeslice, Some(at(1300)));
        assert_eq!(state.replay_rcb_end, Some(130));
    }

    /// Only one clock may have passed: an RCB target still ahead governs a
    /// slice whose end has passed, and an end still ahead (with no RCB
    /// target, or a passed one) governs on its own.
    #[test]
    fn an_edited_record_keeps_a_slice_one_of_whose_deadlines_is_ahead() {
        let config = edited_replay_config();
        let start = ThreadState::new(DetPid::from_raw(5), &config, ())
            .thread_logical_time
            .as_nanos();
        let at = |ns: u64| start + Duration::from_nanos(ns);
        let one_slice = |end: u64, rcbs: Option<u64>| {
            let history = ThreadHistory::new().with_prio_changes(vec![
                (at(end), DEFAULT_PRIORITY + 7),
                (at(5000), DEFAULT_PRIORITY),
            ]);
            match rcbs {
                Some(rcbs) => history.with_preemption_rcbs(vec![rcbs, 500]),
                None => history,
            }
        };
        // End passed, RCB target ahead: the target governs.
        let (state, prio, _) = replay_from_500ns(&config, one_slice(100, Some(20)));
        assert_eq!(prio, Some(DEFAULT_PRIORITY + 7));
        assert_eq!(state.replay_rcb_end, Some(20));
        assert!(!state.timeslice_expired());
        // RCB target passed, end ahead: the end governs, without the target.
        let (state, prio, _) = replay_from_500ns(&config, one_slice(800, Some(4)));
        assert_eq!(prio, Some(DEFAULT_PRIORITY + 7));
        assert_eq!(
            (state.end_of_timeslice, state.replay_rcb_end),
            (Some(at(800)), None)
        );
        // An RCB target exactly at the current count ends the slice at once,
        // as in any replay.
        let (state, prio, _) = replay_from_500ns(&config, one_slice(100, Some(10)));
        assert_eq!(prio, Some(DEFAULT_PRIORITY + 7));
        assert_eq!(state.replay_rcb_end, Some(10));
        assert!(state.timeslice_expired());
    }

    /// When every recorded slice has ended, the thread goes on at the
    /// record's final priority, as after the last recorded slice.
    #[test]
    fn an_edited_record_whose_slices_all_ended_goes_on_at_its_final_priority() {
        let config = edited_replay_config();
        let start = ThreadState::new(DetPid::from_raw(5), &config, ())
            .thread_logical_time
            .as_nanos();
        let history = ThreadHistory::new().with_prio_changes(vec![
            (start + Duration::from_nanos(100), DEFAULT_PRIORITY + 1),
            (start + Duration::from_nanos(200), DEFAULT_PRIORITY + 2),
        ]);
        let final_priority = history.clone().into_iter().final_priority();
        let (state, prio, now) = replay_from_500ns(&config, history);
        assert_eq!(prio, Some(final_priority));
        // As after the last recorded slice of any replay: no recorded end,
        // so the configured maximum timeslice bounds it.
        assert_eq!(
            state.end_of_timeslice,
            Some(now + Duration::from_nanos(100_000))
        );
        assert!(state.stats.last_recorded_slice.is_some());
    }

    /// The one-RCB margin follows the clock multiplier and the thread's chaos
    /// slowdown: at multiplier 0.3 one RCB is 3 ns, so an end 2 ns ahead is
    /// within it (the post-hook would end that slice at once) and one 3 ns
    /// ahead is not. Without a PMU maximum there is no margin.
    #[test]
    fn the_skipping_margin_is_one_rcb_at_the_thread_clock() {
        let config = Config {
            clock_multiplier: Some(0.3),
            ..edited_replay_config()
        };
        let state = ThreadState::new(DetPid::from_raw(5), &config, ());
        assert_eq!(state.pmu_epsilon(&config), Duration::from_nanos(3));
        let mut slowed = ThreadState::new(DetPid::from_raw(5), &config, ());
        slowed.chaos_slowdown_factor = RcbTimeMultiplier::from_f64(2.0);
        slowed.chaos_slowdown_active = true;
        assert_eq!(slowed.pmu_epsilon(&config), Duration::from_nanos(6));

        let start = state.thread_logical_time.as_nanos();
        for (ahead, skipped) in [(2, true), (3, false)] {
            // 50 RCBs at multiplier 1 are 500 ns; at 0.3 the thread's time
            // advances by 150 ns.
            let mut state = ThreadState::new(DetPid::from_raw(5), &config, ());
            state
                .thread_logical_time
                .add_rcbs_with_multiplier(50, RcbTimeMultiplier::ONE);
            let now = state.thread_logical_time.as_nanos();
            state.preemption_points = Some(
                ThreadHistory::new()
                    .with_prio_changes(vec![
                        (now + Duration::from_nanos(ahead), DEFAULT_PRIORITY + 1),
                        (start + Duration::from_nanos(9000), DEFAULT_PRIORITY + 2),
                    ])
                    .into_iter(),
            );
            let prio = state.next_timeslice(&config);
            let expected = if skipped {
                DEFAULT_PRIORITY + 2
            } else {
                DEFAULT_PRIORITY + 1
            };
            assert_eq!(prio, Some(expected), "{ahead} ns ahead");
        }

        // At a 2 times slowdown (a recorded epoch) one RCB is 6 ns: an end
        // 5 ns ahead is within it, one 6 ns ahead is not.
        for (ahead, skipped) in [(5, true), (6, false)] {
            let mut state = ThreadState::new(DetPid::from_raw(5), &config, ());
            let begin = state.thread_logical_time.as_nanos();
            state.preemption_points = Some(
                ThreadHistory::new()
                    .with_prio_changes(vec![
                        (begin + Duration::from_nanos(ahead), DEFAULT_PRIORITY + 1),
                        (begin + Duration::from_nanos(9000), DEFAULT_PRIORITY + 2),
                    ])
                    .with_chaos_epochs(vec![ChaosEpochTransition {
                        logical_time: begin,
                        epoch: 0,
                        factor: RcbTimeMultiplier::from_f64(2.0),
                    }])
                    .into_iter(),
            );
            let prio = state.next_timeslice(&config);
            assert_eq!(state.pmu_epsilon(&config), Duration::from_nanos(6));
            let expected = if skipped {
                DEFAULT_PRIORITY + 2
            } else {
                DEFAULT_PRIORITY + 1
            };
            assert_eq!(prio, Some(expected), "{ahead} ns ahead at 2 times");
        }

        // Without a PMU maximum the post-hook has no margin: a slice is kept
        // whenever it ends ahead, as in a faithful replay.
        let unbounded = Config {
            max_timeslice: None,
            ..edited_replay_config()
        };
        for (end, expected) in [(501, DEFAULT_PRIORITY + 1), (500, DEFAULT_PRIORITY + 2)] {
            let history = ThreadHistory::new().with_prio_changes(vec![
                (start + Duration::from_nanos(end), DEFAULT_PRIORITY + 1),
                (start + Duration::from_nanos(9000), DEFAULT_PRIORITY + 2),
            ]);
            let (_, prio, _) = replay_from_500ns(&unbounded, history);
            assert_eq!(
                prio,
                Some(expected),
                "an end at {end} ns, without a maximum"
            );
        }
    }

    /// A recorded one-RCB slice that starts exactly where the previous one
    /// ended is one the post-hook runs, not one that ended: on a record
    /// without RCB targets (as minimization writes), an edited replay keeps
    /// every such slice, with the same ends and priorities as a faithful one.
    #[test]
    fn an_edited_replay_keeps_one_rcb_slices_as_a_faithful_one_does() {
        let edited = edited_replay_config();
        let faithful = Config {
            replay_edited_preemptions: false,
            ..edited_replay_config()
        };
        let start = ThreadState::new(DetPid::from_raw(5), &edited, ())
            .thread_logical_time
            .as_nanos();
        let at = |ns: u64| start + Duration::from_nanos(ns);
        // At 500 ns, one RCB (10 ns) per slice, then a longer one.
        let history = ThreadHistory::new().with_prio_changes(vec![
            (at(510), DEFAULT_PRIORITY + 1),
            (at(520), DEFAULT_PRIORITY + 2),
            (at(530), DEFAULT_PRIORITY + 3),
            (at(900), DEFAULT_PRIORITY + 4),
        ]);
        let slices = |config: &Config| {
            let (mut state, prio, _) = replay_from_500ns(config, history.clone());
            let mut slices = vec![(prio, state.end_of_timeslice)];
            for _ in 0..3 {
                state.thread_logical_time.add_rcbs(1);
                let prio = state.next_timeslice(config);
                slices.push((prio, state.end_of_timeslice));
            }
            slices
        };
        assert_eq!(slices(&edited), slices(&faithful));
        assert_eq!(
            slices(&edited)[..3],
            [
                (Some(DEFAULT_PRIORITY + 1), Some(at(510))),
                (Some(DEFAULT_PRIORITY + 2), Some(at(520))),
                (Some(DEFAULT_PRIORITY + 3), Some(at(530))),
            ]
        );
    }

    /// Past an edited record's last recorded slice, a restored chaos slowdown
    /// can make one RCB longer than the configured maximum timeslice: the
    /// slice's PMU maximum is kept at least one RCB ahead, so the post-hook
    /// neither ends it at once nor finds itself past it again. A faithful
    /// replay keeps the configured maximum.
    #[test]
    fn past_an_edited_record_the_maximum_keeps_one_rcb_at_a_restored_slowdown() {
        let config = |edited: bool| Config {
            target_timeslice: None,
            max_timeslice: nz(10),
            replay_edited_preemptions: edited,
            ..Default::default()
        };
        for (edited, expected) in [(true, 20), (false, 10)] {
            let config = config(edited);
            let mut state = ThreadState::new(DetPid::from_raw(5), &config, ());
            let now = state.thread_logical_time.as_nanos();
            let doubled = ChaosEpochTransition {
                logical_time: now,
                epoch: 1,
                factor: RcbTimeMultiplier::from_f64(2.0),
            };
            state.preemption_points = Some(
                ThreadHistory::new()
                    .with_chaos_epochs(vec![doubled])
                    .into_iter(),
            );
            state.next_timeslice(&config);
            assert_eq!(state.chaos_slowdown_factor, doubled.factor);
            assert_eq!(state.pmu_epsilon(&config), Duration::from_nanos(20));
            assert!(state.stats.last_recorded_slice.is_some());
            assert_eq!(
                state.max_timeslice_end,
                Some(now + Duration::from_nanos(expected)),
                "edited: {edited}"
            );
        }
    }

    /// Without --replay-edited-preemptions, a replay that reaches a recorded
    /// slice end already passed stops: a faithful replay never does, so the
    /// replay diverged.
    #[test]
    #[should_panic(expected = "needs --replay-edited-preemptions")]
    fn a_faithful_replay_past_a_recorded_end_stops() {
        let config = Config {
            replay_edited_preemptions: false,
            ..edited_replay_config()
        };
        let start = ThreadState::new(DetPid::from_raw(5), &config, ())
            .thread_logical_time
            .as_nanos();
        let history = ThreadHistory::new().with_prio_changes(vec![
            (start + Duration::from_nanos(100), DEFAULT_PRIORITY + 1),
            (start + Duration::from_nanos(900), DEFAULT_PRIORITY),
        ]);
        replay_from_500ns(&config, history);
    }

    /// The same for a recorded RCB deadline the PMU count has passed.
    #[test]
    #[should_panic(expected = "count is already 10. An edited record")]
    fn a_faithful_replay_past_a_recorded_rcb_end_stops() {
        let config = Config {
            replay_edited_preemptions: false,
            ..edited_replay_config()
        };
        let start = ThreadState::new(DetPid::from_raw(5), &config, ())
            .thread_logical_time
            .as_nanos();
        let history = ThreadHistory::new()
            .with_prio_changes(vec![
                (start + Duration::from_nanos(100), DEFAULT_PRIORITY + 1),
                (start + Duration::from_nanos(900), DEFAULT_PRIORITY),
            ])
            .with_preemption_rcbs(vec![4, 90]);
        replay_from_500ns(&config, history);
    }

    // Exercise the same production selector with larger counts and both
    // settings of the optional skid diagnostic. A faithful replay must stop
    // at its first already-passed RCB end, even when later ends remain ahead.
    fn assert_faithful_rcb_ends_stop(panic_on_rcb_overshoot: bool) {
        for counts in [
            vec![40, 80],
            vec![40, 45, 80],
            vec![40, 51, 80],
            vec![40, 45],
        ] {
            let config = Config {
                target_timeslice: nz(10_000),
                max_timeslice: nz(100_000),
                panic_on_rcb_overshoot,
                ..Default::default()
            };
            let mut state = ThreadState::new(DetPid::from_raw(5), &config, ());
            let now = state.thread_logical_time.as_nanos();
            let history = ThreadHistory::new()
                .with_prio_changes(
                    (0..counts.len())
                        .map(|i| {
                            (
                                now + Duration::from_nanos(100 * (i as u64 + 1)),
                                DEFAULT_PRIORITY,
                            )
                        })
                        .collect(),
                )
                .with_preemption_rcbs(counts.clone());
            state.preemption_points = Some(history.into_iter());
            state.committed_clock_value = 51;
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                state.next_timeslice(&config)
            }))
            .expect_err("faithful replay must not pass a recorded RCB end");
            let message = panic.downcast_ref::<String>().unwrap();
            assert!(
                message.contains("Cannot set RCB end of timeslice to 40"),
                "{counts:?}: {message}"
            );
            assert!(
                message.contains("count is already 51. An edited record"),
                "{counts:?}: {message}"
            );
            assert!(
                message.contains("needs --replay-edited-preemptions"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_faithful_replay_past_larger_rcb_ends_stops_by_default() {
        assert_faithful_rcb_ends_stop(false);
    }

    #[test]
    fn a_faithful_replay_past_larger_rcb_ends_stops_with_skid_panic_enabled() {
        assert_faithful_rcb_ends_stop(true);
    }

    /// Edited histories retain the semantics of
    /// https://github.com/rrnewton/hermit/pull/4672: an equal-count slice
    /// ends at once, a future time-only slice is kept, and filed change
    /// points need not advance. The optional diagnostic must not panic.
    #[test]
    fn an_edited_record_reports_no_departure() {
        let config = Config {
            panic_on_rcb_overshoot: true,
            replay_preemptions_from: Some(std::path::PathBuf::from("history.json")),
            ..edited_replay_config()
        };
        let start = ThreadState::new(DetPid::from_raw(5), &config, ())
            .thread_logical_time
            .as_nanos();
        let at = |ns: u64| start + Duration::from_nanos(ns);
        let history = |end: u64, rcbs: u64| {
            ThreadHistory::new()
                .with_prio_changes(vec![
                    (at(end), DEFAULT_PRIORITY + 7),
                    (at(5000), DEFAULT_PRIORITY),
                ])
                .with_preemption_rcbs(vec![rcbs, 500])
        };
        let (mut state, prio, now) = replay_from_500ns(&config, history(100, 10));
        assert_eq!(prio, Some(DEFAULT_PRIORITY + 7));
        assert_eq!(
            (state.end_of_timeslice, state.replay_rcb_end),
            (Some(at(100)), Some(10))
        );
        assert!(state.timeslice_expired());
        assert_eq!(state.file_replay_change_point(&config, now), Ok(()));
        assert_eq!(state.file_replay_change_point(&config, now), Ok(()));
        assert_eq!(state.file_replay_change_point(&config, at(100)), Ok(()));
        let (state, prio, _) = replay_from_500ns(&config, history(800, 4));
        assert_eq!(prio, Some(DEFAULT_PRIORITY + 7));
        assert_eq!(
            (state.end_of_timeslice, state.replay_rcb_end),
            (Some(at(800)), None)
        );
        // Main also keeps this equal-count slice in faithful mode. Unlike
        // edited exploration, filing the same change point twice is refused.
        let faithful = Config {
            replay_edited_preemptions: false,
            ..config
        };
        let (mut state, _, now) = replay_from_500ns(&faithful, history(100, 10));
        assert_eq!(state.replay_rcb_end, Some(10));
        assert_eq!(state.file_replay_change_point(&faithful, now), Ok(()));
        assert_eq!(state.file_replay_change_point(&faithful, now), Err(now));
    }

    /// Main admits an equal-RCB end even when its timestamp is behind.
    /// Preserve that instant slice in this Android-shaped history, but refuse
    /// filing it twice at the preceding slice's unchanged logical instant.
    #[test]
    fn a_replay_keeps_an_equal_rcb_end_and_refuses_repeated_filing() {
        let config = Config {
            target_timeslice: nz(10_000),
            max_timeslice: nz(100_000),
            replay_preemptions_from: Some("history.json".into()),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(31754), &config, ());
        state.thread_logical_time.add_rcbs(863_958);
        state.committed_clock_value = 863_958;
        let now = state.thread_logical_time.as_nanos();
        let history = ThreadHistory::new()
            .with_prio_changes(vec![
                (
                    LogicalTime::from_nanos(now.as_nanos() - 1_802_680),
                    DEFAULT_PRIORITY + 1,
                ),
                (
                    LogicalTime::from_nanos(now.as_nanos() - 1_802_670),
                    DEFAULT_PRIORITY + 2,
                ),
                (now + Duration::from_nanos(10_780), DEFAULT_PRIORITY + 3),
            ])
            .with_preemption_rcbs(vec![863_958, 863_959, 864_037]);
        state.preemption_points = Some(history.into_iter());

        assert_eq!(state.file_replay_change_point(&config, now), Ok(()));
        let prio = state.next_timeslice(&config);
        assert_eq!(prio, Some(DEFAULT_PRIORITY + 1));
        assert_eq!(state.replay_rcb_end, Some(863_958));
        assert_eq!(
            state.end_of_timeslice,
            Some(LogicalTime::from_nanos(now.as_nanos() - 1_802_680))
        );
        assert!(state.timeslice_expired());
        assert_eq!(state.file_replay_change_point(&config, now), Err(now));
        state.abandon_replayed_slices(&config);
        assert!(!state.timeslice_expired());
        assert_eq!(state.end_of_timeslice, Some(LogicalTime::MAX));
    }

    /// An end exactly at the thread's count whose time is not behind it is
    /// the slice's end, now, as before (the record's adjacent zero-RCB
    /// slices, `replay_preserves_adjacent_zero_rcb_slices`).
    #[test]
    fn a_replay_follows_an_end_at_its_count_whose_time_is_ahead() {
        let config = Config {
            target_timeslice: nz(10_000),
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(5), &config, ());
        let now = state.thread_logical_time.as_nanos();
        let history = ThreadHistory::new()
            .with_prio_changes(vec![(
                now + Duration::from_nanos(25_000),
                DEFAULT_PRIORITY + 1,
            )])
            .with_preemption_rcbs(vec![0]);
        state.preemption_points = Some(history.into_iter());

        assert_eq!(state.next_timeslice(&config), Some(DEFAULT_PRIORITY + 1));
        assert_eq!(state.replay_rcb_end, Some(0));
        assert_eq!(
            state.end_of_timeslice,
            Some(now + Duration::from_nanos(25_000))
        );

        // At the thread's time exactly, too.
        let mut state = ThreadState::new(DetPid::from_raw(6), &config, ());
        let now = state.thread_logical_time.as_nanos();
        let history = ThreadHistory::new()
            .with_prio_changes(vec![(now, DEFAULT_PRIORITY + 1)])
            .with_preemption_rcbs(vec![0]);
        state.preemption_points = Some(history.into_iter());
        assert_eq!(state.next_timeslice(&config), Some(DEFAULT_PRIORITY + 1));
        assert_eq!(state.replay_rcb_end, Some(0));
        assert_eq!(state.end_of_timeslice, Some(now));
    }

    /// The shape of a stalled Android emulator replay (`--chaos
    /// --preemption-timeout=1000`): a slice ended at the branch count of the
    /// next recorded end, whose time was still ahead, so that end was
    /// followed and its slice was over at once (adjacent zero-RCB slices);
    /// but the thread's time had not moved, so the second change point would
    /// have been filed at the instant of the first. It is caught before it
    /// is filed, and the thread runs on as its final slice, not already over.
    #[test]
    fn a_replay_change_point_at_the_last_ones_instant_is_caught_before_filing() {
        let config = Config {
            target_timeslice: nz(10_000),
            max_timeslice: nz(100_000),
            replay_preemptions_from: Some(std::path::PathBuf::from("history.json")),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(31744), &config, ());
        let start = state.thread_logical_time.as_nanos();
        let history = ThreadHistory::new()
            .with_prio_changes(vec![
                (start + Duration::from_nanos(70), DEFAULT_PRIORITY + 1),
                (start + Duration::from_nanos(80), DEFAULT_PRIORITY + 2),
                (start + Duration::from_nanos(500), DEFAULT_PRIORITY + 3),
            ])
            .with_preemption_rcbs(vec![4, 4, 40]);
        state.preemption_points = Some(history.into_iter());

        state.next_timeslice(&config);
        state.thread_logical_time.add_rcbs(4);
        state.committed_clock_value = 4;
        assert!(state.timeslice_expired());
        let now = state.thread_logical_time.as_nanos();
        assert_eq!(state.file_replay_change_point(&config, now), Ok(()));

        // The end at the same count, 10 ns later, is followed (its time is
        // ahead), and its slice is over at once; the thread's time is still
        // `now`.
        state.next_timeslice(&config);
        assert_eq!(state.replay_rcb_end, Some(4));
        assert!(state.timeslice_expired());
        assert_eq!(state.file_replay_change_point(&config, now), Err(now));

        state.abandon_replayed_slices(&config);
        assert!(!state.timeslice_expired());
        assert_eq!(state.replay_rcb_end, None);
        assert_eq!(state.end_of_timeslice, Some(LogicalTime::MAX));
        assert_eq!(
            state.max_timeslice_end,
            Some(now + Duration::from_nanos(100_000))
        );

        // A later change point files as usual; outside a preemption replay
        // nothing is checked.
        let later = now + Duration::from_nanos(1);
        assert_eq!(state.file_replay_change_point(&config, later), Ok(()));
        let recording = Config {
            replay_preemptions_from: None,
            ..config
        };
        assert_eq!(state.file_replay_change_point(&recording, later), Ok(()));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn replay_preserves_adjacent_zero_rcb_slices() {
        let config = Config {
            target_timeslice: nz(10_000),
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(5), &config, ());
        let now = state.thread_logical_time.as_nanos();
        let history = ThreadHistory::new()
            .with_prio_changes(vec![
                (now + Duration::from_nanos(70), DEFAULT_PRIORITY),
                (now + Duration::from_nanos(80), DEFAULT_PRIORITY),
            ])
            .with_preemption_rcbs(vec![4, 4]);
        state.preemption_points = Some(history.into_iter());

        state.next_timeslice(&config);
        state.thread_logical_time.add_rcbs(4);
        state.committed_clock_value = 4;
        assert!(state.timeslice_expired());

        state.next_timeslice(&config);
        assert_eq!(state.replay_rcb_end, Some(4));
        assert!(state.timeslice_expired());
    }

    /// `--chaos-instruction-offset=N` draws each timeslice's offset uniformly
    /// from `0..=N`, as a pure function of the seed, the thread and the draw.
    #[test]
    fn an_instruction_offset_draw_is_seeded_bounded_and_per_thread() {
        let (three, four) = (DetTid::from_raw(3), DetTid::from_raw(4));
        let mut seen = [false; 9];
        for draw in 0..200 {
            let k = chaos_instruction_offset_draw(7, three, draw, 8);
            assert_eq!(k, chaos_instruction_offset_draw(7, three, draw, 8));
            seen[usize::try_from(k).unwrap()] = true;
            assert!(chaos_instruction_offset_draw(7, three, draw, 1) <= 1);
            assert!(chaos_instruction_offset_draw(7, three, draw, 65_536) <= 65_536);
        }
        assert!(
            seen.iter().all(|seen| *seen),
            "0..=8 not all drawn: {seen:?}"
        );
        let stream = |seed, tid| -> Vec<u64> {
            (0..32)
                .map(|draw| chaos_instruction_offset_draw(seed, tid, draw, 1_000))
                .collect()
        };
        assert_ne!(stream(7, three), stream(7, four));
        assert_ne!(stream(7, three), stream(8, three));
    }

    /// The offsets come from a stream of their own: a seed draws the same
    /// timeslice ends with the option as without it, and without it draws no
    /// offset at all.
    #[test]
    fn an_instruction_offset_leaves_the_chaos_draws_alone() {
        let off = Config {
            chaos: true,
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let on = Config {
            chaos_instruction_offset: 8,
            ..off.clone()
        };
        let tid = DetTid::from_raw(3);
        let mut without = ThreadState::new(tid, &off, ());
        let mut with = ThreadState::new(tid, &on, ());
        let mut offsets = Vec::new();
        for _ in 0..20 {
            without.next_timeslice(&off);
            with.next_timeslice(&on);
            assert_eq!(with.end_of_timeslice, without.end_of_timeslice);
            assert_eq!(with.max_timeslice_end, without.max_timeslice_end);
            assert_eq!(without.slice_instruction_offset, 0);
            assert!(with.slice_instruction_offset <= 8);
            offsets.push(with.slice_instruction_offset);
        }
        assert_eq!(without.chaos_instruction_offset_draws, 0);
        assert_eq!(with.chaos_instruction_offset_draws, 20);
        let expected: Vec<u64> = (0..20)
            .map(|draw| chaos_instruction_offset_draw(off.sched_seed(), tid, draw, 8))
            .collect();
        assert_eq!(offsets, expected);
    }

    /// A new timeslice drops the offset the old timer was armed with (the
    /// hooks hand it to the change point that ends the slice), and a replayed
    /// timeslice takes the offset its row recorded, only together with that
    /// row's exact RCB end.
    #[test]
    fn a_timeslice_offset_reaches_its_record_and_its_replay() {
        let chaos = Config {
            chaos: true,
            chaos_instruction_offset: 8,
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(5), &chaos, ());
        state.next_timeslice(&chaos);
        let armed = InstructionOffset {
            rcb_target: 40,
            instructions: 3,
        };
        state.armed_instruction_offset = Some(armed);
        // A reset that ends no timeslice of its own drops the offset with
        // the timer, so no later change point can file it.
        state.next_timeslice(&chaos);
        assert_eq!(state.armed_instruction_offset, None);
        // The end of a slice takes the offset only once its branch target
        // is reached.
        state.armed_instruction_offset = Some(armed);
        state.committed_clock_value = 39;
        assert_eq!(state.take_ended_slice_offset(), None);
        assert_eq!(state.armed_instruction_offset, None);
        state.armed_instruction_offset = Some(armed);
        state.committed_clock_value = 40;
        assert_eq!(state.take_ended_slice_offset(), Some(armed));

        let replay = Config {
            target_timeslice: nz(10_000),
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(5), &replay, ());
        let now = state.thread_logical_time.as_nanos();
        let history = ThreadHistory::new()
            .with_prio_changes(vec![
                (now + Duration::from_nanos(70), DEFAULT_PRIORITY),
                (now + Duration::from_nanos(800), DEFAULT_PRIORITY),
            ])
            .with_preemption_rcbs(vec![42, 80])
            .with_preemption_instruction_offsets(vec![Some(armed), None]);
        state.preemption_points = Some(history.into_iter());
        state.next_timeslice(&replay);
        assert_eq!(state.replay_rcb_end, Some(42));
        assert_eq!(state.replay_instruction_offset, Some(armed));
        state.thread_logical_time.add_rcbs(42);
        state.committed_clock_value = 42;
        assert!(state.timeslice_expired());
        state.next_timeslice(&replay);
        assert_eq!(state.replay_rcb_end, Some(80));
        assert_eq!(state.replay_instruction_offset, None);

        // A row without an exact RCB end is replayed by logical time, which
        // an offset cannot refine.
        let mut state = ThreadState::new(DetPid::from_raw(5), &replay, ());
        state.preemption_points = Some(
            ThreadHistory::new()
                .with_prio_changes(vec![(now + Duration::from_nanos(70), DEFAULT_PRIORITY)])
                .with_preemption_instruction_offsets(vec![Some(armed)])
                .into_iter(),
        );
        state.next_timeslice(&replay);
        assert_eq!(state.replay_rcb_end, None);
        assert_eq!(state.replay_instruction_offset, None);
    }

    #[test]
    fn target_and_pmu_deadlines_are_independent() {
        let config = Config {
            target_timeslice: nz(20_000),
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        let now = state.thread_logical_time.as_nanos();

        state.next_timeslice(&config);

        assert_eq!(
            state.end_of_timeslice,
            Some(now + Duration::from_nanos(20_000))
        );
        assert_eq!(
            state.max_timeslice_end,
            Some(now + Duration::from_nanos(100_000))
        );
    }

    #[test]
    fn target_only_mode_does_not_create_a_pmu_deadline() {
        let config = Config {
            target_timeslice: nz(20_000),
            max_timeslice: None,
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        let now = state.thread_logical_time.as_nanos();

        state.next_timeslice(&config);

        assert_eq!(
            state.end_of_timeslice,
            Some(now + Duration::from_nanos(20_000))
        );
        assert_eq!(state.max_timeslice_end, None);
    }

    #[test]
    fn max_timeslice_caps_a_larger_target() {
        let config = Config {
            target_timeslice: nz(100_000),
            max_timeslice: nz(20_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        let now = state.thread_logical_time.as_nanos();

        state.next_timeslice(&config);

        let max_end = now + Duration::from_nanos(20_000);
        assert_eq!(state.end_of_timeslice, Some(max_end));
        assert_eq!(state.max_timeslice_end, Some(max_end));
    }

    #[test]
    fn chaos_without_target_caps_randomized_deadline_at_maximum() {
        let config = Config {
            chaos: true,
            target_timeslice: None,
            max_timeslice: nz(100_000),
            clock_multiplier: Some(1.05),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        let now = state.thread_logical_time.as_nanos();
        let configured_max = now + Duration::from_nanos(100_000);
        let minimum_progress = now + Duration::from_nanos(11);

        state.next_timeslice(&config);

        assert_eq!(state.max_timeslice_end, state.end_of_timeslice);
        assert!(state.max_timeslice_end.unwrap() <= configured_max);
        assert!(state.max_timeslice_end.unwrap() >= minimum_progress);
    }

    /// Under `--chaos-timeslice-range` every slice ends within one RCB and
    /// MAX of its start, and the slices span the range's scales.
    #[test]
    fn a_chaos_timeslice_range_draws_slices_of_every_scale_up_to_its_max() {
        let config = Config {
            chaos: true,
            max_timeslice: nz(100_000_000),
            chaos_timeslice_range: Some("1000..100000000".parse().unwrap()),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        let now = state.thread_logical_time.as_nanos();
        let lengths: Vec<u64> = (0..3000)
            .map(|_| {
                state.next_timeslice(&config);
                let end = state.end_of_timeslice.unwrap();
                assert_eq!(state.max_timeslice_end, Some(end));
                (end - now).as_nanos()
            })
            .collect();
        assert!(
            lengths
                .iter()
                .all(|length| (10..=100_000_000).contains(length)),
            "{:?}",
            lengths.iter().max()
        );
        assert!(lengths.iter().filter(|length| **length < 10_000).count() > 100);
        assert!(
            lengths
                .iter()
                .filter(|length| **length > 10_000_000)
                .count()
                > 100
        );
    }

    /// A schedule replay draws no slice mean from the range: its deadlines
    /// and generator state are those of the same replay without one.
    #[test]
    fn a_schedule_replay_draws_nothing_from_the_chaos_timeslice_range() {
        let plain = Config {
            chaos: true,
            max_timeslice: nz(100_000),
            replay_schedule_from: Some(std::path::PathBuf::from("schedule.json")),
            ..Default::default()
        };
        let ranged = Config {
            chaos_timeslice_range: Some("1000..100000".parse().unwrap()),
            ..plain.clone()
        };
        let mut without = ThreadState::new(DetPid::from_raw(1), &plain, ());
        let mut with = ThreadState::new(DetPid::from_raw(1), &ranged, ());
        for _ in 0..16 {
            without.next_timeslice(&plain);
            with.next_timeslice(&ranged);
            assert_eq!(with.end_of_timeslice, without.end_of_timeslice);
            assert_eq!(with.max_timeslice_end, without.max_timeslice_end);
        }
        assert!(with.chaos_prng == without.chaos_prng);
    }

    /// Once its process has switched to the post-crash bound, a thread draws
    /// no slice mean from `--chaos-timeslice-range`: its deadlines and
    /// generator state are those of the same switched thread without a range,
    /// drawn around the post-crash bound, where before the range replaced
    /// that mean with a fresh fine-grained draw at every slice. Before the
    /// switch the range draws as usual. A schedule replay of such a run
    /// draws nothing either way (`replay_controls_deadline`).
    #[test]
    fn a_switched_thread_draws_no_slice_mean_from_the_chaos_timeslice_range() {
        let plain = Config {
            chaos: true,
            max_timeslice: nz(2_000),
            post_crash_timeslice: nz(200_000_000),
            ..Default::default()
        };
        let ranged = Config {
            chaos_timeslice_range: Some("1000..2000".parse().unwrap()),
            ..plain.clone()
        };
        let mut without = ThreadState::new(DetPid::from_raw(1), &plain, ());
        let mut with = ThreadState::new(DetPid::from_raw(1), &ranged, ());
        without.next_timeslice(&plain);
        with.next_timeslice(&ranged);
        assert!(
            with.chaos_prng != without.chaos_prng,
            "before the switch the range draws"
        );
        let mut without = ThreadState::new(DetPid::from_raw(1), &plain, ());
        let mut with = ThreadState::new(DetPid::from_raw(1), &ranged, ());
        let bound = std::num::NonZeroU64::new(200_000_000);
        without.post_crash_bound = bound;
        with.post_crash_bound = bound;
        let now = with.thread_logical_time.as_nanos();
        let mut longest = 0;
        for _ in 0..16 {
            without.next_timeslice(&plain);
            with.next_timeslice(&ranged);
            assert_eq!(with.end_of_timeslice, without.end_of_timeslice);
            assert_eq!(with.max_timeslice_end, without.max_timeslice_end);
            longest = longest.max((with.end_of_timeslice.unwrap() - now).as_nanos());
        }
        assert!(with.chaos_prng == without.chaos_prng);
        assert!(
            longest > 2_000,
            "slices are drawn around the post-crash bound"
        );
    }

    /// A schedule replay under `--chaos` draws no slice end: the slice runs to
    /// the backstop, `--max-timeslice` from now, unless the replayed schedule
    /// ends it first, exactly as without `--chaos`, and the thread's chaos
    /// generator is left as it was.
    #[test]
    fn chaos_schedule_replay_draws_no_slice_end() {
        let replay = |chaos| Config {
            chaos,
            max_timeslice: nz(100_000),
            replay_schedule_from: Some(std::path::PathBuf::from("schedule.json")),
            ..Default::default()
        };
        let mut plain = ThreadState::new(DetPid::from_raw(1), &replay(false), ());
        let mut chaos = ThreadState::new(DetPid::from_raw(1), &replay(true), ());
        let generator = chaos.chaos_prng.clone();

        assert_eq!(plain.next_timeslice(&replay(false)), None);
        assert_eq!(chaos.next_timeslice(&replay(true)), None);

        let backstop = chaos.thread_logical_time.as_nanos() + Duration::from_nanos(100_000);
        assert_eq!(chaos.end_of_timeslice, Some(backstop));
        assert_eq!(chaos.end_of_timeslice, plain.end_of_timeslice);
        assert_eq!(chaos.max_timeslice_end, plain.max_timeslice_end);
        assert_eq!(chaos.chaos_prng, generator);
    }

    /// Disabling the PMU maximum does not disable the cooperative target or
    /// forbid its schedule replay. Replayed checkpoints own its slice ends;
    /// after EOF the same configured target owns ordinary slices again.
    #[test]
    fn target_only_chaos_schedule_replay_keeps_recorded_boundaries_without_a_pmu_maximum() {
        for (replay, no_rcb_time) in [(false, true), (true, false), (true, true)] {
            let mut config = Config {
                chaos: true,
                no_rcb_time,
                max_timeslice: None,
                target_timeslice: nz(100_000),
                replay_schedule_from: replay.then(|| "schedule.json".into()),
                ..Default::default()
            };
            config.validate();
            assert_eq!(config.max_timeslice, None);
            assert_eq!(config.target_timeslice, nz(100_000));
            let mut state = ThreadState::new(DetPid::from_raw(3), &config, ());
            let generator = state.chaos_prng.clone();
            assert_eq!(state.next_timeslice(&config), None);
            assert_eq!(state.max_timeslice_end, None);
            if replay {
                assert_eq!(state.end_of_timeslice, Some(LogicalTime::MAX));
                assert_eq!(state.chaos_prng, generator);
                state.schedule_replay_over = true;
                state.next_timeslice(&config);
                assert_eq!(
                    state.end_of_timeslice,
                    Some(state.thread_logical_time.as_nanos() + Duration::from_nanos(100_000))
                );
                assert_eq!(state.max_timeslice_end, None);
                assert_eq!(state.chaos_prng, generator);
            } else {
                assert!(state.end_of_timeslice > Some(state.thread_logical_time.as_nanos()));
            }
        }
    }

    /// At a checkpoint turn that keeps the slice, a schedule replay counts its
    /// backstop from now, as the recorded run's new slice did, unless another
    /// slice end, such as the replayed schedule's deadline, replaced it.
    #[test]
    fn a_kept_replay_slice_restarts_only_the_backstop() {
        let config = Config {
            max_timeslice: nz(100_000),
            replay_schedule_from: Some(std::path::PathBuf::from("schedule.json")),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        state.next_timeslice(&config);
        state.thread_logical_time.add_rcbs(1000);
        let now = state.thread_logical_time.as_nanos();
        assert!(state.end_of_timeslice < Some(now + Duration::from_nanos(100_000)));

        state.restart_replay_backstop(&config);
        let backstop = Some(now + Duration::from_nanos(100_000));
        assert_eq!(state.end_of_timeslice, backstop);
        assert_eq!(state.max_timeslice_end, backstop);

        let scheduled = now + Duration::from_nanos(10);
        state.install_schedule_deadline(scheduled, true);
        state.restart_replay_backstop(&config);
        assert_eq!(state.end_of_timeslice, Some(scheduled));
        assert_eq!(state.max_timeslice_end, Some(scheduled));

        state.next_timeslice(&config);
        state.thread_logical_time.add_rcbs(1000);
        state.restart_replay_backstop(&config);
        assert_eq!(
            state.end_of_timeslice,
            Some(state.thread_logical_time.as_nanos() + Duration::from_nanos(100_000))
        );
    }

    /// A slice end the replayed schedule installs stays one even on the
    /// backstop's nanosecond. A recorded slice end can fall there by chance
    /// (a chaos draw of 9 branches that ended 90 ns after a checkpoint, where
    /// the kept slice's backstop also ran out); restarting the backstop there
    /// moved the schedule's slice end 1000 ns later.
    #[test]
    fn a_schedule_deadline_on_the_backstop_keeps_its_place() {
        let config = Config {
            max_timeslice: nz(1_000),
            replay_schedule_from: Some(std::path::PathBuf::from("schedule.json")),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        state.next_timeslice(&config);
        let backstop = state.replay_backstop.expect("a schedule replay's backstop");
        state.thread_logical_time.add_rcbs(91);
        state.install_schedule_deadline(backstop, true);

        state.restart_replay_backstop(&config);
        assert_eq!(state.end_of_timeslice, Some(backstop));
        assert_eq!(state.max_timeslice_end, Some(backstop));
        assert_eq!(state.replay_backstop, None);
    }

    #[test]
    fn schedule_replay_without_rcb_time_arms_pmu_maximum() {
        let config = Config {
            no_rcb_time: true,
            max_timeslice: nz(100_000),
            replay_schedule_from: Some(std::path::PathBuf::from("schedule.json")),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        let now = state.thread_logical_time.as_nanos();

        state.next_timeslice(&config);

        let expected = now + Duration::from_nanos(100_000);
        assert_eq!(state.end_of_timeslice, Some(expected));
        assert_eq!(state.max_timeslice_end, Some(expected));
    }

    #[test]
    fn exhausted_preemption_replay_uses_bounded_pmu_maximum() {
        let config = Config {
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(3), &config, ());
        state.preemption_points = Some(ThreadHistory::new().into_iter());
        let now = state.thread_logical_time.as_nanos();

        state.next_timeslice(&config);

        let bounded_end = Some(now + Duration::from_nanos(100_000));
        assert_eq!(state.end_of_timeslice, bounded_end);
        assert_eq!(state.max_timeslice_end, bounded_end);
    }

    /// A preemption replay keeps a recorded slice end uncapped. Once the
    /// recording runs out its slice end is `LogicalTime::MAX`, which the cap
    /// may lower.
    #[test]
    fn preemption_replay_caps_only_past_its_recording() {
        let config = Config {
            max_timeslice: nz(100_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(3), &config, ());
        let now = state.thread_logical_time.as_nanos();
        let recorded_end = now + Duration::from_nanos(50_000);
        let history = ThreadHistory::new()
            .with_prio_changes(vec![(recorded_end, DEFAULT_PRIORITY)])
            .with_preemption_rcbs(vec![5_000]);
        state.preemption_points = Some(history.into_iter());

        state.next_timeslice(&config);
        assert_eq!(state.end_of_timeslice, Some(recorded_end));
        state.cap_timeslice_at_deadline(Duration::from_nanos(1_000), 1.0, Duration::ZERO, false);
        assert_eq!(state.end_of_timeslice, Some(recorded_end));

        let mut exhausted = ThreadState::new(DetPid::from_raw(3), &config, ());
        exhausted.preemption_points = Some(ThreadHistory::new().into_iter());
        exhausted.next_timeslice(&config);
        exhausted.cap_timeslice_at_deadline(
            Duration::from_nanos(1_000),
            1.0,
            Duration::ZERO,
            false,
        );
        let capped = Some(now + Duration::from_nanos(1_000));
        assert_eq!(exhausted.end_of_timeslice, capped);
        assert_eq!(
            exhausted.max_timeslice_end,
            Some(now + Duration::from_nanos(100_000))
        );
    }

    #[test]
    fn timeslice_expiry_is_inclusive() {
        let config = Config::default();
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        let now = state.thread_logical_time.as_nanos();

        state.end_of_timeslice = Some(now + Duration::from_nanos(1));
        assert!(!state.timeslice_expired());
        state.end_of_timeslice = Some(now);
        assert!(state.timeslice_expired());
    }

    // x86_64 only: instruction traps (CPUID, RDTSC) exist only there.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn instruction_trap_ends_an_expired_target_only_at_the_pmu_maximum() {
        let config = Config {
            target_timeslice: nz(1_000),
            target_timeslice_syscalls_only: true,
            max_timeslice: nz(20_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        state.next_timeslice(&config);

        // Past the target: a syscall would end the slice, an rdtsc must not.
        state.thread_logical_time.add_rcbs(500);
        assert!(state.timeslice_expired());
        assert!(!state.instruction_trap_ends_timeslice(&config));

        // At the PMU maximum the trap ends it, as a branch would.
        state.thread_logical_time.add_rcbs(1_500);
        assert!(state.instruction_trap_ends_timeslice(&config));
    }

    // x86_64 only: instruction traps (CPUID, RDTSC) exist only there.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn instruction_trap_ends_an_expired_target_by_default() {
        let config = Config {
            target_timeslice: nz(1_000),
            max_timeslice: nz(20_000),
            ..Default::default()
        };
        assert!(!config.target_timeslice_syscalls_only);
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        state.next_timeslice(&config);
        state.thread_logical_time.add_rcbs(99);
        assert!(!state.instruction_trap_ends_timeslice(&config));
        state.thread_logical_time.add_rcbs(1);
        assert!(state.instruction_trap_ends_timeslice(&config));
    }

    // x86_64 only: instruction traps (CPUID, RDTSC) exist only there.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn instruction_trap_ends_an_expired_target_without_a_pmu_maximum() {
        // With PMU preemption disabled nothing else can break a busy-wait on
        // the TSC, so the trap keeps ending the expired target.
        let config = Config {
            target_timeslice: nz(1_000),
            target_timeslice_syscalls_only: true,
            max_timeslice: None,
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &config, ());
        state.next_timeslice(&config);
        assert_eq!(state.max_timeslice_end, None);

        // 1,000 ns is 100 RCBs at the default 10 ns per RCB.
        state.thread_logical_time.add_rcbs(99);
        assert!(!state.instruction_trap_ends_timeslice(&config));
        state.thread_logical_time.add_rcbs(1);
        assert!(state.instruction_trap_ends_timeslice(&config));
    }

    // x86_64 only: instruction traps (CPUID, RDTSC) exist only there.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn instruction_trap_keeps_replayed_and_untargeted_boundaries() {
        // Without a target the slice end is the PMU maximum itself, and a
        // replayed preemption record keeps its recorded boundaries: in both
        // cases a trap ends exactly the slices it ended before.
        let untargeted = Config {
            max_timeslice: nz(20_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &untargeted, ());
        state.next_timeslice(&untargeted);
        state.thread_logical_time.add_rcbs(1_999);
        assert_eq!(
            state.instruction_trap_ends_timeslice(&untargeted),
            state.timeslice_expired()
        );
        state.thread_logical_time.add_rcbs(1);
        assert!(state.instruction_trap_ends_timeslice(&untargeted));

        let targeted = Config {
            target_timeslice: nz(1_000),
            target_timeslice_syscalls_only: true,
            max_timeslice: nz(20_000),
            ..Default::default()
        };
        let mut state = ThreadState::new(DetPid::from_raw(1), &targeted, ());
        state.next_timeslice(&targeted);
        state.preemption_points = Some(ThreadHistory::new().into_iter());
        state.thread_logical_time.add_rcbs(500);
        assert!(state.timeslice_expired());
        assert!(state.instruction_trap_ends_timeslice(&targeted));
    }

    #[test]
    fn child_rng_distinguishes_adjacent_thread_ids() {
        let parent = Pcg64Mcg::seed_from_u64(0);
        let mut even = thread_rng_from_parent("test", &parent, DetTid::from_raw(8));
        let mut odd = thread_rng_from_parent("test", &parent, DetTid::from_raw(9));

        let even_values: [u64; 4] = std::array::from_fn(|_| even.next_u64());
        let odd_values: [u64; 4] = std::array::from_fn(|_| odd.next_u64());
        assert_ne!(even_values, odd_values);
    }

    #[test]
    fn child_rng_uses_high_entropy_bits() {
        let parent = Pcg64Mcg::seed_from_u64(0);
        let mut low = thread_rng_from_parent_entropy("test", &parent, 1);
        let mut high = thread_rng_from_parent_entropy("test", &parent, (1_u128 << 64) | 1);

        let low_values: [u64; 4] = std::array::from_fn(|_| low.next_u64());
        let high_values: [u64; 4] = std::array::from_fn(|_| high.next_u64());
        assert_ne!(low_values, high_values);
    }

    #[test]
    fn child_rng_pedigree_uses_the_full_unbounded_path() {
        let parent_rng = Pcg64Mcg::seed_from_u64(0);
        let mut long_path = Pedigree::new();
        for index in 0..2_048 {
            let (parent, child) = long_path.fork();
            long_path = if index % 2 == 0 { child } else { parent };
        }
        let (left, right) = long_path.fork();
        let mut left_rng =
            thread_rng_from_parent_pedigree("test", &parent_rng, &left, ChildRngStream::User);
        let mut right_rng =
            thread_rng_from_parent_pedigree("test", &parent_rng, &right, ChildRngStream::User);

        let left_values: [u64; 4] = std::array::from_fn(|_| left_rng.next_u64());
        let right_values: [u64; 4] = std::array::from_fn(|_| right_rng.next_u64());
        assert_ne!(left_values, right_values);
    }

    #[test]
    fn child_rng_pedigree_separates_user_and_chaos_streams() {
        let parent_rng = Pcg64Mcg::seed_from_u64(0);
        let (_, child) = Pedigree::new().fork();
        let mut user_rng = thread_rng_from_parent_pedigree(
            "same logging label",
            &parent_rng,
            &child,
            ChildRngStream::User,
        );
        let mut chaos_rng = thread_rng_from_parent_pedigree(
            "same logging label",
            &parent_rng,
            &child,
            ChildRngStream::Chaos,
        );

        let user_values: [u64; 4] = std::array::from_fn(|_| user_rng.next_u64());
        let chaos_values: [u64; 4] = std::array::from_fn(|_| chaos_rng.next_u64());
        assert_ne!(user_values, chaos_values);
    }
}

/// Generate a new thread-local PRNG from the parent's PRNG state, mixing in the
/// new DetTid for some deterministic entropy. This ensures sequentially-spawned
/// threads get distinct PRNG states.
// TODO-HUMAN-REVIEW(PR-1052): Review collision-free child-thread PRNG seeding.
pub fn thread_rng_from_parent(msg: &str, parent: &Pcg64Mcg, child: DetTid) -> Pcg64Mcg {
    thread_rng_from_parent_entropy_labeled(msg, parent, child.as_raw() as u32 as u128, "tid")
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ChildRngStream {
    User,
    Chaos,
}

impl ChildRngStream {
    fn domain(self) -> &'static [u8] {
        match self {
            Self::User => b"user",
            Self::Chaos => b"chaos",
        }
    }
}

// TODO-HUMAN-REVIEW(PR-1052): Review pedigree-derived child-thread PRNG seeding.
pub(crate) fn thread_rng_from_parent_pedigree(
    msg: &str,
    parent: &Pcg64Mcg,
    child: &Pedigree,
    stream: ChildRngStream,
) -> Pcg64Mcg {
    let bits = child.raw();
    let mut packed = Vec::with_capacity(bits.len().div_ceil(8));
    let mut byte = 0_u8;
    for (index, bit) in bits.iter().enumerate() {
        if *bit {
            byte |= 1 << (index % 8);
        }
        if index % 8 == 7 {
            packed.push(byte);
            byte = 0;
        }
    }
    if !bits.len().is_multiple_of(8) {
        packed.push(byte);
    }

    // Pedigrees are unbounded, while Pcg64Mcg has a 128-bit state. Hash the
    // complete, length-delimited path instead of using the fallible compressed
    // virtual-PID encoding, so child RNG identity has no artificial tree-depth
    // boundary. The domain string prevents this digest from being confused
    // with another future use of the same pedigree serialization. Pcg64Mcg
    // forces its low state bit, so this retains 127 effective digest bits.
    let mut hasher = Sha256::new();
    hasher.update(b"hermit-child-rng-pedigree-v1\0");
    hasher.update(stream.domain());
    hasher.update([0]);
    hasher.update((bits.len() as u64).to_le_bytes());
    hasher.update(&packed);
    let digest = hasher.finalize();

    let mut seed = <Pcg64Mcg as SeedableRng>::Seed::default();
    parent.clone().fill_bytes(seed.as_mut());
    for (seed_byte, pedigree_byte) in seed.iter_mut().zip(digest) {
        *seed_byte ^= pedigree_byte;
    }
    detlog!(
        "RNG {} seeding child {:?} pedigree {}: {:?} from parent {:?}",
        msg,
        stream,
        child,
        seed,
        parent
    );
    let mut rng = Pcg64Mcg::from_seed(seed);
    rng.next_u64();
    rng.next_u64();
    rng.next_u64();
    rng.next_u64();
    rng
}

#[cfg(test)]
mod native_exec_cut_tests {
    use super::*;
    use crate::native_exec::NativeExecCutError;
    use crate::native_exec::capture_prepared_native_exec;
    use crate::native_exec::tests::fixture;
    use crate::native_exec::tests::prepare;
    use crate::native_exec::tests::state_bytes;
    use crate::native_exec::validate_old_native_exec_cut;

    #[test]
    fn native_exec_child_publications_refuse_every_runtime_token_state() {
        for completion in [None, Some(true), Some(false)] {
            let (tool, thread, binding) = fixture();
            let before = state_bytes(&thread);
            let child = DetPid::from_raw(43);
            let token = thread.prepare_child_cpu_publication(child);
            if let Some(published) = completion {
                token.finish(published);
            }
            // The map is genuinely invisible to serde in all three states.
            assert_eq!(state_bytes(&thread), before);
            assert!(matches!(
                validate_old_native_exec_cut(&tool, &thread, &binding),
                Err(NativeExecCutError::LiveCapability("child_cpu_publications"))
            ));
            assert!(Arc::ptr_eq(
                thread
                    .process_cpu_time
                    .lock()
                    .unwrap()
                    .child_cpu_publications
                    .get(&child)
                    .unwrap(),
                &token
            ));
        }
    }

    #[test]
    fn native_exec_historical_cpu_accounting_survives_without_live_publications() {
        let (tool, mut thread, binding) = fixture();
        {
            let mut cpu = thread.process_cpu_time.lock().unwrap();
            cpu.add_thread_delta(LogicalTime::from_nanos(17), LogicalTime::from_nanos(31));
            cpu.record_exited_child(
                DetPid::from_raw(43),
                ProcessCpuSnapshot {
                    user: LogicalTime::from_nanos(19),
                    system: LogicalTime::from_nanos(23),
                    children_user: LogicalTime::from_nanos(29),
                    children_system: LogicalTime::from_nanos(37),
                },
            );
            cpu.reap_child(DetPid::from_raw(43));
            cpu.record_exited_child(
                DetPid::from_raw(44),
                ProcessCpuSnapshot {
                    user: LogicalTime::from_nanos(41),
                    system: LogicalTime::from_nanos(47),
                    ..ProcessCpuSnapshot::default()
                },
            );
        }
        let before = serde_json::to_value(&*thread.process_cpu_time.lock().unwrap()).unwrap();
        let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
        prepare(&mut thread, &binding);
        let bytes = bincode::serde::encode_to_vec(
            capture_prepared_native_exec(&tool, &thread, &binding, &receipt).unwrap(),
            bincode::config::legacy(),
        )
        .unwrap();
        let (payload, used): (crate::native_exec::DetcoreExecSnapshotV2, _) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::legacy()).unwrap();
        assert_eq!(used, bytes.len());
        let (_, restored) = payload.into_validated_parts(&binding, &tool.cfg).unwrap();
        assert_eq!(
            serde_json::to_value(&*restored.process_cpu_time.lock().unwrap()).unwrap(),
            before
        );
        let cpu = restored.process_cpu_time.lock().unwrap();
        assert_eq!(cpu.snapshot.children_user, LogicalTime::from_nanos(48));
        assert_eq!(cpu.snapshot.children_system, LogicalTime::from_nanos(60));
        assert_eq!(cpu.exited_children.len(), 1);
        assert!(cpu.child_cpu_publications.is_empty());
    }

    #[test]
    fn native_exec_old_accept_waits_and_invalid_table_slots_refuse() {
        let (tool, thread, binding) = fixture();
        let operation = {
            let mut table = thread.file_metadata.lock().unwrap();
            table
                .add_fd(binding.dettid, 33, OFlag::O_CLOEXEC, FdType::Socket, None)
                .unwrap();
            table.add_accept_wait(33)
        };
        assert!(matches!(
            validate_old_native_exec_cut(&tool, &thread, &binding),
            Err(NativeExecCutError::LiveCapability("accept_waits"))
        ));
        let mut table = thread.file_metadata.lock().unwrap();
        assert!(table.for_exec(binding.dettid).accept_waits.is_empty());
        assert_eq!(table.accept_waits, [(operation, 33)]);
        table.remove_accept_wait(operation);
        // An old operation counter is historical, but the existing prepared
        // table has a fresh counter. Neither validation path resets it.
        assert_eq!(table.next_accept_wait, operation + 1);
        table
            .validate_native_exec_cut(binding.old_files, false)
            .unwrap();
        let mut prepared = table.for_exec(binding.dettid);
        prepared
            .validate_native_exec_cut(binding.prepared_files(), true)
            .unwrap();
        prepared.next_accept_wait = 1;
        assert_eq!(
            prepared.validate_native_exec_cut(binding.prepared_files(), true),
            Err(NativeExecCutError::PreparedState("next_accept_wait"))
        );
        let only = table.file_handles.remove(&33).unwrap();
        table.file_handles.insert(34, only);
        assert_eq!(
            table.validate_native_exec_cut(binding.old_files, false),
            Err(NativeExecCutError::Descriptor("slot number"))
        );
        table.file_handles.clear();
        table.file_handles.insert(
            -1,
            DetFd::new(
                -1,
                OFlag::empty(),
                FdType::Rng,
                OpenFileId::new(binding.dettid, 1),
            ),
        );
        assert_eq!(
            table.validate_native_exec_cut(binding.old_files, false),
            Err(NativeExecCutError::Descriptor("slot number"))
        );
    }

    #[test]
    fn native_exec_each_skipped_or_live_thread_transient_refuses() {
        for field in 0..26 {
            let (tool, mut thread, binding) = fixture();
            match field {
                0 => {
                    thread.parent_process_cpu_time =
                        Some(Arc::new(Mutex::new(ProcessCpuTime::default())))
                }
                1 => thread.parent_cpu_publication = Some(ChildCpuPublication::pending()),
                2 => {
                    thread.restart_block = Some(crate::syscalls::helpers::RestartBlock {
                        rip: 0x1234,
                        deadline: None,
                        // Any call with a restart block will do; aarch64 has
                        // no poll.
                        #[cfg(target_arch = "x86_64")]
                        call: crate::syscalls::helpers::RestartCall::Poll(
                            reverie::syscalls::Poll::new(),
                        ),
                        #[cfg(not(target_arch = "x86_64"))]
                        call: crate::syscalls::helpers::RestartCall::Futex(
                            reverie::syscalls::Futex::new(),
                        ),
                    })
                }
                3 => thread.signal_register_projection = Some(unsafe { std::mem::zeroed() }),
                4 => thread.clone_flags = Some(CloneFlags::empty()),
                5 => {
                    thread.pending_vfork = Some(PendingVfork {
                        parent_dettid: binding.dettid,
                        op: ExternalOpId::new(binding.dettid, 0),
                        parent_detpid: binding.detpid,
                        child_tid_addr: 0,
                        flags: CloneFlags::CLONE_VFORK,
                        exit_signal: libc::SIGCHLD,
                        child_priority_entropy: None,
                        user_namespace_owner_may_differ: false,
                    })
                }
                6 => thread.own_signal_turn_hold = Some(libc::SIGALRM),
                7 => thread.sigalrm_handled = true,
                8 => thread.handler_restore_rt_reported = 1,
                9 => {
                    thread.initialized_random_auxv = Some((
                        crate::random::InitialImage {
                            pid: binding.physical_tid,
                            start_time_ticks: 1,
                            at_random: 0x234000,
                        },
                        [7; 16],
                        Vec::new(),
                    ))
                }
                10 => thread.uncharged_bootstrap_syscalls = 1,
                11 => thread.in_uncharged_bootstrap_syscall = true,
                12 => thread.yield_after_futex_wake = true,
                13 => {
                    thread.interrupt_at.insert(1);
                }
                14 => thread.replay_rcb_end = Some(1),
                15 => thread.last_rcb_timer = Some(1),
                16 => thread.last_rcb_timer_is_max = true,
                17 => thread.max_timeslice_end = Some(LogicalTime::from_nanos(1)),
                18 => thread.chaos_slowdown_active = true,
                19 => thread.nested_pid_namespace = true,
                20 => thread.thread_start_entered = false,
                21 => thread.past_global_first_execve = false,
                22 => {
                    thread.last_accounted_system_time =
                        thread.thread_logical_time.system_cpu_time() + Duration::from_nanos(1)
                }
                23 => {
                    thread.preemption_points =
                        Some(crate::preemptions::ThreadHistory::new().into_iter())
                }
                24 => thread.pending_chaos_epochs.push(ChaosEpochTransition {
                    logical_time: LogicalTime::from_nanos(1),
                    epoch: 1,
                    factor: RcbTimeMultiplier::from_f64(1.5),
                }),
                25 => {
                    thread.signal_task_identity = Some(reverie::SignalTaskIdentity {
                        process: reverie::SignalProcessId {
                            tgid: reverie::Pid::from_raw(binding.physical_tid),
                            generation: 0,
                        },
                        tid: reverie::Pid::from_raw(binding.physical_tid),
                        task_generation: 0,
                    })
                }
                _ => unreachable!(),
            }
            assert!(
                validate_old_native_exec_cut(&tool, &thread, &binding).is_err(),
                "field {field}"
            );
        }
    }

    #[test]
    fn native_exec_robust_and_prepared_memory_must_match_the_actual_cut() {
        for field in 0..4 {
            let (tool, mut thread, binding) = fixture();
            thread.record_robust_list_head(Some(0x1000));
            {
                let mut robust = thread.robust_list_process.lock().unwrap();
                match field {
                    0 => {
                        robust.heads.insert(DetTid::from_raw(43), 0x2000);
                    }
                    1 => {
                        robust.pending.insert(
                            binding.dettid,
                            PendingRobustListWakes {
                                reason: RobustListExit::ExitGroup,
                                wakes: Vec::new(),
                            },
                        );
                    }
                    2 => {
                        robust
                            .matched_physical_exits
                            .insert(binding.dettid, thread.thread_logical_time.clone());
                    }
                    3 => robust.exec_pending.push((
                        binding.dettid,
                        RobustListWake {
                            futex: FutexID::private(binding.old_mm, 0x3000),
                        },
                    )),
                    _ => unreachable!(),
                }
            }
            assert!(
                validate_old_native_exec_cut(&tool, &thread, &binding).is_err(),
                "old field {field}"
            );
        }
        for field in 0..8 {
            let (tool, mut thread, binding) = fixture();
            let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
            prepare(&mut thread, &binding);
            match field {
                0 => thread.memory_metadata.lock().unwrap().observe_brk(0x4000),
                1 => thread.memory_metadata.lock().unwrap().map_anonymous(
                    binding.prepared_mm(),
                    0x5000,
                    0x1000,
                ),
                2 => thread.record_robust_list_head(Some(0x1000)),
                3 => thread.stage_robust_list_wakes_for_exec(vec![(
                    binding.dettid,
                    vec![RobustListWake {
                        futex: FutexID::private(binding.prepared_mm(), 0x3000),
                    }],
                )]),
                4 => thread.stage_robust_list_wakes_for_exec(vec![(
                    DetTid::from_raw(43),
                    vec![RobustListWake {
                        futex: FutexID::private(binding.old_mm, 0x3000),
                    }],
                )]),
                5 => thread.file_metadata.lock().unwrap().files_id = binding.old_files,
                6 => {
                    thread.file_metadata.lock().unwrap().file_handles.insert(
                        3,
                        DetFd::new(
                            3,
                            OFlag::O_CLOEXEC,
                            FdType::Rng,
                            OpenFileId::new(binding.dettid, 1),
                        ),
                    );
                }
                7 => thread.mm_id = binding.old_mm,
                _ => unreachable!(),
            }
            assert!(
                capture_prepared_native_exec(&tool, &thread, &binding, &receipt).is_err(),
                "prepared field {field}"
            );
        }
    }

    #[test]
    fn native_exec_model_lock_contention_is_a_refusal_without_mutation() {
        let (tool, thread, binding) = fixture();
        let before = state_bytes(&thread);
        macro_rules! locked {
            ($field:ident) => {{
                let guard = thread.$field.lock().unwrap();
                assert!(matches!(
                    validate_old_native_exec_cut(&tool, &thread, &binding),
                    Err(NativeExecCutError::LockUnavailable(_))
                ));
                drop(guard);
                assert_eq!(state_bytes(&thread), before);
            }};
        }
        locked!(file_metadata);
        locked!(process_cpu_time);
        locked!(guest_clock);
        locked!(resource_limits);
        locked!(posix_timers);
        locked!(robust_list_process);
        validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
    }
}

fn thread_rng_from_parent_entropy(msg: &str, parent: &Pcg64Mcg, entropy: u128) -> Pcg64Mcg {
    thread_rng_from_parent_entropy_labeled(msg, parent, entropy, "entropy")
}

fn thread_rng_from_parent_entropy_labeled(
    msg: &str,
    parent: &Pcg64Mcg,
    entropy: u128,
    identity_kind: &str,
) -> Pcg64Mcg {
    // Perform the default SeedableRng::from_seed procedure
    let mut seed = <Pcg64Mcg as SeedableRng>::Seed::default();
    // Generate a seed from the parent:
    parent.clone().fill_bytes(seed.as_mut());
    detlog!("RNG {} Generated new seed {:?}", msg, seed);
    // Pcg64Mcg forces its internal state odd, so seed bit zero carries no
    // entropy. DBT uses 96 bits for a stable process/thread sequence; mix those
    // bytes after the forced bit while retaining the existing DetTid layout.
    let entropy_bytes = entropy.to_le_bytes();
    for (seed_byte, entropy_byte) in seed[4..].iter_mut().zip(entropy_bytes) {
        *seed_byte ^= entropy_byte;
    }
    detlog!(
        "RNG {} seeding child {} {}: {:?} from parent {:?}",
        msg,
        identity_kind,
        entropy,
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

/// The procfs path that descriptor `fd` of the calling process is a
/// placeholder for, which this image of the process has not seen: a memfd
/// named for it and sealed as Detcore seals its placeholders, which a guest
/// could imitate (a known limitation, see
/// <https://github.com/rrnewton/hermit/issues/4263>). Read with raw
/// system calls (under in-guest backends the tool shares the guest's C
/// library).
/// What this thread's descriptor `fd` links to (`/proc/thread-self/fd/<fd>`),
/// read with a raw `readlinkat` as the in-guest runtime must.
fn own_fd_link(fd: RawFd) -> Option<Vec<u8>> {
    // The calling thread's own table: `/proc/self/fd` is the leader's.
    let link = std::ffi::CString::new(format!("/proc/thread-self/fd/{fd}")).ok()?;
    let mut target = [0_u8; 256];
    // SAFETY: the path is NUL-terminated and `target` is writable.
    let length = unsafe {
        crate::util::raw_syscall(
            libc::SYS_readlinkat,
            [
                libc::AT_FDCWD as u64,
                link.as_ptr() as u64,
                target.as_mut_ptr() as u64,
                target.len() as u64,
                0,
                0,
            ],
        )
    };
    let length = usize::try_from(length)
        .ok()
        .filter(|length| *length < target.len())?;
    Some(target[..length].to_vec())
}

/// The path a descriptor's link `link` names where it is a per-process file
/// that follows its record's sequence as the kernel opens one, and the
/// descriptor's file (`raw_stat`) is an empty regular file with a link, as
/// every procfs file is (a placeholder has none): a task's `stat`, `statm`,
/// `io`, `arch_status` or `cgroup` (`ProcfsFile::follows_record_sequence`),
/// not its `comm`, which Detcore leaves to the kernel's own file whoever
/// opened it, so the kernel's one open file serves every holder. ASSUMED,
/// as for `fixed_procfs_behind`: a link under `/proc` naming such a file is
/// the procfs file, not an ordinary file a changed root put at that path.
pub(crate) fn kernel_record_file_link(
    raw_stat: &libc::stat,
    link: &[u8],
) -> Option<std::path::PathBuf> {
    if raw_stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || raw_stat.st_size != 0
        || raw_stat.st_nlink == 0
    {
        return None;
    }
    let path =
        std::path::Path::new(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(link));
    crate::procfs::ProcfsFile::from_path(path)
        .filter(|file| file.follows_record_sequence() && !file.is_task_comm())
        .map(|_| path.to_path_buf())
}

/// The fixed procfs file the kernel opened at `fd`, if its descriptor link
/// names one (`ProcfsFile::snapshot_is_fixed`).
fn fixed_procfs_behind(fd: RawFd) -> Option<crate::procfs::ProcfsFile> {
    let target = own_fd_link(fd)?;
    let path = std::path::Path::new(
        <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&target),
    );
    crate::procfs::ProcfsFile::from_path(path).filter(crate::procfs::ProcfsFile::snapshot_is_fixed)
}

fn placeholder_behind(
    fd: RawFd,
    files: crate::procfs::RenderedFiles,
) -> Option<(&'static std::path::Path, Vec<u8>)> {
    // SAFETY: F_GET_SEALS only reads the descriptor's seals.
    let seals = unsafe {
        crate::util::raw_syscall(
            libc::SYS_fcntl,
            [fd as u64, libc::F_GET_SEALS as u64, 0, 0, 0, 0],
        )
    };
    // An `O_PATH` descriptor cannot show its seals (EBADF); its name decides.
    let wanted = i64::from(crate::syscalls::rendered_procfs::PLACEHOLDER_SEALS);
    if seals != -i64::from(libc::EBADF) && (seals < 0 || seals & wanted != wanted) {
        return None;
    }
    // The calling thread's own table: `/proc/self/fd` is the leader's.
    let link = std::ffi::CString::new(format!("/proc/thread-self/fd/{fd}")).ok()?;
    let mut target = [0_u8; crate::procfs::PLACEHOLDER_LINK_CAPACITY];
    // SAFETY: the path is NUL-terminated and `target` is writable.
    let length = unsafe {
        crate::util::raw_syscall(
            libc::SYS_readlinkat,
            [
                libc::AT_FDCWD as u64,
                link.as_ptr() as u64,
                target.as_mut_ptr() as u64,
                target.len() as u64,
                0,
                0,
            ],
        )
    };
    let length = usize::try_from(length).ok()?.min(target.len());
    crate::procfs::placeholder_link(&target[..length], files)
}

#[cfg(test)]
mod native_main_state_tests {
    use super::*;
    use crate::native_exec::capture_prepared_native_exec;
    use crate::native_exec::tests::fixture;
    use crate::native_exec::tests::prepare;
    use crate::native_exec::tests::state_bytes;
    use crate::native_exec::validate_old_native_exec_cut;
    fn snapshot(
        tool: &crate::Detcore<crate::native_exec::NoopTool>,
        thread: &ThreadState<()>,
        binding: &crate::native_exec::NativeExecCutBinding,
        receipt: &crate::native_exec::NativeExecPreparationReceipt,
    ) -> crate::native_exec::DetcoreExecSnapshotV2 {
        let bytes = bincode::serde::encode_to_vec(
            capture_prepared_native_exec(tool, thread, binding, receipt).unwrap(),
            bincode::config::legacy(),
        )
        .unwrap();
        crate::native_exec::tests::decoded(&bytes)
    }

    #[test]
    fn native_main_claimed_sigchld_refuses_live_and_decoded_without_reset() {
        let (tool, mut thread, binding) = fixture();
        thread.claimed_sigchld = true;
        let before = state_bytes(&thread);
        assert!(matches!(
            validate_old_native_exec_cut(&tool, &thread, &binding),
            Err(crate::native_exec::NativeExecCutError::LiveCapability(
                "claimed SIGCHLD"
            ))
        ));
        assert_eq!(state_bytes(&thread), before);
        thread.claimed_sigchld = false;
        let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
        prepare(&mut thread, &binding);
        let mut payload = snapshot(&tool, &thread, &binding, &receipt);
        crate::native_exec::tests::thread_for_test(&mut payload).claimed_sigchld = true;
        assert!(matches!(
            payload.validate(&binding, &tool.cfg),
            Err(crate::native_exec::NativeExecCutError::LiveCapability(
                "claimed SIGCHLD"
            ))
        ));
        assert!(crate::native_exec::tests::thread_for_test(&mut payload).claimed_sigchld);
    }

    fn mark_barrier(process: &mut RobustListProcessState, field: usize, owner: DetTid) {
        match field {
            0 => {
                process.tool_walked.insert(owner);
            }
            1 => process.exit_group_filed = true,
            2 => {
                process.exit_group_filers.insert(owner);
            }
            3 => process.exit_group_walker = Some(owner),
            4 => process.exit_group_walked = true,
            _ => unreachable!(),
        }
    }

    #[test]
    fn native_main_robust_barrier_state_refuses_old_prepared_and_decoded() {
        for field in 0..5 {
            let (tool, thread, binding) = fixture();
            mark_barrier(
                &mut thread.robust_list_process.lock().unwrap(),
                field,
                binding.dettid,
            );
            let before = state_bytes(&thread);
            assert!(
                validate_old_native_exec_cut(&tool, &thread, &binding).is_err(),
                "old {field}"
            );
            assert_eq!(state_bytes(&thread), before);

            let (tool, mut thread, binding) = fixture();
            let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
            prepare(&mut thread, &binding);
            let mut payload = snapshot(&tool, &thread, &binding, &receipt);
            mark_barrier(
                &mut thread.robust_list_process.lock().unwrap(),
                field,
                binding.dettid,
            );
            let before = state_bytes(&thread);
            assert!(
                capture_prepared_native_exec(&tool, &thread, &binding, &receipt).is_err(),
                "prepared {field}"
            );
            assert_eq!(state_bytes(&thread), before);
            let decoded = crate::native_exec::tests::thread_for_test(&mut payload);
            mark_barrier(
                &mut decoded.robust_list_process.lock().unwrap(),
                field,
                binding.dettid,
            );
            let before = state_bytes(decoded);
            assert!(
                payload.validate(&binding, &tool.cfg).is_err(),
                "decoded {field}"
            );
            assert_eq!(
                state_bytes(crate::native_exec::tests::thread_for_test(&mut payload)),
                before
            );
        }
    }

    #[test]
    fn native_main_nanosleep_restart_refuses_even_when_serde_skips_it() {
        for deadline in [None, Some(crate::types::LogicalTime::from_nanos(4567))] {
            let (tool, mut thread, binding) = fixture();
            let before = state_bytes(&thread);
            thread.restart_block = Some(crate::syscalls::helpers::RestartBlock {
                rip: 0x1234,
                deadline,
                call: crate::syscalls::helpers::RestartCall::Nanosleep(
                    reverie::syscalls::Nanosleep::new().into(),
                ),
            });
            assert_eq!(state_bytes(&thread), before);
            assert!(matches!(
                validate_old_native_exec_cut(&tool, &thread, &binding),
                Err(crate::native_exec::NativeExecCutError::LiveCapability(
                    "restart/projection"
                ))
            ));
            assert!(thread.restart_block.is_some());
        }
    }

    #[test]
    fn native_main_rng_promotion_history_survives_snapshot_and_some_empty() {
        use std::os::fd::AsRawFd;
        crate::fd::install_test_shared_open_file_channel();
        {
            let (tool, mut thread, binding) = fixture();
            let local = std::fs::File::open("/dev/urandom").unwrap();
            thread
                .add_fd(local.as_raw_fd(), OFlag::O_RDONLY, FdType::Rng, None)
                .unwrap();
            assert!(thread.shared_open_files_for_exec().unwrap().is_none());
            let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
            prepare(&mut thread, &binding);
            let (_, restored) = snapshot(&tool, &thread, &binding, &receipt)
                .into_validated_parts(&binding, &tool.cfg)
                .unwrap();
            assert!(restored.shared_open_files_for_exec().unwrap().is_none());
        }
        let (tool, mut thread, binding) = fixture();
        let creator = DetTid::from_raw(930001);
        thread.set_open_file_creator(creator);
        let file = std::fs::File::open("/dev/urandom").unwrap();
        let alias = nix::unistd::dup(&file).unwrap();
        let cloexec = nix::unistd::dup(&file).unwrap();
        let distinct = std::fs::File::open("/dev/urandom").unwrap();
        let (fd, alias_fd, cloexec_fd, distinct_fd) = (
            file.as_raw_fd(),
            alias.as_raw_fd(),
            cloexec.as_raw_fd(),
            distinct.as_raw_fd(),
        );
        thread
            .add_fd(fd, OFlag::O_RDONLY, FdType::Rng, None)
            .unwrap();
        thread.dup_fd(fd, alias_fd, OFlag::empty()).unwrap();
        thread.dup_fd(fd, cloexec_fd, OFlag::O_CLOEXEC).unwrap();
        thread
            .add_fd(distinct_fd, OFlag::O_RDONLY, FdType::Rng, None)
            .unwrap();
        assert!(
            thread.shared_open_files_for_exec().unwrap().is_none(),
            "local RNG is not promotion"
        );
        let high = thread
            .metadata()
            .allocate_open_file_id(creator, FdType::Rng);
        let high_socket = thread
            .metadata()
            .allocate_open_file_id(creator, FdType::Socket);
        thread.promote_shared_open_files().unwrap();
        let expected = thread.shared_open_files_for_exec().unwrap().unwrap();
        assert_eq!(expected.creator, creator);
        assert_eq!(expected.handles[&fd].id, expected.handles[&alias_fd].id);
        assert_ne!(expected.handles[&fd].id, expected.handles[&distinct_fd].id);
        assert!(!expected.handles.contains_key(&cloexec_fd));
        let old_table = Arc::clone(&thread.file_metadata);
        let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
        prepare(&mut thread, &binding);
        assert!(
            old_table
                .lock()
                .unwrap()
                .file_handles
                .contains_key(&cloexec_fd)
        );
        let (_, restored) = snapshot(&tool, &thread, &binding, &receipt)
            .into_validated_parts(&binding, &tool.cfg)
            .unwrap();
        assert_eq!(
            restored.shared_open_files_for_exec().unwrap(),
            Some(expected)
        );
        assert_eq!(restored.open_file_creator, Some(creator));
        assert_eq!(
            restored.metadata().next_open_file_sequence,
            thread.metadata().next_open_file_sequence
        );
        assert_eq!(
            restored.metadata().next_socket_open_file_sequence,
            thread.metadata().next_socket_open_file_sequence
        );
        restored.metadata().file_handles[&alias_fd]
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 0);
                Ok(11)
            })
            .unwrap();
        thread.metadata().file_handles[&fd]
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 11);
                Ok(0)
            })
            .unwrap();
        // Both tables retain the same canonical shared ID, not a copied cursor.
        for slot in [fd, alias_fd, distinct_fd] {
            thread.remove_fd(slot);
        }
        let empty = thread.shared_open_files_for_exec().unwrap().unwrap();
        assert!(empty.handles.is_empty());
        assert_eq!(empty.creator, creator);
        assert_ne!(OpenFileId::new(creator, empty.next_sequence), high);
        assert_ne!(
            OpenFileId::new_socket(creator, empty.next_socket_sequence),
            high_socket
        );
        // Drop the test-owned metadata guard before the validator probes it.
        let old_files = thread.metadata().files_id;
        let binding2 = crate::native_exec::NativeExecCutBinding {
            old_mm: thread.mm_id,
            old_files,
            ..binding
        };
        let receipt2 = validate_old_native_exec_cut(&tool, &thread, &binding2).unwrap();
        prepare(&mut thread, &binding2);
        let (_, again) = snapshot(&tool, &thread, &binding2, &receipt2)
            .into_validated_parts(&binding2, &tool.cfg)
            .unwrap();
        assert_eq!(again.shared_open_files_for_exec().unwrap(), Some(empty));
    }
}
/// The hint every report of a preemption replay that left its record ends
/// with: a faithful replay never leaves it, but a record edited after it was
/// recorded (preemptions knocked out, as `hermit analyze` does) does by
/// design, and is replayed with `--replay-edited-preemptions`, which skips the
/// slices that already ended instead (`next_edited_slice`) and reports none
/// of these.
pub(crate) const EDITED_RECORD_HINT: &str = "An edited record, not the recording of this run's \
     schedule, needs --replay-edited-preemptions.";

/// Reports a preemption replay's thread about to file a change point at
/// `time`, not after its last one at `last`
/// ([`ThreadState::file_replay_change_point`]). This is not evidence of PMU
/// skid. The caller publishes it to the coordinator before abandonment, unless
/// `--panic-on-rcb-overshoot` preserves its synchronous stop here.
#[cold]
pub(crate) fn report_stale_replay_filing(
    panic_on_rcb_overshoot: bool,
    dettid: DetTid,
    slice: u64,
    time: LogicalTime,
    last: LogicalTime,
) {
    let message = format!(
        "{} preemption replay: thread {} would end slice T{} with a change point at {}, not \
         after its last change point at {}: the slice closed with no time in it, which the \
         replayed record never does. The thread files no change point here and runs on as \
         its final slice; the rest of this run does not replay the record exactly. {}",
        "HERMIT_REPLAY_DEPARTURE:", dettid, slice, time, last, EDITED_RECORD_HINT,
    );
    if panic_on_rcb_overshoot {
        panic!("{message}");
    }
    tracing::error!("{message}");
}
