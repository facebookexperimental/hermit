/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1c: the waiting thread's own side of the parked-wait checks
//! under the in-guest delivery gate (`Config::in_guest_signal_delivery`),
//! and the restart checkpoint they feed (design 16.11, D40, D50; IR-2).
//!
//! The gate is forced on in the Detcore `Config` with the in-guest backend's
//! capabilities. The test thread plays the guest thread: its `/proc` reads
//! are scripted (`signal_state_read_seam`), the backend's record of the
//! guest's mask (`Guest::resume_signal_mask`) is a value the test chooses,
//! and its scheduler RPCs are answered by the harness, which logs them. In
//! every scripted state the kernel's mask is the in-guest runtime's, all
//! blocked, unless a row says otherwise, so a reader that took `SigBlk`
//! instead of the published mask would find nothing deliverable.

use std::collections::VecDeque;
use std::sync::Mutex;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use super::*;
use crate::ThreadState;
use crate::syscalls::KernelSignalState;
use crate::syscalls::signal_state_read_seam;
use crate::wait_signal_end::WaitSignalEnd;

/// The guest thread, as the scheduler and `/proc` number it.
const THREAD: DetTid = DetTid::from_raw(31);

/// Another guest process's thread, its process's leader.
const PEER: DetTid = DetTid::from_raw(41);

fn bit(signal: i32) -> u64 {
    1_u64 << (signal - 1)
}

/// A scripted `/proc` state: `pending` in the thread's queue, the kernel mask
/// the in-guest runtime's (all blocked), `caught` and `ignored`.
fn proc_state(pending: u64, caught: u64, ignored: u64) -> KernelSignalState {
    KernelSignalState {
        pending,
        thread_pending: pending,
        shared_pending: 0,
        blocked: !0,
        caught,
        ignored,
    }
}

/// Answer this thread's `/proc` reads of `THREAD` with `state`.
fn script(state: KernelSignalState) -> signal_state_read_seam::Installed {
    signal_state_read_seam::install(move |pid, tid| {
        (pid.as_raw() == THREAD.as_raw() && tid.as_raw() == THREAD.as_raw()).then_some(Ok(state))
    })
}

/// Answer this thread's `/proc` reads of `THREAD` with `own` and of `PEER`
/// with `peer` (an error: an unreadable status).
fn script_both(
    own: KernelSignalState,
    peer: Result<KernelSignalState, Errno>,
) -> signal_state_read_seam::Installed {
    signal_state_read_seam::install(move |pid, tid| {
        if pid.as_raw() == THREAD.as_raw() && tid.as_raw() == THREAD.as_raw() {
            Some(Ok(own))
        } else if tid.as_raw() == PEER.as_raw() {
            Some(peer)
        } else {
            None
        }
    })
}

/// A scratch stack over a leaked buffer of this process, so that injected
/// queries the harness answers can write where Detcore reads. A `tight` one
/// stands for a guest stack with no writable scratch below its red zone: its
/// commit faults (`EFAULT`), and with the zeroed registers the red zone has
/// no window either (`stage_private_cells` finds no room).
struct ScratchStack {
    base: usize,
    used: usize,
    tight: bool,
}

struct ScratchGuard;
impl Drop for ScratchGuard {
    fn drop(&mut self) {}
}

impl Stack for ScratchStack {
    type StackGuard = ScratchGuard;
    fn size(&self) -> usize {
        self.used
    }
    fn capacity(&self) -> usize {
        4096
    }
    fn push<'a, T>(&mut self, value: T) -> Addr<'a, T> {
        let at = self.reserve::<T>();
        // SAFETY: `reserve` handed out an aligned, unused slot of the buffer.
        unsafe { std::ptr::write(at.as_raw() as *mut T, value) };
        Addr::from_raw(at.as_raw()).unwrap()
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        let align = std::mem::align_of::<T>().max(8);
        let start = (self.base + self.used).next_multiple_of(align);
        self.used = start + std::mem::size_of::<T>() - self.base;
        assert!(self.used <= 4096, "the scratch stack is full");
        // SAFETY: the slot lies inside the leaked buffer.
        unsafe { std::ptr::write_bytes(start as *mut u8, 0, std::mem::size_of::<T>()) };
        AddrMut::from_raw(start).unwrap()
    }
    fn commit(self) -> Result<ScratchGuard, Errno> {
        if self.tight {
            return Err(Errno::EFAULT);
        }
        Ok(ScratchGuard)
    }
}

/// Guest memory over this process's own (`LocalMemory`) that logs the start
/// of every read Detcore makes through `Guest::memory`, in order
/// (`TurnGuest::memory_reads`), so a test can tell that a call read none of
/// its arguments (round-2 ruling 3).
struct RecordingMemory {
    reads: std::sync::Arc<Mutex<Vec<usize>>>,
}

impl MemoryAccess for RecordingMemory {
    fn read_vectored(
        &self,
        read_from: &[std::io::IoSlice],
        write_to: &mut [std::io::IoSliceMut],
    ) -> Result<usize, Errno> {
        self.reads
            .lock()
            .unwrap()
            .extend(read_from.iter().map(|slice| slice.as_ptr() as usize));
        LocalMemory::new().read_vectored(read_from, write_to)
    }
    fn write_vectored(
        &mut self,
        read_from: &[std::io::IoSlice],
        write_to: &mut [std::io::IoSliceMut],
    ) -> Result<usize, Errno> {
        LocalMemory::new().write_vectored(read_from, write_to)
    }
    fn read<'a, A>(&self, addr: A, buf: &mut [u8]) -> Result<usize, Errno>
    where
        A: Into<Addr<'a, u8>>,
    {
        let addr = addr.into();
        self.reads.lock().unwrap().push(addr.as_raw());
        LocalMemory::new().read(addr, buf)
    }
    fn write(&mut self, addr: AddrMut<u8>, buf: &[u8]) -> Result<usize, Errno> {
        LocalMemory::new().write(addr, buf)
    }
    fn write_with_user_access(&mut self, addr: AddrMut<u8>, buf: &[u8]) -> Result<usize, Errno> {
        LocalMemory::new().write_with_user_access(addr, buf)
    }
}

/// What the harness answers a `RequestResources` with, in order; a request
/// with nothing left is answered `Normal`.
type Answers = VecDeque<ResumeStatus>;

