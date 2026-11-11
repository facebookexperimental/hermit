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
// Only the kernel-run `poll` restart reads the host clock, and only x86_64
// has `poll`.
#[cfg(target_arch = "x86_64")]
use std::time::Instant;

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
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie::syscalls::Timespec;
use reverie::syscalls::linux_abi::Cmsghdr;
use reverie::syscalls::linux_abi::Mmsghdr;
use reverie::syscalls::linux_abi::Msghdr;
#[cfg(target_arch = "x86_64")]
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
use crate::syscalls::files::SocketRole;
use crate::syscalls::helpers::GuestHeldPollProof;
#[cfg(target_arch = "x86_64")]
use crate::syscalls::helpers::KernelPollDeadline;
use crate::syscalls::helpers::KernelSignalWait;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::syscalls::helpers::PublishedWaitMask;
use crate::syscalls::helpers::REAL_TIME_SIGNALS;
#[cfg(target_arch = "x86_64")]
use crate::syscalls::helpers::RestartCall;
use crate::syscalls::helpers::act_on_pending_real_time;
use crate::syscalls::helpers::finish_wait_mask;
use crate::syscalls::helpers::guest_signal_state_task;
use crate::syscalls::helpers::is_accepted_connection;
#[cfg(target_arch = "x86_64")]
use crate::syscalls::helpers::keep_restart_block;
use crate::syscalls::helpers::millis_duration_to_absolute_timeout;
use crate::syscalls::helpers::observe_under_mask;
use crate::syscalls::helpers::pending_real_time_among;
use crate::syscalls::helpers::poll_deadline_for;
use crate::syscalls::helpers::poll_targets_guest_held;
use crate::syscalls::helpers::publish_wait_mask;
use crate::syscalls::helpers::record_retry_event;
use crate::syscalls::helpers::refuse_held_signal_loss;
use crate::syscalls::helpers::result_after_restore;
use crate::syscalls::helpers::retry_blocking_wait_under_mask;
use crate::syscalls::helpers::retry_nonblocking_syscall_with_timeout;
use crate::syscalls::helpers::unblocked_by;
use crate::syscalls::scm_rights::ReceiveGeometry;
use crate::syscalls::scm_rights::ReceivedFrom;
use crate::syscalls::scm_rights::SentRights;
use crate::syscalls::scm_rights::received_descriptors;
use crate::syscalls::signal::STACK_RED_ZONE;
use crate::syscalls::signal::kernel_installed_signal_mask;
use crate::syscalls::signal::read_kernel_sigset;
use crate::syscalls::threads::KernelSigset;
use crate::tool_global::*;
use crate::tool_local::Detcore;
use crate::types::DetTid;
use crate::types::LogicalTime;

mod temporary_mask_forms;

// Printing helper
// TODO: this should be subsumed by better syscall printing.
#[cfg(target_arch = "x86_64")]
fn print_poll(call: &syscalls::Poll) {
    let len = call.nfds();
    debug!("POLL: on {} fds, timeout {}", len, call.timeout());
    let Some(fds) = call.fds() else {
        return;
    };
    // TODO: nicer API for reading arrays from the guest:
    unsafe {
        for i in 0..len {
            debug!("POLL: fd {} = {}", i, fds.offset(i as isize));
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

/// Where a call's private cells (`PrivateCells`) live while the kernel reads
/// them (`stage_private_cells`).
enum StagedCellsStorage<Guard> {
    /// The guest stack scratch below the red zone, checked out by the guard.
    Stack(Guard),
    /// Part of the guest's red zone, whose first `len` bytes `saved` holds
    /// until `StagedCells::release` writes them back.
    RedZone {
        at: usize,
        len: usize,
        saved: [u8; MAX_PRIVATE_CELLS_LEN],
    },
}

/// What a call hands the kernel in place of the guest's own memory: a
/// temporary signal mask, which a sibling could rewrite while the call waits
/// and which Hermit sanitizes, and the cell that goes with it. The mask comes
/// first.
///
/// Every layout with a mask takes 24 bytes, the size of replay's signal
/// delivery for an interrupted call (`end_replayed_pselect6_under_its_temporary_mask`),
/// so replay can deliver wherever recording found room for the call (Claude
/// re-review of https://github.com/rrnewton/hermit/pull/4087).
#[derive(Clone, Copy)]
enum PrivateCells {
    /// A pselect6 temporary mask and the `{ sigmask, sigsetsize }` wrapper
    /// that names it.
    Pselect6Mask(KernelSigset),
    /// A temporary mask for a call that takes the mask without a wrapper
    /// (`epoll_pwait`, a ppoll the kernel waits in), and 16 unused bytes.
    Mask(KernelSigset),
    /// A temporary mask and a zero timeout (a zero-timeout ppoll probe, and
    /// the replayed signal delivery).
    MaskAndZeroTimeout(KernelSigset),
    /// A zero timeout alone (a zero-timeout ppoll probe with no mask).
    ZeroTimeout,
}

const MASK_LEN: usize = std::mem::size_of::<KernelSigset>();

/// The most any `PrivateCells` takes: a mask and a 16-byte cell.
const MAX_PRIVATE_CELLS_LEN: usize = MASK_LEN + std::mem::size_of::<Timespec>();

// The wrapper and the timeout share the cell after the mask.
const _: () = assert!(std::mem::size_of::<Pselect6SigmaskArg>() == std::mem::size_of::<Timespec>());

impl PrivateCells {
    fn len(self) -> usize {
        match self {
            Self::Pselect6Mask(_) | Self::Mask(_) | Self::MaskAndZeroTimeout(_) => {
                MAX_PRIVATE_CELLS_LEN
            }
            Self::ZeroTimeout => std::mem::size_of::<Timespec>(),
        }
    }

    /// The cells' bytes when they start at `at`. Only the first `len` count,
    /// and a zero timeout is all zeroes.
    fn bytes(self, at: usize) -> [u8; MAX_PRIVATE_CELLS_LEN] {
        let mut cells = [0u8; MAX_PRIVATE_CELLS_LEN];
        match self {
            Self::Pselect6Mask(mask) => {
                cells[..MASK_LEN].copy_from_slice(&mask.to_ne_bytes());
                cells[MASK_LEN..MASK_LEN + 8].copy_from_slice(&at.to_ne_bytes());
                cells[MASK_LEN + 8..].copy_from_slice(&KERNEL_SIGSET_SIZE.to_ne_bytes());
            }
            Self::Mask(mask) | Self::MaskAndZeroTimeout(mask) => {
                cells[..MASK_LEN].copy_from_slice(&mask.to_ne_bytes());
            }
            Self::ZeroTimeout => {}
        }
        cells
    }
}

/// A call's private cells in guest memory (`stage_private_cells`).
struct StagedCells<Guard> {
    /// The first cell: the mask, or the zero timeout of
    /// `PrivateCells::ZeroTimeout`.
    at: usize,
    storage: StagedCellsStorage<Guard>,
    /// Memory for making an expired timerfd readable while these cells hold
    /// the scratch stack (`crate::syscalls::timerfd::publish_expired_in`);
    /// `None` for red-zone cells, which hold no stack.
    timerfd_scratch: Option<crate::syscalls::timerfd::Scratch>,
}

impl<Guard> StagedCells<Guard> {
    /// The mask copy.
    fn mask(&self) -> usize {
        self.at
    }

    /// The cell after the mask: a pselect6 wrapper, or a zero timeout.
    fn after_mask(&self) -> usize {
        self.at + MASK_LEN
    }

    /// Gives the storage back once the call has returned: drops the stack
    /// guard, or writes the guest's red-zone bytes back. That is a write to
    /// guest memory, not an injected syscall, so a call that ended with a
    /// restart code goes back to the kernel untouched.
    fn release<G, T>(self, guest: &mut G)
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        if let StagedCellsStorage::RedZone { at, len, saved } = self.storage
            && let Err(errno) = AddrMut::<u8>::from_raw(at)
                .ok_or(Errno::EFAULT)
                .and_then(|bytes| guest.memory().write_exact(bytes, &saved[..len]))
        {
            // Not expected: the same bytes were written before the call.
            tracing::warn!("[detcore] could not restore the red zone after a syscall: {errno}");
        }
    }
}

/// Stages `cells` where the kernel can read them in place of the guest's own
/// memory.
///
/// The cells go in the guest stack scratch below the red zone when that is
/// writable. A thread may run with no writable memory there, on a stack it
/// placed near the start of a page, and a call on such a stack is valid
/// (Codex re-check of https://github.com/rrnewton/hermit/pull/4053). Then the
/// cells go in the red zone, whose bytes are saved here and written back by
/// `StagedCells::release` when the call returns. The red zone may hold the
/// call's own buffers, which Linux reads and writes, so the cells take the
/// highest 8-aligned window that overlaps none of `live` (Codex re-check #3).
/// The guest's own copies of the cells (its mask, wrapper or timeout) may be
/// overwritten: Linux reads none of them, as the call is redirected to the
/// private cells, and all are restored. When no window is free and writable,
/// this returns `None`, and the caller refuses the call
/// (`refuse_unstageable_pselect6_mask`). The guest runs no code in between:
/// the call holds the thread in the kernel, a signal frame is built below the
/// red zone, and a call the kernel restarts comes back as a new syscall with
/// the guest's own arguments, which is staged again. Nothing is mapped, so the
/// guest's address space is unchanged (Codex re-check of the earlier
/// per-thread page).
///
/// Whether the scratch faults is a function of the guest's own stack, so the
/// choice is deterministic.
async fn stage_private_cells<G, T>(
    guest: &mut G,
    cells: PrivateCells,
    live: &[std::ops::Range<usize>],
) -> Result<Option<StagedCells<<G::Stack as Stack>::StackGuard>>, Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let len = cells.len();
    let mut stack = guest.stack().await;
    let at = match cells {
        PrivateCells::ZeroTimeout => stack.reserve::<Timespec>().as_raw(),
        _ => stack.reserve::<[u64; 3]>().as_raw(),
    };
    // Only a process with an armed virtual timerfd can need the publication
    // scratch; any other keeps the whole stack for its cells, as before
    // timerfds were virtualized.
    let timerfd_scratch = crate::syscalls::timerfd::holds_armed_virtual_timerfd(guest)
        .then(|| crate::syscalls::timerfd::reserve_scratch(&mut stack));
    match stack.commit() {
        Ok(guard) => {
            let bytes = AddrMut::<u8>::from_raw(at).ok_or(Errno::EFAULT)?;
            // The commit wrote zeroes, which the cells' address fills in.
            guest.memory().write_exact(bytes, &cells.bytes(at)[..len])?;
            return Ok(Some(StagedCells {
                at,
                storage: StagedCellsStorage::Stack(guard),
                timerfd_scratch,
            }));
        }
        Err(Errno::EFAULT) => {}
        Err(errno) => return Err(errno),
    }
    let rsp = crate::arch::stack_pointer(&guest.regs().await) as usize;
    let Some(top) = rsp.checked_sub(len).map(|top| top & !7) else {
        return Ok(None);
    };
    let bottom = rsp.saturating_sub(STACK_RED_ZONE);
    let mut saved = [0u8; MAX_PRIVATE_CELLS_LEN];
    let mut found = None;
    for at in (bottom..=top).rev().step_by(8) {
        let window = at..at + len;
        if live
            .iter()
            .any(|range| range.start < window.end && window.start < range.end)
        {
            continue;
        }
        let Some(bytes) = AddrMut::<u8>::from_raw(at) else {
            continue;
        };
        if guest.memory().read_exact(bytes, &mut saved[..len]).is_ok() {
            found = Some((at, bytes));
            break;
        }
    }
    let Some((at, bytes)) = found else {
        return Ok(None);
    };
    let staged = StagedCells {
        at,
        storage: StagedCellsStorage::RedZone { at, len, saved },
        timerfd_scratch: None,
    };
    if guest
        .memory()
        .write_exact(bytes, &cells.bytes(at)[..len])
        .is_err()
    {
        // A read-only stack page. A write that failed partway may have changed
        // some bytes, so put them back.
        staged.release(guest);
        return Ok(None);
    }
    Ok(Some(staged))
}

/// The temporary mask a record/replay pselect6 that may wait sleeps under,
/// read at the call's entry, where Linux copies it (recording waits on a
/// private copy of it, `handle_pselect6`). The mask is read through the kernel
/// (`read_kernel_sigset`), so a mask the kernel's copy faults on, such as one
/// on a `PROT_NONE` page that a tracer read would still return, gives `None`:
/// the call then keeps the guest's wrapper and the kernel fails it with EFAULT
/// in Linux's order (Claude review of
/// https://github.com/rrnewton/hermit/pull/4166). `None` too when the call
/// has no mask, its wrapper cannot be read as the kernel copies it
/// (`read_pselect6_argument`), or the size is not the kernel's.
///
/// The read is the call's first injection: a signal that stops it restarts
/// the call (`restart_if_first_probe_not_run`), the only error returned.
async fn entry_pselect6_temporary_mask<G, T>(
    guest: &mut G,
    call: syscalls::Pselect6,
) -> Result<Option<KernelSigset>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some(wrapper) = call.sigmask() else {
        return Ok(None);
    };
    let Some(argument) = read_pselect6_argument(guest, wrapper) else {
        return Ok(None);
    };
    if argument.sigsetsize != KERNEL_SIGSET_SIZE {
        return Ok(None);
    }
    let Some(mask_addr) = Addr::<libc::sigset_t>::from_raw(argument.sigmask) else {
        return Ok(None);
    };
    let mask = match read_kernel_sigset(guest, mask_addr).await {
        Ok(mask) => mask,
        Err(error @ Error::Errno(Errno::ERESTARTSYS)) => {
            return crate::syscalls::signal::restart_if_first_probe_not_run(Err(error));
        }
        Err(_) => return Ok(None),
    };
    super::signal_shadow::note_wait_mask(guest, mask);
    Ok(Some(mask))
}

/// Whether a pselect6 names no mask: it passes no `{ sigmask, sigsetsize }`
/// argument, or one whose mask pointer is NULL, as glibc's `pselect` passes
/// it. Linux then leaves the thread's mask in place without reading the size
/// (`set_user_sigmask`). The argument is read whole, once, with the guest's
/// own access (`read_pselect6_argument`), so one the kernel would fail to copy
/// counts as naming a mask: the call keeps it, and its EFAULT.
fn pselect6_names_no_mask<G, T>(guest: &mut G, call: syscalls::Pselect6) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some(wrapper) = call.sigmask() else {
        return true;
    };
    read_pselect6_argument(guest, wrapper).is_some_and(|argument| argument.sigmask == 0)
}

/// pselect6's `{ sigmask, sigsetsize }` argument at `wrapper`, read as Linux
/// copies it (`get_sigset_argpack`): whole, in one read that honours the
/// guest's page protections, or `None` where the kernel's copy would fault,
/// or where the tracer cannot read it although the kernel can (a write-only
/// page on x86). Either way the call keeps the guest's argument. A tracer read
/// retried word by word reaches a `PROT_NONE` tail on ptrace
/// (`PTRACE_PEEKDATA`), which the kernel's copy does not.
fn read_pselect6_argument<G, T>(
    guest: &mut G,
    wrapper: Addr<'_, libc::sigset_t>,
) -> Option<Pselect6SigmaskArg>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut bytes = [0u8; std::mem::size_of::<Pselect6SigmaskArg>()];
    guest
        .memory()
        .read_exact_with_user_access(wrapper.cast::<u8>(), &mut bytes)
        .ok()?;
    let word = |at: usize| {
        usize::from_ne_bytes(
            bytes[at..at + std::mem::size_of::<usize>()]
                .try_into()
                .expect("a word"),
        )
    };
    Some(Pselect6SigmaskArg {
        sigmask: word(0),
        sigsetsize: word(std::mem::size_of::<usize>()),
    })
}

/// The temporary mask a replayed zero-timeout pselect6 sleeps under, read
/// once the scheduler grants the call, which keeps its turn until it returns;
/// `None` when the call has no mask or the wrapper or the mask cannot be read
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
    let mask = guest.memory().read_value(mask_addr).ok()?;
    super::signal_shadow::note_wait_mask(guest, mask);
    Some(mask)
}

/// Whether replay could deliver a signal that interrupts this record/replay
/// pselect6 (`end_replayed_pselect6_under_its_temporary_mask`), whose 24 bytes
/// go in the stack scratch or at the top of the red zone. Recording a
/// zero-timeout call passes the guest's own wrapper to the kernel, so it
/// accepts the call on any stack, and a call whose stack pointer is less than
/// 24 bytes above unwritable memory left replay nowhere to deliver: the kernel
/// restarted the served call and replay stopped at the next syscall
/// (https://github.com/rrnewton/hermit/issues/4112). The top 24 bytes of the
/// red zone are readable exactly when the stack pointer is at least 24 bytes
/// above unwritable memory, and the stack scratch lies below them, so this
/// reads them. `true` for a call with no mask to deliver under, and for one
/// whose wrapper Linux cannot read, which it fails before it waits. A function
/// of the guest's stack and memory, which replay reproduces.
async fn replayed_pselect6_delivery_fits<G, T>(guest: &mut G, call: syscalls::Pselect6) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some(wrapper) = call.sigmask() else {
        return true;
    };
    let argument: Result<Pselect6SigmaskArg, _> = guest.memory().read_value(wrapper.cast());
    match argument {
        Ok(argument) if argument.sigmask != 0 => {}
        _ => return true,
    }
    let rsp = crate::arch::stack_pointer(&guest.regs().await) as usize;
    let mut cells = [0u8; MAX_PRIVATE_CELLS_LEN];
    rsp.checked_sub(MAX_PRIVATE_CELLS_LEN)
        .and_then(|top| AddrMut::<u8>::from_raw(top & !7))
        .is_some_and(|top| guest.memory().read_exact(top, &mut cells).is_ok())
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
/// do both, so replay injects a ppoll with no descriptors, a zero timeout
/// and a scratch copy of the mask the call read at entry (`temporary_mask`,
/// from `replayed_pselect6_temporary_mask`), never the guest's buffer, which a
/// sibling may have rewritten while the call was served (review of
/// https://github.com/rrnewton/hermit/pull/4052): the kernel installs
/// the mask, finds the signal pending, returns `ERESTARTNOHAND` and delivers
/// the signal, as it did when the recorded call returned. A zero timeout never
/// sleeps, and Linux writes no remaining time back for it, so neither host
/// timing nor host time reaches the guest.
///
/// A zero-timeout masked pselect6 and a zero-timeout masked ppoll end the same
/// way when a signal their mask unblocks is already pending, and replay serves
/// them the same way, so both end through this too
/// (https://github.com/rrnewton/hermit/issues/4054).
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
    let Ok(state) = crate::syscalls::read_guest_thread_signal_state(guest) else {
        return;
    };
    if state.pending
        & state.blocked
        & state.interrupting(kernel_installed_signal_mask(temporary_mask))
        == 0
    {
        return;
    }
    // The cells go in the stack scratch, or at the top of the red zone when
    // the thread runs on a stack it placed near the start of a page, which is
    // valid. The guest's red-zone bytes go back once the injected call has
    // returned. The kernel builds a signal frame below the red zone, and the
    // guest runs no code before the bytes are back, so it never sees them
    // change. Nothing is mapped, so the replayed address space stays the
    // recording's. No window is excluded: the call has returned, and Linux
    // touches only the cells. The 24 bytes fit wherever recording staged its
    // own 24-byte mask copy (Claude re-review of
    // https://github.com/rrnewton/hermit/pull/4087).
    let Ok(Some(staged)) =
        stage_private_cells(guest, PrivateCells::MaskAndZeroTimeout(temporary_mask), &[]).await
    else {
        return;
    };
    inject_signal_delivery(guest, staged.mask(), staged.after_mask()).await;
    staged.release(guest);
}

/// Injects the zero-timeout ppoll with no descriptors that delivers a
/// replayed call's signal under its temporary mask (`mask`), as recording ran
/// the call itself: the kernel installs the mask, finds the signal pending,
/// returns `ERESTARTNOHAND` and delivers the signal as the injected call
/// returns, as it did after the recorded call, and the guest then gets the
/// recorded result. ppoll takes the mask pointer directly, so the delivery
/// needs no `{ sigmask, sigsetsize }` wrapper, which a pselect6 would.
async fn inject_signal_delivery<G, T>(guest: &mut G, mask: usize, zero_timeout: usize)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let _ = guest
        .inject(
            syscalls::Ppoll::new()
                .with_fds(None)
                .with_nfds(0)
                .with_timeout(AddrMut::from_raw(zero_timeout))
                .with_sigmask(Addr::from_raw(mask))
                .with_sigsetsize(KERNEL_SIGSET_SIZE),
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

/// Runs `probe`, a zero-timeout ppoll, and returns its result, `EINTR`
/// included. Only `ERESTARTSYS`, which ppoll itself never returns, is retried:
/// it is the backend's report that a signal stopped the probe before its
/// `syscall` instruction ran, and the backend holds that signal for the
/// guest's resume, so the probe runs again, as `inject_with_retry` ran it.
///
/// `inject_with_retry` also retried `EINTR`, which hung the call forever on a
/// backend that runs it itself: where the probe's temporary mask lets in a
/// pending caught signal, such a backend answers `EINTR` and installs the
/// mask for that signal's delivery as the guest resumes, and the signal stays
/// pending until then, so every retry found it again. Linux answers that case
/// with ERESTARTNOHAND (`do_poll`, fs/select.c), keeps the temporary mask for
/// the delivery (`poll_select_finish`), and the delivery to a handler makes
/// it `EINTR` (`handle_signal`, arch/x86/kernel/signal.c), so `EINTR` is the
/// guest's answer, as for pselect6's probes. A traced guest's probe gets the
/// kernel's own ERESTARTNOHAND, which was never retried. A backend that runs
/// the real call inside the guest's process gets `EINTR` once a handler has
/// run for the signal, which the retry turned into a later probe's 0.
async fn inject_ppoll_probe<G, T>(guest: &mut G, probe: syscalls::Ppoll) -> Result<i64, Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    loop {
        let result = guest.inject(probe).await;
        if !ppoll_probe_runs_again(result) {
            return result;
        }
    }
}

/// Whether `inject_ppoll_probe` runs a probe again after `result`: only after
/// the backend's report that it did not run (`ERESTARTSYS`).
fn ppoll_probe_runs_again(result: Result<i64, Errno>) -> bool {
    result == Err(Errno::ERESTARTSYS)
}

/// The pending signals that the temporary mask `mask` lets in, in the signal
/// state `sets` a backend reports, when every one of them is a signal the
/// process ignores (`crate::syscalls::ignored_at_generation`: set to SIG_IGN,
/// or default-ignored with no handler), and so one Linux discards as a ppoll
/// under `mask` returns (`get_signal`, kernel/signal.c); the empty set when
/// the mask lets none in. `None` when it lets in one that is not ignored.
fn ignored_signals_let_in(sets: &reverie::SignalSets, mask: KernelSigset) -> Option<KernelSigset> {
    let let_in = (sets.pending | sets.shared_pending) & !mask;
    let ignored = crate::syscalls::ignored_at_generation(sets.ignored, sets.caught);
    (let_in & !ignored == 0).then_some(let_in)
}

/// [`ignored_signals_let_in`] for the calling thread, from the signal state
/// its backend reports (`Guest::signal_sets`); `None` from a backend that
/// reports none.
fn ignored_signals_let_in_now<G, T>(guest: &G, mask: KernelSigset) -> Option<KernelSigset>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    guest
        .signal_sets(guest.tid())
        .ok()
        .and_then(|sets| ignored_signals_let_in(&sets, mask))
}

/// The refusal of a masked pselect6 that a backend running the call itself
/// failed with ENOSYS for a pending signal its temporary mask lets in, which
/// it cannot act on inside the call, as for ppoll
/// (`Detcore::refuse_masked_ppoll_signal`).
const MASKED_PSELECT6_SIGNAL_REASON: &str = "a pselect6 whose temporary signal mask lets in a \
     pending signal that would stop the process, or an ignored one pending beside a signal \
     that ends the call, cannot be served on this backend: it cannot act on that signal inside \
     the call";

/// The refusal of a masked pselect6 that a backend running the call itself
/// failed with ENOSYS for its descriptor sets, as KVM's does for more than 64
/// descriptors or for a selected standard output or error it captures, each
/// before it looks at a signal (`masked_pselect6_refusal_reason`). Linux
/// serves both: it clamps the count to its descriptor table
/// (`core_sys_select`, fs/select.c:641-642 at v6.17) and never fails the
/// call with ENOSYS.
const MASKED_PSELECT6_DECLINED_REASON: &str = "a pselect6 with a temporary signal mask whose \
     descriptor sets this backend does not serve (more than 64 descriptors, or a standard \
     output or error it captures) cannot be served on this backend";

/// The signals whose default action stops the process (`SIG_KERNEL_STOP_MASK`
/// and `sig_kernel_stop`, include/linux/signal.h:422-424 and :448 at v6.17):
/// SIGSTOP, SIGTSTP, SIGTTIN and SIGTTOU.
const DEFAULT_STOP_SIGNALS: KernelSigset = (1 << (libc::SIGSTOP - 1))
    | (1 << (libc::SIGTSTP - 1))
    | (1 << (libc::SIGTTIN - 1))
    | (1 << (libc::SIGTTOU - 1));

/// Whether the signal state `sets` holds a pending signal that the temporary
/// mask `mask` lets in (SIGKILL and SIGSTOP always: no mask blocks them) and
/// that a backend running a masked ppoll or pselect6 itself cannot act on
/// inside the call, so that it fails the call with ENOSYS: one the process
/// ignores (the shared rule, `crate::syscalls::ignored_at_generation`), which
/// Linux discards before it restarts the call, or one whose default action
/// stops the process with no handler (`DEFAULT_STOP_SIGNALS`). A caught
/// signal, or one whose default action ends the process, ends the call
/// instead.
fn lets_in_a_signal_the_backend_cannot_act_on(
    sets: &reverie::SignalSets,
    mask: KernelSigset,
) -> bool {
    let unblockable: KernelSigset = (1 << (libc::SIGKILL - 1)) | (1 << (libc::SIGSTOP - 1));
    let let_in = (sets.pending | sets.shared_pending) & !(mask & !unblockable);
    let ignored = crate::syscalls::ignored_at_generation(sets.ignored, sets.caught);
    let stops = DEFAULT_STOP_SIGNALS & !sets.caught & !ignored;
    let_in & (ignored | stops) != 0
}

/// Why a masked pselect6 of `nfds` descriptors that the backend failed with
/// ENOSYS, under the temporary mask `mask`, is refused, from the signal state
/// `sets` the backend reports for the thread, if it reports any. More
/// descriptors than one machine word holds (`PSELECT6_INTERNAL_MAX_NFDS`)
/// KVM's backend declines first, before it reads the sets or looks at a
/// signal, so the call is refused for its descriptor sets whatever is
/// pending. Otherwise it is refused for a pending signal the backend cannot
/// act on inside the call (`lets_in_a_signal_the_backend_cannot_act_on`), or,
/// with none pending, for the call's descriptor sets. A selected standard
/// output or error the backend captures it also declines before it looks at
/// a signal, and Detcore cannot see which descriptors it captures, so such a
/// call with such a signal pending is refused for the signal. With no state
/// reported, the signal.
fn masked_pselect6_refusal_reason(
    sets: Option<&reverie::SignalSets>,
    mask: KernelSigset,
    nfds: i32,
) -> &'static str {
    if nfds > PSELECT6_INTERNAL_MAX_NFDS {
        return MASKED_PSELECT6_DECLINED_REASON;
    }
    match sets {
        Some(sets) if !lets_in_a_signal_the_backend_cannot_act_on(sets, mask) => {
            MASKED_PSELECT6_DECLINED_REASON
        }
        _ => MASKED_PSELECT6_SIGNAL_REASON,
    }
}

/// [`masked_pselect6_refusal_reason`] for the calling thread, from the signal
/// state its backend reports now (`Guest::signal_sets`).
fn masked_pselect6_refusal_reason_now<G, T>(
    guest: &G,
    mask: KernelSigset,
    nfds: i32,
) -> &'static str
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    masked_pselect6_refusal_reason(guest.signal_sets(guest.tid()).ok().as_ref(), mask, nfds)
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

