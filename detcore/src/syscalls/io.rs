/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls dealing with IO and networking.
//!
//! Of course this overlaps somewhat with "files.rs".

use std::os::unix::io::RawFd;
use std::time::Duration;

use nix::fcntl::OFlag;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use tracing::debug;
use tracing::trace;

use crate::config::SchedHeuristic;
use crate::fd::FdType;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::syscalls::helpers::millis_duration_to_absolute_timeout;
use crate::syscalls::helpers::retry_nonblocking_syscall_with_timeout;
use crate::tool_global::*;
use crate::tool_local::Detcore;

const NSCD_SOCKET_PATHS: [&[u8]; 2] = [b"/var/run/nscd/socket", b"/run/nscd/socket"];

fn should_isolate_nscd(config: &crate::config::Config) -> bool {
    config.deterministic_io
        && config.sequentialize_threads
        && !config.recordreplay_modes
        && !config.debug_externalize_sockets
}

fn is_nscd_socket(addr: &libc::sockaddr_un, addrlen: usize) -> bool {
    let path_offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
    if addr.sun_family != libc::AF_UNIX as libc::sa_family_t
        || addrlen <= path_offset
        || addrlen > std::mem::size_of::<libc::sockaddr_un>()
    {
        return false;
    }

    let path_len = (addrlen - path_offset).min(addr.sun_path.len());
    let path = &addr.sun_path[..path_len];
    let path_len = path.iter().position(|&byte| byte == 0).unwrap_or(path_len);

    NSCD_SOCKET_PATHS.iter().any(|candidate| {
        candidate.len() == path_len
            && path[..path_len]
                .iter()
                .zip(candidate.iter())
                .all(|(&actual, &expected)| actual as u8 == expected)
    })
}