/// A guest thread whose scheduler RPCs the harness answers and logs.
struct TurnGuest {
    config: Config,
    thread: ThreadState<()>,
    /// The guest's own mask as the backend records it.
    own_mask: Option<u64>,
    /// Every RPC, in order.
    rpcs: Mutex<Vec<GlobalRequest>>,
    answers: Mutex<Answers>,
    /// What a `FutexAction::WaitRequest` is answered with.
    futex_answer: Option<SchedValue>,
    /// `sa_flags` an injected `rt_sigaction` query reports.
    action_flags: u64,
    /// `sa_mask` an injected `rt_sigaction` query reports, and whether the
    /// query fails instead.
    action_mask: u64,
    action_query_fails: bool,
    /// Every injected syscall, in order.
    injected: Vec<Sysno>,
    /// What each injected `read` returns, in order (`EAGAIN` once empty).
    read_results: VecDeque<Result<i64, Errno>>,
    /// What each injected `write` or `writev` returns, in order (`EAGAIN`
    /// once empty).
    write_results: VecDeque<Result<i64, Errno>>,
    /// What `GlobalRequest::KernelRunTarget` is answered with (D44): for
    /// any thread, or only for `kernel_run_for` when that is set.
    kernel_run: Option<crate::scheduler::KernelRunBlock>,
    kernel_run_for: Option<DetTid>,
    /// A nested PID namespace's ids and the tasks they name: what
    /// `GlobalRequest::GuestPid` answers (`NoSuchPid` for any other id).
    nested: Vec<(i32, DetTid)>,
    /// The threads Detcore's model shows live, each its own process: what
    /// `GlobalRequest::ResolveKillTargets` answers.
    live: Vec<DetTid>,
    /// The host thread id `Guest::tid` reports instead of `THREAD`, for a
    /// path that reads the caller's own `/proc/<tid>` (`None`: `THREAD`); the
    /// thread's `dettid` is set to it too, as a guest thread's always is.
    host_tid: Option<i32>,
    stack: usize,
    /// Whether the scratch stack is tight (`ScratchStack::tight`).
    tight_stack: bool,
    /// The start of every guest-memory read (`RecordingMemory`), in order.
    memory_reads: std::sync::Arc<Mutex<Vec<usize>>>,
    /// The set of each injected `rt_sigtimedwait` (A5 step 3c.1d's removal of
    /// ignored signals, D37), in order.
    sigtimedwait_sets: Vec<u64>,
    /// What each injected `rt_sigtimedwait` returns, in order (`EAGAIN` once
    /// empty: nothing left to dequeue).
    sigtimedwait_results: VecDeque<Result<i64, Errno>>,
    /// For each injected `rt_sigtimedwait`, how many `RequestResources` RPCs
    /// had been filed before it (whether a removal ran in the call's own
    /// turn, before any request).
    sigtimedwait_after_requests: Vec<usize>,
    /// What each injected poll probe (`pselect6`, `select`, `ppoll`,
    /// `epoll_pwait`) returns, in order (0 once empty: nothing ready).
    probe_results: VecDeque<Result<i64, Errno>>,
    /// The temporary mask (the inner pointer of a `pselect6` wrapper, or the
    /// mask pointer of a `ppoll` or `epoll_pwait`) each injected probe
    /// carried, `None` for none, in order (IR-9).
    probe_masks: Vec<Option<usize>>,
    /// How many poll probes have been injected, shared with a `/proc` script
    /// that answers by it (`temporary_mask_turn_tests::script_by_probe`).
    probe_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// The scheduler's clock, in nanoseconds, which every
    /// `GlobalTimeLowerBound` answers and then advances by `clock_step`.
    clock: Mutex<u64>,
    clock_step: u64,
    /// How far each `RequestResources` advances that clock (0: not at all),
    /// as other threads' turns would (round-3 ruling 6's write-back row).
    clock_step_on_request: u64,
    /// How many of the next `GlobalTimeLowerBound` RPCs are answered as a
    /// hand-off (`GlobalResponse::Granted`), as a scheduler that let another
    /// thread run would (round-3 ruling 1), and how many such hand-offs have
    /// happened, shared with a `/proc` script that answers by it
    /// (`temporary_mask_turn_tests::script_by_handoff`).
    handoffs_left: std::sync::atomic::AtomicUsize,
    handoffs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// The register record `Guest::regs` reports: zeroed but for `rsp`, 0
    /// unless a test places a red zone (`stage_private_cells`).
    rsp: u64,
    /// The timeout pointer of each injected `pselect6` probe, in order.
    probe_timeouts: Vec<usize>,
    /// What a withdrawal of a temporary-mask publication answers (A5 step
    /// 3c.1e A): the committed-send window record.
    send_window: crate::scheduler::guest_mask_publication::CommittedSendWindow,
    /// What `GlobalRequest::InPidNamespaceInit` answers (IR-15).
    pid_namespace_init: bool,
    /// What `GlobalRequest::PublishedGuestSigmask` answers, under the gate.
    status_mask: crate::scheduler::guest_mask_publication::StatusGuestMask,
    /// Each injected `rt_tgsigqueueinfo` (signal, siginfo bytes), in order.
    requeued: Vec<(i32, [u8; 128])>,
    /// For each injected `rt_sigtimedwait`, how many RPCs of any kind had
    /// been sent before it.
    sigtimedwait_after_rpcs: Vec<usize>,
    /// How many `rt_sigtimedwait` calls have been injected, shared with a
    /// `/proc` script that answers by it.
    dequeues: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl TurnGuest {
    fn rpcs(&self) -> Vec<GlobalRequest> {
        self.rpcs.lock().unwrap().clone()
    }

    /// The start of every guest-memory read so far (`RecordingMemory`).
    fn memory_reads(&self) -> Vec<usize> {
        self.memory_reads.lock().unwrap().clone()
    }

    fn requests(&self) -> Vec<Resources> {
        self.rpcs()
            .into_iter()
            .filter_map(|rpc| match rpc {
                GlobalRequest::RequestResources(resources, _) => Some(resources),
                _ => None,
            })
            .collect()
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for TurnGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.rpcs.lock().unwrap().push(message.2.clone());
        let response = match message.2 {
            GlobalRequest::RequestResources(..) => {
                *self.clock.lock().unwrap() += self.clock_step_on_request;
                GlobalResponse::RequestResources(
                    self.answers
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(ResumeStatus::Normal),
                )
            }
            GlobalRequest::ReleaseAllResources => GlobalResponse::ReleaseAllResources(()),
            GlobalRequest::SetSavedGuestSigmask(_) => GlobalResponse::SetSavedGuestSigmask(()),
            GlobalRequest::FutexAction(_, FutexAction::WaitRequest(..), ..) => {
                GlobalResponse::FutexAction(self.futex_answer.clone())
            }
            GlobalRequest::FutexAction(..) => GlobalResponse::FutexAction(None),
            GlobalRequest::StoppingRefusal(_) => GlobalResponse::PolicyRefusal(()),
            GlobalRequest::KernelRunTarget(target) => GlobalResponse::KernelRunTarget(
                self.kernel_run
                    .filter(|_| self.kernel_run_for.is_none_or(|only| only == target)),
            ),
            GlobalRequest::GuestPid(_, pid) => {
                GlobalResponse::GuestPid(match self.nested.iter().find(|(id, _)| *id == pid) {
                    Some((_, task)) => crate::scheduler::pid_namespaces::GuestPidAnswer::Task {
                        task: *task,
                        own_init_caught: None,
                    },
                    None => crate::scheduler::pid_namespaces::GuestPidAnswer::NoSuchPid,
                })
            }
            GlobalRequest::RegisteredProcessOf(thread) => GlobalResponse::RegisteredProcessOf(
                self.live
                    .contains(&thread)
                    .then(|| DetPid::from_raw(thread.as_raw())),
            ),
            GlobalRequest::NamesHermitTask(..) => GlobalResponse::NamesHermitTask(false),
            GlobalRequest::TouchFile(_) => GlobalResponse::TouchFile(()),
            // Signal phase 1's ledger: no entry is due, nothing is refused.
            GlobalRequest::Sigalrm(_) => GlobalResponse::Sigalrm(false),
            GlobalRequest::GlobalTimeLowerBound => {
                let mut clock = self.clock.lock().unwrap();
                let now = *clock;
                *clock += self.clock_step;
                let answer = GlobalResponse::GlobalTimeLowerBound(
                    crate::types::LogicalTime::from_nanos(now),
                );
                let handoff = self
                    .handoffs_left
                    .try_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |left| left.checked_sub(1),
                    )
                    .is_ok();
                if handoff {
                    self.handoffs
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    GlobalResponse::Granted(Box::new(answer))
                } else {
                    answer
                }
            }
            GlobalRequest::ResolveKillTargets(process) => {
                let target = DetTid::from_raw(process.as_raw());
                let live = self.live.contains(&target);
                GlobalResponse::ResolveKillTargets(
                    if live { vec![target] } else { Vec::new() },
                    Some(false),
                )
            }
            GlobalRequest::PublishTemporaryMask(control) => {
                GlobalResponse::PublishTemporaryMask(match control {
                    crate::scheduler::guest_mask_publication::TemporaryMaskControl::Publish {
                        ..
                    } => None,
                    crate::scheduler::guest_mask_publication::TemporaryMaskControl::Withdraw {
                        ..
                    } => Some(self.send_window),
                })
            }
            GlobalRequest::InPidNamespaceInit(_) => {
                GlobalResponse::InPidNamespaceInit(self.pid_namespace_init)
            }
            GlobalRequest::PublishedGuestSigmask(_) => {
                GlobalResponse::GuestSigmaskForStatus(self.status_mask)
            }
            request => panic!("an unexpected RPC: {request:?}"),
        };
        (None, response)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for TurnGuest {
    type Memory = RecordingMemory;
    type Stack = ScratchStack;
    fn pid(&self) -> Pid {
        Pid::from_raw(THREAD.as_raw())
    }
    fn tid(&self) -> Pid {
        Pid::from_raw(self.host_tid.unwrap_or(THREAD.as_raw()))
    }
    fn ppid(&self) -> Option<Pid> {
        None
    }
    fn memory(&self) -> RecordingMemory {
        RecordingMemory {
            reads: self.memory_reads.clone(),
        }
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    fn resume_signal_mask(&self) -> Option<u64> {
        self.own_mask
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        // SAFETY: a plain register record.
        let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        regs.rsp = self.rsp;
        regs
    }
    async fn stack(&mut self) -> ScratchStack {
        ScratchStack {
            base: self.stack,
            used: 0,
            tight: self.tight_stack,
        }
    }
    async fn daemonize(&mut self) {
        panic!("a wait must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        self.injected.push(call.number());
        let (number, args) = call.into_parts();
        match Syscall::from_raw(number, args) {
            Syscall::Read(_) => self.read_results.pop_front().unwrap_or(Err(Errno::EAGAIN)),
            Syscall::Write(_) | Syscall::Writev(_) => {
                self.write_results.pop_front().unwrap_or(Err(Errno::EAGAIN))
            }
            // A send the harness lets through, failing as the kernel would
            // for an invalid signal number.
            Syscall::Tgkill(send) => valid_signal_sent(send.sig()),
            Syscall::Tkill(send) => valid_signal_sent(send.sig()),
            Syscall::RtTgsigqueueinfo(send) => {
                // Recorded where its siginfo is readable; the answer is the
                // harness's as before.
                if let Some(Ok(bytes)) = send
                    .siginfo()
                    .map(|info| LocalMemory::new().read_value::<_, [u8; 128]>(info.cast()))
                {
                    self.requeued.push((send.sig(), bytes));
                }
                valid_signal_sent(send.sig())
            }
            Syscall::RtSigqueueinfo(send) => valid_signal_sent(send.sig()),
            // The kernel-access probe of an action (`read_kernel_sigaction`):
            // SIGKILL's action cannot change, so the kernel refuses it once it
            // has copied the action.
            Syscall::RtSigaction(probe) if probe.signum() == libc::SIGKILL => {
                match probe.action() {
                    Some(action) => LocalMemory::new()
                        .read_value::<_, [u64; 4]>(action.cast())
                        .map_or(Err(Errno::EFAULT), |_| Err(Errno::EINVAL)),
                    None => Ok(0),
                }
            }
            Syscall::RtSigaction(query) => {
                if self.action_query_fails {
                    return Err(Errno::EFAULT);
                }
                if let Some(old) = query.old_action() {
                    let mut action = [0_u64; 4];
                    action[1] = self.action_flags;
                    action[3] = self.action_mask;
                    LocalMemory::new().write_value(old.cast::<[u64; 4]>(), &action)?;
                }
                Ok(0)
            }
            // The kernel-access probe of a mask (`read_kernel_sigset`): an
            // unknown `how`, refused once the set is copied.
            Syscall::RtSigprocmask(probe) if probe.how() == -1 => match probe.set() {
                Some(set) => LocalMemory::new()
                    .read_value::<_, u64>(set.cast())
                    .map_or(Err(Errno::EFAULT), |_| Err(Errno::EINVAL)),
                None => Err(Errno::EINVAL),
            },
            Syscall::RtSigtimedwait(wait) => {
                let set = wait
                    .set()
                    .map(|set| LocalMemory::new().read_value::<_, u64>(set.cast()))
                    .transpose()?
                    .unwrap_or(0);
                self.sigtimedwait_sets.push(set);
                let requests = self.requests().len();
                self.sigtimedwait_after_requests.push(requests);
                let rpcs = self.rpcs.lock().unwrap().len();
                self.sigtimedwait_after_rpcs.push(rpcs);
                self.dequeues
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.sigtimedwait_results
                    .pop_front()
                    .unwrap_or(Err(Errno::EAGAIN))
            }
            // Each poll probe as the kernel answers it: the scripted result,
            // and the copy-out the kernel makes then (a select family call
            // writes its sets, emptied for nothing ready, `set_fd_set`; a
            // poll writes every `revents`, `do_sys_poll`), which a read-only
            // buffer fails with EFAULT.
            // The wrapper's validation (`validate_pselect6_wrapper`): a
            // malformed timeout, refused once the wrapper is copied.
            Syscall::Pselect6(validation)
                if validation.timeout().is_some_and(|timeout| {
                    LocalMemory::new()
                        .read_value::<_, [i64; 2]>(timeout.cast())
                        .is_ok_and(|timeout| timeout[1] >= 1_000_000_000)
                }) =>
            {
                match validation.sigmask() {
                    Some(wrapper) => LocalMemory::new()
                        .read_value::<_, [usize; 2]>(wrapper.cast())
                        .map_or(Err(Errno::EFAULT), |_| Err(Errno::EINVAL)),
                    None => Err(Errno::EINVAL),
                }
            }
            Syscall::Pselect6(probe) => {
                self.probe_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mask = probe
                    .sigmask()
                    .map(|wrapper| LocalMemory::new().read_value::<_, usize>(wrapper.cast()))
                    .transpose()?
                    .filter(|inner| *inner != 0);
                self.probe_masks.push(mask);
                self.probe_timeouts
                    .push(probe.timeout().map_or(0, |timeout| timeout.as_raw()));
                let result = self.probe_results.pop_front().unwrap_or(Ok(0));
                // A count copies the sets out too (`set_fd_set` runs for any
                // result that is not an error): the harness writes back the
                // bytes in place, every requested descriptor taken as ready,
                // so a read-only set fails with EFAULT as on Linux.
                if let Ok(count) = result {
                    let len = (probe.nfds().max(0) as usize).div_ceil(64) * 8;
                    for set in [probe.readfds(), probe.writefds(), probe.exceptfds()]
                        .into_iter()
                        .flatten()
                    {
                        if len != 0 {
                            let mut bytes = vec![0; len];
                            if count != 0 {
                                LocalMemory::new()
                                    .read_exact(set.cast::<u8>(), &mut bytes)
                                    .map_err(|_| Errno::EFAULT)?;
                            }
                            LocalMemory::new()
                                .write_exact(set.cast(), &bytes)
                                .map_err(|_| Errno::EFAULT)?;
                        }
                    }
                }
                result
            }
            Syscall::Select(_) => {
                self.probe_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.probe_masks.push(None);
                self.probe_results.pop_front().unwrap_or(Ok(0))
            }
            Syscall::Ppoll(probe) => {
                self.probe_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.probe_masks
                    .push(probe.sigmask().map(|mask| mask.as_raw()));
                let result = self.probe_results.pop_front().unwrap_or(Ok(0));
                if result.is_ok()
                    && let Some(fds) = probe.fds()
                {
                    for index in 0..probe.nfds() as usize {
                        let revents = fds.as_raw() + index * 8 + 6;
                        LocalMemory::new()
                            .write_exact(AddrMut::<u8>::from_raw(revents).unwrap(), &[0, 0])
                            .map_err(|_| Errno::EFAULT)?;
                    }
                }
                result
            }
            // A stage-free `poll` probe (round-2 ruling 2), answered as a
            // ppoll probe is: the scripted result, and `revents` written for a
            // count, which an unreadable array fails with EFAULT.
            Syscall::Poll(probe) => {
                self.probe_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.probe_masks.push(None);
                let result = self.probe_results.pop_front().unwrap_or(Ok(0));
                if result.is_ok()
                    && let Some(fds) = probe.fds()
                {
                    for index in 0..probe.nfds() as usize {
                        let revents = fds.as_raw() + index * 8 + 6;
                        LocalMemory::new()
                            .write_exact(AddrMut::<u8>::from_raw(revents).unwrap(), &[0, 0])
                            .map_err(|_| Errno::EFAULT)?;
                    }
                }
                result
            }
            Syscall::EpollPwait(probe) => {
                self.probe_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.probe_masks
                    .push(probe.sigmask().map(|mask| mask.as_raw()));
                self.probe_results.pop_front().unwrap_or(Ok(0))
            }
            Syscall::RtSigprocmask(query) => {
                if let Some(old) = query.oldset() {
                    LocalMemory::new().write_value(old.cast::<u64>(), &0)?;
                }
                Ok(0)
            }
            other => panic!("an unexpected injection: {other:?}"),
        }
    }
    async fn inject_with_purpose<S: SyscallInfo>(
        &mut self,
        call: S,
        _: reverie::InjectionPurpose,
    ) -> Result<i64, Error> {
        self.inject(call).await.map_err(Error::from)
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("a wait must not tail-inject")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        Ok(())
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        Ok(())
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        Ok(0)
    }
}

/// Make `guest` the test's own thread, for a test whose `tid()` must name a
/// real thread: a refused send's unreadable `siginfo` is classified from the
/// caller's `/proc/<tid>/smaps` (`refused_send_siginfo`), and the harness's
/// default `THREAD` (31) is whatever task has that id on the host or in the
/// validation container, so a null or unmapped `siginfo` gave EFAULT on one
/// host and the named refusal in the pinned root.
fn use_real_test_thread(guest: &mut TurnGuest) {
    // SAFETY: gettid has no preconditions.
    let own = unsafe { libc::gettid() };
    guest.host_tid = Some(own);
    // An ordinary RPC asserts that the backend tid is the thread's dettid
    // (`send_and_update_time`).
    guest.thread.dettid = DetTid::from_raw(own);
}

/// An in-guest thread with the delivery gate `gate`, whose backend records
/// the guest's own mask as `own_mask`.
fn turn_guest(gate: bool, own_mask: Option<u64>) -> TurnGuest {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: gate,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    let detpid = DetPid::from_raw(THREAD.as_raw());
    let mut thread = ThreadState::new(detpid, &config, ());
    thread.dettid = THREAD;
    thread.detpid = Some(detpid);
    let stack = Box::leak(Box::new([0_u64; 512])).as_mut_ptr() as usize;
    TurnGuest {
        config,
        thread,
        own_mask,
        rpcs: Mutex::new(Vec::new()),
        answers: Mutex::new(VecDeque::new()),
        futex_answer: Some(SchedValue::Value(0)),
        action_flags: 0,
        action_mask: 0,
        action_query_fails: false,
        injected: Vec::new(),
        read_results: VecDeque::new(),
        write_results: VecDeque::new(),
        kernel_run: None,
        kernel_run_for: None,
        nested: Vec::new(),
        live: vec![THREAD, PEER],
        host_tid: None,
        stack,
        tight_stack: false,
        memory_reads: Default::default(),
        sigtimedwait_sets: Vec::new(),
        sigtimedwait_results: VecDeque::new(),
        sigtimedwait_after_requests: Vec::new(),
        probe_results: VecDeque::new(),
        probe_masks: Vec::new(),
        probe_count: Default::default(),
        clock: Mutex::new(0),
        clock_step: 300_000_000,
        clock_step_on_request: 0,
        handoffs_left: Default::default(),
        handoffs: Default::default(),
        rsp: 0,
        probe_timeouts: Vec::new(),
        send_window: Default::default(),
        pid_namespace_init: false,
        status_mask: Default::default(),
        requeued: Vec::new(),
        sigtimedwait_after_rpcs: Vec::new(),
        dequeues: Default::default(),
    }
}

/// What the kernel answers a send the harness lets through: `EINVAL` for a
/// signal number `valid_signal` refuses (kernel/signal.c:836 at v6.12), 0
/// otherwise.
fn valid_signal_sent(signal: i32) -> Result<i64, Errno> {
    if (0..=64).contains(&signal) {
        Ok(0)
    } else {
        Err(Errno::EINVAL)
    }
}

fn tool(guest: &TurnGuest) -> Detcore {
    Detcore::new(Pid::from_raw(THREAD.as_raw()), &guest.config)
}

/// The rows every gated reader is checked against: the signal pending, the
/// guest's own mask, the dispositions, the end expected.
struct Row {
    name: &'static str,
    state: KernelSignalState,
    own: u64,
    end: Option<WaitSignalEnd>,
}

fn rows() -> Vec<Row> {
    let term = bit(libc::SIGTERM);
    let usr1 = bit(libc::SIGUSR1);
    vec![
        Row {
            name: "default-fatal, unblocked by the published mask",
            state: proc_state(term, 0, 0),
            own: 0,
            end: Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
        },
        Row {
            name: "caught",
            state: proc_state(usr1, usr1, 0),
            own: 0,
            end: Some(WaitSignalEnd::Caught(libc::SIGUSR1)),
        },
        Row {
            name: "blocked by the published mask, the kernel's unblocked",
            state: KernelSignalState {
                blocked: 0,
                ..proc_state(term, 0, 0)
            },
            own: term,
            end: None,
        },
        Row {
            name: "SIG_IGN",
            state: proc_state(term, 0, term),
            own: 0,
            end: None,
        },
        Row {
            name: "default-ignored",
            state: proc_state(bit(libc::SIGURG), 0, 0),
            own: 0,
            end: None,
        },
    ]
}

/// D50 reader 8, `sleep_signal`: before the park (a signal sent before the
/// thread filed its sleep) and after a wake, the sleep ends exactly for a
/// deliverable signal, and records the end for the restart checkpoint.
/// Mutation "the at-park check removed": the `BeforePark` rows that must end
/// do not.
#[tokio::test]
async fn under_the_gate_a_sleep_ends_before_its_park_and_after_a_wake_only_for_a_deliverable_signal()
 {
    use crate::syscalls::SleepCheck;
    use crate::syscalls::SleepSignal;
    for row in rows() {
        for check in [
            SleepCheck::BeforePark,
            SleepCheck::AfterWake(Some(vec![SigWrapper(libc::SIGTERM)])),
        ] {
            let _seam = script(row.state);
            let mut guest = turn_guest(true, Some(row.own));
            let verdict = crate::syscalls::sleep_signal(&mut guest, check.clone(), true)
                .await
                .unwrap();
            let expected = if row.end.is_some() {
                SleepSignal::Pending
            } else {
                SleepSignal::None
            };
            assert_eq!(verdict, expected, "{}: {check:?}", row.name);
            assert_eq!(guest.thread.wait_signal_end, row.end, "{}", row.name);
        }
    }
}

/// Signal phase 1's virtual SIGALRM wake ends a `pause` as a caught
/// delivery of SIGALRM, with no physical copy pending (IR-12), taking its
/// place on the shared queue in Linux's dequeue order: a deliverable
/// default-fatal signal that does not dump core still makes the end fatal,
/// and so does a core-dumping one dequeued first (a private one, or a shared
/// lower-numbered one); a shared core-dumping one that SIGALRM precedes is
/// `BehindCaught`, which the restart checkpoint resolves by SIGALRM's mask
/// (`under_the_gate_the_restart_checkpoint_resolves_an_end_behind_a_caught_signal`).
#[tokio::test]
async fn under_the_gate_phase_1_s_virtual_sigalrm_wake_ends_a_pause_as_caught() {
    use crate::syscalls::SleepCheck;
    use crate::syscalls::SleepSignal;
    let wake = SleepCheck::AfterWake(Some(vec![SigWrapper(libc::SIGALRM)]));
    let shared = |pending| KernelSignalState {
        thread_pending: 0,
        shared_pending: pending,
        ..proc_state(pending, 0, 0)
    };
    for (state, end) in [
        (proc_state(0, 0, 0), WaitSignalEnd::Caught(libc::SIGALRM)),
        (
            proc_state(bit(libc::SIGTERM), 0, 0),
            WaitSignalEnd::Fatal(libc::SIGTERM),
        ),
        (
            proc_state(bit(libc::SIGXCPU), 0, 0),
            WaitSignalEnd::Fatal(libc::SIGXCPU),
        ),
        (
            shared(bit(libc::SIGQUIT)),
            WaitSignalEnd::Fatal(libc::SIGQUIT),
        ),
    ] {
        let _seam = script(state);
        let mut guest = turn_guest(true, Some(0));
        guest.thread.sigalrm_handled = true;
        let verdict = crate::syscalls::sleep_signal(&mut guest, wake.clone(), true)
            .await
            .unwrap();
        assert_eq!(verdict, SleepSignal::Pending);
        assert_eq!(guest.thread.wait_signal_end, Some(end));
    }
    // A shared SIGXCPU behind the virtual SIGALRM.
    let _seam = script(shared(bit(libc::SIGXCPU)));
    let mut guest = turn_guest(true, Some(0));
    guest.thread.sigalrm_handled = true;
    let verdict = crate::syscalls::sleep_signal(&mut guest, wake.clone(), true)
        .await
        .unwrap();
    assert_eq!(verdict, SleepSignal::Pending);
    assert!(
        matches!(
            guest.thread.wait_signal_end,
            Some(WaitSignalEnd::BehindCaught(_))
        ),
        "{:?}",
        guest.thread.wait_signal_end
    );
    // Not a phase-1 process: nothing pending, nothing ends it.
    let _seam = script(proc_state(0, 0, 0));
    let mut guest = turn_guest(true, Some(0));
    let verdict = crate::syscalls::sleep_signal(&mut guest, wake, true)
        .await
        .unwrap();
    assert_eq!(verdict, SleepSignal::None);
}

/// The restart checkpoint resolves a `BehindCaught` end (Claude review round
/// 2): phase 1's virtual SIGALRM wakes a `pause` with a shared SIGXCPU
/// deliverable behind it, and the checkpoint reads SIGALRM's action with one
/// query `rt_sigaction`. (a) Its `sa_mask` holds SIGXCPU: `Caught(SIGALRM)`,
/// the restart table's `EINTR` and a plain checkpoint. (b) It does not:
/// `Fatal(SIGXCPU)`, `-EINTR` and the interrupted code. (e) The query fails:
/// fail closed, the stopping refusal HERMIT_CAUGHT_ACTION_UNREADABLE is filed
/// and the end is `Fatal`. Mutations: "the handler's sa_mask ignored" ((a)
/// gives the interrupted code), "no fail-closed" ((e) files nothing).
#[tokio::test]
async fn under_the_gate_the_restart_checkpoint_resolves_an_end_behind_a_caught_signal() {
    use crate::syscalls::SleepCheck;
    let wake = SleepCheck::AfterWake(Some(vec![SigWrapper(libc::SIGALRM)]));
    let xcpu = bit(libc::SIGXCPU);
    let state = KernelSignalState {
        thread_pending: 0,
        shared_pending: xcpu,
        ..proc_state(xcpu, 0, 0)
    };
    let pause = Syscall::from(reverie::syscalls::Pause::new());
    for (name, mask, fails, interrupted, refused) in [
        ("(a) SIGALRM's mask holds SIGXCPU", xcpu, false, None, false),
        (
            "(b) SIGALRM's mask does not hold SIGXCPU",
            0,
            false,
            Some(Errno::ERESTARTNOHAND),
            false,
        ),
        (
            "(e) the action cannot be read",
            xcpu,
            true,
            Some(Errno::ERESTARTNOHAND),
            true,
        ),
    ] {
        let _seam = script(state);
        let mut guest = turn_guest(true, Some(0));
        guest.thread.sigalrm_handled = true;
        guest.action_mask = mask;
        guest.action_query_fails = fails;
        let tool = tool(&guest);
        crate::syscalls::sleep_signal(&mut guest, wake.clone(), true)
            .await
            .unwrap();
        let (res, code) = tool
            .wait_end_checkpoint(&mut guest, &pause, Err(Errno::ERESTARTNOHAND.into()))
            .await;
        assert!(
            matches!(res, Err(Error::Errno(Errno::EINTR))),
            "{name}: {res:?}"
        );
        assert_eq!(code, interrupted, "{name}");
        assert_eq!(guest.injected, [Sysno::rt_sigaction], "{name}");
        let refusals = guest
            .rpcs()
            .iter()
            .filter(|rpc| {
                matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                    if reason.contains("HERMIT_CAUGHT_ACTION_UNREADABLE"))
            })
            .count();
        assert_eq!(refusals, usize::from(refused), "{name}");
    }
}

/// With the gate off the in-guest sleep keeps today's answers (its backend
/// does not report the guest's signal state): nothing ends it before the
/// park, and any wake ends it (`Held`), and no end is recorded.
#[tokio::test]
async fn with_the_gate_off_a_sleep_keeps_today_s_answers() {
    use crate::syscalls::SleepCheck;
    use crate::syscalls::SleepSignal;
    let _seam = script(proc_state(bit(libc::SIGTERM), 0, 0));
    let mut guest = turn_guest(false, Some(0));
    for (check, expected) in [
        (SleepCheck::BeforePark, SleepSignal::None),
        (
            SleepCheck::AfterWake(Some(vec![SigWrapper(libc::SIGTERM)])),
            SleepSignal::Held,
        ),
    ] {
        let verdict = crate::syscalls::sleep_signal(&mut guest, check, true)
            .await
            .unwrap();
        assert_eq!(verdict, expected);
    }
    assert_eq!(guest.thread.wait_signal_end, None);
}

/// D50 reader 9, `handle_futex_blocking`: a deliverable signal pending at the
/// park ends the wait with the futex's restart errno before any wait request,
/// and records the end; otherwise the wait publishes the guest's mask
/// (`SetSavedGuestSigmask`, the 3c.1b carry: a futex wait files no
/// `Resources` request) before its `FutexAction::WaitRequest`, whose watch
/// unblocks exactly what the published mask does. Mutations: "SigBlk used"
/// (the kernel mask blocks everything) and "publication after the wait
/// request".
#[tokio::test]
async fn under_the_gate_a_futex_wait_ends_at_its_park_or_publishes_its_mask_before_waiting() {
    let word: &'static mut u32 = Box::leak(Box::new(7));
    let address = word as *mut u32 as usize;
    let call = reverie::syscalls::Futex::new()
        .with_uaddr(AddrMut::from_raw(address))
        .with_futex_op(libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG)
        .with_val(7);
    for row in rows() {
        let _seam = script(row.state);
        let mut guest = turn_guest(true, Some(row.own));
        let tool = tool(&guest);
        let result = tool
            .handle_futex_blocking(
                &mut guest,
                call,
                7,
                crate::syscalls::FutexDeadline::FromCall,
            )
            .await;
        assert_eq!(guest.thread.wait_signal_end, row.end, "{}", row.name);
        let rpcs = guest.rpcs();
        if row.end.is_some() {
            assert!(
                matches!(result, Err(Error::Errno(Errno::ERESTARTSYS))),
                "{}: {result:?}",
                row.name
            );
            assert!(
                !rpcs
                    .iter()
                    .any(|rpc| matches!(rpc, GlobalRequest::FutexAction(..))),
                "{}: no wait request",
                row.name
            );
            continue;
        }
        assert_eq!(result.unwrap(), 0, "{}", row.name);
        let publication = rpcs
            .iter()
            .position(|rpc| *rpc == GlobalRequest::SetSavedGuestSigmask(Some(row.own)))
            .unwrap_or_else(|| panic!("{}: no publication in {rpcs:?}", row.name));
        let wait = rpcs
            .iter()
            .position(|rpc| {
                matches!(rpc, GlobalRequest::FutexAction(_, FutexAction::WaitRequest(_, Some(watch)), ..)
                    if watch.unblocked & bit(libc::SIGTERM) == !row.own & bit(libc::SIGTERM))
            })
            .unwrap_or_else(|| panic!("{}: no wait request in {rpcs:?}", row.name));
        assert!(
            publication < wait,
            "{}: published before the wait",
            row.name
        );
    }
}

/// With the gate off an in-guest futex wait publishes nothing and reports no
/// watch, as before.
#[tokio::test]
async fn with_the_gate_off_a_futex_wait_publishes_nothing() {
    let word: &'static mut u32 = Box::leak(Box::new(7));
    let call = reverie::syscalls::Futex::new()
        .with_uaddr(AddrMut::from_raw(word as *mut u32 as usize))
        .with_futex_op(libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG)
        .with_val(7);
    let _seam = script(proc_state(bit(libc::SIGTERM), 0, 0));
    let mut guest = turn_guest(false, Some(0));
    let tool = tool(&guest);
    let result = tool
        .handle_futex_blocking(
            &mut guest,
            call,
            7,
            crate::syscalls::FutexDeadline::FromCall,
        )
        .await;
    assert_eq!(result.unwrap(), 0);
    let rpcs = guest.rpcs();
    assert!(
        !rpcs
            .iter()
            .any(|rpc| matches!(rpc, GlobalRequest::SetSavedGuestSigmask(_)))
    );
    assert!(rpcs.iter().any(|rpc| matches!(
        rpc,
        GlobalRequest::FutexAction(_, FutexAction::WaitRequest(_, None), ..)
    )));
    assert_eq!(guest.thread.wait_signal_end, None);
}

/// D50 reader 2, the retry loop under `retry_nonblocking_syscall_with_timeout`
/// and `retry_nonblocking_syscall` (`retry_nonblocking_syscall_helper`), here
/// through the latter, as a polled read takes it: a restartable
/// read that would block ends with `ERESTARTSYS` once a deliverable signal is
/// pending, at its first turn (a signal sent before it parked), and records
/// the end; otherwise it polls on.
#[tokio::test]
async fn under_the_gate_a_restartable_poll_ends_where_it_would_block_only_for_a_deliverable_signal()
{
    for row in rows() {
        let _seam = script(row.state);
        let mut guest = turn_guest(true, Some(row.own));
        // Descriptor 3, a pipe Hermit made physically nonblocking, as the
        // internal-polling path requires.
        let pipe = crate::fd::DetFd::new(
            3,
            nix::fcntl::OFlag::O_RDONLY,
            crate::fd::FdType::Pipe,
            crate::types::OpenFileId::new(DetPid::from_raw(THREAD.as_raw()), 99),
        );
        pipe.set_physically_nonblocking();
        guest
            .thread
            .file_metadata
            .lock()
            .unwrap()
            .file_handles
            .insert(3, pipe);
        // A first attempt that would block, then data.
        guest.read_results = VecDeque::from([Err(Errno::EAGAIN), Ok(4)]);
        let mut poll = Resources::new(THREAD);
        poll.insert(ResourceID::InternalIOPolling, Permission::R);
        poll.fyi("read");
        let read = reverie::syscalls::Read::new()
            .with_fd(3)
            .with_buf(AddrMut::from_raw(guest.stack))
            .with_len(8);
        let result = crate::syscalls::helpers::retry_nonblocking_syscall::<
            crate::record_or_replay::NoopTool,
            _,
            _,
        >(&mut guest, read, poll, None)
        .await;
        assert_eq!(guest.thread.wait_signal_end, row.end, "{}", row.name);
        if row.end.is_some() {
            assert!(
                matches!(result, Err(Error::Errno(Errno::ERESTARTSYS))),
                "{}: {result:?}",
                row.name
            );
        } else {
            assert_eq!(result.unwrap(), 4, "{}: it polled on", row.name);
        }
    }
}

/// D40's checkpoint, row by row (IR-2): the guest's result, and the code the
/// posthook checkpoint records. Mutations: "interrupted dropped" (the fatal
/// row returns no code), "the restart-after-handler refusal removed" (the
/// caught ERESTARTNOINTR row returns the raw code and files nothing).
#[tokio::test]
async fn under_the_gate_the_restart_checkpoint_gives_each_row_its_result() {
    use reverie::syscalls::Pause;
    let pause = Syscall::from(Pause::new());
    let code = |errno: Errno| Err::<i64, Error>(errno.into());
    let errno_of = |result: &Result<i64, Error>| match result {
        Err(Error::Errno(errno)) => Some(*errno),
        _ => None,
    };
    // name, flag, sa_flags, result, guest errno, interrupted code, the
    // stopping refusal filed
    type Case = (
        &'static str,
        Option<WaitSignalEnd>,
        u64,
        Errno,
        Option<Errno>,
        Option<Errno>,
        Option<&'static str>,
    );
    let cases: Vec<Case> = vec![
        (
            "fatal, ERESTARTNOHAND",
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            0,
            Errno::ERESTARTNOHAND,
            Some(Errno::EINTR),
            Some(Errno::ERESTARTNOHAND),
            None,
        ),
        (
            "fatal, ERESTARTSYS",
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            0,
            Errno::ERESTARTSYS,
            Some(Errno::EINTR),
            Some(Errno::ERESTARTSYS),
            None,
        ),
        (
            "caught, ERESTARTNOHAND",
            Some(WaitSignalEnd::Caught(libc::SIGALRM)),
            0,
            Errno::ERESTARTNOHAND,
            Some(Errno::EINTR),
            None,
            None,
        ),
        (
            "caught, ERESTARTSYS without SA_RESTART",
            Some(WaitSignalEnd::Caught(libc::SIGUSR1)),
            0,
            Errno::ERESTARTSYS,
            Some(Errno::EINTR),
            None,
            None,
        ),
        (
            "caught, ERESTARTSYS with SA_RESTART: the code is kept for step 3c.2's rewind",
            Some(WaitSignalEnd::Caught(libc::SIGUSR1)),
            libc::SA_RESTART as u64,
            Errno::ERESTARTSYS,
            Some(Errno::ERESTARTSYS),
            None,
            None,
        ),
        (
            "caught, ERESTARTNOINTR: the table restarts, which 3c.1 cannot honour: refused",
            Some(WaitSignalEnd::Caught(libc::SIGUSR1)),
            0,
            Errno::ERESTARTNOINTR,
            Some(Errno::EINTR),
            None,
            Some("HERMIT_RESTART_AFTER_HANDLER"),
        ),
        (
            "no flag, ERESTARTSYS: Reverie's re-run (https://github.com/rrnewton/reverie/issues/362)",
            None,
            0,
            Errno::ERESTARTSYS,
            Some(Errno::ERESTARTSYS),
            None,
            None,
        ),
        (
            "no flag, ERESTARTNOHAND: refused",
            None,
            0,
            Errno::ERESTARTNOHAND,
            Some(Errno::ERESTARTNOHAND),
            None,
            Some("HERMIT_RESTART_CODE_WITHOUT_SIGNAL"),
        ),
        (
            "no flag, ERESTARTNOINTR: refused",
            None,
            0,
            Errno::ERESTARTNOINTR,
            Some(Errno::ERESTARTNOINTR),
            None,
            Some("HERMIT_RESTART_CODE_WITHOUT_SIGNAL"),
        ),
        (
            "no flag, ERESTART_RESTARTBLOCK: refused",
            None,
            0,
            Errno::ERESTART_RESTARTBLOCK,
            Some(Errno::ERESTART_RESTARTBLOCK),
            None,
            Some("HERMIT_RESTART_CODE_WITHOUT_SIGNAL"),
        ),
        (
            "fatal, EINTR already: unchanged",
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            0,
            Errno::EINTR,
            Some(Errno::EINTR),
            None,
            None,
        ),
    ];
    for (name, flag, sa_flags, result, guest_errno, interrupted, refused) in cases {
        let mut guest = turn_guest(true, Some(0));
        guest.action_flags = sa_flags;
        guest.thread.wait_signal_end = flag;
        let tool = tool(&guest);
        let (res, code_out) = tool
            .wait_end_checkpoint(&mut guest, &pause, code(result))
            .await;
        assert_eq!(errno_of(&res), guest_errno, "{name}");
        assert_eq!(code_out, interrupted, "{name}");
        assert_eq!(
            guest.thread.wait_signal_end, None,
            "{name}: the flag is taken"
        );
        let filed: Vec<&str> = guest
            .rpcs()
            .iter()
            .filter_map(|rpc| match rpc {
                GlobalRequest::StoppingRefusal(reason) => [
                    "HERMIT_RESTART_CODE_WITHOUT_SIGNAL",
                    "HERMIT_RESTART_AFTER_HANDLER",
                ]
                .into_iter()
                .find(|code| reason.contains(code)),
                _ => None,
            })
            .collect();
        assert_eq!(filed, Vec::from_iter(refused), "{name}");
    }
}

/// With the gate off the checkpoint changes nothing and refuses nothing,
/// whatever the result.
#[tokio::test]
async fn with_the_gate_off_the_restart_checkpoint_is_the_identity() {
    let pause = Syscall::from(reverie::syscalls::Pause::new());
    for result in [
        Errno::ERESTARTSYS,
        Errno::ERESTARTNOINTR,
        Errno::ERESTARTNOHAND,
        Errno::ERESTART_RESTARTBLOCK,
        Errno::EINTR,
    ] {
        let mut guest = turn_guest(false, None);
        let tool = tool(&guest);
        let (res, code) = tool
            .wait_end_checkpoint(&mut guest, &pause, Err(result.into()))
            .await;
        assert!(matches!(res, Err(Error::Errno(errno)) if errno == result));
        assert_eq!(code, None);
        assert!(guest.rpcs().is_empty());
    }
}

/// IR-2's observers for a `pause` a default-fatal signal ends before its park,
/// through the whole syscall pipeline (`handle_syscall_event`): the backend
/// and `delivery_interruption` see `-EINTR`, no sleep request is filed, and the
/// posthook happens-before anchor fires as interrupted with the kernel's
/// `ERESTARTNOHAND`, because the guest never resumes. Mutations: the
/// checkpoint moved to `complete_syscall` (the backend, `delivery_interruption`
/// and DETLOG would see a raw 514), and `interrupted` dropped (the anchor
/// would file a plain checkpoint).
#[tokio::test]
async fn under_the_gate_a_pause_ended_by_a_fatal_signal_shows_eintr_to_every_observer() {
    use detcore_model::happens_before::HappensBeforeSpec;
    let _seam = script(proc_state(bit(libc::SIGTERM), 0, 0));
    let mut guest = turn_guest(true, Some(0));
    guest.config.happens_before = Some(
        HappensBeforeSpec::from_json(&format!(
            r#"{{"version": 1, "events": {{"p": {{"thread": "{}", "syscall": "pause",
                                         "phase": "posthook"}}}}}}"#,
            THREAD.as_raw()
        ))
        .unwrap()
        .normalize()
        .unwrap(),
    );
    let tool = <Detcore as Tool>::new(Pid::from_raw(THREAD.as_raw()), &guest.config);
    let result = tool
        .handle_syscall_event(&mut guest, Syscall::from(reverie::syscalls::Pause::new()))
        .await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::EINTR))),
        "{result:?}"
    );
    assert_eq!(
        guest.thread.delivery_interruption,
        Some(reverie::Interruption::Return {
            syscall: libc::SYS_pause,
            result: -i64::from(libc::EINTR),
        })
    );
    let requests = guest.requests();
    assert!(
        !requests.iter().any(|request| request
            .resources
            .keys()
            .any(|rid| matches!(rid, ResourceID::SleepUntil(_)))),
        "the pause never parked: {requests:?}"
    );
    let interrupted: Vec<_> = requests
        .iter()
        .flat_map(|request| request.resources.keys().cloned())
        .filter(|rid| {
            matches!(
                rid,
                ResourceID::HappensBeforePosthookInterrupted { .. }
                    | ResourceID::HappensBeforeSyscallCheckpoint { .. }
            )
        })
        .collect();
    assert_eq!(
        interrupted,
        [ResourceID::HappensBeforePosthookInterrupted {
            count: 1,
            anchors: vec!["p".to_owned()],
            code: "ERESTARTNOHAND".to_owned(),
        }]
    );
    assert_eq!(guest.thread.wait_signal_end, None);
}