/// The start time and deadline of a wait for `timeout`, or neither without
/// one. Observing the virtual clock asks the scheduler, which publishes the
/// thread's signal state and can end its turn, so a call whose first mask probe
/// may restart it (`restart_if_first_probe_not_run`) calls this after the
/// probe. `LogicalTime + Duration` saturates, where a nanosecond count cast to
/// u64 would wrap a 2^64 ns timeout to none at all (Claude review of
/// https://github.com/rrnewton/hermit/pull/4235).
async fn ppoll_start<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &mut G,
    timeout: Option<Duration>,
) -> (Option<LogicalTime>, Option<LogicalTime>) {
    match timeout {
        Some(timeout) => {
            let started_at = thread_observe_time(guest).await;
            (Some(started_at), Some(started_at + timeout))
        }
        None => (None, None),
    }
}

/// The temporary mask `call` names, if any, read by a kernel-access probe;
/// EINVAL for a size the kernel rejects. `first` says the read is the call's
/// first injection, so a signal that stops it restarts the call
/// (`restart_if_first_probe_not_run`).
async fn read_ppoll_mask<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &mut G,
    call: syscalls::Ppoll,
    first: bool,
) -> Result<Option<KernelSigset>, Error> {
    let Some(signal_mask) = call.sigmask() else {
        return Ok(None);
    };
    if call.sigsetsize() != KERNEL_SIGSET_SIZE {
        return Err(Errno::EINVAL.into());
    }
    let read = read_kernel_sigset(guest, signal_mask).await;
    let signal_mask = if first {
        crate::syscalls::signal::restart_if_first_probe_not_run(read)?
    } else {
        read?
    };
    super::signal_shadow::note_wait_mask(guest, signal_mask);
    Ok(Some(signal_mask))
}

