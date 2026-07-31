/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod fs;
mod mmap;
mod network;
mod random;
mod time;

use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Mutex;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Rdtsc;
use reverie::RdtscResult;
use reverie::Subscription;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::event::Event;
use crate::event::OpenEvent;
use crate::event::ReplayFdKind;
use crate::event::SyscallEvent;
use crate::event_stream::DebugEvent;
use crate::event_stream::EventWriter;

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd
)]
struct OutputIdentity {
    device: u64,
    inode: u64,
}

impl OutputIdentity {
    fn for_fd(pid: Pid, fd: i32) -> Option<Self> {
        let metadata = std::fs::metadata(format!("/proc/{}/fd/{fd}", pid.as_raw())).ok()?;
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn matches(&self, metadata: &std::fs::Metadata) -> bool {
        self.device == metadata.dev() && self.inode == metadata.ino()
    }
}

fn duplicate_regular_output(pid: Pid, fd: libc::c_int) -> Option<std::os::fd::OwnedFd> {
    let metadata = std::fs::metadata(format!("/proc/{}/fd/{fd}", pid.as_raw())).ok()?;
    metadata
        .file_type()
        .is_file()
        .then(|| crate::fd::duplicate_guest_fd(pid, fd).ok())
        .flatten()
}
fn guest_has_open_file_description(pid: Pid, target: &std::os::fd::OwnedFd) -> bool {
    let entries = match std::fs::read_dir(format!("/proc/{}/fd", pid.as_raw())) {
        Ok(entries) => entries,
        Err(_) => return true,
    };
    let mut compared = false;
    let mut saw_any = false;
    for entry in entries.flatten() {
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::c_int>().ok())
        else {
            continue;
        };
        saw_any = true;
        let Ok(candidate) = crate::fd::duplicate_guest_fd(pid, fd) else {
            continue;
        };
        match crate::fd::same_open_file_description(candidate.as_raw_fd(), target.as_raw_fd()) {
            Ok(true) => return true,
            Ok(false) => compared = true,
            Err(error) => tracing::debug!(
                %error,
                fd,
                "could not compare guest fd while releasing captured output"
            ),
        }
    }
    saw_any && !compared
}

/// A Reverie tool that records syscalls. Note that only syscalls that cannot be
/// made deterministic are forwarded to this tool.
#[derive(Default, Serialize, Deserialize)]
pub struct Recorder {
    // TODO: We'll need to keep track of file descriptors here in order to
    // determine if a file descriptor should be fully recorded or simply cached
    // with a reflink. We can use `fstatfs` to figure out if the target file
    // system supports reflinks or not. All other file systems will need their
    // file interactions to be recorded on the syscall level.

    // Keep track of the data directory. Each thread uses this path to open its
    // event stream.
    data: PathBuf,
    /// Physical output endpoints inherited by the root guest.
    stdout: Option<OutputIdentity>,
    stderr: Option<OutputIdentity>,
    /// Stable regular-file OFDs used for offset aliasing checks.
    #[serde(skip)]
    stdout_ofd: Mutex<Option<std::os::fd::OwnedFd>>,
    #[serde(skip)]
    stderr_ofd: Mutex<Option<std::os::fd::OwnedFd>>,
}

#[reverie::tool]
impl Tool for Recorder {
    type GlobalState = detcore::GlobalState;
    type ThreadState = EventWriter;

    fn new(pid: Pid, cfg: &<Self::GlobalState as GlobalTool>::Config) -> Self {
        Self {
            data: cfg.replay_data.as_ref().unwrap().clone(),
            stdout: OutputIdentity::for_fd(pid, libc::STDOUT_FILENO),
            stderr: OutputIdentity::for_fd(pid, libc::STDERR_FILENO),
            stdout_ofd: Mutex::new(duplicate_regular_output(pid, libc::STDOUT_FILENO)),
            stderr_ofd: Mutex::new(duplicate_regular_output(pid, libc::STDERR_FILENO)),
        }
    }