/// Registers descriptor `fd` of `guest` as a pipe Hermit made physically
/// nonblocking, with open file `id`.
fn add_pipe(guest: &mut TurnGuest, fd: i32, id: crate::types::OpenFileId) {
    let pipe = crate::fd::DetFd::new(fd, nix::fcntl::OFlag::O_WRONLY, crate::fd::FdType::Pipe, id);
    pipe.set_physically_nonblocking();
    guest
        .thread
        .file_metadata
        .lock()
        .unwrap()
        .file_handles
        .insert(fd, pipe);
}

/// D50 reader 1's waiting side, a parked pipe write, scalar and vectored:
/// under the gate it installs no private mask (no `rt_sigprocmask` is
/// injected), and once an attempt would wait for room, a deliverable signal
/// ends it, with `ERESTARTSYS` before any byte and with the count after
/// some, and records the end; otherwise it waits on and completes.
/// Mutations: "SigBlk used" (the kernel mask blocks everything) and "the
/// private mask kept" (the `rt_sigprocmask` assertion).
#[tokio::test]
async fn under_the_gate_a_pipe_write_ends_where_it_would_wait_only_for_a_deliverable_signal() {
    let id = crate::types::OpenFileId::new(DetPid::from_raw(THREAD.as_raw()), 77);
    for vectored in [false, true] {
        for (first, ended) in [
            (Err(Errno::EAGAIN), Err(Errno::ERESTARTSYS)),
            (Ok(8), Ok(8)),
        ] {
            // A vectored write this small is atomic: an attempt writes all of
            // it or nothing, so it never has a partial count to return.
            if vectored && first.is_ok() {
                continue;
            }
            for row in rows() {
                let _seam = script(row.state);
                let mut guest = turn_guest(true, Some(row.own));
                add_pipe(&mut guest, 4, id);
                let rest = if first.is_ok() { Ok(8) } else { Ok(16) };
                guest.write_results = VecDeque::from([first, rest]);
                let tool = tool(&guest);
                let data = guest.stack + 2048;
                let result = if vectored {
                    let iov = guest.stack + 1024;
                    LocalMemory::new()
                        .write_value(AddrMut::<[usize; 2]>::from_raw(iov).unwrap(), &[data, 16])
                        .unwrap();
                    let call = reverie::syscalls::Writev::new()
                        .with_fd(4)
                        .with_iov(Addr::from_raw(iov))
                        .with_len(1);
                    tool.execute_blocking_pipe_writev(&mut guest, call, id)
                        .await
                } else {
                    let call = reverie::syscalls::Write::new()
                        .with_fd(4)
                        .with_buf(Addr::from_raw(data))
                        .with_len(16);
                    tool.execute_blocking_pipe_write(&mut guest, call, id).await
                };
                let name = format!("{} vectored={vectored} first={first:?}", row.name);
                assert_eq!(guest.thread.wait_signal_end, row.end, "{name}");
                assert!(
                    !guest.injected.contains(&Sysno::rt_sigprocmask),
                    "{name}: a private mask was installed: {:?}",
                    guest.injected
                );
                match (row.end, &ended, &result) {
                    (Some(_), Err(errno), Err(Error::Errno(got))) => {
                        assert_eq!(errno, got, "{name}")
                    }
                    (Some(_), Ok(count), Ok(got)) => assert_eq!(count, got, "{name}"),
                    (None, _, Ok(got)) => assert_eq!(*got, 16, "{name}: it waited on"),
                    _ => panic!("{name}: {result:?}"),
                }
            }
        }
    }
}