/// `select`'s timeout as Linux reads it. In Linux v7.1 `kern_select`
/// (`fs/select.c`) first refuses a negative `tv_sec` or `tv_usec` with
/// `EINVAL`, then passes `tv_sec + tv_usec / USEC_PER_SEC` seconds and
/// `(tv_usec % USEC_PER_SEC) * NSEC_PER_USEC` nanoseconds to
/// `poll_select_set_timeout`, which refuses only what `timespec64_valid`
/// (`include/linux/time64.h`) refuses: so microseconds of a second or more
/// fold into the seconds. (v6.12 has no separate sign check, so there a
/// mixed-sign timeval such as `{2, -1000000}` normalizes to 1 s; v7.1 added
/// the check because the signed sum could wrap a negative timeout into a
/// valid one. This follows v7.1, the measured host behaviour.) A sum that
/// would overflow the signed seconds is `EINVAL` here.
#[cfg(target_arch = "x86_64")]
pub(super) fn select_timeout_duration(timeout: libc::timeval) -> Result<Duration, Errno> {
    if timeout.tv_sec < 0 || timeout.tv_usec < 0 {
        return Err(Errno::EINVAL);
    }
    let seconds = timeout
        .tv_sec
        .checked_add(timeout.tv_usec / 1_000_000)
        .ok_or(Errno::EINVAL)?;
    let nanoseconds = (timeout.tv_usec % 1_000_000) * 1_000;
    Ok(Duration::new(seconds as u64, nanoseconds as u32))
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

/// Why a `recvmmsg` is refused before it receives
/// (`ReceiveGeometry::control_read_back_refused`, judged on its structures).
const BATCH_CONTROL_GEOMETRY_REFUSAL: &str = "the socket timestamps its entries may receive could \
     not be found to replace with logical time: a write it makes for one entry can land on a \
     header, iovec array, msg_controllen or control buffer that is read after that write";

/// Why a `recvmsg` is refused before it receives
/// (`ReceiveGeometry::control_read_back_refused`, judged on its structures).
const MESSAGE_CONTROL_GEOMETRY_REFUSAL: &str = "the socket timestamps it may receive could not \
     be found to replace with logical time: its data or control messages can land on the \
     msg_namelen that Linux reads afterwards for the name's length";

/// Why a `recvmsg` or `recvmmsg` is refused after it received
/// (`ReceiveGeometry::control_read_back_refused`, judged on the control
/// extents it wrote).
const OWN_HEADER_CONTROL_REFUSAL: &str = "the socket timestamps it received could not be found \
     to replace with logical time: a name or header field Linux writes after a message's control \
     messages landed inside them";

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
    let header_len = cmsg_align(std::mem::size_of::<Cmsghdr>());
    let mut messages = Vec::new();
    let mut offset = 0usize;

    while let Some(header_bytes) = control.get(offset..) {
        let Some(header) = read_control_value::<Cmsghdr>(header_bytes) else {
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

/// Canonicalize the socket timestamps among the control messages a receive
/// wrote: the first `written` bytes of `control`, its `msg_controllen` after
/// the call. Returns how many of `control`'s bytes are the receive's, to be
/// written back, or `None` where none holds a timestamp. Linux writes control
/// messages one after another and leaves the rest of the buffer as the caller
/// left it (`put_cmsg`, `____sys_recvmsg`), so bytes past `written` are the
/// guest's own: they are never parsed or rewritten, even where they hold what
/// reads as a timestamp message, and even where the buffer covers the
/// caller's own header.
fn canonicalize_written_timestamps(
    control: &mut [u8],
    written: usize,
    now: LogicalTime,
) -> Option<usize> {
    let written = written.min(control.len());
    let received = &mut control[..written];
    if socket_timestamp_messages(received).is_empty() {
        return None;
    }
    canonicalize_socket_timestamps(received, now);
    Some(written)
}

/// The `msg_controllen` a receive wrote back to the guest `msghdr` at
/// `header`, or 0 where it cannot be read back, so that nothing is taken as
/// the receive's.
fn written_control_length<T, G>(guest: &G, header: usize) -> usize
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    Addr::<usize>::from_raw(header + std::mem::offset_of!(Msghdr, msg_controllen))
        .and_then(|length| guest.memory().read_value(length).ok())
        .unwrap_or(0)
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

/// [`Detcore::keep_deadline_for_rerun`]'s rule: a `poll` with an end time
/// (`deadline`) keeps it for its rerun on a backend that restarts a woken call
/// only through `ERESTARTSYS` (`restarts_only_through_erestartsys`), when a
/// signal woke it (`woken`) or it ends with `ERESTARTSYS`.
// Used by the x86_64 production path and by an arch-neutral unit test.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
fn keeps_poll_deadline_for_rerun(
    restarts_only_through_erestartsys: bool,
    deadline: Option<LogicalTime>,
    woken: crate::syscalls::helpers::WokenWait,
    result: &Result<i64, Error>,
) -> bool {
    restarts_only_through_erestartsys
        && deadline.is_some()
        && (woken == crate::syscalls::helpers::WokenWait::Poll
            || matches!(result, Err(Error::Errno(errno)) if *errno == Errno::ERESTARTSYS))
}

/// The request each turn of a masked ppoll's wait files
/// (`Detcore::backend_masked_ppoll_wait`), before its guest-dependent poll
/// fields: a polled call (`InternalIOPolling`) named `ppoll`, with the EINTR
/// marker, and the call's temporary mask `signal_mask` as Linux installs it.
/// Linux holds that mask in `blocked` for the whole call (`set_user_sigmask`,
/// kernel/signal.c:3274 at v6.17, called by `do_sys_ppoll`, fs/select.c:1108),
/// while this backend's `blocked` between probes is the thread's own. Filed
/// here, as `pselect6` files its mask (`handle_internal_pselect6`), so the
/// scheduler decides by the call's mask: a SIGALRM expiry's discard at
/// generation and the choice of the thread that takes it
/// (`signal_state_reader`). Before, it filed none, and a masked ppoll whose
/// mask blocked SIGALRM was chosen for the copy by its own mask, woken, and
/// never took it (the coord7 review of
/// <https://github.com/rrnewton/hermit/pull/5221>, finding 5). The
/// scheduler's polling record leaves the mask out of the request it prints
/// (`Scheduler::logged_polling_request`), as for `pselect6`.
fn masked_ppoll_wait_request(dettid: DetTid, signal_mask: KernelSigset) -> Resources {
    let mut rsrc = Resources::new(dettid);
    rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
    rsrc.fyi("ppoll");
    rsrc.set_signal_interrupt_errno(Errno::EINTR);
    rsrc.blocked_signal_mask = Some(kernel_installed_signal_mask(sanitize_ppoll_signal_mask(
        signal_mask,
    )));
    rsrc
}

fn sanitize_ppoll_signal_mask(mask: u64) -> u64 {
    let signal_bit = (reverie::PERF_EVENT_SIGNAL as usize) - 1;
    mask & !(1_u64 << signal_bit)
}

/// The pollfd array a ppoll reads and writes, if it has one.
fn ppoll_fds_range(call: syscalls::Ppoll) -> Option<std::ops::Range<usize>> {
    call.fds().map(|fds| {
        let start = fds.as_raw();
        let len = usize::try_from(call.nfds())
            .unwrap_or(usize::MAX)
            .saturating_mul(std::mem::size_of::<libc::pollfd>());
        start..start.saturating_add(len)
    })
}

/// Whether the kernel owns a ppoll's blocking wait: when threads are not
/// sequentialized, and for every record/replay call, masked or not, as for a
/// masked pselect6 (https://github.com/rrnewton/hermit/issues/4042). A masked
/// sequentialized run-mode wait still takes the probe path.
fn ppoll_uses_kernel_wait(sequentialize_threads: bool, recordreplay_modes: bool) -> bool {
    !sequentialize_threads || recordreplay_modes
}

// Linux copies the outer wrapper before rejecting this private malformed
// timeout. Keep the operation and its closed purpose together: using this
// disposition with a valid timeout would bypass runtime mask interposition.
async fn validate_pselect6_wrapper<G, T>(
    guest: &mut G,
    argument: Addr<'_, libc::sigset_t>,
) -> Result<(), Error>
where
    T: reverie::Tool,
    G: Guest<T>,
{
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
    match guest
        .inject_with_purpose(
            validation,
            reverie::InjectionPurpose::Pselect6WrapperValidation,
        )
        .await
    {
        Err(error) => match error.into_errno()? {
            Errno::EINVAL => Ok(()),
            // This is the call's first injection. A mechanism failure must not
            // be converted to a guest errno or a successful validation.
            errno => crate::syscalls::signal::restart_if_first_probe_not_run(Err(errno.into())),
        },
        // Success would mean the backend did not validate the probe.
        Ok(_) => Err(Errno::EIO.into()),
    }
}

/// The result Detcore gives the guest for a `poll` it ran in the kernel. A
/// signal pending while the poll runs ends it with `ERESTART_RESTARTBLOCK`,
/// even with a zero timeout, which cannot wait (`do_poll` checks for a signal
/// before it honours the timeout), and with no handler to run the kernel
/// restarts it through `restart_syscall`. The poll returns the code Detcore's
/// own polls return for it instead (`NonblockableSyscall::kernel_restart_errno`).
/// A zero or infinite timeout keeps no deadline (`do_restart_poll` reruns the
/// poll as issued), so that code is `ERESTARTNOHAND`, which the kernel restarts
/// by running the call again; a positive timeout keeps Linux's own. A handler
/// turns either code into `EINTR`. Under a tracer a default-ignored `SIGCHLD`
/// is still queued, so a child that exits while its parent polls is such a
/// signal, and so is the backend's own timer signal under `--imprecise-timers`.
///
/// A poll that `keeps_record` (`RestartCall::KernelPoll`) always returns
/// `ERESTART_RESTARTBLOCK` here, even a restart whose time is up, and keeps
/// its record for `restart_syscall`; `kernel_poll_restart_code` then decides
/// the code the guest gets.
#[cfg(target_arch = "x86_64")]
fn kernel_poll_result(
    call: &syscalls::Poll,
    result: Result<i64, Error>,
    keeps_record: bool,
) -> Result<i64, Error> {
    if !poll_interrupted(&result) {
        result
    } else if keeps_record {
        Err(Errno::ERESTART_RESTARTBLOCK.into())
    } else {
        Err(call.kernel_restart_errno().into())
    }
}

/// The code a `poll` with a positive timeout returns to the guest when a
/// signal stopped it before it ran (`before_run`,
/// `poll_stopped_before_it_ran`), from `kept`, the result
/// `kernel_poll_result` gave it. The backend holds such a signal for the
/// guest's resume, and passes a held signal on unreported when the guest's
/// call returns `ERESTART_RESTARTBLOCK`. Record then took no turn for its
/// delivery, while replay, which serves the poll from the recording without
/// running it, took one where the signal reached the guest at an ordinary
/// delivery stop, and refused at the next logged readmission. Such a poll
/// returns `ERESTARTNOHAND` instead, as a wait that holds a signal does
/// (`helpers::held_signal_restart_errno`), and still keeps its record. With no
/// handler to run the kernel then runs the guest's call again: a resumed
/// poll's `restart_syscall`, which finds the record, or the guest's own poll,
/// which never ran and starts its timeout again (Linux's restart would keep
/// the first end time; under `--no-sequentialize-threads` the end moves by
/// the delivery's latency). A handler turns either code into `EINTR`. A
/// signal that ends a poll that ran is not held: it reaches the guest at an
/// ordinary delivery stop, which the backend reports. Any other result passes
/// through.
///
/// Unlike `held_signal_restart_errno`, this does not know which signal the
/// backend holds, so a held `SIGSTOP` also gets `ERESTARTNOHAND`. That is
/// harmless: after the stop the kernel runs the guest's call again, as for
/// any other held signal above. A resumed poll's `restart_syscall` still finds
/// its record; a poll that never ran starts its timeout again, which loses
/// nothing under record and replay and, under `--no-sequentialize-threads`,
/// drops the end time Detcore read for it as described above. For a stop,
/// the guest's poll runs again only once the stop ends, so its end moves by
/// the whole length of the stop, not by a delivery's latency.
#[cfg(target_arch = "x86_64")]
fn kernel_poll_restart_code(kept: Result<i64, Error>, before_run: bool) -> Result<i64, Error> {
    match kept {
        Err(Error::Errno(Errno::ERESTART_RESTARTBLOCK)) if before_run => {
            Err(Errno::ERESTARTNOHAND.into())
        }
        kept => kept,
    }
}

/// Whether a signal stopped a `poll` before it ran: the backend reports that
/// as `ERESTARTSYS` (`Guest::inject`), which Linux's poll never returns.
#[cfg(target_arch = "x86_64")]
fn poll_stopped_before_it_ran(raw: &Result<i64, Error>) -> bool {
    matches!(raw, Err(Error::Errno(Errno::ERESTARTSYS)))
}

/// Whether a signal ended a `poll` Detcore ran in the kernel: Linux's own
/// `ERESTART_RESTARTBLOCK`, or `ERESTARTSYS` when the signal stopped the call
/// before it ran (`Guest::inject`). Linux's poll never returns `ERESTARTSYS`
/// (`do_poll` reports any signal as `-ERESTARTNOHAND`, which `poll` turns
/// into `ERESTART_RESTARTBLOCK`), so both are the same interruption, and
/// passing `ERESTARTSYS` on would let `SA_RESTART` rerun a poll that Linux
/// ends with `EINTR`, and rerun a `restart_syscall` that keeps no record.
#[cfg(target_arch = "x86_64")]
fn poll_interrupted(result: &Result<i64, Error>) -> bool {
    matches!(
        result,
        Err(Error::Errno(
            Errno::ERESTART_RESTARTBLOCK | Errno::ERESTARTSYS
        ))
    )
}

/// Whether a `poll` with a nonzero timeout runs in the kernel: under record and
/// replay, whose replay cannot rely on the descriptors, and without
/// sequentialized threads. A plain or strict run (`--strict` refuses
/// `--no-sequentialize-threads`) emulates it on virtual time
/// (`handle_internal_poll`), so the host clock `kernel_poll_deadline` reads
/// never reaches such a run.
#[cfg(target_arch = "x86_64")]
fn polls_in_kernel(sequentialize_threads: bool, recordreplay_modes: bool) -> bool {
    !sequentialize_threads || recordreplay_modes
}

/// The end of a positive timeout for a `poll` Detcore runs in the kernel
/// (`RestartCall::KernelPoll`), read when the poll is first issued; `None`
/// for a zero or infinite timeout, whose restart reruns the call
/// (`kernel_poll_result`). Under record and replay the end holds no instant
/// (`KernelPollDeadline`), so those modes read no host clock here.
#[cfg(target_arch = "x86_64")]
fn kernel_poll_deadline(
    call: &syscalls::Poll,
    recordreplay_modes: bool,
) -> Option<KernelPollDeadline> {
    call.restart_keeps_deadline().then(|| {
        KernelPollDeadline(
            (!recordreplay_modes)
                .then(|| Instant::now() + Duration::from_millis(call.timeout() as u64)),
        )
    })
}

/// Whether `restart`, a `restart_syscall` at the interrupted poll's address,
/// still holds `call`'s pollfd array and length in its argument registers.
/// Linux's restart leaves the registers as the poll left them (it sets only
/// the syscall number and moves the instruction pointer back,
/// `arch_do_signal_or_restart`), so the kernel's restart block and the
/// registers describe the same poll.
#[cfg(target_arch = "x86_64")]
fn restart_names_poll(restart: &Syscall, call: &syscalls::Poll) -> bool {
    let (number, args) = restart.into_parts();
    number == Sysno::restart_syscall
        && args.arg0 == call.fds().map_or(0, |fds| fds.as_raw())
        && args.arg1 == call.nfds() as usize
}

/// The timeout, in whole milliseconds rounded up, that a restarted kernel-run
/// `poll` waits from `now` until `end`: 0 once `end` has passed.
#[cfg(target_arch = "x86_64")]
fn remaining_poll_millis(now: Instant, end: Instant) -> i32 {
    let left = end.saturating_duration_since(now).as_nanos();
    i32::try_from(left.div_ceil(1_000_000)).unwrap_or(i32::MAX)
}

impl<T: RecordOrReplay> Detcore<T> {
    /// poll syscall (MAYHANG)
    ///
    /// `restart` is the record the thread's previous syscall kept for its
    /// restart, if any: a woken `poll` that runs again after `ERESTARTSYS`
    /// keeps its deadline there (`keep_deadline_for_rerun`). Only such a
    /// rerun record is taken here.
    // TODO-HUMAN-REVIEW(PR-1023): Review zero-timeout poll scheduling across backends.
    #[cfg(target_arch = "x86_64")]
    pub(crate) async fn handle_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
        restart: Option<crate::syscalls::helpers::RestartBlock>,
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
            let result = if self.cfg.recordreplay_modes {
                self.record_or_replay(guest, call).await
            } else {
                // A virtual timerfd among the fds is readable exactly when its
                // deadline has passed (`crate::syscalls::timerfd`).
                crate::syscalls::timerfd::publish_expired(guest).await;
                guest.inject(call).await
            };
            kernel_poll_result(&call, result.map_err(Error::from), false)
        } else if polls_in_kernel(self.cfg.sequentialize_threads, self.cfg.recordreplay_modes) {
            // In replay mode, we cannot assume the existence of FILES during replay.
            // Thus we must record the poll and replay it from the trace.
            let deadline = kernel_poll_deadline(&call, self.cfg.recordreplay_modes);
            Ok(self.handle_external_poll(guest, call, deadline).await?)
        } else {
            // A woken `poll` that runs again after `ERESTARTSYS` waits until the
            // deadline it kept, not for its whole timeout again. Only that
            // rerun's record is taken (`RestartBlock::rerun_deadline`): a
            // record kept for the kernel's `restart_syscall` is not, so a
            // program's own retry after EINTR waits its whole timeout.
            if let Some(block) = restart {
                let rip = crate::arch::instruction_pointer(&guest.regs().await);
                if let Some(deadline) = block.rerun_deadline(rip, call) {
                    return self.handle_internal_poll_until(guest, call, deadline).await;
                }
            }
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
    #[cfg(target_arch = "x86_64")]
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
            self.record_or_replay_shielded_wait(guest, call).await
        }
    }

    /// Refuses a large-nfds pselect6 with a temporary mask, or a record/replay
    /// one that may wait (`record_or_replay_pselect6_under_a_private_mask`),
    /// whose private copy has nowhere to go: the stack has no writable scratch below its red zone,
    /// and every red-zone window overlaps one of the call's own buffers
    /// (`stage_private_cells`). Forwarding the guest's wrapper instead
    /// would let a sibling change the mask the kernel installs after the
    /// scheduler has recorded the one it waits for, so the call could time out
    /// with a wrong answer (Codex re-check #4 of
    /// https://github.com/rrnewton/hermit/pull/4053). A fail-closed run stops
    /// with the policy refusal and this message. A run started with
    /// `--allow-unsupported-syscalls` runs on: the call keeps the guest's
    /// wrapper, whose mask a sibling may still change, so the scheduler records
    /// its sleeping mask as unknown (`Resources::blocked_signal_mask_unknown`),
    /// not the thread's own, and arms no entry barrier for it (Codex re-check
    /// #5).
    async fn refuse_unstageable_pselect6_mask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
    ) -> Result<i64, Error> {
        use std::io::Write as _;
        let message = "unsupported: pselect6 with a temporary signal mask, on a stack with \
            no room below its red zone whose red zone the call's own buffers fill: Hermit \
            has nowhere to put the private copy of the mask the kernel must install \
            (https://github.com/rrnewton/hermit/pull/4053)";
        tracing::error!("[tid {}] {message}", guest.tid());
        if self.cfg.panic_on_unsupported_syscalls {
            let _ = writeln!(crate::util::RetryingStderr, "{message}");
            return self
                .refuse_unserviceable_operation(guest, Sysno::pselect6, Errno::EFAULT)
                .await;
        }
        self.record_or_replay_blocking_with_unknown_mask(guest, Syscall::Pselect6(call))
            .await
    }

    /// Refuses a record/replay pselect6 with a temporary mask on a stack less
    /// than 24 bytes above unwritable memory, where replay could not deliver a
    /// signal that interrupts it (`replayed_pselect6_delivery_fits`). Recording
    /// and replay refuse it alike. A fail-closed run stops with the policy
    /// refusal and this message, whose result this returns. A run started
    /// with `--allow-unsupported-syscalls` records the call as before, and a
    /// replay of an interrupted one stops at the next syscall; this returns
    /// `None` for it.
    async fn refuse_undeliverable_pselect6_mask<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Option<Result<i64, Error>> {
        use std::io::Write as _;
        let message = "unsupported: pselect6 with a temporary signal mask on a stack less than \
            24 bytes above unwritable memory: replay has nowhere to deliver a signal that \
            interrupts it (https://github.com/rrnewton/hermit/issues/4112)";
        tracing::error!("[tid {}] {message}", guest.tid());
        if !self.cfg.panic_on_unsupported_syscalls {
            return None;
        }
        let _ = writeln!(crate::util::RetryingStderr, "{message}");
        Some(
            self.refuse_unserviceable_operation(guest, Sysno::pselect6, Errno::EFAULT)
                .await,
        )
    }

    /// Records or replays a masked pselect6 that may wait, under a private
    /// copy of the mask read at its entry (`mask`), staged clear of the fd
    /// sets and timeout the kernel reads and writes (`stage_private_cells`).
    /// The outer `Err` is a staging failure; a call with no room for the copy
    /// is refused (`refuse_unstageable_pselect6_mask`).
    async fn record_or_replay_pselect6_under_a_private_mask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        mask: KernelSigset,
    ) -> Result<Result<i64, Error>, Error> {
        let set_len = pselect6_fd_set_len(call.nfds()).unwrap_or(0);
        let live: Vec<std::ops::Range<usize>> = [
            call.readfds().map(|set| (set.as_raw(), set_len)),
            call.writefds().map(|set| (set.as_raw(), set_len)),
            call.exceptfds().map(|set| (set.as_raw(), set_len)),
            call.timeout()
                .map(|timeout| (timeout.as_raw(), std::mem::size_of::<Timespec>())),
        ]
        .into_iter()
        .flatten()
        .map(|(start, len)| start..start.saturating_add(len))
        .collect();
        let Some(staged) =
            stage_private_cells(guest, PrivateCells::Pselect6Mask(mask), &live).await?
        else {
            return Ok(self.refuse_unstageable_pselect6_mask(guest, call).await);
        };
        let result = self
            .record_or_replay_blocking_with_unknown_mask(
                guest,
                Syscall::Pselect6(call.with_sigmask(Addr::from_raw(staged.after_mask()))),
            )
            .await;
        staged.release(guest);
        Ok(result)
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
            // A call that may wait sleeps under a private copy of its mask,
            // read here, at its entry, where Linux copies it. Handing the kernel
            // the guest's wrapper let a sibling turn that ran after the grant
            // change the mask the kernel copied, at a point host timing chose,
            // and replay could not tell (https://github.com/rrnewton/hermit/issues/4095).
            // Replay reads the mask at the same point and delivers a recorded
            // interruption under it. A zero-timeout call holds its turn until
            // it returns, so no sibling runs before the kernel copies the mask,
            // and replay reads it once the call is granted (review of
            // https://github.com/rrnewton/hermit/pull/4084). A zero timeout still
            // installs the mask and ends the call with ERESTARTNOHAND for a
            // pending signal it unblocks
            // (https://github.com/rrnewton/hermit/issues/4054).
            //
            // The entry read is the call's first injection, which a signal can
            // stop and restart (`restart_if_first_probe_not_run`), so it comes
            // before the refusal check: that logs, and the restarted call would
            // log again.
            // `None` inside when the call names no mask, or one the kernel
            // rejects or the tracer cannot read, though the kernel can: such a
            // call keeps the guest's own wrapper.
            let entry_mask = if call.sigmask().is_some() && !zero_timeout {
                Some(entry_pselect6_temporary_mask(guest, call).await?)
            } else {
                None
            };
            if !replayed_pselect6_delivery_fits(guest, call).await
                && let Some(refused) = self.refuse_undeliverable_pselect6_mask(guest).await
            {
                return refused;
            }
            let read_mask = self.cfg.replaying && call.sigmask().is_some();
            let mut temporary_mask = None;
            let result = if let Some(entry_mask) = entry_mask {
                if read_mask {
                    temporary_mask = entry_mask;
                }
                match entry_mask {
                    Some(mask) => {
                        self.record_or_replay_pselect6_under_a_private_mask(guest, call, mask)
                            .await?
                    }
                    // A wrapper that names no mask leaves the thread's own
                    // mask in place, which the scheduler reads and the shield
                    // covers. The kernel is handed no wrapper, which Linux
                    // takes alike, so a sibling cannot write a mask into the
                    // guest's before the kernel reads it.
                    None if pselect6_names_no_mask(guest, call) => {
                        self.record_or_replay_shielded_wait(
                            guest,
                            Syscall::Pselect6(call.with_sigmask(None)),
                        )
                        .await
                    }
                    None => {
                        self.record_or_replay_blocking_with_unknown_mask(
                            guest,
                            Syscall::Pselect6(call),
                        )
                        .await
                    }
                }
            } else if zero_timeout {
                resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
                if read_mask {
                    temporary_mask = replayed_pselect6_temporary_mask(guest, call);
                }
                self.record_or_replay(guest, Syscall::Pselect6(call))
                    .await
                    .map_err(Into::into)
            } else {
                self.record_or_replay_shielded_wait(guest, Syscall::Pselect6(call))
                    .await
            };
            if let Some(mask) = temporary_mask
                && matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND)))
            {
                end_replayed_pselect6_under_its_temporary_mask(guest, mask).await;
            }
            return result;
        }
        if !self.cfg.sequentialize_threads {
            // A call with a mask waits under it; one that names none is
            // shielded as select is, and the kernel is handed no wrapper (as
            // under record), since a sibling runs concurrently here.
            if !pselect6_names_no_mask(guest, call) {
                return Ok(guest.inject(call).await?);
            }
            let may_wait = !call.timeout().is_some_and(|timeout| {
                guest
                    .memory()
                    .read_value(timeout)
                    .is_ok_and(|timeout: Timespec| timeout.tv_sec == 0 && timeout.tv_nsec == 0)
            });
            return self
                .inject_shielded_wait(guest, call.with_sigmask(None), may_wait)
                .await;
        }
        // Under the in-guest delivery gate (A5 step 3c.1d) the forms follow
        // design D39 and D50, and no probe carries the guest's temporary mask
        // (IR-9): `gated_pselect6`.
        if self.cfg.in_guest_signal_delivery {
            return Box::pin(self.gated_pselect6(guest, call)).await;
        }

        if call.nfds() < 0 {
            return Ok(guest.inject(call).await?);
        }

        // Linux copies pselect6's outer { sigmask, sigsetsize } wrapper, then
        // validates the timeout, then reads the inner mask (fs/select.c,
        // https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/select.c?h=v6.12#n800
        // and #n752). Linux checks the wrapper here; its words are read only
        // after the timeout, because a tracer read can fail where the kernel's
        // copy succeeds: a write-only page is readable on x86, but not to
        // process_vm_readv. Read first, a malformed timeout there returned
        // EFAULT, not Linux's EINVAL (dh-obsc8).
        let sigmask_wrapper = match call.sigmask() {
            Some(argument) => {
                // A split read can fall back to PTRACE_PEEKDATA for the final
                // word, bypassing PROT_NONE or reporting EIO for an unmapped
                // page. Have Linux validate both wrapper words first. It copies
                // this wrapper before rejecting a malformed timeout, without
                // reading the inner mask, changing it, waiting, or writing output.
                validate_pselect6_wrapper::<_, Self>(guest, argument).await?;
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
        // Word by word, as `read_kernel_sigset` reads a mask: the probe above
        // showed Linux can copy both words, and on ptrace a word-sized read
        // is PTRACE_PEEKDATA, which reaches a write-only page that
        // process_vm_readv refuses. Backends that read through
        // process_vm_readv at every size (DBT, SaBRe, LiteInst and
        // in-guest-trap) still fail there with EFAULT, as does the timeout's
        // own read on every tracer-read backend
        // (https://github.com/rrnewton/hermit/issues/4530).
        let sigmask_argument = match sigmask_wrapper {
            Some(argument) => {
                let sigmask: usize = guest.memory().read_value(argument.cast::<usize>())?;
                let second = argument
                    .as_raw()
                    .checked_add(std::mem::size_of::<usize>())
                    .and_then(Addr::<usize>::from_raw)
                    .ok_or(Errno::EFAULT)?;
                let sigsetsize: usize = guest.memory().read_value(second)?;
                Some(Pselect6SigmaskArg {
                    sigmask,
                    sigsetsize,
                })
            }
            None => None,
        };
        if let Some(argument) = sigmask_argument
            && argument.sigmask != 0
        {
            // After the wrapper's validation: not the call's first injection.
            self.observe_under_temporary_mask(
                guest,
                Addr::<libc::sigset_t>::from_raw(argument.sigmask),
                argument.sigsetsize,
                false,
            )
            .await?;
        }
        if timeout == Some(Duration::ZERO) {
            // A virtual timerfd among the sets is readable exactly when its
            // deadline has passed (`crate::syscalls::timerfd`).
            crate::syscalls::timerfd::publish_expired(guest).await;
            let result = guest.inject(call).await;
            // A backend that runs the call itself answers ENOSYS for a pending
            // signal the mask lets in that it cannot act on inside the call
            // (`settle_masked_pselect6_enosys`).
            if result == Err(Errno::ENOSYS)
                && let Some(argument) = sigmask_argument
                && argument.sigmask != 0
                && argument.sigsetsize == KERNEL_SIGSET_SIZE
                && let Some(mask_addr) = Addr::<libc::sigset_t>::from_raw(argument.sigmask)
            {
                let mask = read_kernel_sigset(guest, mask_addr).await?;
                return self
                    .settle_masked_pselect6_enosys(guest, call, sanitize_ppoll_signal_mask(mask))
                    .await;
            }
            return Ok(result?);
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
            let snapshot = match sigmask_argument {
                Some(argument)
                    if argument.sigmask != 0 && argument.sigsetsize == KERNEL_SIGSET_SIZE =>
                {
                    match Addr::<libc::sigset_t>::from_raw(argument.sigmask) {
                        Some(mask_addr) => {
                            let mask = read_kernel_sigset(guest, mask_addr).await.ok();
                            if let Some(mask) = mask {
                                super::signal_shadow::note_wait_mask(guest, mask);
                            }
                            mask
                        }
                        None => None,
                    }
                }
                _ => None,
            };
            let Some(mask) = snapshot else {
                if sigmask_argument.is_none_or(|argument| argument.sigmask == 0) {
                    // No mask, read in this turn: hand the kernel no wrapper,
                    // as record does.
                    return self
                        .record_or_replay_shielded_wait(
                            guest,
                            Syscall::Pselect6(call.with_sigmask(None)),
                        )
                        .await;
                }
                return self
                    .record_or_replay_blocking_with_mask(guest, Syscall::Pselect6(call), None)
                    .await;
            };
            // The call sleeps under a private copy of the mask read here, not the
            // guest's wrapper: a sibling may rewrite the guest's buffer before the
            // released call reaches the kernel, and the scheduler waits for the
            // kernel to install exactly this mask (`background_entry_barriers`,
            // Codex review of https://github.com/rrnewton/hermit/pull/4053). The
            // wrapper and the mask were validated above, in Linux's order. The
            // copy stays readable until the call has rejoined.
            // The buffers Linux reads and writes, which the copy must not
            // overlap. The fd sets span at most their nominal length; Linux
            // may copy less.
            let set_len = pselect6_fd_set_len(call.nfds())?;
            let live: Vec<std::ops::Range<usize>> = [
                call.readfds().map(|set| (set.as_raw(), set_len)),
                call.writefds().map(|set| (set.as_raw(), set_len)),
                call.exceptfds().map(|set| (set.as_raw(), set_len)),
                call.timeout()
                    .map(|timeout| (timeout.as_raw(), std::mem::size_of::<Timespec>())),
            ]
            .into_iter()
            .flatten()
            .map(|(start, len)| start..start.saturating_add(len))
            .collect();
            let Some(staged) =
                stage_private_cells(guest, PrivateCells::Pselect6Mask(mask), &live).await?
            else {
                return self.refuse_unstageable_pselect6_mask(guest, call).await;
            };
            let result = self
                .record_or_replay_blocking_with_mask(
                    guest,
                    Syscall::Pselect6(call.with_sigmask(Addr::from_raw(staged.after_mask()))),
                    Some(kernel_installed_signal_mask(mask)),
                )
                .await;
            staged.release(guest);
            // Linux's pselect6 never fails with ENOSYS. A backend that runs
            // the call itself answers it for more descriptors than it serves,
            // which it declines before it looks at any signal: refused by name,
            // as the zero-timeout call is (`settle_masked_pselect6_enosys`),
            // never handed to the guest.
            if let Err(Error::Errno(Errno::ENOSYS)) = result {
                let reason = masked_pselect6_refusal_reason_now(guest, mask, call.nfds());
                return self
                    .refuse_masked_wait_because(guest, Sysno::pselect6, reason)
                    .await;
            }
            return result;
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
                super::signal_shadow::note_wait_mask(guest, mask);
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
        // Under the in-guest delivery gate (A5 step 3c.1d, D50 reader (3)):
        // no probe carries the temporary mask (IR-9); each turn that finds
        // nothing ready decides from the thread's `/proc` status under the
        // temporary mask (`temporary_mask_check`), or under the guest's own
        // mask without one (`gated_wait_ends`); a masked call's first probe
        // and its check run in the call's own turn, before any request, so
        // that check sees exactly the instances pending at entry (IR-7,
        // round-1 ruling 2), and a result that first probe finds is returned
        // after one yielding empty request
        // (`temporary_mask_forms::yield_after_in_turn_result`, round-2 ruling
        // 5); the request publishes the temporary mask; and a set Detcore
        // cannot read is EFAULT only where Linux's copy faults
        // (`temporary_mask_forms::read_gated_fd_sets`).
        let gated = self.cfg.in_guest_signal_delivery;
        // A masked call under the gate observes the clock only after its first
        // probe and check (round-3 ruling 1,
        // `temporary_mask_forms::observe_deadline`): `deferred` holds the
        // timeout until then. Gate off, as before.
        let in_turn_first = gated && sigmask.is_some();
        let deferred = if in_turn_first { timeout } else { None };
        let deadline = match timeout {
            Some(timeout) if !in_turn_first => Some(thread_observe_time(guest).await + timeout),
            _ => None,
        };
        // Gate off, the three sets are read into locals as before the gate,
        // with no allocation (round-2 ruling 1).
        let (original_readfds, original_writefds, original_exceptfds) = if gated {
            // Boxed: the gate-off future stays its size.
            match Box::pin(temporary_mask_forms::read_gated_fd_sets(
                guest, "pselect6", call, len,
            ))
            .await
            {
                Ok(sets) => sets,
                Err(error) => {
                    let deadline = match deferred {
                        Some(_) => temporary_mask_forms::observe_deadline(guest, deferred)
                            .await
                            .map(|(_, deadline)| deadline),
                        None => deadline,
                    };
                    self.write_pselect6_remaining(guest, call, deadline).await?;
                    return Err(error);
                }
            }
        } else {
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
            (original_readfds, original_writefds, original_exceptfds)
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
        // On a backend whose kernel reports the guest's signal state, a
        // `KernelSignalWait` below decides interruption against the temporary
        // mask instead (`with_wait_mask`), and the probes carry no mask.
        let kernel_signal_wait = self.cfg.backend.supports_blocked_wait_signal_interruption;
        // The probes' mask cell and the mask it holds, kept to write back after
        // a removal in that cell (`remove_signals_discarded_under_a_temporary_mask_in`).
        let probe_mask_cell = sigmask
            .filter(|_| !kernel_signal_wait && !gated)
            .map(|mask| (stack.push(mask).as_raw(), mask));
        let probe_sigmask = probe_mask_cell.map(|(sigset, _)| {
            stack
                .push(Pselect6SigmaskArg {
                    sigmask: sigset,
                    sigsetsize: KERNEL_SIGSET_SIZE,
                })
                .cast()
        });
        // On a backend whose kernel reports the guest's signal state, only a
        // signal that would end the wait natively does; the mask cell lets the
        // wait block signals under the one scratch-stack guard
        // (https://github.com/rrnewton/hermit/issues/3146). A temporary mask is
        // the mask the call sleeps under, as Linux installs it for the whole
        // call: a signal it blocks never ends the call, and one it unblocks ends
        // it although the guest's own mask blocks it (`with_wait_mask`,
        // https://github.com/rrnewton/hermit/issues/3991). A restart resumes from
        // the remaining time written back below, so a default stop ends the wait
        // as on Linux. Unlike the other waits, it holds no signal until the call
        // returns, so a pending caught `SIGCHLD` ends it as on Linux, whoever
        // sent it, at a turn that host timing can choose: a known gap that this
        // wait had before (`KernelSignalWait::for_select`).
        let mut signals = kernel_signal_wait.then(|| {
            KernelSignalWait::for_select(guest)
                .with_wait_mask(sigmask.map(crate::syscalls::signal::kernel_installed_signal_mask))
        });
        // Set when a signal the temporary mask unblocks, pending in the kernel,
        // ends the call: the kernel must then deliver it under that mask (see
        // below).
        let mut deliver_under_temporary_mask = false;
        let mask_cell = signals.as_ref().map(|_| stack.reserve::<KernelSigset>());
        // This frame holds the scratch stack across every probe, so the
        // memory that makes an expired timerfd readable is reserved here
        // (`crate::syscalls::timerfd::publish_expired_in`).
        let timerfd_scratch = crate::syscalls::timerfd::reserve_scratch(&mut stack);
        let _guard = stack.commit()?;
        // The scheduler reads the effective mask while the wait parks
        // (`publish_wait_mask`).
        let mut published = PublishedWaitMask::default();
        if let Some(signals) = signals.as_ref() {
            publish_wait_mask(guest, signals.effective_mask(), &mut published).await;
        }
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
        if gated {
            resources.blocked_signal_mask = sigmask;
        } else if self.cfg.backend.provides_process_signal_control && self.cfg.sequentialize_threads
        {
            // Where the backend controls the guest's signals (KVM), its
            // `blocked` set between probes is the thread's own mask, while
            // Linux's is the call's for the whole call (`set_user_sigmask`).
            // The scheduler reads the call's from this request where it
            // decides by the thread's mask: a SIGALRM expiry's discard at
            // generation and the choice of the thread that takes it
            // (`signal_state_reader`, https://github.com/rrnewton/hermit/issues/4929).
            // The scheduler's records leave it out of the request they print,
            // so the canonical INFO stream reads the same here as on ptrace
            // (`Scheduler::logged_polling_request`).
            resources.blocked_signal_mask =
                sigmask.map(crate::syscalls::signal::kernel_installed_signal_mask);
        }
        // Keep the request metadata accurate, but do not make it eligible for
        // the scheduler's ERESTARTSYS wakeup. A cross-task signal must first be
        // checked against pselect6's snapshotted temporary mask and disposition.
        resources.set_signal_interrupt_errno(Errno::EINTR);

        // A masked call's entry and end under the gate (A5 step 3c.1e A,
        // `temporary_mask_forms::TemporaryMaskCall`: IR-23 and D37's R2).
        // Boxed, and `None` with the gate off, so the gate-off future stays
        // its size.
        let mut masked: Option<Box<temporary_mask_forms::TemporaryMaskCall>> = None;
        // Every exit, including a `?` on a guest-memory write, must reach the mask
        // restore below, so the loop runs in its own async block.
        let result: Result<i64, Error> = {
            // Moved reborrows: a future holding a shared reference to a scratch
            // address would not be `Send`.
            let guest = &mut *guest;
            let signals = &mut signals;
            let resources = &mut resources;
            let published = &mut published;
            let deliver_under_temporary_mask = &mut deliver_under_temporary_mask;
            let masked = &mut masked;
            async move {
                // Under the gate a masked call's first probe and its check run
                // in the call's own turn (see `entry` above); an unmasked one,
                // which has no entry rule, requests before each probe as
                // `select` does (round-2 ruling 4).
                let mut first_turn = in_turn_first;
                let mut deadline = deadline;
                let mut deferred = deferred;
                // The at-entry set of the first check, from the entry before
                // the first timerfd publication (`TemporaryMaskCall::enter`),
                // and the deadline, observed after the first probe and check
                // (`observe_deadline`; round-3 ruling 1), or at the entry with
                // an armed virtual timerfd (IR-23(c)).
                if in_turn_first && let Some(mask) = sigmask {
                    let call = Box::pin(temporary_mask_forms::TemporaryMaskCall::enter(
                        guest, mask, deferred,
                    ))
                    .await?;
                    if call.observes_deadline_at_entry() {
                        deadline = call.entry_deadline().map(|(_, deadline)| deadline);
                        deferred = None;
                    }
                    *masked = Some(Box::new(call));
                }
                let mut entry = masked.as_deref().map(|call| call.entry());
                macro_rules! settle_deadline {
                    () => {
                        if deferred.is_some() {
                            deadline = Box::pin(temporary_mask_forms::observe_deadline(
                                guest,
                                deferred.take(),
                            ))
                            .await
                            .map(|(_, deadline)| deadline);
                        }
                    };
                }
                loop {
                    let in_turn = first_turn;
                    if !in_turn && let Some(call) = masked.as_deref_mut() {
                        call.note_request();
                    }
                    let signaled = !in_turn
                        && matches!(
                            resource_request(guest, resources.clone()).await,
                            ResumeStatus::Signaled(_)
                        );
                    first_turn = false;
                    // Virtual timerfds first (`crate::syscalls::timerfd`).
                    let next_timerfd =
                        crate::syscalls::timerfd::publish_expired_in(guest, Some(timerfd_scratch))
                            .await;
                    resources.poll_deadline =
                        crate::syscalls::timerfd::poll_deadline_with(guest, deadline, next_timerfd);
                    // A pending real-time timer expiration that this wait would
                    // be ended by, under the guest's mask or the call's temporary
                    // one (https://github.com/rrnewton/hermit/issues/4089).
                    let pending = pending_real_time_among(guest, REAL_TIME_SIGNALS).await;
                    if pending != 0 {
                        let (observable, discarded) = match (signals.as_ref(), sigmask) {
                            (Some(signals), _) => (signals.observable_now(guest)?, 0),
                            (None, Some(mask)) => unblocked_by(
                                &crate::syscalls::read_guest_wait_signal_state(guest)?,
                                mask,
                            ),
                            (None, None) => (0, 0),
                        };
                        act_on_pending_real_time(guest, pending, observable, discarded).await;
                    }
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
                    // the call before the probe, unless the probes carry the call's
                    // temporary mask (`probe_mask_cell`, KVM): the probe then takes
                    // the signal that woke the wait under that mask, which the backend
                    // delivers there as it answers EINTR, as Linux delivers it before
                    // it restores the thread's own mask (`restore_saved_sigmask`).
                    // Ended before the probe, the call answered EINTR with the signal
                    // still pending under an own mask that blocks it, and no handler
                    // ran (`chld-pselect-lets-in` in tests/c/process_signal_target.c,
                    // the Codex review of https://github.com/rrnewton/hermit/pull/5259).
                    if signals.is_none() && signaled && !gated && probe_mask_cell.is_none() {
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
                    let mut result = pselect6_probe_result(raw_result);
                    // A backend that runs the call itself answers ENOSYS for a
                    // pending signal the mask lets in that it cannot act on, as
                    // it does for ppoll (`settle_masked_ppoll_enosys`): ignored
                    // ones alone are removed, in the probe's own cells, as
                    // Linux discards them, and the probe runs again with its
                    // cells as it read them. Anything else is refused by name.
                    if result == Err(Errno::ENOSYS)
                        && let Some((mask_cell, mask)) = probe_mask_cell
                    {
                        if let Some(set) = ignored_signals_let_in_now(guest, mask) {
                            crate::syscalls::helpers::remove_signals_discarded_under_a_temporary_mask_in(
                                guest,
                                set,
                                mask_cell,
                                probe_timeout,
                            )
                            .await?;
                            guest.memory().write_value(
                                AddrMut::<KernelSigset>::from_raw(mask_cell).ok_or(Errno::EFAULT)?,
                                &mask,
                            )?;
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
                            result = pselect6_probe_result(guest.inject(probe).await);
                        }
                        if result == Err(Errno::ENOSYS) {
                            self.write_pselect6_remaining(guest, call, deadline).await?;
                            let reason =
                                masked_pselect6_refusal_reason_now(guest, mask, call.nfds());
                            break self
                                .refuse_masked_wait_because(guest, Sysno::pselect6, reason)
                                .await;
                        }
                    }
                    if result != Ok(0) {
                        let copy_result = if result.is_ok() {
                            self.copy_pselect6_results(guest, probe, call, len)
                        } else {
                            Ok(())
                        };
                        settle_deadline!();
                        self.write_pselect6_remaining(guest, call, deadline).await?;
                        // D37's R2 as the result is decided, before the yield
                        // (A5 step 3c.1e A).
                        let decided: Result<i64, Error> = match &result {
                            Ok(count) => Ok(*count),
                            Err(errno) => Err((*errno).into()),
                        };
                        if let Some(call) = masked.as_deref_mut()
                            && let Err(error) = Box::pin(call.finish(guest, &decided)).await
                        {
                            break Err(error);
                        }
                        if in_turn {
                            // Boxed: the gate-off future stays its size.
                            Box::pin(temporary_mask_forms::yield_after_in_turn_result(guest)).await;
                        }
                        copy_result?;
                        break result.map_err(Into::into);
                    }
                    // The turn's check, after its probe found no descriptor ready and no
                    // error: only then does a pending signal end the call. A scheduler
                    // `Signaled` answer is only a hint here; the kernel's state decides.
                    // pselect6 returns ERESTARTNOHAND: EINTR after a handler, a restart
                    // with the remaining timeout after a stop.
                    if gated {
                        let ends = match sigmask {
                            Some(mask) => {
                                // Boxed: the gate-off future stays its size.
                                let check =
                                    Box::pin(crate::syscalls::helpers::temporary_mask_check(
                                        guest,
                                        mask,
                                        entry.take(),
                                        false,
                                    ))
                                    .await;
                                if let (Ok(check), Some(call)) = (&check, masked.as_deref_mut()) {
                                    call.note_check(*check);
                                }
                                match check {
                                    Ok(crate::syscalls::helpers::TemporaryMaskCheck::Waits) => None,
                                    Ok(crate::syscalls::helpers::TemporaryMaskCheck::Fatal) => {
                                        Some(Errno::ERESTARTNOHAND)
                                    }
                                    Ok(_) => Some(Errno::EINTR),
                                    Err(error) => break Err(error),
                                }
                            }
                            None => match crate::syscalls::helpers::gated_wait_ends(guest, None) {
                                Ok(true) => Some(Errno::ERESTARTNOHAND),
                                Ok(false) => None,
                                Err(error) => break Err(error),
                            },
                        };
                        settle_deadline!();
                        if let Some(errno) = ends {
                            self.write_pselect6_remaining(guest, call, deadline).await?;
                            break Err(errno.into());
                        }
                        resources.poll_deadline = crate::syscalls::timerfd::poll_deadline_with(
                            guest,
                            deadline,
                            next_timerfd,
                        );
                    }
                    if let Some(signals) = signals.as_ref() {
                        match signals.interrupted_with_state(guest) {
                            Ok((false, _)) => {}
                            Ok((true, state)) => {
                                self.write_pselect6_remaining(guest, call, deadline).await?;
                                // A signal the backend holds, or one the guest's own
                                // mask leaves unblocked, is delivered as the call
                                // returns. One that only the temporary mask unblocks
                                // stays pending under the guest's own mask, and must be
                                // delivered under the temporary mask.
                                *deliver_under_temporary_mask = !signals.holds_signal()
                                    && signals.interrupts_only_under_wait_mask(&state);
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
                    if let Some(signals) = signals.as_ref() {
                        publish_wait_mask(guest, signals.effective_mask(), published).await;
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
        // D37's R2 at any other end of a masked call (A5 step 3c.1e A); a
        // no-op once it ran before a yield, and with the gate off.
        let result = match masked.as_deref_mut() {
            Some(call) => match Box::pin(call.finish(guest, &result)).await {
                Ok(()) => result,
                Err(error) => Err(error),
            },
            None => result,
        };
        let result = match &mut signals {
            Some(signals) => {
                let restored = signals.restore(guest, mask_cell).await;
                finish_wait_mask(
                    guest,
                    matches!(
                        result,
                        Err(Error::Errno(
                            reverie::syscalls::Errno::ERESTARTNOHAND
                                | reverie::syscalls::Errno::EINTR
                        ))
                    ),
                    &mut published,
                )
                .await;
                result_after_restore(restored, result)?
            }
            None => result,
        };
        // Restoring the guest's mask cannot deliver that signal, which the mask
        // blocks, but check again that the backend holds nothing.
        if deliver_under_temporary_mask
            && !signals.as_ref().is_some_and(KernelSignalWait::holds_signal)
            && let Some(mask) = sigmask
            && matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND)))
        {
            // Linux delivers such a signal before it puts the guest's own mask
            // back, which may block it, and the handler's frame restores the
            // guest's mask when it returns. Only the kernel can do both, so the
            // thread makes one more call, a ppoll with no descriptors, a zero
            // timeout and the call's temporary mask, as replay delivers a
            // recorded call's signal (`inject_signal_delivery`): the kernel
            // installs the mask, finds the signal pending, returns
            // ERESTARTNOHAND and delivers it as the call returns, EINTR after a
            // handler (https://github.com/rrnewton/hermit/issues/3991).
            // - The mask is a private copy of the one this call read at entry
            //   (`sigmask`), as Linux copies it once at entry; the guest's
            //   buffer, which a sibling may have rewritten since, is not read
            //   again (review of https://github.com/rrnewton/hermit/pull/4051).
            // - A zero timeout never sleeps, and Linux writes no remaining time
            //   back for it; the guest's timeout already holds the remaining
            //   time written above.
            // - A plain injection, not a tail injection, so the rest of this
            //   handler and Detcore's post-syscall work still run.
            drop(_guard);
            // No room for the cells cannot happen in practice: this call's
            // probe cells, at least as large, were committed in the same stack
            // scratch above. Should it happen, the signal stays pending under
            // the guest's own mask and the call returns EINTR without its
            // handler run, as before this delivery existed.
            match stage_private_cells(guest, PrivateCells::MaskAndZeroTimeout(mask), &[]).await? {
                Some(staged) => {
                    inject_signal_delivery(guest, staged.mask(), staged.after_mask()).await;
                    staged.release(guest);
                }
                None => tracing::warn!(
                    "[tid {}] no room to deliver a pselect6 signal under its temporary mask; \
                     its handler runs only once the guest unblocks it",
                    guest.tid()
                ),
            }
        }
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
    #[cfg(target_arch = "x86_64")]
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
            // A zero timeout is not shielded, as it is not under record: a
            // pending signal ends such a call with ERESTARTNOHAND and Linux
            // runs it again in place, which the guest does not see, and every
            // probe would pay for two mask changes. A timeout the tracer
            // cannot read is shielded.
            let may_wait = !call.timeout().is_some_and(|timeout| {
                guest
                    .memory()
                    .read_value(timeout)
                    .is_ok_and(|timeout: libc::timeval| timeout.tv_sec == 0 && timeout.tv_usec == 0)
            });
            return self.inject_shielded_wait(guest, call, may_wait).await;
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
            // A zero timeout is a pure non-blocking poll; the kernel can service it directly,
            // once a virtual timerfd among the sets is readable exactly when its deadline
            // has passed (`crate::syscalls::timerfd`).
            crate::syscalls::timerfd::publish_expired(guest).await;
            return Ok(guest.inject(call).await?);
        }

        // Mirror pselect6: keep large fd tables under kernel ownership rather than
        // over-reading the guest bitmap (Linux clamps raw fd-set copies to max_fds).
        if call.nfds() > PSELECT6_INTERNAL_MAX_NFDS {
            return self
                .record_or_replay_shielded_wait(guest, Syscall::Select(call))
                .await;
        }

        let timeout = raw_timeout.map(select_timeout_duration).transpose()?;
        self.handle_internal_select(guest, call, timeout).await
    }

    #[cfg(target_arch = "x86_64")]
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
            .backend
            .supports_blocked_wait_signal_interruption
            .then(|| KernelSignalWait::for_select(guest));
        let mask_cell = signals.as_ref().map(|_| stack.reserve::<KernelSigset>());
        // This frame holds the scratch stack across every probe, so the
        // memory that makes an expired timerfd readable is reserved here
        // (`crate::syscalls::timerfd::publish_expired_in`).
        let timerfd_scratch = crate::syscalls::timerfd::reserve_scratch(&mut stack);
        let _guard = stack.commit()?;
        // The scheduler reads the effective mask while the wait parks
        // (`publish_wait_mask`).
        let mut published = PublishedWaitMask::default();
        if let Some(signals) = signals.as_ref() {
            publish_wait_mask(guest, signals.effective_mask(), &mut published).await;
        }
        let probe = call
            .with_readfds(readfds)
            .with_writefds(writefds)
            .with_exceptfds(exceptfds)
            .with_timeout(Some(probe_timeout));

        // Under the in-guest delivery gate (A5 step 3c.1d, D50 reader (4)): a
        // turn that finds nothing ready decides from the thread's `/proc`
        // status under the mask it published, its own (`gated_wait_ends`).
        let gated = self.cfg.in_guest_signal_delivery;
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
            let published = &mut published;
            async move {
                loop {
                    let signaled = matches!(
                        resource_request(guest, resources.clone()).await,
                        ResumeStatus::Signaled(_)
                    );
                    // Virtual timerfds first (`crate::syscalls::timerfd`).
                    let next_timerfd =
                        crate::syscalls::timerfd::publish_expired_in(guest, Some(timerfd_scratch))
                            .await;
                    resources.poll_deadline =
                        crate::syscalls::timerfd::poll_deadline_with(guest, deadline, next_timerfd);
                    // A pending real-time timer expiration that this wait would
                    // be ended by (https://github.com/rrnewton/hermit/issues/4089).
                    let pending = pending_real_time_among(guest, REAL_TIME_SIGNALS).await;
                    if pending != 0
                        && let Some(signals) = signals.as_ref()
                    {
                        let observable = signals.observable_now(guest)?;
                        act_on_pending_real_time(guest, pending, observable, 0).await;
                    }
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
                    if signals.is_none() && signaled && !gated {
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
                    if gated {
                        match crate::syscalls::helpers::gated_wait_ends(guest, None) {
                            Ok(true) => {
                                self.write_select_remaining(guest, call, deadline).await?;
                                break Err(Errno::ERESTARTNOHAND.into());
                            }
                            Ok(false) => {}
                            Err(error) => break Err(error),
                        }
                    }
                    if let Some(signals) = signals.as_ref() {
                        match signals.interrupted_with_state(guest) {
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
                    if let Some(signals) = signals.as_ref() {
                        publish_wait_mask(guest, signals.effective_mask(), published).await;
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
            Some(signals) => {
                let restored = signals.restore(guest, mask_cell).await;
                finish_wait_mask(
                    guest,
                    matches!(
                        result,
                        Err(Error::Errno(
                            reverie::syscalls::Errno::ERESTARTNOHAND
                                | reverie::syscalls::Errno::EINTR
                        ))
                    ),
                    &mut published,
                )
                .await;
                result_after_restore(restored, result)?
            }
            None => result,
        };
        refuse_held_signal_loss(guest, Sysno::select, result).await
    }

    #[cfg(target_arch = "x86_64")]
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

    #[cfg(target_arch = "x86_64")]
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
        // A masked ppoll under the in-guest delivery gate (A5 step 3c.1d, D50
        // reader (5)): `gated_masked_ppoll`, which reads the timeout itself.
        if self.cfg.in_guest_signal_delivery
            && call.sigmask().is_some()
            && !ppoll_uses_kernel_wait(self.cfg.sequentialize_threads, self.cfg.recordreplay_modes)
        {
            return Box::pin(self.gated_masked_ppoll(guest, call)).await;
        }
        let timeout_address = call.timeout();
        let timeout = match timeout_address {
            Some(timeout) => Some(ppoll_timeout_duration(guest.memory().read_value(timeout)?)?),
            None => None,
        };

        let result: Result<i64, Error> = if timeout == Some(Duration::ZERO) {
            // In run mode the mask's read below is the call's first injection
            // unless a virtual timerfd's expiration is published first, which
            // can inject only for a process holding an armed one. Under record
            // and replay a scheduler request comes first, which changes
            // scheduler state (it publishes the thread's signal state and can
            // grant another thread a turn), so a restart would repeat it: the
            // read keeps the not-run report there
            // (https://github.com/rrnewton/hermit/issues/4526).
            let first = !self.cfg.recordreplay_modes
                && !crate::syscalls::timerfd::holds_armed_virtual_timerfd(guest);
            if self.cfg.recordreplay_modes {
                resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            } else {
                // A virtual timerfd among the fds is readable exactly when its
                // deadline has passed (`crate::syscalls::timerfd`); before the
                // probe's copies take the scratch stack.
                crate::syscalls::timerfd::publish_expired(guest).await;
            }
            let signal_mask = read_ppoll_mask(guest, call, first).await?;
            let (probe, staged) = match self.stage_ppoll_probe(guest, call, signal_mask).await? {
                Some((probe, staged)) => (probe, Some(staged)),
                None => {
                    if let Some(refused) = self.refuse_unstageable_ppoll_copy(guest).await {
                        return refused;
                    }
                    // Runs on with the guest's own zero timeout and mask.
                    (call, None)
                }
            };
            // The mask the probe installs, read at entry, for replay to deliver
            // under (https://github.com/rrnewton/hermit/issues/4054).
            let temporary_mask = if self.cfg.replaying {
                match call.sigmask() {
                    Some(signal_mask) => read_kernel_sigset(guest, signal_mask)
                        .await
                        .ok()
                        .map(sanitize_ppoll_signal_mask),
                    None => None,
                }
            } else {
                None
            };
            if let Some(mask) = temporary_mask {
                super::signal_shadow::note_wait_mask(guest, mask);
            }
            // No `?` on the call's own result: an interruption must reach the
            // replay delivery below.
            let result: Result<i64, Error> = if self.cfg.recordreplay_modes {
                self.record_or_replay(guest, probe)
                    .await
                    .map_err(Into::into)
            } else {
                inject_ppoll_probe(guest, probe).await.map_err(Into::into)
            };
            if let Some(staged) = staged {
                staged.release(guest);
            }
            // A backend that runs the call itself answers ENOSYS for a pending
            // signal the mask lets in that it cannot act on inside the call
            // (`settle_masked_ppoll_enosys`); after the copies are given back,
            // whose stack the settling may use.
            let result = match (result, signal_mask) {
                (Err(Error::Errno(Errno::ENOSYS)), Some(signal_mask))
                    if !self.cfg.recordreplay_modes =>
                {
                    self.settle_masked_ppoll_enosys(guest, call, signal_mask, false)
                        .await
                }
                (result, _) => result,
            };
            // Replay serves the probe from the recording, so the kernel never
            // installs its mask; deliver a recorded interruption under it, as
            // for pselect6.
            if let Some(mask) = temporary_mask
                && matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND)))
            {
                end_replayed_pselect6_under_its_temporary_mask(guest, mask).await;
            }
            // Linux does not write back an initially zero timeout. Besides matching the
            // kernel, omitting this write matters when the timeout aliases the pollfd array:
            // the injected probe may have just stored revents in those same bytes.
            result
        } else if ppoll_uses_kernel_wait(
            self.cfg.sequentialize_threads,
            self.cfg.recordreplay_modes,
        ) {
            // The kernel owns the blocking wait (`ppoll_uses_kernel_wait`). Use
            // a private copy only for the signal mask so raw ppoll can still
            // update the guest timeout.
            let mut staged = None;
            // The mask the real call sleeps under, recorded for the scheduler's
            // SIGCHLD target choice (`Resources::blocked_signal_mask`).
            let mut blocked_signal_mask = None;
            // The mask read at entry, for replay to deliver under.
            let mut temporary_mask = None;
            let call = if let Some(signal_mask) = call.sigmask() {
                if call.sigsetsize() != KERNEL_SIGSET_SIZE {
                    return Err(Errno::EINVAL.into());
                }
                let signal_mask = crate::syscalls::signal::restart_if_first_probe_not_run(
                    read_kernel_sigset(guest, signal_mask).await,
                )?;
                super::signal_shadow::note_wait_mask(guest, signal_mask);
                let signal_mask = sanitize_ppoll_signal_mask(signal_mask);
                blocked_signal_mask = Some(kernel_installed_signal_mask(signal_mask));
                temporary_mask = Some(signal_mask);
                // Linux writes revents into the pollfd array and the time left
                // into the timeout, so the copy overlaps neither.
                let live: Vec<std::ops::Range<usize>> = ppoll_fds_range(call)
                    .into_iter()
                    .chain(call.timeout().map(|timeout| {
                        let start = timeout.as_raw();
                        start..start.saturating_add(std::mem::size_of::<Timespec>())
                    }))
                    .collect();
                match stage_private_cells(guest, PrivateCells::Mask(signal_mask), &live).await? {
                    Some(cells) => {
                        let mask = cells.mask();
                        staged = Some(cells);
                        call.with_sigmask(Addr::from_raw(mask))
                    }
                    None => {
                        if let Some(refused) = self.refuse_unstageable_ppoll_copy(guest).await {
                            return refused;
                        }
                        // Runs on with the guest's own mask. A sibling may
                        // change it while the call waits, which a masked
                        // record/replay wait already allows for: its sleeping
                        // mask is recorded as unknown.
                        call
                    }
                }
            } else {
                call
            };
            // A masked record/replay wait sleeps under a temporary mask the
            // scheduler records as unknown, as a masked pselect6's does
            // (`Resources::blocked_signal_mask_unknown`), although Hermit knows
            // it exactly. A known mask would let the scheduler wait for the
            // kernel to install it, and replay serves the call from the
            // recording without the kernel ever doing so; recording and replay
            // must schedule the call alike. The result has no `?`: an
            // interruption must reach the replay delivery below.
            let result: Result<i64, Error> = if temporary_mask.is_none() {
                // No mask: the call waits under the thread's own.
                self.record_or_replay_shielded_wait(guest, Syscall::Ppoll(call))
                    .await
            } else if self.cfg.recordreplay_modes {
                self.record_or_replay_blocking_with_unknown_mask(guest, Syscall::Ppoll(call))
                    .await
            } else {
                self.record_or_replay_blocking_with_mask(
                    guest,
                    Syscall::Ppoll(call),
                    blocked_signal_mask,
                )
                .await
            };
            if let Some(staged) = staged {
                staged.release(guest);
            }
            // Linux's ppoll never fails with ENOSYS. A backend that runs the
            // call itself answers it for a masked wait it cannot keep under
            // the mask, and for a pending signal the mask lets in that it
            // cannot act on inside the call: refused by name
            // (`refuse_masked_ppoll_enosys`), never handed to the guest.
            if let (Err(Error::Errno(Errno::ENOSYS)), Some(mask)) = (&result, temporary_mask) {
                return self.refuse_masked_ppoll_enosys(guest, mask).await;
            }
            // Replay serves the call from the recording, so the kernel never
            // installs its mask; deliver a recorded interruption under it
            // (https://github.com/rrnewton/hermit/issues/3992).
            if self.cfg.replaying
                && let Some(mask) = temporary_mask
                && matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND)))
            {
                end_replayed_pselect6_under_its_temporary_mask(guest, mask).await;
            }
            result
        } else {
            self.handle_internal_ppoll(guest, call, timeout).await
        };

        result
    }

    /// The zero-timeout probe for `call`: it reads a private zero timeout,
    /// and a private, sanitized copy of the call's mask, staged by
    /// `stage_private_cells` clear of the pollfd array that Linux writes. So
    /// neither a sibling's write nor the guest's own mask can make the probe
    /// sleep or block Hermit's timer signal. `None` when the copies have
    /// nowhere to go (`refuse_unstageable_ppoll_copy`). `signal_mask` is the
    /// mask `read_ppoll_mask` read, if the call names one.
    async fn stage_ppoll_probe<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
        signal_mask: Option<KernelSigset>,
    ) -> Result<
        Option<(
            syscalls::Ppoll,
            StagedCells<<G::Stack as Stack>::StackGuard>,
        )>,
        Error,
    > {
        let signal_mask = match signal_mask {
            Some(signal_mask) => {
                // The probe's temporary mask lets the kernel take a pending
                // real-time timer expiration that every thread blocked when it
                // expired (https://github.com/rrnewton/hermit/issues/4089).
                observe_under_mask(guest, signal_mask).await?;
                Some(sanitize_ppoll_signal_mask(signal_mask))
            }
            None => None,
        };
        let cells = match signal_mask {
            Some(signal_mask) => PrivateCells::MaskAndZeroTimeout(signal_mask),
            None => PrivateCells::ZeroTimeout,
        };
        let live: Vec<std::ops::Range<usize>> = ppoll_fds_range(call).into_iter().collect();
        let Some(staged) = stage_private_cells(guest, cells, &live).await? else {
            return Ok(None);
        };
        let probe = match signal_mask {
            Some(_) => call
                .with_timeout(AddrMut::from_raw(staged.after_mask()))
                .with_sigmask(Addr::from_raw(staged.mask())),
            None => call.with_timeout(AddrMut::from_raw(staged.at)),
        };
        Ok(Some((probe, staged)))
    }

    /// Refuses a ppoll whose private copies have nowhere to go
    /// (`stage_private_cells`): a zero-timeout probe's zero timeout and mask,
    /// or the mask of a call the kernel waits in. The stack has no writable
    /// scratch below its red zone, and every red-zone window overlaps the
    /// pollfd array or, for a kernel wait, the timeout
    /// (https://github.com/rrnewton/hermit/issues/4113). A fail-closed run
    /// stops with the policy refusal and this message, whose result this
    /// returns. A run started with `--allow-unsupported-syscalls` runs on with
    /// the guest's own timeout and mask, and this returns `None`.
    async fn refuse_unstageable_ppoll_copy<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Option<Result<i64, Error>> {
        use std::io::Write as _;
        let message = "unsupported: ppoll on a stack with no room below its red zone whose \
            red zone the call's pollfd array and timeout fill: Hermit has nowhere to put its \
            private copy of the timeout and the signal mask \
            (https://github.com/rrnewton/hermit/issues/4113)";
        tracing::error!("[tid {}] {message}", guest.tid());
        if !self.cfg.panic_on_unsupported_syscalls {
            return None;
        }
        let _ = writeln!(crate::util::RetryingStderr, "{message}");
        Some(
            self.refuse_unserviceable_operation(guest, Sysno::ppoll, Errno::EFAULT)
                .await,
        )
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

        // Every zero-timeout probe carries a private copy of the call's temporary
        // mask, so the kernel applies it atomically, as pselect6's probes do
        // (`handle_internal_pselect6`). A pending signal that the mask lets in
        // ends a probe with ERESTARTNOHAND at that probe's scheduler point, and
        // the kernel delivers it under the temporary mask and only then puts the
        // thread's own mask back, as Linux does for a ppoll that a signal
        // interrupts. That probe is the last injection before the guest
        // resumes, so nothing disturbs the pending restore
        // (https://github.com/rrnewton/reverie/issues/1003). A ppoll that would
        // block used to return ENOSYS here
        // (https://github.com/rrnewton/hermit/issues/4042).
        if call.sigmask().is_some() {
            // Only memory was read before: the mask's read is the call's
            // first injection. The virtual clock is observed right after it
            // (`ppoll_start`), before the observation under the mask, which
            // can ask the scheduler too: Linux takes the end time before it
            // installs the mask.
            let signal_mask = read_ppoll_mask(guest, call, true).await?;
            let (started_at, deadline) = ppoll_start(guest, timeout).await;
            let Some((probe, staged)) = self.stage_ppoll_probe(guest, call, signal_mask).await?
            else {
                // The guest's own mask could block Hermit's timer signal while
                // the call is parked, so a run that allows unsupported
                // operations gets the same answer as a mask it cannot honor.
                return match self.refuse_unstageable_ppoll_copy(guest).await {
                    Some(refused) => refused,
                    None => Err(Errno::ENOSYS.into()),
                };
            };
            // The copies are given back before the time left is written: their
            // red-zone window may cover the guest's timeout, which Linux never
            // sees.
            let result = if self.cfg.backend.supports_blocked_wait_signal_interruption {
                let result = self
                    .wait_on_masked_ppoll_probe(
                        guest,
                        probe,
                        staged.mask(),
                        staged.timerfd_scratch,
                        deadline,
                    )
                    .await;
                staged.release(guest);
                result
            } else if self.backend_serves_masked_ppoll_waits()
                && let Some(signal_mask) = signal_mask
            {
                // The backend runs the call and reports the guest's signal
                // state (SIGSHARE S2b): the wait polls with the backend's own
                // probe, which stages its copies each turn.
                staged.release(guest);
                self.backend_masked_ppoll_wait(guest, call, signal_mask, deadline)
                    .await
            } else {
                // Without the kernel's signal state, Hermit cannot tell whether
                // the backend holds a signal that should end the wait, so a
                // ppoll that would block fails closed, refused by name
                // (`refuse_masked_ppoll_wait`). An expired timerfd on virtual
                // time is made readable first, in the staged cells' scratch
                // (`crate::syscalls::timerfd`). A backend that runs the call
                // itself answers EINTR, with the mask installed for the
                // delivery, when a pending caught signal ends it
                // (`inject_ppoll_probe`), and ENOSYS for one it cannot act on
                // (`settle_masked_ppoll_enosys`, after the copies are given
                // back, whose stack the settling may use).
                crate::syscalls::timerfd::publish_expired_in(guest, staged.timerfd_scratch).await;
                let result = inject_ppoll_probe(guest, probe).await;
                staged.release(guest);
                match (result, signal_mask) {
                    (Err(Errno::ENOSYS), Some(signal_mask)) => {
                        self.settle_masked_ppoll_enosys(guest, call, signal_mask, true)
                            .await
                    }
                    (result, _) if probe.syscall_would_have_blocked(result) => {
                        self.refuse_masked_ppoll_wait(guest).await
                    }
                    (result, _) => result.map_err(Into::into),
                }
            };
            if let (Some(timeout_address), Some(timeout), Some(started_at)) =
                (timeout_address, timeout, started_at)
            {
                self.write_ppoll_remaining(guest, timeout_address, timeout, started_at)
                    .await?;
            }
            return result;
        }

        let (started_at, deadline) = ppoll_start(guest, timeout).await;
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("ppoll");
        rsrc.set_signal_interrupt_errno(Errno::EINTR);
        rsrc.signal_state_task = guest_signal_state_task(guest);
        rsrc.relies_on_guest_held_marks = poll_targets_guest_held(
            guest,
            call.fds().map(|fds| fds.as_raw()),
            call.nfds() as usize,
        );
        rsrc.polls_guest_state_only = rsrc.relies_on_guest_held_marks;
        let result = retry_nonblocking_syscall_with_timeout(guest, call, rsrc, deadline).await;
        if let (Some(timeout_address), Some(timeout), Some(started_at)) =
            (timeout_address, timeout, started_at)
        {
            self.write_ppoll_remaining(guest, timeout_address, timeout, started_at)
                .await?;
        }
        result
    }

    /// The answer to a ppoll with a temporary signal mask that would wait, on
    /// a backend that cannot interrupt a blocked wait for a signal
    /// (`BackendCapabilities::supports_blocked_wait_signal_interruption` is
    /// false): Hermit cannot keep the mask in place across the wait, so the
    /// call is refused by name through the unsupported-operation policy, on
    /// stderr when the run ends (`report_refusal_reason`); with unsupported
    /// operations allowed the guest gets ENOSYS, as before. Linux's ppoll
    /// never fails with ENOSYS.
    async fn refuse_masked_ppoll_wait<G: Guest<Self>>(&self, guest: &mut G) -> Result<i64, Error> {
        const REASON: &str = "a ppoll with a temporary signal mask that would wait cannot be \
             served on this backend: it cannot keep the mask in place across the wait";
        self.refuse_masked_wait_because(guest, Sysno::ppoll, REASON)
            .await
    }

    /// The answer to a ppoll whose temporary signal mask lets in a pending
    /// signal that the backend, which runs the call itself, cannot act on
    /// inside the call: one that would stop the process, or an ignored one
    /// pending beside one that ends the call (`settle_masked_ppoll_enosys`).
    /// Refused by name as `refuse_masked_ppoll_wait` refuses a wait.
    async fn refuse_masked_ppoll_signal<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<i64, Error> {
        const REASON: &str = "a ppoll whose temporary signal mask lets in a pending signal that \
             would stop the process, or an ignored one pending beside a signal that ends the \
             call, cannot be served on this backend: it cannot act on that signal inside the call";
        self.refuse_masked_wait_because(guest, Sysno::ppoll, REASON)
            .await
    }

    /// A masked wait, `sysno`, refused by name for `reason`, through the
    /// unsupported-operation policy.
    async fn refuse_masked_wait_because<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        reason: &'static str,
    ) -> Result<i64, Error> {
        crate::syscalls::helpers::report_refusal_reason(
            self.cfg.panic_on_unsupported_syscalls,
            &format!(
                "[dtid {}] {sysno} refused: {reason}",
                guest.thread_state().dettid
            ),
        );
        if !self.cfg.panic_on_unsupported_syscalls {
            crate::tool_global::report_unsupported_syscall(guest, sysno).await;
        }
        self.refuse_unserviceable_operation_because(guest, sysno, Errno::ENOSYS, reason)
            .await
    }

    /// The answer to a masked ppoll that the kernel beneath waits in
    /// (`ppoll_uses_kernel_wait`) and the backend failed with ENOSYS, which
    /// Linux's ppoll never returns: a backend that runs the call itself
    /// declines a wait it cannot keep under the mask, and a pending signal
    /// the mask lets in that it cannot act on inside the call. Ignored ones
    /// alone, which Linux discards as the call returns before it restarts
    /// the call to wait again, are removed as it removes them
    /// (`discard_ignored_signals_let_in`), and the call is refused as a wait;
    /// anything else is refused as such a signal.
    async fn refuse_masked_ppoll_enosys<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mask: KernelSigset,
    ) -> Result<i64, Error> {
        if self.discard_ignored_signals_let_in(guest, mask).await? {
            self.refuse_masked_ppoll_wait(guest).await
        } else {
            self.refuse_masked_ppoll_signal(guest).await
        }
    }

    /// The answer to a zero-timeout probe of a masked ppoll, `call` with
    /// the temporary mask `signal_mask` as `read_ppoll_mask` read it, that the
    /// backend failed with ENOSYS, which Linux's ppoll never returns. A
    /// backend that runs the call itself answers it when a pending signal the
    /// mask lets in is one it cannot act on inside the call, where Linux
    /// returns ERESTARTNOHAND (`do_poll`, fs/select.c): it cannot discard an
    /// ignored signal, restart the call, or stop the process.
    ///
    /// When every such signal is one the process ignores (the shared rule,
    /// `crate::syscalls::ignored_at_generation`), Linux discards each as the
    /// call returns (`get_signal`, kernel/signal.c) and, having delivered
    /// nothing, restarts the call, which polls again
    /// (`arch_do_signal_or_restart`, arch/x86/kernel/signal.c). So does this:
    /// they are removed from the thread's queues
    /// (`discard_ignored_signals_let_in`), and a new probe runs, whose result
    /// is the call's. Where the call would wait (`waits`), a probe that finds
    /// nothing ready is refused as a wait (`refuse_masked_ppoll_wait`), as
    /// one without those signals is. Any other such signal, a stop or an
    /// ignored one beside a signal that ends the call, is refused by name
    /// (`refuse_masked_ppoll_signal`): which ignored signals Linux discards
    /// then depends on the handler's own mask, read nowhere here.
    async fn settle_masked_ppoll_enosys<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
        signal_mask: KernelSigset,
        waits: bool,
    ) -> Result<i64, Error> {
        if !self
            .discard_ignored_signals_let_in(guest, sanitize_ppoll_signal_mask(signal_mask))
            .await?
        {
            return self.refuse_masked_ppoll_signal(guest).await;
        }
        let again = match self
            .stage_ppoll_probe(guest, call, Some(signal_mask))
            .await?
        {
            Some((probe, staged)) => {
                let again = inject_ppoll_probe(guest, probe).await;
                staged.release(guest);
                again
            }
            None => {
                return match self.refuse_unstageable_ppoll_copy(guest).await {
                    Some(refused) => refused,
                    None => Err(Errno::ENOSYS.into()),
                };
            }
        };
        match again {
            Err(Errno::ENOSYS) => self.refuse_masked_ppoll_signal(guest).await,
            Ok(0) if waits => self.refuse_masked_ppoll_wait(guest).await,
            result => result.map_err(Into::into),
        }
    }

    /// The answer to a zero-timeout masked pselect6, `call` with the
    /// temporary mask `mask`, that the backend failed with ENOSYS, as for
    /// ppoll (`settle_masked_ppoll_enosys`): when every pending signal the mask
    /// lets in is one the process ignores, they are removed, as Linux discards
    /// them before it restarts the call, and the call runs again, whose result
    /// is the answer. Anything else is refused by name: for a pending signal
    /// the backend cannot act on inside the call, or, with none, for the
    /// descriptor sets it declined (`masked_pselect6_refusal_reason`).
    async fn settle_masked_pselect6_enosys<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        mask: KernelSigset,
    ) -> Result<i64, Error> {
        if self.discard_ignored_signals_let_in(guest, mask).await? {
            match guest.inject(call).await {
                Err(Errno::ENOSYS) => {}
                result => return Ok(result?),
            }
        }
        let reason = masked_pselect6_refusal_reason_now(guest, mask, call.nfds());
        self.refuse_masked_wait_because(guest, Sysno::pselect6, reason)
            .await
    }

    /// Removes the pending signals the temporary mask `mask` lets in, when
    /// the backend reports this thread's signal state (`Guest::signal_sets`)
    /// and every one of them is a signal the process ignores
    /// (`ignored_signals_let_in`): Linux discards those as the call returns.
    /// True when it removed them, or there were none; false, removing
    /// nothing, when one is not ignored or the backend reports no state.
    async fn discard_ignored_signals_let_in<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mask: KernelSigset,
    ) -> Result<bool, Error> {
        let Some(ignored) = ignored_signals_let_in_now(guest, mask) else {
            return Ok(false);
        };
        crate::syscalls::helpers::remove_signals_discarded_under_a_temporary_mask(guest, ignored)
            .await?;
        Ok(true)
    }

    /// The wait of a ppoll with a temporary mask under sequentialized threads:
    /// `probe`, a zero-timeout copy of the call that reads the mask staged at
    /// `mask` (`prepare_ppoll_probe`), runs once per scheduler turn until it
    /// reports a ready descriptor or an error, until a signal that the mask
    /// lets in ends the wait, or until `deadline` passes, which returns 0.
    ///
    /// The probe carries the mask, so the kernel ends a probe with
    /// ERESTARTNOHAND for a pending signal the mask lets in, and delivers it
    /// under the mask as the call returns. A signal that the guest's own mask
    /// lets in can instead stop a probe before the kernel installs the mask;
    /// the backend then holds it, and `KernelSignalWait` decides against the
    /// call's mask whether it ends the wait (`with_wait_mask`). Without that,
    /// the wait ran on to its deadline, or forever, with the signal held. The
    /// backend delivers a held signal under the guest's own mask
    /// (https://github.com/rrnewton/reverie/issues/1011). A signal that only
    /// the mask lets in can be pending at a turn's check without having ended
    /// that turn's probe only if it arrived from outside the guest between
    /// the two; the next probe, which carries the mask, ends the call for it
    /// and the kernel delivers it under the mask. The wait never blocks
    /// signals (`KernelSignalWait::block`): a probe that a signal ended would
    /// then leave the all-blocked mask to be restored.
    ///
    /// Every probe runs before the turn's check, as Linux's do_poll polls every
    /// descriptor before it tests for a pending signal. The wait holds no
    /// signal until the call returns (`KernelSignalWait::for_select`): the
    /// kernel ends a probe for any pending signal the mask lets in, `SIGCHLD`
    /// included, so a `SIGCHLD` the kernel posts for a child ends it at a turn
    /// that host timing chooses, the known gap that select and pselect6 waits
    /// share (https://github.com/rrnewton/hermit/issues/3146).
    async fn wait_on_masked_ppoll_probe<G: Guest<Self>>(
        &self,
        guest: &mut G,
        probe: syscalls::Ppoll,
        mask: usize,
        timerfd_scratch: Option<crate::syscalls::timerfd::Scratch>,
        deadline: Option<LogicalTime>,
    ) -> Result<i64, Error> {
        let temporary_mask: KernelSigset = guest
            .memory()
            .read_value(Addr::<KernelSigset>::from_raw(mask).ok_or(Errno::EFAULT)?)?;
        let mut signals = KernelSignalWait::for_select(guest)
            .with_wait_mask(Some(crate::syscalls::signal::kernel_installed_signal_mask(
                temporary_mask,
            )))
            .with_deadline(deadline.is_some());
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("ppoll");
        rsrc.poll_deadline = poll_deadline_for(guest, deadline);
        rsrc.relies_on_guest_held_marks = poll_targets_guest_held(
            guest,
            probe.fds().map(|fds| fds.as_raw()),
            probe.nfds() as usize,
        );
        rsrc.polls_guest_state_only = rsrc.relies_on_guest_held_marks;
        // As for pselect6: a scheduler `Signaled` answer wakes the wait, and
        // the probe and the kernel's state decide whether a signal ends it.
        rsrc.set_signal_interrupt_errno(Errno::EINTR);
        rsrc.signal_state_task = guest_signal_state_task(guest);
        // Between probes the kernel holds the guest's own mask, while Linux's
        // `blocked` is the temporary one for the whole call.
        let mut guest_held = GuestHeldPollProof::capture(guest, probe.into(), &mut rsrc);
        let mut published = PublishedWaitMask::default();
        publish_wait_mask(guest, signals.effective_mask(), &mut published).await;
        let result = loop {
            crate::syscalls::helpers::refresh_guest_poll_state(guest, probe.into(), &mut rsrc);
            // A guest-held proof this call already lost stays lost.
            guest_held.revalidate(guest, &mut rsrc);
            resource_request(guest, rsrc.clone()).await;
            guest_held.revalidate(guest, &mut rsrc);
            // Virtual timerfds first (`crate::syscalls::timerfd`), in the
            // staged cells' scratch while they hold the stack.
            let next_timerfd =
                crate::syscalls::timerfd::publish_expired_in(guest, timerfd_scratch).await;
            rsrc.poll_deadline =
                crate::syscalls::timerfd::poll_deadline_with(guest, deadline, next_timerfd);
            // A pending real-time timer expiration that the temporary mask lets
            // the kernel deliver or discard
            // (https://github.com/rrnewton/hermit/issues/4089).
            if let Err(error) = observe_under_mask(guest, temporary_mask).await {
                break Err(error);
            }
            let injected = match signals.inject_first_probe(guest, probe).await {
                Ok(result) => result,
                Err(error) => break Err(error),
            };
            if !probe.syscall_would_have_blocked(injected) {
                break injected.map_err(Into::into);
            }
            match signals.interrupted_with_state(guest) {
                Ok((false, _)) => {}
                // The next probe ends the call for it (see above).
                Ok((true, state))
                    if !signals.holds_signal()
                        && signals.interrupts_only_under_wait_mask(&state) =>
                {
                    continue;
                }
                Ok((true, _)) => break Err(Errno::ERESTARTNOHAND.into()),
                Err(error) => break Err(error),
            }
            rsrc.poll_attempt += 1;
            if let Some(deadline) = deadline
                && thread_observe_time(guest).await >= deadline
            {
                break Ok(0);
            }
            record_retry_event(guest, probe).await;
        };
        finish_wait_mask(
            guest,
            matches!(
                result,
                Err(Error::Errno(
                    reverie::syscalls::Errno::ERESTARTNOHAND | reverie::syscalls::Errno::EINTR
                ))
            ),
            &mut published,
        )
        .await;
        refuse_held_signal_loss(guest, Sysno::ppoll, result).await
    }

    /// Whether a masked ppoll that would wait polls with the backend's own
    /// probes (`backend_masked_ppoll_wait`): on a backend that runs the call
    /// itself and reports the guest's signal state
    /// (`reads_signal_state_from_backend`, KVM), under the scheduler, and
    /// outside record and replay (which that backend refuses anyway).
    fn backend_serves_masked_ppoll_waits(&self) -> bool {
        crate::syscalls::reads_signal_state_from_backend(&self.cfg.backend)
            && self.cfg.sequentialize_threads
            && !self.cfg.recordreplay_modes
    }

    /// SIGSHARE S2b: the wait of a ppoll with a temporary mask, `call` with
    /// the mask `signal_mask` as `read_ppoll_mask` read it, on a backend that
    /// runs the call itself and reports the guest's signal state
    /// (`backend_serves_masked_ppoll_waits`). Each turn runs a zero-timeout
    /// probe that carries a private copy of the mask (`stage_ppoll_probe`),
    /// so the backend applies the mask inside the call, as Linux installs it
    /// for the whole call: a pending signal the mask lets in ends the probe
    /// with `EINTR` and the mask installed for the delivery, and one the
    /// guest's own mask lets in but the temporary mask blocks never ends it.
    /// No signal can reach the thread between two probes: the backend
    /// delivers only at a return to the guest. The wait ends with the first
    /// probe whose result is not a wait (a ready count, `EINTR`, an error),
    /// or with 0 once `deadline` passes.
    ///
    /// A probe the backend declines (`ENOSYS`) found a pending signal the mask
    /// lets in that it cannot act on inside the call. When every such signal
    /// is one the process ignores, they are removed, as Linux discards them as
    /// the call returns before it restarts the call, and the wait goes on
    /// (`discard_ignored_signals_let_in`); anything else is refused by name
    /// (`refuse_masked_ppoll_signal`).
    async fn backend_masked_ppoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
        signal_mask: KernelSigset,
        deadline: Option<LogicalTime>,
    ) -> Result<i64, Error> {
        let mask = sanitize_ppoll_signal_mask(signal_mask);
        let mut rsrc = masked_ppoll_wait_request(guest.thread_state().dettid, signal_mask);
        rsrc.poll_deadline = poll_deadline_for(guest, deadline);
        rsrc.relies_on_guest_held_marks = poll_targets_guest_held(
            guest,
            call.fds().map(|fds| fds.as_raw()),
            call.nfds() as usize,
        );
        rsrc.polls_guest_state_only = rsrc.relies_on_guest_held_marks;
        rsrc.signal_state_task = guest_signal_state_task(guest);
        let mut guest_held = GuestHeldPollProof::capture(guest, call.into(), &mut rsrc);
        loop {
            crate::syscalls::helpers::refresh_guest_poll_state(guest, call.into(), &mut rsrc);
            // A guest-held proof this call already lost stays lost.
            guest_held.revalidate(guest, &mut rsrc);
            resource_request(guest, rsrc.clone()).await;
            guest_held.revalidate(guest, &mut rsrc);
            let Some((probe, staged)) = self
                .stage_ppoll_probe(guest, call, Some(signal_mask))
                .await?
            else {
                return match self.refuse_unstageable_ppoll_copy(guest).await {
                    Some(refused) => refused,
                    None => Err(Errno::ENOSYS.into()),
                };
            };
            // Virtual timerfds first (`crate::syscalls::timerfd`), in the
            // staged cells' scratch while they hold the stack.
            let next_timerfd =
                crate::syscalls::timerfd::publish_expired_in(guest, staged.timerfd_scratch).await;
            rsrc.poll_deadline =
                crate::syscalls::timerfd::poll_deadline_with(guest, deadline, next_timerfd);
            let result = inject_ppoll_probe(guest, probe).await;
            staged.release(guest);
            match result {
                Err(Errno::ENOSYS) => {
                    if !self.discard_ignored_signals_let_in(guest, mask).await? {
                        return self.refuse_masked_ppoll_signal(guest).await;
                    }
                }
                result if !probe.syscall_would_have_blocked(result) => {
                    return result.map_err(Into::into);
                }
                _ => {}
            }
            rsrc.poll_attempt += 1;
            if let Some(deadline) = deadline
                && thread_observe_time(guest).await >= deadline
            {
                return Ok(0);
            }
            record_retry_event(guest, probe).await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn masked_ppoll_probe_for_test<G: Guest<Self>>(
        &self,
        guest: &mut G,
        probe: syscalls::Ppoll,
        mask: usize,
    ) -> Result<i64, Error> {
        self.wait_on_masked_ppoll_probe(guest, probe, mask, None, None)
            .await
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
    #[cfg(target_arch = "x86_64")]
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
            crate::syscalls::timerfd::publish_expired(guest).await;
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
    #[cfg(target_arch = "x86_64")]
    pub(crate) async fn handle_internal_poll_until<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
        deadline: Option<LogicalTime>,
    ) -> Result<i64, Error> {
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("poll");
        rsrc.set_signal_interrupt_errno(Errno::EINTR);
        rsrc.signal_state_task = guest_signal_state_task(guest);
        rsrc.relies_on_guest_held_marks = poll_targets_guest_held(
            guest,
            call.fds().map(|fds| fds.as_raw()),
            call.nfds() as usize,
        );
        rsrc.polls_guest_state_only = rsrc.relies_on_guest_held_marks;
        let result = retry_nonblocking_syscall_with_timeout(guest, call, rsrc, deadline).await;
        keep_restart_block(guest, &result, RestartCall::Poll(call), deadline).await;
        Self::keep_deadline_for_rerun(guest, &result, call, deadline).await;
        result
    }

    /// Where a signal woke this `poll` and no handler ran, Linux restarts it
    /// through its restart block, which keeps the call's end time
    /// (`do_restart_poll`; fs/select.c:1076-1090 at v6.17). A backend that
    /// restarts a woken call only through `ERESTARTSYS`
    /// (`restarts_woken_calls_only_through_erestartsys`, KVM) restarts it by
    /// running the guest's own `poll` again (`woken_poll_errno`), with the timeout
    /// the guest passed, so the deadline is kept here, in the thread's restart
    /// record, as a rerun record (`RestartCall::PollRerun`), for that next run
    /// of the same call at the same address ([`Self::handle_poll`], or
    /// `network_poll` under network recording or replay, which passes the
    /// record on; neither takes a record of any other kind), which then ends
    /// when the first would have
    /// (<https://github.com/rrnewton/hermit/issues/4929>). Any other syscall
    /// first discards the record, as it discards one kept for
    /// `restart_syscall`. A call whose errno is settled again at its return
    /// keeps the record whichever errno it ends with here. The settling drops
    /// it where it decides EINTR (`syscalls::helpers::settle_woken_wait_at_return`),
    /// and so does a delivery at that return whose handler the backend fails
    /// the call with EINTR for, a handler without `SA_RESTART`
    /// (`syscalls::helpers::check_woken_wait_delivery`): the program's next
    /// `poll` is then its own, and waits its whole timeout.
    #[cfg(target_arch = "x86_64")]
    pub(crate) async fn keep_deadline_for_rerun<G: Guest<Self>>(
        guest: &mut G,
        result: &Result<i64, Error>,
        call: syscalls::Poll,
        deadline: Option<LogicalTime>,
    ) {
        if keeps_poll_deadline_for_rerun(
            crate::syscalls::restarts_woken_calls_only_through_erestartsys(&guest.config().backend),
            deadline,
            guest.thread_state().woken_wait,
            result,
        ) {
            let rip = crate::arch::instruction_pointer(&guest.regs().await);
            guest.thread_state_mut().restart_block = Some(crate::syscalls::helpers::RestartBlock {
                rip,
                deadline,
                call: RestartCall::PollRerun(call),
            });
        }
    }

    /// Handle a poll syscall that deponds on external, nondeterminstic IO.
    ///
    /// `deadline` is the end of a positive timeout (`kernel_poll_deadline`).
    #[cfg(target_arch = "x86_64")]
    pub(crate) async fn handle_external_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
        deadline: Option<KernelPollDeadline>,
    ) -> Result<i64, Error> {
        let len = call.nfds();
        let time_delta = Duration::from_millis(call.timeout() as u64);

        if len == 0 && time_delta.is_zero() {
            let request = Self::sleep_request(guest, time_delta).await;
            resource_request(guest, request).await;
            Ok(0)
        } else if call.timeout() == 0 {
            // Not shielded, as no zero-timeout probe is: a signal pending at
            // its entry still ends it with a restart code.
            print_poll(&call);
            self.run_kernel_poll(guest, call, deadline).await
        } else {
            print_poll(&call);
            let result = self
                .record_or_replay_shielded_wait(guest, Syscall::Poll(call))
                .await;
            let result = kernel_poll_result(&call, result, deadline.is_some());
            // The shielded wait replaced `run_kernel_poll` on this path, but
            // a signal that ends the poll still leaves the guest's own
            // `restart_syscall` to resume it, so keep the same record
            // `run_kernel_poll_as` keeps.
            if let Some(deadline) = deadline {
                keep_restart_block(
                    guest,
                    &result,
                    RestartCall::KernelPoll(call, deadline),
                    None,
                )
                .await;
            }
            result
        }
    }

    /// Runs `call` in the kernel through the recorder. A signal that ends a
    /// poll with a positive timeout there leaves `ERESTART_RESTARTBLOCK`, and
    /// with no handler Linux restarts it through `restart_syscall` until the
    /// end time it kept (`do_restart_poll`), so such a poll keeps `deadline`
    /// for that restart (`RestartCall::KernelPoll`). A poll without a deadline
    /// restarts by running again (`kernel_poll_result`), and one that a
    /// signal stopped before it ran returns `ERESTARTNOHAND`
    /// (`kernel_poll_restart_code`).
    #[cfg(target_arch = "x86_64")]
    async fn run_kernel_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
        deadline: Option<KernelPollDeadline>,
    ) -> Result<i64, Error> {
        self.run_kernel_poll_as(guest, call, deadline, Syscall::Poll(call))
            .await
    }

    /// `run_kernel_poll`, with `run` the call that runs for `call` (or that
    /// replay serves): the poll itself, or the guest's own `restart_syscall`
    /// that restarts it (`resume_kernel_poll`).
    #[cfg(target_arch = "x86_64")]
    async fn run_kernel_poll_as<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
        deadline: Option<KernelPollDeadline>,
        run: Syscall,
    ) -> Result<i64, Error> {
        debug_assert!(
            polls_in_kernel(self.cfg.sequentialize_threads, self.cfg.recordreplay_modes),
            "a plain or strict run emulates poll; it never runs one in the kernel"
        );
        let raw = self
            .record_or_replay_blocking_as(guest, Syscall::Poll(call), run)
            .await;
        let before_run = poll_stopped_before_it_ran(&raw);
        let result = kernel_poll_result(&call, raw, deadline.is_some());
        if let Some(deadline) = deadline {
            keep_restart_block(
                guest,
                &result,
                RestartCall::KernelPoll(call, deadline),
                None,
            )
            .await;
        }
        kernel_poll_restart_code(result, before_run)
    }

    /// The kernel's restart of a `poll` with a positive timeout that a signal
    /// ended in the kernel (`RestartCall::KernelPoll`). `restart` is the
    /// guest's own `restart_syscall`. It runs in place: the poll ran in the
    /// kernel, so the thread's restart block describes it, and Linux's
    /// `do_restart_poll` waits until the end time the poll kept and writes its
    /// `revents`. Nothing runs in its stead, so no signal is taken off the queue
    /// before the restart runs, as the single step of a poll injected for it
    /// did: the kernel delivers pending signals in its own order when the
    /// restart returns. Record keeps the restart's outputs as the poll's, and
    /// replay serves them (`hermit_record_format::event::InterruptedPoll`).
    ///
    /// The restart names the poll in its argument registers, which Linux
    /// leaves as the poll left them (`restart_names_poll`). If it does not,
    /// the poll runs again with the time left until `deadline`, as before:
    /// rounded up to whole milliseconds, so the poll never ends before its
    /// deadline, and 0 once the deadline has passed. Under record and replay
    /// that poll keeps the timeout it was issued with (`KernelPollDeadline`).
    /// Either way it runs the recorded kernel call, never the zero-length
    /// sleep `handle_external_poll` makes of a poll of no descriptors.
    #[cfg(target_arch = "x86_64")]
    pub(crate) async fn resume_kernel_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
        deadline: KernelPollDeadline,
        restart: Syscall,
    ) -> Result<i64, Error> {
        if restart_names_poll(&restart, &call) {
            return self
                .run_kernel_poll_as(guest, call, Some(deadline), restart)
                .await;
        }
        let call = match deadline.0 {
            Some(end) => call.with_timeout(remaining_poll_millis(Instant::now(), end)),
            None => call,
        };
        self.run_kernel_poll(guest, call, Some(deadline)).await
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
    /// Record/replay records each answer and replays it; replay reapplies only the recorded
    /// successes, so its epoll sets hold what the recording's held
    /// (<https://github.com/rrnewton/hermit/issues/31>).
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
        // A NON-NULL sigmask swaps the signal mask atomically for the duration
        // of the wait. Under record and replay a masked call that may wait is
        // a kernel wait outside the turn (`record_or_replay_masked_epoll_pwait`).
        // A sequentialized run on a backend whose kernel reports the guest's
        // signal state polls it under that mask
        // (`handle_internal_masked_epoll_pwait`). Otherwise, and for a zero
        // timeout, it keeps its turn: on another backend, or without
        // sequentialized threads, such a call can still block the scheduler
        // (https://github.com/rrnewton/hermit/issues/4041), and because it
        // never sleeps outside the runnable set the scheduler needs no record
        // of its temporary mask (`Resources::blocked_signal_mask`).
        if call.sigmask().is_some() && self.cfg.recordreplay_modes && call.timeout() != 0 {
            return self.record_or_replay_masked_epoll_pwait(guest, call).await;
        }
        // Under the in-guest delivery gate (A5 step 3c.1d, D39) a masked call
        // polls with no mask on its probes and decides under its temporary
        // mask, with D37's EINTR exception; a zero timeout makes one maskless
        // probe (`gated_masked_epoll_pwait`; IR-9).
        if call.sigmask().is_some()
            && self.cfg.sequentialize_threads
            && self.cfg.in_guest_signal_delivery
        {
            return Box::pin(self.gated_masked_epoll_pwait(guest, call)).await;
        }
        if call.sigmask().is_some()
            && call.timeout() != 0
            && self.cfg.sequentialize_threads
            && self.cfg.backend.supports_blocked_wait_signal_interruption
        {
            return self.handle_internal_masked_epoll_pwait(guest, call).await;
        }
        if call.sigmask().is_some() {
            self.observe_under_temporary_mask(guest, call.sigmask(), call.sigsetsize(), true)
                .await?;
            let dettid = guest.thread_state().dettid;
            resource_request(guest, Resources::new(dettid)).await; // empty request
            // The raw call runs in this turn. A zero timeout only probes, once a
            // virtual timerfd is readable exactly when its deadline has passed;
            // any other waits in the host (`crate::syscalls::timerfd`).
            if call.timeout() == 0 {
                crate::syscalls::timerfd::publish_expired(guest).await;
            } else {
                crate::syscalls::timerfd::refuse_host_wait(guest, "epoll_pwait with a signal mask");
            }
            return Ok(self.record_or_replay(guest, call).await?);
        }
        if self.cfg.recordreplay_modes && call.timeout() == 0 {
            // Cannot block, but still yield a scheduler turn so a polling thread
            // cannot monopolize the guest between preemptions.
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            self.record_or_replay_shielded_wait(guest, Syscall::EpollPwait(call))
                .await
        } else {
            self.handle_internal_epoll_pwait(guest, call).await
        }
    }

    /// A masked `epoll_pwait` that may wait, under sequentialized threads on a
    /// backend whose kernel reports the guest's signal state. It used to run
    /// the real call in the thread's turn, so nothing else ran while it waited
    /// and it returned 0 at once or at its timeout
    /// (https://github.com/rrnewton/hermit/issues/4041). It now polls, as an
    /// unmasked `epoll_pwait` does, with probes that carry no mask, and decides
    /// interruption against the temporary mask read at entry
    /// (`retry_blocking_wait_under_mask`): a signal the mask blocks never ends
    /// the call and is delivered after it, and one the mask lets in ends it
    /// with EINTR although the guest's own mask may block it. A zero-timeout
    /// `epoll_pwait` never looks at pending signals, so no probe could be ended
    /// by one in any case. A signal that only the temporary mask lets in stays
    /// pending under the guest's own mask once it is put back, and is delivered
    /// under the temporary mask (`inject_signal_delivery`), as for a masked
    /// pselect6. Linux checks the mask's size, then reads it, before it looks
    /// at the epoll descriptor.
    async fn handle_internal_masked_epoll_pwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollPwait,
    ) -> Result<i64, Error> {
        if call.sigsetsize() != KERNEL_SIGSET_SIZE {
            return Err(Errno::EINVAL.into());
        }
        let mask_address = call.sigmask().ok_or(Errno::EFAULT)?;
        let mask = crate::syscalls::signal::restart_if_first_probe_not_run(
            read_kernel_sigset(guest, mask_address.cast()).await,
        )?;
        super::signal_shadow::note_wait_mask(guest, mask);
        // A pending real-time timer expiration that the mask lets the kernel
        // deliver or discard (https://github.com/rrnewton/hermit/issues/4089).
        observe_under_mask(guest, mask).await?;
        let mask = sanitize_ppoll_signal_mask(mask);
        let deadline = millis_duration_to_absolute_timeout(guest, call.timeout()).await;
        self.wait_on_masked_epoll(
            guest,
            call.with_sigmask(None),
            mask,
            deadline,
            "epoll_pwait",
        )
        .await
    }

    /// The wait of a masked epoll_pwait or epoll_pwait2 under sequentialized
    /// threads, once the call's temporary `mask` (sanitized) has been read at
    /// entry: `probe`, an unmasked epoll_pwait, polls until an event, an
    /// error, a signal that `mask` lets in (EINTR), or `deadline`, which
    /// returns 0 (`handle_internal_masked_epoll_pwait`).
    async fn wait_on_masked_epoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        probe: syscalls::EpollPwait,
        mask: KernelSigset,
        deadline: Option<LogicalTime>,
        fyi: &'static str,
    ) -> Result<i64, Error> {
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi(fyi);
        let (result, deliver) = retry_blocking_wait_under_mask(
            guest,
            probe,
            rsrc,
            // Both calls return 0 at their timeout.
            deadline.map(|deadline| (deadline, Ok(0))),
            Some(crate::syscalls::signal::kernel_installed_signal_mask(mask)),
        )
        .await;
        if deliver && matches!(result, Err(Error::Errno(Errno::EINTR))) {
            // The probes staged nothing, so they say nothing about room for
            // these 24 bytes. `stage_private_cells`, which tests a window by
            // reading it, finds none only on a stack whose pointer is less
            // than 24 bytes above unreadable memory and whose red zone is
            // unreadable too
            // (https://github.com/rrnewton/hermit/issues/4113). Then the signal
            // stays pending under the guest's own mask, and the call returns
            // EINTR without its handler run and logs this warning.
            match stage_private_cells(guest, PrivateCells::MaskAndZeroTimeout(mask), &[]).await? {
                Some(staged) => {
                    inject_signal_delivery(guest, staged.mask(), staged.after_mask()).await;
                    staged.release(guest);
                }
                None => tracing::warn!(
                    "[tid {}] no room to deliver an {fyi} signal under its temporary mask; its \
                     handler runs only once the guest unblocks it",
                    guest.tid()
                ),
            }
        }
        result
    }

    /// A masked `epoll_pwait` that may wait, under record and replay. As for a
    /// masked pselect6 or ppoll there, the kernel owns the wait outside the
    /// scheduler turn, so the timer's signal can end it as on Linux; it kept
    /// its turn before, so nothing else ran and the call returned at its
    /// timeout (https://github.com/rrnewton/hermit/issues/4041). The call sleeps
    /// under a private copy of the mask read at entry, which a sibling cannot
    /// rewrite, and the scheduler records its sleeping mask as unknown
    /// (`Resources::blocked_signal_mask_unknown`). Replay serves the call from
    /// the recording, so for a recorded EINTR it delivers the signal under that
    /// mask (`end_replayed_pselect6_under_its_temporary_mask`).
    async fn record_or_replay_masked_epoll_pwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollPwait,
    ) -> Result<i64, Error> {
        if call.sigsetsize() != KERNEL_SIGSET_SIZE {
            return Err(Errno::EINVAL.into());
        }
        let mask_address = call.sigmask().ok_or(Errno::EFAULT)?;
        let mask = crate::syscalls::signal::restart_if_first_probe_not_run(
            read_kernel_sigset(guest, mask_address.cast()).await,
        )?;
        super::signal_shadow::note_wait_mask(guest, mask);
        // The copy may go in the red zone on a stack with no scratch below it,
        // clear of the events buffer the kernel writes (`stage_private_cells`;
        // re-review of https://github.com/rrnewton/hermit/pull/4087).
        let live: Vec<std::ops::Range<usize>> = call
            .events()
            .map(|events| {
                let start = events.as_raw();
                let len = usize::try_from(call.maxevents().max(0)).unwrap_or(0)
                    * std::mem::size_of::<libc::epoll_event>();
                start..start.saturating_add(len)
            })
            .into_iter()
            .collect();
        let Some(staged) = stage_private_cells(guest, PrivateCells::Mask(mask), &live).await?
        else {
            return self.refuse_unstageable_epoll_pwait_mask(guest, call).await;
        };
        let result = self
            .record_or_replay_blocking_with_unknown_mask(
                guest,
                Syscall::EpollPwait(call.with_sigmask(Addr::from_raw(staged.mask()))),
            )
            .await;
        staged.release(guest);
        if self.cfg.replaying && matches!(result, Err(Error::Errno(Errno::EINTR))) {
            end_replayed_pselect6_under_its_temporary_mask(guest, mask).await;
        }
        result
    }

    /// Refuses a masked record/replay `epoll_pwait` whose private mask copy has
    /// nowhere to go: no stack scratch below the red zone, and every red-zone
    /// window overlaps the events buffer (`refuse_unstageable_pselect6_mask`
    /// for pselect6). A run that allows unsupported operations forwards the
    /// guest's own mask.
    async fn refuse_unstageable_epoll_pwait_mask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollPwait,
    ) -> Result<i64, Error> {
        use std::io::Write as _;
        let message = "unsupported: epoll_pwait with a temporary signal mask, on a stack \
            with no room below its red zone whose red zone the call's events buffer fills: \
            Hermit has nowhere to put the private copy of the mask \
            (https://github.com/rrnewton/hermit/pull/4087)";
        tracing::error!("[tid {}] {message}", guest.tid());
        if self.cfg.panic_on_unsupported_syscalls {
            let _ = writeln!(crate::util::RetryingStderr, "{message}");
            return self
                .refuse_unserviceable_operation(guest, Sysno::epoll_pwait, Errno::EFAULT)
                .await;
        }
        self.record_or_replay_blocking_with_unknown_mask(guest, Syscall::EpollPwait(call))
            .await
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
            crate::syscalls::timerfd::publish_expired(guest).await;
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
    /// like epoll_pwait: under record and replay a call that may wait waits
    /// outside the turn; under sequentialized threads it polls
    /// (`handle_internal_epoll_pwait2`); otherwise, and for a zero timeout, a
    /// scheduler yield point is followed by the raw call.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#773)
    pub async fn handle_epoll_pwait2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        // epoll_pwait2(epfd, events, maxevents, timeout, sigmask, sigsetsize)
        let (_, args) = call.into_parts();
        // Under the in-guest delivery gate (A5 step 3c.1d, D39), as a masked
        // epoll_pwait (`gated_masked_epoll_pwait2`), whose D41 check comes
        // before anything else, the observation and the timeout's read below
        // included (round-2 ruling 3); its mask's read is then the call's
        // first injection.
        if !self.cfg.recordreplay_modes
            && self.cfg.sequentialize_threads
            && args.arg4 != 0
            && self.cfg.in_guest_signal_delivery
        {
            return Box::pin(self.gated_masked_epoll_pwait2(guest, args, true)).await;
        }
        // The call's first injection, if it makes one; when it does not, the
        // mask's read below is the first.
        let observed = self
            .observe_under_temporary_mask(
                guest,
                Addr::<libc::sigset_t>::from_raw(args.arg4),
                args.arg5,
                true,
            )
            .await?;
        // A NULL timeout waits forever; a zero timespec cannot wait and keeps
        // the turn.
        let zero_timeout = args.arg3 != 0
            && Addr::<Timespec>::from_raw(args.arg3)
                .and_then(|timeout| guest.memory().read_value(timeout).ok())
                .is_some_and(|timeout: Timespec| timeout.tv_sec == 0 && timeout.tv_nsec == 0);
        // Under record and replay, a call that may wait does so outside the
        // scheduler turn, as epoll_pwait's does: unmasked through the logged
        // background path, masked as a kernel wait under a private copy of its
        // mask (`record_or_replay_masked_epoll_pwait2`).
        if self.cfg.recordreplay_modes {
            if !zero_timeout {
                return if args.arg4 != 0 {
                    self.record_or_replay_masked_epoll_pwait2(guest, args, !observed)
                        .await
                } else {
                    self.record_or_replay_shielded_wait(guest, call).await
                };
            }
        } else if self.cfg.sequentialize_threads
            && !zero_timeout
            && (args.arg4 == 0 || self.cfg.backend.supports_blocked_wait_signal_interruption)
        {
            // With threads sequentialized, a call that may wait polls, as
            // epoll_pwait does, rather than waiting in the kernel inside the
            // turn, where no other guest thread could run
            // (https://github.com/rrnewton/hermit/issues/4228). A masked call
            // on a backend whose kernel does not report the guest's signal
            // state keeps the turn, as a masked epoll_pwait does there.
            return self
                .handle_internal_epoll_pwait2(guest, args, !observed)
                .await;
        }
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        // The raw call runs in this turn. A zero timeout only probes, once a
        // virtual timerfd is readable exactly when its deadline has passed;
        // any other wait here is in the host (`crate::syscalls::timerfd`).
        if zero_timeout {
            crate::syscalls::timerfd::publish_expired(guest).await;
        } else {
            crate::syscalls::timerfd::refuse_host_wait(guest, "epoll_pwait2 with a signal mask");
        }
        // Only a call that may wait under the thread's own mask needs the
        // shield; a masked one waits under its own temporary mask.
        let shield = if zero_timeout || args.arg4 != 0 {
            crate::syscalls::helpers::IgnoredSignalShield::none()
        } else {
            crate::syscalls::helpers::IgnoredSignalShield::raise(guest).await?
        };
        let result = self
            .record_or_replay(guest, call)
            .await
            .map_err(Error::from);
        shield.lower(guest).await?;
        result
    }

    /// `record_or_replay_blocking` for a wait in the kernel under the thread's
    /// own mask, with the signals the guest ignores blocked for the wait
    /// (`IgnoredSignalShield`): an `epoll_wait`, or an `epoll_pwait` or
    /// `epoll_pwait2` without a mask, or a `poll`, `select`, or `ppoll` or
    /// `pselect6` naming no mask. Linux discards those signals at generation,
    /// so they never end the wait, but ptrace queues them.
    async fn record_or_replay_shielded_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let shield = crate::syscalls::helpers::IgnoredSignalShield::raise(guest).await?;
        let result = self.record_or_replay_blocking(guest, call).await;
        shield.lower(guest).await?;
        result
    }

    /// `guest.inject(call)` for a `select`, or a `pselect6` handed no mask,
    /// that waits in the kernel without sequentialized threads, with the
    /// signals the guest ignores blocked for the wait when it `may_wait`
    /// (`IgnoredSignalShield`).
    async fn inject_shielded_wait<G: Guest<Self>, S: SyscallInfo>(
        &self,
        guest: &mut G,
        call: S,
        may_wait: bool,
    ) -> Result<i64, Error> {
        let shield = if may_wait {
            crate::syscalls::helpers::IgnoredSignalShield::raise(guest).await?
        } else {
            crate::syscalls::helpers::IgnoredSignalShield::none()
        };
        let result = guest.inject(call).await.map_err(Error::from);
        shield.lower(guest).await?;
        result
    }

    /// An epoll_pwait2 that may wait, under sequentialized threads: it polls
    /// with zero-timeout probes, as epoll_pwait does. A probe is an
    /// epoll_pwait with no mask, whose answer for a zero timeout is the same
    /// as epoll_pwait2's. Linux copies and checks the timespec before the
    /// mask (`record_or_replay_masked_epoll_pwait2`), then the mask's size
    /// and the mask, before it looks at the epoll descriptor. An unmasked
    /// call waits as an unmasked epoll_pwait does
    /// (`handle_internal_epoll_pwait`), a masked one as a masked epoll_pwait
    /// does (`wait_on_masked_epoll`). It used to make the real call in the
    /// thread's turn, so it returned 0 at its timeout, or never, when only
    /// another guest thread could make an event ready
    /// (https://github.com/rrnewton/hermit/issues/4228).
    /// `first` says the mask's read is the call's first injection
    /// (`restart_if_first_probe_not_run`).
    async fn handle_internal_epoll_pwait2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: SyscallArgs,
        first: bool,
    ) -> Result<i64, Error> {
        let timeout = match Addr::<Timespec>::from_raw(args.arg3) {
            Some(timeout) => {
                let timeout: Timespec = guest.memory().read_value(timeout)?;
                Some(ppoll_timeout_duration(timeout)?)
            }
            None => None,
        };
        let probe = syscalls::EpollPwait::new()
            .with_epfd(args.arg0 as i32)
            .with_events(AddrMut::from_raw(args.arg1))
            .with_maxevents(args.arg2 as i32)
            .with_timeout(0)
            .with_sigmask(None)
            .with_sigsetsize(args.arg5);
        if args.arg4 == 0 {
            let (_, deadline) = ppoll_start(guest, timeout).await;
            let mut rsrc = Resources::new(guest.thread_state().dettid);
            rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
            rsrc.fyi("epoll_pwait2");
            return retry_nonblocking_syscall_with_timeout(guest, probe, rsrc, deadline).await;
        }
        if args.arg5 != KERNEL_SIGSET_SIZE {
            return Err(Errno::EINVAL.into());
        }
        let mask_address = Addr::<libc::sigset_t>::from_raw(args.arg4).ok_or(Errno::EFAULT)?;
        let read = read_kernel_sigset(guest, mask_address).await;
        let mask = if first {
            crate::syscalls::signal::restart_if_first_probe_not_run(read)?
        } else {
            read?
        };
        super::signal_shadow::note_wait_mask(guest, mask);
        let (_, deadline) = ppoll_start(guest, timeout).await;
        // A pending real-time timer expiration that the mask lets the kernel
        // deliver or discard (https://github.com/rrnewton/hermit/issues/4089).
        observe_under_mask(guest, mask).await?;
        let mask = sanitize_ppoll_signal_mask(mask);
        self.wait_on_masked_epoll(guest, probe, mask, deadline, "epoll_pwait2")
            .await
    }

    /// Before a call that sleeps or probes under the temporary mask at `mask`
    /// (`sigsetsize` bytes), refuse the run by name if the mask lets the kernel
    /// take a pending real-time timer expiration that every thread blocked when
    /// it expired, and discard the ones it lets the kernel discard
    /// (https://github.com/rrnewton/hermit/issues/4089). A mask the kernel would
    /// reject fails the call itself, which then observes nothing.
    ///
    /// Returns whether it made an injection, which it does only while a
    /// real-time expiration is kept pending. With `first`, its mask read is
    /// the call's first injection, so a signal that stops it restarts the call
    /// (`restart_if_first_probe_not_run`); any other failed read is left to
    /// the call itself, as before.
    async fn observe_under_temporary_mask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mask: Option<Addr<'_, libc::sigset_t>>,
        sigsetsize: usize,
        first: bool,
    ) -> Result<bool, Error> {
        let Some(mask) = mask else {
            return Ok(false);
        };
        if sigsetsize != KERNEL_SIGSET_SIZE
            || !crate::scheduler::real_time_expiration_kept_pending()
        {
            return Ok(false);
        }
        match read_kernel_sigset(guest, mask).await {
            Ok(mask) => observe_under_mask(guest, mask).await?,
            Err(error @ Error::Errno(Errno::ERESTARTSYS)) if first => {
                return crate::syscalls::signal::restart_if_first_probe_not_run(Err(error));
            }
            Err(_) => {}
        }
        Ok(true)
    }

    /// `record_or_replay_masked_epoll_pwait` for `epoll_pwait2`, whose mask
    /// pointer and size are its fifth and sixth arguments.
    /// `first` says the mask's read is the call's first injection
    /// (`restart_if_first_probe_not_run`).
    async fn record_or_replay_masked_epoll_pwait2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: SyscallArgs,
        first: bool,
    ) -> Result<i64, Error> {
        // Linux copies and checks the timespec before the mask, so an
        // unreadable timeout is EFAULT and an invalid one EINVAL whatever the
        // mask and its size (Claude review of
        // https://github.com/rrnewton/hermit/pull/4128).
        if let Some(timeout) = Addr::<Timespec>::from_raw(args.arg3) {
            let timeout: Timespec = guest.memory().read_value(timeout)?;
            ppoll_timeout_duration(timeout)?;
        }
        if args.arg5 != KERNEL_SIGSET_SIZE {
            return Err(Errno::EINVAL.into());
        }
        let mask_address = Addr::<libc::sigset_t>::from_raw(args.arg4).ok_or(Errno::EFAULT)?;
        let read = read_kernel_sigset(guest, mask_address).await;
        let mask = if first {
            crate::syscalls::signal::restart_if_first_probe_not_run(read)?
        } else {
            read?
        };
        super::signal_shadow::note_wait_mask(guest, mask);
        // The copy may go in the red zone on a stack with no scratch below it,
        // clear of the events buffer the kernel writes and of the timeout it
        // reads, as for epoll_pwait (`stage_private_cells`).
        let events_len = usize::try_from((args.arg2 as i32).max(0)).unwrap_or(0)
            * std::mem::size_of::<libc::epoll_event>();
        let live: Vec<std::ops::Range<usize>> = [
            (args.arg1, events_len),
            (args.arg3, std::mem::size_of::<Timespec>()),
        ]
        .into_iter()
        .filter(|&(start, _)| start != 0)
        .map(|(start, len)| start..start.saturating_add(len))
        .collect();
        let Some(staged) = stage_private_cells(guest, PrivateCells::Mask(mask), &live).await?
        else {
            return self.refuse_unstageable_epoll_pwait2_mask(guest, args).await;
        };
        let mut private = args;
        private.arg4 = staged.mask();
        let result = self
            .record_or_replay_blocking_with_unknown_mask(
                guest,
                Syscall::Other(Sysno::epoll_pwait2, private),
            )
            .await;
        staged.release(guest);
        if self.cfg.replaying && matches!(result, Err(Error::Errno(Errno::EINTR))) {
            end_replayed_pselect6_under_its_temporary_mask(guest, mask).await;
        }
        result
    }

    /// `refuse_unstageable_epoll_pwait_mask` for `epoll_pwait2`, whose red
    /// zone the events buffer and the timeout fill.
    async fn refuse_unstageable_epoll_pwait2_mask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: SyscallArgs,
    ) -> Result<i64, Error> {
        use std::io::Write as _;
        let message = "unsupported: epoll_pwait2 with a temporary signal mask, on a stack \
            with no room below its red zone whose red zone the call's events buffer and \
            timeout fill: Hermit has nowhere to put the private copy of the mask \
            (https://github.com/rrnewton/hermit/issues/4088)";
        tracing::error!("[tid {}] {message}", guest.tid());
        if self.cfg.panic_on_unsupported_syscalls {
            let _ = writeln!(crate::util::RetryingStderr, "{message}");
            return self
                .refuse_unserviceable_operation(guest, Sysno::epoll_pwait2, Errno::EFAULT)
                .await;
        }
        self.record_or_replay_blocking_with_unknown_mask(
            guest,
            Syscall::Other(Sysno::epoll_pwait2, args),
        )
        .await
    }

    /// epoll_wait syscall (MAYHANG)
    #[cfg(target_arch = "x86_64")]
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
            self.record_or_replay_shielded_wait(guest, Syscall::EpollWait(call))
                .await
        } else {
            self.handle_internal_epoll_wait(guest, call).await
        }
    }

    /// Handle a guest-internal `epoll_wait` call that can be fully determinized.
    #[cfg(target_arch = "x86_64")]
    pub async fn handle_internal_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollWait,
    ) -> Result<i64, Error> {
        let timeout_millis = call.timeout();
        if timeout_millis == 0 {
            crate::syscalls::timerfd::publish_expired(guest).await;
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

        // A property the guest sets reaches the live area, not the frozen copy
        // it reads (`Config::property_snapshot`).
        if self.cfg.property_snapshot.is_some()
            && let Some(address) = uservaddr
        {
            let mut bytes = [0_u8; std::mem::size_of::<libc::sockaddr_un>()];
            let len = usize::try_from(addrlen).unwrap_or(0).min(bytes.len());
            if guest
                .memory()
                .read_exact(address.cast::<u8>(), &mut bytes[..len])
                .is_ok()
                && names_property_service(&bytes[..len])
            {
                warn_property_set_not_visible();
            }
        }

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

        // Whether the connection completes in this turn.
        let in_turn = self.completes_in_turn(guest, call);
        guest.thread_state_mut().nonblocking_result_normalized = false;
        let result = self.execute_nonblockable_fd_syscall(guest, call).await;
        // A retried attempt's `EISCONN`, read as success, saw only that the
        // open file was connected, by an earlier attempt or another thread's
        // connect, not to which socket, nor that it read the address now
        // there (Codex review of https://github.com/rrnewton/hermit/pull/4862).
        let completed_by_another_attempt =
            std::mem::take(&mut guest.thread_state_mut().nonblocking_result_normalized);
        // What the address names, as the attempt that connected read it. A
        // connect that completes in its turn made that attempt in this turn,
        // after the last request it filed, so no other thread has rewritten
        // the address since; an earlier read could name another socket than
        // the one it reached, if another thread rewrote the address between
        // attempts (Codex review of
        // https://github.com/rrnewton/hermit/pull/4862). A connect outside its
        // turn may have reached any name (`Detcore::note_unix_connect`).
        let connected_to = uservaddr.map_or(
            crate::syscalls::scm_rights::UnixAddress::Unnamed,
            |address| {
                crate::syscalls::scm_rights::unix_address(
                    &guest.memory(),
                    address.as_raw(),
                    usize::try_from(addrlen).unwrap_or(0),
                )
            },
        );
        if let Ok(0) = &result {
            // Pair it with nothing, and no name with anything.
            let in_turn = in_turn && !completed_by_another_attempt;
            self.note_unix_connect(guest, fd, &connected_to, in_turn);
        }
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
        if matches!(result, Ok(0)) {
            self.classify_socket_peer(guest, fd, SocketRole::Connected)
                .await;
        }
        // A connect ends what a guest-held socket was connected to: a datagram
        // socketpair end reconnected to another process's socket, or
        // disconnected, is no longer guest-held. A stream connection to a
        // listener this process made is (`classify_guest_held_peer`).
        if result.is_ok() {
            let _ = guest.thread_state().with_detfd(fd, |detfd| {
                if detfd.is_guest_held() {
                    detfd.clear_guest_held();
                }
            });
            let connecting = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.open_file_handle())
                .ok();
            let listener = self.listener_named(guest, &connected_to);
            let guest_created = crate::syscalls::scm_rights::both_ends_guest_made(
                in_turn && !completed_by_another_attempt,
                connecting.as_ref(),
                listener.as_ref(),
            );
            self.classify_guest_held_peer(guest, fd, guest_created)
                .await;
        }

        result
    }

    /// Mark guest-held (`DetFd::is_guest_held`) the pipes and sockets among
    /// `received`, descriptors a receive on `channel` just recorded from
    /// `SCM_RIGHTS`, whose object only guests hold. That needs `channel` to be
    /// guest-held, so the sender was a guest. Then a received descriptor is
    /// guest-held when this table already names its object, or the other end
    /// of the same pipe, through a guest-held description (the same host
    /// device and inode). A socket's peer credentials do not mark it here:
    /// they name its peer, not every holder of its open file.
    ///
    /// A received descriptor that joins the open file its sender's table
    /// modeled (`crate::syscalls::scm_rights`) shares that open file's mark,
    /// set or not, as a dup would. The mark it arrives with is sound: a join
    /// takes only what a guest's `sendmsg` or `sendmmsg` queued on the
    /// receiving socket's known peer (`queue_sent_rights`), so the sender is a
    /// guest; either the channel was guest-held, so the receiver is a guest
    /// too and every holder of the open file is one, or the send revoked every
    /// mark first (`note_descriptors_passed_out`, which both send paths run
    /// before they send). This function never sees a joined descriptor:
    /// `record_received_descriptors` returns only the descriptors it recorded
    /// as descriptions of their own (`Receipt::New`), so a join keeps its open
    /// file's mark as it is. The rule here holds for the open file all the
    /// same: the object or its other end is already guest-held in this table.
    /// A descriptor nothing queued names, such as one from a process outside
    /// the guest, is a description of its own and starts unmarked; this
    /// function marks it only on a guest-held channel, as a known object.
    /// Marks
    /// only ever claim that no process outside the guest holds the object;
    /// once a descriptor may have left the guest, the scheduler stops trusting
    /// every mark (`Scheduler::polls_only_guest_state`).
    async fn classify_received_descriptors<G: Guest<Self>>(
        &self,
        guest: &mut G,
        channel: RawFd,
        received: &[RawFd],
    ) {
        if received.is_empty() {
            return;
        }
        let from_a_guest = guest
            .thread_state()
            .with_detfd(channel, |detfd| detfd.is_guest_held())
            .unwrap_or(false);
        if !from_a_guest {
            return;
        }
        for &fd in received {
            let Ok((ty, stat)) = guest
                .thread_state()
                .with_detfd(fd, |detfd| (detfd.ty(), detfd.stat()))
            else {
                continue;
            };
            if !matches!(ty, FdType::Pipe | FdType::Socket) {
                continue;
            }
            let known = stat.is_some_and(|stat| {
                guest
                    .thread_state()
                    .names_guest_held_object(fd, stat.dev, stat.inode)
            });
            // A received socket's peer credentials name its peer, not every
            // holder of its open file: a host process may still hold the same
            // open file, having handed it in over a channel that is not
            // guest-held, from which a guest forwarded it here. So a socket
            // that joined nothing is marked only as a known object, never
            // from its credentials (Codex review of
            // https://github.com/rrnewton/hermit/pull/4862).
            if known {
                let _ = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.set_guest_held());
            }
        }
    }

    /// Mark socket `fd`, a connection `connect` or `accept` just made,
    /// guest-held (`DetFd::is_guest_held`) when guests alone hold both of its
    /// ends and its peer is the caller's own process.
    ///
    /// `guest_created` is the provenance, which the caller checks: the
    /// connecting socket and the listener are both sockets a guest made in
    /// this run and never passed out of the guest (`DetFd::is_guest_created`),
    /// the connecting socket being `fd` itself for a `connect` and, for an
    /// `accept`, the socket whose connection it took
    /// (`Detcore::note_unix_accept`). Only guests then hold the connecting
    /// socket, and only guests can accept from the listener, so only guests
    /// hold either end. Peer credentials alone prove neither: they name the
    /// process that connected or listened, not every holder of the open file,
    /// and a host process may have kept an alias of a socket it handed in
    /// (Codex review of https://github.com/rrnewton/hermit/pull/4862). On
    /// top of the provenance, `SO_PEERCRED` must name the caller's own
    /// process:
    ///
    /// - An accepted socket (`accept`, `accept4`): Linux takes its credentials
    ///   from the task that connected, when it connected.
    /// - A connected stream or seqpacket socket (`connect`): Linux copies the
    ///   credentials of whoever called `listen`.
    ///
    /// A received socket is never classified here
    /// (`classify_received_descriptors`): its credentials name its peer, but
    /// a host process may hold the received open file itself.
    ///
    /// A datagram socket's credentials are not its peer's, so it is never
    /// marked here. Nor is a listening socket: `listen` sets a listener's
    /// credentials to its caller's, so one this process listened on and then
    /// received names this process, yet any process that can name its
    /// address can connect and make it ready. Accept and connect never return
    /// a listener, and receipts are not classified here, so the
    /// `SO_ACCEPTCONN` test, which must read 0 and leaves a socket whose
    /// value cannot be read unmarked, is a guard against a future caller. A peer in
    /// another process, guest or not, and a socket with
    /// no peer credentials (any non-`AF_UNIX` socket, whose peer pid is 0) are
    /// left as they were. Best-effort and read-only: an injection that fails
    /// leaves the socket unmarked, and nothing the guest sees changes. Only
    /// plain runs read the mark for the scheduler's new idle-poller detection,
    /// so only they pay for it. Record/replay retains its earlier internal-FD
    /// polling and FIFO-open decisions without consulting these marks.
    pub(crate) async fn classify_guest_held_peer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        guest_created: bool,
    ) {
        if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes || !guest_created {
            return;
        }
        // A datagram socket's credentials are its creator's, or its
        // socketpair's, whatever it is connected to now: only a connection's
        // name its peer.
        let Some(kind) = socket_option::<libc::c_int, _, _>(guest, fd, libc::SO_TYPE).await else {
            return;
        };
        if kind != libc::SOCK_STREAM && kind != libc::SOCK_SEQPACKET {
            return;
        }
        // A listener's credentials are its own (`unix_listen` sets them to
        // the caller's), and an outside client's connect makes it ready.
        if socket_option::<libc::c_int, _, _>(guest, fd, libc::SO_ACCEPTCONN).await != Some(0) {
            return;
        }
        let Some(credentials) =
            socket_option::<libc::ucred, _, _>(guest, fd, libc::SO_PEERCRED).await
        else {
            return;
        };
        let peer = credentials.pid;
        if peer <= 0 {
            return;
        }
        // The caller's own process id in the same PID namespace that
        // `SO_PEERCRED` translated the peer's into.
        let Ok(own) = guest.inject(syscalls::Getpid::new()).await else {
            return;
        };
        if i64::from(peer) == own {
            let _ = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.set_guest_held());
        }
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
        let headers: Vec<usize> = call
            .msg()
            .map(|header| header.as_raw())
            .into_iter()
            .collect();
        self.refuse_sending_virtual_timerfds(guest, &headers);
        // The placeholder descriptions it sends, counted in flight, and what
        // each attempt checks again, or its refusal by name
        // (`Detcore::inspect_placeholder_send`).
        let (copies, outside_turn) = match self
            .inspect_send_placeholders(guest, Sysno::sendmsg, call.fd(), &headers, call)
            .await
        {
            Ok(inspected) => inspected,
            Err(refusal) => return refusal,
        };
        // A per-process file's record sequence, which every holder of the
        // open file shares, moves to Detcore's global state before the
        // descriptor can reach another process.
        Box::pin(self.share_sent_record_sequences(guest, &headers)).await;
        let in_turn = self.completes_in_turn(guest, call);
        if outside_turn || !sent_descriptors(guest, &headers).0.is_empty() {
            Self::note_descriptors_passed_out(guest, call.fd(), &headers, outside_turn);
        }
        self.note_sockets_passed_out(guest, &headers);
        let result = self.execute_nonblockable_fd_syscall(guest, call).await;
        guest.thread_state_mut().send_inspection = None;
        if outside_turn
            && Self::may_send_placeholders(guest)
            && let Some(refusal) =
                Box::pin(self.placeholder_send_after(guest, Sysno::sendmsg, call.fd())).await
        {
            return refusal;
        }
        if result.is_err() && !copies.is_empty() {
            let _ = crate::tool_global::placeholder_copies(
                guest,
                crate::procfs::locks::PlaceholderCopies::Unsent(copies),
            )
            .await;
        }
        if crate::syscalls::scm_rights::found_its_peer_gone(&result) {
            let addressed = sends_to_a_name(guest, &headers);
            self.note_peer_gone(guest, call.fd(), addressed);
        }
        let result = result?;
        guest.thread_state().forget_flock_modes();
        let mut followed = Vec::new();
        if let Some(stream) = self.rights_peer_is_stream(guest, call.fd()) {
            if in_turn {
                // One message, of `result` bytes.
                let sent = sent_rights(guest, &headers, |_| usize::try_from(result).unwrap_or(0));
                let out_of_band = call.flags() & libc::MSG_OOB != 0;
                followed = self
                    .queue_sent_rights(guest, call.fd(), stream, sent, out_of_band)
                    .await?;
            } else {
                self.lose_peer_lockstep(guest, call.fd());
            }
        }
        // The descriptors the send passed, as its control data named them at
        // entry, or, for a send outside the caller's turn, whose control data
        // another thread could rewrite before Linux read it, any descriptor of
        // the table: Detcore cannot follow their references to the end, but
        // for the copies the lock ledger counted in flight (`flock_model`).
        let passed = if outside_turn {
            guest.thread_state().open_fds()
        } else {
            sent_descriptors(guest, &headers).0
        };
        self.note_flock_descriptors_sent(guest, &passed, &followed)
            .await;
        Ok(result)
    }

    /// The placeholder step of a send `call` (`sysno`, on `socket`), whose
    /// messages' headers are at the guest addresses `headers`, where its
    /// `SCM_RIGHTS` descriptors can matter to the `/proc` directories Detcore
    /// serves (`Detcore::may_send_placeholders`): the inspection
    /// (`Detcore::inspect_placeholder_send`), kept for the attempts the call
    /// makes (`ThreadState::send_inspection`), with the descriptions it
    /// counted in flight, or the refusal by name; and, for every send,
    /// whether the call runs outside this thread's turn in a run that
    /// sequentializes threads (a blocking call), after which
    /// `Detcore::placeholder_send_after` checks what joined the table. The
    /// inspection is boxed: only a send that may pass a placeholder carries
    /// it.
    async fn inspect_send_placeholders<G, C>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        socket: RawFd,
        headers: &[usize],
        call: C,
    ) -> Result<(Vec<crate::types::OpenFileId>, bool), Result<i64, Error>>
    where
        G: Guest<Self>,
        C: NonblockableSyscall + Into<Syscall> + Copy,
    {
        // Without sequentialized threads no thread has a turn: every check
        // Detcore makes may race the guest's other threads, that mode's named
        // residual, which a refusal here would not narrow.
        let outside_turn =
            self.cfg.sequentialize_threads && self.nonblockable_call_runs_outside_turn(guest, call);
        if !Self::may_send_placeholders(guest) {
            return Ok((Vec::new(), outside_turn));
        }
        let (copies, inspection) =
            Box::pin(self.inspect_placeholder_send(guest, sysno, socket, headers, outside_turn))
                .await?;
        guest.thread_state_mut().send_inspection = Some(inspection);
        Ok((copies, outside_turn))
    }

    /// Note, for the scheduler, that a send on `sockfd` may pass
    /// descriptors over `SCM_RIGHTS` to a process outside
    /// the guest (`ThreadState::passed_descriptors_out_unreported`). A passed
    /// descriptor may be, or reach, an object some guest holds a guest-held
    /// description of, in any process's table, which a process outside
    /// Detcore's view could then make ready; so from then on the scheduler
    /// trusts no guest-held mark (`Scheduler::polls_only_guest_state`), and
    /// that covers polls already waiting. Control data read at entry cannot
    /// prove that no descriptor will be sent: a sibling may rewrite it before
    /// a later attempt. Linux imports each attempt's current msghdr and control
    /// data (`copy_msghdr_from_user`, `____sys_sendmsg`, Linux v6.12
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/socket.c?h=v6.12#n2529>).
    /// A guest-held channel sends only to its guest peer, whatever `msg_name`
    /// says, but that exception requires an in-turn attempt: outside the turn
    /// even the destination descriptor may change before Linux imports it.
    /// Each in-turn retry checks the channel again immediately before sending.
    /// Revoke before the call regardless of its eventual result: sendmmsg can
    /// send a message before failing to write its length back to guest memory
    /// (Linux v6.12 `__sys_sendmmsg`,
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/socket.c?h=v6.12#n2747>).
    pub(crate) fn note_descriptors_passed_out<G: Guest<Self>>(
        guest: &mut G,
        sockfd: RawFd,
        headers: &[usize],
        outside_turn: bool,
    ) {
        let reaches_a_guest = guest
            .thread_state()
            .with_detfd(sockfd, |detfd| detfd.is_guest_held())
            .unwrap_or(false);
        if reaches_a_guest && !outside_turn {
            return;
        }
        // No control data at entry proves nothing about what another thread
        // can install before a later attempt, and outside the turn even the
        // destination may change: the send counts as passing descriptors.
        // The descriptors the entry headers name also lose their
        // guest-created mark, as before.
        guest.thread_state_mut().passed_descriptors_out_unreported = true;
        let (sent, unread) = sent_descriptors(guest, headers);
        crate::reserved_input::before_descriptor_export(guest, &sent, unread)
            .unwrap_or_else(|error| panic!("reserved-input export publication failed: {error}"));
        if !sent.is_empty() || unread {
            clear_guest_created(guest, &sent);
        }
    }

    /// Notes, for a `sendmsg` or `sendmmsg` `call` about to run outside the
    /// caller's turn in a plain sequentialized run, that it may pass
    /// descriptors out of the guest
    /// (`ThreadState::passed_descriptors_out_unreported`), whatever its
    /// descriptor and messages name now. The kernel imports them at a
    /// host-chosen moment, after other guest threads ran: one may have
    /// replaced the descriptor with an `AF_UNIX` socket that is not
    /// guest-held and rewritten a message to pass a guest-held one. Called
    /// where the call leaves the turn (`record_or_replay_blocking_resource_then`),
    /// so the request that leaves it carries the report, whatever the handler
    /// decided earlier and however many requests came between (Codex review
    /// of https://github.com/rrnewton/hermit/pull/4862).
    pub(crate) fn passes_descriptors_out_outside_the_turn<G: Guest<Self>>(
        guest: &mut G,
        call: Syscall,
    ) {
        let config = guest.config();
        if config.sequentialize_threads
            && !config.recordreplay_modes
            && matches!(call, Syscall::Sendmsg(_) | Syscall::Sendmmsg(_))
        {
            // The native ancillary import and descriptor lookup can happen
            // after another task changed them. The old poll-reliance report
            // is not a persistent-reference certificate.
            crate::reserved_input::before_descriptor_export(guest, &[], true).unwrap_or_else(
                |error| panic!("reserved-input export publication failed: {error}"),
            );
            guest.thread_state_mut().passed_descriptors_out_unreported = true;
        }
    }

    /// Whether `call`, a `sendmsg` or `sendmmsg` about to make an attempt in
    /// the caller's turn, would pass descriptors out of the guest that this
    /// thread has not reported yet (`note_descriptors_passed_out`), read from
    /// its messages as they are now: a sibling may have rewritten them since
    /// the handler read them, or while an earlier attempt waited. If so, the
    /// pass-out is noted (`ThreadState::passed_descriptors_out_unreported`),
    /// and the caller reports it in a request of its own before the attempt
    /// runs. Only plain sequentialized runs read the marks, so only they
    /// check; once this thread has reported a pass-out the scheduler trusts no
    /// mark again, so it checks no more (Codex review of
    /// https://github.com/rrnewton/hermit/pull/4862).
    pub(crate) fn passes_descriptors_out_now<G: Guest<Self>>(guest: &mut G, call: Syscall) -> bool {
        let config = guest.config();
        if !config.sequentialize_threads || config.recordreplay_modes {
            return false;
        }
        let (sockfd, headers): (RawFd, Vec<usize>) = match call {
            Syscall::Sendmsg(call) => (
                call.fd(),
                call.msg()
                    .map(|header| header.as_raw())
                    .into_iter()
                    .collect(),
            ),
            Syscall::Sendmmsg(call) => (
                call.sockfd(),
                call.msgvec().map_or_else(Vec::new, |vector| {
                    (0..call.vlen().min(libc::UIO_MAXIOV as u32) as usize)
                        .map(|index| vector.as_raw() + index * std::mem::size_of::<Mmsghdr>())
                        .collect()
                }),
            ),
            _ => return false,
        };
        let reaches_a_guest = guest
            .thread_state()
            .with_detfd(sockfd, |detfd| detfd.is_guest_held())
            .unwrap_or(false);
        if reaches_a_guest {
            return false;
        }
        let (sent, unread) = sent_descriptors(guest, &headers);
        crate::reserved_input::before_descriptor_export(guest, &sent, unread)
            .unwrap_or_else(|error| panic!("reserved-input export publication failed: {error}"));
        if guest.thread_state().passed_descriptors_out_reported {
            return false;
        }
        if sent.is_empty() && !unread {
            return false;
        }
        guest.thread_state_mut().passed_descriptors_out_unreported = true;
        clear_guest_created(guest, &sent);
        true
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
        let (sent, unread) = sent_descriptors(guest, headers);
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

    /// Refuse the run, by name, before messages at `headers` pass a timerfd
    /// over `SCM_RIGHTS` while timerfds run on Detcore's virtual clock, armed
    /// or not (one armed after it is sent would be modelled only by the
    /// sender), or an epoll descriptor while this process holds an armed one
    /// (its set may include it). The receiving process keeps no model of the
    /// timer, so its probes would never make it readable and it would wait
    /// where Linux wakes (`crate::syscalls::timerfd`). A message whose control
    /// data Detcore cannot read refuses too while this process holds an armed
    /// timerfd.
    fn refuse_sending_virtual_timerfds<G: Guest<Self>>(&self, guest: &mut G, headers: &[usize]) {
        if !crate::syscalls::timerfd::enabled(guest) {
            return;
        }
        let holds_armed = crate::syscalls::timerfd::has_virtual_timerfds(guest);
        let (sent, unread) = sent_descriptors(guest, headers);
        if unread && holds_armed {
            crate::syscalls::timerfd::refuse(format_args!(
                "a message whose SCM_RIGHTS control data Detcore cannot read may pass a timerfd to another process"
            ));
        }
        for fd in sent {
            match guest.thread_state().with_detfd(fd, |detfd| detfd.ty()) {
                Ok(FdType::Timerfd) => crate::syscalls::timerfd::refuse(format_args!(
                    "a message passes timerfd {fd} over SCM_RIGHTS, and its receiver keeps no model of the timer"
                )),
                Ok(FdType::Epoll) if holds_armed => crate::syscalls::timerfd::refuse(format_args!(
                    "a message passes epoll descriptor {fd} over SCM_RIGHTS while this process holds a timerfd its set may include"
                )),
                _ => {}
            }
        }
    }
}

