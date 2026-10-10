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

use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::os::unix::io::RawFd;
use std::time::Duration;

use nix::fcntl::OFlag;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Displayable;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie::syscalls::Timespec;
use tracing::debug;
use tracing::trace;

use crate::config::SchedHeuristic;
use crate::fd::FdType;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::LOOPBACK_POLL_YIELD_FYI;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::syscalls::helpers::KernelSignalWait;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::syscalls::helpers::RestartCall;
use crate::syscalls::helpers::is_accepted_connection;
use crate::syscalls::helpers::keep_restart_block;
use crate::syscalls::helpers::millis_duration_to_absolute_timeout;
use crate::syscalls::helpers::poll_deadline_for;
use crate::syscalls::helpers::record_retry_event;
use crate::syscalls::helpers::refuse_held_signal_loss;
use crate::syscalls::helpers::result_after_restore;
use crate::syscalls::helpers::retry_nonblocking_syscall_with_timeout;
use crate::syscalls::network_trace::received_descriptors;
use crate::syscalls::signal::kernel_installed_signal_mask;
use crate::syscalls::signal::read_kernel_sigset;
use crate::syscalls::threads::KernelSigset;
use crate::syscalls::threads::read_thread_signal_state;
use crate::tool_global::*;
use crate::tool_local::Detcore;
use crate::types::DetTid;
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

/// Build the scheduler request for a zero-timeout poll.
///
/// An empty request only rotates the caller within its current priority band. That is not a
/// yield when a busy poller has a higher priority than the producer whose readiness it is
/// probing. `SchedYield` excludes the caller from the next selection, allowing exactly one
/// other runnable guest to make progress before the nonblocking poll executes. Limit that strong
/// yield to SaBRe tasks with a loopback peer: libcurl alternates its loopback socket with an
/// internal wakeup fd, and either zero-timeout probe can otherwise starve the peer. General
/// build-tool polling retains the existing empty-turn behavior.
fn zero_timeout_poll_request(dettid: DetTid, yield_to_peer: bool) -> Resources {
    let mut request = Resources::new(dettid);
    if yield_to_peer {
        request.insert(ResourceID::SchedYield, Permission::W);
        request.fyi(LOOPBACK_POLL_YIELD_FYI);
    }
    request
}

fn connect_result_allows_peer_classification(result: &Result<i64, Error>) -> bool {
    match result {
        Ok(_) => true,
        Err(Error::Errno(errno)) => *errno == Errno::EINPROGRESS,
        Err(_) => false,
    }
}

const KERNEL_SIGSET_SIZE: usize = std::mem::size_of::<u64>();
pub(super) const PSELECT6_INTERNAL_MAX_NFDS: i32 =
    (std::mem::size_of::<libc::c_ulong>() * 8) as i32;

#[derive(Clone, Copy)]
#[repr(C)]
pub(super) struct Pselect6SigmaskArg {
    pub(super) sigmask: usize,
    pub(super) sigsetsize: usize,
}

/// The temporary mask a replayed pselect6 sleeps under, read from guest
/// memory at the call's entry, as Linux copies it once at entry; `None` when the
/// call has no mask or the wrapper or the mask cannot be read
/// (`end_replayed_pselect6_under_its_temporary_mask`).
fn replayed_pselect6_temporary_mask<G, T>(
    guest: &mut G,
    call: syscalls::Pselect6,
) -> Option<KernelSigset>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let wrapper = call.sigmask()?;
    let argument: Pselect6SigmaskArg = guest.memory().read_value(wrapper.cast()).ok()?;
    if argument.sigsetsize != KERNEL_SIGSET_SIZE {
        return None;
    }
    let mask_addr = Addr::<KernelSigset>::from_raw(argument.sigmask)?;
    guest.memory().read_value(mask_addr).ok()
}

/// Ends a replayed pselect6 that the recording says a signal interrupted
/// (`ERESTARTNOHAND`), when the signal is one that only the call's temporary
/// mask unblocks.
///
/// Replay serves pselect6 from the recording, so the kernel never installs the
/// call's temporary mask. Under the thread's own mask, which blocks the signal
/// the scheduler sent at the recorded point, the kernel finds nothing to
/// deliver, restarts the call, and the guest's next system call no longer
/// matches the recording (https://github.com/rrnewton/hermit/issues/3992).
/// Linux delivers such a signal before it puts the thread's own mask back, and
/// the handler's frame restores that mask when it returns. Only the kernel can
/// do both, so replay injects a pselect6 with no descriptors, a zero timeout
/// and a scratch copy of the mask the call read at entry (`temporary_mask`,
/// from `replayed_pselect6_temporary_mask`), never the guest's buffer, which a
/// sibling may have rewritten while the call was served (review of
/// https://github.com/rrnewton/hermit/pull/4052): the kernel installs
/// the mask, finds the signal pending, returns `ERESTARTNOHAND` and delivers
/// the signal, as it did when the recorded call returned. A zero timeout never
/// sleeps, and Linux writes no remaining time back for it, so neither host
/// timing nor host time reaches the guest.
///
/// Every value this reads (guest memory, and the kernel's pending set, mask
/// and dispositions in this thread's turn) is a function of the replayed
/// schedule. When no such signal is pending, the call returns as before.
async fn end_replayed_pselect6_under_its_temporary_mask<G, T>(
    guest: &mut G,
    temporary_mask: KernelSigset,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Ok(state) = read_thread_signal_state(guest.pid(), guest.tid()) else {
        return;
    };
    if state.pending
        & state.blocked
        & state.interrupting(kernel_installed_signal_mask(temporary_mask))
        == 0
    {
        return;
    }
    let mut stack = guest.stack().await;
    let mask_copy = stack.push(temporary_mask);
    let wrapper = stack
        .push(Pselect6SigmaskArg {
            sigmask: mask_copy.as_raw(),
            sigsetsize: KERNEL_SIGSET_SIZE,
        })
        .cast();
    // A reserved cell is staged as zeroes, which `commit` writes: a zero
    // timeout.
    let zero_timeout = stack.reserve::<Timespec>();
    let Ok(_guard) = stack.commit() else {
        return;
    };
    // An injection, as recording ran the call itself: the kernel delivers the
    // signal as the injected call returns, as it did after the recorded call,
    // and the guest then gets the recorded `ERESTARTNOHAND`.
    let _ = guest
        .inject(
            syscalls::Pselect6::new()
                .with_nfds(0)
                .with_readfds(None)
                .with_writefds(None)
                .with_exceptfds(None)
                .with_timeout(Some(zero_timeout))
                .with_sigmask(Some(wrapper)),
        )
        .await;
}

pub(super) fn pselect6_fd_set_len(nfds: i32) -> Result<usize, Errno> {
    let nfds = usize::try_from(nfds).map_err(|_| Errno::EINVAL)?;
    let bits_per_word = std::mem::size_of::<libc::c_ulong>() * 8;
    Ok(nfds.div_ceil(bits_per_word) * std::mem::size_of::<libc::c_ulong>())
}

fn pselect6_probe_result(result: Result<i64, Errno>) -> Result<i64, Errno> {
    match result {
        // The injected syscall runs outside the guest's original restart frame.
        // Do not expose this kernel-internal restart instruction at the
        // rewritten pselect6 call site.
        Err(Errno::ERESTARTSYS) => Err(Errno::EINTR),
        result => result,
    }
}