    fn init_thread_state(
        &self,
        child: Tid,
        _parent: Option<(Tid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        // We have to unwrap because there is no way to handle errors here.
        EventWriter::create(&self.data, child).unwrap_or_else(|err| {
            panic!(
                "Failed to create {:?} for thread {}: {}",
                self.data, child, err
            )
        })
    }

    fn subscriptions(_config: &<Self::GlobalState as GlobalTool>::Config) -> Subscription {
        let mut subscription = Subscription::none();
        subscription.rdtsc().cpuid().syscalls([
            Sysno::execve,
            Sysno::execveat,
            //Sysno::brk,
            Sysno::mprotect,
            //Sysno::arch_prctl,
            Sysno::read,
            Sysno::pread64,
            Sysno::readv,
            Sysno::preadv,
            Sysno::preadv2,
            Sysno::recvfrom,
            Sysno::recvmsg,
            Sysno::write,
            Sysno::pwrite64,
            Sysno::writev,
            Sysno::pwritev,
            Sysno::pwritev2,
            Sysno::access,
            Sysno::lseek,
            Sysno::stat,
            Sysno::fstat,
            Sysno::lstat,
            Sysno::newfstatat,
            Sysno::statfs,
            Sysno::fstatfs,
            Sysno::statx,
            Sysno::getdents,
            Sysno::getdents64,
            Sysno::mmap,
            //Sysno::munmap,
            Sysno::open,
            Sysno::openat,
            Sysno::close,
            Sysno::openat2,
            Sysno::mkdirat,
            Sysno::mknodat,
            Sysno::fchownat,
            Sysno::linkat,
            Sysno::renameat,
            Sysno::renameat2,
            Sysno::symlinkat,
            Sysno::fchmodat,
            Sysno::utimensat,
            Sysno::fchdir,
            Sysno::close_range,
            Sysno::fadvise64,
            Sysno::flock,
            Sysno::ftruncate,
            Sysno::dup,
            Sysno::dup2,
            Sysno::dup3,
            Sysno::ioctl,
            Sysno::socket,
            Sysno::clock_gettime,
            Sysno::gettimeofday,
            Sysno::settimeofday,
            Sysno::time,
            Sysno::setsockopt,
            Sysno::fcntl,
            Sysno::connect,
            Sysno::sendto,
            Sysno::sendmsg,
            Sysno::poll,
            Sysno::ppoll,
            Sysno::epoll_wait,
            Sysno::getsockopt,
            Sysno::getpeername,
            Sysno::getsockname,
            Sysno::getrandom,
            Sysno::readlink,
            Sysno::mkdir,
            Sysno::unlink,
            Sysno::unlinkat,
        ]);

        subscription
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        self.record_raw_syscall(guest, syscall);
        self.record_exec_path(guest, syscall);

        Ok(match syscall {
            // We must let through execve without any modification. Recording
            // events for these is hard because execve only returns upon
            // failure.
            Syscall::Execve(_) => guest.inject(syscall).await,
            Syscall::Execveat(_) => guest.inject(syscall).await,
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
            Syscall::Read(syscall) => self.handle_read(guest, syscall).await,
            Syscall::Pread64(syscall) => self.handle_pread64(guest, syscall).await,
            Syscall::Readv(syscall) => {
                self.handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.len(),
                    syscall.fd(),
                    syscall.into(),
                )
                .await
            }
            Syscall::Preadv(syscall) => {
                self.handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len(),
                    syscall.fd(),
                    syscall.into(),
                )
                .await
            }
            Syscall::Preadv2(syscall) => {
                self.handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len() as usize,
                    syscall.fd(),
                    syscall.into(),
                )
                .await
            }
            Syscall::Recvfrom(syscall) => self.handle_recvfrom(guest, syscall).await,
            Syscall::Recvmsg(syscall) => self.handle_recvmsg(guest, syscall).await,
            Syscall::Write(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwrite64(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Writev(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwritev(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwritev2(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Access(_) => self.handle_simple(guest, syscall).await,
            Syscall::Lseek(_) => self.handle_simple(guest, syscall).await,
            Syscall::Stat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Fstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Lstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Newfstatat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Statfs(syscall) => {
                self.handle_statfs(guest, syscall.into(), syscall.buf())
                    .await
            }
            Syscall::Fstatfs(syscall) => {
                self.handle_statfs(guest, syscall.into(), syscall.buf())
                    .await
            }
            Syscall::Statx(syscall) => self.handle_statx(guest, syscall).await,
            Syscall::Getdents(syscall) => self.handle_getdents(guest, syscall).await,
            Syscall::Getdents64(syscall) => self.handle_getdents64(guest, syscall).await,
            Syscall::Mmap(syscall) => self.handle_mmap(guest, syscall).await,
            Syscall::Munmap(_) => self.let_through(guest, syscall).await,
            Syscall::Open(_) | Syscall::Openat(_) => self.handle_open(guest, syscall).await,
            Syscall::Close(_) => self.handle_fd_table_mutation(guest, syscall).await,
            Syscall::Openat2(_) => self.handle_simple(guest, syscall).await,
            Syscall::Mkdirat(_)
            | Syscall::Mknodat(_)
            | Syscall::Fchownat(_)
            | Syscall::Linkat(_)
            | Syscall::Renameat(_)
            | Syscall::Renameat2(_)
            | Syscall::Symlinkat(_)
            | Syscall::Fchmodat(_)
            | Syscall::Utimensat(_) => self.handle_simple(guest, syscall).await,
            Syscall::Fchdir(_) => self.handle_simple(guest, syscall).await,
            Syscall::Fadvise64(_) => self.handle_simple(guest, syscall).await,
            Syscall::Flock(_) => self.handle_simple(guest, syscall).await,
            Syscall::Ftruncate(syscall) => self.handle_ftruncate(guest, syscall).await,
            Syscall::Dup(_) => self.handle_simple(guest, syscall).await,
            Syscall::Dup2(_) | Syscall::Dup3(_) => {
                self.handle_fd_table_mutation(guest, syscall).await
            }
            Syscall::Ioctl(syscall) => self.handle_ioctl(guest, syscall).await,
            Syscall::Socket(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-979): pidfd_open is an input-only,
            // fd-returning syscall (like socket): record its return value so
            // the pidfd allocation is captured and can be recreated/validated
            // on replay. Without this arm it fell through to live injection and
            // the fd side effect was neither recorded nor replayed.
            Syscall::PidfdOpen(_) => self.handle_simple(guest, syscall).await,
            Syscall::ClockGettime(syscall) => self.handle_clock_gettime(guest, syscall).await,
            Syscall::Gettimeofday(syscall) => self.handle_gettimeofday(guest, syscall).await,
            Syscall::Settimeofday(_) => self.handle_simple(guest, syscall).await,
            Syscall::Time(syscall) => self.handle_time(guest, syscall).await,
            Syscall::Setsockopt(_) => self.handle_simple(guest, syscall).await,
            // FIXME: Not all fcntl cases are simple.
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
            Syscall::Mkdir(_) => self.handle_simple(guest, syscall).await,
            Syscall::Unlink(_) => self.handle_simple(guest, syscall).await,
            Syscall::Unlinkat(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Other(Sysno::close_range, _) => self.handle_close_range(guest, syscall).await,
            unsupported => return Ok(guest.inject(unsupported).await?),
        }?)
    }

    async fn handle_post_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        self.release_unreferenced_outputs(guest.pid());
        Ok(())
    }

    async fn handle_rdtsc_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: Rdtsc,
    ) -> Result<RdtscResult, Errno> {
        let result = RdtscResult::new(request);
        self.record_event(guest, Ok(SyscallEvent::Rdtsc(result)));
        Ok(result)
    }
}

impl Recorder {
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#662): Audit recorded physical-open classification.
    async fn handle_open<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let materialize = result
            .ok()
            .and_then(|fd| {
                std::fs::metadata(format!("/proc/{}/fd/{fd}", guest.pid().as_raw())).ok()
            })
            .is_some_and(|metadata| {
                metadata.file_type().is_file() || metadata.file_type().is_dir()
            });
        self.record_event(
            guest,
            Ok(SyscallEvent::Open(OpenEvent {
                result,
                materialize,
            })),
        );
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#662): Audit guest FD replay classification.
    fn fd_replay_kind(&self, pid: Pid, fd: libc::c_int) -> ReplayFdKind {
        let path = format!("/proc/{}/fd/{fd}", pid.as_raw());
        if std::fs::read_link(&path)
            .ok()
            .is_some_and(|target| target == std::path::Path::new("anon_inode:[eventfd]"))
        {
            return ReplayFdKind::Eventfd;
        }

        if std::fs::metadata(path)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_file())
        {
            ReplayFdKind::RegularFile
        } else {
            ReplayFdKind::None
        }
    }

    fn epoll_requires_replay_kernel_side_effect(&self, pid: Pid, fd: libc::c_int) -> bool {
        let Ok(fdinfo) = std::fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", pid.as_raw()))
        else {
            return false;
        };
        let targets = fdinfo.lines().filter_map(|line| {
            line.strip_prefix("tfd:")?
                .split_whitespace()
                .next()?
                .parse::<libc::c_int>()
                .ok()
        });
        let mut saw_target = false;
        for target in targets {
            saw_target = true;
            if self.fd_replay_kind(pid, target) == ReplayFdKind::None {
                return false;
            }
        }
        saw_target
    }

    pub(super) fn output_ofd_matches(
        &self,
        output_fd: libc::c_int,
        candidate: &std::os::fd::OwnedFd,
    ) -> bool {
        let output = match output_fd {
            libc::STDOUT_FILENO => &self.stdout_ofd,
            libc::STDERR_FILENO => &self.stderr_ofd,
            _ => return false,
        };
        let output = output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        output.as_ref().is_some_and(|target| {
            crate::fd::same_open_file_description(candidate.as_raw_fd(), target.as_raw_fd())
                .unwrap_or(false)
        })
    }

    fn release_unreferenced_output(output: &Mutex<Option<std::os::fd::OwnedFd>>, pid: Pid) {
        let mut output = output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if output
            .as_ref()
            .is_some_and(|target| !guest_has_open_file_description(pid, target))
        {
            output.take();
        }
    }

    fn release_unreferenced_outputs(&self, pid: Pid) {
        Self::release_unreferenced_output(&self.stdout_ofd, pid);
        Self::release_unreferenced_output(&self.stderr_ofd, pid);
    }

    async fn handle_fd_table_mutation<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        self.release_unreferenced_outputs(guest.pid());
        self.record_event(guest, result.map(SyscallEvent::Return));
        result
    }