/// What the messages at `headers`, which a send completed in the caller's
/// turn reports sending, passed over `SCM_RIGHTS`: for each that carried
/// descriptors, those descriptors and the bytes `sent_bytes` says it sent
/// (`Detcore::queue_sent_rights`). After such a send no other guest thread
/// has run, so its header and control data are the ones the send read. A
/// message whose header or control data cannot be read in full marks the
/// whole as `unread`.
/// Withdraws `DetFd::set_guest_created` from each of `sent`, descriptors a
/// send passes out of the guest.
fn clear_guest_created<T, G>(guest: &G, sent: &[RawFd])
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    for &fd in sent {
        let _ = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.clear_guest_created());
    }
}

/// Whether a `msg_len` that a `sendmmsg` wrote back to one of the `sent`
/// entries may lie over a byte of the control buffer of one of the entries
/// at `read_back`, whose control data is read after the call. Linux writes
/// each sent entry's `msg_len` after sending its message (`__sys_sendmmsg`),
/// so control data read back after the call is then not necessarily what
/// was sent: a `msg_len` over a `cmsg_level` hides an `SCM_RIGHTS` message.
/// An entry whose header cannot be read counts as one whose control buffer
/// a `msg_len` may cover.
fn msg_len_may_overwrite_control<T, G>(guest: &G, sent: &[usize], read_back: &[usize]) -> bool
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let mut controls = Vec::with_capacity(read_back.len());
    for &header in read_back {
        let Some(message) = Addr::<Msghdr>::from_raw(header)
            .and_then(|header| guest.memory().read_value::<_, Msghdr>(header).ok())
        else {
            return !sent.is_empty();
        };
        controls.push((message.msg_control as usize, message.msg_controllen));
    }
    let lengths: Vec<usize> = sent
        .iter()
        .map(|header| header + std::mem::offset_of!(Mmsghdr, msg_len))
        .collect();
    msg_lengths_meet_controls(&lengths, &controls)
}

