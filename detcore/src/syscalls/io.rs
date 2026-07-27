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
use reverie::syscalls::Displayable;
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
use crate::syscalls::helpers::record_retry_event;
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
const PSELECT6_INTERNAL_MAX_NFDS: i32 = (std::mem::size_of::<libc::c_ulong>() * 8) as i32;

#[derive(Clone, Copy)]
#[repr(C)]
struct Pselect6SigmaskArg {
    sigmask: usize,
    sigsetsize: usize,
}

fn pselect6_fd_set_len(nfds: i32) -> Result<usize, Errno> {
    let nfds = usize::try_from(nfds).map_err(|_| Errno::EINVAL)?;
    let bits_per_word = std::mem::size_of::<libc::c_ulong>() * 8;
    Ok(nfds.div_ceil(bits_per_word) * std::mem::size_of::<libc::c_ulong>())
}

fn read_pselect6_fd_set<T, G>(
    guest: &mut G,
    address: Option<AddrMut<'_, libc::fd_set>>,
    len: usize,
) -> Result<Option<Vec<u8>>, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let Some(address) = address else {
        return Ok(None);
    };
    let mut bytes = vec![0; len];
    if len != 0 {
        guest
            .memory()
            .read_exact(address.cast(), &mut bytes)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(Some(bytes))
}

fn write_pselect6_fd_set<T, G>(
    guest: &mut G,
    address: Option<AddrMut<'_, libc::fd_set>>,
    bytes: &Option<Vec<u8>>,
) -> Result<(), Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if let (Some(address), Some(bytes)) = (address, bytes)
        && !bytes.is_empty()
    {
        guest
            .memory()
            .write_exact(address.cast(), bytes)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(())
}

fn copy_pselect6_fd_set<T, G>(
    guest: &mut G,
    source: Option<AddrMut<'_, libc::fd_set>>,
    destination: Option<AddrMut<'_, libc::fd_set>>,
    len: usize,
) -> Result<(), Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if let (Some(source), Some(destination)) = (source, destination)
        && len != 0
    {
        let mut bytes = vec![0; len];
        guest
            .memory()
            .read_exact(source.cast(), &mut bytes)
            .map_err(|_| Errno::EFAULT)?;
        guest
            .memory()
            .write_exact(destination.cast(), &bytes)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(())
}

fn ppoll_timeout_duration(timeout: Timespec) -> Result<Duration, Errno> {
    let seconds = u64::try_from(timeout.tv_sec).map_err(|_| Errno::EINVAL)?;
    let nanoseconds = u32::try_from(timeout.tv_nsec).map_err(|_| Errno::EINVAL)?;
    if nanoseconds >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    Ok(Duration::new(seconds, nanoseconds))
}

fn select_timeout_duration(timeout: libc::timeval) -> Result<Duration, Errno> {
    let seconds = u64::try_from(timeout.tv_sec).map_err(|_| Errno::EINVAL)?;
    let microseconds = u64::try_from(timeout.tv_usec).map_err(|_| Errno::EINVAL)?;
    // Linux rejects select timeouts whose microsecond field is out of range.
    if microseconds >= 1_000_000 {
        return Err(Errno::EINVAL);
    }
    Ok(Duration::new(seconds, (microseconds * 1_000) as u32))
}

fn timespec_from_duration(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    }
}

const SCM_TIMESTAMP_OLD: libc::c_int = 29;
const SCM_TIMESTAMPNS_OLD: libc::c_int = 35;
const SCM_TIMESTAMPING_OLD: libc::c_int = 37;
const SCM_TIMESTAMP_NEW: libc::c_int = 63;
const SCM_TIMESTAMPNS_NEW: libc::c_int = 64;
const SCM_TIMESTAMPING_NEW: libc::c_int = 65;
const MAX_CONTROL_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
enum SocketTimestampKind {
    Timeval,
    Timespec,
    Timestamping,
}

fn cmsg_align(length: usize) -> usize {
    let alignment = std::mem::size_of::<usize>();
    (length + alignment - 1) & !(alignment - 1)
}

