/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[macro_use]
mod macros;

mod fs;
mod mmap;
mod network;
mod random;
mod time;

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::io::Seek;
use std::io::SeekFrom;
use std::ops::Deref;
use std::ops::DerefMut;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Rdtsc;
use reverie::RdtscResult;
use reverie::Stack;
use reverie::Subscription;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Close;
use reverie::syscalls::Dup3;
use reverie::syscalls::EfdFlags;
use reverie::syscalls::Eventfd2;
use reverie::syscalls::Fchdir;
use reverie::syscalls::Fcntl;
use reverie::syscalls::FcntlCmd;
use reverie::syscalls::Flock;
use reverie::syscalls::FromToRaw;
use reverie::syscalls::Lseek;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::OFlag;
use reverie::syscalls::Openat;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;
use reverie::syscalls::Unlinkat;
use reverie::syscalls::Whence;
use serde::Deserialize;
use serde::Serialize;
fn capture_guest_fd(pid: Pid, fd: libc::c_int) -> (Option<std::os::fd::OwnedFd>, Option<String>) {
    match crate::fd::duplicate_guest_fd(pid, fd) {
        Ok(duplicate) => (Some(duplicate), None),
        Err(error) if error.raw_os_error() == Some(libc::EBADF) => (None, None),
        Err(error) => (None, Some(error.to_string())),
    }
}

fn replay_root(pid: Pid) -> Option<PathBuf> {
    std::fs::canonicalize(format!("/proc/{}/root", pid.as_raw())).ok()
}

fn same_guest_open_file_description(
    pid: Pid,
    left: libc::c_int,
    right: libc::c_int,
) -> io::Result<bool> {
    const KCMP_FILE: libc::c_int = 0;
    let comparison = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            pid.as_raw(),
            pid.as_raw(),
            KCMP_FILE,
            left,
            right,
        )
    };
    if comparison == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(comparison == 0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReplayFileIdentity {
    device: u64,
    inode: u64,
}

impl ReplayFileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

type MaterializedFiles = BTreeMap<ReplayFileIdentity, BTreeMap<ReplayFileIdentity, OwnedFd>>;

static MATERIALIZED_FILES: OnceLock<Mutex<MaterializedFiles>> = OnceLock::new();

fn materialized_files() -> &'static Mutex<MaterializedFiles> {
    MATERIALIZED_FILES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn identity_for_fd(fd: libc::c_int) -> io::Result<ReplayFileIdentity> {
    Ok(ReplayFileIdentity::from_metadata(&std::fs::metadata(
        format!("/proc/self/fd/{fd}"),
    )?))
}

fn replay_root_identity(pid: Pid) -> io::Result<ReplayFileIdentity> {
    Ok(ReplayFileIdentity::from_metadata(&std::fs::metadata(
        format!("/proc/{}/root", pid.as_raw()),
    )?))
}

pub(crate) struct ReplayMaterializationScope {
    identity: ReplayFileIdentity,
    _root: OwnedFd,
}

impl ReplayMaterializationScope {
    pub(crate) fn new(root_fd: libc::c_int) -> io::Result<Self> {
        let root = duplicate_controller_fd(root_fd)?;
        Ok(Self {
            identity: identity_for_fd(root.as_raw_fd())?,
            _root: root,
        })
    }
}

impl Drop for ReplayMaterializationScope {
    fn drop(&mut self) {
        materialized_files()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.identity);
    }
}

fn duplicate_controller_fd(fd: libc::c_int) -> io::Result<OwnedFd> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned by this process.
        Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
    }
}

fn register_materialized(root: ReplayFileIdentity, object: OwnedFd) -> io::Result<()> {
    let identity = identity_for_fd(object.as_raw_fd())?;
    materialized_files()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(root)
        .or_default()
        .entry(identity)
        .or_insert(object);
    Ok(())
}

fn materialized_file_is_registered(pid: Pid, metadata: &std::fs::Metadata) -> bool {
    let Ok(root) = replay_root_identity(pid) else {
        return false;
    };
    materialized_files()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&root)
        .is_some_and(|files| files.contains_key(&ReplayFileIdentity::from_metadata(metadata)))
}

fn unexpected_pidfd_getfd_fd(
    recorded: &Result<i64, Errno>,
    actual: &Result<i64, Errno>,
) -> Option<libc::c_int> {
    if recorded == actual {
        None
    } else {
        actual.as_ref().ok().map(|fd| *fd as libc::c_int)
    }
}

fn remember_materialized_file(pid: Pid, fd: libc::c_int) {
    let Ok(metadata) = std::fs::metadata(format!("/proc/{}/fd/{fd}", pid.as_raw())) else {
        return;
    };
    if metadata.file_type().is_file() {
        let Ok(root) = replay_root_identity(pid) else {
            return;
        };
        let Ok(object) = crate::record_replay_path::open_process_fd(pid, fd) else {
            return;
        };
        let _ = register_materialized(root, object);
    }
}

pub(crate) fn remember_materialized_object(
    root_fd: libc::c_int,
    fd: libc::c_int,
) -> io::Result<()> {
    let metadata = std::fs::metadata(format!("/proc/self/fd/{fd}"))?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "exec materialization did not resolve to a regular file",
        ));
    }
    register_materialized(identity_for_fd(root_fd)?, duplicate_controller_fd(fd)?)
}

#[cfg(test)]
pub(crate) fn registered_materialized_count(root_fd: libc::c_int) -> usize {
    let Ok(root) = identity_for_fd(root_fd) else {
        return 0;
    };
    materialized_files()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&root)
        .map_or(0, BTreeMap::len)
}

fn remember_materialized_path(
    root: &std::os::fd::OwnedFd,
    start: &std::os::fd::OwnedFd,
    path: &Path,
) -> io::Result<()> {
    let resolved = crate::record_replay_path::resolve_existing_path(root, start, path, false)?;
    remember_materialized_object(root.as_raw_fd(), resolved.object.as_raw_fd())
}

use crate::desync::DesyncError;
use crate::event::OpenMaterialization;
use crate::event_stream::ChildEventStreamIds;
use crate::event_stream::DebugEvent;
use crate::event_stream::EventReader;
use crate::event_stream::EventStreamId;
use crate::event_stream::normalize_unused_args;

#[derive(Serialize, Deserialize)]
pub struct ReplayerThreadState {
    events: EventReader,
    stream_id: EventStreamId,
    child_stream_ids: ChildEventStreamIds,
    bootstrapped: bool,
}

impl Deref for ReplayerThreadState {
    type Target = EventReader;

    fn deref(&self) -> &Self::Target {
        &self.events
    }
}

impl DerefMut for ReplayerThreadState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.events
    }
}

impl Default for ReplayerThreadState {
    fn default() -> Self {
        panic!("thread state should be explicitly initialized in init_thread_state")
    }
}

/// A Reverie tool that replays syscalls. Note that only syscalls that cannot be
/// made deterministic are forwarded to this tool.
#[derive(Serialize, Deserialize)]
pub struct Replayer {
    // Keep track of the data directory. Each thread uses this path to open its
    // event stream.
    data: PathBuf,
    /// Duplicates of this guest process's captured output endpoints.
    #[serde(skip)]
    stdout: Option<std::os::fd::OwnedFd>,
    #[serde(skip)]
    stderr: Option<std::os::fd::OwnedFd>,
    /// Preserve replayed write ordering independently for each captured stream.
    #[serde(skip)]
    stdout_output_lock: tokio::sync::Mutex<()>,
    #[serde(skip)]
    stderr_output_lock: tokio::sync::Mutex<()>,
    #[serde(skip)]
    stdout_error: Option<String>,
    #[serde(skip)]
    stderr_error: Option<String>,
}

impl Default for Replayer {
    fn default() -> Self {
        Self {
            data: PathBuf::new(),
            stdout: None,
            stderr: None,
            stdout_output_lock: tokio::sync::Mutex::new(()),
            stderr_output_lock: tokio::sync::Mutex::new(()),
            stdout_error: None,
            stderr_error: None,
        }
    }
}

#[reverie::tool]
impl Tool for Replayer {
    type GlobalState = detcore::GlobalState;
    type ThreadState = ReplayerThreadState;

