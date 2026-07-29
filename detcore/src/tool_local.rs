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
use nix::fcntl::AtFlags;
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
    /// Track what file handles actually point to (e.g. after dup2).
    /// This includes both the identifying resource (usually inode) and the deterministic file handle.
    pub(crate) file_handles: HashMap<RawFd, DetFd>,
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
            self.allocate_open_file_id(owner),
        )
        .with_stat(stat)
        .with_resource(ResourceID::Device(Device::ContainerStdin));
        let stdout = DetFd::new(
            1,
            OFlag::empty(),
            FdType::Regular,
            self.allocate_open_file_id(owner),
        )
        .with_stat(stat)
        .with_resource(ResourceID::Device(Device::ContainerStdout));
        let stderr = DetFd::new(
            2,
            OFlag::empty(),
            FdType::Regular,
            self.allocate_open_file_id(owner),
        )
        .with_stat(stat)
        .with_resource(ResourceID::Device(Device::ContainerStderr));

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

/// Guest-visible wall-clock calibration shared by every thread in a process.
///
/// Detcore's raw logical clock includes backend-specific implementation work
/// (for example, ptrace RCBs versus DBI's syscall-only fallback). Calibrating
/// the first observation after exec keeps that startup work out of the clock
/// exposed to the new executable while preserving subsequent logical deltas.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct GuestClock {
    origin: Option<LogicalTime>,
    elapsed: LogicalTime,
}

impl GuestClock {
    fn observe(&mut self, raw: LogicalTime, epoch: LogicalTime) -> LogicalTime {
        let origin = *self.origin.get_or_insert(raw);
        let candidate = if raw >= origin {
            raw - origin
        } else {
            LogicalTime::ZERO
        };
        self.elapsed = self.elapsed.max(candidate);
        epoch + self.elapsed
    }

    fn reset(&mut self) {
        self.origin = None;
        self.elapsed = LogicalTime::ZERO;
    }
}

/// The Detcore per-thread state.
#[derive(Serialize, Deserialize, Clone)]
pub struct ThreadState<T> {
    /// The deterministic thread ID of the this thread.
    pub dettid: DetTid,
    /// The deterministic process ID of the this thread.
    pub detpid: Option<DetTid>,

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

    /// POSIX per-process timers created via `timer_create(2)`. Shared among the
    /// threads of a process (`CLONE_THREAD`) and not inherited across `fork`.
    pub(crate) posix_timers: Arc<Mutex<PosixTimers>>,

    /// Resource limits shared by threads and copied when a new process forks.
    pub(crate) resource_limits: Arc<Mutex<ResourceLimits>>,

    /// Logical CPU accounting shared by all threads in this process.
    pub(crate) process_cpu_time: Arc<Mutex<ProcessCpuTime>>,

    /// Wall-clock calibration shared by all threads in this process.
    #[serde(default)]
    pub(crate) guest_clock: Arc<Mutex<GuestClock>>,

    /// Parent process accounting notified when this process leader exits.
    pub(crate) parent_process_cpu_time: Option<Arc<Mutex<ProcessCpuTime>>>,

    /// Per-thread checkpoints used to add only new work to the process totals.
    pub(crate) last_accounted_user_time: LogicalTime,
    pub(crate) last_accounted_system_time: LogicalTime,

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
    /// Transition waiting to be committed into the preemption artifact.
    #[serde(default)]
    pub pending_chaos_epoch: Option<ChaosEpochTransition>,

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
            .field("resource_limits", &self.resource_limits)
            .field("process_cpu_time", &self.process_cpu_time)
            .field("prng", &self.prng)
            .field("chaos_prng", &self.chaos_prng)
            .field("thread_logical_time", &self.thread_logical_time)
            .field("committed_clock_value", &self.committed_clock_value)
            .field("end_of_timeslice", &self.end_of_timeslice)
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
    pub(crate) fn observe_guest_clock(&self, raw: LogicalTime, epoch: LogicalTime) -> LogicalTime {
        self.guest_clock
            .lock()
            .expect("guest clock mutex poisoned")
            .observe(raw, epoch)
    }

    pub(crate) fn reset_guest_clock(&self) {
        self.guest_clock
            .lock()
            .expect("guest clock mutex poisoned")
            .reset();
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
            open_file_creator: None,
            mm_id: MmId::initial(pid),
            memory_metadata: Arc::new(Mutex::new(MemoryMetadata::new())),
            pedigree: Pedigree::new(), // Root thread.
            stats: ThreadStats::new(),
            file_metadata: Arc::new(Mutex::new(file_metadata)),
            discover_live_file_metadata: cfg.discover_live_file_metadata,
            posix_timers: Arc::new(Mutex::new(PosixTimers::default())),
            resource_limits: Arc::new(Mutex::new(ResourceLimits::default())),
            process_cpu_time: Arc::new(Mutex::new(ProcessCpuTime::default())),
            guest_clock: Arc::new(Mutex::new(GuestClock::default())),
            parent_process_cpu_time: None,
            last_accounted_user_time,
            last_accounted_system_time,
            clone_flags: None,
            pending_vfork: None,
            // For the root thread, we initialize from the seed in the config:
            prng: Pcg64Mcg::seed_from_u64(cfg.rng_seed()),
            chaos_prng: Pcg64Mcg::seed_from_u64(cfg.sched_seed()),
            thread_logical_time,
            committed_clock_value: 0,
            end_of_timeslice: None, // Temporary/bogus.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1151)
            chaos_epoch: chaos_epoch_sentinel(),
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1151)
            chaos_slowdown_factor: RcbTimeMultiplier::ONE,
            chaos_slowdown_active: false,
            pending_chaos_epoch: None,
            max_timeslice_end: None,
            last_rcb_timer: None,
            last_rcb_timer_is_max: false,
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