fn read_control_value<T: Copy>(bytes: &[u8]) -> Option<T> {
    if bytes.len() < std::mem::size_of::<T>() {
        return None;
    }
    // SAFETY: the length check guarantees a complete T, and read_unaligned
    // permits the control buffer's byte alignment.
    Some(unsafe { bytes.as_ptr().cast::<T>().read_unaligned() })
}

fn write_control_value<T: Copy>(bytes: &mut [u8], value: T) -> bool {
    if bytes.len() < std::mem::size_of::<T>() {
        return false;
    }
    // SAFETY: the length check guarantees room for T, and write_unaligned
    // permits the control buffer's byte alignment.
    unsafe { bytes.as_mut_ptr().cast::<T>().write_unaligned(value) };
    true
}

fn socket_timestamp_messages(control: &[u8]) -> Vec<(usize, SocketTimestampKind)> {
    let header_len = cmsg_align(std::mem::size_of::<libc::cmsghdr>());
    let mut messages = Vec::new();
    let mut offset = 0usize;

    while let Some(header_bytes) = control.get(offset..) {
        let Some(header) = read_control_value::<libc::cmsghdr>(header_bytes) else {
            break;
        };
        if header.cmsg_len < header_len {
            break;
        }
        let Some(end) = offset.checked_add(header.cmsg_len) else {
            break;
        };
        if end > control.len() {
            break;
        }

        if header.cmsg_level == libc::SOL_SOCKET {
            let data_offset = offset + header_len;
            let data_len = header.cmsg_len - header_len;
            let kind = match header.cmsg_type {
                SCM_TIMESTAMP_OLD | SCM_TIMESTAMP_NEW
                    if data_len >= std::mem::size_of::<libc::timeval>() =>
                {
                    Some(SocketTimestampKind::Timeval)
                }
                SCM_TIMESTAMPNS_OLD | SCM_TIMESTAMPNS_NEW
                    if data_len >= std::mem::size_of::<libc::timespec>() =>
                {
                    Some(SocketTimestampKind::Timespec)
                }
                SCM_TIMESTAMPING_OLD | SCM_TIMESTAMPING_NEW
                    if data_len >= 3 * std::mem::size_of::<libc::timespec>() =>
                {
                    Some(SocketTimestampKind::Timestamping)
                }
                _ => None,
            };
            if let Some(kind) = kind {
                messages.push((data_offset, kind));
            }
        }

        let step = cmsg_align(header.cmsg_len);
        let Some(next) = offset.checked_add(step) else {
            break;
        };
        if next <= offset {
            break;
        }
        offset = next;
    }
    messages
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-901)
fn canonicalize_socket_timestamps(control: &mut [u8], now: LogicalTime) -> usize {
    let messages = socket_timestamp_messages(control);
    let timespec = libc::timespec {
        tv_sec: now.as_secs() as libc::time_t,
        tv_nsec: now.subsec_nanos() as libc::c_long,
    };
    let timeval = libc::timeval {
        tv_sec: timespec.tv_sec,
        tv_usec: (timespec.tv_nsec / 1_000) as libc::suseconds_t,
    };

    for (offset, kind) in &messages {
        match kind {
            SocketTimestampKind::Timeval => {
                let _ = write_control_value(&mut control[*offset..], timeval);
            }
            SocketTimestampKind::Timespec => {
                let _ = write_control_value(&mut control[*offset..], timespec);
            }
            SocketTimestampKind::Timestamping => {
                let size = std::mem::size_of::<libc::timespec>();
                let _ = write_control_value(&mut control[*offset..], timespec);
                let zero = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                let _ = write_control_value(&mut control[*offset + size..], zero);
                let _ = write_control_value(&mut control[*offset + 2 * size..], zero);
            }
        }
    }
    messages.len()
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

    /// pselect6 syscall (MAYHANG).
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#686): Review scratch fd sets and scheduler polling.
    pub async fn handle_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
    ) -> Result<i64, Error> {
        if self.cfg.recordreplay_modes || !self.cfg.sequentialize_threads {
            // Recorder/Replayer do not model pselect6 events. Preserve their existing
            // live-kernel behavior without adding BlockingExternalIO scheduler events.
            return Ok(guest.inject(call).await?);
        }

        if call.nfds() < 0 {
            return Ok(guest.inject(call).await?);
        }

        let raw_timeout = match call.timeout() {
            Some(timeout) => {
                let timeout: Timespec = guest.memory().read_value(timeout.cast())?;
                Some(timeout)
            }
            None => None,
        };
        if matches!(raw_timeout, Some(timeout) if timeout.tv_sec == 0 && timeout.tv_nsec == 0) {
            return Ok(guest.inject(call).await?);
        }

        // Linux clamps raw fd-set copies to the process fd table's current max_fds.
        // Its initial table holds one machine word; larger nfds values can therefore
        // require fewer bytes than a userspace calculation predicts. Keep those calls
        // under kernel ownership rather than over-reading the guest bitmap.
        if call.nfds() > PSELECT6_INTERNAL_MAX_NFDS {
            return self
                .record_or_replay_blocking(guest, Syscall::Pselect6(call))
                .await;
        }

        // Linux wraps pselect6's temporary mask in { pointer, size }. Glibc supplies
        // the wrapper even when the inner pointer is null. A real mask needs atomic
        // kernel ownership across the wait, so keep it on the external-blocking path.
        if let Some(argument) = call.sigmask() {
            let argument: Pselect6SigmaskArg = guest.memory().read_value(argument.cast())?;
            if argument.sigmask != 0 {
                return self
                    .record_or_replay_blocking(guest, Syscall::Pselect6(call))
                    .await;
            }
        }
        // The null inner mask was snapshotted above. Do not let later guest mutations of
        // the outer wrapper change the meaning of a retry probe.
        let call = call.with_sigmask(None);
        let timeout = raw_timeout.map(ppoll_timeout_duration).transpose()?;

        self.handle_internal_pselect6(guest, call, timeout).await
    }

    async fn handle_internal_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        timeout: Option<Duration>,
    ) -> Result<i64, Error> {
        let len = pselect6_fd_set_len(call.nfds())?;
        let deadline = match timeout {
            Some(timeout) => Some(thread_observe_time(guest).await + timeout),
            None => None,
        };
        let original_readfds = match read_pselect6_fd_set(guest, call.readfds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_pselect6_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };
        let original_writefds = match read_pselect6_fd_set(guest, call.writefds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_pselect6_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };
        let original_exceptfds = match read_pselect6_fd_set(guest, call.exceptfds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_pselect6_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };

        let mut stack = guest.stack().await;
        let readfds = call.readfds().map(|_| stack.reserve::<libc::fd_set>());
        let writefds = call.writefds().map(|_| stack.reserve::<libc::fd_set>());
        let exceptfds = call.exceptfds().map(|_| stack.reserve::<libc::fd_set>());
        let probe_timeout = stack
            .push(Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            })
            .cast::<libc::timeval>();
        let _guard = stack.commit()?;
        let probe = call
            .with_readfds(readfds)
            .with_writefds(writefds)
            .with_exceptfds(exceptfds)
            .with_timeout(Some(probe_timeout));

        let mut resources = Resources::new(guest.thread_state().dettid);
        resources.insert(ResourceID::InternalIOPolling, Permission::W);
        resources.fyi("pselect6");

        loop {
            if resource_request(guest, resources.clone()).await == ResumeStatus::Signaled {
                self.write_pselect6_remaining(guest, call, deadline).await?;
                return Err(Errno::EINTR.into());
            }
            write_pselect6_fd_set(guest, probe.readfds(), &original_readfds)?;
            write_pselect6_fd_set(guest, probe.writefds(), &original_writefds)?;
            write_pselect6_fd_set(guest, probe.exceptfds(), &original_exceptfds)?;

            let result = guest.inject(probe).await;
            if result != Ok(0) {
                let copy_result = if result.is_ok() {
                    self.copy_pselect6_results(guest, probe, call, len)
                } else {
                    Ok(())
                };
                self.write_pselect6_remaining(guest, call, deadline).await?;
                copy_result?;
                return result.map_err(Into::into);
            }

            resources.poll_attempt += 1;
            if let Some(deadline) = deadline
                && thread_observe_time(guest).await >= deadline
            {
                let copy_result = self.copy_pselect6_results(guest, probe, call, len);
                self.write_pselect6_remaining(guest, call, Some(deadline))
                    .await?;
                copy_result?;
                return Ok(0);
            }
            trace!(
                "Retry #{} for syscall due to result Ok(0): {}",
                resources.poll_attempt,
                probe.display(&guest.memory())
            );
            record_retry_event(guest, probe).await;
        }
    }

    fn copy_pselect6_results<G: Guest<Self>>(
        &self,
        guest: &mut G,
        probe: syscalls::Pselect6,
        call: syscalls::Pselect6,
        len: usize,
    ) -> Result<(), Error> {
        copy_pselect6_fd_set(guest, probe.readfds(), call.readfds(), len)?;
        copy_pselect6_fd_set(guest, probe.writefds(), call.writefds(), len)?;
        copy_pselect6_fd_set(guest, probe.exceptfds(), call.exceptfds(), len)
    }

    async fn write_pselect6_remaining<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        deadline: Option<LogicalTime>,
    ) -> Result<(), Error> {
        if let (Some(timeout), Some(deadline)) = (call.timeout(), deadline) {
            let now = thread_observe_time(guest).await;
            let remaining = deadline.as_nanos().saturating_sub(now.as_nanos());
            let remaining = Timespec {
                tv_sec: (remaining / 1_000_000_000) as libc::time_t,
                tv_nsec: (remaining % 1_000_000_000) as libc::c_long,
            };
            // SAFETY: pselect6's timeout is a writable in-out kernel timespec even though
            // reverie-syscalls exposes the ABI-compatible pointer as shared.
            let timeout = unsafe { timeout.cast::<Timespec>().into_mut() };
            if let Err(error) = guest.memory().write_value(timeout, &remaining) {
                // Linux preserves the pselect6 result when remaining-time copyout faults.
                trace!(?error, "ignoring pselect6 timeout writeback failure");
            }
        }
        Ok(())
    }

    /// select syscall (MAYHANG).
    ///
    /// `select` is the classic `timeval` sibling of `pselect6` (which is already
    /// Determinized). It reuses the pselect6 fd-set scratch machinery, but takes
    /// a `struct timeval` timeout (which Linux updates in place with the time not
    /// slept) and carries no signal mask.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#800): Review select determinization mirroring pselect6.
    pub async fn handle_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Select,
    ) -> Result<i64, Error> {
        if self.cfg.recordreplay_modes || !self.cfg.sequentialize_threads {
            // Recorder/Replayer do not model select events. Preserve their existing
            // live-kernel behavior without adding BlockingExternalIO scheduler events.
            return Ok(guest.inject(call).await?);
        }

        if call.nfds() < 0 {
            return Ok(guest.inject(call).await?);
        }

        let raw_timeout = match call.timeout() {
            Some(timeout) => {
                let timeout: libc::timeval = guest.memory().read_value(timeout)?;
                Some(timeout)
            }
            None => None,
        };
        if matches!(raw_timeout, Some(timeout) if timeout.tv_sec == 0 && timeout.tv_usec == 0) {
            // A zero timeout is a pure non-blocking poll; the kernel can service it directly.
            return Ok(guest.inject(call).await?);
        }

        // Mirror pselect6: keep large fd tables under kernel ownership rather than
        // over-reading the guest bitmap (Linux clamps raw fd-set copies to max_fds).
        if call.nfds() > PSELECT6_INTERNAL_MAX_NFDS {
            return self
                .record_or_replay_blocking(guest, Syscall::Select(call))
                .await;
        }

        let timeout = raw_timeout.map(select_timeout_duration).transpose()?;
        self.handle_internal_select(guest, call, timeout).await
    }

    async fn handle_internal_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Select,
        timeout: Option<Duration>,
    ) -> Result<i64, Error> {
        let len = pselect6_fd_set_len(call.nfds())?;
        let deadline = match timeout {
            Some(timeout) => Some(thread_observe_time(guest).await + timeout),
            None => None,
        };
        let original_readfds = match read_pselect6_fd_set(guest, call.readfds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_select_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };
        let original_writefds = match read_pselect6_fd_set(guest, call.writefds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_select_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };
        let original_exceptfds = match read_pselect6_fd_set(guest, call.exceptfds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_select_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };

        let mut stack = guest.stack().await;
        let readfds = call.readfds().map(|_| stack.reserve::<libc::fd_set>());
        let writefds = call.writefds().map(|_| stack.reserve::<libc::fd_set>());
        let exceptfds = call.exceptfds().map(|_| stack.reserve::<libc::fd_set>());
        // select modifies its timeout in place, so the probe timeout must be a
        // writable scratch cell. It is re-zeroed each iteration to keep every
        // probe a non-blocking poll (a NULL timeout would block indefinitely).
        let probe_timeout = stack.reserve::<libc::timeval>();
        let _guard = stack.commit()?;
        let probe = call
            .with_readfds(readfds)
            .with_writefds(writefds)
            .with_exceptfds(exceptfds)
            .with_timeout(Some(probe_timeout));

        let mut resources = Resources::new(guest.thread_state().dettid);
        resources.insert(ResourceID::InternalIOPolling, Permission::W);
        resources.fyi("select");

        loop {
            if resource_request(guest, resources.clone()).await == ResumeStatus::Signaled {
                self.write_select_remaining(guest, call, deadline).await?;
                return Err(Errno::EINTR.into());
            }
            guest.memory().write_value(
                probe_timeout,
                &libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
            )?;
            write_pselect6_fd_set(guest, probe.readfds(), &original_readfds)?;
            write_pselect6_fd_set(guest, probe.writefds(), &original_writefds)?;
            write_pselect6_fd_set(guest, probe.exceptfds(), &original_exceptfds)?;

            let result = guest.inject(probe).await;
            if result != Ok(0) {
                let copy_result = if result.is_ok() {
                    self.copy_select_results(guest, probe, call, len)
                } else {
                    Ok(())
                };
                self.write_select_remaining(guest, call, deadline).await?;
                copy_result?;
                return result.map_err(Into::into);
            }

            resources.poll_attempt += 1;
            if let Some(deadline) = deadline
                && thread_observe_time(guest).await >= deadline
            {
                let copy_result = self.copy_select_results(guest, probe, call, len);
                self.write_select_remaining(guest, call, Some(deadline))
                    .await?;
                copy_result?;
                return Ok(0);
            }
            trace!(
                "Retry #{} for syscall due to result Ok(0): {}",
                resources.poll_attempt,
                probe.display(&guest.memory())
            );
            record_retry_event(guest, probe).await;
        }
    }

    fn copy_select_results<G: Guest<Self>>(
        &self,
        guest: &mut G,
        probe: syscalls::Select,
        call: syscalls::Select,
        len: usize,
    ) -> Result<(), Error> {
        copy_pselect6_fd_set(guest, probe.readfds(), call.readfds(), len)?;
        copy_pselect6_fd_set(guest, probe.writefds(), call.writefds(), len)?;
        copy_pselect6_fd_set(guest, probe.exceptfds(), call.exceptfds(), len)
    }

    async fn write_select_remaining<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Select,
        deadline: Option<LogicalTime>,
    ) -> Result<(), Error> {
        if let (Some(timeout), Some(deadline)) = (call.timeout(), deadline) {
            let now = thread_observe_time(guest).await;
            let remaining = deadline.as_nanos().saturating_sub(now.as_nanos());
            let remaining = libc::timeval {
                tv_sec: (remaining / 1_000_000_000) as libc::time_t,
                tv_usec: ((remaining % 1_000_000_000) / 1_000) as libc::suseconds_t,
            };
            // select's timeout is a writable in-out kernel timeval reporting the
            // time not slept; derive it from deterministic virtual time.
            if let Err(error) = guest.memory().write_value(timeout, &remaining) {
                // Linux preserves the select result when remaining-time copyout faults.
                trace!(?error, "ignoring select timeout writeback failure");
            }
        }
        Ok(())
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

    /// epoll_pwait2 syscall (MAYHANG).
    ///
    /// epoll_pwait2 is epoll_pwait with a `struct timespec *` timeout instead of
    /// an int-milliseconds timeout; recent glibc implements epoll_wait/
    /// epoll_pwait via epoll_pwait2 when the kernel supports it. The pinned
    /// Reverie revision has no typed variant, so it arrives as a raw
    /// `Syscall::Other` and is dispatched here by Sysno. Detcore treats it
    /// exactly like epoll_pwait: a scheduler yield point followed by
    /// record/replay-aware forwarding of the raw call.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#773)
    pub async fn handle_epoll_pwait2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
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

    /// Handles recvfrom, sendto, sendmsg, and sendmmsg syscalls (MAYHANG).
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

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-901)
    /// Receive one message and replace host socket timestamps with logical time.
    pub async fn handle_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvmsg,
    ) -> Result<i64, Error> {
        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        let Some(message_address) = call.msg() else {
            return Ok(result);
        };
        let message: libc::msghdr = guest.memory().read_value(message_address)?;
        if message.msg_control.is_null() || message.msg_controllen == 0 {
            return Ok(result);
        }

        let control_len = message.msg_controllen.min(MAX_CONTROL_BYTES);
        let control_address: AddrMut<'_, u8> =
            AddrMut::from_raw(message.msg_control as usize).ok_or(Errno::EFAULT)?;
        let mut control = vec![0; control_len];
        guest.memory().read_exact(control_address, &mut control)?;
        if socket_timestamp_messages(&control).is_empty() {
            return Ok(result);
        }

        let now = thread_observe_time(guest).await;
        canonicalize_socket_timestamps(&mut control, now);
        guest.memory().write_exact(control_address, &control)?;
        Ok(result)
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
    fn pselect6_fd_set_lengths_follow_the_raw_linux_abi() {
        assert_eq!(PSELECT6_INTERNAL_MAX_NFDS, 64);
        assert_eq!(pselect6_fd_set_len(-1), Err(Errno::EINVAL));
        assert_eq!(pselect6_fd_set_len(0), Ok(0));
        assert_eq!(pselect6_fd_set_len(1), Ok(8));
        assert_eq!(pselect6_fd_set_len(65), Ok(16));
        assert_eq!(pselect6_fd_set_len(libc::FD_SETSIZE as i32), Ok(128));
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

    #[test]
    fn socket_timestamp_control_messages_use_logical_time() {
        let header_len = cmsg_align(std::mem::size_of::<libc::cmsghdr>());
        let timeval_len = std::mem::size_of::<libc::timeval>();
        let timespec_len = std::mem::size_of::<libc::timespec>();
        let first_len = header_len + timeval_len;
        let second_offset = cmsg_align(first_len);
        let second_len = header_len + timespec_len;
        let third_offset = second_offset + cmsg_align(second_len);
        let third_len = header_len + std::mem::size_of::<i32>();
        let mut control = vec![0; third_offset + cmsg_align(third_len)];

        assert!(write_control_value(
            &mut control,
            libc::cmsghdr {
                cmsg_len: first_len,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: SCM_TIMESTAMP_OLD,
            }
        ));
        assert!(write_control_value(
            &mut control[header_len..],
            libc::timeval {
                tv_sec: 99,
                tv_usec: 88,
            }
        ));
        assert!(write_control_value(
            &mut control[second_offset..],
            libc::cmsghdr {
                cmsg_len: second_len,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: SCM_TIMESTAMPNS_OLD,
            }
        ));
        assert!(write_control_value(
            &mut control[second_offset + header_len..],
            libc::timespec {
                tv_sec: 77,
                tv_nsec: 66,
            }
        ));
        assert!(write_control_value(
            &mut control[third_offset..],
            libc::cmsghdr {
                cmsg_len: third_len,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: libc::SCM_RIGHTS,
            }
        ));
        assert!(write_control_value(
            &mut control[third_offset + header_len..],
            42_i32
        ));
        let unrelated_message = control[third_offset..].to_vec();

        assert_eq!(
            canonicalize_socket_timestamps(&mut control, LogicalTime::from_nanos(2_345_678_901)),
            2
        );
        let timeval = read_control_value::<libc::timeval>(&control[header_len..]).unwrap();
        assert_eq!(timeval.tv_sec, 2);
        assert_eq!(timeval.tv_usec, 345_678);
        let timespec =
            read_control_value::<libc::timespec>(&control[second_offset + header_len..]).unwrap();
        assert_eq!(timespec.tv_sec, 2);
        assert_eq!(timespec.tv_nsec, 345_678_901);
        assert_eq!(control[third_offset..], unrelated_message);
    }
}