/// With the gate off a restartable poll keeps today's path: no gated check,
/// so a pending fatal signal does not end it, and no end is recorded.
#[tokio::test]
async fn with_the_gate_off_a_restartable_poll_keeps_today_s_path() {
    let _seam = script(proc_state(bit(libc::SIGTERM), 0, 0));
    let mut guest = turn_guest(false, Some(0));
    let pipe = crate::fd::DetFd::new(
        3,
        nix::fcntl::OFlag::O_RDONLY,
        crate::fd::FdType::Pipe,
        crate::types::OpenFileId::new(DetPid::from_raw(THREAD.as_raw()), 99),
    );
    pipe.set_physically_nonblocking();
    guest
        .thread
        .file_metadata
        .lock()
        .unwrap()
        .file_handles
        .insert(3, pipe);
    guest.read_results = VecDeque::from([Err(Errno::EAGAIN), Ok(4)]);
    let mut poll = Resources::new(THREAD);
    poll.insert(ResourceID::InternalIOPolling, Permission::R);
    poll.fyi("read");
    let read = reverie::syscalls::Read::new()
        .with_fd(3)
        .with_buf(AddrMut::from_raw(guest.stack))
        .with_len(8);
    let result = crate::syscalls::helpers::retry_nonblocking_syscall::<
        crate::record_or_replay::NoopTool,
        _,
        _,
    >(&mut guest, read, poll, None)
    .await;
    assert_eq!(result.unwrap(), 4);
    assert_eq!(guest.thread.wait_signal_end, None);
}