/// Whether the `sendmmsg` entry at `header`, which the call did not count,
/// may have carried descriptors. Linux sends an entry before it writes the
/// entry's `msg_len` back, so an entry it did not count may have been sent;
/// it reads the entry's header and control data before sending
/// (`____sys_sendmsg`), so one whose header or control buffer cannot be read
/// was not. One that may have been sent carried descriptors if its control
/// data names `SCM_RIGHTS` descriptors, or is too long to read here.
fn uncounted_entry_may_carry_rights<T, G>(guest: &G, header: usize) -> bool
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let Some(message) = Addr::<Msghdr>::from_raw(header)
        .and_then(|header| guest.memory().read_value::<_, Msghdr>(header).ok())
    else {
        return false;
    };
    let Some(control) = Addr::<u8>::from_raw(message.msg_control as usize) else {
        return false;
    };
    if message.msg_controllen > MAX_CONTROL_BYTES {
        return true;
    }
    let mut bytes = vec![0; message.msg_controllen];
    guest.memory().read_exact(control, &mut bytes).is_ok()
        && !received_descriptors(&bytes).is_empty()
}

/// Whether one of the 4-byte `msg_len` fields at the addresses `lengths`
/// shares a byte with one of the control buffers `controls`, each an address
/// and a length. A buffer of no bytes shares none.
fn msg_lengths_meet_controls(lengths: &[usize], controls: &[(usize, usize)]) -> bool {
    lengths.iter().any(|&length| {
        let end = length.saturating_add(std::mem::size_of::<u32>());
        controls.iter().any(|&(control, size)| {
            size != 0 && control < end && length < control.saturating_add(size)
        })
    })
}

