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
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Timespec;
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
use crate::types::LogicalTime;

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

const KERNEL_SIGSET_SIZE: usize = std::mem::size_of::<u64>();

fn ppoll_timeout_duration(timeout: Timespec) -> Result<Duration, Errno> {
    let seconds = u64::try_from(timeout.tv_sec).map_err(|_| Errno::EINVAL)?;
    let nanoseconds = u32::try_from(timeout.tv_nsec).map_err(|_| Errno::EINVAL)?;
    if nanoseconds >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    Ok(Duration::new(seconds, nanoseconds))
}

fn timespec_from_duration(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    }
}

fn sanitize_ppoll_signal_mask(mask: u64) -> u64 {
    let signal_bit = (reverie::PERF_EVENT_SIGNAL as usize) - 1;
    mask & !(1_u64 << signal_bit)
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

    /// ppoll syscall (MAYHANG)
    // TODO-HUMAN-REVIEW(PR-273)
    pub async fn handle_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
    ) -> Result<i64, Error> {
        let timeout_address = call.timeout();
        let timeout = match timeout_address {
            Some(timeout) => Some(ppoll_timeout_duration(guest.memory().read_value(timeout)?)?),
            None => None,
        };

        let result: Result<i64, Error> = if timeout == Some(Duration::ZERO) {
            if self.cfg.recordreplay_modes {
                resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            }
            let (probe, _probe_guard) = self.prepare_ppoll_probe(guest, call).await?;
            let result = if self.cfg.recordreplay_modes {
                Ok(self.record_or_replay(guest, probe).await?)
            } else {
                Ok(guest.inject_with_retry(probe).await?)
            };
            if let Some(timeout_address) = timeout_address {
                guest
                    .memory()
                    .write_value(timeout_address, &timespec_from_duration(Duration::ZERO))?;
            }
            result
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            // The kernel owns the blocking wait in these modes. Use scratch memory only
            // for the signal mask so raw ppoll can still update the guest timeout.
            let mut signal_mask_guard = None;
            let call = if let Some(signal_mask) = call.sigmask() {
                if call.sigsetsize() != KERNEL_SIGSET_SIZE {
                    return Err(Errno::EINVAL.into());
                }
                let signal_mask: u64 = guest.memory().read_value(signal_mask.cast())?;
                let mut stack = guest.stack().await;
                let signal_mask = stack.push(sanitize_ppoll_signal_mask(signal_mask)).cast();
                signal_mask_guard = Some(stack.commit()?);
                call.with_sigmask(Some(signal_mask))
            } else {
                call
            };
            let result = Ok(self
                .record_or_replay_blocking(guest, Syscall::Ppoll(call))
                .await?);
            drop(signal_mask_guard);
            result
        } else {
            self.handle_internal_ppoll(guest, call, timeout).await
        };

        result
    }

    async fn prepare_ppoll_probe<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
    ) -> Result<(syscalls::Ppoll, <G::Stack as Stack>::StackGuard), Error> {
        let signal_mask = match call.sigmask() {
            Some(signal_mask) => {
                if call.sigsetsize() != KERNEL_SIGSET_SIZE {
                    return Err(Errno::EINVAL.into());
                }
                let signal_mask: u64 = guest.memory().read_value(signal_mask.cast())?;
                Some(sanitize_ppoll_signal_mask(signal_mask))
            }
            None => None,
        };

        let mut stack = guest.stack().await;
        let timeout = stack.push(timespec_from_duration(Duration::ZERO));
        // The scratch stack guard outlives the injected syscall, so the pointee is writable.
        let timeout = unsafe { timeout.into_mut() };
        let mut probe = call.with_timeout(Some(timeout));
        if let Some(signal_mask) = signal_mask {
            probe = probe.with_sigmask(Some(stack.push(signal_mask).cast()));
        }
        let guard = stack.commit()?;
        Ok((probe, guard))
    }

    /// Handle a guest-internal `ppoll` using zero-time kernel probes.
    async fn handle_internal_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
        timeout: Option<Duration>,
    ) -> Result<i64, Error> {
        debug_assert_ne!(timeout, Some(Duration::ZERO));
        let timeout_address = call.timeout();
        let started_at = if timeout.is_some() {
            Some(thread_observe_time(guest).await)
        } else {
            None
        };
        let deadline = match (timeout, started_at) {
            (Some(duration), Some(started_at)) => Some(started_at + duration),
            (None, None) => None,
            _ => unreachable!(),
        };

        // A zero probe can honor a temporary signal mask atomically. Keeping that mask
        // active while parked would require scheduler-level pending-signal state, so fail
        // closed rather than letting a masked signal interrupt a simulated wait.
        if call.sigmask().is_some() {
            let (probe, _probe_guard) = self.prepare_ppoll_probe(guest, call).await?;
            let result = guest.inject_with_retry(probe).await;
            if probe.syscall_would_have_blocked(result) {
                return Err(Errno::ENOSYS.into());
            }
            let result = result.map_err(Into::into);
            if let (Some(timeout_address), Some(timeout), Some(started_at)) =
                (timeout_address, timeout, started_at)
            {
                self.write_ppoll_remaining(guest, timeout_address, timeout, started_at)
                    .await?;
            }
            return result;
        }

        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("ppoll");
        let result = retry_nonblocking_syscall_with_timeout(guest, call, rsrc, deadline).await;
        if let (Some(timeout_address), Some(timeout), Some(started_at)) =
            (timeout_address, timeout, started_at)
        {
            self.write_ppoll_remaining(guest, timeout_address, timeout, started_at)
                .await?;
        }
        result
    }

    async fn write_ppoll_remaining<G: Guest<Self>>(
        &self,
        guest: &mut G,
        timeout_address: AddrMut<'_, Timespec>,
        timeout: Duration,
        started_at: LogicalTime,
    ) -> Result<(), Error> {
        let now = thread_observe_time(guest).await;
        let elapsed = Duration::from_nanos(now.as_nanos().saturating_sub(started_at.as_nanos()));
        let remaining = timeout.saturating_sub(elapsed);
        guest
            .memory()
            .write_value(timeout_address, &timespec_from_duration(remaining))?;
        Ok(())
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

    /// Apply an ADD, MOD, or DEL mutation to an epoll interest list.
    ///
    /// Determinism: strict execution serializes this mutation, so its result depends only on the
    /// operation, event payload, and deterministically reconstructed epoll/file-descriptor state.
    /// Record/replay rebuilds that state by reinjecting the same control operations.
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

        self.execute_nonblockable_fd_syscall(guest, call).await
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

    #[test]
    fn ppoll_timeout_uses_timespec_units() {
        assert_eq!(
            ppoll_timeout_duration(Timespec {
                tv_sec: 2,
                tv_nsec: 345_678_901,
            }),
            Ok(Duration::new(2, 345_678_901))
        );
        assert_eq!(
            timespec_from_duration(Duration::new(2, 345_678_901)),
            Timespec {
                tv_sec: 2,
                tv_nsec: 345_678_901,
            }
        );
    }

    #[test]
    fn ppoll_signal_mask_keeps_reverie_preemption_unblocked() {
        let preemption_bit = 1_u64 << ((reverie::PERF_EVENT_SIGNAL as usize) - 1);
        assert_eq!(sanitize_ppoll_signal_mask(u64::MAX), !preemption_bit);
    }

    #[test]
    fn ppoll_timeout_rejects_invalid_timespecs() {
        assert_eq!(
            ppoll_timeout_duration(Timespec {
                tv_sec: -1,
                tv_nsec: 0,
            }),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            ppoll_timeout_duration(Timespec {
                tv_sec: 0,
                tv_nsec: 1_000_000_000,
            }),
            Err(Errno::EINVAL)
        );
    }
}