/// D41 with IR-5 and IR-14: under the gate, a default-fatal signal
/// deliverable when a kernel-run call begins means the call is not run. Each
/// family returns `ERESTARTNOINTR` with the end recorded as `Fatal` (the
/// checkpoint makes it `-EINTR`), before any guest-visible effect: no
/// scheduler request, no injection, and for a vfork parent neither
/// `clone_flags` nor `pending_vfork` set. Representative buffers: a read into
/// a read-only page (which, run, would fault), and an `rt_sigsuspend` with an
/// invalid mask size and a null mask (which, run, would be `EINVAL`).
/// Mutation "the check moved after the first guest-visible effect": the read
/// injects and faults, the vfork leaves `clone_flags` set.
#[tokio::test]
async fn under_the_gate_a_kernel_run_call_with_a_fatal_signal_at_its_entry_is_not_run() {
    let fatal = proc_state(bit(libc::SIGTERM), 0, 0);
    let readonly: &'static [u8; 16] = Box::leak(Box::new([0; 16]));
    let read = reverie::syscalls::Read::new()
        .with_fd(5)
        .with_buf(AddrMut::from_raw(readonly.as_ptr() as usize))
        .with_len(16);
    let sigsuspend = reverie::syscalls::RtSigsuspend::new()
        .with_mask(None)
        .with_sigsetsize(3);
    let vfork = reverie::syscalls::family::CloneFamily::Vfork(reverie::syscalls::Vfork::new());
    for family in ["background read", "rt_sigsuspend", "vfork"] {
        let _seam = script(fatal);
        let mut guest = turn_guest(true, Some(0));
        let tool = tool(&guest);
        let result = match family {
            "background read" => {
                tool.record_or_replay_blocking(&mut guest, read.into())
                    .await
            }
            "rt_sigsuspend" => tool.handle_rt_sigsuspend(&mut guest, sigsuspend).await,
            _ => tool.handle_clone_family(&mut guest, vfork).await,
        };
        assert!(
            matches!(result, Err(Error::Errno(Errno::ERESTARTNOINTR))),
            "{family}: {result:?}"
        );
        assert_eq!(
            guest.thread.wait_signal_end,
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            "{family}"
        );
        assert!(guest.rpcs().is_empty(), "{family}: {:?}", guest.rpcs());
        assert!(guest.injected.is_empty(), "{family}: {:?}", guest.injected);
        assert_eq!(guest.thread.clone_flags, None, "{family}");
        assert!(guest.thread.pending_vfork.is_none(), "{family}");
        // The checkpoint's row for it: -EINTR, interrupted.
        guest.thread.wait_signal_end = Some(WaitSignalEnd::Fatal(libc::SIGTERM));
        let (res, code) = tool
            .wait_end_checkpoint(
                &mut guest,
                &Syscall::from(read),
                Err(Errno::ERESTARTNOINTR.into()),
            )
            .await;
        assert!(matches!(res, Err(Error::Errno(Errno::EINTR))), "{family}");
        assert_eq!(code, Some(Errno::ERESTARTNOINTR), "{family}");
    }
    // A core-dumping signal deliverable behind a caught one: Linux delivers
    // the caught signal first and then the core-dumping one, at once or at
    // the return from the handler, before the call is made again, so the call
    // is not run either, and the end is left for the checkpoint to resolve.
    // Mutation "a BehindCaught end runs the call": the read is filed.
    let _seam = script(proc_state(
        bit(libc::SIGHUP) | bit(libc::SIGQUIT),
        bit(libc::SIGHUP),
        0,
    ));
    let mut guest = turn_guest(true, Some(0));
    let tool = tool(&guest);
    let result = tool
        .record_or_replay_blocking(&mut guest, read.into())
        .await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ERESTARTNOINTR))),
        "{result:?}"
    );
    assert!(
        matches!(
            guest.thread.wait_signal_end,
            Some(WaitSignalEnd::BehindCaught(_))
        ),
        "{:?}",
        guest.thread.wait_signal_end
    );
    assert!(guest.rpcs().is_empty(), "{:?}", guest.rpcs());
    assert!(guest.injected.is_empty(), "{:?}", guest.injected);
}

/// D41's not-run call with a core-dumping signal behind a caught one, through
/// the restart checkpoint (Claude review round 3): the checkpoint reads the
/// caught action. Its mask does not hold SIGQUIT: `Fatal`, `-EINTR` with the
/// interrupted code, nothing refused. Its mask holds SIGQUIT: `Caught`, whose
/// restart table would restart `ERESTARTNOINTR` after the handler, which 3c.1
/// cannot do, so the stopping refusal HERMIT_RESTART_AFTER_HANDLER is filed
/// and no restart code reaches the guest. Mutation "the restart-after-handler
/// refusal removed": the held row returns `ERESTARTNOINTR` and files nothing.
#[tokio::test]
async fn under_the_gate_a_not_run_call_behind_a_caught_signal_is_resolved_at_the_checkpoint() {
    let readonly: &'static [u8; 16] = Box::leak(Box::new([0; 16]));
    let read = reverie::syscalls::Read::new()
        .with_fd(5)
        .with_buf(AddrMut::from_raw(readonly.as_ptr() as usize))
        .with_len(16);
    let quit = bit(libc::SIGQUIT);
    for (name, mask, result, interrupted, refused) in [
        (
            "the handler's mask does not hold SIGQUIT",
            0,
            Errno::EINTR,
            Some(Errno::ERESTARTNOINTR),
            false,
        ),
        (
            "the handler's mask holds SIGQUIT",
            quit,
            Errno::EINTR,
            None,
            true,
        ),
    ] {
        let _seam = script(proc_state(bit(libc::SIGHUP) | quit, bit(libc::SIGHUP), 0));
        let mut guest = turn_guest(true, Some(0));
        guest.action_mask = mask;
        let tool = tool(&guest);
        let entry = tool
            .record_or_replay_blocking(&mut guest, read.into())
            .await;
        assert!(
            matches!(entry, Err(Error::Errno(Errno::ERESTARTNOINTR))),
            "{name}: {entry:?}"
        );
        let (res, code) = tool
            .wait_end_checkpoint(&mut guest, &Syscall::from(read), entry)
            .await;
        assert!(
            matches!(res, Err(Error::Errno(errno)) if errno == result),
            "{name}: {res:?}"
        );
        assert_eq!(code, interrupted, "{name}");
        assert_eq!(guest.injected, [Sysno::rt_sigaction], "{name}");
        let filed = guest.rpcs().iter().any(|rpc| {
            matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                if reason.contains("HERMIT_RESTART_AFTER_HANDLER"))
        });
        assert_eq!(filed, refused, "{name}");
    }
}

/// D41 refuses only a default-fatal signal at entry: a caught one, a blocked
/// one, or the gate off, and the background read is filed and run, here
/// faulting on its read-only buffer as the kernel would.
#[tokio::test]
async fn a_kernel_run_call_runs_when_no_default_fatal_signal_is_deliverable_at_its_entry() {
    let readonly: &'static [u8; 16] = Box::leak(Box::new([0; 16]));
    let read = reverie::syscalls::Read::new()
        .with_fd(5)
        .with_buf(AddrMut::from_raw(readonly.as_ptr() as usize))
        .with_len(16);
    for (name, gate, state, own) in [
        (
            "caught",
            true,
            proc_state(bit(libc::SIGUSR1), bit(libc::SIGUSR1), 0),
            0,
        ),
        (
            "blocked",
            true,
            proc_state(bit(libc::SIGTERM), 0, 0),
            bit(libc::SIGTERM),
        ),
        ("gate off", false, proc_state(bit(libc::SIGTERM), 0, 0), 0),
    ] {
        let _seam = script(state);
        let mut guest = turn_guest(gate, Some(own));
        guest.read_results = VecDeque::from([Err(Errno::EFAULT)]);
        let tool = tool(&guest);
        let result = tool
            .record_or_replay_blocking(&mut guest, read.into())
            .await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::EFAULT))),
            "{name}: {result:?}"
        );
        assert_eq!(guest.injected, [Sysno::read], "{name}");
        let filed: Vec<_> = guest
            .requests()
            .iter()
            .flat_map(|request| request.resources.keys().cloned())
            .collect();
        assert!(
            matches!(
                filed.as_slice(),
                [
                    ResourceID::BlockingExternalIO(_),
                    ResourceID::BlockedExternalContinue(_)
                ]
            ),
            "{name}: {filed:?}"
        );
        assert_eq!(guest.thread.wait_signal_end, None, "{name}");
    }
}

/// D44(b), one refusal per kernel-run resource: a send to a thread the
/// scheduler's committed state shows blocked in a background external call,
/// an `rt_sigsuspend` (under its temporary mask) or a vfork parent's wait,
/// of a signal that would interrupt it, files exactly one
/// `HERMIT_KERNEL_RUN_SIGNAL_REFUSED` stopping refusal, in the sender's turn,
/// and gets `EPERM`; nothing is sent. Not refused, and sent: a caught or
/// core-dumping signal to a vfork parent, a signal the call's mask blocks, an
/// ignored one, and any to a thread not so blocked. Unreadable dispositions
/// refuse, fail closed. SIGKILL and signal 0 never ask, nor does any send
/// with the gate off. The run-wide exit the refusal takes under the gate is
/// pinned by `stopping_refusal_tests`.
#[tokio::test]
async fn under_the_gate_a_send_to_a_kernel_run_call_is_refused_once_or_sent() {
    use crate::scheduler::KernelRunBlock;
    let usr1 = bit(libc::SIGUSR1);
    let term = bit(libc::SIGTERM);
    let external = KernelRunBlock {
        vfork: false,
        mask: Some(0),
    };
    let sigsuspend = KernelRunBlock {
        vfork: false,
        mask: Some(term),
    };
    let vfork = KernelRunBlock {
        vfork: true,
        mask: Some(0),
    };
    let readable = |caught| Ok(proc_state(0, caught, bit(libc::SIGHUP)));
    // name, what the scheduler holds, signal, the target's status, refused
    type Case = (
        &'static str,
        Option<KernelRunBlock>,
        i32,
        Result<KernelSignalState, Errno>,
        bool,
    );
    let cases: Vec<Case> = vec![
        (
            "external, default-fatal",
            Some(external),
            libc::SIGTERM,
            readable(0),
            true,
        ),
        (
            "external, caught",
            Some(external),
            libc::SIGUSR1,
            readable(usr1),
            true,
        ),
        (
            "rt_sigsuspend, unblocked by its temporary mask",
            Some(sigsuspend),
            libc::SIGUSR1,
            readable(usr1),
            true,
        ),
        (
            "rt_sigsuspend, blocked by its temporary mask",
            Some(sigsuspend),
            libc::SIGTERM,
            readable(0),
            false,
        ),
        (
            "vfork, default-fatal",
            Some(vfork),
            libc::SIGTERM,
            readable(0),
            true,
        ),
        (
            "vfork, caught",
            Some(vfork),
            libc::SIGUSR1,
            readable(usr1),
            false,
        ),
        (
            "vfork, core-dumping",
            Some(vfork),
            libc::SIGQUIT,
            readable(0),
            false,
        ),
        (
            "external, SIG_IGN",
            Some(external),
            libc::SIGHUP,
            readable(0),
            false,
        ),
        (
            "not blocked in a kernel-run call",
            None,
            libc::SIGTERM,
            readable(0),
            false,
        ),
        (
            "external, unreadable",
            Some(external),
            libc::SIGUSR2,
            Err(Errno::ESRCH),
            true,
        ),
    ];
    for (name, block, signal, peer, refused) in cases {
        let _seam = script_both(proc_state(0, 0, 0), peer);
        let mut guest = turn_guest(true, Some(0));
        guest.kernel_run = block;
        let tool = tool(&guest);
        let tgkill = reverie::syscalls::Tgkill::new()
            .with_tgid(PEER.as_raw())
            .with_tid(PEER.as_raw())
            .with_sig(signal);
        let result = tool
            .send_guest_signal(
                &mut guest,
                tgkill,
                PEER,
                signal,
                Some(DetPid::from_raw(PEER.as_raw())),
                true,
                PEER,
            )
            .await;
        let refusals = guest
            .rpcs()
            .iter()
            .filter(|rpc| {
                matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                if reason.contains("HERMIT_KERNEL_RUN_SIGNAL_REFUSED"))
            })
            .count();
        if refused {
            assert!(
                matches!(result, Err(Error::Errno(Errno::EPERM))),
                "{name}: {result:?}"
            );
            assert_eq!(refusals, 1, "{name}");
            assert!(guest.injected.is_empty(), "{name}: nothing sent");
        } else {
            assert_eq!(result.unwrap(), 0, "{name}");
            assert_eq!(refusals, 0, "{name}");
            assert_eq!(guest.injected, [Sysno::tgkill], "{name}: sent");
        }
    }
    // SIGKILL, signal 0 and the gate off never ask the scheduler.
    for (gate, signal) in [(true, libc::SIGKILL), (true, 0), (false, libc::SIGTERM)] {
        let _seam = script_both(proc_state(0, 0, 0), readable(0));
        let mut guest = turn_guest(gate, Some(0));
        guest.kernel_run = Some(external);
        let tool = tool(&guest);
        let tgkill = reverie::syscalls::Tgkill::new()
            .with_tgid(PEER.as_raw())
            .with_tid(PEER.as_raw())
            .with_sig(signal);
        let _ = tool
            .send_guest_signal(
                &mut guest,
                tgkill,
                PEER,
                signal,
                Some(DetPid::from_raw(PEER.as_raw())),
                true,
                PEER,
            )
            .await;
        assert!(
            !guest
                .rpcs()
                .iter()
                .any(|rpc| matches!(rpc, GlobalRequest::KernelRunTarget(_))),
            "gate={gate} signal={signal}"
        );
    }
}