    fn new(pid: Pid, cfg: &<Self::GlobalState as GlobalTool>::Config) -> Self {
        let (stdout, stdout_error) = capture_guest_fd(pid, libc::STDOUT_FILENO);
        let (stderr, stderr_error) = capture_guest_fd(pid, libc::STDERR_FILENO);
        Self {
            data: cfg.replay_data.as_ref().unwrap().clone(),
            stdout,
            stderr,
            stdout_output_lock: tokio::sync::Mutex::new(()),
            stderr_output_lock: tokio::sync::Mutex::new(()),
            stdout_error,
            stderr_error,
        }
    }

    fn init_thread_state(
        &self,
        child: Tid,
        parent: Option<(Tid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        let stream_id = match parent {
            None => EventStreamId::root(),
            Some((_, state)) => state.child_stream_ids.next(&state.stream_id),
        };
        // We have to unwrap because there is no way to handle errors here.
        ReplayerThreadState {
            events: EventReader::open(&self.data, &stream_id).unwrap_or_else(|err| {
                panic!(
                    "Failed to open {:?} for replay thread {} (physical {}): {}",
                    self.data, stream_id, child, err
                )
            }),
            stream_id,
            child_stream_ids: ChildEventStreamIds::default(),
            bootstrapped: parent.is_some_and(|(_, state)| state.bootstrapped),
        }
    }

    fn subscriptions(config: &<Self::GlobalState as GlobalTool>::Config) -> Subscription {
        // Subscribe to the exact same events as the recorder does.
        crate::recorder::Recorder::subscriptions(config)
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        self.expect_syscall(guest, syscall);

        // NOTE: This match statement should be identical to the one in the
        // recorder. Otherwise, our recorder and replayer will disagree about
        // how syscalls should be handled.
        //
        // FIXME: Figure out a way to avoid duplicate code. (Merge record/replay
        // into a single tool?)
        Ok(match syscall {
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Execve(_) | Syscall::Execveat(_) => self.handle_exec(guest, syscall).await,
            Syscall::Brk(_) => self.let_through(guest, syscall).await,
            Syscall::Mprotect(_) => self.let_through(guest, syscall).await,
            Syscall::ArchPrctl(_) => {
                // To properly handle arch_prctl, we should prevent calls from
                // using ARCH_SET_CPUID since we already do that for the
                // tracees. However, it is rare for programs to use
                // ARCH_SET_CPUID. For all other arch_prctl subfunctions, we
                // should let it through.
                self.let_through(guest, syscall).await
            }
            Syscall::Read(syscall) => Ok(self.handle_read(guest, syscall).await?),
            Syscall::Pread64(syscall) => self.handle_pread64(guest, syscall).await,
            Syscall::Readv(syscall) => Ok(self
                .handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.len(),
                    syscall.into(),
                )
                .await?),
            Syscall::Preadv(syscall) => Ok(self
                .handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len(),
                    syscall.into(),
                )
                .await?),
            Syscall::Preadv2(syscall) => Ok(self
                .handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len() as usize,
                    syscall.into(),
                )
                .await?),
            Syscall::Recvfrom(syscall) => self.handle_recvfrom(guest, syscall).await,
            Syscall::Recvmsg(syscall) => self.handle_recvmsg(guest, syscall).await,
            Syscall::Write(syscall) => Ok(self.handle_write_family(guest, syscall.into()).await?),
            Syscall::Pwrite64(syscall) => {
                Ok(self.handle_write_family(guest, syscall.into()).await?)
            }
            Syscall::Writev(syscall) => Ok(self.handle_write_family(guest, syscall.into()).await?),
            Syscall::Pwritev(syscall) => {
                Ok(self.handle_write_family(guest, syscall.into()).await?)
            }
            Syscall::Pwritev2(syscall) => {
                Ok(self.handle_write_family(guest, syscall.into()).await?)
            }
            Syscall::Access(_) => self.handle_simple(guest, syscall).await,
            Syscall::Lseek(_) => self.handle_optional_fd_position(guest, syscall).await,
            Syscall::Stat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Fstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Lstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Newfstatat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Statfs(syscall) => self.handle_statfs(guest, syscall.buf()).await,
            Syscall::Fstatfs(syscall) => self.handle_statfs(guest, syscall.buf()).await,
            Syscall::Statx(syscall) => self.handle_statx(guest, syscall).await,
            Syscall::Getdents(syscall) => self.handle_getdents(guest, syscall).await,
            Syscall::Getdents64(syscall) => self.handle_getdents64(guest, syscall).await,
            Syscall::Mmap(syscall) => self.handle_mmap(guest, syscall).await,
            Syscall::Munmap(_) => self.let_through(guest, syscall).await,
            Syscall::Open(_) | Syscall::Openat(_) => {
                self.handle_virtual_fd_create(guest, syscall).await
            }
            Syscall::Openat2(call) => self.handle_openat2(guest, call).await,
            Syscall::Close(_) => self.handle_close(guest, syscall).await,
            Syscall::Fchdir(call) => self.handle_fchdir(guest, call).await,
            Syscall::Fadvise64(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#2373)
            Syscall::Flock(call) => self.handle_flock(guest, call).await,
            Syscall::Ftruncate(syscall) => Ok(self.handle_ftruncate(guest, syscall).await?),
            Syscall::Dup(_) => {
                self.handle_replayed_side_effect(guest, syscall, "dup")
                    .await
            }
            Syscall::Dup2(_) => self.handle_dup2(guest, syscall).await,
            Syscall::Dup3(_) => {
                self.handle_replayed_side_effect(guest, syscall, "dup3")
                    .await
            }
            Syscall::Ioctl(syscall) => self.handle_ioctl(guest, syscall).await,
            Syscall::Socket(_) => {
                self.handle_replayed_side_effect(guest, syscall, "socket")
                    .await
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-979): recreate and validate the pidfd on
            // replay. pidfd_open returns a real kernel fd that later modeled
            // descriptor ops (fcntl/poll/waitid/close) act on, so we re-inject
            // it and assert the fd matches the recorded value, catching
            // fd-allocation or target-lifetime drift rather than blindly
            // substituting the recorded number.
            Syscall::PidfdOpen(_) => {
                self.handle_replayed_side_effect(guest, syscall, "pidfd_open")
                    .await
            }
            Syscall::ClockGettime(syscall) => self.handle_clock_gettime(guest, syscall).await,
            Syscall::Gettimeofday(syscall) => self.handle_gettimeofday(guest, syscall).await,
            Syscall::Settimeofday(_) => self.handle_simple(guest, syscall).await,
            Syscall::Time(syscall) => self.handle_time(guest, syscall).await,
            Syscall::Setsockopt(_) => {
                self.handle_replayed_side_effect(guest, syscall, "setsockopt")
                    .await
            }
            Syscall::Fcntl(call)
                if matches!(
                    call.cmd(),
                    FcntlCmd::F_DUPFD(_) | FcntlCmd::F_DUPFD_CLOEXEC(_)
                ) =>
            {
                self.handle_replayed_side_effect(guest, syscall, "fcntl_dupfd")
                    .await
            }
            Syscall::Fcntl(call) if matches!(call.cmd(), FcntlCmd::F_SETFD(_)) => {
                self.handle_replayed_side_effect(guest, syscall, "fcntl_setfd")
                    .await
            }
            Syscall::Fcntl(_) => self.handle_simple(guest, syscall).await,
            Syscall::Connect(_) => self.handle_simple(guest, syscall).await,
            Syscall::Sendto(_) => self.handle_simple(guest, syscall).await,
            Syscall::Sendmsg(_) => self.handle_simple(guest, syscall).await,
            Syscall::Poll(syscall) => self.handle_poll(guest, syscall).await,
            Syscall::Ppoll(syscall) => self.handle_ppoll(guest, syscall).await,
            Syscall::EpollWait(syscall) => self.handle_epoll_wait(guest, syscall).await,
            Syscall::Getsockopt(syscall) => self.handle_sockopt_family(guest, syscall.into()).await,
            Syscall::Getpeername(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getsockname(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getrandom(syscall) => self.handle_getrandom(guest, syscall).await,
            Syscall::Readlink(syscall) => self.handle_readlink(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Mkdir(_) => self.handle_mkdir(guest, syscall, false).await,
            Syscall::Unlink(_) => self.handle_optional_path_removal(guest, syscall).await,
            Syscall::Unlinkat(call) => self.handle_unlinkat(guest, call).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Mkdirat(_) => self.handle_mkdir(guest, syscall, true).await,
            Syscall::Mknodat(_)
            | Syscall::Fchownat(_)
            | Syscall::Linkat(_)
            | Syscall::Renameat(_)
            | Syscall::Renameat2(_)
            | Syscall::Symlinkat(_)
            | Syscall::Fchmodat(_)
            | Syscall::Utimensat(_) => self.handle_confined_path_mutation(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Other(Sysno::close_range, _) => self.handle_close_range(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#2407): pidfd_getfd is raw in the pinned
            // Reverie. Replay must materialize its real OFD alias and validate
            // both successful fd allocation and failure errno.
            Syscall::Other(Sysno::pidfd_getfd, _) => self.handle_pidfd_getfd(guest, syscall).await,
            unsupported => return Ok(guest.inject_with_retry(unsupported).await?),
        }?)
    }

    // TODO-HUMAN-REVIEW(#2370)
    async fn handle_post_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        guest.thread_state_mut().bootstrapped = true;
        Ok(())
    }

    async fn handle_rdtsc_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        _request: Rdtsc,
    ) -> Result<RdtscResult, Errno> {
        next_event!(guest, Rdtsc)
    }
}

impl Replayer {
    // TODO-HUMAN-REVIEW(#2370)
    async fn handle_exec<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let event = match next_event!(guest, Exec) {
            Ok(event) => event,
            Err(error) => return Err(error),
        };
        let actual_request = self
            .read_exec_request(guest, syscall)
            .unwrap_or_else(|error| {
                panic!("could not read replay exec request after recorded success: {error}")
            });
        assert_eq!(
            actual_request.dirfd, event.request.dirfd,
            "replay exec directory context differs from recording"
        );
        assert_eq!(
            actual_request.flags, event.request.flags,
            "replay exec flags differ from recording"
        );
        let bootstrap_path = if actual_request.path == event.request.path {
            None
        } else {
            let valid_bootstrap_substitution = guest.is_root_thread()
                && !guest.thread_state().bootstrapped
                && matches!(
                    &event.target,
                    crate::event::ExecTarget::Materialize(materialization)
                        if matches!(
                            materialization.base,
                            crate::event::ExecMaterializationBase::Root
                        ) && materialization.path == event.request.path
                );
            assert!(
                valid_bootstrap_substitution,
                "replay exec pathname differs from recording outside the root bootstrap"
            );
            assert!(
                actual_request.path.len() >= event.request.path.len(),
                "private replay bootstrap pathname is shorter than the recorded pathname"
            );
            Some(event.request.path.clone())
        };

        let root = crate::record_replay_path::open_process_root(guest.pid())
            .unwrap_or_else(|error| panic!("could not pin replay root for exec: {error}"));
        for dependency in &event.dependencies {
            let path = PathBuf::from(std::ffi::OsString::from_vec(dependency.path.clone()));
            let start = match dependency.base {
                crate::event::ExecMaterializationBase::Root => {
                    crate::record_replay_path::open_process_root(guest.pid())
                }
                crate::event::ExecMaterializationBase::Cwd => {
                    crate::record_replay_path::open_process_cwd(guest.pid())
                }
                crate::event::ExecMaterializationBase::DirectoryFd(fd) => {
                    crate::record_replay_path::open_process_directory_fd(guest.pid(), fd)
                }
            }
            .unwrap_or_else(|error| panic!("could not pin replay exec dependency base: {error}"));
            let mut snapshot = self
                .open_exec_snapshot(&dependency.image)
                .unwrap_or_else(|error| {
                    panic!("could not open recorded exec dependency snapshot: {error}")
                });
            crate::record_replay_path::materialize_regular_file(
                &root,
                &start,
                &path,
                &dependency.symlinks,
                &mut snapshot,
                dependency.image.digest,
                dependency.image.mode,
            )
            .unwrap_or_else(|error| {
                panic!("could not materialize recorded exec dependency {path:?}: {error}")
            });
            remember_materialized_path(&root, &start, &path).unwrap_or_else(|error| {
                panic!("could not register recorded exec dependency {path:?}: {error}")
            });
        }

        if let crate::event::ExecTarget::Materialize(materialization) = &event.target {
            let path = PathBuf::from(std::ffi::OsString::from_vec(materialization.path.clone()));
            let start = match materialization.base {
                crate::event::ExecMaterializationBase::Root => {
                    crate::record_replay_path::open_process_root(guest.pid())
                }
                crate::event::ExecMaterializationBase::Cwd => {
                    crate::record_replay_path::open_process_cwd(guest.pid())
                }
                crate::event::ExecMaterializationBase::DirectoryFd(fd) => {
                    crate::record_replay_path::open_process_directory_fd(guest.pid(), fd)
                }
            }
            .unwrap_or_else(|error| {
                panic!("could not pin replay exec materialization base: {error}")
            });
            let mut snapshot = self
                .open_exec_snapshot(&event.executable)
                .unwrap_or_else(|error| panic!("could not open recorded exec snapshot: {error}"));
            crate::record_replay_path::materialize_regular_file(
                &root,
                &start,
                &path,
                &materialization.symlinks,
                &mut snapshot,
                event.executable.digest,
                event.executable.mode,
            )
            .and_then(|()| remember_materialized_path(&root, &start, &path))
            .unwrap_or_else(|error| {
                panic!("could not materialize recorded exec target {path:?}: {error}")
            });
        }

        if let crate::event::ExecTarget::RestoreDescriptor(descriptor) = &event.target {
            self.restore_exec_descriptor(guest, descriptor, &event.executable)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "could not restore recorded executable descriptor {}: {error}",
                        descriptor.target_fd
                    )
                });
        }

        self.verify_live_exec_target(guest, syscall, &event.executable)
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "replay exec target {:?} (dirfd {}, flags {:#x}) does not match recorded image: {error}",
                    std::ffi::OsString::from_vec(event.request.path.clone()),
                    event.request.dirfd,
                    event.request.flags,
                )
            });

        if let Some(path) = bootstrap_path {
            return self
                .inject_successful_exec_at_path(guest, syscall, &path)
                .await;
        }

        match guest.inject(syscall).await {
            Err(error) => panic!(
                "replayed exec returned {error} after recording successfully replaced the image"
            ),
            Ok(value) => panic!(
                "replayed exec returned {value} after recording successfully replaced the image"
            ),
        }
    }

    async fn inject_successful_exec_at_path<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        path: &[u8],
    ) -> Result<i64, Errno> {
        let address = match syscall {
            Syscall::Execve(call) => <Option<PathPtr<'_>> as FromToRaw>::into_raw(call.path()),
            Syscall::Execveat(call) => <Option<PathPtr<'_>> as FromToRaw>::into_raw(call.path()),
            _ => unreachable!("bootstrap path rewrite requested for {syscall:?}"),
        };
        assert_ne!(
            address, 0,
            "successful bootstrap exec has a non-null pathname"
        );
        let mut replacement = path.to_vec();
        replacement.push(0);
        guest.memory().write_exact(
            AddrMut::<u8>::from_raw(address).expect("bootstrap pathname address is non-null"),
            &replacement,
        )?;
        match guest.inject(syscall).await {
            Err(error) => panic!(
                "replayed bootstrap exec returned {error} after recording successfully replaced the image"
            ),
            Ok(value) => panic!(
                "replayed bootstrap exec returned {value} after recording successfully replaced the image"
            ),
        }
    }

    // TODO-HUMAN-REVIEW(#2370)
    async fn preserve_materialized_exec_descriptor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        descriptor: &crate::event::ExecDescriptor,
        image: &crate::event::ExecImage,
    ) -> io::Result<bool> {
        let target_path = format!("/proc/{}/fd/{}", guest.pid().as_raw(), descriptor.target_fd);
        let target_metadata = match std::fs::metadata(&target_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !target_metadata.file_type().is_file()
            || !materialized_file_is_registered(guest.pid(), &target_metadata)
        {
            return Ok(false);
        }

        let mut actual_status = guest
            .inject_with_retry(
                Fcntl::new()
                    .with_fd(descriptor.target_fd)
                    .with_cmd(FcntlCmd::F_GETFL),
            )
            .await
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
            as libc::c_int;
        const SETTABLE_STATUS_FLAGS: libc::c_int =
            libc::O_APPEND | libc::O_ASYNC | libc::O_DIRECT | libc::O_NOATIME | libc::O_NONBLOCK;
        if actual_status & !SETTABLE_STATUS_FLAGS
            != descriptor.status_flags & !SETTABLE_STATUS_FLAGS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "materialized exec descriptor immutable status flags differ: expected {:#x}, found {actual_status:#x}",
                    descriptor.status_flags
                ),
            ));
        }
        if actual_status != descriptor.status_flags {
            let normalized = (actual_status & !SETTABLE_STATUS_FLAGS)
                | (descriptor.status_flags & SETTABLE_STATUS_FLAGS);
            let result = guest
                .inject_with_retry(
                    Fcntl::new()
                        .with_fd(descriptor.target_fd)
                        .with_cmd(FcntlCmd::F_SETFL(normalized)),
                )
                .await;
            if result != Ok(0) {
                return Err(io::Error::other(format!(
                    "could not restore materialized exec descriptor status flags: {result:?}"
                )));
            }
            actual_status = guest
                .inject_with_retry(
                    Fcntl::new()
                        .with_fd(descriptor.target_fd)
                        .with_cmd(FcntlCmd::F_GETFL),
                )
                .await
                .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
                as libc::c_int;
            if actual_status != descriptor.status_flags {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "materialized exec descriptor status flags differ after F_SETFL: expected {:#x}, found {actual_status:#x}",
                        descriptor.status_flags
                    ),
                ));
            }
        }

        let actual_offset = guest
            .inject_with_retry(
                Lseek::new()
                    .with_fd(descriptor.target_fd)
                    .with_offset(0)
                    .with_whence(Whence::SEEK_CUR),
            )
            .await;
        match (descriptor.offset, actual_offset) {
            (Some(expected), Ok(actual)) if actual == expected => {}
            (Some(expected), Ok(actual)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "materialized exec descriptor offset differs: expected {expected}, found {actual}"
                    ),
                ));
            }
            (Some(_), Err(error)) => {
                return Err(io::Error::from_raw_os_error(error.into_raw()));
            }
            (None, Err(Errno::EBADF | Errno::ESPIPE)) => {}
            (None, Err(error)) => {
                return Err(io::Error::from_raw_os_error(error.into_raw()));
            }
            (None, Ok(actual)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "materialized exec descriptor unexpectedly has seekable offset {actual}"
                    ),
                ));
            }
        }

        let pinned = crate::record_replay_path::open_process_fd(guest.pid(), descriptor.target_fd)?;
        let identity = crate::record_replay_path::file_identity(pinned.as_raw_fd())?;
        if !crate::record_replay_path::is_regular_file(identity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "materialized exec descriptor is not a regular file",
            ));
        }
        if identity.mode & 0o7777 != image.mode {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "materialized exec descriptor mode differs: expected {:o}, found {:o}",
                    image.mode,
                    identity.mode & 0o7777
                ),
            ));
        }
        let actual_digest = detcore::Digest::digest_reader(
            crate::record_replay_path::open_readable_fd(pinned.as_raw_fd())?,
        )?;
        if actual_digest != image.digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "materialized exec descriptor digest differs: expected {}, found {}",
                    image.digest, actual_digest
                ),
            ));
        }

        let mut expected_aliases = descriptor
            .aliases
            .iter()
            .map(|alias| (alias.fd, alias.descriptor_flags))
            .collect::<Vec<_>>();
        expected_aliases.sort_unstable();
        let original_count = expected_aliases.len();
        expected_aliases.dedup_by_key(|alias| alias.0);
        if expected_aliases.len() != original_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recorded exec descriptor contains duplicate aliases",
            ));
        }

        let target_identity = ReplayFileIdentity::from_metadata(&target_metadata);
        let mut actual_aliases = Vec::new();
        for entry in std::fs::read_dir(format!("/proc/{}/fd", guest.pid().as_raw()))? {
            let entry = entry?;
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::c_int>().ok())
            else {
                continue;
            };
            let metadata = match std::fs::metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if ReplayFileIdentity::from_metadata(&metadata) != target_identity {
                continue;
            }
            if same_guest_open_file_description(guest.pid(), descriptor.target_fd, fd)? {
                let descriptor_flags = guest
                    .inject_with_retry(Fcntl::new().with_fd(fd).with_cmd(FcntlCmd::F_GETFD))
                    .await
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
                    as libc::c_int;
                actual_aliases.push((fd, descriptor_flags));
            }
        }
        actual_aliases.sort_unstable();
        if actual_aliases != expected_aliases {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "materialized exec descriptor aliases differ: expected {expected_aliases:?}, found {actual_aliases:?}"
                ),
            ));
        }

        Ok(true)
    }

    // TODO-HUMAN-REVIEW(#2370)
    async fn restore_exec_descriptor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        descriptor: &crate::event::ExecDescriptor,
        image: &crate::event::ExecImage,
    ) -> io::Result<()> {
        if descriptor.aliases.is_empty()
            || !descriptor
                .aliases
                .iter()
                .any(|alias| alias.fd == descriptor.target_fd)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recorded exec descriptor has no target alias",
            ));
        }
        for alias in &descriptor.aliases {
            let actual = guest
                .inject_with_retry(Fcntl::new().with_fd(alias.fd).with_cmd(FcntlCmd::F_GETFD))
                .await
                .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
                as libc::c_int;
            if actual != alias.descriptor_flags {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "replay descriptor {} flags differ before exec restore: expected {:#x}, found {actual:#x}",
                        alias.fd, alias.descriptor_flags
                    ),
                ));
            }
        }
        if self
            .preserve_materialized_exec_descriptor(guest, descriptor, image)
            .await?
        {
            return Ok(());
        }

        let root = crate::record_replay_path::open_process_root(guest.pid())?;
        let mut snapshot = self.open_exec_snapshot(image)?;
        let private_name = crate::record_replay_path::create_root_temporary_regular_file(
            &root,
            &mut snapshot,
            image.digest,
            image.mode,
        )?;
        let private_path = Path::new("/").join(&private_name);
        let path_bytes = private_path.as_os_str().as_bytes();
        if path_bytes.len() + 1 > 128 {
            let _ = crate::record_replay_path::unlink_root_entry(&root, &private_name);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "temporary executable pathname is too long",
            ));
        }
        let mut path = [0_u8; 128];
        path[..path_bytes.len()].copy_from_slice(path_bytes);
        let mut stack = guest.stack().await;
        let path_addr = stack.push(path);
        let path_ptr =
            PathPtr::from_ptr(unsafe { path_addr.cast::<u8>().as_ptr().cast::<libc::c_char>() })
                .expect("guest stack address is non-null");
        let _path_guard = stack
            .commit()
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
        let reopen_flags = OFlag::from_bits_truncate(descriptor.status_flags) | OFlag::O_CLOEXEC;
        let source_fd = match guest
            .inject_with_retry(
                Openat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(Some(path_ptr))
                    .with_flags(reopen_flags),
            )
            .await
        {
            Ok(fd) => fd as libc::c_int,
            Err(error) => {
                let _ = crate::record_replay_path::unlink_root_entry(&root, &private_name);
                return Err(io::Error::from_raw_os_error(error.into_raw()));
            }
        };
        drop(_path_guard);
        crate::record_replay_path::unlink_root_entry(&root, &private_name)?;

        let result = async {
            let actual_status = guest
                .inject_with_retry(Fcntl::new().with_fd(source_fd).with_cmd(FcntlCmd::F_GETFL))
                .await
                .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
                as libc::c_int;
            if actual_status != descriptor.status_flags {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "restored exec descriptor status flags differ: expected {:#x}, found {actual_status:#x}",
                        descriptor.status_flags
                    ),
                ));
            }
            if let Some(offset) = descriptor.offset {
                let actual = guest
                    .inject_with_retry(
                        Lseek::new()
                            .with_fd(source_fd)
                            .with_offset(offset)
                            .with_whence(Whence::SEEK_SET),
                    )
                    .await
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
                if actual != offset {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "restored exec descriptor offset differs: expected {offset}, found {actual}"
                        ),
                    ));
                }
            }

            for alias in &descriptor.aliases {
                let flags = if alias.descriptor_flags & libc::FD_CLOEXEC != 0 {
                    OFlag::O_CLOEXEC
                } else {
                    OFlag::empty()
                };
                let actual = guest
                    .inject_with_retry(
                        Dup3::new()
                            .with_oldfd(source_fd)
                            .with_newfd(alias.fd)
                            .with_flags(flags),
                    )
                    .await
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
                if actual != i64::from(alias.fd) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "restored exec descriptor alias returned {actual}, expected {}",
                            alias.fd
                        ),
                    ));
                }
                remember_materialized_file(guest.pid(), alias.fd);
            }
            Ok(())
        }
        .await;
        let close_source = guest
            .inject_with_retry(Close::new().with_fd(source_fd))
            .await;
        if close_source != Ok(0) && result.is_ok() {
            return Err(io::Error::other(format!(
                "could not close executable staging descriptor: {close_source:?}"
            )));
        }
        result
    }

    fn read_exec_request<G: Guest<Self>>(
        &self,
        guest: &G,
        syscall: Syscall,
    ) -> io::Result<crate::event::ExecRequest> {
        let call = match syscall {
            Syscall::Execve(call) => call.into(),
            Syscall::Execveat(call) => call,
            _ => unreachable!("exec request read for {syscall:?}"),
        };
        let path = call
            .path()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?
            .read(&guest.memory())
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
        Ok(crate::event::ExecRequest {
            dirfd: call.dirfd(),
            path: path.as_os_str().as_bytes().to_vec(),
            flags: call.flags(),
        })
    }

    fn open_exec_snapshot(&self, image: &crate::event::ExecImage) -> io::Result<File> {
        let path = self
            .data
            .join(crate::consts::EXEC_FILES_NAME)
            .join(image.digest.to_string());
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("recorded exec snapshot is not a regular file: {path:?}"),
            ));
        }
        let mut file = File::open(&path)?;
        let actual = detcore::Digest::digest_reader(&mut file)?;
        if actual != image.digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "recorded exec snapshot digest mismatch at {path:?}: expected {}, found {}",
                    image.digest, actual
                ),
            ));
        }
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    }

    async fn verify_live_exec_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        expected: &crate::event::ExecImage,
    ) -> io::Result<()> {
        let call = match syscall {
            Syscall::Execve(call) => call.into(),
            Syscall::Execveat(call) => call,
            _ => unreachable!("exec target verification for {syscall:?}"),
        };
        let path_ptr = call
            .path()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?;
        let path = path_ptr
            .read(&guest.memory())
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
        let pinned = if path.as_os_str().is_empty() {
            if call.flags() & libc::AT_EMPTY_PATH == 0 {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            crate::record_replay_path::open_process_fd(guest.pid(), call.dirfd())?
        } else {
            let mut flags = OFlag::O_PATH | OFlag::O_CLOEXEC;
            if call.flags() & libc::AT_SYMLINK_NOFOLLOW != 0 {
                flags |= OFlag::O_NOFOLLOW;
            }
            let temporary = guest
                .inject_with_retry(
                    reverie::syscalls::Openat::new()
                        .with_dirfd(call.dirfd())
                        .with_path(Some(path_ptr))
                        .with_flags(flags),
                )
                .await
                .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
            let pinned =
                crate::record_replay_path::open_process_fd(guest.pid(), temporary as libc::c_int);
            let close = guest
                .inject_with_retry(Close::new().with_fd(temporary as libc::c_int))
                .await;
            if close != Ok(0) {
                return Err(io::Error::other(format!(
                    "could not close temporary replay exec descriptor: {close:?}"
                )));
            }
            pinned?
        };
        let identity = crate::record_replay_path::file_identity(pinned.as_raw_fd())?;
        if !crate::record_replay_path::is_regular_file(identity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "replay exec target is not a regular file",
            ));
        }
        if identity.mode & 0o7777 != expected.mode {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "replay exec mode differs: expected {:o}, found {:o}",
                    expected.mode,
                    identity.mode & 0o7777
                ),
            ));
        }
        let actual = detcore::Digest::digest_reader(crate::record_replay_path::open_readable_fd(
            pinned.as_raw_fd(),
        )?)?;
        if actual != expected.digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "replay exec digest differs: expected {}, found {}",
                    expected.digest, actual
                ),
            ));
        }
        Ok(())
    }

    pub(super) async fn reserve_replay_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        cloexec: bool,
    ) {
        let flags = if cloexec {
            EfdFlags::EFD_CLOEXEC
        } else {
            EfdFlags::empty()
        };
        let placeholder = guest
            .inject_with_retry(Eventfd2::new().with_count(0).with_flags(flags))
            .await
            .unwrap_or_else(|error| {
                panic!("could not reserve replay FD {fd} with an eventfd: {error}")
            });
        if placeholder != i64::from(fd) {
            let _ = guest.inject(Close::new().with_fd(placeholder as i32)).await;
            panic!(
                "replay FD namespace diverged: expected slot {fd}, placeholder returned {placeholder}"
            );
        }
    }
    pub(super) fn fd_is_in_replay_root(&self, pid: Pid, fd: libc::c_int) -> bool {
        let path = format!("/proc/{}/fd/{fd}", pid.as_raw());
        let Ok(metadata) = std::fs::metadata(&path) else {
            return false;
        };
        if metadata.file_type().is_file() {
            return materialized_file_is_registered(pid, &metadata);
        }
        if !metadata.file_type().is_dir() {
            return false;
        }
        let Some(root) = replay_root(pid) else {
            return false;
        };
        std::fs::canonicalize(path).is_ok_and(|path| path.starts_with(root))
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#662): Audit replay path confinement.
    fn open_request<G: Guest<Self>>(
        &self,
        guest: &G,
        syscall: Syscall,
    ) -> io::Result<(libc::c_int, PathBuf, OFlag)> {
        let request = match syscall {
            Syscall::Open(call) => (
                libc::AT_FDCWD,
                call.path()
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?
                    .read(&guest.memory())
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?,
                call.flags(),
            ),
            Syscall::Openat(call) => (
                call.dirfd(),
                call.path()
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?
                    .read(&guest.memory())
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?,
                call.flags(),
            ),
            _ => unreachable!("open replay path requested for {syscall:?}"),
        };
        Ok(request)
    }

    fn open_path_in_replay_root<G: Guest<Self>>(
        &self,
        guest: &G,
        dirfd: libc::c_int,
        path: &Path,
    ) -> Option<PathBuf> {
        let root = replay_root(guest.pid())?;

        let candidate = if let Ok(relative) = path.strip_prefix(Path::new("/")) {
            root.join(relative)
        } else {
            let base = if dirfd == libc::AT_FDCWD {
                format!("/proc/{}/cwd", guest.pid().as_raw())
            } else {
                format!("/proc/{}/fd/{dirfd}", guest.pid().as_raw())
            };
            let base = std::fs::canonicalize(base).ok()?;
            if !base.starts_with(&root) {
                return None;
            }
            base.join(path)
        };

        let resolved = match std::fs::canonicalize(&candidate) {
            Ok(path) => path,
            Err(_) => {
                let parent = std::fs::canonicalize(candidate.parent()?).ok()?;
                parent.join(candidate.file_name()?)
            }
        };
        resolved.starts_with(&root).then_some(resolved)
    }

    fn materialize_recorded_directory<G: Guest<Self>>(
        &self,
        guest: &G,
        syscall: Syscall,
        dirfd: libc::c_int,
        path: &Path,
        flags: OFlag,
    ) -> io::Result<bool> {
        let root = crate::record_replay_path::open_process_root(guest.pid())?;
        let start = if path.is_absolute() {
            crate::record_replay_path::open_process_root(guest.pid())?
        } else if dirfd == libc::AT_FDCWD {
            crate::record_replay_path::open_process_cwd(guest.pid())?
        } else {
            let start =
                match crate::record_replay_path::open_process_directory_fd(guest.pid(), dirfd) {
                    Ok(start) => start,
                    Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
                        tracing::debug!(
                            ?syscall,
                            "recorded open directory base is a virtual replay descriptor"
                        );
                        return Ok(false);
                    }
                    Err(error) => return Err(error),
                };
            match crate::record_replay_path::directory_is_beneath(&root, &start)? {
                true => {}
                false => {
                    tracing::debug!(
                        ?syscall,
                        "recorded open directory base is not confined to replay root"
                    );
                    return Ok(false);
                }
            }
            start
        };
        if flags.contains(OFlag::O_NOFOLLOW) {
            crate::record_replay_path::ensure_directory_path(&root, &start, path)?;
        } else {
            crate::record_replay_path::ensure_directory_path_follow_final(&root, &start, path)?;
        }
        Ok(true)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#662): Audit replay-root file and directory materialization.
    async fn handle_virtual_fd_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Open)?;
        let recorded = event.result;
        if let Ok(fd) = recorded {
            let (dirfd, path, flags) = self.open_request(guest, syscall).unwrap_or_else(|error| {
                panic!("could not decode successful recorded open: {error}")
            });
            let candidate = (event.materialize == OpenMaterialization::RegularFile)
                .then(|| self.open_path_in_replay_root(guest, dirfd, &path))
                .flatten();
            if let Some(candidate) = &candidate
                && flags.contains(OFlag::O_CREAT)
                && let Some(parent) = candidate.parent()
            {
                let _ = std::fs::create_dir_all(parent);
            }
            let materialize = if event.materialize == OpenMaterialization::Directory {
                self.materialize_recorded_directory(guest, syscall, dirfd, &path, flags)
                    .unwrap_or_else(|error| {
                        panic!("failed to materialize recorded open directory: {error}")
                    })
            } else {
                candidate.as_ref().is_some_and(|candidate| {
                    if flags.contains(OFlag::O_TMPFILE) {
                        return candidate.is_dir();
                    }
                    match (event.materialize, std::fs::metadata(candidate)) {
                        (OpenMaterialization::RegularFile, Ok(metadata))
                            if metadata.file_type().is_file() =>
                        {
                            materialized_file_is_registered(guest.pid(), &metadata)
                        }
                        (OpenMaterialization::RegularFile, Err(error))
                            if error.kind() == std::io::ErrorKind::NotFound
                                && flags.contains(OFlag::O_CREAT) =>
                        {
                            true
                        }
                        _ => false,
                    }
                })
            };
            if materialize {
                match guest.inject_with_retry(syscall).await {
                    Ok(actual) => {
                        assert_eq!(
                            actual, fd,
                            "replay materialized open returned a different descriptor"
                        );
                        remember_materialized_file(guest.pid(), actual as libc::c_int);
                    }
                    Err(error) => {
                        tracing::debug!(
                            ?syscall,
                            %error,
                            "replay path unavailable; reserving a virtual descriptor"
                        );
                        self.reserve_replay_fd(guest, fd as i32, flags.contains(OFlag::O_CLOEXEC))
                            .await;
                    }
                }
            } else {
                self.reserve_replay_fd(guest, fd as i32, flags.contains(OFlag::O_CLOEXEC))
                    .await;
            }
        }
        recorded
    }

    fn mkdir_request<G: Guest<Self>>(
        &self,
        guest: &G,
        syscall: Syscall,
    ) -> io::Result<(libc::c_int, PathBuf)> {
        let (dirfd, path) = match syscall {
            Syscall::Mkdir(call) => (
                libc::AT_FDCWD,
                call.path()
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?
                    .read(&guest.memory())
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?,
            ),
            Syscall::Mkdirat(call) => (
                call.dirfd(),
                call.path()
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?
                    .read(&guest.memory())
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?,
            ),
            _ => unreachable!("mkdir replay path requested for {syscall:?}"),
        };
        Ok((dirfd, path))
    }

    fn materialize_recorded_mkdir<G: Guest<Self>>(
        &self,
        guest: &G,
        syscall: Syscall,
    ) -> io::Result<bool> {
        let (dirfd, path) = self.mkdir_request(guest, syscall)?;
        let root = crate::record_replay_path::open_process_root(guest.pid())?;
        if path.is_absolute() {
            crate::record_replay_path::ensure_directory_path(&root, &root, &path)?;
            return Ok(true);
        }

        let start = if dirfd == libc::AT_FDCWD {
            crate::record_replay_path::open_process_cwd(guest.pid())?
        } else {
            match crate::record_replay_path::open_process_directory_fd(guest.pid(), dirfd) {
                Ok(start) => start,
                Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
                    tracing::debug!(
                        ?syscall,
                        "recorded mkdir directory base is a virtual replay descriptor"
                    );
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
        };
        match crate::record_replay_path::directory_is_beneath(&root, &start)? {
            true => {}
            false => {
                tracing::debug!(
                    ?syscall,
                    "recorded mkdir directory base is not confined to replay root"
                );
                return Ok(false);
            }
        }
        crate::record_replay_path::ensure_directory_path(&root, &start, &path)?;
        Ok(true)
    }

    // TODO-HUMAN-REVIEW(#2370)
    /// Replays successful mkdir side effects exactly as before. For a recorded
    /// `EEXIST` caused by a directory, it reconstructs that directory in the
    /// fresh chroot while still returning the recorded error to the guest.
    async fn handle_mkdir<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        confined_dirfd: bool,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Mkdir)?;
        if event.result == Err(Errno::EEXIST) && event.existing_directory {
            self.materialize_recorded_mkdir(guest, syscall)
                .unwrap_or_else(|error| {
                    panic!("failed to materialize recorded mkdir directory: {error}")
                });
        } else if let Ok(expected) = event.result {
            if !confined_dirfd {
                let actual = guest.inject_with_retry(syscall).await;
                assert_eq!(actual, Ok(expected), "mkdir side effects diverged");
            } else if self.path_mutation_dirfds_are_confined(guest.pid(), syscall) {
                match guest.inject_with_retry(syscall).await {
                    Ok(actual) => assert_eq!(
                        actual, expected,
                        "replayed mkdirat returned a different result"
                    ),
                    Err(error @ (Errno::ENOENT | Errno::ENOTDIR | Errno::EROFS)) => {
                        tracing::debug!(?syscall, %error, "replay mkdirat kept virtual");
                    }
                    Err(error) => {
                        panic!(
                            "replayed mkdirat {syscall:?} failed after recording returned {expected}: {error}"
                        );
                    }
                }
            }
        }
        event.result
    }

    fn dirfd_is_confined(&self, pid: Pid, fd: libc::c_int) -> bool {
        fd == libc::AT_FDCWD || self.fd_is_in_replay_root(pid, fd)
    }

    fn path_mutation_dirfds_are_confined(&self, pid: Pid, syscall: Syscall) -> bool {
        match syscall {
            Syscall::Mkdirat(call) => self.dirfd_is_confined(pid, call.dirfd()),
            Syscall::Mknodat(call) => self.dirfd_is_confined(pid, call.dirfd()),
            Syscall::Fchownat(call) => self.dirfd_is_confined(pid, call.dirfd()),
            Syscall::Fchmodat(call) => self.dirfd_is_confined(pid, call.dirfd()),
            Syscall::Utimensat(call) => self.dirfd_is_confined(pid, call.dirfd()),
            Syscall::Symlinkat(call) => self.dirfd_is_confined(pid, call.newdirfd()),
            Syscall::Linkat(call) => {
                self.dirfd_is_confined(pid, call.olddirfd())
                    && self.dirfd_is_confined(pid, call.newdirfd())
            }
            Syscall::Renameat(call) => {
                self.dirfd_is_confined(pid, call.olddirfd())
                    && self.dirfd_is_confined(pid, call.newdirfd())
            }
            Syscall::Renameat2(call) => {
                self.dirfd_is_confined(pid, call.olddirfd())
                    && self.dirfd_is_confined(pid, call.newdirfd())
            }
            _ => false,
        }
    }

    async fn handle_openat2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: reverie::syscalls::Openat2,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if let Ok(fd) = recorded {
            if self.dirfd_is_confined(guest.pid(), syscall.dirfd()) {
                match guest.inject_with_retry(syscall).await {
                    Ok(actual) => {
                        assert_eq!(actual, fd, "replayed openat2 returned a different fd")
                    }
                    Err(error) => {
                        tracing::debug!(%error, "replay openat2 path unavailable");
                        self.reserve_replay_fd(guest, fd as i32, false).await;
                    }
                }
            } else {
                self.reserve_replay_fd(guest, fd as i32, false).await;
            }
        }
        recorded
    }

    async fn handle_confined_path_mutation<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if let Ok(expected) = recorded
            && self.path_mutation_dirfds_are_confined(guest.pid(), syscall)
        {
            match guest.inject_with_retry(syscall).await {
                Ok(actual) => assert_eq!(
                    actual, expected,
                    "replayed path mutation returned a different result"
                ),
                Err(error @ (Errno::ENOENT | Errno::ENOTDIR | Errno::EROFS)) => {
                    tracing::debug!(?syscall, %error, "replay path mutation kept virtual");
                }
                Err(error) => {
                    panic!(
                        "replayed path mutation {syscall:?} failed after recording returned {expected}: {error}"
                    );
                }
            }
        }
        recorded
    }

    async fn handle_fchdir<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Fchdir,
    ) -> Result<i64, Errno> {
        if self.fd_is_in_replay_root(guest.pid(), syscall.fd()) {
            self.handle_replayed_side_effect(guest, syscall.into(), "fchdir")
                .await
        } else {
            self.handle_simple(guest, syscall.into()).await
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// Replay `flock(2)` by taking the lock again, not by consuming its recorded
    /// return value.
    ///
    /// `handle_simple` is only valid when nothing beyond the return value depends
    /// on a call. `flock` is the opposite case: its entire product is kernel state
    /// that a contender observes -- another process, or any other open file
    /// description in the same lock domain. Consuming the recorded `Return`
    /// without re-issuing the call makes a replayed run a program that merely
    /// *claims* to hold locks, so replaying a lockfile-based workload excludes
    /// nothing and a concurrent writer can corrupt what the recording protected.
    ///
    /// Only a descriptor that materialized in the replay root is a real kernel
    /// descriptor; the others are eventfd placeholders standing in for opens that
    /// replay serves from the log (see `handle_virtual_fd_create`). Locking a
    /// placeholder would lock the wrong object, while replaying only the return
    /// value would falsely claim the lock exists. Replay therefore fails closed
    /// for such a descriptor rather than silently dropping the side effect.
    ///
    /// Unlike `handle_replayed_side_effect`, this re-issues the call even when
    /// the recording FAILED, and that is the load-bearing part. A recorded
    /// `EWOULDBLOCK` is the observable that proves the conflicting lock is
    /// really held; skipping the attempt on a failed record would leave replay
    /// unable to notice that no lock exists at all, because the only calls it
    /// checked would be the ones that succeed whether or not the lock domain is
    /// real. Re-issuing both outcomes makes the guest's own contention pattern
    /// the check: if a replayed run had faked its locks, the contended probe
    /// would come back `Ok` where the recording said `EWOULDBLOCK`.
    ///
    /// Consequence, stated rather than hidden: within the materialized set,
    /// replay now depends on real lock state the same way recording does. If a
    /// conflicting lock exists during replay but not during recording (or the
    /// reverse), the re-issued call disagrees with the recording and this
    /// reports the divergence instead of silently continuing. That is the
    /// intended failure direction: replay is not faithful under a contender the
    /// recording did not have, and saying so beats pretending. The exposure is
    /// narrow in practice, because a pre-existing host file is exactly what
    /// replay does NOT materialize -- the materialized set is dominated by
    /// files the guest itself created inside the replay root.
    async fn handle_flock<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Flock,
    ) -> Result<i64, Errno> {
        assert!(
            self.fd_is_in_replay_root(guest.pid(), syscall.fd()),
            "cannot replay flock side effects for descriptor {} outside the replay root; \
             returning the recorded result would falsely claim a lock was acquired",
            syscall.fd()
        );
        let call = Syscall::from(syscall);
        let recorded = next_event!(guest, Return);
        let actual = guest.inject_with_retry(call).await;
        assert_eq!(
            actual, recorded,
            "flock side effects diverged: replay observed {actual:?} where the recording \
             observed {recorded:?} for {call:?}"
        );
        recorded
    }

    async fn handle_unlinkat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Unlinkat,
    ) -> Result<i64, Errno> {
        if syscall.dirfd() == libc::AT_FDCWD
            || self.fd_is_in_replay_root(guest.pid(), syscall.dirfd())
        {
            self.handle_optional_path_removal(guest, syscall.into())
                .await
        } else {
            self.handle_simple(guest, syscall.into()).await
        }
    }

    async fn handle_optional_path_removal<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if let Ok(expected) = recorded {
            match guest.inject_with_retry(syscall).await {
                Ok(actual) => assert_eq!(
                    actual, expected,
                    "replayed path removal returned a different result"
                ),
                Err(error @ (Errno::ENOENT | Errno::ENOTDIR)) => {
                    tracing::debug!(
                        ?syscall,
                        %error,
                        "replay path unavailable; keeping path removal virtual"
                    );
                }
                Err(error) => {
                    panic!(
                        "replayed path removal {syscall:?} failed after recording returned {expected}: {error}"
                    );
                }
            }
        }
        recorded
    }

    async fn handle_optional_fd_position<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let Syscall::Lseek(call) = syscall else {
            unreachable!("descriptor-position handler received {syscall:?}");
        };
        if !self.fd_is_in_replay_root(guest.pid(), call.fd()) {
            return self.handle_simple(guest, syscall).await;
        }
        if matches!(call.whence(), Whence::SEEK_DATA | Whence::SEEK_HOLE) {
            let recorded = next_event!(guest, Return);
            if let Ok(expected) = recorded {
                let duplicate = crate::fd::duplicate_guest_fd(guest.pid(), call.fd())
                    .unwrap_or_else(|error| {
                        panic!("failed to duplicate replay file for extent seek: {error}")
                    });
                // SAFETY: duplicate owns the same open-file description and the
                // recorded extent seek returned this nonnegative offset.
                let actual =
                    unsafe { libc::lseek(duplicate.as_raw_fd(), expected, libc::SEEK_SET) };
                assert_eq!(
                    actual, expected,
                    "failed to restore replay position after extent seek"
                );
            }
            return recorded;
        }
        let recorded = next_event!(guest, Return);
        if let Ok(expected) = recorded {
            match guest.inject_with_retry(syscall).await {
                Ok(actual) => assert_eq!(
                    actual, expected,
                    "replayed descriptor position returned a different offset"
                ),
                Err(error @ (Errno::EBADF | Errno::ESPIPE)) => {
                    tracing::debug!(
                        ?syscall,
                        %error,
                        "replay descriptor is virtual; keeping position change virtual"
                    );
                }
                Err(error) => {
                    panic!(
                        "replayed descriptor position {syscall:?} failed after recording returned {expected}: {error}"
                    );
                }
            }
        }
        recorded
    }

    /// Replays the recorded result of `close` while preserving its physical FD
    /// namespace effect.
    async fn handle_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);

        // Linux releases a descriptor even when close reports EINTR, EIO,
        // ENOSPC, or EDQUOT. EBADF leaves the namespace unchanged, while
        // ERESTARTSYS means Reverie must restart the injection first.
        if !matches!(recorded, Err(Errno::EBADF | Errno::ERESTARTSYS))
            && let Err(error) = guest.inject(syscall).await
        {
            if error == Errno::EBADF {
                // Some replayed descriptor sources, notably SCM_RIGHTS,
                // currently have no physical peer.
                tracing::debug!(?syscall, "replayed close had no physical descriptor");
            } else {
                tracing::warn!(
                    ?error,
                    "physical close during replay differed from the recorded result"
                );
            }
        }

        recorded
    }

    // Check if we received the expected syscall or not.
    fn expect_syscall<G: Guest<Self>>(&self, guest: &mut G, syscall: Syscall) {
        let thread = guest.tid();
        let next_count = guest.thread_state().count + 1;
        let debug_event = guest
            .thread_state_mut()
            .next_debug_event()
            .unwrap_or_else(|source| {
                panic!(
                    "Replay syscall stream ended unexpectedly for recording {} on thread {} at event {} while the guest executed {:?}: {}",
                    self.data.display(),
                    thread,
                    next_count,
                    syscall,
                    source,
                )
            });

        let actual_event = DebugEvent::new(syscall, &guest.memory());
        let exec_syscall = matches!(syscall, Syscall::Execve(_) | Syscall::Execveat(_));

        // Compare only the argument registers the syscall actually uses. Reverie
        // keeps all six raw registers in every typed syscall and derives
        // `PartialEq` over them, so unused registers (which hold arbitrary
        // leftover guest values) would otherwise produce false desyncs for any
        // syscall with fewer than six arguments.
        if normalize_unused_args(debug_event.syscall()) == normalize_unused_args(syscall)
            && debug_event.exec_request_matches(&actual_event)
        {
            return;
        }

        if guest.is_root_thread() && !guest.thread_state().bootstrapped && exec_syscall {
            // The controller-visible replay bootstrap pathname is deliberately
            // substituted before injection and therefore differs here.
            return;
        }

        let error = DesyncError {
            thread,
            stream_id: guest.thread_state().stream_id.clone(),
            count: guest.thread_state().count,
            actual: actual_event,
            expected: debug_event,
        };
        let summary = error.summary(&self.data, 16, 4).to_string();
        let report = match error.generate_report(&self.data) {
            Ok(report) => format!("Full desynchronization report: {}", report.display()),
            Err(report_error) => {
                format!("Could not write the full desynchronization report: {report_error}")
            }
        };

        panic!(
            "Replay diverged from recording {} on thread {} at syscall event {}. Re-record the workload with the same Hermit build after diagnosing the mismatch.\n{}\n{}",
            self.data.display(),
            thread,
            error.count,
            summary,
            report,
        );
    }

    /// Called for syscalls to explicitly let through. This should only be called
    /// for syscalls that cannot be recorded and are necessary for the program to
    /// function correctly. Examples of syscalls that fall into this category are
    /// ones that help with memory management (e.g., `brk`, `mprotect`, or
    /// `munmap`) or process management (e.g., `fork`, `vfork`, `clone`).
    ///
    /// For these syscalls, we just care about detecting dsynchronization and
    /// simply inject them to let them through.
    async fn let_through<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        // NOTE: Must use `inject_with_retry` here. Otherwise, we may end up
        // introducing non-determinism into the replay and popping multiple
        // syscall events.
        guest.inject_with_retry(syscall).await
    }

    /// Handles a syscall whose only value we care about is the return value
    /// (i.e., simple syscalls).
    ///
    /// For recording, this means we only record the return value of the syscall.
    /// For replay, this means we substitute the return value in lieu of actually
    /// performing the injection.
    ///
    /// The syscall must have two properties satisfied for this to be called:
    ///  1. The syscall must only have "input" arguments. That is, all arguments
    ///     must either be values or const pointers.
    ///  2. The execution of the program must not depend on anything else other
    ///     than the return value of the syscall. For example, `mmap` would violate
    ///     this rule since it affects later memory access.
    ///
    /// There are many syscalls who satisfy these two requirements.
    async fn handle_simple<G: Guest<Self>>(
        &self,
        guest: &mut G,
        _syscall: Syscall,
    ) -> Result<i64, Errno> {
        next_event!(guest, Return)
    }

    async fn handle_dup2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let actual = guest.inject_with_retry(syscall).await;
            // Some source descriptors are virtual: open-family syscalls replay
            // their recorded return value without creating a live kernel fd.
            // Preserve that behavior when there is nothing to duplicate in the
            // replay process.
            if actual != Err(Errno::EBADF) {
                assert_eq!(actual, recorded, "dup2 fd-table mutation diverged");
            }
        }
        recorded
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    async fn handle_replayed_side_effect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        operation: &str,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let actual = guest.inject_with_retry(syscall).await;
            assert_eq!(actual, recorded, "{operation} side effects diverged");
        }
        recorded
    }

    async fn handle_pidfd_getfd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        let actual = guest.inject_with_retry(syscall).await;
        if let Some(fd) = unexpected_pidfd_getfd_fd(&recorded, &actual) {
            let cleanup = guest.inject_with_retry(Close::new().with_fd(fd)).await;
            assert_eq!(
                cleanup,
                Ok(0),
                "pidfd_getfd replay diverged and cleanup of unexpected fd {fd} failed"
            );
        }
        assert_eq!(
            actual, recorded,
            "pidfd_getfd side effects diverged: replay observed {actual:?}, recording observed {recorded:?}"
        );
        recorded
    }

    // TODO-HUMAN-REVIEW(#557): Audit close_range fd-table replay semantics.
    async fn handle_close_range<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let actual = guest.inject_with_retry(syscall).await;
            assert_eq!(actual, recorded, "close_range side effects diverged");
        }
        recorded
    }
}

