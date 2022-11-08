/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls for dealing with the file system.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::path::PathBuf;

use nix::fcntl::OFlag;
use rand::Rng;
use reverie::syscalls;
use reverie::syscalls::family::StatFamily;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::FcntlCmd::*;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::SockFlag;
use reverie::syscalls::StatPtr;
use reverie::syscalls::Syscall;
use reverie::syscalls::Timespec;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use tracing::info;
use tracing::trace;
use tracing::warn;

use crate::config::SchedHeuristic;
use crate::detlog;
use crate::dirents::*;
use crate::fd::*;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
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
    async fn add_fd<G: Guest<Self>>(
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
        guest.thread_state_mut().remove_fd(fd);
        Ok(res)
    }

    /// SYS_read system call (MAYHANG).
    pub async fn handle_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<i64, Error> {
        let (fd_type, resource) = guest
            .thread_state_mut()
            .with_detfd(call.fd(), |detfd| (detfd.ty, detfd.resource.clone()))?;

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::R);
            resource_request(guest, request).await;
        }

        let res = match fd_type {
            FdType::Rng => {
                trace!("Read call RNG fd {}, simulating...", call.fd());
                let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
                let mut local_buf: Vec<u8> = vec![0; call.len()];
                guest
                    .thread_state_mut()
                    .thread_prng()
                    .fill(local_buf.as_mut_slice());
                let n = guest.memory().write(remote_buf, &local_buf)?;
                if cfg!(debug_assertions) {
                    let mut hasher = DefaultHasher::new();
                    local_buf.hash(&mut hasher);
                    detlog!(
                        "[dtid {}] USER RAND [/dev/urandom] Filled guest memory with {} random bytes, hash of bytes: {}",
                        guest.thread_state().dettid,
                        n,
                        hasher.finish()
                    );
                }
                return Ok(call.len() as i64);
            }
            FdType::Regular => {
                if guest.config().deterministic_io {
                    self.deterministic_read(guest, call).await
                } else {
                    Ok(self.record_or_replay(guest, call).await?)
                }
            }
            FdType::Signalfd
            | FdType::Eventfd
            | FdType::Timerfd
            | FdType::Memfd
            | FdType::Pidfd
            | FdType::Userfaultfd => {
                trace!("Read call on unusual fd {}, type {:?}", call.fd(), fd_type);
                // TODO, WARNING: this code path is not exercised by our tests [2021.11.09].
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

    /// SYS_write system call.
    pub async fn handle_write<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Write,
    ) -> Result<i64, Error> {
        let (resource, raw_ino) = guest.thread_state().with_detfd(call.fd(), |detfd| {
            (detfd.resource.clone(), detfd.stat.map(|x| x.inode))
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

        let res = if guest.config().deterministic_io {
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
        // This is a far-from-complete placeholder:
        if call.fd() == -1 {
            return Ok(self.record_or_replay(guest, call).await?);
        }
        // TODO/FIXME: a writable memory mapping means the file is essentially written continuously UNTIL it is closed.
        /* Therefore we need something that will grab write permission but maybe not release it?  Like so:
                let request = guest.thread_state().mk_request(rsrc, Permission::W);
                resource_request(guest, request).await; // Request but don't release?
        */

        // More accurately, this *associates* write permission with all future timeslices of this thread.
        // TODO(T78627117): We need thread-level permissions associated with scheduling each thread.
        Ok(self.record_or_replay(guest, call).await?)
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
            F_DUPFD(_) | F_DUPFD_CLOEXEC(_) => {
                let newfd = self.record_or_replay(guest, call).await? as RawFd;
                guest.thread_state_mut().dup_fd(fd, newfd, o_cloexec)?;
                Ok(newfd as i64)
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

    /// dup system call.
    pub async fn handle_dup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = self.record_or_replay(guest, call).await? as RawFd;
        guest
            .thread_state_mut()
            .dup_fd(old_fd, new_fd, OFlag::empty())?;
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
        guest
            .thread_state_mut()
            .dup_fd(old_fd, new_fd, OFlag::empty())?;
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
        guest.thread_state_mut().dup_fd(old_fd, new_fd, flags)?;
        Ok(res)
    }

    /// pipe2 system call.
    pub async fn handle_pipe2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pipe2,
    ) -> Result<i64, Errno> {
        let res = self.record_or_replay(guest, call).await?;
        let memory = guest.memory();

        if let Some(pipefd) = call.pipefd() {
            let fds: [i32; 2] = memory.read_value(pipefd)?;
            self.add_fd(guest, fds[0], call.flags(), FdType::Pipe)
                .await?;
            self.add_fd(guest, fds[1], call.flags(), FdType::Pipe)
                .await?;
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
        if guest.config().warn_non_zero_binds {
            let addr = call.umyaddr().ok_or(Errno::EFAULT)?;

            // like: sockaddr.sa_family
            let sockaddr_family = guest.memory().read_value(addr.cast::<u16>())?;

            if sockaddr_family == libc::AF_INET as u16 {
                let sockaddr_in: libc::sockaddr_in = guest
                    .memory()
                    .read_value(addr.cast::<libc::sockaddr_in>())?;
                // to_be() == htons
                let port = sockaddr_in.sin_port.to_be();
                let addr = Ipv4Addr::from(sockaddr_in.sin_addr.s_addr);
                if port != 0 {
                    warn!(
                        "Analyze Networking: Non-zero port detected: {:?}:{:?}",
                        addr, port
                    );
                }
            } else if sockaddr_family == libc::AF_INET6 as u16 {
                let sockaddr_in: libc::sockaddr_in6 = guest
                    .memory()
                    .read_value(addr.cast::<libc::sockaddr_in6>())?;
                // to_be() == htons
                let port = sockaddr_in.sin6_port.to_be();
                let addr = Ipv6Addr::from(sockaddr_in.sin6_addr.s6_addr);
                if port != 0 {
                    warn!(
                        "Analyze Networking: Non-zero port detected: {:?}:{:?}",
                        addr, port
                    );
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
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::EFD_CLOEXEC | libc::EFD_NONBLOCK),
            ),
            FdType::Eventfd,
        )
        .await?;
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
            OFlag::from_bits_truncate((call.flags() & libc::MFD_CLOEXEC as u32) as i32),
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

        let mut cached_bytes = vec![0; nb as usize];
        cached_bytes.reserve_exact(128);

        guest
            .memory()
            .read_exact(dirent.cast(), cached_bytes.as_mut_slice())?;

        let mut dents = unsafe { deserialize_dirents(&cached_bytes) };
        dents.sort();
        for dent in &mut dents {
            let (d_ino, _) = determinize_inode(guest, dent.ino).await;
            dent.ino = d_ino;
        }
        let _ = unsafe { serialize_dirents(&dents, cached_bytes.as_mut_slice()) };

        guest
            .memory()
            .write_exact(dirent.cast(), cached_bytes.as_slice())?;
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

        let mut cached_bytes = vec![0; nb as usize];
        cached_bytes.reserve_exact(128);

        guest
            .memory()
            .read_exact(dirent.cast(), cached_bytes.as_mut_slice())?;

        let mut dents = unsafe { deserialize_dirents64(&cached_bytes) };
        dents.sort();
        for dent in &mut dents {
            let (d_ino, _) = determinize_inode(guest, dent.ino).await;
            dent.ino = d_ino;
        }
        let _ = unsafe { serialize_dirents64(&dents, cached_bytes.as_mut_slice()) };

        guest
            .memory()
            .write_exact(dirent.cast(), cached_bytes.as_slice())?;
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