fn sent_rights<T, G>(
    guest: &G,
    headers: &[usize],
    sent_bytes: impl Fn(usize) -> usize,
) -> SentRights
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let mut rights = SentRights::default();
    for &header in headers {
        match sent_descriptors(guest, &[header]) {
            (_, true) => rights.unread = true,
            (sent, false) => {
                // Whether the message named its destination: a header whose
                // name length cannot be read counts as one that did.
                let addressed = Addr::<Msghdr>::from_raw(header)
                    .and_then(|header| guest.memory().read_value::<_, Msghdr>(header).ok())
                    .is_none_or(|header| header.msg_namelen != 0);
                rights.messages.push((sent, sent_bytes(header), addressed));
            }
        }
    }
    rights
}

/// Whether the first message at `headers` names its destination (a nonzero
/// `msg_namelen`); one whose header cannot be read counts as one that did
/// (`Detcore::note_peer_gone`).
fn sends_to_a_name<T, G>(guest: &G, headers: &[usize]) -> bool
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    headers.first().is_none_or(|&header| {
        Addr::<Msghdr>::from_raw(header)
            .and_then(|header| guest.memory().read_value::<_, Msghdr>(header).ok())
            .is_none_or(|header| header.msg_namelen != 0)
    })
}

/// The descriptors the messages at `headers` name in `SCM_RIGHTS`, and
/// whether a header or control buffer could not be read in full, in which case
/// the list may be incomplete (`mark_exported_listeners`).
pub(crate) fn sent_descriptors<T, G>(guest: &G, headers: &[usize]) -> (Vec<RawFd>, bool)
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let mut sent: Vec<RawFd> = Vec::new();
    let mut unread = false;
    for header in headers {
        // A null header or control buffer fails the call with EFAULT
        // before Linux reads any descriptor.
        let Some(header) = Addr::<Msghdr>::from_raw(*header) else {
            continue;
        };
        let Ok(header) = guest.memory().read_value::<_, Msghdr>(header) else {
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
    (sent, unread)
}

impl<T: RecordOrReplay> Detcore<T> {
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
                    .map(|index| vector.as_raw() + index * std::mem::size_of::<Mmsghdr>())
                    .collect()
            });
            self.mark_exported_listeners(guest, &headers);
        }
        // As for `sendmsg`: every entry Linux could send.
        let headers: Vec<usize> = call.msgvec().map_or_else(Vec::new, |vector| {
            (0..call.vlen().min(libc::UIO_MAXIOV as u32) as usize)
                .map(|index| vector.as_raw() + index * std::mem::size_of::<Mmsghdr>())
                .collect()
        });
        self.refuse_sending_virtual_timerfds(guest, &headers);
        // A placeholder sent in a batch is refused by name: which of its
        // messages the batch sent is not known (`descriptor_directory`). Each
        // attempt checks again that the batch passes none.
        let outside_turn = match self
            .inspect_send_placeholders(guest, Sysno::sendmmsg, call.sockfd(), &headers, call)
            .await
        {
            Ok((_, outside_turn)) => outside_turn,
            Err(refusal) => return refusal,
        };
        let in_turn = self.completes_in_turn(guest, call);
        if outside_turn || !sent_descriptors(guest, &headers).0.is_empty() {
            Self::note_descriptors_passed_out(guest, call.sockfd(), &headers, outside_turn);
        }
        self.note_sockets_passed_out(guest, &headers);
        let result = self.execute_nonblockable_fd_syscall(guest, call).await;
        guest.thread_state_mut().send_inspection = None;
        if outside_turn
            && Self::may_send_placeholders(guest)
            && let Some(refusal) =
                Box::pin(self.placeholder_send_after(guest, Sysno::sendmmsg, call.sockfd())).await
        {
            return refusal;
        }
        // A batch fails only where its first message did.
        if crate::syscalls::scm_rights::found_its_peer_gone(&result) {
            let addressed = sends_to_a_name(guest, &headers[..headers.len().min(1)]);
            self.note_peer_gone(guest, call.sockfd(), addressed);
        }
        // Linux sends each entry before it writes the entry's msg_len back,
        // and stops at the first entry whose send or msg_len write fails,
        // having counted the entries before it; with none counted the call
        // fails, with EFAULT where that write faulted (`__sys_sendmmsg`). So
        // the counted entries were sent, and the first one not counted may
        // have been. Any other error is the first entry's own send failing.
        let (counted, uncounted) = match &result {
            Ok(count) => {
                let counted = usize::try_from(*count).unwrap_or(0).min(headers.len());
                (counted, (counted < headers.len()).then_some(counted))
            }
            Err(Error::Errno(Errno::EFAULT)) => (0, (!headers.is_empty()).then_some(0)),
            Err(_) => (0, None),
        };
        let mut followed = Vec::new();
        if (counted != 0 || uncounted.is_some())
            && let Some(stream) = self.rights_peer_is_stream(guest, call.sockfd())
        {
            let sent_headers = &headers[..counted];
            let read_back = &headers[..uncounted.map_or(counted, |index| index + 1)];
            if in_turn && !msg_len_may_overwrite_control(guest, sent_headers, read_back) {
                if counted != 0 {
                    // Each sent entry's bytes are what Linux wrote back to
                    // its msg_len.
                    let sent = sent_rights(guest, sent_headers, |header| {
                        Addr::<u32>::from_raw(header + std::mem::offset_of!(Mmsghdr, msg_len))
                            .and_then(|length| guest.memory().read_value(length).ok())
                            .map_or(0, |length: u32| length as usize)
                    });
                    let out_of_band = call.flags() & libc::MSG_OOB != 0;
                    followed = self
                        .queue_sent_rights(guest, call.sockfd(), stream, sent, out_of_band)
                        .await?;
                }
                // The first entry not counted may have been sent unseen,
                // with the descriptors it names.
                if uncounted
                    .is_some_and(|index| uncounted_entry_may_carry_rights(guest, headers[index]))
                {
                    self.lose_peer_lockstep(guest, call.sockfd());
                }
            } else {
                // Outside the turn, or where a msg_len Linux wrote back after
                // sending may lie over control data read back here, which
                // then no longer names what was sent.
                self.lose_peer_lockstep(guest, call.sockfd());
            }
        }
        let result = result?;
        if result > 0 {
            guest.thread_state().forget_flock_modes();
        }
        if result > 0 {
            // Every message the batch named at entry, sent or not (which of
            // them Linux sent changes only how many escaped), or, outside the
            // caller's turn, any descriptor of the table, but for the copies
            // the lock ledger counted in flight (`flock_model`).
            let passed = if outside_turn {
                guest.thread_state().open_fds()
            } else {
                sent_descriptors(guest, &headers).0
            };
            self.note_flock_descriptors_sent(guest, &passed, &followed)
                .await;
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

    /// Receive one message, then record the descriptors it received over
    /// `SCM_RIGHTS` (`Detcore::record_received_descriptors`) and read their
    /// locks (`Detcore::refresh_description_locks`). Both read only the
    /// control bytes Linux reports writing, at the control buffer the
    /// message named before the call (`ReceiveGeometry`).
    pub async fn handle_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvmsg,
    ) -> Result<i64, Error> {
        let discovered = self.discovers_received_descriptors(guest);
        // Where the tool discovers received descriptors, what a receive
        // delivered is needed for the locks of every descriptor delivered
        // (with virtual time), and to mark the inotify instances among them
        // (`Detcore::marks_discovered_inotify_receipts`).
        let wanted = !discovered
            || self.cfg.virtualize_time
            || self.marks_discovered_inotify_receipts(guest);
        let in_turn = self.completes_in_turn(guest, call);
        Self::begin_receive_geometry(guest, wanted, &Syscall::Recvmsg(call));
        let result = self.receive_one_message(guest, call).await;
        let geometry = guest.thread_state_mut().receive_geometry.take().flatten();
        let from = ReceivedFrom {
            socket: call.sockfd(),
            peek: call.flags() & libc::MSG_PEEK != 0,
            messages: 1,
            call: Sysno::recvmsg,
        };
        // A datagram or seqpacket receive takes its message before it copies
        // the bytes, and frees it if that copy fails.
        if let Err(Error::Errno(errno)) = &result {
            self.note_receive_error(guest, from, *errno, &Syscall::Recvmsg(call), in_turn);
        }
        let result = result?;
        let delivered = geometry
            .map(|geometry| geometry.delivered(&guest.memory(), result))
            .unwrap_or_default();
        let cloexec =
            Syscall::Recvmsg(call).into_parts().1.arg2 as i32 & libc::MSG_CMSG_CLOEXEC != 0;
        let recorded = self
            .record_received_descriptors(
                guest,
                &delivered,
                super::scm_rights::Channel::ScmRights,
                in_turn,
                cloexec,
                from,
            )
            .await?;
        self.classify_received_descriptors(guest, call.sockfd(), &recorded)
            .await;
        // Where the tool discovers received descriptors, recvmsg's refresh
        // covers every descriptor delivered, as before; elsewhere only those
        // this receipt recorded.
        let refreshed = if discovered {
            delivered.fds.as_slice()
        } else {
            recorded.as_slice()
        };
        self.refresh_received_locks(guest, refreshed, super::files::LockCall::Recvmsg)
            .await?;
        Ok(result)
    }

    /// Keep the input geometry of `call` (a `recvmsg` or `recvmmsg`) in the
    /// thread state when `wanted`: read here, at entry, and again before each
    /// attempt of the internal polling loop, so that the geometry the caller
    /// takes when the call returns is that of the attempt that completed
    /// (`ThreadState::receive_geometry`).
    fn begin_receive_geometry<G: Guest<Self>>(guest: &mut G, wanted: bool, call: &Syscall) {
        guest.thread_state_mut().receive_geometry =
            wanted.then(|| ReceiveGeometry::before(&guest.memory(), call));
    }

    /// A descriptor received over `SCM_RIGHTS` that Detcore models joins
    /// this process's table in `procfs::locks::LockLedger` with whatever OFD
    /// or flock locks its description holds, and is marked as received, so
    /// that a lock change through it invalidates the identity the sender's
    /// tables know the description by. One Detcore keeps no model of (not
    /// recorded, `Detcore::record_received_descriptors`) is left out, so its
    /// locks are hidden once no modelled table names the description (a
    /// named residual).
    ///
    /// Not under record and replay: they render no procfs files
    /// (`Detcore::renders_procfs`), so the ledger feeds nothing the guest
    /// reads, and the refresh reads each descriptor's fdinfo live, which in
    /// replay is a stand-in eventfd's wherever the recording held a file this
    /// process held nowhere else.
    pub(crate) async fn refresh_received_locks<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fds: &[RawFd],
        call: super::files::LockCall,
    ) -> Result<(), Error> {
        if self.cfg.virtualize_time && !self.cfg.recordreplay_modes {
            for &fd in fds {
                self.refresh_description_locks(guest, fd, call, None)
                    .await?;
            }
        }
        Ok(())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-901)
    /// Receive one message and replace host socket timestamps with logical time.
    async fn receive_one_message<G: Guest<Self>>(
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
        let message: Msghdr = guest.memory().read_value(message_address)?;
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
        let header = message_address.as_raw();
        // Where Linux writes this message's name and header fields, which it
        // does after its control messages, and whether it re-reads a
        // msg_namelen its data or control messages can have changed
        // (`ReceiveGeometry::control_read_back_refused`).
        let geometry = ReceiveGeometry::before(&guest.memory(), &Syscall::Recvmsg(call));
        if geometry
            .as_ref()
            .is_some_and(|geometry| geometry.control_read_back_refused(None))
        {
            let dettid = guest.thread_state().dettid;
            crate::syscalls::helpers::report_refusal_reason(
                self.cfg.panic_on_unsupported_syscalls,
                &format!("[dtid {dettid}] recvmsg: {MESSAGE_CONTROL_GEOMETRY_REFUSAL}"),
            );
            return self
                .refuse_unserviceable_operation_because(
                    guest,
                    Sysno::recvmsg,
                    Errno::EOPNOTSUPP,
                    MESSAGE_CONTROL_GEOMETRY_REFUSAL,
                )
                .await;
        }

        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        let now = self.observe_socket_receive(guest, call.sockfd()).await?;
        // Only the control messages the receive wrote, msg_controllen bytes,
        // which Linux writes last, unless a name or header field written
        // after them landed inside them.
        let written = written_control_length(guest, header);
        if geometry
            .as_ref()
            .is_none_or(|geometry| geometry.control_read_back_refused(Some(&[written])))
        {
            self.refuse_unfound_control_messages(guest, Sysno::recvmsg)
                .await?;
            return Ok(result);
        }
        let mut control = vec![0; written.min(control_len)];
        guest.memory().read_exact(control_address, &mut control)?;
        let Some(written) = canonicalize_written_timestamps(&mut control, written, now) else {
            return Ok(result);
        };
        guest
            .memory()
            .write_exact(control_address, &control[..written])?;
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
            let message: Msghdr = guest.memory().read_value(message_address)?;
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
            let mut headers: Vec<Mmsghdr> = (0..vlen as usize)
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
            let address: AddrMut<'_, Mmsghdr> = AddrMut::from_raw(base).ok_or(Errno::EFAULT)?;
            // SAFETY: as above.
            let mut headers: Vec<Mmsghdr> = (0..vlen as usize)
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

    /// Receive a message batch, then record the descriptors it received over
    /// `SCM_RIGHTS` (`Detcore::record_received_descriptors`) and read their
    /// locks, as for `recvmsg`.
    pub async fn handle_recvmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvmmsg,
    ) -> Result<i64, Error> {
        // Where the tool discovers received descriptors, what a receive
        // delivered is needed only to mark the inotify instances among them
        // (`Detcore::marks_discovered_inotify_receipts`).
        let wanted = !self.discovers_received_descriptors(guest)
            || self.marks_discovered_inotify_receipts(guest);
        let in_turn = self.completes_in_turn(guest, call);
        Self::begin_receive_geometry(guest, wanted, &Syscall::Recvmmsg(call));
        let result = self.receive_message_batch(guest, call).await;
        let geometry = guest.thread_state_mut().receive_geometry.take().flatten();
        let mut from = ReceivedFrom {
            socket: call.fd(),
            peek: call.flags() & libc::MSG_PEEK as u32 != 0,
            messages: 1,
            call: Sysno::recvmmsg,
        };
        // As for `recvmsg`: an error before any message is the first
        // message's.
        if let Err(Error::Errno(errno)) = &result {
            self.note_receive_error(guest, from, *errno, &Syscall::Recvmmsg(call), in_turn);
        }
        let result = result?;
        from.messages = usize::try_from(result).unwrap_or(0);
        let delivered = geometry
            .map(|geometry| geometry.delivered(&guest.memory(), result))
            .unwrap_or_default();
        let cloexec =
            Syscall::Recvmmsg(call).into_parts().1.arg3 as i32 & libc::MSG_CMSG_CLOEXEC != 0;
        let recorded = self
            .record_received_descriptors(
                guest,
                &delivered,
                super::scm_rights::Channel::ScmRights,
                in_turn,
                cloexec,
                from,
            )
            .await?;
        self.classify_received_descriptors(guest, call.fd(), &recorded)
            .await;
        self.note_short_batch(guest, from, call.vlen() as usize);
        // Only the descriptors this receipt recorded.
        self.refresh_received_locks(guest, &recorded, super::files::LockCall::Recvmmsg)
            .await?;
        Ok(result)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-901)
    /// Receive a message batch and replace every host socket timestamp with logical time.
    async fn receive_message_batch<G: Guest<Self>>(
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
            let mut messages: Vec<Mmsghdr> = (0..call.vlen())
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
        let entries = messages_address.as_raw();
        // Whether Detcore can find the control messages each entry receives
        // (`ReceiveGeometry::control_read_back_refused`), judged first on the
        // batch's own structures, before it receives anything.
        let geometry = ReceiveGeometry::before(&guest.memory(), &Syscall::Recvmmsg(call));
        let Some(geometry) = geometry.filter(|geometry| !geometry.control_read_back_refused(None))
        else {
            let dettid = guest.thread_state().dettid;
            crate::syscalls::helpers::report_refusal_reason(
                self.cfg.panic_on_unsupported_syscalls,
                &format!("[dtid {dettid}] recvmmsg: {BATCH_CONTROL_GEOMETRY_REFUSAL}"),
            );
            return self
                .refuse_unserviceable_operation_because(
                    guest,
                    Sysno::recvmmsg,
                    Errno::EOPNOTSUPP,
                    BATCH_CONTROL_GEOMETRY_REFUSAL,
                )
                .await;
        };

        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        let delivered = usize::try_from(result).unwrap_or(0).min(controls.len());
        if delivered == 0 {
            return Ok(result);
        }
        let now = self.observe_socket_receive(guest, call.fd()).await?;
        // Only the control messages the receive wrote to each entry, its
        // msg_controllen bytes; its msghdr begins the entry. The judgement
        // above leaves no later write of the batch able to replace them.
        let extents: Vec<usize> = controls
            .iter()
            .take(delivered)
            .enumerate()
            .map(|(index, control)| match control {
                Some(_) => {
                    written_control_length(guest, entries + index * std::mem::size_of::<Mmsghdr>())
                }
                None => 0,
            })
            .collect();
        if geometry.control_read_back_refused(Some(&extents)) {
            self.refuse_unfound_control_messages(guest, Sysno::recvmmsg)
                .await?;
            return Ok(result);
        }
        let mut timestamped = Vec::new();
        for (index, control) in controls.into_iter().take(delivered).enumerate() {
            let Some((address, capacity)) = control else {
                continue;
            };
            let written = extents[index];
            let address: AddrMut<'_, u8> = AddrMut::from_raw(address).ok_or(Errno::EFAULT)?;
            let mut bytes = vec![0; written.min(capacity)];
            guest.memory().read_exact(address, &mut bytes)?;
            if let Some(written) = canonicalize_written_timestamps(&mut bytes, written, now) {
                bytes.truncate(written);
                timestamped.push((address, bytes));
            }
        }
        for (address, bytes) in timestamped {
            guest.memory().write_exact(address, &bytes)?;
        }
        Ok(result)
    }

    /// After a receive whose control messages Detcore cannot find
    /// (`ReceiveGeometry::control_read_back_refused` with their extents): a
    /// fail-closed run ends as the named policy refusal, and a run that allows
    /// unsupported operations is warned and keeps the result with its control
    /// messages as the host wrote them, a host timestamp among them. The call
    /// has already received, so the errno a permissive run would get is never
    /// given.
    async fn refuse_unfound_control_messages<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
    ) -> Result<(), Error> {
        if self.cfg.panic_on_unsupported_syscalls {
            // Named on stderr before the policy ends the run, as for an
            // SCM_RIGHTS receipt (`Detcore::record_received_descriptors`).
            let dettid = guest.thread_state().dettid;
            crate::syscalls::helpers::report_refusal_reason(
                true,
                &format!("[dtid {dettid}] {sysno}: {OWN_HEADER_CONTROL_REFUSAL}"),
            );
            self.refuse_unserviceable_operation_because(
                guest,
                sysno,
                Errno::EOPNOTSUPP,
                OWN_HEADER_CONTROL_REFUSAL,
            )
            .await?;
        }
        tracing::warn!("[detcore] {sysno}: {OWN_HEADER_CONTROL_REFUSAL}");
        Ok(())
    }
}