#[cfg(test)]
mod exec_snapshot_tests {
    use super::*;

    fn replayer_with_snapshot(
        contents: &[u8],
    ) -> (tempfile::TempDir, Replayer, crate::event::ExecImage) {
        let directory = tempfile::tempdir().unwrap();
        let digest = detcore::Digest::new(contents);
        let snapshots = directory.path().join(crate::consts::EXEC_FILES_NAME);
        std::fs::create_dir(&snapshots).unwrap();
        std::fs::write(snapshots.join(digest.to_string()), contents).unwrap();
        let replayer = Replayer {
            data: directory.path().to_path_buf(),
            ..Default::default()
        };
        let image = crate::event::ExecImage {
            digest,
            mode: 0o755,
        };
        (directory, replayer, image)
    }

    #[test]
    fn recorded_exec_snapshot_digest_is_verified() {
        let (directory, replayer, image) = replayer_with_snapshot(b"recorded image");
        std::fs::write(
            directory
                .path()
                .join(crate::consts::EXEC_FILES_NAME)
                .join(image.digest.to_string()),
            b"corrupt image",
        )
        .unwrap();

        let error = replayer.open_exec_snapshot(&image).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("digest mismatch"));
    }

    #[test]
    fn missing_recorded_exec_snapshot_fails_closed() {
        let (directory, replayer, image) = replayer_with_snapshot(b"recorded image");
        std::fs::remove_file(
            directory
                .path()
                .join(crate::consts::EXEC_FILES_NAME)
                .join(image.digest.to_string()),
        )
        .unwrap();

        assert_eq!(
            replayer.open_exec_snapshot(&image).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn materialized_object_pin_survives_unlink_until_scope_drop() {
        let directory = tempfile::tempdir().unwrap();
        let root = crate::record_replay_path::open_directory_path(directory.path()).unwrap();
        let scope = ReplayMaterializationScope::new(root.as_raw_fd()).unwrap();
        let root_identity = identity_for_fd(root.as_raw_fd()).unwrap();
        let path = directory.path().join("image");
        std::fs::write(&path, b"recorded").unwrap();
        let object: OwnedFd = File::open(&path).unwrap().into();
        let identity = identity_for_fd(object.as_raw_fd()).unwrap();
        register_materialized(root_identity, object).unwrap();
        std::fs::remove_file(&path).unwrap();

        let registry = materialized_files()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let pin = registry
            .get(&root_identity)
            .and_then(|files| files.get(&identity))
            .expect("registered object remains pinned after unlink");
        let pinned = std::fs::metadata(format!("/proc/self/fd/{}", pin.as_raw_fd())).unwrap();
        assert_eq!(
            (pinned.dev(), pinned.ino()),
            (identity.device, identity.inode)
        );
        drop(registry);

        std::fs::write(&path, b"replacement").unwrap();
        let replacement = std::fs::metadata(&path).unwrap();
        assert_ne!(
            (replacement.dev(), replacement.ino()),
            (identity.device, identity.inode)
        );
        assert_eq!(registered_materialized_count(root.as_raw_fd()), 1);

        drop(scope);
        assert_eq!(registered_materialized_count(root.as_raw_fd()), 0);
    }
}

#[cfg(test)]
mod pidfd_getfd_replay_tests {
    use super::*;

    #[test]
    fn exact_success_and_errno_need_no_cleanup() {
        assert_eq!(unexpected_pidfd_getfd_fd(&Ok(7), &Ok(7)), None);
        assert_eq!(
            unexpected_pidfd_getfd_fd(&Err(Errno::EBADF), &Err(Errno::EBADF)),
            None
        );
    }

    #[test]
    fn unexpected_success_is_owned_for_cleanup_before_refusal() {
        assert_eq!(
            unexpected_pidfd_getfd_fd(&Err(Errno::EBADF), &Ok(9)),
            Some(9)
        );
        assert_eq!(unexpected_pidfd_getfd_fd(&Ok(7), &Ok(9)), Some(9));
        assert_eq!(unexpected_pidfd_getfd_fd(&Ok(7), &Err(Errno::EPERM)), None);
    }
}