    /// get thread prng, note this rng is deterministic and should not be used
    /// for crypto.
    pub fn thread_prng(&mut self) -> &mut Pcg64Mcg {
        &mut self.prng
    }

    /// Whether this thread has consumed its current logical timeslice.
    pub(crate) fn timeslice_expired(&self) -> bool {
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
    pub(crate) fn take_pending_chaos_epoch(&mut self) -> Option<ChaosEpochTransition> {
        self.pending_chaos_epoch.take()
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    fn install_chaos_epoch(&mut self, transition: ChaosEpochTransition, record: bool) {
        self.chaos_epoch = transition.epoch;
        self.chaos_slowdown_factor = transition.factor;
        self.chaos_slowdown_active = true;
        self.pending_chaos_epoch = record.then_some(transition);
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
                let transition = self
                    .preemption_points
                    .as_mut()
                    .and_then(|history| history.advance_chaos_epoch(current_ns));
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
                self.pending_chaos_epoch = None;
            }

            let mut result = None;
            let replay_controls_deadline =
                self.preemption_points.is_some() || cfg.replay_schedule_from.is_some();

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
    use crate::preemptions::ThreadHistory;

    #[test]
    fn guest_clock_rebases_backend_startup_work_and_preserves_deltas() {
        let epoch = LogicalTime::from_secs(1_000);
        let mut ptrace = GuestClock::default();
        let mut dbi = GuestClock::default();

        assert_eq!(
            ptrace.observe(epoch + Duration::from_nanos(41_000_000), epoch),
            epoch
        );
        assert_eq!(
            dbi.observe(epoch + Duration::from_nanos(822_000_000), epoch),
            epoch
        );
        assert_eq!(
            ptrace.observe(epoch + Duration::from_nanos(41_025_000), epoch),
            epoch + Duration::from_nanos(25_000)
        );
        assert_eq!(
            dbi.observe(epoch + Duration::from_nanos(822_025_000), epoch),
            epoch + Duration::from_nanos(25_000)
        );
    }

    #[test]
    fn guest_clock_reset_starts_a_new_executable_at_epoch() {
        let epoch = LogicalTime::from_secs(1_000);
        let mut clock = GuestClock::default();
        assert_eq!(clock.observe(epoch + Duration::from_secs(1), epoch), epoch);
        assert_eq!(
            clock.observe(epoch + Duration::from_secs(2), epoch),
            epoch + Duration::from_secs(1)
        );

        clock.reset();
        assert_eq!(clock.observe(epoch + Duration::from_secs(9), epoch), epoch);
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
        let first = state.take_pending_chaos_epoch().unwrap();
        assert_eq!(first.epoch, 0);
        assert_eq!(state.chaos_epoch, 0);

        state
            .thread_logical_time
            .add_rcbs_with_multiplier(10_000, first.factor);
        state.next_timeslice(&config);
        assert_eq!(state.chaos_epoch, 0);
        assert_eq!(state.chaos_slowdown_factor, first.factor);
        assert_eq!(state.take_pending_chaos_epoch(), None);
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
        let first = state.take_pending_chaos_epoch().unwrap();

        state
            .thread_logical_time
            .add_rcbs_with_multiplier(1_000, first.factor);
        let expected_epoch =
            state.thread_logical_time.without_starting().as_nanos() / config.chaos_epoch_length_ns;
        assert!(expected_epoch > 0);

        state.next_timeslice(&config);
        let redraw = state.take_pending_chaos_epoch().unwrap();
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
        assert_eq!(state.take_pending_chaos_epoch(), None);
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
}

/// Generate a new thread-local PRNG from the parent's PRNG state, mixing in the
/// new DetTid for some deterministic entropy. This ensures sequentially-spawned
/// threads get distinct PRNG states.
// TODO-HUMAN-REVIEW(PR-1052): Review collision-free child-thread PRNG seeding.
pub fn thread_rng_from_parent(msg: &str, parent: &Pcg64Mcg, child: DetTid) -> Pcg64Mcg {
    thread_rng_from_parent_entropy_labeled(msg, parent, child.as_raw() as u32 as u128, "tid")
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
    // entropy. DBI uses 96 bits for a stable process/thread sequence; mix those
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