/// Collects the WARN messages that name `HERMIT_STOP_SIGNAL_SEND_REFUSED`.
#[derive(Clone, Default)]
struct WarnLog(std::sync::Arc<Mutex<Vec<String>>>);

struct MessageVisitor(Option<String>);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

impl tracing::Subscriber for WarnLog {
    // "Sometimes" for every callsite, and a WARN hint: tracing caches each
    // callsite's interest across every registered subscriber, so a test on
    // another thread of the same process that registers its own subscriber
    // could otherwise leave the WARN callsite cached as never, and the event
    // would not reach `event`.
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::WARN)
    }
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = MessageVisitor(None);
        event.record(&mut visitor);
        if let Some(message) = visitor.0
            && message.contains("HERMIT_STOP_SIGNAL_SEND_REFUSED")
        {
            self.0.lock().unwrap().push(message);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// D48 with IR-4(b): under the gate a stop-signal send gets `EPERM` and the
/// named WARN, by each target form and to the sender itself: SIGSTOP always;
/// SIGTSTP, SIGTTIN and SIGTTOU while the target's action is `SIG_DFL` in its
/// `/proc` status, read in the sender's turn, and when that status cannot be
/// read (fail closed). Not refused: SIGTSTP the target ignores or catches,
/// SIGCONT, a target with no live thread (its ordinary answer), and every
/// send with the gate off.
#[tokio::test]
async fn under_the_gate_a_stop_signal_send_is_refused_with_eperm_and_a_warn() {
    use crate::syscalls::ReservedSendSiginfo;
    use crate::syscalls::ReservedSendTarget;
    let tstp = bit(libc::SIGTSTP);
    let peer_target = |form: &str| match form {
        "kill" => ReservedSendTarget::Process(PEER.as_raw()),
        "tkill" => ReservedSendTarget::Thread {
            tgid: None,
            tid: PEER.as_raw(),
        },
        "tgkill" => ReservedSendTarget::Thread {
            tgid: Some(PEER.as_raw()),
            tid: PEER.as_raw(),
        },
        "self" => ReservedSendTarget::Process(THREAD.as_raw()),
        _ => ReservedSendTarget::Pidfd {
            named: Some(DetPid::from_raw(PEER.as_raw())),
            thread: false,
        },
    };
    let cases: Vec<(&str, i32, Result<KernelSignalState, Errno>, bool)> = vec![
        ("SIGSTOP", libc::SIGSTOP, Ok(proc_state(0, 0, 0)), true),
        (
            "SIGTSTP SIG_DFL",
            libc::SIGTSTP,
            Ok(proc_state(0, 0, 0)),
            true,
        ),
        (
            "SIGTTIN SIG_DFL",
            libc::SIGTTIN,
            Ok(proc_state(0, 0, 0)),
            true,
        ),
        (
            "SIGTTOU SIG_DFL",
            libc::SIGTTOU,
            Ok(proc_state(0, 0, 0)),
            true,
        ),
        (
            "SIGTSTP SIG_IGN",
            libc::SIGTSTP,
            Ok(proc_state(0, 0, tstp)),
            false,
        ),
        (
            "SIGTSTP caught",
            libc::SIGTSTP,
            Ok(proc_state(0, tstp, 0)),
            false,
        ),
        (
            "SIGTSTP, status unreadable",
            libc::SIGTSTP,
            Err(Errno::ESRCH),
            true,
        ),
        ("SIGCONT", libc::SIGCONT, Ok(proc_state(0, 0, 0)), false),
    ];
    for form in ["kill", "tkill", "tgkill", "pidfd", "self"] {
        for (name, signal, status, refused) in &cases {
            let status = *status;
            // The sender's own status, for the self row, is the peer's.
            let own = status.unwrap_or_else(|_| proc_state(0, 0, 0));
            let _seam = if form == "self" {
                signal_state_read_seam::install(move |_, tid| {
                    (tid.as_raw() == THREAD.as_raw()).then_some(status)
                })
            } else {
                script_both(own, status)
            };
            let log = WarnLog::default();
            let _subscriber = tracing::subscriber::set_default(log.clone());
            let mut guest = turn_guest(true, Some(0));
            let tool = tool(&guest);
            let result = tool
                .refuse_stop_signal_send(
                    &mut guest,
                    Sysno::kill,
                    *signal,
                    peer_target(form),
                    ReservedSendSiginfo::None,
                )
                .await;
            let warns = log.0.lock().unwrap().len();
            if *refused {
                assert!(
                    matches!(result, Some(Err(Error::Errno(Errno::EPERM)))),
                    "{form}: {name}: {result:?}"
                );
                assert_eq!(warns, 1, "{form}: {name}");
            } else {
                assert!(result.is_none(), "{form}: {name}: {result:?}");
                assert_eq!(warns, 0, "{form}: {name}");
            }
        }
    }
    // No live target keeps the ordinary path; the gate off refuses nothing.
    let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
    let mut guest = turn_guest(true, Some(0));
    guest.live = vec![THREAD];
    let tool = tool(&guest);
    assert!(
        tool.refuse_stop_signal_send(
            &mut guest,
            Sysno::kill,
            libc::SIGSTOP,
            ReservedSendTarget::Process(PEER.as_raw()),
            ReservedSendSiginfo::None,
        )
        .await
        .is_none()
    );
    let mut guest = turn_guest(false, Some(0));
    assert!(
        tool.refuse_stop_signal_send(
            &mut guest,
            Sysno::kill,
            libc::SIGSTOP,
            ReservedSendTarget::Process(PEER.as_raw()),
            ReservedSendSiginfo::None,
        )
        .await
        .is_none()
    );
    assert!(guest.rpcs().is_empty());
}

/// D48's hooks: each send handler (`kill`, `tkill`, `tgkill`,
/// `rt_sigqueueinfo`, `rt_tgsigqueueinfo`) refuses a SIGSTOP under the gate
/// before anything else reaches the scheduler or the kernel. The queued
/// sends here carry a null `siginfo`, which Linux's copy refuses with
/// `EFAULT` before any send or permission check (`__copy_siginfo_from_user`,
/// kernel/signal.c:3417-3424 at v6.12), so they get `EFAULT` and no WARN;
/// `under_the_gate_a_queued_stop_signal_send_keeps_the_kernel_s_earlier_errors`
/// has their valid-`siginfo` refusals. (The `pidfd_send_signal` hook sits
/// beside its reserved-signal refusal and is covered through
/// `refuse_stop_signal_send`'s pidfd form.)
#[tokio::test]
async fn under_the_gate_every_send_handler_refuses_a_sigstop() {
    let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
    let peer = PEER.as_raw();
    for handler in [
        "kill",
        "tkill",
        "tgkill",
        "rt_sigqueueinfo",
        "rt_tgsigqueueinfo",
    ] {
        let log = WarnLog::default();
        let _subscriber = tracing::subscriber::set_default(log.clone());
        let mut guest = turn_guest(true, Some(0));
        use_real_test_thread(&mut guest);
        let tool = tool(&guest);
        let result = match handler {
            "kill" => {
                tool.handle_kill(
                    &mut guest,
                    reverie::syscalls::Kill::new()
                        .with_pid(peer)
                        .with_sig(libc::SIGSTOP),
                )
                .await
            }
            "tkill" => {
                tool.handle_tkill(
                    &mut guest,
                    reverie::syscalls::Tkill::new()
                        .with_tid(peer)
                        .with_sig(libc::SIGSTOP),
                )
                .await
            }
            "tgkill" => {
                tool.handle_tgkill(
                    &mut guest,
                    reverie::syscalls::Tgkill::new()
                        .with_tgid(peer)
                        .with_tid(peer)
                        .with_sig(libc::SIGSTOP),
                )
                .await
            }
            "rt_sigqueueinfo" => {
                tool.handle_rt_sigqueueinfo(
                    &mut guest,
                    reverie::syscalls::RtSigqueueinfo::new()
                        .with_tgid(peer)
                        .with_sig(libc::SIGSTOP),
                )
                .await
            }
            _ => {
                tool.handle_rt_tgsigqueueinfo(
                    &mut guest,
                    reverie::syscalls::RtTgsigqueueinfo::new()
                        .with_tgid(peer)
                        .with_tid(peer)
                        .with_sig(libc::SIGSTOP),
                )
                .await
            }
        };
        let queued = handler.starts_with("rt_");
        let wanted = if queued { Errno::EFAULT } else { Errno::EPERM };
        assert!(
            matches!(result, Err(Error::Errno(errno)) if errno == wanted),
            "{handler}: {result:?}"
        );
        assert_eq!(
            log.0.lock().unwrap().len(),
            usize::from(!queued),
            "{handler}"
        );
        assert!(guest.injected.is_empty(), "{handler}: {:?}", guest.injected);
        assert!(
            guest
                .rpcs()
                .iter()
                .all(|rpc| matches!(rpc, GlobalRequest::ResolveKillTargets(_))),
            "{handler}: {:?}",
            guest.rpcs()
        );
    }
}

/// A `siginfo_t` for `signal` with `code`, in a fresh page of this process
/// left with protection `prot`; `expansion` is the first byte after the 48
/// every copy reads (`KERNEL_SIGINFO_SIZE`). Leaked: the sends read it.
fn siginfo_page(signal: i32, code: i32, expansion: u8, prot: i32) -> usize {
    // SAFETY: a fresh private anonymous page of this process, written before
    // its protection is lowered.
    unsafe {
        let page = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        assert_ne!(page, libc::MAP_FAILED);
        let bytes = page.cast::<u8>();
        std::ptr::copy_nonoverlapping(signal.to_ne_bytes().as_ptr(), bytes, 4);
        std::ptr::copy_nonoverlapping(code.to_ne_bytes().as_ptr(), bytes.add(8), 4);
        *bytes.add(48) = expansion;
        assert_eq!(libc::mprotect(page, 4096, prot), 0);
        page as usize
    }
}

/// The `siginfo` rows of a queued send of `signal` that D44 or D48 would
/// refuse: the name, the address the call carries, and the errno Linux gives
/// before any send, in its order (v6.12 kernel/signal.c), or `None` for a
/// valid send, which reaches the refusal: a null pointer and a `PROT_NONE`
/// page are `EFAULT` (the copy, :3417-3424); a code whose layout Linux does
/// not know with a nonzero expansion byte is `E2BIG` (:3394-3415); `SI_USER`
/// and `SI_TKILL` from a caller that is not the target are `EPERM`
/// (`do_rt_sigqueueinfo`, :4065-4067; `do_rt_tgsigqueueinfo`, :4112-4114). A
/// valid `SI_QUEUE` siginfo in a read-only page copies: Linux only reads it.
fn queued_siginfo_rows(signal: i32) -> Vec<(&'static str, usize, Option<Errno>)> {
    let page = |code, expansion, prot| siginfo_page(signal, code, expansion, prot);
    vec![
        ("a null siginfo", 0, Some(Errno::EFAULT)),
        (
            "a siginfo in a PROT_NONE page",
            page(libc::SI_QUEUE, 0, libc::PROT_NONE),
            Some(Errno::EFAULT),
        ),
        (
            "an unknown si_code with a nonzero expansion byte",
            page(-100, 1, libc::PROT_READ),
            Some(Errno::E2BIG),
        ),
        (
            "a forged SI_USER",
            page(libc::SI_USER, 0, libc::PROT_READ),
            Some(Errno::EPERM),
        ),
        (
            "a forged SI_TKILL",
            page(libc::SI_TKILL, 0, libc::PROT_READ),
            Some(Errno::EPERM),
        ),
        (
            "a valid SI_QUEUE siginfo in a read-only page",
            page(libc::SI_QUEUE, 0, libc::PROT_READ),
            None,
        ),
    ]
}

/// D48's errno precedence (Codex review round 1): through the real
/// `rt_sigqueueinfo` and `rt_tgsigqueueinfo` handlers, a SIGSTOP whose send
/// Linux fails before sending anything gets that errno and no WARN; only a
/// valid one is refused (`EPERM` and the WARN). Through
/// `refuse_stop_signal_send`'s pidfd form: no siginfo, or a valid one, is
/// refused; a `PROT_NONE` page is `EFAULT`, a `si_signo` that is not the
/// signal `EINVAL` (`pidfd_send_signal`, :3959-3966) and a forged code `EPERM`
/// (:3968-3972), without the WARN. An invalid signal number is no stop
/// signal: it takes the ordinary path, and the kernel's `EINVAL`. Mutation
/// "the kernel's earlier errors not checked before the refusal": each errno
/// row becomes a refused `EPERM` with a WARN.
#[tokio::test]
async fn under_the_gate_a_queued_stop_signal_send_keeps_the_kernel_s_earlier_errors() {
    use crate::syscalls::ReservedSendSiginfo;
    use crate::syscalls::ReservedSendTarget;
    let peer = PEER.as_raw();
    for handler in ["rt_sigqueueinfo", "rt_tgsigqueueinfo"] {
        let mut rows = queued_siginfo_rows(libc::SIGSTOP)
            .into_iter()
            .map(|(name, address, errno)| (name, libc::SIGSTOP, address, errno))
            .collect::<Vec<_>>();
        // The ordinary path of a process-directed send asks the scheduler
        // more than this harness answers; the thread-directed one is enough
        // to show an invalid number is not refused.
        if handler == "rt_tgsigqueueinfo" {
            rows.push((
                "an invalid signal number",
                65,
                siginfo_page(65, libc::SI_QUEUE, 0, libc::PROT_READ),
                Some(Errno::EINVAL),
            ));
        }
        for (name, signal, address, errno) in rows {
            let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
            let log = WarnLog::default();
            let _subscriber = tracing::subscriber::set_default(log.clone());
            let mut guest = turn_guest(true, Some(0));
            use_real_test_thread(&mut guest);
            let tool = tool(&guest);
            let info = AddrMut::from_raw(address);
            let result = if handler == "rt_sigqueueinfo" {
                tool.handle_rt_sigqueueinfo(
                    &mut guest,
                    reverie::syscalls::RtSigqueueinfo::new()
                        .with_tgid(peer)
                        .with_sig(signal)
                        .with_siginfo(info),
                )
                .await
            } else {
                tool.handle_rt_tgsigqueueinfo(
                    &mut guest,
                    reverie::syscalls::RtTgsigqueueinfo::new()
                        .with_tgid(peer)
                        .with_tid(peer)
                        .with_sig(signal)
                        .with_siginfo(info),
                )
                .await
            };
            let wanted = errno.unwrap_or(Errno::EPERM);
            assert!(
                matches!(result, Err(Error::Errno(got)) if got == wanted),
                "{handler}: {name}: {result:?}"
            );
            let warns = log.0.lock().unwrap().len();
            assert_eq!(warns, usize::from(errno.is_none()), "{handler}: {name}");
            if signal == libc::SIGSTOP {
                assert!(guest.injected.is_empty(), "{handler}: {name}: nothing sent");
            }
        }
    }
    // The pidfd form.
    let pidfd_rows = [
        ("no siginfo", None, None),
        (
            "a siginfo in a PROT_NONE page",
            Some(siginfo_page(
                libc::SIGSTOP,
                libc::SI_QUEUE,
                0,
                libc::PROT_NONE,
            )),
            Some(Errno::EFAULT),
        ),
        (
            "a si_signo that is not the signal",
            Some(siginfo_page(
                libc::SIGTERM,
                libc::SI_QUEUE,
                0,
                libc::PROT_READ,
            )),
            Some(Errno::EINVAL),
        ),
        (
            "a forged SI_USER",
            Some(siginfo_page(
                libc::SIGSTOP,
                libc::SI_USER,
                0,
                libc::PROT_READ,
            )),
            Some(Errno::EPERM),
        ),
        (
            "a valid SI_QUEUE siginfo in a read-only page",
            Some(siginfo_page(
                libc::SIGSTOP,
                libc::SI_QUEUE,
                0,
                libc::PROT_READ,
            )),
            None,
        ),
    ];
    for (name, address, errno) in pidfd_rows {
        let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
        let log = WarnLog::default();
        let _subscriber = tracing::subscriber::set_default(log.clone());
        let mut guest = turn_guest(true, Some(0));
        use_real_test_thread(&mut guest);
        let tool = tool(&guest);
        let result = tool
            .refuse_stop_signal_send(
                &mut guest,
                Sysno::pidfd_send_signal,
                libc::SIGSTOP,
                ReservedSendTarget::Pidfd {
                    named: Some(DetPid::from_raw(peer)),
                    thread: false,
                },
                ReservedSendSiginfo::Pidfd(address),
            )
            .await;
        let wanted = errno.unwrap_or(Errno::EPERM);
        assert!(
            matches!(result, Some(Err(Error::Errno(got))) if got == wanted),
            "pidfd: {name}: {result:?}"
        );
        assert_eq!(
            log.0.lock().unwrap().len(),
            usize::from(errno.is_none()),
            "pidfd: {name}"
        );
    }
}

/// The number of `HERMIT_KERNEL_RUN_SIGNAL_REFUSED` stopping refusals
/// `guest` filed.
fn kernel_run_refusals(guest: &TurnGuest) -> usize {
    guest
        .rpcs()
        .iter()
        .filter(|rpc| {
            matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                if reason.contains("HERMIT_KERNEL_RUN_SIGNAL_REFUSED"))
        })
        .count()
}