/// Read socket option `optname` (`SOL_SOCKET`) of guest socket `fd` through an
/// injected `getsockopt`, read-only. `None` when the injection fails or Linux
/// writes less than a whole `T`.
async fn socket_option<T, D, G>(guest: &mut G, fd: RawFd, optname: libc::c_int) -> Option<T>
where
    T: Copy,
    D: reverie::Tool,
    G: Guest<D>,
{
    let mut stack = guest.stack().await;
    let value: AddrMut<T> = stack.reserve();
    let length = stack.push(std::mem::size_of::<T>() as libc::socklen_t);
    let _guard = stack.commit().ok()?;
    let length = AddrMut::<libc::socklen_t>::from_raw(length.as_raw())?;
    let call = syscalls::Getsockopt::new()
        .with_fd(fd)
        .with_level(libc::SOL_SOCKET)
        .with_optname(optname)
        .with_optval(Some(value.cast()))
        .with_optlen(Some(length));
    guest.inject(call).await.ok()?;
    let written = guest.memory().read_value(length).ok()?;
    if (written as usize) < std::mem::size_of::<T>() {
        return None;
    }
    guest.memory().read_value(value).ok()
}

/// Whether `address`, a `struct sockaddr_un` as the guest passed it to
/// `connect`, names one of init's property service sockets:
/// `property_service`, or `property_service_for_system`, which system
/// processes use.
fn names_property_service(address: &[u8]) -> bool {
    const PATHS: [&[u8]; 2] = [
        b"/dev/socket/property_service",
        b"/dev/socket/property_service_for_system",
    ];
    let family = std::mem::size_of::<libc::sa_family_t>();
    if address.len() < family
        || libc::sa_family_t::from_ne_bytes([address[0], address[1]]) != libc::AF_UNIX as u16
    {
        return false;
    }
    let path = &address[family..];
    let path = path.split(|byte| *byte == 0).next().unwrap_or(path);
    PATHS.contains(&path)
}