// Printing helper
// TODO: this should be subsumed by better syscall printing.
fn print_poll(call: &syscalls::Poll) {
    let len = call.nfds();
    debug!("POLL: on {} fds, timeout {}", len, call.timeout());
    // TODO: nicer API for reading arrays from the guest:
    unsafe {
        for i in 0..len {
            debug!(
                "POLL: fd {} = {}",
                i,
                call.fds().unwrap().offset(i as isize)
            );
        }
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// poll syscall (MAYHANG)
    pub async fn handle_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,

        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        if self.cfg.recordreplay_modes && call.timeout() == 0 {
            // This cannot block, but still yield a scheduler turn so a polling thread cannot
            // monopolize the guest between preemptions.
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            // In replay mode, we cannot assume the existence of FILES during replay.
            // Thus we must record the poll and replay it from the trace.
            Ok(self.handle_external_poll(guest, call).await?)
        } else {
            // TODO:
            // if is-external-poll { self.handle_external_poll(guest, call) }
            self.handle_internal_poll(guest, call).await
        }
    }

    /// Handle a guest-internal poll call that can be fully determinized.
    pub async fn handle_internal_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        let timeout_millis = call.timeout();
        if timeout_millis == 0 {
            Ok(guest.inject(call).await?) // Already non-blocking.
        } else {
            let maybe_timeout_ns = millis_duration_to_absolute_timeout(guest, timeout_millis).await;
            let mut rsrc = Resources::new(guest.thread_state().dettid);
            rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
            rsrc.fyi("poll");
            retry_nonblocking_syscall_with_timeout(guest, call, rsrc, maybe_timeout_ns).await
        }
    }

    /// Handle a poll syscall that deponds on external, nondeterminstic IO.
    pub async fn handle_external_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        let len = call.nfds();
        let time_delta = Duration::from_millis(call.timeout() as u64);

        if len == 0 && time_delta.is_zero() {
            let request = Self::sleep_request(guest, time_delta).await;
            resource_request(guest, request).await;
            Ok(0)
        } else {
            print_poll(&call);
            Ok(self
                .record_or_replay_blocking(guest, Syscall::Poll(call))
                .await?)
        }
    }

    /// epoll_create1 syscall
    pub async fn handle_epoll_create1<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollCreate1,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        // Register the epoll fd in the DetFd table like every other
        // fd-creating syscall (openat, eventfd2, pipe2, socket, ...). Without
        // this, later operations that consult the table via `with_detfd` /
        // `dup_fd` (F_GETFL, F_SETFD, F_DUPFD[_CLOEXEC], dup, ...) would fail
        // with EBADF even though the underlying kernel fd is valid. This broke,
        // for example, running rustup proxies (cargo/rustc) under hermit, whose
        // tokio runtime dups its epoll fd at startup.
        //
        // EPOLL_CLOEXEC shares the same bit value as O_CLOEXEC, so we can carry
        // the cloexec flag straight across.
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.flags().bits()),
            FdType::Epoll,
        )
        .await?;
        Ok(fd as i64)
    }

    /// epoll_ctl syscall
    pub async fn handle_epoll_ctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollCtl,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// epoll_pwait syscall (MAYHANG)
    pub async fn handle_epoll_pwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollPwait,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// epoll_wait syscall (MAYHANG)
    pub async fn handle_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollWait,
    ) -> Result<i64, Error> {
        if self.cfg.recordreplay_modes && call.timeout() == 0 {
            // This cannot block, but still yield a scheduler turn so a polling thread cannot
            // monopolize the guest between preemptions.
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            Ok(self
                .record_or_replay_blocking(guest, Syscall::EpollWait(call))
                .await?)
        } else {
            self.handle_internal_epoll_wait(guest, call).await
        }
    }

    /// Handle a guest-internal `epoll_wait` call that can be fully determinized.
    pub async fn handle_internal_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollWait,
    ) -> Result<i64, Error> {
        let timeout_millis = call.timeout();
        if timeout_millis == 0 {
            Ok(guest.inject(call).await?) // Already non-blocking.
        } else {
            let maybe_timeout_ns = millis_duration_to_absolute_timeout(guest, timeout_millis).await;
            let mut rsrc = Resources::new(guest.thread_state().dettid);
            rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
            rsrc.fyi("epoll_wait");
            retry_nonblocking_syscall_with_timeout(guest, call, rsrc, maybe_timeout_ns).await
        }
    }

    /// Connect system call (MAYHANG)
    /// Note that connect waits until a TCP handshake but does not wait for accept() on the other end.
    /// Nevertheless, it can block for a long time while waiting for connection, unless the socket
    /// is already nonblocking.
    pub async fn handle_connect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Connect,
    ) -> Result<i64, Error> {
        if guest.config().sched_heuristic == SchedHeuristic::ConnectBind {
            trace!("Scheduling heuristic: reprioritizing connect");
            let resource = ResourceID::PriorityChangePoint(
                FIRST_PRIORITY,
                guest.thread_state().thread_logical_time.as_nanos(),
            );
            let req = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, req).await;
        }

        // nscd exposes a mutable host-global mmap cache. Refuse the endpoint so
        // deterministic runs consistently fall back to files-based NSS lookup.
        if should_isolate_nscd(&self.cfg) && self.connect_targets_nscd(guest, &call)? {
            trace!("Rejecting connection to the host nscd cache");
            return Err(Error::Errno(Errno::ECONNREFUSED));
        }

        self.execute_nonblockable_fd_syscall(guest, call).await
    }

    fn connect_targets_nscd<G: Guest<Self>>(
        &self,
        guest: &G,
        call: &syscalls::Connect,
    ) -> Result<bool, Errno> {
        let is_socket = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.ty() == FdType::Socket)?;
        if !is_socket {
            return Ok(false);
        }

        let Some(addr) = call.uservaddr() else {
            return Ok(false);
        };
        let Ok(addrlen) = usize::try_from(call.addrlen()) else {
            return Ok(false);
        };
        if addrlen > std::mem::size_of::<libc::sockaddr_un>() {
            return Ok(false);
        }
        let read_len = addrlen;
        if read_len < std::mem::size_of::<libc::sa_family_t>() {
            return Ok(false);
        }

        let mut sockaddr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(
                (&mut sockaddr as *mut libc::sockaddr_un).cast::<u8>(),
                std::mem::size_of::<libc::sockaddr_un>(),
            )
        };
        guest
            .memory()
            .read_exact(addr.cast::<u8>(), &mut bytes[..read_len])?;

        Ok(is_nscd_socket(&sockaddr, addrlen))
    }

    /// Handles all of: recvfrom, recvmsg, sendto, sendmsg, sendmmsg syscalls (MAYHANG)
    pub async fn handle_sendrecv<
        G: Guest<Self>,
        C: SyscallInfo + NonblockableSyscall + Into<Syscall>,
    >(
        &self,
        guest: &mut G,
        call: C,
    ) -> Result<i64, Error> {
        self.execute_nonblockable_fd_syscall(guest, call).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix_addr(path: &[u8]) -> libc::sockaddr_un {
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, &byte) in addr.sun_path.iter_mut().zip(path) {
            *slot = byte as libc::c_char;
        }
        addr
    }

    #[test]
    fn recognizes_only_nscd_unix_socket_paths() {
        let path_offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);

        for path in NSCD_SOCKET_PATHS {
            let addr = unix_addr(path);
            assert!(is_nscd_socket(&addr, path_offset + path.len() + 1));
            assert!(is_nscd_socket(&addr, path_offset + path.len()));
            assert!(!is_nscd_socket(&addr, path_offset + path.len() - 1));
            assert!(!is_nscd_socket(&addr, std::mem::size_of_val(&addr) + 1));
        }

        let other = b"/tmp/guest.sock";
        assert!(!is_nscd_socket(
            &unix_addr(other),
            path_offset + other.len() + 1
        ));
    }

    #[test]
    fn isolates_nscd_only_for_deterministic_run_io() {
        let mut config = crate::config::Config::default();
        assert!(!should_isolate_nscd(&config));

        config.deterministic_io = true;
        assert!(!should_isolate_nscd(&config));

        config.sequentialize_threads = true;
        assert!(should_isolate_nscd(&config));

        config.recordreplay_modes = true;
        assert!(!should_isolate_nscd(&config));

        config.recordreplay_modes = false;
        config.debug_externalize_sockets = true;
        assert!(!should_isolate_nscd(&config));
    }
}