/// D44's errno precedence (Codex review round 1): a send to a thread blocked
/// in a kernel-run call, of a signal that would interrupt it, files the
/// stopping refusal only when Linux would send it. Through the real handlers:
/// `rt_tgsigqueueinfo` with each `siginfo` row (`queued_siginfo_rows`);
/// `tgkill` with a tgid of 0 (`EINVAL`, kernel/signal.c:4038-4039 at v6.12)
/// or a tgid that is not the target's process (`ESRCH`, `do_send_specific`,
/// :3994); `tkill`. An invalid signal number is not refused and the kernel
/// answers `EINVAL`. Through `send_guest_signal`: `rt_sigqueueinfo`'s and
/// `pidfd_send_signal`'s `siginfo` (no siginfo refused; a `si_signo` that is
/// not the signal `EINVAL`, :3959-3966). Each errno row files no refusal and
/// sends nothing. Mutation "the kernel's earlier errors not checked before
/// the refusal": every errno row files a refusal and gets `EPERM`.
#[tokio::test]
async fn under_the_gate_a_send_to_a_kernel_run_call_keeps_the_kernel_s_earlier_errors() {
    use crate::scheduler::KernelRunBlock;
    let peer = PEER.as_raw();
    let external = KernelRunBlock {
        vfork: false,
        mask: Some(0),
    };
    let blocked_peer = || {
        let mut guest = turn_guest(true, Some(0));
        use_real_test_thread(&mut guest);
        guest.kernel_run = Some(external);
        guest
    };
    // name, result, injected, refusals, wanted errno (None: refused)
    let check =
        |name: &str, result: Result<i64, Error>, guest: &TurnGuest, errno: Option<Errno>| {
            let wanted = errno.unwrap_or(Errno::EPERM);
            assert!(
                matches!(result, Err(Error::Errno(got)) if got == wanted),
                "{name}: {result:?}"
            );
            assert_eq!(
                kernel_run_refusals(guest),
                usize::from(errno.is_none()),
                "{name}"
            );
        };
    for (name, address, errno) in queued_siginfo_rows(libc::SIGTERM) {
        let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
        let mut guest = blocked_peer();
        let tool = tool(&guest);
        let result = tool
            .handle_rt_tgsigqueueinfo(
                &mut guest,
                reverie::syscalls::RtTgsigqueueinfo::new()
                    .with_tgid(peer)
                    .with_tid(peer)
                    .with_sig(libc::SIGTERM)
                    .with_siginfo(AddrMut::from_raw(address)),
            )
            .await;
        check(&format!("rt_tgsigqueueinfo: {name}"), result, &guest, errno);
        assert!(guest.injected.is_empty(), "rt_tgsigqueueinfo: {name}");
    }
    let valid = siginfo_page(libc::SIGTERM, libc::SI_QUEUE, 0, libc::PROT_READ);
    // rt_tgsigqueueinfo: a thread outside the named group.
    {
        let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
        let mut guest = blocked_peer();
        let tool = tool(&guest);
        let result = tool
            .handle_rt_tgsigqueueinfo(
                &mut guest,
                reverie::syscalls::RtTgsigqueueinfo::new()
                    .with_tgid(THREAD.as_raw())
                    .with_tid(peer)
                    .with_sig(libc::SIGTERM)
                    .with_siginfo(AddrMut::from_raw(valid)),
            )
            .await;
        check(
            "rt_tgsigqueueinfo: another group",
            result,
            &guest,
            Some(Errno::ESRCH),
        );
        assert!(guest.injected.is_empty());
    }
    // tgkill and tkill: name, tgid, signal, wanted errno, sent.
    for (name, tgid, signal, errno, sent) in [
        ("tgkill", Some(peer), libc::SIGTERM, None, false),
        (
            "tgkill: tgid 0",
            Some(0),
            libc::SIGTERM,
            Some(Errno::EINVAL),
            false,
        ),
        (
            "tgkill: another group",
            Some(THREAD.as_raw()),
            libc::SIGTERM,
            Some(Errno::ESRCH),
            false,
        ),
        (
            "tgkill: invalid signal",
            Some(peer),
            65,
            Some(Errno::EINVAL),
            true,
        ),
        ("tkill", None, libc::SIGTERM, None, false),
        ("tkill: invalid signal", None, 65, Some(Errno::EINVAL), true),
    ] {
        let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
        let mut guest = blocked_peer();
        let tool = tool(&guest);
        let result = match tgid {
            Some(tgid) => {
                tool.handle_tgkill(
                    &mut guest,
                    reverie::syscalls::Tgkill::new()
                        .with_tgid(tgid)
                        .with_tid(peer)
                        .with_sig(signal),
                )
                .await
            }
            None => {
                tool.handle_tkill(
                    &mut guest,
                    reverie::syscalls::Tkill::new()
                        .with_tid(peer)
                        .with_sig(signal),
                )
                .await
            }
        };
        check(name, result, &guest, errno);
        assert_eq!(
            !guest.injected.is_empty(),
            sent,
            "{name}: {:?}",
            guest.injected
        );
    }
    // rt_sigqueueinfo, process-directed, through send_guest_signal.
    for (name, address, errno) in [
        ("rt_sigqueueinfo: a null siginfo", 0, Some(Errno::EFAULT)),
        ("rt_sigqueueinfo: a valid siginfo", valid, None),
    ] {
        let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
        let mut guest = blocked_peer();
        let tool = tool(&guest);
        let send = reverie::syscalls::RtSigqueueinfo::new()
            .with_tgid(peer)
            .with_sig(libc::SIGTERM)
            .with_siginfo(AddrMut::from_raw(address));
        let result = tool
            .send_guest_signal(
                &mut guest,
                send,
                PEER,
                libc::SIGTERM,
                Some(DetPid::from_raw(peer)),
                false,
                PEER,
            )
            .await;
        check(name, result, &guest, errno);
        assert!(guest.injected.is_empty(), "{name}");
    }
    // pidfd_send_signal, thread-scoped, through send_guest_signal.
    for (name, address, errno) in [
        ("pidfd: no siginfo", 0, None),
        (
            "pidfd: a siginfo in a PROT_NONE page",
            siginfo_page(libc::SIGTERM, libc::SI_QUEUE, 0, libc::PROT_NONE),
            Some(Errno::EFAULT),
        ),
        (
            "pidfd: a si_signo that is not the signal",
            siginfo_page(libc::SIGUSR1, libc::SI_QUEUE, 0, libc::PROT_READ),
            Some(Errno::EINVAL),
        ),
        (
            "pidfd: a forged SI_USER",
            siginfo_page(libc::SIGTERM, libc::SI_USER, 0, libc::PROT_READ),
            Some(Errno::EPERM),
        ),
        ("pidfd: a valid siginfo", valid, None),
    ] {
        let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
        let mut guest = blocked_peer();
        let tool = tool(&guest);
        let send = Syscall::from_raw(
            Sysno::pidfd_send_signal,
            reverie::syscalls::SyscallArgs::new(9, libc::SIGTERM as usize, address, 0, 0, 0),
        );
        let result = tool
            .send_guest_signal(
                &mut guest,
                send,
                PEER,
                libc::SIGTERM,
                Some(DetPid::from_raw(peer)),
                true,
                PEER,
            )
            .await;
        check(name, result, &guest, errno);
        assert!(guest.injected.is_empty(), "{name}");
    }
}