/// Says once per run that the guest's own property sets are not visible to
/// it: under `Config::property_snapshot` it reads a copy of the property area
/// taken before it started, while a set goes through init's property service
/// to the live area.
fn warn_property_set_not_visible() {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            "[detcore] HERMIT_PROPERTY_SET_NOT_VISIBLE: the guest connected to init's property \
             service. This run reads a frozen copy of the system property area taken before \
             the guest started, so a property it sets changes the live area and is not visible \
             to it, and a wait for a property change does not end"
        );
    }
}

#[cfg(test)]
mod tests {
    /// The coord7 review of https://github.com/rrnewton/hermit/pull/5221
    /// (finding 5): each turn of a masked ppoll's wait files the call's
    /// temporary mask as Linux installs it for the whole call
    /// (`set_user_sigmask`, kernel/signal.c:3274 at v6.17): `SIGKILL` and
    /// `SIGSTOP` dropped (`set_current_blocked`), and Reverie's preemption
    /// marker too, as the probe drops it. With the EINTR marker and the call's
    /// name, it reads as a `pselect6`-like wait whose probe takes a signal.
    #[test]
    fn a_masked_ppoll_files_its_mask_for_the_scheduler() {
        use crate::types::DetTid;
        let bit = |signal: i32| 1u64 << (signal - 1);
        let tid = DetTid::from_raw(7);
        for (label, mask, filed) in [
            ("empty", 0, 0),
            ("SIGALRM", bit(libc::SIGALRM), bit(libc::SIGALRM)),
            (
                "unblockable and the marker",
                bit(libc::SIGALRM)
                    | bit(libc::SIGKILL)
                    | bit(libc::SIGSTOP)
                    | bit(reverie::PERF_EVENT_SIGNAL as i32),
                bit(libc::SIGALRM),
            ),
        ] {
            let request = super::masked_ppoll_wait_request(tid, mask);
            assert_eq!(request.blocked_signal_mask, Some(filed), "{label}");
            assert!(!request.blocked_signal_mask_unknown, "{label}");
            assert_eq!(request.fyi.as_str(), "ppoll", "{label}");
            assert_eq!(
                request.signal_interrupt_errno(),
                Some(reverie::Errno::EINTR.into_raw()),
                "{label}"
            );
            assert!(
                request
                    .resources
                    .contains_key(&crate::resources::ResourceID::InternalIOPolling)
                    && request.resources.len() == 1,
                "{label}"
            );
        }
    }

    /// Mutation M23 of the coord7 review of
    /// https://github.com/rrnewton/hermit/pull/5221 (finding 4): a `poll` with
    /// an end time keeps it for its rerun exactly where the backend restarts a
    /// woken call only through `ERESTARTSYS`, and only when a signal woke it or
    /// it ends with `ERESTARTSYS`. Linux's restart block keeps the end time
    /// (`do_restart_poll`, fs/select.c:1076-1090 at v6.17); a backend that
    /// hands the delivery a restart errno restarts through that block, and a
    /// call that ends with `EINTR` is not restarted at all.
    #[test]
    fn a_poll_keeps_its_end_time_for_a_rerun_only_through_erestartsys() {
        use reverie::Errno;
        use reverie::Error;

        use crate::syscalls::helpers::WokenWait;
        use crate::types::LogicalTime;
        let at = Some(LogicalTime::from_nanos(5_000_000));
        let ready: Result<i64, Error> = Ok(1);
        let restart: Result<i64, Error> = Err(Error::Errno(Errno::ERESTARTSYS));
        let eintr: Result<i64, Error> = Err(Error::Errno(Errno::EINTR));
        for (label, restarts, deadline, woken, result, kept) in [
            ("woken, then ready", true, at, WokenWait::Poll, &ready, true),
            (
                "not woken, ERESTARTSYS",
                true,
                at,
                WokenWait::None,
                &restart,
                true,
            ),
            (
                "woken, no end time",
                true,
                None,
                WokenWait::Poll,
                &restart,
                false,
            ),
            (
                "a backend with restart codes",
                false,
                at,
                WokenWait::Poll,
                &restart,
                false,
            ),
            ("not woken, EINTR", true, at, WokenWait::None, &eintr, false),
            ("not woken, ready", true, at, WokenWait::None, &ready, false),
        ] {
            assert_eq!(
                super::keeps_poll_deadline_for_rerun(restarts, deadline, woken, result),
                kept,
                "{label}"
            );
        }
    }

    /// Only init's two property service sockets count: an exact `sun_path`, with
    /// or without its terminating NUL, under `AF_UNIX`.
    #[test]
    fn the_property_service_socket_is_recognized_by_path() {
        let address = |family: libc::sa_family_t, path: &[u8]| {
            let mut bytes = family.to_ne_bytes().to_vec();
            bytes.extend_from_slice(path);
            bytes
        };
        let unix = libc::AF_UNIX as libc::sa_family_t;
        assert!(super::names_property_service(&address(
            unix,
            b"/dev/socket/property_service\0"
        )));
        assert!(super::names_property_service(&address(
            unix,
            b"/dev/socket/property_service"
        )));
        assert!(super::names_property_service(&address(
            unix,
            b"/dev/socket/property_service_for_system\0"
        )));
        assert!(!super::names_property_service(&address(
            unix,
            b"/dev/socket/property_service2\0"
        )));
        assert!(!super::names_property_service(&address(
            unix,
            b"/dev/socket/logdw\0"
        )));
        assert!(!super::names_property_service(&address(
            libc::AF_INET as libc::sa_family_t,
            b"/dev/socket/property_service\0"
        )));
        assert!(!super::names_property_service(&[]));
    }

    use super::*;

    /// A sent entry's `msg_len` (4 bytes at offset 56 of its `mmsghdr`)
    /// meets a control buffer that shares one of its bytes, whichever entry
    /// the buffer belongs to, and no buffer that ends where it starts,
    /// starts where it ends, or holds no bytes.
    #[test]
    fn msg_len_meets_only_control_buffers_that_share_its_bytes() {
        const ENTRY: usize = 0x1000;
        let size = std::mem::size_of::<Mmsghdr>();
        let msg_len = |entry: usize| ENTRY + entry * size + std::mem::offset_of!(Mmsghdr, msg_len);
        assert_eq!(msg_len(0), ENTRY + 56);
        let first = [msg_len(0)];
        // A cmsghdr whose cmsg_level (bytes 8..12) is msg_len.
        assert!(msg_lengths_meet_controls(&first, &[(msg_len(0) - 8, 24)]));
        assert!(msg_lengths_meet_controls(&first, &[(msg_len(0) + 3, 1)]));
        assert!(!msg_lengths_meet_controls(&first, &[(msg_len(0) - 16, 16)]));
        assert!(!msg_lengths_meet_controls(&first, &[(msg_len(0) + 4, 16)]));
        assert!(!msg_lengths_meet_controls(&first, &[(msg_len(0), 0)]));
        // The second entry's buffer over the first entry's msg_len, and the
        // first entry's buffer over the second entry's.
        let both = [msg_len(0), msg_len(1)];
        assert!(msg_lengths_meet_controls(
            &both,
            &[(0x9000, 24), (msg_len(0), 4)]
        ));
        assert!(msg_lengths_meet_controls(
            &both,
            &[(msg_len(1) - 2, 4), (0x9000, 24)]
        ));
        assert!(!msg_lengths_meet_controls(
            &both,
            &[(0x9000, 24), (0x9100, 24)]
        ));
        assert!(!msg_lengths_meet_controls(&[], &[(msg_len(0), 4)]));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn only_record_replay_and_unsequentialized_runs_poll_in_the_kernel() {
        // A plain or strict run sequentializes threads without record or replay.
        assert!(!polls_in_kernel(true, false));
        assert!(polls_in_kernel(true, true));
        assert!(polls_in_kernel(false, false));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_restarted_kernel_poll_waits_only_until_its_deadline() {
        // Only a positive timeout has an end to keep, and record and replay
        // read no clock.
        let poll = |timeout| syscalls::Poll::new().with_timeout(timeout);
        assert_eq!(kernel_poll_deadline(&poll(0), false), None);
        assert_eq!(kernel_poll_deadline(&poll(-1), false), None);
        assert_eq!(
            kernel_poll_deadline(&poll(100), true),
            Some(KernelPollDeadline(None))
        );
        let before = Instant::now();
        let end = kernel_poll_deadline(&poll(100), false)
            .and_then(|deadline| deadline.0)
            .unwrap();
        assert!(end >= before + Duration::from_millis(100));
        assert!(end <= Instant::now() + Duration::from_millis(100));
        // The restart waits for the time left, rounded up to whole milliseconds.
        let now = Instant::now();
        let end = now + Duration::from_millis(100);
        assert_eq!(remaining_poll_millis(now, end), 100);
        assert_eq!(
            remaining_poll_millis(now + Duration::from_nanos(1), end),
            100
        );
        assert_eq!(
            remaining_poll_millis(now + Duration::from_millis(99), end),
            1
        );
        // At or past the end it only reports what is ready.
        assert_eq!(remaining_poll_millis(end, end), 0);
        assert_eq!(remaining_poll_millis(end + Duration::from_secs(1), end), 0);
        // An end beyond the range of poll's timeout waits the longest it can.
        assert_eq!(
            remaining_poll_millis(now, now + Duration::from_secs(10_000_000)),
            i32::MAX
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn an_external_send_revokes_marks_before_mutable_control_data_is_read() {
        use std::os::fd::AsRawFd;

        let mut guest = crate::syscalls::helpers::kernel_signal_wait_failures::fd_test_guest();
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = socket.as_raw_fd();
        guest
            .thread_state_mut()
            .add_fd(fd, nix::fcntl::OFlag::O_RDWR, FdType::Socket, None)
            .unwrap();
        // No control data at entry proves nothing about what another thread
        // can install while this send is descheduled before a later attempt.
        Detcore::note_descriptors_passed_out(&mut guest, fd, &[], false);
        assert!(guest.thread_state().passed_descriptors_out_unreported);

        // A stable guest-held channel cannot export even changed control data.
        guest.thread_state_mut().passed_descriptors_out_unreported = false;
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.set_guest_held())
            .unwrap();
        Detcore::note_descriptors_passed_out(&mut guest, fd, &[], false);
        assert!(!guest.thread_state().passed_descriptors_out_unreported);

        // Outside the turn, even a channel's descriptor identity can change
        // before the kernel imports it. Revoke before handing the call off.
        Detcore::note_descriptors_passed_out(&mut guest, fd, &[], true);
        assert!(guest.thread_state().passed_descriptors_out_unreported);
    }

    // x86_64 only: aarch64 has ppoll alone (kernel_poll_result is Poll-typed).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_kernel_poll_without_a_deadline_restarts_by_running_again() {
        // Linux's code, and a signal that stopped the call before it ran.
        let interrupted = [Errno::ERESTART_RESTARTBLOCK, Errno::ERESTARTSYS];
        for errno in interrupted {
            for timeout in [0, -1] {
                let call = syscalls::Poll::new().with_timeout(timeout);
                assert!(matches!(
                    kernel_poll_result(&call, Err(Error::Errno(errno)), false),
                    Err(Error::Errno(Errno::ERESTARTNOHAND))
                ));
            }
            // A positive timeout keeps its deadline in the kernel's restart block.
            let timed = syscalls::Poll::new().with_timeout(5);
            assert!(matches!(
                kernel_poll_result(&timed, Err(Error::Errno(errno)), false),
                Err(Error::Errno(Errno::ERESTART_RESTARTBLOCK))
            ));
        }
        // Every other result is the call's own.
        let zero = syscalls::Poll::new().with_timeout(0);
        assert!(matches!(kernel_poll_result(&zero, Ok(2), false), Ok(2)));
        assert!(matches!(
            kernel_poll_result(&zero, Err(Error::Errno(Errno::EINTR)), false),
            Err(Error::Errno(Errno::EINTR))
        ));
        assert!(matches!(
            kernel_poll_result(&zero, Err(Error::Errno(Errno::ERESTARTNOINTR)), false),
            Err(Error::Errno(Errno::ERESTARTNOINTR))
        ));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_kernel_poll_that_keeps_a_record_restarts_through_restart_syscall() {
        // Linux's code and a signal that stopped the call before it ran both
        // restart a kept record, even once its time is up (timeout 0).
        for errno in [Errno::ERESTART_RESTARTBLOCK, Errno::ERESTARTSYS] {
            for timeout in [100, 0] {
                let call = syscalls::Poll::new().with_timeout(timeout);
                assert!(matches!(
                    kernel_poll_result(&call, Err(Error::Errno(errno)), true),
                    Err(Error::Errno(Errno::ERESTART_RESTARTBLOCK))
                ));
            }
        }
        let call = syscalls::Poll::new().with_timeout(100);
        assert!(matches!(kernel_poll_result(&call, Ok(1), true), Ok(1)));
        assert!(matches!(
            kernel_poll_result(&call, Err(Error::Errno(Errno::EINTR)), true),
            Err(Error::Errno(Errno::EINTR))
        ));
        // A poll that a signal stopped before it ran gives the guest
        // ERESTARTNOHAND, so the backend reports the signal it holds; one that
        // ran keeps Linux's code.
        let restart = || Err(Error::Errno(Errno::ERESTART_RESTARTBLOCK));
        assert!(poll_stopped_before_it_ran(&Err(Error::Errno(
            Errno::ERESTARTSYS
        ))));
        for other in [Ok(0), restart(), Err(Error::Errno(Errno::EINTR))] {
            assert!(!poll_stopped_before_it_ran(&other), "{other:?}");
        }
        assert!(matches!(
            kernel_poll_restart_code(restart(), true),
            Err(Error::Errno(Errno::ERESTARTNOHAND))
        ));
        assert!(matches!(
            kernel_poll_restart_code(restart(), false),
            Err(Error::Errno(Errno::ERESTART_RESTARTBLOCK))
        ));
        for before_run in [true, false] {
            assert!(matches!(kernel_poll_restart_code(Ok(0), before_run), Ok(0)));
            assert!(matches!(
                kernel_poll_restart_code(Err(Error::Errno(Errno::EINTR)), before_run),
                Err(Error::Errno(Errno::EINTR))
            ));
        }
    }

    // x86_64 only: aarch64 has ppoll alone (print_poll is Poll-typed).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn print_poll_accepts_null_fds_without_touching_syscall_arguments() {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(std::io::sink)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for nfds in [0, 1, libc::nfds_t::MAX] {
                let call = syscalls::Poll::new()
                    .with_fds(None)
                    .with_nfds(nfds)
                    .with_timeout(1);
                assert!(std::panic::catch_unwind(|| print_poll(&call)).is_ok());
                assert_eq!(call.fds(), None);
                assert_eq!(call.nfds(), nfds);
                assert_eq!(call.timeout(), 1);
            }
        });
    }

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

    /// A ppoll probe runs again only after the backend's report that it did
    /// not run. EINTR, which a backend that runs the call itself answers with
    /// the temporary mask installed for a pending caught signal's delivery,
    /// is the guest's answer: retrying it found the signal again forever.
    #[test]
    fn ppoll_probes_run_again_only_after_a_not_run_report() {
        assert!(ppoll_probe_runs_again(Err(Errno::ERESTARTSYS)));
        for result in [
            Err(Errno::EINTR),
            Err(Errno::ERESTARTNOHAND),
            Err(Errno::ENOSYS),
            Err(Errno::EFAULT),
            Ok(0),
            Ok(1),
        ] {
            assert!(!ppoll_probe_runs_again(result), "{result:?}");
        }
    }

    /// The pending signals a temporary mask lets in, when every one of them
    /// is ignored by the shared rule: set to SIG_IGN, or ignored by default
    /// with no handler, on either queue; none when the mask lets in a caught
    /// or default-fatal one, or a default-ignored one with a handler.
    #[test]
    fn a_temporary_mask_lets_in_only_ignored_signals_or_not() {
        let bit = |signal: i32| 1_u64 << (signal - 1);
        let sets = |pending: u64, shared: u64, ignored: u64, caught: u64| {
            reverie::SignalSets::new(pending, shared, bit(libc::SIGWINCH), ignored, caught)
        };
        let winch = bit(libc::SIGWINCH);
        let chld = bit(libc::SIGCHLD);
        let usr1 = bit(libc::SIGUSR1);
        let hup = bit(libc::SIGHUP);
        let none = 0;
        for (sets, mask, expected) in [
            (sets(winch, 0, 0, 0), none, Some(winch)),
            (sets(0, chld, 0, 0), none, Some(chld)),
            (sets(winch, chld, 0, 0), none, Some(winch | chld)),
            (sets(hup, 0, hup, 0), none, Some(hup)),
            (sets(winch | usr1, 0, 0, usr1), usr1, Some(winch)),
            (sets(winch, 0, 0, 0), winch, Some(0)),
            (sets(0, 0, 0, 0), none, Some(0)),
            (sets(winch | usr1, 0, 0, usr1), none, None),
            (sets(usr1, 0, 0, 0), none, None),
            (sets(winch, 0, 0, winch), none, None),
            (sets(bit(libc::SIGTSTP), 0, 0, 0), none, None),
        ] {
            assert_eq!(
                ignored_signals_let_in(&sets, mask),
                expected,
                "{sets:?} {mask:#x}"
            );
        }
    }

    /// A masked pselect6 that the backend failed with ENOSYS is refused for a
    /// pending signal only when the mask lets in one the backend cannot act on
    /// inside the call: an ignored one, or one whose default action stops the
    /// process (SIGSTOP whatever the mask). With nothing pending, a blocked
    /// signal, or only a caught or default-fatal one let in, which would end
    /// the call instead, the backend declined the call's descriptor sets, as
    /// KVM's does for nfds 100. Before, every such call was refused for a
    /// pending signal. A backend that reports no signal state keeps that
    /// reason.
    #[test]
    fn a_masked_pselect6_refusal_names_a_signal_only_when_one_is_pending() {
        let bit = |signal: i32| 1_u64 << (signal - 1);
        let sets = |pending: u64, shared: u64, ignored: u64, caught: u64| {
            reverie::SignalSets::new(pending, shared, 0, ignored, caught)
        };
        let winch = bit(libc::SIGWINCH);
        let usr1 = bit(libc::SIGUSR1);
        let tstp = bit(libc::SIGTSTP);
        let stop = bit(libc::SIGSTOP);
        let term = bit(libc::SIGTERM);
        let none = 0;
        for (sets, mask, expected) in [
            (
                Some(sets(0, 0, 0, 0)),
                none,
                MASKED_PSELECT6_DECLINED_REASON,
            ),
            (
                Some(sets(usr1, 0, 0, usr1)),
                none,
                MASKED_PSELECT6_DECLINED_REASON,
            ),
            (
                Some(sets(0, term, 0, 0)),
                none,
                MASKED_PSELECT6_DECLINED_REASON,
            ),
            (
                Some(sets(winch, 0, 0, 0)),
                winch,
                MASKED_PSELECT6_DECLINED_REASON,
            ),
            (
                Some(sets(tstp, 0, 0, tstp)),
                none,
                MASKED_PSELECT6_DECLINED_REASON,
            ),
            (
                Some(sets(tstp, 0, tstp, 0)),
                none,
                MASKED_PSELECT6_SIGNAL_REASON,
            ),
            (
                Some(sets(winch, 0, 0, 0)),
                none,
                MASKED_PSELECT6_SIGNAL_REASON,
            ),
            (
                Some(sets(0, winch, 0, 0)),
                none,
                MASKED_PSELECT6_SIGNAL_REASON,
            ),
            (
                Some(sets(usr1 | winch, 0, 0, usr1)),
                none,
                MASKED_PSELECT6_SIGNAL_REASON,
            ),
            (
                Some(sets(tstp, 0, 0, 0)),
                none,
                MASKED_PSELECT6_SIGNAL_REASON,
            ),
            (
                Some(sets(stop, 0, 0, 0)),
                stop,
                MASKED_PSELECT6_SIGNAL_REASON,
            ),
            (None, none, MASKED_PSELECT6_SIGNAL_REASON),
        ] {
            for nfds in [0, 1, PSELECT6_INTERNAL_MAX_NFDS] {
                assert_eq!(
                    masked_pselect6_refusal_reason(sets.as_ref(), mask, nfds),
                    expected,
                    "{sets:?} {mask:#x} {nfds}"
                );
            }
        }
    }

    /// A masked pselect6 of more descriptors than one machine word holds is
    /// refused for its descriptor sets whatever is pending, a caught and an
    /// ignored signal together included: KVM's backend declines that count
    /// before it looks at any signal. Before, nfds 100 with such signals
    /// pending was refused for a signal.
    #[test]
    fn a_masked_pselect6_of_more_than_64_descriptors_is_refused_for_its_sets() {
        let bit = |signal: i32| 1_u64 << (signal - 1);
        let winch = bit(libc::SIGWINCH);
        let usr1 = bit(libc::SIGUSR1);
        let tstp = bit(libc::SIGTSTP);
        for sets in [
            Some(reverie::SignalSets::new(usr1 | winch, 0, 0, 0, usr1)),
            Some(reverie::SignalSets::new(winch, 0, 0, winch, 0)),
            Some(reverie::SignalSets::new(tstp, 0, 0, 0, 0)),
            Some(reverie::SignalSets::new(0, 0, 0, 0, 0)),
            None,
        ] {
            for nfds in [PSELECT6_INTERNAL_MAX_NFDS + 1, 100, 1024, i32::MAX] {
                assert_eq!(
                    masked_pselect6_refusal_reason(sets.as_ref(), 0, nfds),
                    MASKED_PSELECT6_DECLINED_REASON,
                    "{sets:?} {nfds}"
                );
            }
        }
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

    // x86_64 only: it tests select's timeval timeout, and aarch64 has no
    // select.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn select_timeout_folds_whole_seconds_out_of_microseconds() {
        let timeval = |tv_sec, tv_usec| libc::timeval { tv_sec, tv_usec };
        assert_eq!(
            select_timeout_duration(timeval(0, 1_500_000)),
            Ok(Duration::new(1, 500_000_000))
        );
        assert_eq!(
            select_timeout_duration(timeval(2, 3_000_001)),
            Ok(Duration::new(5, 1_000))
        );
        assert_eq!(
            select_timeout_duration(timeval(0, 999_999)),
            Ok(Duration::new(0, 999_999_000))
        );
        assert_eq!(select_timeout_duration(timeval(0, -1)), Err(Errno::EINVAL));
        assert_eq!(select_timeout_duration(timeval(-1, 0)), Err(Errno::EINVAL));
        // v7.1 refuses a negative field before normalizing, so neither mixed
        // sign is read as the 1 s it would sum to.
        assert_eq!(
            select_timeout_duration(timeval(2, -1_000_000)),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            select_timeout_duration(timeval(-1, 2_000_000)),
            Err(Errno::EINVAL)
        );
        assert_eq!(select_timeout_duration(timeval(1, -1)), Err(Errno::EINVAL));
        assert_eq!(
            select_timeout_duration(timeval(libc::time_t::MAX, 1_000_000)),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn ppoll_signal_mask_keeps_reverie_preemption_unblocked() {
        let preemption_bit = 1_u64 << ((reverie::PERF_EVENT_SIGNAL as usize) - 1);
        assert_eq!(sanitize_ppoll_signal_mask(u64::MAX), !preemption_bit);
    }

    #[test]
    fn ppoll_record_replay_waits_are_kernel_waits_and_run_mode_masked_waits_probe() {
        // Record/replay: the kernel owns the wait (issue 4042).
        assert!(ppoll_uses_kernel_wait(true, true));
        // A sequentialized run keeps the probe path.
        assert!(!ppoll_uses_kernel_wait(true, false));
        // Without sequentialization the kernel always owns it.
        assert!(ppoll_uses_kernel_wait(false, false));
        assert!(ppoll_uses_kernel_wait(false, true));
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
        let header_len = cmsg_align(std::mem::size_of::<Cmsghdr>());
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
            Cmsghdr {
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
            Cmsghdr {
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
            Cmsghdr {
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

    /// Only the control messages a receive wrote, its `msg_controllen` bytes,
    /// are canonicalized and given back to write; a timestamp message past
    /// them is the guest's own and stays as it was, as does a buffer whose
    /// receive wrote none.
    #[test]
    fn timestamps_are_canonicalized_only_in_the_control_bytes_written() {
        let header_len = cmsg_align(std::mem::size_of::<Cmsghdr>());
        let message_len = header_len + std::mem::size_of::<libc::timeval>();
        let second = cmsg_align(message_len);
        let mut control = vec![0; second + cmsg_align(message_len)];
        for offset in [0, second] {
            assert!(write_control_value(
                &mut control[offset..],
                Cmsghdr {
                    cmsg_len: message_len,
                    cmsg_level: libc::SOL_SOCKET,
                    cmsg_type: SCM_TIMESTAMP_OLD,
                }
            ));
            assert!(write_control_value(
                &mut control[offset + header_len..],
                libc::timeval {
                    tv_sec: 99,
                    tv_usec: 88,
                }
            ));
        }
        let original = control.clone();
        let now = LogicalTime::from_nanos(2_345_678_901);
        let seconds = |control: &[u8], offset: usize| {
            read_control_value::<libc::timeval>(&control[offset + header_len..])
                .unwrap()
                .tv_sec
        };

        // None written: nothing is the receive's.
        assert_eq!(canonicalize_written_timestamps(&mut control, 0, now), None);
        assert_eq!(control, original);
        // The first message written: the second, past msg_controllen, stays.
        assert_eq!(
            canonicalize_written_timestamps(&mut control, second, now),
            Some(second)
        );
        assert_eq!(seconds(&control, 0), 2);
        assert_eq!(seconds(&control, second), 99);
        assert_eq!(control[second..], original[second..]);
        // Both written, and a msg_controllen past the buffer is held to it.
        let mut control = original.clone();
        assert_eq!(
            canonicalize_written_timestamps(&mut control, usize::MAX, now),
            Some(original.len())
        );
        assert_eq!((seconds(&control, 0), seconds(&control, second)), (2, 2));
    }

    #[test]
    fn truncated_timestamp_payload_prefix_is_rewritten() {
        let header_len = cmsg_align(std::mem::size_of::<Cmsghdr>());
        let full_len = header_len + std::mem::size_of::<libc::timeval>();
        let mut control = vec![0xaa; header_len + std::mem::size_of::<i32>()];
        assert!(write_control_value(
            &mut control,
            Cmsghdr {
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
        let header_len = cmsg_align(std::mem::size_of::<Cmsghdr>());
        let timespec_len = std::mem::size_of::<libc::timespec>();
        let message_len = header_len + 3 * timespec_len;
        let mut control = vec![0; cmsg_align(message_len)];
        assert!(write_control_value(
            &mut control,
            Cmsghdr {
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