    // TODO-HUMAN-REVIEW(#557): Audit close_range fd-table replay semantics.
    async fn handle_close_range<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let Syscall::Other(Sysno::close_range, args) = syscall else {
            unreachable!("handle_close_range called for {syscall:?}");
        };

        if args.arg2 & libc::CLOSE_RANGE_UNSHARE as usize != 0 {
            let result = Err(Errno::ENOSYS);
            self.record_event(guest, result.map(SyscallEvent::Return));
            return result;
        }

        self.handle_fd_table_mutation(guest, syscall).await
    }

    fn record_raw_syscall<G: Guest<Self>>(&self, guest: &mut G, syscall: Syscall) {
        let debug_event = DebugEvent::new(syscall, &guest.memory());
        guest
            .thread_state_mut()
            .push_debug_event(debug_event)
            .unwrap();
    }

    /// Records the absolute path of any executable that the guest execs, so that
    /// the replayer can make the same binary available inside its chroot.
    ///
    /// Without this, a guest process that forks and execs another binary (for
    /// example a shell running an external command) would desynchronize on
    /// replay: the injected `execve` fails with `ENOENT` inside the chroot,
    /// causing the guest to take a different code path than it did while
    /// recording.
    fn record_exec_path<G: Guest<Self>>(&self, guest: &mut G, syscall: Syscall) {
        let path = match syscall {
            Syscall::Execve(call) => call.path().map(|p| p.read(&guest.memory())),
            // Only AT_FDCWD execveat calls carry a path we can resolve without
            // reconstructing the guest's fd table; dirfd-relative execs are rare
            // and are skipped (best effort).
            Syscall::Execveat(call) if call.dirfd() == libc::AT_FDCWD => {
                call.path().map(|p| p.read(&guest.memory()))
            }
            _ => return,
        };
        let Some(Ok(path)) = path else {
            return;
        };
        // Only absolute paths can be reproduced in the chroot without also
        // knowing the guest's working directory at exec time.
        if !path.is_absolute() {
            return;
        }
        if let Err(err) = self.append_exec_path(&path) {
            tracing::warn!("Failed to record exec path {:?}: {}", path, err);
        }
    }

    /// Appends a single executable path to the recording's `exec_paths` file.
    ///
    /// The file is opened in append mode for every exec so that concurrent guest
    /// threads each contribute their targets; a single small `write` is atomic
    /// under `O_APPEND` on Linux. Duplicates are deduplicated by the replayer.
    fn append_exec_path(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;

        let bytes = path.as_os_str().as_bytes();
        // Paths are newline-delimited in the manifest; skip the (pathological)
        // case of an embedded newline rather than corrupt the file.
        if bytes.contains(&b'\n') {
            return Ok(());
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.data.join(crate::consts::EXEC_PATHS_NAME))?;
        let mut line = Vec::with_capacity(bytes.len() + 1);
        line.extend_from_slice(bytes);
        line.push(b'\n');
        file.write_all(&line)
    }

    fn record_event<G: Guest<Self>>(&self, guest: &mut G, event: Result<SyscallEvent, Errno>) {
        // Record the event.
        guest
            .thread_state_mut()
            .push_event(Event { event })
            // TODO: Log errors instead of panicking.
            .unwrap();
    }

    /// Called for syscalls to explicitly let through. This should only be called
    /// for syscalls that cannot be recorded and are necessary for the program to
    /// function correctly. Examples of syscalls that fall into this category are
    /// ones that help with memory management (e.g., `brk`, `mprotect`, `mmap`,
    /// or `munmap`) or process management (e.g., `fork`, `vfork`, `clone`).
    ///
    /// For these syscalls, we don't really need to record anything, but we
    /// record their arguments to detect any desynchronization.
    async fn let_through<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        guest.inject(syscall).await
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
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(guest, result.map(SyscallEvent::Return));

        result
    }
}
