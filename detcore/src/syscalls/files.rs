/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls for dealing with the file system.

use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::path::PathBuf;

use nix::fcntl::OFlag;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::FcntlCmd::*;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::SockFlag;
use reverie::syscalls::StatPtr;
use reverie::syscalls::Syscall;
use reverie::syscalls::Timespec;
use reverie::syscalls::family::StatFamily;
use tracing::info;
use tracing::trace;
use tracing::warn;

use crate::config::SchedHeuristic;
use crate::dirents::*;
use crate::fd::*;
use crate::procfs::ProcfsFile;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::stat::*;
use crate::tool_global::*;
use crate::tool_local::Detcore;
use crate::types::*;

/// A conversion from SOCK_* flags to O_* flags which makes unsafe (but checked during testing) assumptions.
fn oflag_from_sock_bits(s_bits: i32) -> OFlag {
    // An otherwise unsafe "cast" which leans on the `linux_flags_assumptions` below.
    OFlag::from_bits_truncate(s_bits & (libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK))
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Inject an extra fstat to retrieve file metadata.
    async fn inject_fstat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        raw_fd: RawFd,
    ) -> Result<libc::stat, Errno> {
        info!(
            "Injecting additional fstat to retrieve file metadata on fd {}.",
            raw_fd
        );
        let mut stack = guest.stack().await;
        let statptr: StatPtr = StatPtr(stack.reserve());
        stack.commit()?;

        // NOTE: Must retry the injection here. This could get interrupted and
        // we don't want to rerun the entire syscall handler twice.
        guest
            .inject_with_retry(Syscall::Fstat(
                syscalls::Fstat::new()
                    .with_fd(raw_fd)
                    .with_stat(Some(statptr)),
            ))
            .await?;

        let copied = statptr.read(&guest.memory())?;
        // clear stack memory used for fstat allocation
        guest
            .memory()
            .write_exact(statptr.0.cast(), &[0; std::mem::size_of::<libc::stat>()])?;
        trace!("extra fstat returned inode {}", copied.st_ino);
        Ok(copied)
    }

    // helper function to track a new file descriptor.
    pub(crate) async fn add_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        flags: OFlag,
        ty: FdType,
    ) -> Result<(), Errno> {
        let stat = if guest.config().virtualize_metadata {
            Some(self.inject_fstat(guest, fd).await?.into())
        } else {
            None
        };
        guest.thread_state().add_fd(fd, flags, ty, stat)
    }

    pub(crate) async fn release_port_for_open_file<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file_id: OpenFileId,
    ) -> Option<u16> {
        let mytime = guest.thread_state().thread_logical_time.clone();
        let response = guest
            .send_rpc((mytime, GlobalRequest::ReleasePort(open_file_id)))
            .await;
        match response.1 {
            GlobalResponse::ReleasePort(port) => port,
            other => panic!("unexpected release-port response: {other:?}"),
        }
    }

    pub(crate) async fn restore_port_for_open_file<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file_id: OpenFileId,
        port: u16,
    ) {
        let mytime = guest.thread_state().thread_logical_time.clone();
        let response = guest
            .send_rpc((mytime, GlobalRequest::AddUsedPort(port, open_file_id)))
            .await;
        match response.1 {
            GlobalResponse::AddUsedPort => {}
            other => panic!("unexpected restore-port response: {other:?}"),
        }
    }

    /// Openat system call.
    pub async fn handle_openat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
    ) -> Result<i64, Error> {
        let path = call.path().ok_or(Errno::EFAULT)?;
        let path: PathBuf = path.read(&guest.memory())?;

        let resource = ResourceID::Path(path.clone());
        // Ask for permission to resolve this path into a file:
        let request = guest.thread_state().mk_request(resource, Permission::R);
        resource_request(guest, request).await;
        let res = self.record_or_replay(guest, Syscall::Openat(call)).await;

        match res {
            Ok(fd) => {
                let fd = fd as RawFd;
                let fd_type = path.to_str().map_or(FdType::Regular, |fname| {
                    if fname == "/dev/random" || fname == "/dev/urandom" {
                        FdType::Rng
                    } else {
                        FdType::Regular
                    }
                });
                self.add_fd(guest, fd, call.flags(), fd_type).await?;
                if let Some(procfs) = ProcfsFile::from_path(&path) {
                    guest
                        .thread_state()
                        .with_detfd(fd, |detfd| detfd.set_procfs(procfs.clone()))?;
                }
                resource_release_all(guest).await;
                Ok(fd as i64)
            }
            // TODO: audit for error-nondeterminism:
            Err(e) => {
                resource_release_all(guest).await;
                Err(e.into())
            }
        }
    }

    /// SYS_close system call.
    pub async fn handle_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Close,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let res = self.record_or_replay(guest, call).await?;
        if let Some(open_file_id) = guest.thread_state_mut().remove_fd(fd) {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        trace!("Closed {}", fd);
        Ok(res)
    }

    async fn snapshot_procfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<Vec<u8>, Error> {
        const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

        let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
        let mut contents = Vec::new();
        loop {
            let bytes_read = self.record_or_replay(guest, call).await? as usize;
            if bytes_read == 0 {
                return Ok(contents);
            }
            if contents.len() + bytes_read > MAX_SNAPSHOT_BYTES {
                return Err(Errno::EFBIG.into());
            }

            let mut chunk = vec![0; bytes_read];
            guest.memory().read_exact(remote_buf, &mut chunk)?;
            contents.extend_from_slice(&chunk);
        }
    }

    /// SYS_read system call (MAYHANG).
    pub async fn handle_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<i64, Error> {
        if call.len() == 0 {
            // Zero-count reads only serve to detect errors.
            let res = guest.inject(Syscall::from(call)).await?;
            return Ok(res);
        }

        let needs_procfs_snapshot = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_snapshot())?;
        if needs_procfs_snapshot {
            let contents = self.snapshot_procfs(guest, call).await?;
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                detfd.initialize_procfs(contents.clone());
            })?;
        }

        let procfs_bytes = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.take_procfs(call.len()))?;
        if let Some(bytes) = procfs_bytes {
            let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
            guest.memory().write_exact(remote_buf, &bytes)?;
            return Ok(bytes.len() as i64);
        }

        let (fd_type, resource) = guest
            .thread_state_mut()
            .with_detfd(call.fd(), |detfd| (detfd.ty(), detfd.resource()))?;

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::R);
            resource_request(guest, request).await;
        }

        let res = match fd_type {
            FdType::Rng => {
                trace!("Read call RNG fd {}, simulating...", call.fd());
                let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
                let n = self.fill_random_bytes(guest, remote_buf, call.len(), "/dev/[u]random")?;
                return Ok(n as i64);
            }
            FdType::Regular => {
                if guest.config().deterministic_io {
                    self.deterministic_read(guest, call).await
                } else {
                    Ok(self.record_or_replay(guest, call).await?)
                }
            }
            FdType::Signalfd | FdType::Eventfd | FdType::Timerfd | FdType::Inotify => {
                trace!(
                    "Possibly blocking read call on notification fd {}, type {:?}",
                    call.fd(),
                    fd_type
                );
                self.execute_nonblockable_fd_syscall(guest, call).await
            }
            FdType::Memfd | FdType::Pidfd | FdType::Userfaultfd | FdType::Epoll => {
                trace!("Read call on unusual fd {}, type {:?}", call.fd(), fd_type);
                Ok(self.record_or_replay(guest, call).await?)
            }

            FdType::Socket | FdType::Pipe => {
                trace!(
                    "Possibly blocking read call on {:?} fd {}",
                    fd_type,
                    call.fd()
                );
                self.execute_nonblockable_fd_syscall(guest, call).await
            }
        };
        resource_release_all(guest).await;
        res
    }

    /// SYS_pread64 system call.
    pub async fn handle_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pread64,
    ) -> Result<i64, Error> {
        if call.len() == 0 {
            // Zero-count reads only serve to detect errors.
            let res = guest.inject(Syscall::from(call)).await?;
            return Ok(res);
        }

        let (fd_type, resource) = guest
            .thread_state_mut()
            .with_detfd(call.fd(), |detfd| (detfd.ty(), detfd.resource()))?;

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::R);
            resource_request(guest, request).await;
        }

        let res = match fd_type {
            FdType::Rng => (|| -> Result<i64, Error> {
                trace!("Pread64 call RNG fd {}, simulating...", call.fd());
                let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
                let n = self.fill_random_bytes(guest, remote_buf, call.len(), "/dev/[u]random")?;
                Ok(n as i64)
            })(),
            FdType::Regular if guest.config().deterministic_io => {
                self.deterministic_pread64(guest, call).await
            }
            _ => match self.record_or_replay(guest, call).await {
                Ok(value) => Ok(value),
                Err(error) => Err(error.into()),
            },
        };

        resource_release_all(guest).await;
        res
    }

    /// Helper for performing a deterministic read that retries until it gets all its
    /// bytes.
    async fn deterministic_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Read,
    ) -> Result<i64, Error> {
        let mut total_read_bytes = 0;
        let mut remaining_buf = call.len();

        trace!(
            "[detcore/det_io]: Requested read buffer size: {:?}",
            remaining_buf
        );

        loop {
            match guest.inject_with_retry(call).await {
                Ok(res) => {
                    remaining_buf -= res as usize;
                    total_read_bytes += res;

                    trace!(
                        "[detcore/det_io]: Remaining read buffer size: {:?}",
                        remaining_buf
                    );

                    if res == 0 || remaining_buf == 0 {
                        break Ok(total_read_bytes);
                    }

                    // Buf is guaranteed to exist as we already issued a syscall.
                    let old_ptr = call.buf().unwrap().as_raw();
                    call = call
                        .with_len(remaining_buf)
                        .with_buf(AddrMut::<u8>::from_raw(old_ptr + res as usize));
                }
                Err(e) => {
                    break Err(e.into());
                }
            }
        }
    }

    /// Perform a positional read until the requested buffer is full or EOF is reached.
    async fn deterministic_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Pread64,
    ) -> Result<i64, Error> {
        let mut total_read_bytes = 0;
        let mut remaining_buf = call.len();

        trace!(
            "[detcore/det_io]: Requested pread64 buffer size: {:?}",
            remaining_buf
        );

        loop {
            match guest.inject_with_retry(call).await {
                Ok(res) => {
                    remaining_buf -= res as usize;
                    total_read_bytes += res;

                    trace!(
                        "[detcore/det_io]: Remaining pread64 buffer size: {:?}",
                        remaining_buf
                    );

                    if res == 0 || remaining_buf == 0 {
                        break Ok(total_read_bytes);
                    }

                    let old_ptr = call
                        .buf()
                        .expect("successful pread64 requires a valid guest buffer")
                        .as_raw();
                    let offset = call.offset().checked_add(res).ok_or(Errno::EOVERFLOW)?;
                    call = call
                        .with_len(remaining_buf)
                        .with_buf(AddrMut::<u8>::from_raw(old_ptr + res as usize))
                        .with_offset(offset);
                }
                Err(error) => break Err(error.into()),
            }
        }
    }

    /// SYS_write system call.
    pub async fn handle_write<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Write,
    ) -> Result<i64, Error> {
        let (fd_type, physically_nonblocking, resource, raw_ino) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.physically_nonblocking(),
                    detfd.resource(),
                    detfd.stat().map(|x| x.inode),
                )
            })?;
        // It doesn't matter much where the linearization point for this mtime bump falls:
        if guest.config().virtualize_metadata {
            let r =
                raw_ino.expect("Expect that when virtualize_metadata, DetFd's stat is populated!");
            touch_file(guest, r).await;
        }

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, request).await;
        }

        // Only route writes through the nonblockable-fd path when the fd is actually
        // physically nonblocking (the "hermit run" case, where pipe2/eventfd2 injected
        // O_NONBLOCK and we can nonblockize-and-retry deterministically). On a physically
        // blocking fd (e.g. record/replay mode, where O_NONBLOCK is intentionally not
        // injected) that path would treat the write as BlockingExternalIO and deschedule it
        // to run in the background, which assumes non-interference -- but a pipe/socket write
        // and its paired read are not independent, deadlocking the scheduler. Blocking-fd
        // writes therefore use the original synchronous path, as before this feature.
        let res = if physically_nonblocking
            && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd)
        {
            self.execute_nonblockable_fd_syscall(guest, call).await
        } else if guest.config().deterministic_io {
            let mut total_written_bytes = 0;
            let mut remaining_buf = call.len();

            trace!(
                "[detcore/det_io]: Requested write buffer size: {:?}",
                remaining_buf
            );

            loop {
                match self.record_or_replay(guest, call).await {
                    Ok(res) => {
                        remaining_buf -= res as usize;
                        total_written_bytes += res;

                        trace!(
                            "[detcore/det_io]: Remaining write buffer size: {:?}",
                            remaining_buf
                        );

                        if res == 0 || remaining_buf == 0 {
                            break Ok(total_written_bytes);
                        }

                        // Buf is guaranteed to exist as we already issued a syscall.
                        let old_ptr = call.buf().unwrap().as_raw();
                        call = call
                            .with_len(remaining_buf)
                            .with_buf(Addr::<u8>::from_raw(old_ptr + res as usize));
                    }
                    Err(e) => {
                        break Err(e.into());
                    }
                }
            }
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        };

        resource_release_all(guest).await;
        res
    }

    /// SYS_mmap system call.
    pub async fn handle_mmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Mmap,
    ) -> Result<i64, Error> {
        enum SharedBacking {
            Anonymous,
            File {
                object: SharedMemoryObjectId,
                offset: u64,
            },
        }

        let backing = if call.flags().contains(MapFlags::MAP_SHARED) {
            if call.fd() == -1 {
                Some(SharedBacking::Anonymous)
            } else {
                let offset = u64::try_from(call.offset()).map_err(|_| Errno::EINVAL)?;
                guest
                    .thread_state()
                    .with_detfd(call.fd(), |fd| {
                        let object = fd.stat().map_or_else(
                            || SharedMemoryObjectId::OpenFile {
                                id: fd.open_file_id(),
                            },
                            |stat| SharedMemoryObjectId::File {
                                device: stat.dev,
                                inode: stat.inode,
                            },
                        );
                        SharedBacking::File { object, offset }
                    })
                    .ok()
            }
        } else {
            None
        };
        let len = call.len();
        let result = self.record_or_replay(guest, call).await?;
        let start = usize::try_from(result).expect("a successful mmap must return an address");

        guest.thread_state().unmap_memory(start, len);
        match backing {
            Some(SharedBacking::Anonymous) => {
                guest.thread_state().map_shared_anonymous(start, len);
            }
            Some(SharedBacking::File { object, offset }) => {
                guest
                    .thread_state()
                    .map_shared_object(start, len, object, offset);
            }
            None => {}
        }
        Ok(result)
    }

    /// SYS_munmap system call.
    pub async fn handle_munmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Munmap,
    ) -> Result<i64, Error> {
        let start = call.addr().map(Addr::as_raw).unwrap_or(0);
        let len = call.len();
        let result = self.record_or_replay(guest, call).await?;
        guest.thread_state().unmap_memory(start, len);
        Ok(result)
    }

    /// SYS_mremap system call.
    pub async fn handle_mremap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Mremap,
    ) -> Result<i64, Error> {
        let old_start = call.addr().map(AddrMut::as_raw).unwrap_or(0);
        let old_len = call.old_len();
        let new_len = call.new_len();
        let result = self.record_or_replay(guest, call).await?;
        let new_start =
            usize::try_from(result).expect("a successful mremap must return an address");
        guest
            .thread_state()
            .remap_memory(old_start, old_len, new_start, new_len);
        Ok(result)
    }

    // Determinize stat by doing:
    //   - using virtual inode instead of real inodes. The virtual inodes
    //     increase monolitically and won't be re-used (like ext4)
    //   - use logical modtime which could be used by program like GNU make
    //     to determine file changes
    async fn determinize_stat<G, S>(&self, guest: &mut G, stat: S) -> Result<DetStat, Error>
    where
        G: Guest<Self>,
        S: Into<DetStat>,
    {
        let cfg = guest.config().clone();

        let mut stat: DetStat = stat.into();
        let (d_ino, global_mtime) = determinize_inode(guest, stat.inode).await;
        stat.inode = d_ino; // Reveal only the deterministic inode.

        let epoch_tp = Timespec {
            tv_sec: cfg.epoch.timestamp(),
            tv_nsec: cfg.epoch.timestamp_subsec_nanos() as i64,
        };

        let mtime: Timespec = global_mtime.into();
        stat.atime = epoch_tp;
        stat.ctime = epoch_tp;
        stat.btime = epoch_tp;

        stat.mtime = mtime;

        Ok(stat)
    }

    /// Handles all stat syscalls.
    pub async fn handle_stat_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: StatFamily,
    ) -> Result<i64, Error> {
        if guest.config().virtualize_metadata {
            // NB: let kernel handle error codes, it's not easy to do so without
            // kernel because there're many corner cases. i.e.: even access
            // filepath from tracer may cause tracer to hang under certain fuse
            // filesystem (squashfs_ll).
            guest.inject(Syscall::from(call)).await?;
            let statptr = call.stat().ok_or(Errno::EFAULT)?;
            let mut memory = guest.memory();
            let stat = memory.read_value(statptr.0)?;
            let stat = self.determinize_stat(guest, stat).await?;
            memory.write_value(statptr.0, &stat.into())?;
            Ok(0)
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// statx system call
    pub async fn handle_statx<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Statx,
    ) -> Result<i64, Error> {
        if guest.config().virtualize_metadata {
            // NB: let kernel handle error codes, it's not easy to do so without kernel
            // because there're many corner cases. i.e.: even access filepath from tracer
            // may cause tracer to hang under certain fuse filesystem (squashfs_ll).
            guest.inject(call).await?;
            let statptr = call.statx().ok_or(Errno::EFAULT)?;
            let mut memory = guest.memory();
            let stat = memory.read_value(statptr.0)?;
            let stat = self.determinize_stat(guest, stat).await?;
            memory.write_value(statptr.0, &stat.into())?;
            Ok(0)
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// fcntl system call
    pub async fn handle_fcntl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Fcntl,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let o_cloexec = match call.cmd() {
            F_DUPFD_CLOEXEC(_) => OFlag::O_CLOEXEC,
            _ => OFlag::empty(),
        };
        match call.cmd() {
            F_GETFL => {
                let physical_flags = self.record_or_replay(guest, call).await?;
                let logical_nonblocking = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.is_nonblocking())?;
                let nonblocking = i64::from(OFlag::O_NONBLOCK.bits());
                if logical_nonblocking {
                    Ok(physical_flags | nonblocking)
                } else {
                    Ok(physical_flags & !nonblocking)
                }
            }
            F_SETFL(flags) => {
                let fd_type = guest.thread_state().with_detfd(fd, |detfd| detfd.ty())?;
                let force_nonblocking = self.cfg.use_nonblocking_sockets()
                    && !self.cfg.recordreplay_modes
                    && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd);
                let physical_flags = if force_nonblocking {
                    flags | OFlag::O_NONBLOCK.bits()
                } else {
                    flags
                };
                let result = self
                    .record_or_replay(guest, call.with_cmd(F_SETFL(physical_flags)))
                    .await?;
                guest.thread_state().with_detfd(fd, |detfd| {
                    // Record the guest's *logical* status flags (derives logical
                    // nonblocking); when we forced O_NONBLOCK physically without the
                    // guest asking, mark the description physically nonblocking too.
                    detfd.set_status_flags(flags);
                    if force_nonblocking {
                        detfd.set_physically_nonblocking();
                    }
                })?;
                Ok(result)
            }
            F_DUPFD(_) | F_DUPFD_CLOEXEC(_) => {
                let newfd = self.record_or_replay(guest, call).await? as RawFd;
                let replaced = guest.thread_state_mut().dup_fd(fd, newfd, o_cloexec)?;
                if let Some(open_file_id) = replaced {
                    self.release_port_for_open_file(guest, open_file_id).await;
                }
                Ok(newfd as i64)
            }
            F_SETFD(flags) => {
                let result = self.record_or_replay(guest, call).await?;
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_cloexec(flags & libc::FD_CLOEXEC != 0);
                })?;
                Ok(result)
            }
            _ => {
                trace!(
                    "[detcore-finishme]: fcntl unhandled cases: {:?}",
                    call.cmd()
                );
                Ok(self.record_or_replay(guest, call).await?)
            }
        }
    }

    /// ioctl system call
    pub async fn handle_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ioctl,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let (cloexec, nonblocking) = match call.request() {
            syscalls::ioctl::Request::FIOCLEX => (Some(true), None),
            syscalls::ioctl::Request::FIONCLEX => (Some(false), None),
            syscalls::ioctl::Request::FIONBIO(value) => {
                let enabled = guest.memory().read_value(value.ok_or(Errno::EFAULT)?)? != 0;
                (None, Some(enabled))
            }
            _ => (None, None),
        };

        // When the guest clears O_NONBLOCK via FIONBIO on an fd that Detcore keeps
        // physically nonblocking for the scheduler, we must NOT let the clear
        // reach the kernel: doing so would make the fd physically blocking and
        // violate the scheduler's invariant (nonblockize-and-retry could then
        // block, risking deadlock). Instead we update only the guest-visible
        // logical flag and leave the physical fd -- and Detcore's physical
        // tracking -- nonblocking. This mirrors the F_SETFL handler's treatment
        // of the same force condition.
        if nonblocking == Some(false) {
            let fd_type = guest.thread_state().with_detfd(fd, |detfd| detfd.ty())?;
            let force_nonblocking = self.cfg.use_nonblocking_sockets()
                && !self.cfg.recordreplay_modes
                && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd);
            if force_nonblocking {
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_logical_nonblocking(false);
                })?;
                // FIONBIO returns 0 on success; the fd stays physically nonblocking.
                return Ok(0);
            }
        }

        let result = self.record_or_replay(guest, call).await?;
        if cloexec.is_some() || nonblocking.is_some() {
            guest.thread_state().with_detfd(fd, |detfd| {
                if let Some(enabled) = cloexec {
                    detfd.set_cloexec(enabled);
                }
                if let Some(enabled) = nonblocking {
                    detfd.set_nonblocking(enabled);
                }
            })?;
        }
        Ok(result)
    }

    /// statfs: report deterministic filesystem statistics.
    ///
    /// The kernel's `statfs` reflects live host state: the free-block counts
    /// (`f_bfree`, `f_bavail`), the free-inode count (`f_ffree`) and the device
    /// id (`f_fsid`) all vary between runs as the underlying host filesystem
    /// fills and drains, which makes a bare passthrough diverge under `--verify`
    /// (e.g. `tar` calls statfs on its target filesystem). The static geometry
    /// of the mount (`f_type`, `f_bsize`, `f_blocks`, `f_namelen`, ...) is
    /// reproducible, so we run the real syscall and then canonicalize only the
    /// volatile fields.
    pub async fn handle_statfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Statfs,
    ) -> Result<i64, Error> {
        let ret = self.record_or_replay(guest, call).await?;
        self.canonicalize_statfs_buf(guest, call.buf())?;
        Ok(ret)
    }

    /// fstatfs: same determinization as [`Self::handle_statfs`], keyed on an fd.
    pub async fn handle_fstatfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Fstatfs,
    ) -> Result<i64, Error> {
        let ret = self.record_or_replay(guest, call).await?;
        self.canonicalize_statfs_buf(guest, call.buf())?;
        Ok(ret)
    }

    /// Overwrite the host-varying fields of a `statfs` result buffer with fixed
    /// values, leaving the static per-mount geometry intact. Shared by statfs
    /// and fstatfs. A null buffer (only possible on an error return, which the
    /// caller has already propagated) is a no-op.
    fn canonicalize_statfs_buf<G: Guest<Self>>(
        &self,
        guest: &mut G,
        buf: Option<AddrMut<libc::statfs>>,
    ) -> Result<(), Error> {
        // Fixed *caps* for the volatile counters. The exact values are
        // arbitrary; they only need to be constant so repeated runs agree. We
        // clamp each free count to the mount's (static) total so we never report
        // the impossible "free > total": a filesystem may be smaller than the
        // cap, and some (e.g. overlayfs) report no inode accounting at all
        // (`f_files == 0`).
        const FREE_BLOCKS_CAP: libc::fsblkcnt_t = 1_000_000;
        const FREE_INODES_CAP: libc::fsfilcnt_t = 500_000;

        if let Some(buf) = buf {
            let mut sf = guest.memory().read_value(buf)?;
            let free_blocks = FREE_BLOCKS_CAP.min(sf.f_blocks);
            sf.f_bfree = free_blocks;
            sf.f_bavail = free_blocks;
            // `f_files == 0` means the filesystem does not track inodes; keep the
            // free count at 0 rather than inventing free inodes on a mount that
            // reports none.
            sf.f_ffree = if sf.f_files == 0 {
                0
            } else {
                FREE_INODES_CAP.min(sf.f_files)
            };
            // f_fsid is a device-dependent filesystem identifier; zero it. An
            // all-zero bit pattern is a valid `fsid_t` (a POD id pair).
            sf.f_fsid = unsafe { std::mem::zeroed() };
            guest.memory().write_value(buf, &sf)?;
        }
        Ok(())
    }

    /// dup system call.
    pub async fn handle_dup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = self.record_or_replay(guest, call).await? as RawFd;
        let replaced = guest
            .thread_state_mut()
            .dup_fd(old_fd, new_fd, OFlag::empty())?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(new_fd as i64)
    }

    /// dup2 system call.
    pub async fn handle_dup2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup2,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = call.newfd();
        let res = self.record_or_replay(guest, call).await?;
        let replaced = guest
            .thread_state_mut()
            .dup_fd(old_fd, new_fd, OFlag::empty())?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(res)
    }

    /// dup3 system call.
    pub async fn handle_dup3<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup3,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = call.newfd();
        let flags = call.flags();
        let res = self.record_or_replay(guest, call).await?;
        let replaced = guest.thread_state_mut().dup_fd(old_fd, new_fd, flags)?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(res)
    }

    /// pipe2 system call.
    pub async fn handle_pipe2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pipe2,
    ) -> Result<i64, Errno> {
        // Pipes are unambiguously container-internal: both endpoints are owned by
        // guest processes. Make them physically nonblocking whenever we sequentialize
        // threads -- INCLUDING record/replay modes. This lets a potentially-blocking
        // pipe read follow the deterministic nonblockize-and-retry (InternalIOPolling)
        // path instead of being descheduled as BlockingExternalIO. A pipe reader and
        // its paired writer are NOT independent, so treating an internal pipe as
        // "external blocking IO" (safe to run in the background and rejoin whenever)
        // deadlocks the sequentialized scheduler in R/R (the documented pipe hang). The
        // physical O_NONBLOCK is Detcore-internal and invisible to the guest (F_GETFL is
        // virtualized), and mirrors what `hermit run --strict` already does for pipes.
        let internally_nonblocking = self.cfg.use_nonblocking_sockets();
        let injected = if internally_nonblocking {
            call.with_flags(call.flags() | OFlag::O_NONBLOCK)
        } else {
            call
        };
        let res = self.record_or_replay(guest, injected).await?;
        let memory = guest.memory();

        if let Some(pipefd) = call.pipefd() {
            let fds: [i32; 2] = memory.read_value(pipefd)?;
            self.add_fd(guest, fds[0], call.flags(), FdType::Pipe)
                .await?;
            self.add_fd(guest, fds[1], call.flags(), FdType::Pipe)
                .await?;
            if internally_nonblocking {
                self.maybe_set_nonblocking_fd(guest, fds[0]);
                self.maybe_set_nonblocking_fd(guest, fds[1]);
            }
        }

        Ok(res)
    }

    /// utime syscall: update access/modification time on a file
    pub async fn handle_utime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utime,
    ) -> Result<i64, Errno> {
        let mut stack = guest.stack().await;
        let mut memory = guest.memory();
        let tp: AddrMut<[Timespec; 2]> = stack.reserve();

        let tp_val = match call.times() {
            None => {
                let now: Timespec = thread_observe_time(guest).await.into();
                [now, now]
            }
            Some(times) => {
                let utimptr = times;
                let utimbuf = memory.read_value(utimptr)?;
                [
                    Timespec {
                        tv_sec: utimbuf.actime,
                        tv_nsec: 0,
                    },
                    Timespec {
                        tv_sec: utimbuf.modtime,
                        tv_nsec: 0,
                    },
                ]
            }
        };

        memory.write_value(tp, &tp_val)?;
        stack.commit()?;

        let utimensat = syscalls::Utimensat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(call.path())
            .with_times(Some(tp.into()));

        self.handle_utimensat(guest, utimensat).await
    }

    /// utimes syscall
    pub async fn handle_utimes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimes,
    ) -> Result<i64, Errno> {
        let mut memory = guest.memory();

        let tp: AddrMut<[Timespec; 2]> = match call.times() {
            None => {
                let now: Timespec = thread_observe_time(guest).await.into();
                let mut stack = guest.stack().await;
                let tp: AddrMut<[Timespec; 2]> = stack.reserve();
                memory.write_value(tp, &[now, now])?;
                stack.commit()?;
                tp
            }
            Some(times) => {
                // Convert the timeval array to a timespec array.
                let tvs = memory.read_value(times)?;
                let tp: Addr<[Timespec; 2]> = times.cast();

                // Safety: The address could point to read-only memory and the
                // write below could fail.
                let tp = unsafe { tp.into_mut() };

                memory.write_value(tp, &[tvs[0].into(), tvs[1].into()])?;
                tp
            }
        };

        let utimensat = syscalls::Utimensat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(call.filename())
            .with_times(Some(tp.into()));

        self.handle_utimensat(guest, utimensat).await
    }

    /// ustimensat syscall
    pub async fn handle_utimensat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimensat,
    ) -> Result<i64, Errno> {
        self.record_or_replay(guest, call).await
    }

    /// socket system call.
    pub async fn handle_socket<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socket,
    ) -> Result<i64, Error> {
        // The socket syscall itself is not blocking, but we must decide whether to make the socket
        // returned physically nonblocking.
        if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            // Allow possibly blocking syscall in record mode
            let fd = self.record_or_replay(guest, call).await? as RawFd;
            self.add_fd(
                guest,
                fd,
                OFlag::from_bits_truncate(call.r#type()),
                FdType::Socket,
            )
            .await?;
            Ok(fd as i64)
        } else {
            // Under run mode, force all sockets to be registered to be nonblocking in the OS:
            let call2 = if self.cfg.use_nonblocking_sockets() {
                call.with_type(call.r#type() | libc::SOCK_NONBLOCK)
            } else {
                call
            };
            let fd = self.record_or_replay(guest, call2).await? as RawFd; // Cannot hang.
            self.add_fd(
                guest,
                fd,
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;
            self.maybe_set_nonblocking_fd(guest, fd);

            Ok(fd as i64)
        }
    }

    /// socketpair system call.
    pub async fn handle_socketpair<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socketpair,
    ) -> Result<i64, Error> {
        let call2 = if self.cfg.sequentialize_threads && !self.cfg.debug_externalize_sockets {
            call.with_type(call.r#type() | libc::SOCK_NONBLOCK)
        } else {
            call
        };
        let res = self.record_or_replay(guest, call2).await?;
        if let Some(usockvec) = call.usockvec() {
            let memory = guest.memory();
            let fds: [i32; 2] = memory.read_value(usockvec)?;

            // Logical flags are as requested:
            self.add_fd(
                guest,
                fds[0],
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;
            self.add_fd(
                guest,
                fds[1],
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;

            self.maybe_set_nonblocking_fd(guest, fds[0]);
            self.maybe_set_nonblocking_fd(guest, fds[1]);
        }
        Ok(res)
    }

    /// bind system call.
    pub async fn handle_bind<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Bind,
    ) -> Result<i64, Error> {
        // WIP!
        if guest.config().sched_heuristic == SchedHeuristic::ConnectBind {
            trace!("Scheduling heuristic: reprioritizing bind");
            let resource = ResourceID::PriorityChangePoint(
                LAST_PRIORITY,
                guest.thread_state().thread_logical_time.as_nanos(),
            );
            let req = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, req).await;
        }
        let addr = call.umyaddr().ok_or(Errno::EFAULT)?;
        let sock_fd = call.fd();
        let open_file_id = guest
            .thread_state()
            .with_detfd(sock_fd, |detfd| detfd.open_file_id())?;

        let sockaddr_family = guest.memory().read_value(addr.cast::<u16>())?;
        if sockaddr_family == libc::AF_INET as u16 {
            // For IPv4
            let mut sockaddr_in: libc::sockaddr_in = guest
                .memory()
                .read_value(addr.cast::<libc::sockaddr_in>())?;

            let port = sockaddr_in.sin_port.to_be();
            let ipaddr = Ipv4Addr::from(sockaddr_in.sin_addr.s_addr);
            if port != 0 {
                if guest.config().warn_non_zero_binds {
                    warn!(
                        "Analyze Networking: Non-zero port detected: {:?}:{:?}",
                        ipaddr, port
                    );
                }
                let mytime = guest.thread_state().thread_logical_time.clone();
                // Send RPC to make sure already used ports are not used.
                let resp = guest
                    .send_rpc((mytime, GlobalRequest::AddUsedPort(port, open_file_id)))
                    .await;
                match resp.1 {
                    GlobalResponse::AddUsedPort => {
                        trace!("Added to used port {}", port);
                    }
                    _ => unreachable!(),
                }
            } else {
                let mytime = guest.thread_state().thread_logical_time.clone();
                // Request a determinzed port
                let resp = guest
                    .send_rpc((mytime, GlobalRequest::RequestPort(open_file_id)))
                    .await;
                match resp.1 {
                    GlobalResponse::RequestPort(port_assigned) => {
                        sockaddr_in.sin_port = port_assigned.to_be();
                        guest
                            .memory()
                            .write_value(addr.cast::<libc::sockaddr_in>(), &sockaddr_in)?;
                    }
                    GlobalResponse::PortFull => {
                        return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                    }
                    _ => unreachable!(),
                }
            }
        } else if sockaddr_family == libc::AF_INET6 as u16 {
            // For IPv6
            let mut sockfaddr_in: libc::sockaddr_in6 = guest
                .memory()
                .read_value(addr.cast::<libc::sockaddr_in6>())?;
            let port = sockfaddr_in.sin6_port.to_be();
            let ipaddr = Ipv6Addr::from(sockfaddr_in.sin6_addr.s6_addr);
            if port != 0 {
                if guest.config().warn_non_zero_binds {
                    warn!(
                        "Analyze Networking: Non-zero port detected: {:?}:{:?}",
                        ipaddr, port
                    );
                }
                let mytime = guest.thread_state().thread_logical_time.clone();
                let resp = guest
                    .send_rpc((mytime, GlobalRequest::AddUsedPort(port, open_file_id)))
                    .await;
                match resp.1 {
                    GlobalResponse::AddUsedPort => {
                        trace!("Added to used port {}", port);
                    }
                    _ => unreachable!(),
                }
            } else {
                let mytime = guest.thread_state().thread_logical_time.clone();
                let resp = guest
                    .send_rpc((mytime, GlobalRequest::RequestPort(open_file_id)))
                    .await;
                match resp.1 {
                    GlobalResponse::RequestPort(port_assigned) => {
                        sockfaddr_in.sin6_port = port_assigned.to_be();
                        guest
                            .memory()
                            .write_value(addr.cast::<libc::sockaddr_in6>(), &sockfaddr_in)?;
                        trace!("Port assigned {}", port_assigned)
                    }
                    GlobalResponse::PortFull => {
                        return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                    }
                    _ => unreachable!(),
                }
            }
        }
        let res = self.record_or_replay(guest, call).await?;

        Ok(res)
    }

    /// socket system call.
    pub async fn handle_eventfd2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Eventfd2,
    ) -> Result<i64, Error> {
        let internally_nonblocking =
            self.cfg.use_nonblocking_sockets() && !self.cfg.recordreplay_modes;
        let injected = if internally_nonblocking {
            call.with_flags(call.flags() | syscalls::EfdFlags::EFD_NONBLOCK)
        } else {
            call
        };
        let fd = self.record_or_replay(guest, injected).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::EFD_CLOEXEC | libc::EFD_NONBLOCK),
            ),
            FdType::Eventfd,
        )
        .await?;
        if internally_nonblocking {
            self.maybe_set_nonblocking_fd(guest, fd);
        }
        Ok(fd as i64)
    }

    /// signalfd4 system call.
    pub async fn handle_signalfd4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Signalfd4,
    ) -> Result<i64, Error> {
        let signalfd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            signalfd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::SFD_CLOEXEC | libc::SFD_NONBLOCK),
            ),
            FdType::Signalfd,
        )
        .await?;
        Ok(signalfd as i64)
    }

    /// timerfd_create system call.
    pub async fn handle_timerfd_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdCreate,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::TFD_CLOEXEC | libc::TFD_NONBLOCK),
            ),
            FdType::Timerfd,
        )
        .await?;
        Ok(fd as i64)
    }

    /// Serialize a notification descriptor control operation.
    async fn notification_fd_control<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await;
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// timerfd_settime system call.
    pub async fn handle_timerfd_settime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdSettime,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// timerfd_gettime system call.
    pub async fn handle_timerfd_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdGettime,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// inotify_init1 system call.
    pub async fn handle_inotify_init1<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyInit1,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.flags().bits() & (libc::IN_CLOEXEC | libc::IN_NONBLOCK)),
            FdType::Inotify,
        )
        .await?;
        Ok(fd as i64)
    }

    /// inotify_add_watch system call.
    pub async fn handle_inotify_add_watch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyAddWatch,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// inotify_rm_watch system call.
    pub async fn handle_inotify_rm_watch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyRmWatch,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// memfd_create system call.
    pub async fn handle_memfd_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::MemfdCreate,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate((call.flags() & libc::MFD_CLOEXEC) as i32),
            FdType::Memfd,
        )
        .await?;
        Ok(fd as i64)
    }

    /// userfaultfd system call.
    pub async fn handle_userfaultfd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Userfaultfd,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.flags()),
            FdType::Userfaultfd,
        )
        .await?;
        Ok(fd as i64)
    }

    /// accept4 system call (MAYHANG).
    ///
    /// Category: External OR Internal IO
    /// ---------------------------------
    /// When do we know?  We only know if an accept4 did an extra-container IO AFTER it returns.
    /// I.e. we could accept a connection from another endpoint in the container, or from the outside,
    /// and we don't know which at the point where `accept4` is called.
    pub async fn handle_accept4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Accept4,
    ) -> Result<i64, Error> {
        // This option applies both to the socket we're doing the accept call on, and the connection
        // that we return. We don't have any smart detection yet to separate internal/external, so
        // applies to everything.
        let call2 = if self.cfg.use_nonblocking_sockets() {
            // Let the socket returned from accept4 be physically nonblocking:
            call.with_flags(call.flags() | SockFlag::SOCK_NONBLOCK)
        } else {
            call
        };
        // This will do blocking/polling as appropriate based on the fd status:
        let fd = self.execute_nonblockable_fd_syscall(guest, call2).await? as RawFd;

        self.add_fd(
            guest,
            fd,
            // This will specify whether the socket returned is logically non-blocking:
            oflag_from_sock_bits(call.flags().bits()),
            FdType::Socket,
        )
        .await?;

        self.maybe_set_nonblocking_fd(guest, fd);

        Ok(fd as i64)
    }

    /// getdents system call.
    pub async fn handle_getdents<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getdents,
    ) -> Result<i64, Error> {
        if !guest.config().virtualize_metadata {
            return Ok(self.record_or_replay(guest, call).await?);
        }

        let dirent = call.dirent().ok_or(Errno::EFAULT)?;

        let nb = self.record_or_replay(guest, call).await?;
        if nb == 0 {
            return Ok(0);
        }

        let mut dents_bytes = vec![0; nb as usize];
        dents_bytes.reserve_exact(128);

        guest
            .memory()
            .read_exact(dirent.cast(), dents_bytes.as_mut_slice())?;

        let mut dents = unsafe { deserialize_dirents(&dents_bytes) };
        dents.sort();
        for dent in &mut dents {
            let (d_ino, _) = determinize_inode(guest, dent.ino).await;
            dent.ino = d_ino;
        }

        let mut dents_bytes = vec![0; dents_bytes.len()];
        let _ = unsafe { serialize_dirents(&dents, &mut dents_bytes) };

        guest
            .memory()
            .write_exact(dirent.cast(), dents_bytes.as_slice())?;
        Ok(nb)
    }

    /// getdents64 system call.
    pub async fn handle_getdents64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getdents64,
    ) -> Result<i64, Error> {
        if !guest.config().virtualize_metadata {
            return Ok(self.record_or_replay(guest, call).await?);
        }

        let dirent = call.dirent().ok_or(Errno::EFAULT)?;

        let nb = self.record_or_replay(guest, call).await?;
        if nb == 0 {
            return Ok(0);
        }

        let mut dents_bytes = vec![0; nb as usize];
        dents_bytes.reserve_exact(128);

        guest
            .memory()
            .read_exact(dirent.cast(), dents_bytes.as_mut_slice())?;

        let mut dents = unsafe { deserialize_dirents64(&dents_bytes) };
        dents.sort();
        for dent in &mut dents {
            let (d_ino, _) = determinize_inode(guest, dent.ino).await;
            dent.ino = d_ino;
        }

        let mut dents_bytes = vec![0; dents_bytes.len()];
        let _ = unsafe { serialize_dirents64(&dents, &mut dents_bytes) };

        guest
            .memory()
            .write_exact(dirent.cast(), dents_bytes.as_slice())?;
        Ok(nb)
    }
}

#[cfg(test)]
mod test {
    use nix::fcntl::OFlag;

    /// This is an assumption we're making about flags.  Probably these flags can never be
    /// changed, but let's check just in case.
    #[test]
    fn linux_flags_assumptions() {
        assert_eq!(libc::SOCK_NONBLOCK, OFlag::O_NONBLOCK.bits());
        assert_eq!(libc::SOCK_CLOEXEC, OFlag::O_CLOEXEC.bits());
    }
}