pub(super) fn read_pselect6_fd_set<T, G>(
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

pub(super) fn write_pselect6_fd_set<T, G>(
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

pub(super) fn ppoll_timeout_duration(timeout: Timespec) -> Result<Duration, Errno> {
    let seconds = u64::try_from(timeout.tv_sec).map_err(|_| Errno::EINVAL)?;
    let nanoseconds = u32::try_from(timeout.tv_nsec).map_err(|_| Errno::EINVAL)?;
    if nanoseconds >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    Ok(Duration::new(seconds, nanoseconds))
}

pub(super) fn select_timeout_duration(timeout: libc::timeval) -> Result<Duration, Errno> {
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

#[derive(Clone, Copy)]
struct SocketTimestampMessage {
    data_offset: usize,
    available_data_len: usize,
    kind: SocketTimestampKind,
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

fn write_control_prefix<T: Copy>(bytes: &mut [u8], value: T) -> usize {
    let value_len = std::mem::size_of::<T>();
    let write_len = bytes.len().min(value_len);
    // SAFETY: `value` is alive for this copy and the resulting byte view has
    // exactly its initialized object representation.
    let value_bytes =
        unsafe { std::slice::from_raw_parts((&value as *const T).cast::<u8>(), value_len) };
    bytes[..write_len].copy_from_slice(&value_bytes[..write_len]);
    write_len
}

fn socket_timestamp_messages(control: &[u8]) -> Vec<SocketTimestampMessage> {
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

        if header.cmsg_level == libc::SOL_SOCKET {
            let data_offset = offset + header_len;
            let declared_data_len = header.cmsg_len - header_len;
            let available_data_len = control
                .len()
                .saturating_sub(data_offset)
                .min(declared_data_len);
            let kind = match header.cmsg_type {
                SCM_TIMESTAMP_OLD | SCM_TIMESTAMP_NEW => Some(SocketTimestampKind::Timeval),
                SCM_TIMESTAMPNS_OLD | SCM_TIMESTAMPNS_NEW => Some(SocketTimestampKind::Timespec),
                SCM_TIMESTAMPING_OLD | SCM_TIMESTAMPING_NEW => {
                    Some(SocketTimestampKind::Timestamping)
                }
                _ => None,
            };
            if let Some(kind) = kind {
                messages.push(SocketTimestampMessage {
                    data_offset,
                    available_data_len,
                    kind,
                });
            }
        }

        if end > control.len() {
            break;
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

    for message in &messages {
        let available_end = message
            .data_offset
            .saturating_add(message.available_data_len)
            .min(control.len());
        let data = &mut control[message.data_offset..available_end];
        match message.kind {
            SocketTimestampKind::Timeval => {
                write_control_prefix(data, timeval);
            }
            SocketTimestampKind::Timespec => {
                write_control_prefix(data, timespec);
            }
            SocketTimestampKind::Timestamping => {
                let size = std::mem::size_of::<libc::timespec>();
                let zero = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                for slot in 0..3 {
                    let start = slot * size;
                    if start >= data.len() {
                        break;
                    }
                    let end = (start + size).min(data.len());
                    let slot_bytes = &mut data[start..end];
                    if slot_bytes.len() != size {
                        slot_bytes.fill(0);
                        continue;
                    }
                    let original = read_control_value::<libc::timespec>(slot_bytes)
                        .expect("complete timestamping slot");
                    let replacement = if original.tv_sec == 0 && original.tv_nsec == 0 {
                        zero
                    } else {
                        timespec
                    };
                    let _ = write_control_value(slot_bytes, replacement);
                }
            }
        }
    }
    messages.len()
}

fn sanitize_ppoll_signal_mask(mask: u64) -> u64 {
    let signal_bit = (reverie::PERF_EVENT_SIGNAL as usize) - 1;
    mask & !(1_u64 << signal_bit)
}

fn ppoll_uses_kernel_wait(
    sequentialize_threads: bool,
    recordreplay_modes: bool,
    has_signal_mask: bool,
) -> bool {
    !sequentialize_threads || (recordreplay_modes && !has_signal_mask)
}

impl<T: RecordOrReplay> Detcore<T> {
    /// poll syscall (MAYHANG)
    // TODO-HUMAN-REVIEW(PR-1023): Review zero-timeout poll scheduling across backends.
    pub async fn handle_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,

        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        if self.cfg.sequentialize_threads && call.timeout() == 0 {
            // This cannot block, but still yield a scheduler turn so a polling thread cannot
            // monopolize the guest between preemptions.
            let yield_to_peer = self.cfg.backend.loopback_pollers_yield_to_peers
                && guest.thread_state().has_loopback_peer();
            resource_request(
                guest,
                zero_timeout_poll_request(guest.thread_state().dettid, yield_to_peer),
            )
            .await;
            if self.cfg.recordreplay_modes {
                Ok(self.record_or_replay(guest, call).await?)
            } else {
                Ok(guest.inject(call).await?)
            }
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

    /// Record or replay a raw `select` or `pselect6` the way `poll` is: a zero
    /// timeout cannot block and takes an ordinary scheduler turn; any other call
    /// may wait in the kernel, so it runs as a blocking external operation. The
    /// recorder captures the descriptor sets and remaining time, so replay never
    /// asks the kernel about descriptors it did not recreate.
    async fn record_or_replay_select_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        zero_timeout: bool,
    ) -> Result<i64, Error> {
        if zero_timeout {
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else {
            self.record_or_replay_blocking(guest, call).await
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
        if self.cfg.recordreplay_modes {
            let zero_timeout = call.timeout().is_some_and(|timeout| {
                guest
                    .memory()
                    .read_value(timeout)
                    .is_ok_and(|timeout: Timespec| timeout.tv_sec == 0 && timeout.tv_nsec == 0)
            });
            // A call with a `{ sigmask, sigsetsize }` wrapper may sleep under a
            // temporary mask, and the scheduler must not take the thread's
            // ordinary mask for it: it would arm the release barrier for a
            // signal the call blocks and wait for a wake Linux never delivers
            // (https://github.com/rrnewton/hermit/issues/3963). Nor can this turn
            // read the mask the kernel will install: the wrapper may sit in
            // memory a tracer read cannot reach though the kernel can, and a
            // sibling may rewrite the mask before the kernel copies it. So the
            // scheduler records no mask for such a call
            // (`Resources::blocked_signal_mask_unknown`). Without a wrapper the
            // call sleeps under the thread's own mask, which the scheduler reads.
            return if call.sigmask().is_some() && !zero_timeout {
                // Replay reads the temporary mask here, at the call's entry, as
                // Linux copies it; a sibling may rewrite the buffer while the
                // call is served (review of https://github.com/rrnewton/hermit/pull/4052).
                let temporary_mask = if self.cfg.replaying {
                    replayed_pselect6_temporary_mask(guest, call)
                } else {
                    None
                };
                let result = self
                    .record_or_replay_blocking_with_unknown_mask(guest, Syscall::Pselect6(call))
                    .await;
                if let Some(mask) = temporary_mask
                    && matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND)))
                {
                    end_replayed_pselect6_under_its_temporary_mask(guest, mask).await;
                }
                result
            } else {
                self.record_or_replay_select_family(guest, Syscall::Pselect6(call), zero_timeout)
                    .await
            };
        }
        if !self.cfg.sequentialize_threads {
            return Ok(guest.inject(call).await?);
        }

        if call.nfds() < 0 {
            return Ok(guest.inject(call).await?);
        }

        // Linux copies pselect6's outer { sigmask, sigsetsize } wrapper before
        // validating the timeout. Copy only the wrapper here; validation of the
        // pointed-to signal mask remains below, after timeout validation.
        let sigmask_argument = match call.sigmask() {
            Some(argument) => {
                // A split read can fall back to PTRACE_PEEKDATA for the final
                // word, bypassing PROT_NONE or reporting EIO for an unmapped
                // page. Have Linux validate both wrapper words first. It copies
                // this wrapper before rejecting a malformed timeout, without
                // reading the inner mask, changing it, waiting, or writing output.
                let mut stack = guest.stack().await;
                let validation_timeout = stack.reserve::<Timespec>();
                let _guard = stack.commit()?;
                guest.memory().write_value(
                    validation_timeout,
                    &Timespec {
                        tv_sec: 0,
                        tv_nsec: 1_000_000_000,
                    },
                )?;
                let validation = syscalls::Pselect6::new()
                    .with_nfds(0)
                    .with_readfds(None)
                    .with_writefds(None)
                    .with_exceptfds(None)
                    .with_timeout(Some(validation_timeout))
                    .with_sigmask(Some(argument));
                match guest.inject(validation).await {
                    Err(Errno::EINVAL) => {}
                    Err(errno) => return Err(errno.into()),
                    // Success would mean the backend did not validate the probe.
                    Ok(_) => return Err(Errno::EIO.into()),
                }
                let argument: Pselect6SigmaskArg = guest.memory().read_value(argument.cast())?;
                Some(argument)
            }
            None => None,
        };
        let raw_timeout = match call.timeout() {
            Some(timeout) => {
                let timeout: Timespec = guest.memory().read_value(timeout)?;
                Some(timeout)
            }
            None => None,
        };
        let timeout = raw_timeout.map(ppoll_timeout_duration).transpose()?;
        if timeout == Some(Duration::ZERO) {
            return Ok(guest.inject(call).await?);
        }

        // Linux clamps raw fd-set copies to the process fd table's current max_fds.
        // Its initial table holds one machine word; larger nfds values can therefore
        // require fewer bytes than a userspace calculation predicts. Keep those calls
        // under kernel ownership rather than over-reading the guest bitmap.
        if call.nfds() > PSELECT6_INTERNAL_MAX_NFDS {
            // The kernel sleeps under the inner mask, so record it for the
            // scheduler's SIGCHLD target choice (`Resources::blocked_signal_mask`).
            // A mask the kernel would reject (wrong size, or a pointer it cannot
            // read) fails the real call before it sleeps or installs anything, so
            // the thread's own mask is the right record there (`None`).
            let blocked_signal_mask = match sigmask_argument {
                Some(argument)
                    if argument.sigmask != 0 && argument.sigsetsize == KERNEL_SIGSET_SIZE =>
                {
                    match Addr::<libc::sigset_t>::from_raw(argument.sigmask) {
                        Some(mask_addr) => read_kernel_sigset(guest, mask_addr)
                            .await
                            .ok()
                            .map(kernel_installed_signal_mask),
                        None => None,
                    }
                }
                _ => None,
            };
            return self
                .record_or_replay_blocking_with_mask(
                    guest,
                    Syscall::Pselect6(call),
                    blocked_signal_mask,
                )
                .await;
        }

        // Linux wraps pselect6's temporary mask in { pointer, size }. Glibc supplies
        // the wrapper even when the inner pointer is null. A real mask must stay in
        // effect for the whole wait so an unblocked signal (make's jobserver unblocks
        // SIGCHLD) can interrupt it. Previously that forced the external-blocking path,
        // whose completion timing is host-decided and is a source of `make -jN`
        // execution-log divergence. With SIGCHLD admission now deterministic (scheduler
        // `sigchld_deferred`/`sigchld_ready`), honor the mask on each deterministic poll
        // probe instead: a pending unblocked signal is observed at a scheduler-decided
        // probe point rather than at host signal-arrival time.
        let sigmask = if let Some(argument) = sigmask_argument {
            if argument.sigmask != 0 {
                if argument.sigsetsize != KERNEL_SIGSET_SIZE {
                    return Err(Errno::EINVAL.into());
                }
                let mask_addr =
                    Addr::<libc::sigset_t>::from_raw(argument.sigmask).ok_or(Errno::EFAULT)?;
                let mask = read_kernel_sigset(guest, mask_addr).await?;
                Some(sanitize_ppoll_signal_mask(mask))
            } else {
                None
            }
        } else {
            None
        };
        // The inner mask was snapshotted above. Do not let later guest mutations of the
        // outer wrapper change the meaning of a retry probe.
        let call = call.with_sigmask(None);

        self.handle_internal_pselect6(guest, call, timeout, sigmask)
            .await
    }

    async fn handle_internal_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        timeout: Option<Duration>,
        sigmask: Option<u64>,
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
        // pselect6's timeout is a writable in-out kernel timespec, so the probe
        // needs a mutable scratch cell (re-zeroed each iteration below to keep
        // every probe a non-blocking poll).
        let probe_timeout = stack.reserve::<Timespec>();
        // Carry the temporary signal mask on every zero-timeout probe so the kernel
        // applies it atomically: a pending, mask-unblocked signal makes the probe return
        // EINTR at a deterministic scheduler point. The probe's wrapper points at scratch
        // memory the guard keeps alive across each injection.
        let probe_sigmask = sigmask.map(|mask| {
            let sigset = stack.push(mask);
            stack
                .push(Pselect6SigmaskArg {
                    sigmask: sigset.as_raw(),
                    sigsetsize: KERNEL_SIGSET_SIZE,
                })
                .cast()
        });
        // Without a temporary mask, on a backend whose kernel reports the guest's
        // signal state, only a signal that would end the wait natively does; the mask
        // cell lets the wait block signals under the one scratch-stack guard
        // (https://github.com/rrnewton/hermit/issues/3146). A restart resumes from
        // the remaining time written back below, so a default stop ends the wait
        // as on Linux. Unlike the other waits, it holds no signal until the call
        // returns, so a pending caught `SIGCHLD` ends it as on Linux, whoever
        // sent it, at a turn that host timing can choose: a known gap that this
        // wait had before (`KernelSignalWait::for_select`).
        let mut signals = (sigmask.is_none()
            && self.cfg.backend_supports_blocked_wait_signal_interruption)
            .then(|| KernelSignalWait::for_select(guest));
        let mask_cell = signals.as_ref().map(|_| stack.reserve::<KernelSigset>());
        let _guard = stack.commit()?;
        let probe = call
            .with_readfds(readfds)
            .with_writefds(writefds)
            .with_exceptfds(exceptfds)
            .with_timeout(Some(probe_timeout))
            .with_sigmask(probe_sigmask);

        let mut resources = Resources::new(guest.thread_state().dettid);
        resources.insert(ResourceID::InternalIOPolling, Permission::W);
        resources.fyi("pselect6");
        resources.poll_deadline = poll_deadline_for(guest, deadline);
        // Keep the request metadata accurate, but do not make it eligible for
        // the scheduler's ERESTARTSYS wakeup. A cross-task signal must first be
        // checked against pselect6's snapshotted temporary mask and disposition.
        resources.set_signal_interrupt_errno(Errno::EINTR);

        // Every exit, including a `?` on a guest-memory write, must reach the mask
        // restore below, so the loop runs in its own async block.
        let result: Result<i64, Error> = {
            // Moved reborrows: a future holding a shared reference to a scratch
            // address would not be `Send`.
            let guest = &mut *guest;
            let signals = &mut signals;
            let resources = &mut resources;
            async move {
                loop {
                    let signaled = matches!(
                        resource_request(guest, resources.clone()).await,
                        ResumeStatus::Signaled(_)
                    );
                    // Every turn probes before it looks at pending signals, as Linux
                    // does: do_select polls every descriptor on each pass and returns
                    // the count of ready ones before it tests signal_pending, and at
                    // the start max_select_fd returns EBADF for a descriptor that is
                    // not open (`core_sys_select`). So a descriptor that becomes ready
                    // in a later turn in which a signal is also pending is reported
                    // (round-12 Medium on https://github.com/rrnewton/hermit/pull/3361).
                    // Each probe is a new select call, so a descriptor closed during
                    // the wait gets EBADF, where Linux reports it ready. Without the
                    // kernel's signal state, a scheduler `Signaled` answer still ends
                    // the call before the probe.
                    if signals.is_none() && signaled {
                        self.write_pselect6_remaining(guest, call, deadline).await?;
                        return Err(Errno::EINTR.into());
                    }
                    guest.memory().write_value(
                        probe_timeout,
                        &Timespec {
                            tv_sec: 0,
                            tv_nsec: 0,
                        },
                    )?;
                    write_pselect6_fd_set(guest, probe.readfds(), &original_readfds)?;
                    write_pselect6_fd_set(guest, probe.writefds(), &original_writefds)?;
                    write_pselect6_fd_set(guest, probe.exceptfds(), &original_exceptfds)?;

                    let raw_result = match signals.as_mut() {
                        // A signal stop that does not end the wait is absorbed (see
                        // `KernelSignalWait`). Every probe runs before the turn's check
                        // (`inject_first_probe`): a signal that would end the wait and
                        // stops the probe is held, the probe runs again, and the signal
                        // is delivered as the call returns. Once `block` has blocked
                        // every blockable signal, a pending one cannot stop the probe.
                        Some(signals) => match signals.inject_first_probe(guest, probe).await {
                            Ok(result) => result,
                            Err(error) => {
                                self.write_pselect6_remaining(guest, call, deadline).await?;
                                break Err(error);
                            }
                        },
                        None => guest.inject(probe).await,
                    };
                    let result = pselect6_probe_result(raw_result);
                    if result != Ok(0) {
                        let copy_result = if result.is_ok() {
                            self.copy_pselect6_results(guest, probe, call, len)
                        } else {
                            Ok(())
                        };
                        self.write_pselect6_remaining(guest, call, deadline).await?;
                        copy_result?;
                        break result.map_err(Into::into);
                    }
                    // The turn's check, after its probe found no descriptor ready and no
                    // error: only then does a pending signal end the call. A scheduler
                    // `Signaled` answer is only a hint here; the kernel's state decides.
                    // pselect6 returns ERESTARTNOHAND: EINTR after a handler, a restart
                    // with the remaining timeout after a stop.
                    if let Some(signals) = signals.as_ref() {
                        match signals.interrupted_with_state() {
                            Ok((false, _)) => {}
                            Ok((true, _)) => {
                                self.write_pselect6_remaining(guest, call, deadline).await?;
                                break Err(Errno::ERESTARTNOHAND.into());
                            }
                            Err(error) => break Err(error),
                        }
                    }
                    if let Some(signals) = signals.as_mut()
                        && signals.needs_block()
                        && let Err(error) = signals.block(guest, mask_cell).await
                    {
                        self.write_pselect6_remaining(guest, call, deadline).await?;
                        break Err(error);
                    }

                    resources.poll_attempt += 1;
                    if let Some(deadline) = deadline
                        && thread_observe_time(guest).await >= deadline
                    {
                        let copy_result = self.copy_pselect6_results(guest, probe, call, len);
                        self.write_pselect6_remaining(guest, call, Some(deadline))
                            .await?;
                        copy_result?;
                        break Ok(0);
                    }
                    trace!(
                        "Retry #{} for syscall due to result Ok(0): {}",
                        resources.poll_attempt,
                        probe.display(&guest.memory())
                    );
                    record_retry_event(guest, probe).await;
                }
            }
            .await
        };
        let result = match &mut signals {
            Some(signals) => result_after_restore(signals.restore(guest, mask_cell).await, result)?,
            None => result,
        };
        refuse_held_signal_loss(guest, Sysno::pselect6, result).await
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

    pub(super) async fn write_pselect6_remaining<G: Guest<Self>>(
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
            // pselect6's timeout is a writable in-out kernel timespec; reverie-syscalls
            // now types it as `AddrMut<Timespec>`, so the remaining time can be written
            // back directly without an unsafe pointer cast.
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
        if self.cfg.recordreplay_modes {
            let zero_timeout = call.timeout().is_some_and(|timeout| {
                guest
                    .memory()
                    .read_value(timeout)
                    .is_ok_and(|timeout: libc::timeval| timeout.tv_sec == 0 && timeout.tv_usec == 0)
            });
            return self
                .record_or_replay_select_family(guest, Syscall::Select(call), zero_timeout)
                .await;
        }
        if !self.cfg.sequentialize_threads {
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
        // On a backend whose kernel reports the guest's signal state, only a signal
        // that would end the wait natively does; the mask cell lets the wait block
        // signals under the one scratch-stack guard
        // (https://github.com/rrnewton/hermit/issues/3146). A restart resumes from
        // the remaining time written back below, so a default stop ends the wait
        // as on Linux. Unlike the other waits, it holds no signal until the call
        // returns, so a pending caught `SIGCHLD` ends it as on Linux, whoever
        // sent it, at a turn that host timing can choose: a known gap that this
        // wait had before (`KernelSignalWait::for_select`).
        let mut signals = self
            .cfg
            .backend_supports_blocked_wait_signal_interruption
            .then(|| KernelSignalWait::for_select(guest));
        let mask_cell = signals.as_ref().map(|_| stack.reserve::<KernelSigset>());
        let _guard = stack.commit()?;
        let probe = call
            .with_readfds(readfds)
            .with_writefds(writefds)
            .with_exceptfds(exceptfds)
            .with_timeout(Some(probe_timeout));

        let mut resources = Resources::new(guest.thread_state().dettid);
        resources.insert(ResourceID::InternalIOPolling, Permission::W);
        resources.fyi("select");
        resources.poll_deadline = poll_deadline_for(guest, deadline);
        // EINTR records select's interruption result. The scheduler only wakes
        // ERESTARTSYS requests: blocked and ignored signals still need a target-
        // side disposition check before this Signaled path can be used.
        resources.set_signal_interrupt_errno(Errno::EINTR);

        // Every exit, including a `?` on a guest-memory write, must reach the mask
        // restore below, so the loop runs in its own async block.
        let result: Result<i64, Error> = {
            // Moved reborrows: a future holding a shared reference to a scratch
            // address would not be `Send`.
            let guest = &mut *guest;
            let signals = &mut signals;
            let resources = &mut resources;
            async move {
                loop {
                    let signaled = matches!(
                        resource_request(guest, resources.clone()).await,
                        ResumeStatus::Signaled(_)
                    );
                    // Every turn probes before it looks at pending signals, as Linux
                    // does: do_select polls every descriptor on each pass and returns
                    // the count of ready ones before it tests signal_pending, and at
                    // the start max_select_fd returns EBADF for a descriptor that is
                    // not open (`core_sys_select`). So a descriptor that becomes ready
                    // in a later turn in which a signal is also pending is reported
                    // (round-12 Medium on https://github.com/rrnewton/hermit/pull/3361).
                    // Each probe is a new select call, so a descriptor closed during
                    // the wait gets EBADF, where Linux reports it ready. Without the
                    // kernel's signal state, a scheduler `Signaled` answer still ends
                    // the call before the probe.
                    if signals.is_none() && signaled {
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

                    let result = match signals.as_mut() {
                        // A signal stop that does not end the wait is absorbed (see
                        // `KernelSignalWait`). Every probe runs before the turn's check
                        // (`inject_first_probe`): a signal that would end the wait and
                        // stops the probe is held, the probe runs again, and the signal
                        // is delivered as the call returns. Once `block` has blocked
                        // every blockable signal, a pending one cannot stop the probe.
                        Some(signals) => match signals.inject_first_probe(guest, probe).await {
                            Ok(result) => result,
                            Err(error) => {
                                self.write_select_remaining(guest, call, deadline).await?;
                                break Err(error);
                            }
                        },
                        None => guest.inject(probe).await,
                    };
                    if result != Ok(0) {
                        let copy_result = if result.is_ok() {
                            self.copy_select_results(guest, probe, call, len)
                        } else {
                            Ok(())
                        };
                        self.write_select_remaining(guest, call, deadline).await?;
                        copy_result?;
                        break result.map_err(Into::into);
                    }
                    // The turn's check, after its probe found no descriptor ready and no
                    // error: only then does a pending signal end the call. A scheduler
                    // `Signaled` answer is only a hint here; the kernel's state decides.
                    // select returns ERESTARTNOHAND: EINTR after a handler, a restart
                    // with the remaining timeout after a stop.
                    if let Some(signals) = signals.as_ref() {
                        match signals.interrupted_with_state() {
                            Ok((false, _)) => {}
                            Ok((true, _)) => {
                                self.write_select_remaining(guest, call, deadline).await?;
                                break Err(Errno::ERESTARTNOHAND.into());
                            }
                            Err(error) => break Err(error),
                        }
                    }
                    if let Some(signals) = signals.as_mut()
                        && signals.needs_block()
                        && let Err(error) = signals.block(guest, mask_cell).await
                    {
                        self.write_select_remaining(guest, call, deadline).await?;
                        break Err(error);
                    }

                    resources.poll_attempt += 1;
                    if let Some(deadline) = deadline
                        && thread_observe_time(guest).await >= deadline
                    {
                        let copy_result = self.copy_select_results(guest, probe, call, len);
                        self.write_select_remaining(guest, call, Some(deadline))
                            .await?;
                        copy_result?;
                        break Ok(0);
                    }
                    trace!(
                        "Retry #{} for syscall due to result Ok(0): {}",
                        resources.poll_attempt,
                        probe.display(&guest.memory())
                    );
                    record_retry_event(guest, probe).await;
                }
            }
            .await
        };
        let result = match &mut signals {
            Some(signals) => result_after_restore(signals.restore(guest, mask_cell).await, result)?,
            None => result,
        };
        refuse_held_signal_loss(guest, Sysno::select, result).await
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

    pub(super) async fn write_select_remaining<G: Guest<Self>>(
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
            // Linux does not write back an initially zero timeout. Besides matching the
            // kernel, omitting this write matters when the timeout aliases the pollfd array:
            // the injected probe may have just stored revents in those same bytes.
            result
        } else if ppoll_uses_kernel_wait(
            self.cfg.sequentialize_threads,
            self.cfg.recordreplay_modes,
            call.sigmask().is_some(),
        ) {
            // The kernel owns the blocking wait when threads are not sequentialized and for
            // unmasked record/replay calls. A masked sequentialized wait must use the probe
            // below so a call that would block keeps the strict fail-closed behavior.
            // Use scratch memory only for the signal mask so raw ppoll can still update the
            // guest timeout.
            let mut signal_mask_guard = None;
            // The mask the real call sleeps under, recorded for the scheduler's
            // SIGCHLD target choice (`Resources::blocked_signal_mask`).
            let mut blocked_signal_mask = None;
            let call = if let Some(signal_mask) = call.sigmask() {
                if call.sigsetsize() != KERNEL_SIGSET_SIZE {
                    return Err(Errno::EINVAL.into());
                }
                let signal_mask = read_kernel_sigset(guest, signal_mask).await?;
                let signal_mask = sanitize_ppoll_signal_mask(signal_mask);
                blocked_signal_mask = Some(kernel_installed_signal_mask(signal_mask));
                let mut stack = guest.stack().await;
                let signal_mask = stack.push(signal_mask).cast();
                signal_mask_guard = Some(stack.commit()?);
                call.with_sigmask(Some(signal_mask))
            } else {
                call
            };
            let result = Ok(self
                .record_or_replay_blocking_with_mask(
                    guest,
                    Syscall::Ppoll(call),
                    blocked_signal_mask,
                )
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
                let signal_mask = read_kernel_sigset(guest, signal_mask).await?;
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
            let result = if self.cfg.recordreplay_modes {
                self.record_or_replay(guest, probe).await
            } else {
                guest.inject_with_retry(probe).await
            };
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

    pub(super) async fn write_ppoll_remaining<G: Guest<Self>>(
        &self,
        guest: &mut G,
        timeout_address: AddrMut<'_, Timespec>,
        timeout: Duration,
        started_at: LogicalTime,
    ) -> Result<(), Error> {
        let now = thread_observe_time(guest).await;
        let elapsed = Duration::from_nanos(now.as_nanos().saturating_sub(started_at.as_nanos()));
        let remaining = timeout.saturating_sub(elapsed);
        if let Err(error) = guest
            .memory()
            .write_value(timeout_address, &timespec_from_duration(remaining))
        {
            // Linux preserves the ppoll result when remaining-time copyout faults.
            trace!(?error, "ignoring ppoll timeout writeback failure");
        }
        Ok(())
    }

    /// Handle a guest-internal poll call that can be fully determinized.
    // TODO-HUMAN-REVIEW(PR-1052): Review scheduler fairness for zero-timeout poll.
    pub async fn handle_internal_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        let timeout_millis = call.timeout();
        if timeout_millis == 0 {
            // A nonblocking poll can still be the synchronization point in a
            // userspace polling loop. Yield once before probing so backends
            // without PMU preemption cannot let that loop starve its producer.
            let yield_to_peer = self.cfg.backend.loopback_pollers_yield_to_peers
                && guest.thread_state().has_loopback_peer();
            resource_request(
                guest,
                zero_timeout_poll_request(guest.thread_state().dettid, yield_to_peer),
            )
            .await;
            Ok(guest.inject(call).await?) // Already non-blocking.
        } else {
            let maybe_timeout_ns = millis_duration_to_absolute_timeout(guest, timeout_millis).await;
            self.handle_internal_poll_until(guest, call, maybe_timeout_ns)
                .await
        }
    }

    /// The blocking half of `handle_internal_poll`, waiting until `deadline`. A
    /// `restart_syscall` resumes an interrupted `poll` here with the deadline the
    /// wait kept, as Linux's `do_restart_poll` does
    /// (`Detcore::handle_restart_syscall`).
    pub(crate) async fn handle_internal_poll_until<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
        deadline: Option<LogicalTime>,
    ) -> Result<i64, Error> {
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("poll");
        let result = retry_nonblocking_syscall_with_timeout(guest, call, rsrc, deadline).await;
        keep_restart_block(guest, &result, RestartCall::Poll(call), deadline).await;
        result
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
        // This used to unconditionally inject the raw
        // call and wait for it to return. With an infinite timeout under
        // `--sequentialize-threads` that DEADLOCKS the whole guest: the calling
        // task holds the scheduler turn while blocked in the kernel, and the
        // only task that could ever satisfy the wait is sitting in the run queue
        // waiting for a turn that never comes. Observed as `cmake` configure
        // hanging forever at zero CPU with a grandchild frozen mid-`openat`;
        // hermit's own scheduler log ends at `COMMIT turn N, dettid <parent>`
        // injecting `epoll_pwait(..., -1, NULL, 8)` with `queue len 2`.
        //
        // WHO ACTUALLY REACHES THIS, measured rather than assumed. It is NOT
        // glibc's `epoll_wait(2)` on this architecture: glibc calls
        // `SYS_epoll_pwait` only where `__NR_epoll_wait` does not exist (arm64
        // and friends). x86_64 has it, and `strace` on glibc 2.34/x86_64 shows
        // a plain `epoll_wait` syscall, which `handle_epoll_wait` has always
        // handled correctly. The callers that land here are programs issuing
        // `epoll_pwait` DIRECTLY -- libuv does, which is how the original
        // `cmake` hang was found. With a NULL sigmask the two calls are
        // semantically identical, so route them together.
        //
        // A NON-NULL sigmask keeps the previous behavior: its whole purpose is
        // to swap the signal mask atomically for the duration of the wait, and
        // a timeout-0 polling loop cannot reproduce that atomicity. Such calls
        // remain able to block the scheduler; that is a known remaining gap
        // rather than something this change silently pretends to fix. Because
        // such a call keeps its turn, it never sleeps outside the runnable set,
        // so the scheduler needs no record of its temporary mask
        // (`Resources::blocked_signal_mask`).
        if call.sigmask().is_some() {
            let dettid = guest.thread_state().dettid;
            resource_request(guest, Resources::new(dettid)).await; // empty request
            return Ok(self.record_or_replay(guest, call).await?);
        }
        if self.cfg.recordreplay_modes && call.timeout() == 0 {
            // Cannot block, but still yield a scheduler turn so a polling thread
            // cannot monopolize the guest between preemptions.
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            Ok(self
                .record_or_replay_blocking(guest, Syscall::EpollPwait(call))
                .await?)
        } else {
            self.handle_internal_epoll_pwait(guest, call).await
        }
    }

    /// Handle a guest-internal `epoll_pwait` (NULL sigmask) that can be fully
    /// determinized. Mirrors `handle_internal_epoll_wait`.
    pub async fn handle_internal_epoll_pwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollPwait,
    ) -> Result<i64, Error> {
        let timeout_millis = call.timeout();
        if timeout_millis == 0 {
            // Cannot block, but must still yield a scheduler turn: a
            // zero-timeout polling loop that never requests a resource can
            // monopolize the guest between preemptions and starve the producer
            // it is polling for. `handle_poll` takes a turn for every
            // sequential mode, and the record/replay arm of `handle_epoll_pwait`
            // does the same; before this PR routed NULL-sigmask `epoll_pwait`
            // here, the old handler always made an empty request. Omitting it
            // only on the plain-strict path would be a scheduling regression,
            // not a refactor.
            if self.cfg.sequentialize_threads {
                let yield_to_peer = self.cfg.backend.loopback_pollers_yield_to_peers
                    && guest.thread_state().has_loopback_peer();
                resource_request(
                    guest,
                    zero_timeout_poll_request(guest.thread_state().dettid, yield_to_peer),
                )
                .await;
            }
            Ok(guest.inject(call).await?) // Already non-blocking.
        } else {
            let maybe_timeout_ns = millis_duration_to_absolute_timeout(guest, timeout_millis).await;
            let mut rsrc = Resources::new(guest.thread_state().dettid);
            rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
            rsrc.fyi("epoll_pwait");
            retry_nonblocking_syscall_with_timeout(guest, call, rsrc, maybe_timeout_ns).await
        }
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
        let fd = call.fd();
        let uservaddr = call.uservaddr();
        let addrlen = call.addrlen();

        if guest.config().sched_heuristic == SchedHeuristic::ConnectBind {
            trace!("Scheduling heuristic: reprioritizing connect");
            let resource = ResourceID::PriorityChangePoint(
                FIRST_PRIORITY,
                guest.thread_state().thread_logical_time.as_nanos(),
                guest.thread_state().committed_clock_value,
                Vec::new(),
            );
            let req = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, req).await;
        }

        let result = self.execute_nonblockable_fd_syscall(guest, call).await;
        if self.cfg.backend.loopback_pollers_yield_to_peers
            && connect_result_allows_peer_classification(&result)
        {
            // This metadata is a loopback-poll scheduling hint, not part of connect's semantics.
            // Let the kernel establish the authoritative result first, then classify the peer
            // best-effort so an invalid guest pointer or an untracked fd can never replace the
            // kernel's errno.
            let loopback_peer = (|| -> Result<Option<bool>, Error> {
                let Some(address) = uservaddr else {
                    return Ok(None);
                };
                let addrlen = usize::try_from(addrlen).unwrap_or(0);
                if addrlen < std::mem::size_of::<u16>() {
                    return Ok(None);
                }
                let family: u16 = guest.memory().read_value(address.cast())?;
                if family == libc::AF_INET as u16 {
                    if addrlen < std::mem::size_of::<libc::sockaddr_in>() {
                        return Ok(None);
                    }
                    let address: libc::sockaddr_in = guest.memory().read_value(address.cast())?;
                    Ok(Some(
                        Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes()).is_loopback(),
                    ))
                } else if family == libc::AF_INET6 as u16 {
                    if addrlen < std::mem::size_of::<libc::sockaddr_in6>() {
                        return Ok(None);
                    }
                    let address: libc::sockaddr_in6 = guest.memory().read_value(address.cast())?;
                    Ok(Some(
                        Ipv6Addr::from(address.sin6_addr.s6_addr).is_loopback(),
                    ))
                } else {
                    // This includes a successful AF_UNSPEC disconnect and successful connects
                    // to non-IP families, neither of which has a loopback IP peer.
                    Ok(Some(false))
                }
            })()
            .ok()
            .flatten();
            if let Some(loopback_peer) = loopback_peer {
                let _ = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.set_loopback_peer(loopback_peer));
            }
        }

        result
    }

    /// Handles sendto, sendmsg, and sendmmsg syscalls (MAYHANG).
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

    /// Sends one message and invalidates process-wide flock knowledge after success.
    ///
    /// The guest can mutate shared message and control memory while this helper
    /// deschedules. Parsing before the syscall would not prove which descriptors the
    /// kernel later transferred, so a successful unbound send conservatively makes
    /// every cached open-file-description lock mode unknown.
    pub async fn handle_sendmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sendmsg,
    ) -> Result<i64, Error> {
        if self.accept_in_turn_enabled() {
            let headers: Vec<usize> = call
                .msg()
                .map(|header| header.as_raw())
                .into_iter()
                .collect();
            self.mark_exported_listeners(guest, &headers);
        }
        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        guest.thread_state().forget_flock_modes();
        Ok(result)
    }

    /// Mark every accept-in-turn listener (`DetFd::is_accept_in_turn_eligible`)
    /// these messages name in `SCM_RIGHTS` as exported, before they are sent
    /// and whatever the send returns: Linux may have queued the descriptor
    /// even when the call reports an error, and both phases must decide alike.
    /// `headers` are the guest addresses of the `msghdr`s: one for `sendmsg`,
    /// one per entry Linux could send for `sendmmsg`.
    ///
    /// An exported listener may be held by a process outside Detcore's view,
    /// so its accepts never take the temporary `O_NONBLOCK` again and keep the
    /// backgrounded path. An accept already waiting on it refuses at its next
    /// attempt (`ACCEPT_EXPORT_REFUSAL`). Both phases read the same guest
    /// memory here. When a header or control buffer cannot be read in full
    /// (Linux reads a write-only page, which Detcore cannot, and takes up to
    /// `optmem_max` bytes of control data, more than `MAX_CONTROL_BYTES`),
    /// every eligible listener in the table counts as exported. A thread that
    /// rewrites the control buffer while this one is descheduled can send a
    /// listener this did not see.
    // TODO-HUMAN-REVIEW(PR-3908)
    fn mark_exported_listeners<G: Guest<Self>>(&self, guest: &mut G, headers: &[usize]) {
        let mut sent: Vec<RawFd> = Vec::new();
        let mut unread = false;
        for header in headers {
            // A null header or control buffer fails the call with EFAULT
            // before Linux reads any descriptor.
            let Some(header) = Addr::<libc::msghdr>::from_raw(*header) else {
                continue;
            };
            let Ok(header) = guest.memory().read_value::<_, libc::msghdr>(header) else {
                unread = true;
                continue;
            };
            let Some(control_address) = Addr::<u8>::from_raw(header.msg_control as usize) else {
                continue;
            };
            if header.msg_controllen > MAX_CONTROL_BYTES {
                unread = true;
                continue;
            }
            let mut control = vec![0; header.msg_controllen];
            if guest
                .memory()
                .read_exact(control_address, &mut control)
                .is_err()
            {
                unread = true;
                continue;
            }
            sent.extend(received_descriptors(&control));
        }
        if unread {
            // Linux can read memory Detcore cannot (a write-only page) and
            // accepts more control data than Detcore reads, so a message it
            // could not read may carry any listener.
            guest
                .thread_state()
                .mark_accept_in_turn_listeners_exported();
            return;
        }
        for fd in sent {
            let _ = guest.thread_state().with_detfd(fd, |detfd| {
                if detfd.is_accept_in_turn_eligible() {
                    detfd.set_exported();
                }
            });
        }
    }

    /// Sends a message batch and invalidates process-wide flock knowledge when the
    /// kernel reports at least one message sent. This intentionally includes
    /// descriptors named only by an unsent tail message: the mutable guest array is
    /// not stable across a possible deschedule, so narrower attribution is unsafe.
    pub async fn handle_sendmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sendmmsg,
    ) -> Result<i64, Error> {
        if self.cfg.recordreplay_modes && is_accepted_connection(guest, call.sockfd()) {
            return self.refuse_log_served_mmsg(guest, Sysno::sendmmsg).await;
        }
        if self.accept_in_turn_enabled() {
            // Every entry Linux could send, sent or not: see `mark_exported_listeners`.
            // Reverie types the vector as `msghdr`s, but its entries are
            // `mmsghdr`s, each a `msghdr` followed by `msg_len`, so step by
            // the larger size.
            let headers: Vec<usize> = call.msgvec().map_or_else(Vec::new, |vector| {
                (0..call.vlen().min(libc::UIO_MAXIOV as u32) as usize)
                    .map(|index| vector.as_raw() + index * std::mem::size_of::<libc::mmsghdr>())
                    .collect()
            });
            self.mark_exported_listeners(guest, &headers);
        }
        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        if result > 0 {
            guest.thread_state().forget_flock_modes();
        }
        Ok(result)
    }

    /// Record and replay keep an accepted connection only in the log, and
    /// replay stands in an eventfd for it. Neither recorder nor replayer
    /// records the batched `sendmmsg` and `recvmmsg`, so replay would run them
    /// live against that eventfd and return something other than what the
    /// recording saw. Refuse them in both modes instead. On other sockets
    /// they still run live
    /// (https://github.com/rrnewton/hermit/issues/3883).
    // TODO-HUMAN-REVIEW(PR-3871): Refuse unrecorded batched I/O on accepted connections.
    async fn refuse_log_served_mmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
    ) -> Result<i64, Error> {
        self.refuse_unserviceable_operation(guest, sysno, Errno::EOPNOTSUPP)
            .await
    }

    // TODO-HUMAN-REVIEW(PR-912): Review receive-time capture across socket aliases.
    async fn observe_socket_receive<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
    ) -> Result<LogicalTime, Error> {
        let timestamp = thread_observe_time(guest).await;
        guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_socket_receive_timestamp(timestamp);
        })?;
        Ok(timestamp)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-901)
    /// Receive one message and replace host socket timestamps with logical time.
    pub async fn handle_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvmsg,
    ) -> Result<i64, Error> {
        // NETLINK_SOCK_DIAG replies carry host-assigned socket identities in
        // their msg_iov payload (not msg_control). Canonicalize the supported
        // fields before the binary reply reaches the guest.
        // The predicate is shared with the read/readv/recvfrom/recvmmsg paths so
        // the five receive syscalls cannot drift apart again.
        if self.sock_diag_reply_fd(guest, call.sockfd()) {
            return self.handle_sock_diag_recvmsg(guest, call).await;
        }

        if !self.cfg.virtualize_time {
            return self.execute_nonblockable_fd_syscall(guest, call).await;
        }

        let Some(message_address) = call.msg() else {
            return self
                .handle_socket_receive(guest, call, call.sockfd(), true)
                .await;
        };
        // Snapshot every input field before the receive. Linux permits the
        // control buffer to overlap this header, so rereading it afterward can
        // turn a successful consuming receive into an artificial EFAULT.
        let message: libc::msghdr = guest.memory().read_value(message_address)?;
        if message.msg_control.is_null() || message.msg_controllen == 0 {
            return self
                .handle_socket_receive(guest, call, call.sockfd(), true)
                .await;
        }
        let control_len = message.msg_controllen.min(MAX_CONTROL_BYTES);
        let control_address: AddrMut<'_, u8> =
            AddrMut::from_raw(message.msg_control as usize).ok_or(Errno::EFAULT)?;
        let mut control = vec![0; control_len];
        // Validate the output region before consuming a datagram.
        guest.memory().read_exact(control_address, &mut control)?;

        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        let now = self.observe_socket_receive(guest, call.sockfd()).await?;
        let mut control = vec![0; control_len];
        guest.memory().read_exact(control_address, &mut control)?;
        if socket_timestamp_messages(&control).is_empty() {
            return Ok(result);
        }

        canonicalize_socket_timestamps(&mut control, now);
        guest.memory().write_exact(control_address, &control)?;
        Ok(result)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1064)
    /// Whether replies received on `fd` must have their supported socket
    /// identities canonicalized.
    ///
    /// This is the single predicate every receive path consults. It exists as
    /// one function because the flag used to be tested inline in `recvmsg`
    /// alone, and four other receive syscalls reached the same dump without it
    /// (see [`Self::sanitize_sock_diag_segments`]).
    pub(crate) fn sock_diag_reply_fd<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) -> bool {
        self.cfg.virtualize_metadata
            && guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.is_sock_diag() || detfd.is_netlink_route())
                .unwrap_or(false)
    }

    /// Whether this descriptor is specifically a `NETLINK_ROUTE` socket, which
    /// needs the link-counter sanitizer rather than the sock-diag one.
    fn netlink_route_reply_fd<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) -> bool {
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.is_netlink_route())
            .unwrap_or(false)
    }

    /// Read an `iovec` array out of guest memory as plain `(address, capacity)`
    /// scalars, so no non-`Send` raw pointer is held across an await.
    fn read_iov_segments<G: Guest<Self>>(
        guest: &mut G,
        iov: usize,
        iovlen: usize,
    ) -> Result<Vec<(usize, usize)>, Error> {
        if iov == 0 || iovlen == 0 {
            return Ok(Vec::new());
        }
        let count = iovlen.min(libc::UIO_MAXIOV as usize);
        let address: AddrMut<'_, libc::iovec> = AddrMut::from_raw(iov).ok_or(Errno::EFAULT)?;
        // SAFETY: `iovec` is a plain C record; an all-zero value is a valid
        // staging value immediately overwritten by `read_values`.
        let mut iovecs: Vec<libc::iovec> =
            (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
        guest.memory().read_values(address.into(), &mut iovecs)?;
        Ok(iovecs
            .iter()
            .map(|iov| (iov.iov_base as usize, iov.iov_len))
            .collect())
    }

    /// Canonicalize host-assigned identities in a `NETLINK_SOCK_DIAG` reply that
    /// the kernel has already written into guest memory.
    ///
    /// `segments` describes the destination buffers as `(address, capacity)` in
    /// the order the kernel filled them; `received` is the syscall's return
    /// value. The reply is gathered contiguously (it may be scattered across
    /// several buffers), sanitized via `crate::sock_diag` (fail-open,
    /// zero-only, never resizes), and written back preserving the original
    /// boundaries. Bounded by both each buffer's capacity and `received`, which
    /// under `MSG_TRUNC` can exceed the total capacity.
    ///
    /// Synchronous on purpose: every caller has already completed its receive,
    /// so nothing here awaits and no guest address outlives the borrow.
    fn sanitize_sock_diag_segments<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        segments: &[(usize, usize)],
        received: usize,
    ) -> Result<(), Error> {
        if received == 0 || segments.is_empty() {
            return Ok(());
        }
        let mut filled: Vec<(AddrMut<'_, u8>, usize)> = Vec::new();
        let mut buffer: Vec<u8> = Vec::with_capacity(received);
        let mut remaining = received;
        for &(base, capacity) in segments {
            if remaining == 0 {
                break;
            }
            if base == 0 || capacity == 0 {
                continue;
            }
            let take = capacity.min(remaining);
            let address: AddrMut<'_, u8> = AddrMut::from_raw(base).ok_or(Errno::EFAULT)?;
            let mut segment = vec![0u8; take];
            guest.memory().read_exact(address, &mut segment)?;
            buffer.extend_from_slice(&segment);
            filled.push((address, take));
            remaining -= take;
        }

        // NETLINK_ROUTE and NETLINK_SOCK_DIAG replies need different
        // sanitizers: one zeroes live interface counters, the other determinizes
        // supported socket identities. The descriptor decides which, so a guest
        // holding both kinds of socket gets each handled correctly.
        let modified = if self.netlink_route_reply_fd(guest, fd) {
            crate::netlink_route::sanitize_route_link_stats(&mut buffer)
        } else {
            crate::sock_diag::sanitize_sock_diag_identities(&mut buffer)
        };
        if !modified {
            return Ok(());
        }

        let mut offset = 0;
        for (address, len) in filled {
            guest
                .memory()
                .write_exact(address, &buffer[offset..offset + len])?;
            offset += len;
        }
        Ok(())
    }

    /// `recvfrom` on a socket-diag descriptor: one destination buffer.
    ///
    /// `recv(2)` has no syscall of its own on x86_64 — glibc lowers it to
    /// `recvfrom` with a null address — so this covers `recv` as well, which is
    /// what Python's `socket.recv()` reaches.
    pub async fn handle_sock_diag_recvfrom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvfrom,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let base = call.buf().map(|address| address.as_raw()).unwrap_or(0);
        let len = call.len();
        let result = self.handle_socket_receive(guest, call, fd, true).await?;
        let received = usize::try_from(result).unwrap_or(0);
        self.sanitize_sock_diag_segments(guest, fd, &[(base, len)], received)?;
        Ok(result)
    }

    /// `read` on a socket-diag descriptor: one destination buffer.
    pub async fn handle_sock_diag_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let base = call.buf().map(|address| address.as_raw()).unwrap_or(0);
        let len = call.len();
        let result = self.handle_read(guest, call).await?;
        let received = usize::try_from(result).unwrap_or(0);
        self.sanitize_sock_diag_segments(guest, fd, &[(base, len)], received)?;
        Ok(result)
    }

    /// Receive a `NETLINK_SOCK_DIAG` dump and zero the host-assigned socket
    /// inode numbers in the reply so `ss`-style enumeration is deterministic.
    ///
    /// The dump lands in `msg_iov` (netlink diag sockets carry no ancillary
    /// data), possibly scattered across several iovecs.
    async fn handle_sock_diag_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvmsg,
    ) -> Result<i64, Error> {
        let fd = call.sockfd();
        let Some(message_address) = call.msg() else {
            return self.handle_socket_receive(guest, call, fd, true).await;
        };
        // Snapshot the header's iovec pointer/count before the receive (they are
        // stable across it; the kernel fills the pointed-to buffers, not the
        // array). Scoped so the `msghdr`'s raw pointers are dropped before the
        // await below: holding one would make this future non-`Send`.
        let segments = {
            let message: libc::msghdr = guest.memory().read_value(message_address)?;
            Self::read_iov_segments(guest, message.msg_iov as usize, message.msg_iovlen)?
        };

        let result = self.handle_socket_receive(guest, call, fd, true).await?;
        let received = usize::try_from(result).unwrap_or(0);
        self.sanitize_sock_diag_segments(guest, fd, &segments, received)?;
        Ok(result)
    }

    /// `readv` on a socket-diag descriptor: an `iovec` array, no message header.
    pub async fn handle_sock_diag_readv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readv,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let iov = call.iov().map(|address| address.as_raw()).unwrap_or(0);
        let segments = Self::read_iov_segments(guest, iov, call.len())?;
        let result = self.handle_readv(guest, call).await?;
        let received = usize::try_from(result).unwrap_or(0);
        self.sanitize_sock_diag_segments(guest, fd, &segments, received)?;
        Ok(result)
    }

    /// `recvmmsg` on a socket-diag descriptor.
    ///
    /// Each delivered `mmsghdr` is a separate datagram with its own byte count
    /// in `msg_len`, so each is gathered and sanitized independently; treating
    /// the batch as one buffer would let one message's length run into the
    /// next message's memory.
    pub async fn handle_sock_diag_recvmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvmmsg,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let Some(messages_address) = call.mmsg() else {
            return self.handle_recvmmsg(guest, call).await;
        };
        let vlen = call.vlen();
        if vlen == 0 || vlen > libc::UIO_MAXIOV as u32 {
            return self.handle_recvmmsg(guest, call).await;
        }
        let base = messages_address.as_raw();

        // Snapshot each message's iovec geometry BEFORE the receive: the kernel
        // fills the pointed-to buffers and writes msg_len, but does not move the
        // iovec arrays themselves. Scoped, and reduced to plain scalars, so the
        // `mmsghdr` raw pointers are dropped before the await below: holding one
        // would make this future non-`Send`.
        let geometry: Vec<Vec<(usize, usize)>> = {
            // SAFETY: `mmsghdr` is a plain C record; an all-zero value is a
            // valid staging value immediately overwritten by `read_values`.
            let mut headers: Vec<libc::mmsghdr> = (0..vlen as usize)
                .map(|_| unsafe { std::mem::zeroed() })
                .collect();
            guest
                .memory()
                .read_values(messages_address.into(), &mut headers)?;
            let mut geometry = Vec::with_capacity(headers.len());
            for header in &headers {
                geometry.push(Self::read_iov_segments(
                    guest,
                    header.msg_hdr.msg_iov as usize,
                    header.msg_hdr.msg_iovlen,
                )?);
            }
            geometry
        };

        let result = self.handle_recvmmsg(guest, call).await?;
        let delivered = usize::try_from(result).unwrap_or(0).min(geometry.len());
        if delivered == 0 {
            return Ok(result);
        }

        // Re-read the array for the per-message byte counts the kernel just
        // wrote. Also scoped: nothing awaits past this point, but keeping the
        // raw pointers contained keeps the rule visible.
        let counts: Vec<usize> = {
            let address: AddrMut<'_, libc::mmsghdr> =
                AddrMut::from_raw(base).ok_or(Errno::EFAULT)?;
            // SAFETY: as above.
            let mut headers: Vec<libc::mmsghdr> = (0..vlen as usize)
                .map(|_| unsafe { std::mem::zeroed() })
                .collect();
            guest.memory().read_values(address.into(), &mut headers)?;
            headers.iter().map(|h| h.msg_len as usize).collect()
        };
        for (index, segments) in geometry.iter().enumerate().take(delivered) {
            self.sanitize_sock_diag_segments(guest, fd, segments, counts[index])?;
        }
        Ok(result)
    }

    // TODO-HUMAN-REVIEW(PR-912): Review receive-time capture across socket aliases.
    /// Handle a socket receive and retain one timestamp for every alias of its open file.
    pub async fn handle_socket_receive<
        G: Guest<Self>,
        C: SyscallInfo + NonblockableSyscall + Into<Syscall>,
    >(
        &self,
        guest: &mut G,
        call: C,
        fd: i32,
        zero_delivers_packet: bool,
    ) -> Result<i64, Error> {
        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        if self.cfg.virtualize_time && (result > 0 || (result == 0 && zero_delivers_packet)) {
            self.observe_socket_receive(guest, fd).await?;
        }
        Ok(result)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-901)
    /// Receive a message batch and replace every host socket timestamp with logical time.
    pub async fn handle_recvmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvmmsg,
    ) -> Result<i64, Error> {
        if self.cfg.recordreplay_modes && is_accepted_connection(guest, call.fd()) {
            return self.refuse_log_served_mmsg(guest, Sysno::recvmmsg).await;
        }
        if !self.cfg.virtualize_time || call.vlen() > libc::UIO_MAXIOV as u32 {
            return self.execute_nonblockable_fd_syscall(guest, call).await;
        }

        let Some(messages_address) = call.mmsg() else {
            return self.execute_nonblockable_fd_syscall(guest, call).await;
        };
        let controls = {
            // SAFETY: `mmsghdr` is a plain C record and an all-zero value is a valid
            // initialized staging value that is immediately overwritten by `read_values`.
            let mut messages: Vec<libc::mmsghdr> = (0..call.vlen())
                .map(|_| unsafe { std::mem::zeroed() })
                .collect();
            guest
                .memory()
                .read_values(messages_address.into(), &mut messages)?;

            let mut controls = Vec::with_capacity(messages.len());
            for message in &messages {
                let header = &message.msg_hdr;
                if header.msg_control.is_null() || header.msg_controllen == 0 {
                    controls.push(None);
                    continue;
                }
                controls.push(Some((
                    header.msg_control as usize,
                    header.msg_controllen.min(MAX_CONTROL_BYTES),
                )));
            }
            controls
        };

        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        let delivered = usize::try_from(result).unwrap_or(0).min(controls.len());
        if delivered == 0 {
            return Ok(result);
        }
        let now = self.observe_socket_receive(guest, call.fd()).await?;
        let mut timestamped = Vec::new();
        for (address, length) in controls.into_iter().take(delivered).flatten() {
            let address: AddrMut<'_, u8> = AddrMut::from_raw(address).ok_or(Errno::EFAULT)?;
            let mut bytes = vec![0; length];
            guest.memory().read_exact(address, &mut bytes)?;
            if !socket_timestamp_messages(&bytes).is_empty() {
                timestamped.push((address, bytes));
            }
        }
        if timestamped.is_empty() {
            return Ok(result);
        }

        for (address, mut bytes) in timestamped {
            canonicalize_socket_timestamps(&mut bytes, now);
            guest.memory().write_exact(address, &bytes)?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_timeout_socket_poll_requests_a_strong_one_turn_yield() {
        let request = zero_timeout_poll_request(DetTid::from_raw(17), true);

        assert_eq!(request.resources.len(), 1);
        assert_eq!(
            request.resources.get(&ResourceID::SchedYield),
            Some(&Permission::W)
        );
        assert_eq!(request.fyi, LOOPBACK_POLL_YIELD_FYI);
    }

    #[test]
    fn zero_timeout_non_socket_poll_keeps_the_existing_empty_turn() {
        let request = zero_timeout_poll_request(DetTid::from_raw(17), false);

        assert!(request.resources.is_empty());
        assert!(request.fyi.is_empty());
    }

    #[test]
    fn connect_peer_classification_never_overrides_kernel_errors() {
        assert!(connect_result_allows_peer_classification(&Ok(0)));
        assert!(connect_result_allows_peer_classification(&Err(
            Error::Errno(Errno::EINPROGRESS)
        )));
        assert!(!connect_result_allows_peer_classification(&Err(
            Error::Errno(Errno::EALREADY)
        )));
        assert!(!connect_result_allows_peer_classification(&Err(
            Error::Errno(Errno::EBADF)
        )));
        assert!(!connect_result_allows_peer_classification(&Err(
            Error::Errno(Errno::EFAULT)
        )));
    }

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
    fn pselect6_probe_result_maps_only_erestartsys_to_eintr() {
        assert_eq!(
            pselect6_probe_result(Err(Errno::ERESTARTSYS)),
            Err(Errno::EINTR)
        );

        assert_eq!(pselect6_probe_result(Ok(0)), Ok(0));
        assert_eq!(pselect6_probe_result(Ok(1)), Ok(1));
        assert_eq!(pselect6_probe_result(Err(Errno::EBADF)), Err(Errno::EBADF));
        assert_eq!(
            pselect6_probe_result(Err(Errno::EINVAL)),
            Err(Errno::EINVAL)
        );
        assert_eq!(pselect6_probe_result(Err(Errno::EINTR)), Err(Errno::EINTR));
    }

    #[test]
    fn ppoll_signal_mask_keeps_reverie_preemption_unblocked() {
        let preemption_bit = 1_u64 << ((reverie::PERF_EVENT_SIGNAL as usize) - 1);
        assert_eq!(sanitize_ppoll_signal_mask(u64::MAX), !preemption_bit);
    }

    #[test]
    fn ppoll_record_replay_masked_waits_keep_fail_closed_probe() {
        assert!(!ppoll_uses_kernel_wait(true, true, true));
        assert!(ppoll_uses_kernel_wait(true, true, false));
        assert!(!ppoll_uses_kernel_wait(true, false, true));
        assert!(ppoll_uses_kernel_wait(false, true, true));
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

    #[test]
    fn truncated_timestamp_payload_prefix_is_rewritten() {
        let header_len = cmsg_align(std::mem::size_of::<libc::cmsghdr>());
        let full_len = header_len + std::mem::size_of::<libc::timeval>();
        let mut control = vec![0xaa; header_len + std::mem::size_of::<i32>()];
        assert!(write_control_value(
            &mut control,
            libc::cmsghdr {
                cmsg_len: full_len,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: SCM_TIMESTAMP_OLD,
            }
        ));

        assert_eq!(
            canonicalize_socket_timestamps(&mut control, LogicalTime::from_nanos(2_345_678_901)),
            1
        );
        assert_eq!(
            read_control_value::<i32>(&control[header_len..]),
            Some(2),
            "the visible timeval prefix must not retain host seconds"
        );
    }

    #[test]
    fn timestamping_preserves_populated_source_slots() {
        let header_len = cmsg_align(std::mem::size_of::<libc::cmsghdr>());
        let timespec_len = std::mem::size_of::<libc::timespec>();
        let message_len = header_len + 3 * timespec_len;
        let mut control = vec![0; cmsg_align(message_len)];
        assert!(write_control_value(
            &mut control,
            libc::cmsghdr {
                cmsg_len: message_len,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: SCM_TIMESTAMPING_OLD,
            }
        ));
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let populated = libc::timespec {
            tv_sec: 99,
            tv_nsec: 88,
        };
        assert!(write_control_value(&mut control[header_len..], zero));
        assert!(write_control_value(
            &mut control[header_len + timespec_len..],
            populated
        ));
        assert!(write_control_value(
            &mut control[header_len + 2 * timespec_len..],
            populated
        ));

        assert_eq!(
            canonicalize_socket_timestamps(&mut control, LogicalTime::from_nanos(2_345_678_901)),
            1
        );
        let first = read_control_value::<libc::timespec>(&control[header_len..]).unwrap();
        let second =
            read_control_value::<libc::timespec>(&control[header_len + timespec_len..]).unwrap();
        let third = read_control_value::<libc::timespec>(&control[header_len + 2 * timespec_len..])
            .unwrap();
        assert_eq!((first.tv_sec, first.tv_nsec), (0, 0));
        assert_eq!((second.tv_sec, second.tv_nsec), (2, 345_678_901));
        assert_eq!((third.tv_sec, third.tv_nsec), (2, 345_678_901));
    }
}