/// D44 and D48 ask about the task a nested PID namespace's caller names, as
/// the scheduler numbers it (Codex review round 1): the caller's id 2 names
/// `PEER`. `tgkill`, `tkill` and `rt_tgsigqueueinfo` of SIGTERM to `PEER`,
/// blocked in a kernel-run call, are refused once each, the scheduler asked
/// about `PEER` and never about id 2; a `tkill` of SIGSTOP is refused with
/// the WARN after reading `PEER`'s status. Mutation "D44 asks about the
/// untranslated id" (`target` for a thread-directed send): the query finds no
/// kernel-run call for id 2, and the send is made.
#[tokio::test]
async fn under_the_gate_d44_and_d48_ask_about_the_task_a_nested_caller_names() {
    use crate::scheduler::KernelRunBlock;
    let nested_id = 2;
    let valid = siginfo_page(libc::SIGTERM, libc::SI_QUEUE, 0, libc::PROT_READ);
    for handler in ["tgkill", "tkill", "rt_tgsigqueueinfo", "tkill SIGSTOP"] {
        let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
        let log = WarnLog::default();
        let _subscriber = tracing::subscriber::set_default(log.clone());
        let mut guest = turn_guest(true, Some(0));
        guest.thread.nested_pid_namespace = true;
        guest.nested = vec![(nested_id, PEER)];
        guest.kernel_run = Some(KernelRunBlock {
            vfork: false,
            mask: Some(0),
        });
        guest.kernel_run_for = Some(PEER);
        let tool = tool(&guest);
        let result = match handler {
            "tgkill" => {
                tool.handle_tgkill(
                    &mut guest,
                    reverie::syscalls::Tgkill::new()
                        .with_tgid(nested_id)
                        .with_tid(nested_id)
                        .with_sig(libc::SIGTERM),
                )
                .await
            }
            "tkill" => {
                tool.handle_tkill(
                    &mut guest,
                    reverie::syscalls::Tkill::new()
                        .with_tid(nested_id)
                        .with_sig(libc::SIGTERM),
                )
                .await
            }
            "rt_tgsigqueueinfo" => {
                tool.handle_rt_tgsigqueueinfo(
                    &mut guest,
                    reverie::syscalls::RtTgsigqueueinfo::new()
                        .with_tgid(nested_id)
                        .with_tid(nested_id)
                        .with_sig(libc::SIGTERM)
                        .with_siginfo(AddrMut::from_raw(valid)),
                )
                .await
            }
            _ => {
                tool.handle_tkill(
                    &mut guest,
                    reverie::syscalls::Tkill::new()
                        .with_tid(nested_id)
                        .with_sig(libc::SIGSTOP),
                )
                .await
            }
        };
        assert!(
            matches!(result, Err(Error::Errno(Errno::EPERM))),
            "{handler}: {result:?}"
        );
        assert!(guest.injected.is_empty(), "{handler}: {:?}", guest.injected);
        let stop = handler == "tkill SIGSTOP";
        assert_eq!(kernel_run_refusals(&guest), usize::from(!stop), "{handler}");
        assert_eq!(log.0.lock().unwrap().len(), usize::from(stop), "{handler}");
        let asked: Vec<DetTid> = guest
            .rpcs()
            .iter()
            .filter_map(|rpc| match rpc {
                GlobalRequest::KernelRunTarget(target) => Some(*target),
                _ => None,
            })
            .collect();
        let wanted: &[DetTid] = if stop { &[] } else { &[PEER] };
        assert_eq!(asked, wanted, "{handler}");
    }
}

/// Registers descriptor `fd` of `guest` as a pipe Hermit made physically
/// nonblocking, with open file `id` and the stat a tracked descriptor has
/// when metadata is virtualized.
fn add_pipe_with_stat(guest: &mut TurnGuest, fd: i32, id: crate::types::OpenFileId) {
    let pipe = crate::fd::DetFd::new(fd, nix::fcntl::OFlag::O_WRONLY, crate::fd::FdType::Pipe, id)
        .with_stat(crate::stat::DetStat::default());
    pipe.set_physically_nonblocking();
    guest
        .thread
        .file_metadata
        .lock()
        .unwrap()
        .file_handles
        .insert(fd, pipe);
}

/// D41 at `write`'s entry (both reviews, round 1), through the real
/// `handle_write` on a pipe with virtualized metadata: under the gate, a
/// default-fatal signal deliverable at entry means the write is not run, with
/// no virtual mtime bump (no `TouchFile`), no scheduler request and no
/// injection, and the end recorded `Fatal`. A caught signal, or the gate off,
/// and the write runs: the mtime is bumped and the bytes written. Mutation
/// "the check after `touch_file`": the not-run write bumps the mtime.
#[tokio::test]
async fn under_the_gate_a_write_with_a_fatal_signal_at_its_entry_is_not_run_and_bumps_no_mtime() {
    let buffer: &'static [u8; 4] = Box::leak(Box::new(*b"data"));
    let write = reverie::syscalls::Write::new()
        .with_fd(7)
        .with_buf(Addr::from_raw(buffer.as_ptr() as usize))
        .with_len(4);
    for (name, gate, state, run) in [
        ("fatal", true, proc_state(bit(libc::SIGTERM), 0, 0), false),
        (
            "caught",
            true,
            proc_state(bit(libc::SIGUSR1), bit(libc::SIGUSR1), 0),
            true,
        ),
        (
            "gate off",
            false,
            proc_state(bit(libc::SIGTERM), 0, 0),
            true,
        ),
    ] {
        let _seam = script(state);
        let mut guest = turn_guest(gate, Some(0));
        guest.config.virtualize_metadata = true;
        add_pipe_with_stat(
            &mut guest,
            7,
            crate::types::OpenFileId::new(DetPid::from_raw(THREAD.as_raw()), 70),
        );
        guest.write_results = VecDeque::from([Ok(4)]);
        let tool = tool(&guest);
        let result = tool.handle_write(&mut guest, write).await;
        let touched = guest
            .rpcs()
            .iter()
            .any(|rpc| matches!(rpc, GlobalRequest::TouchFile(_)));
        if run {
            assert_eq!(result.unwrap(), 4, "{name}");
            assert!(touched, "{name}: the mtime is bumped");
            assert!(guest.injected.contains(&Sysno::write), "{name}");
        } else {
            assert!(
                matches!(result, Err(Error::Errno(Errno::ERESTARTNOINTR))),
                "{name}: {result:?}"
            );
            assert!(!touched, "{name}: no mtime bump");
            assert!(guest.rpcs().is_empty(), "{name}: {:?}", guest.rpcs());
            assert!(guest.injected.is_empty(), "{name}: {:?}", guest.injected);
            assert_eq!(
                guest.thread.wait_signal_end,
                Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
                "{name}"
            );
        }
    }
}

/// D41 through the real `handle_read` of an inherited external pipe, a
/// background external read (both reviews, round 1: the handler, not only the
/// filing helper): under the gate a default-fatal signal deliverable at entry
/// means the read is not run, with no scheduler request and no injection,
/// and the end recorded `Fatal`; with the gate off the read is filed in the
/// background and run.
#[tokio::test]
async fn under_the_gate_a_background_read_with_a_fatal_signal_at_its_entry_is_not_run() {
    let buffer: &'static mut [u8; 16] = Box::leak(Box::new([0; 16]));
    let read = reverie::syscalls::Read::new()
        .with_fd(8)
        .with_buf(AddrMut::from_raw(buffer.as_mut_ptr() as usize))
        .with_len(16);
    for (name, gate) in [("gate on", true), ("gate off", false)] {
        let _seam = script(proc_state(bit(libc::SIGTERM), 0, 0));
        let mut guest = turn_guest(gate, Some(0));
        let id = crate::types::OpenFileId::new(DetPid::from_raw(THREAD.as_raw()), 80);
        let pipe =
            crate::fd::DetFd::new(8, nix::fcntl::OFlag::O_RDONLY, crate::fd::FdType::Pipe, id);
        pipe.set_inherited_external_read(crate::fd::InheritedExternalRead {
            origin: id,
            dev: 0,
            inode: 0,
            rdev: 0,
            file_type: libc::S_IFIFO,
            access_mode: libc::O_RDONLY,
            resource: None,
        });
        guest
            .thread
            .file_metadata
            .lock()
            .unwrap()
            .file_handles
            .insert(8, pipe);
        guest.read_results = VecDeque::from([Ok(3)]);
        let tool = tool(&guest);
        let result = tool.handle_read(&mut guest, read).await;
        if gate {
            assert!(
                matches!(result, Err(Error::Errno(Errno::ERESTARTNOINTR))),
                "{name}: {result:?}"
            );
            // The handler's release of what it holds, which is nothing.
            assert!(
                guest
                    .rpcs()
                    .iter()
                    .all(|rpc| matches!(rpc, GlobalRequest::ReleaseAllResources)),
                "{name}: {:?}",
                guest.rpcs()
            );
            assert!(guest.injected.is_empty(), "{name}: {:?}", guest.injected);
            assert_eq!(
                guest.thread.wait_signal_end,
                Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
                "{name}"
            );
        } else {
            assert_eq!(result.unwrap(), 3, "{name}");
            assert_eq!(guest.injected, [Sysno::read], "{name}");
            assert!(
                guest.requests().iter().any(|request| request
                    .resources
                    .keys()
                    .any(|resource| matches!(resource, ResourceID::BlockingExternalIO(_)))),
                "{name}: {:?}",
                guest.requests()
            );
        }
    }
}

/// A siginfo page this test thread can write but whose reads fail: written,
/// then left `PROT_WRITE` only (x86 Linux's `copy_from_user` reads it, and
/// `process_vm_readv` refuses it). Leaked.
fn write_only_siginfo_page(signal: i32) -> usize {
    let page = siginfo_page(
        signal,
        libc::SI_QUEUE,
        0,
        libc::PROT_READ | libc::PROT_WRITE,
    );
    // SAFETY: the page `siginfo_page` mapped for this test.
    assert_eq!(
        unsafe { libc::mprotect(page as *mut libc::c_void, 4096, libc::PROT_WRITE) },
        0
    );
    page
}

/// An address in no mapping of this process, and not below a grow-down
/// one: the page at 0x1000, below `vm.mmap_min_addr` (65536 by default), which
/// no mapping of this test process occupies. A page this test mapped and then
/// unmapped could be taken by another test's mmap in the same process (plain
/// `cargo test` runs tests as threads of one process) before the read, which
/// once made the row fail.
fn unmapped_page() -> usize {
    0x1000
}

/// A refused send's siginfo that the caller's own reads cannot reach is
/// EFAULT only where Linux's copy faults too, as the caller's
/// `/proc/<tid>/smaps` shows (both reviews, round 2): through the real
/// `rt_tgsigqueueinfo` handler, a SIGSTOP (D48) and a SIGTERM to a thread
/// blocked in a kernel-run call (D44). A write-only mapping, which x86 Linux
/// copies from, takes the refusal (EPERM and the WARN; EPERM and the
/// stopping refusal), and so does any unreadable siginfo when the caller's
/// mappings cannot be read (fail closed); a `PROT_NONE` page and an unmapped
/// page are EFAULT, with neither. Mutation "an unverifiable read taken as
/// EFAULT": the write-only and unreadable-mappings rows become EFAULT.
#[tokio::test]
async fn under_the_gate_a_refused_send_s_unreadable_siginfo_is_efault_only_where_linux_faults() {
    use crate::scheduler::KernelRunBlock;
    // SAFETY: gettid has no preconditions.
    let own = unsafe { libc::gettid() };
    let peer = PEER.as_raw();
    for refusal in ["D48", "D44"] {
        let signal = if refusal == "D48" {
            libc::SIGSTOP
        } else {
            libc::SIGTERM
        };
        for (name, address, host_tid, errno) in [
            (
                "a write-only mapping",
                write_only_siginfo_page(signal),
                own,
                None,
            ),
            (
                "a PROT_NONE page",
                siginfo_page(signal, libc::SI_QUEUE, 0, libc::PROT_NONE),
                own,
                Some(Errno::EFAULT),
            ),
            (
                "the caller's mappings unreadable",
                write_only_siginfo_page(signal),
                i32::MAX - 1,
                None,
            ),
            // Last, so no later mapping of this row set takes its place.
            (
                "an unmapped page",
                unmapped_page(),
                own,
                Some(Errno::EFAULT),
            ),
        ] {
            let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
            let log = WarnLog::default();
            let _subscriber = tracing::subscriber::set_default(log.clone());
            let mut guest = turn_guest(true, Some(0));
            guest.host_tid = Some(host_tid);
            guest.thread.dettid = DetTid::from_raw(host_tid);
            if refusal == "D44" {
                guest.kernel_run = Some(KernelRunBlock {
                    vfork: false,
                    mask: Some(0),
                });
            }
            let tool = tool(&guest);
            let result = tool
                .handle_rt_tgsigqueueinfo(
                    &mut guest,
                    reverie::syscalls::RtTgsigqueueinfo::new()
                        .with_tgid(peer)
                        .with_tid(peer)
                        .with_sig(signal)
                        .with_siginfo(AddrMut::from_raw(address)),
                )
                .await;
            let wanted = errno.unwrap_or(Errno::EPERM);
            assert!(
                matches!(result, Err(Error::Errno(got)) if got == wanted),
                "{refusal}: {name}: {result:?}"
            );
            let refused = usize::from(errno.is_none());
            let (warns, stops) = (log.0.lock().unwrap().len(), kernel_run_refusals(&guest));
            if refusal == "D48" {
                assert_eq!((warns, stops), (refused, 0), "{refusal}: {name}");
            } else {
                assert_eq!((warns, stops), (0, refused), "{refusal}: {name}");
            }
            assert!(guest.injected.is_empty(), "{refusal}: {name}: nothing sent");
        }
    }
}

mod temporary_mask_turn_tests;
