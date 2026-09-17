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
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
use tracing::debug;

use crate::config::Config;
use crate::detlog;
use crate::fd::*;
use crate::memory::MemoryMetadata;
use crate::preemptions::ThreadHistoryIterator;
use crate::record_or_replay::NoopTool;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::ChaosEpochTransition;
use crate::resources::Device;
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
    /// Track what file handles actually point to (e.g. after dup2).
    /// This includes both the identifying resource (usually inode) and the deterministic file handle.
    pub(crate) file_handles: HashMap<RawFd, DetFd>,
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
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CapturedDetFdInstallCleanup {
    /// The exact kernel-returned descriptor that must be closed.
    pub(crate) close_fd: RawFd,
    /// A scheduler resource whose final modeled OFD reference was the capture.
    pub(crate) release_open_file: Option<OpenFileId>,
}

impl CapturedDetFdInstallError {
    /// Abandon the failed capture and preserve both cleanup obligations.
    pub(crate) fn into_cleanup(self) -> CapturedDetFdInstallCleanup {
        let release_open_file = (self.captured.detfd.open_file_alias_count() == 1)
            .then(|| self.captured.detfd.open_file_id());
        drop(self.captured);
        CapturedDetFdInstallCleanup {
            close_fd: self.returned_fd,
            release_open_file,
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
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// Allocate a new (disarmed) timer, returning its deterministic id.
    pub(crate) fn create(&mut self, signal: Option<i32>) -> i32 {
        let id = self.next_id;
        self.next_id += 1;
        self.timers.insert(
            id,
            PosixTimer {
                interval_ns: 0,
                deadline: None,
                signal,
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

    /// Remove a timer; returns whether it existed.
    pub(crate) fn remove(&mut self, id: i32) -> bool {
        self.timers.remove(&id).is_some()
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
        let mut limits = vec![unlimited; libc::RLIMIT_RTTIME as usize + 1];
        limits[libc::RLIMIT_STACK as usize] = ResourceLimit {
            current: 8 * 1024 * 1024,
            maximum: libc::RLIM64_INFINITY,
        };
        limits[libc::RLIMIT_NOFILE as usize] = ResourceLimit {
            current: 1_048_576,
            maximum: 1_048_576,
        };
        Self { limits }
    }
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
            self.record_or_replay
                .handle_syscall_event(&mut guest.into_guest(), syscall.into())
                .await,
        )?
        .map_err(Error::from)
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
        self.record_or_replay
            .handle_syscall_event(&mut guest.into_guest(), syscall.into())
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
    fn new(owner: DetTid) -> Self {
        FileMetadata {
            files_id: FilesId::initial(owner),
            next_open_file_sequence: 0,
            next_socket_open_file_sequence: 0,
            file_handles: HashMap::new(),
        }
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
    fn has_unsafe_vfork_flock_state(&self) -> bool {
        self.file_handles.values().any(|detfd| {
            matches!(detfd.known_flock_mode(), Some(Some(_))) || detfd.flock_mode_may_be_stale()
        })
    }

    fn forget_flock_modes(&self) {
        for detfd in self.file_handles.values() {
            detfd.forget_flock_mode();
        }
    }

    // TODO-HUMAN-REVIEW(#2373)
    /// Fork the file table for `child`.
    ///
    /// FORGETTING IS DELIBERATELY *NOT* DONE HERE, and the reason is that this
    /// fork does not produce a second copy of the cache. `DetFd` holds its
    /// `OpenFileDescription` behind an `Arc<Mutex<..>>`, so `file_handles
    /// .clone()` below hands the child the *same* description objects the
    /// parent holds. That mirrors the kernel, where `fork(2)` duplicates the
    /// descriptor table while both tables keep pointing at one open file
    /// description, and an `flock(2)` lock belongs to that description. A
    /// `flock` through either task therefore mutates the one shared record and
    /// both sides observe it; there is no second copy that could go stale, so
    /// invalidating here destroys correct information and buys nothing.
    ///
    /// The case that genuinely needs invalidation is a *separately hosted* tool
    /// state — a real process clone under DBT, where the in-process runtime is
    /// copied into a new address space and the `Arc` stops being shared. That
    /// is handled at its own boundary, by
    /// `reverie_dbt_runtime_process_clone_result`, which calls
    /// `forget_flock_modes` only on a clone the kernel reported as successful.
    ///
    /// Forgetting here as well was not merely redundant, it was harmful: it
    /// marked every descriptor unknown on any fork, and
    /// `has_unsafe_vfork_flock_state` refuses `vfork` on unknown as well as on
    /// held. So any guest that forked and later vforked was refused even when
    /// it had never called `flock` at all.
    pub(crate) fn fork_for(&self, child: DetTid) -> Self {
        self.forget_flock_modes();
        Self {
            files_id: FilesId::forked(child),
            next_open_file_sequence: self.next_open_file_sequence,
            next_socket_open_file_sequence: self.next_socket_open_file_sequence,
            file_handles: self.file_handles.clone(),
        }
    }

    pub(crate) fn for_exec(&self, task: DetTid) -> Self {
        Self {
            files_id: self.files_id.for_exec(task),
            next_open_file_sequence: self.next_open_file_sequence,
            next_socket_open_file_sequence: self.next_socket_open_file_sequence,
            file_handles: self
                .file_handles
                .iter()
                .filter_map(|(&fd, detfd)| (!detfd.is_cloexec()).then_some((fd, detfd.clone())))
                .collect(),
        }
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
    fn setup_stdio(mut self, _pid: Pid, owner: DetTid) -> Self {
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
        let file_type = stat::SFlag::from_bits_truncate(raw_stat.st_mode);
        let ty = if file_type.contains(stat::SFlag::S_IFIFO) {
            FdType::Pipe
        } else if file_type.contains(stat::SFlag::S_IFSOCK) {
            FdType::Socket
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
        self.add_fd(owner, fd, flags, ty, Some(raw_stat.into()))?;
        self.with_detfd(fd, |detfd| detfd.forget_flock_mode())?;
        if let Some(resource) = stdio_resource(fd) {
            self.with_detfd(fd, |detfd| detfd.set_resource(resource.clone()))?;
        }
        if ty == FdType::Pipe && physically_nonblocking {
            self.with_detfd(fd, |detfd| detfd.set_physically_nonblocking())?;
        }
        Ok(())
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
        let id = self.allocate_open_file_id(creator, ty);
        let detfd = DetFd::new(fd, flags, ty, id).with_stat(stat);
        self.add_detfd(detfd);
        Ok(())
    }

    /// remove a rawfd
    fn remove_fd(&mut self, fd: RawFd) -> Option<OpenFileId> {
        let detfd = self.file_handles.remove(&fd)?;
        (detfd.open_file_alias_count() == 1).then(|| detfd.open_file_id())
    }

    /// Remove every modeled descriptor in an inclusive close_range interval.
    fn remove_fd_range(&mut self, first: u32, last: u32) -> Vec<OpenFileId> {
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
        let replaced = self.file_handles.insert(newfd, detfd);
        Ok(replaced
            .and_then(|detfd| (detfd.open_file_alias_count() == 1).then(|| detfd.open_file_id())))
    }

    /// Drop a capture that was not installed and report whether it retained the
    /// final reference to the open-file description.
    fn abandon_captured_fd(&mut self, captured: CapturedDetFd) -> Option<OpenFileId> {
        let release =
            (captured.detfd.open_file_alias_count() == 1).then(|| captured.detfd.open_file_id());
        drop(captured);
        release
    }
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
        assert_eq!(timers.create(None), 0);
        assert_eq!(timers.create(Some(libc::SIGALRM)), 1);
        assert_eq!(timers.create(None), 2);
    }

    #[test]
    fn settime_reports_previous_arming_and_remaining_uses_virtual_clock() {
        let mut timers = PosixTimers::default();
        let id = timers.create(None);

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
        let id = timers.create(None);
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
        let id = timers.create(Some(libc::SIGALRM));
        timers.settime(id, 50, Some(t(100)), t(0));

        assert_eq!(timers.gettime(id, t(100)), Some((50, 50)));
        assert_eq!(timers.gettime(id, t(125)), Some((25, 50)));
        assert_eq!(timers.gettime(id, t(150)), Some((50, 50)));
        assert_eq!(timers.signal(id), Some(Some(libc::SIGALRM)));
    }

    #[test]
    fn disarm_and_unknown_ids() {
        let mut timers = PosixTimers::default();
        let id = timers.create(None);
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
        let id = timers.create(None);
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
            limits.get(libc::RLIMIT_STACK),
            Some(ResourceLimit {
                current: 8 * 1024 * 1024,
                maximum: libc::RLIM64_INFINITY,
            })
        );
        assert_eq!(
            limits.get(libc::RLIMIT_NOFILE),
            Some(ResourceLimit {
                current: 1_048_576,
                maximum: 1_048_576,
            })
        );
        assert_eq!(
            limits.get(libc::RLIMIT_CORE),
            Some(ResourceLimit {
                current: libc::RLIM64_INFINITY,
                maximum: libc::RLIM64_INFINITY,
            })
        );
        assert_eq!(limits.get(libc::RLIMIT_RTTIME + 1), None);
    }

    #[test]
    fn cloned_process_state_changes_independently() {
        let parent = ResourceLimits::default();
        let mut child = parent.clone();
        let lowered = ResourceLimit {
            current: 1024,
            maximum: 1_048_576,
        };
        child.set(libc::RLIMIT_NOFILE, lowered);

        assert_eq!(child.get(libc::RLIMIT_NOFILE), Some(lowered));
        assert_eq!(
            parent.get(libc::RLIMIT_NOFILE),
            Some(ResourceLimit {
                current: 1_048_576,
                maximum: 1_048_576,
            })
        );
    }
}

#[cfg(test)]
mod file_metadata_tests {
    use std::os::fd::AsRawFd;

    use super::*;

    #[test]
    fn on_demand_discovery_finds_a_live_descriptor() {
        let owner = DetTid::from_raw(9);
        let file = std::fs::File::open("/dev/null").expect("open test descriptor");
        let fd = file.as_raw_fd();
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
}

/// Various measurements of one guest thread's execution. This is useful for printing
/// context in logs as we go and printing a final summary.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct ThreadStats {
    /// A simple count of how many syscalls have been handled on this thread.
    pub syscall_count: u64,

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
    pub parent_detpid: DetPid,
    pub child_tid_addr: usize,
    pub flags: CloneFlags,
    pub exit_signal: libc::c_int,
    pub child_priority_entropy: Option<u64>,
}

// TODO-HUMAN-REVIEW(#797): Review process-wide logical CPU aggregation.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct ProcessCpuSnapshot {
    pub user: LogicalTime,
    pub system: LogicalTime,
    pub children_user: LogicalTime,
    pub children_system: LogicalTime,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct ProcessCpuTime {
    snapshot: ProcessCpuSnapshot,
    exited_children: BTreeMap<DetPid, ProcessCpuSnapshot>,
}

impl ProcessCpuTime {
    fn add_thread_delta(&mut self, user: LogicalTime, system: LogicalTime) {
        self.snapshot.user = self.snapshot.user + user;
        self.snapshot.system = self.snapshot.system + system;
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

/// Robust-list registrations and staged wakes shared by one Linux thread
/// group. A forked process starts with an empty instance; `CLONE_THREAD`
/// members share it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct RobustListProcessState {
    heads: BTreeMap<DetTid, usize>,
    pending: BTreeMap<DetTid, PendingRobustListWakes>,
    matched_physical_exits: BTreeMap<DetTid, DetTime>,
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

    /// Host thread ID supplied by a backend whose scheduler identity is
    /// virtual. `None` keeps the existing direct-ID behavior for other
    /// backends.
    #[serde(default)]
    pub physical_tid: Option<i32>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1063): Review backend-supplied open-file creator identity.
    /// Stable identity used when allocating deterministic open-file descriptions.
    #[serde(default)]
    pub(crate) open_file_creator: Option<DetTid>,

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

    /// Registration metadata for a child whose parent cannot resume until the
    /// backend finishes the child. The child consumes this in
    /// `handle_thread_start`; the parent clears its copy when injection returns.
    pub pending_vfork: Option<PendingVfork>,

    /// Shared file metadata among all threads in the same process.
    /// Initialized for new threads (shared or fresh), and then overwritten again on `execve`.
    pub file_metadata: Arc<Mutex<FileMetadata>>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review backend-gated live descriptor discovery state.
    /// Whether missing guest descriptors may be inspected in the current process.
    #[serde(default)]
    pub(crate) discover_live_file_metadata: bool,

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

    /// Parent process accounting notified when this process leader exits.
    pub(crate) parent_process_cpu_time: Option<Arc<Mutex<ProcessCpuTime>>>,

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
    pub(crate) initialized_random_auxv: Option<crate::random::InitialImage>,

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

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Exact per-thread PMU RCB target for the active preemption-replay slice.
    /// `None` selects the legacy logical-time deadline path.
    #[serde(default)]
    pub replay_rcb_end: Option<u64>,

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

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Transitions waiting to be committed into the preemption artifact.
    #[serde(default)]
    pub pending_chaos_epochs: Vec<ChaosEpochTransition>,

    /// Absolute deadline enforced by the PMU-backed `--max-timeslice` timer. This is separate from
    /// `end_of_timeslice` so syscall-heavy workloads can use a shorter, cheap target deadline.
    pub max_timeslice_end: Option<LogicalTime>,

    /// Track what our last timer was set for, just to double check that RCB timers are behaving
    /// as expected and see if we went over.  (For exmaple, this behaves badly if threads are not
    /// pinned and our we migrate between cores.)
    pub last_rcb_timer: Option<u64>,

    /// Whether `last_rcb_timer` represents the maximum deadline rather than a manual interrupt.
    #[serde(default)]
    pub last_rcb_timer_is_max: bool,

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

impl<T> ThreadState<T> {
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
        self.process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned")
            .add_thread_delta(user_delta, system_delta);
        self.last_accounted_user_time = user;
        self.last_accounted_system_time = system;
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

    pub(crate) fn record_exited_child_process_cpu_time(&mut self, pid: DetPid) {
        self.account_process_cpu_time();
        let Some(parent) = &self.parent_process_cpu_time else {
            return;
        };
        let child = self
            .process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned")
            .snapshot;
        parent
            .lock()
            .expect("parent process CPU time mutex poisoned")
            .record_exited_child(pid, child);
    }

    pub(crate) fn has_exited_child_process_cpu_time(&self, pid: DetPid) -> bool {
        self.process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned")
            .exited_children
            .contains_key(&pid)
    }

    pub(crate) fn reap_child_process_cpu_time(&mut self, pid: DetPid) {
        self.account_process_cpu_time();
        self.process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned")
            .reap_child(pid);
    }

    pub(crate) fn prepare_child_process_cpu_time(&self, pid: DetPid) {
        self.process_cpu_time
            .lock()
            .expect("process CPU time mutex poisoned")
            .prepare_child(pid);
    }

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
        let thread_logical_time = DetTime::new(cfg);
        let last_accounted_user_time = thread_logical_time.user_cpu_time();
        let last_accounted_system_time = thread_logical_time.system_cpu_time();
        let file_metadata = if cfg.discover_live_file_metadata {
            let mut metadata = FileMetadata::new(pid);
            for fd in 0..=2 {
                metadata
                    .discover_fd_from_current_process(pid, fd)
                    .expect("SaBRe guest stdio must be open");
            }
            metadata
        } else {
            FileMetadata::new(pid).setup_stdio(pid.into(), pid)
        };
        ThreadState {
            dettid: pid,
            detpid: None, // Initialized later.
            thread_start_entered: false,
            physical_tid: None,
            open_file_creator: None,
            mm_id: MmId::initial(pid),
            memory_metadata: Arc::new(Mutex::new(MemoryMetadata::new())),
            pedigree: Pedigree::new(), // Root thread.
            stats: ThreadStats::new(),
            file_metadata: Arc::new(Mutex::new(file_metadata)),
            discover_live_file_metadata: cfg.discover_live_file_metadata,
            timer_slack_ns: DEFAULT_TIMER_SLACK_NS,
            default_timer_slack_ns: DEFAULT_TIMER_SLACK_NS,
            posix_timers: Arc::new(Mutex::new(PosixTimers::default())),
            resource_limits: Arc::new(Mutex::new(ResourceLimits::default())),
            process_cpu_time: Arc::new(Mutex::new(ProcessCpuTime::default())),
            guest_clock: Arc::new(Mutex::new(GuestClock::default())),
            parent_process_cpu_time: None,
            last_accounted_user_time,
            last_accounted_system_time,
            thread_cpu_start_user_time: last_accounted_user_time,
            thread_cpu_start_system_time: last_accounted_system_time,
            clone_flags: None,
            pending_vfork: None,
            // For the root thread, we initialize from the seed in the config:
            prng: crate::random::root_prng(cfg.rng_seed()),
            initialized_random_auxv: None,
            chaos_prng: Pcg64Mcg::seed_from_u64(cfg.sched_seed()),
            thread_logical_time,
            committed_clock_value: 0,
            end_of_timeslice: None, // Temporary/bogus.
            replay_rcb_end: None,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1151)
            chaos_epoch: chaos_epoch_sentinel(),
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1151)
            chaos_slowdown_factor: RcbTimeMultiplier::ONE,
            chaos_slowdown_active: false,
            pending_chaos_epochs: Vec::new(),
            max_timeslice_end: None,
            last_rcb_timer: None,
            last_rcb_timer_is_max: false,
            record_or_replay,
            preemption_points: None,
            past_global_first_execve: false,
            interrupt_at: cfg.interrupts_for_thread(pid),
            robust_list_head: None,
            robust_list_process: Arc::new(Mutex::new(RobustListProcessState::default())),
        }
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
        let prng = crate::random::decode_initial_state(bytes, config, image)?;
        self.prng = prng;
        self.initialized_random_auxv = Some(image);
        Ok(())
    }

    pub(crate) fn complete_initial_random_auxv(
        &mut self,
        pointer: Option<usize>,
    ) -> Result<bool, Errno> {
        let Some(image) = self.initialized_random_auxv else {
            return Ok(false);
        };
        if pointer != Some(image.at_random) || self.dettid.as_raw() != image.pid {
            return Err(Errno::EPROTO);
        }
        self.initialized_random_auxv = None;
        Ok(true)
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
        process.heads.remove(&self.dettid);
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
            signal_interrupt_errno: None,
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
        self.metadata().add_fd(
            self.open_file_creator.unwrap_or(self.dettid),
            fd,
            flags,
            ty,
            stat,
        )
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1063): Review backend-supplied open-file creator identity.
    /// Overrides the task identity embedded in subsequently allocated open-file descriptions.
    pub fn set_open_file_creator(&mut self, creator: DetTid) {
        self.open_file_creator = Some(creator);
    }

    /// Get a mutable reference of `DetFd` from a raw file descriptor, and
    /// run mutable function `f` on it (`&mut DetFd`).
    pub fn with_detfd<F, U>(&self, fd: RawFd, f: F) -> Result<U, Errno>
    where
        F: FnMut(&mut DetFd) -> U,
    {
        let mut metadata = self.metadata();
        if self.discover_live_file_metadata {
            metadata.discover_fd_from_current_process(self.dettid, fd)?;
        }
        metadata.with_detfd(fd, f)
    }

    pub(crate) fn count_open_files_at_paths(&self, paths: &[&Path]) -> usize {
        self.metadata().count_open_files_at_paths(paths)
    }

    /// Whether this task owns a socket that attempted a loopback connection.
    pub(crate) fn has_loopback_peer(&self) -> bool {
        self.metadata().has_loopback_peer()
    }

    /// Whether any open file description is held or not proven unlocked.
    pub fn has_unsafe_vfork_flock_state(&self) -> bool {
        self.metadata().has_unsafe_vfork_flock_state()
    }

    /// A new process can change every inherited open file description through
    /// a separately hosted tool state, so neither side may keep treating its
    /// cached flock mode as authoritative after fork.
    pub fn forget_flock_modes(&self) {
        self.metadata().forget_flock_modes();
    }

    /// remove a rawfd
    pub fn remove_fd(&self, fd: RawFd) -> Option<OpenFileId> {
        self.metadata().remove_fd(fd)
    }

    /// Remove every modeled descriptor in an inclusive close_range interval.
    pub(crate) fn remove_fd_range(&self, first: u32, last: u32) -> Vec<OpenFileId> {
        self.metadata().remove_fd_range(first, last)
    }

    /// dup raw fds.
    pub fn dup_fd(
        &mut self,
        oldfd: RawFd,
        newfd: RawFd,
        flags: OFlag,
    ) -> Result<Option<OpenFileId>, Errno> {
        let mut metadata = self.metadata();
        if self.discover_live_file_metadata {
            metadata.discover_fd_from_current_process(self.dettid, oldfd)?;
        }
        metadata.dup_fd(oldfd, newfd, flags)
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
        if self.discover_live_file_metadata {
            metadata.discover_fd_from_current_process(self.dettid, pidfd)?;
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
        if self.discover_live_file_metadata {
            metadata.discover_fd_from_current_process(self.dettid, targetfd)?;
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
        self.metadata().install_captured_fd(captured, newfd, flags)
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

    /// Whether this thread has consumed its current logical timeslice.
    pub(crate) fn timeslice_expired(&self) -> bool {
        if let Some(replay_rcb_end) = self.replay_rcb_end {
            return self.committed_clock_value >= replay_rcb_end;
        }
        let current_time = self.thread_logical_time.as_nanos();
        self.end_of_timeslice
            .is_some_and(|end_of_timeslice| current_time >= end_of_timeslice)
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

    // AUTONOMOUS-BOT-IMPLEMENTED
    /// Choose the logical target and PMU maximum deadlines for the next timeslice.
    ///
    /// Effects:
    /// - Sets `end_of_timeslice` for the new timeslice.
    /// - Sets `max_timeslice_end` when PMU-backed preemption is enabled.
    /// - Resets the statistics for the timeslice.
    ///
    /// Returns: an optional new priority.
    pub fn next_timeslice(&mut self, cfg: &Config) -> Option<Priority> {
        let logical_timeslice = cfg.target_timeslice.or(cfg.max_timeslice);
        if let Some(timeout_ns) = logical_timeslice {
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
                let transition_time = self.end_of_timeslice.unwrap_or(current_ns);
                let transition = self
                    .preemption_points
                    .as_mut()
                    .and_then(|history| history.advance_chaos_epoch(transition_time));
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

            let mut result = None;
            self.replay_rcb_end = None;
            let replay_controls_deadline =
                self.preemption_points.is_some() || cfg.replay_schedule_from.is_some();

            // Preemption-point replay from recorded --chaos configuration.
            if let Some(thi) = &mut self.preemption_points {
                if self.stats.last_recorded_slice.is_none() {
                    // We have not tapped out the recording yet.
                    if let Some((end_time, prio, replay_rcb_end)) = thi.next_with_rcbs() {
                        debug!(
                            "[dtid {}] next timeslice (T{}), set by recording to {:?} (current {}), priority {}",
                            self.dettid,
                            self.stats.timeslice_count + 1,
                            end_time,
                            current_ns,
                            prio
                        );
                        let current_rcbs = self.committed_clock_value;
                        if let Some(target_rcbs) = replay_rcb_end
                            && target_rcbs < current_rcbs
                        {
                            panic!(
                                "Cannot set RCB end of timeslice to {} for thread {}, when current RCB count is already {}.",
                                target_rcbs, self.dettid, current_rcbs
                            )
                        }
                        let exact_deadline_is_now =
                            replay_rcb_end.is_some_and(|target| target == current_rcbs);
                        if end_time <= current_ns && !exact_deadline_is_now {
                            panic!(
                                "Cannot set end of timeslice to {} for thread {}, when current thread logical time is already {}.",
                                end_time, self.dettid, current_ns
                            )
                        }
                        self.end_of_timeslice = Some(end_time);
                        self.replay_rcb_end = replay_rcb_end;
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
                    if cfg.no_rcb_time {
                        let max_timeslice = cfg
                            .max_timeslice
                            .expect("schedule replay with PMU requires a maximum");
                        self.end_of_timeslice =
                            Some(current_ns + Duration::from_nanos(u64::from(max_timeslice)));
                    } else {
                        // Branch-event replay will overwrite this deadline when needed.
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

            let configured_max_end = cfg
                .max_timeslice
                .map(|max_timeslice| current_ns + Duration::from_nanos(u64::from(max_timeslice)));
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

            self.last_rcb_timer = None;
            self.last_rcb_timer_is_max = false;
            self.reset_timeslice_stats(current_ns);
            result
        } else {
            self.end_of_timeslice = None;
            self.replay_rcb_end = None;
            self.max_timeslice_end = None;
            self.last_rcb_timer = None;
            self.last_rcb_timer_is_max = false;
            None
        }
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

#[cfg(test)]
mod timeslice_tests {
    use std::num::NonZeroU64;

    use super::*;
    use crate::DEFAULT_PRIORITY;
    use crate::preemptions::ThreadHistory;

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

    fn nz(value: u64) -> Option<NonZeroU64> {
        NonZeroU64::new(value)
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
