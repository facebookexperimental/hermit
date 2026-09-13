/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls for dealing with threads and concurrency.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use procfs::process::Process;
use rand::Rng;
use reverie::Error;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Timespec;
use reverie::syscalls::WaitPidFlag;
use tracing::debug;
use tracing::info;
use tracing::trace;

use crate::config::BlockingMode;
use crate::memory::MemoryMetadata;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::ExternalOpId;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::SchedValue;
use crate::syscalls::helpers::record_retry_event;
use crate::syscalls::helpers::retry_nonblocking_syscall;
use crate::syscalls::helpers::retry_nonblocking_syscall_with_timeout;
use crate::syscalls::robust_list;
use crate::tool_global::FutexAction;
use crate::tool_global::ResumeStatus;
use crate::tool_global::await_exact_child_physical_exit;
use crate::tool_global::cancel_exec;
use crate::tool_global::child_tid_clear_address;
use crate::tool_global::consume_child_wait;
use crate::tool_global::create_child_thread;
use crate::tool_global::futex_action;
use crate::tool_global::prepare_exec;
use crate::tool_global::process_group;
use crate::tool_global::ready_child_wait;
use crate::tool_global::resource_request;
use crate::tool_global::set_child_tid_address;
use crate::tool_global::thread_is_live;
use crate::tool_global::thread_observe_time;
use crate::tool_global::wait_for_child_lifecycle;
use crate::tool_global::yield_once;
use crate::tool_local::Detcore;
use crate::tool_local::PendingVfork;
use crate::tool_local::RobustListExit;
use crate::tool_local::RobustListWake;
use crate::types::ChildWaitExitClass;
use crate::types::ChildWaitSelector;
use crate::types::ChildWaitSpec;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::ExactChildWaitState;
use crate::types::LogicalTime;
use crate::types::SigWrapper;

// Preserve the historical Detcore ABI while hiding the host's configured CPU
// count. This represents one virtual CPU in a fixed 128-bit kernel mask.
const VIRTUAL_CPUSET_BYTES: usize = 16;

const IOPRIO_WHO_PROCESS: libc::c_int = 1;
const IOPRIO_WHO_PGRP: libc::c_int = 2;
const IOPRIO_WHO_USER: libc::c_int = 3;
const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
const IOPRIO_CLASS_BE: libc::c_int = 2;
const IOPRIO_BE_NORM: libc::c_int = 4;
const IOPRIO_DEFAULT_EFFECTIVE: libc::c_int =
    (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | IOPRIO_BE_NORM;

// sched_attr wire contract, from include/uapi/linux/sched/types.h. These are
// spelled out rather than derived from `size_of::<libc::sched_attr>()`: the
// value the kernel reports back on the E2BIG paths and the offset past which
// trailing bytes must be zero are properties of the KERNEL's struct, which is
// larger than the libc crate's 48-byte mirror. Deriving either from a Rust type
// would silently re-point the contract if that type ever grows a field.
const SCHED_ATTR_SIZE_VER0: u32 = 48; // first published struct
const SCHED_ATTR_SIZE_VER1: u32 = 56; // adds sched_util_{min,max}
/// The kernel's own `sizeof(struct sched_attr)`. Two things key off it: bytes at
/// or beyond this offset must be zero, and it is the value written back into
/// `uattr->size` when the request is refused with E2BIG.
const SCHED_ATTR_KERNEL_SIZE: u32 = SCHED_ATTR_SIZE_VER1;
/// `sched_copy_attr` refuses a buffer larger than one page. Hermit is x86-64
/// only, where PAGE_SIZE is 4096.
const SCHED_ATTR_MAX_SIZE: u32 = 4096;

/// The scheduling policy Detcore presents as every thread's current one.
///
/// This is not a guess about the host. It is the value `handle_sched_getattr`
/// writes back for every thread, so it is the policy a guest observes and the
/// only policy `SCHED_FLAG_KEEP_POLICY` can be keeping.
const VIRTUAL_CURRENT_POLICY: u32 = libc::SCHED_OTHER as u32;

/// Outcome of scanning the bytes past the kernel's `struct sched_attr`.
#[derive(Debug, Eq, PartialEq)]
enum TailVerdict {
    AllZero,
    /// A byte the kernel does not understand was set: `copy_struct_from_user`
    /// reports this as E2BIG, after storing its own size back.
    NotZeroed,
    /// Guest memory could not be read before any non-zero byte was seen.
    Faulted,
}

/// Reads of eight bytes or fewer take safeptrace's `PTRACE_PEEKDATA` path,
/// which reads a whole aligned word and BYPASSES GUEST PAGE PROTECTIONS. Only a
/// read strictly larger than that reaches `process_vm_readv`, which honours
/// them. Note this is not symmetric with the write side: `write` special-cases
/// a length of exactly eight, so splitting a write in half escapes it, while
/// `read` special-cases eight *or fewer* and splitting only makes it worse.
const MIN_PROTECTION_RESPECTING_READ: usize = std::mem::size_of::<u64>() + 1;

/// Scan `tail_len` bytes at `base + tail_off` the way `check_zeroed_user` does.
///
/// Two things here are load-bearing and neither is visible from the outcome
/// alone, only from the ORDER the outcome is reached in.
///
/// First, Linux scans FORWARD and stops at the first thing it finds. A non-zero
/// byte followed later by an unreadable page is E2BIG, because the scan never
/// reaches the page; an unreadable page reached before any non-zero byte is
/// EFAULT. Reading the whole tail up front and judging afterwards collapses
/// both into EFAULT and silently changes the errno a guest sees.
///
/// Second, every read here is kept strictly larger than eight bytes, for the
/// reason on [`MIN_PROTECTION_RESPECTING_READ`]. Where the remainder is
/// smaller, the window is extended BACKWARDS over bytes already scanned rather
/// than shortened; re-reading a byte is free and only the new bytes are judged.
fn scan_tail_is_zeroed<M: MemoryAccess>(
    memory: &M,
    base: AddrMut<u8>,
    tail_off: usize,
    tail_len: usize,
) -> TailVerdict {
    const CHUNK: usize = 256;
    let mut done = 0usize;
    let mut buf = [0u8; CHUNK];
    while done < tail_len {
        let remaining = tail_len - done;
        let want = remaining.min(CHUNK);
        // Extend backwards when the remainder alone would be a small read.
        // The window may reach back past the start of the tail into the
        // `sched_attr` prefix: the kernel has already read those bytes, so they
        // are readable, and only the new bytes are judged below. Clamping this
        // to `done` alone left a 1-byte tail with nowhere to grow into and
        // issued exactly the small read this constant exists to avoid.
        let back = MIN_PROTECTION_RESPECTING_READ
            .saturating_sub(want)
            .min(done + tail_off);
        let read_len = want + back;
        let start = tail_off + done - back;
        let Some(addr) = base
            .as_raw()
            .checked_add(start)
            .and_then(AddrMut::<u8>::from_raw)
        else {
            return TailVerdict::Faulted;
        };
        // A failed read means zero NEW bytes were obtained; the `got < read_len`
        // check below is what turns that into `Faulted`. Written `unwrap_or(0)`
        // rather than `unwrap_or_default()` so the 0 stays visible.
        let got = memory.read(addr, &mut buf[..read_len]).unwrap_or(0);
        // Judge only the bytes that are genuinely new AND genuinely read.
        let new_from = back.min(got);
        if buf[new_from..got].iter().any(|byte| *byte != 0) {
            return TailVerdict::NotZeroed;
        }
        if got < read_len {
            // The scan stopped at an unreadable byte with nothing non-zero
            // before it, which is `check_zeroed_user`'s -EFAULT.
            return TailVerdict::Faulted;
        }
        done += want;
    }
    TailVerdict::AllZero
}

// Scheduling policies, from include/uapi/linux/sched.h.
const SCHED_FIFO: u32 = 1;
const SCHED_RR: u32 = 2;
const SCHED_DEADLINE: u32 = 6;
const SCHED_EXT: u32 = 7;
/// The kernel's `MAX_RT_PRIO`; valid real-time priorities are 1..=99.
const MAX_RT_PRIO: u32 = 100;

// Byte offsets of the fields this handler inspects. Taken from the UAPI struct
// rather than from a Rust mirror, for the reason given above.
const SCHED_ATTR_OFF_POLICY: usize = 4;
const SCHED_ATTR_OFF_FLAGS: usize = 8;
const SCHED_ATTR_OFF_PRIORITY: usize = 20;
const SCHED_ATTR_OFF_RUNTIME: usize = 24;
const SCHED_ATTR_OFF_DEADLINE: usize = 32;
const SCHED_ATTR_OFF_PERIOD: usize = 40;

// sched_attr.sched_flags bits, from include/uapi/linux/sched.h.
const SCHED_FLAG_RESET_ON_FORK: u64 = 0x01;
const SCHED_FLAG_RECLAIM: u64 = 0x02;
const SCHED_FLAG_DL_OVERRUN: u64 = 0x04;
const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;
const SCHED_FLAG_UTIL_CLAMP_MAX: u64 = 0x40;
const SCHED_FLAG_UTIL_CLAMP: u64 = SCHED_FLAG_UTIL_CLAMP_MIN | SCHED_FLAG_UTIL_CLAMP_MAX;
const SCHED_FLAG_ALL: u64 = SCHED_FLAG_RESET_ON_FORK
    | SCHED_FLAG_RECLAIM
    | SCHED_FLAG_DL_OVERRUN
    | SCHED_FLAG_KEEP_POLICY
    | SCHED_FLAG_KEEP_PARAMS
    | SCHED_FLAG_UTIL_CLAMP;

/// Whether Linux accepts `policy` as a scheduling policy for `sched_setattr`.
/// This is the kernel's `valid_policy()`: idle, fair, rt, deadline or ext. Note
/// the gap at 4 -- SCHED_ISO is reserved and never valid -- and that 7 is
/// SCHED_EXT, which `valid_policy()` accepts on a kernel built with
/// CONFIG_SCHED_CLASS_EXT.
fn is_valid_sched_policy(policy: u32) -> bool {
    matches!(
        policy,
        // SCHED_OTHER/NORMAL, FIFO, RR, BATCH
        0 | 1 | 2 | 3
        // SCHED_IDLE
        | 5
        | SCHED_DEADLINE
        | SCHED_EXT
    )
}

/// The kernel's `rt_policy()`: the two fixed-priority real-time policies.
fn is_rt_policy(policy: u32) -> bool {
    policy == SCHED_FIFO || policy == SCHED_RR
}

/// The kernel's `__checkparam_dl()`, restricted to the parts that are pure ABI.
///
/// The kernel's final test compares the period against
/// `sysctl_sched_dl_period_{min,max}`, which are runtime-tunable. That bound is
/// a property of the host's configuration rather than of the interface, so it
/// is deliberately not reproduced here -- see the handler's doc comment for the
/// other cases in that class.
fn deadline_params_are_valid(runtime: u64, deadline: u64, period: u64) -> bool {
    // deadline != 0
    if deadline == 0 {
        return false;
    }
    // The kernel truncates DL_SCALE (10) bits, so runtime must be at least that
    // big to survive the truncation.
    if runtime < (1u64 << 10) {
        return false;
    }
    // The MSB is reserved for wrap-around and sign handling.
    if deadline & (1u64 << 63) != 0 || period & (1u64 << 63) != 0 {
        return false;
    }
    // A zero period means "same as the deadline".
    let period = if period == 0 { deadline } else { period };
    // runtime <= deadline <= period
    runtime <= deadline && deadline <= period
}

/// Apply `sched_copy_attr`'s size rules to the `size` the guest declared.
///
/// `Ok(n)` is the effective size to copy with; `Err(())` means the request is
/// refused with E2BIG *and* the kernel first writes its own struct size back
/// into `uattr->size`, so the caller owes that store.
///
/// The zero case is not a mistake: the kernel carries an explicit ABI
/// compatibility quirk, `if (!size) size = SCHED_ATTR_SIZE_VER0;`, so a
/// zero-sized request is a well-formed VER0 request and succeeds.
fn sched_attr_effective_size(declared: u32) -> Result<u32, ()> {
    let size = if declared == 0 {
        SCHED_ATTR_SIZE_VER0
    } else {
        declared
    };
    if !(SCHED_ATTR_SIZE_VER0..=SCHED_ATTR_MAX_SIZE).contains(&size) {
        return Err(());
    }
    Ok(size)
}

/// The descriptor fields this handler inspects, decoded from the guest's bytes.
#[derive(Clone, Copy, Debug)]
struct SchedAttrFields {
    policy: u32,
    sched_flags: u64,
    priority: u32,
    runtime: u64,
    deadline: u64,
    period: u64,
}

/// The checks that run in `sched_copy_attr` and in the `sched_setattr` wrapper,
/// i.e. everything the kernel decides *before* it looks the target pid up.
///
/// `size` is the effective size from [`sched_attr_effective_size`], so the
/// util-clamp rule can be stated in the same terms the kernel uses.
///
/// Measured: with a pid that does not exist, both of these still report their
/// own errno rather than ESRCH, which is what places them on this side of the
/// lookup.
fn validate_sched_attr_before_lookup(size: u32, attr: &SchedAttrFields) -> Result<(), Errno> {
    // `sched_copy_attr`: util-clamp lives past the VER0 tail, so asking for it
    // with a VER0 buffer is incoherent.
    if attr.sched_flags & SCHED_FLAG_UTIL_CLAMP != 0 && size < SCHED_ATTR_SIZE_VER1 {
        return Err(Errno::EINVAL);
    }
    // `sched_setattr`: the policy is compared as a *signed* int, and this test
    // sits above the KEEP_POLICY substitution below, so the sign bit is refused
    // even when the policy field is otherwise ignored.
    if (attr.policy as i32) < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// The checks inside `__sched_setscheduler`, i.e. everything the kernel decides
/// *after* it has resolved the target pid.
///
/// Measured: with a pid that does not exist, every rule here reports ESRCH
/// instead, which is what places them on this side of the lookup.
fn validate_sched_attr_after_lookup(attr: &SchedAttrFields) -> Result<(), Errno> {
    // `sched_setattr` rewrites the policy to SETPARAM_POLICY (-1) when
    // KEEP_POLICY is set, and `__sched_setscheduler` then takes its
    // `policy < 0` branch. That branch does not skip the policy-dependent
    // rules -- it REUSES THE TASK'S CURRENT POLICY and applies them against
    // that. `valid_policy()` is the only thing it skips, which is why an
    // undefined value in the ignored field still passes.
    //
    // Detcore's current policy is not unknown: `handle_sched_getattr` reports
    // SCHED_OTHER, nice 0, priority 0 for every thread, unconditionally, and
    // that is the whole of the guest-visible scheduling state this sandbox
    // exposes. So KEEP_POLICY substitutes SCHED_OTHER here. Treating it as
    // "no policy" and skipping the rules accepted, for instance,
    // KEEP_POLICY with sched_priority=1, which Linux refuses because a
    // non-real-time current policy requires priority 0.
    //
    // These two sites must stay in step: if `handle_sched_getattr` ever
    // reports a different virtual policy, this substitution follows it.
    let policy = if attr.sched_flags & SCHED_FLAG_KEEP_POLICY != 0 {
        Some(VIRTUAL_CURRENT_POLICY)
    } else {
        if !is_valid_sched_policy(attr.policy) {
            return Err(Errno::EINVAL);
        }
        Some(attr.policy)
    };
    // No undefined sched_flags bits.
    if attr.sched_flags & !SCHED_FLAG_ALL != 0 {
        return Err(Errno::EINVAL);
    }
    // Valid priorities are 1..=MAX_RT_PRIO-1 for the real-time policies and
    // exactly 0 for every other policy, so the priority and the policy have to
    // agree; the kernel states that as `rt_policy(policy) != (prio != 0)`.
    if attr.priority > MAX_RT_PRIO - 1 {
        return Err(Errno::EINVAL);
    }
    if let Some(policy) = policy {
        if is_rt_policy(policy) != (attr.priority != 0) {
            return Err(Errno::EINVAL);
        }
        if policy == SCHED_DEADLINE
            && !deadline_params_are_valid(attr.runtime, attr.deadline, attr.period)
        {
            return Err(Errno::EINVAL);
        }
    }
    Ok(())
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-881)
fn virtual_ioprio(which: libc::c_int) -> Result<i64, Errno> {
    match which {
        IOPRIO_WHO_PROCESS => Ok(0),
        IOPRIO_WHO_PGRP | IOPRIO_WHO_USER => Ok(i64::from(IOPRIO_DEFAULT_EFFECTIVE)),
        _ => Err(Errno::EINVAL),
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct WaitidSigchldFields {
    pid: libc::pid_t,
    uid: libc::uid_t,
    status: libc::c_int,
    utime: libc::c_long,
    stime: libc::c_long,
}

#[repr(C)]
union WaitidSiginfoFields {
    _alignment: *mut libc::c_void,
    sigchld: WaitidSigchldFields,
}

#[repr(C)]
struct WaitidSiginfoHead {
    _base: [libc::c_int; 3],
    fields: WaitidSiginfoFields,
}

fn wait_status_is_termination(status: libc::c_int) -> bool {
    libc::WIFEXITED(status) || libc::WIFSIGNALED(status)
}

fn waitid_code_is_termination(code: libc::c_int) -> bool {
    matches!(code, libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED)
}

fn canonicalize_waitid_siginfo(info: &mut libc::siginfo_t) {
    debug_assert!(
        std::mem::size_of::<WaitidSiginfoHead>() <= std::mem::size_of::<libc::siginfo_t>()
    );
    // SAFETY: Linux siginfo_t starts with three c_int fields followed by a
    // pointer-aligned union. Its SIGCHLD member is pid, uid, status, utime,
    // and stime in that order. The local repr(C) mirror changes only the two
    // host CPU-accounting fields and preserves the kernel-populated event.
    let sigchld = unsafe {
        &mut (*(info as *mut libc::siginfo_t).cast::<WaitidSiginfoHead>())
            .fields
            .sigchld
    };
    sigchld.utime = 0;
    sigchld.stime = 0;
}

fn finish_waitid_result<T, G>(
    guest: &mut G,
    call: syscalls::Waitid,
    value: i64,
    mut info_value: libc::siginfo_t,
) -> Result<i64, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    // SAFETY: waitid writes either zeroed output or the SIGCHLD siginfo_t
    // variant, for which libc exposes si_pid.
    let child_pid = unsafe { info_value.si_pid() };
    if child_pid != 0 {
        canonicalize_waitid_siginfo(&mut info_value);
        guest.memory().write_value(
            call.info().expect("waitid infop checked before execution"),
            &info_value,
        )?;
        if call.options() & libc::WNOWAIT == 0 && waitid_code_is_termination(info_value.si_code) {
            guest
                .thread_state_mut()
                .reap_child_process_cpu_time(DetPid::from_raw(child_pid));
        }
        if let Some(rusage) = call.rusage() {
            // Host CPU and scheduling counters are not deterministic.
            let usage: libc::rusage = unsafe { std::mem::zeroed() };
            guest.memory().write_value(rusage, &usage)?;
        }
    }
    Ok(value)
}

#[derive(Debug, Eq, PartialEq)]
enum ExactWaitPollDecision {
    ChildReady,
    AwaitPhysicalExit,
    ReapAfterLogicalExit,
    Interrupted,
    Retry,
}

fn exact_wait_poll_decision(
    child_ready: bool,
    signaled: bool,
    lifecycle: Option<ExactChildWaitState>,
) -> ExactWaitPollDecision {
    if child_ready {
        ExactWaitPollDecision::ChildReady
    } else if lifecycle == Some(ExactChildWaitState::PhysicalExitPending) {
        ExactWaitPollDecision::AwaitPhysicalExit
    } else if matches!(
        lifecycle,
        Some(ExactChildWaitState::LogicallyExited | ExactChildWaitState::PhysicallyExited)
    ) {
        ExactWaitPollDecision::ReapAfterLogicalExit
    } else if signaled {
        ExactWaitPollDecision::Interrupted
    } else {
        ExactWaitPollDecision::Retry
    }
}

fn stale_any_wait_must_interrupt(signaled: bool, next_ready: Option<DetPid>) -> bool {
    signaled && next_ready.is_none()
}

fn terminal_child_wait_spec(
    selector: ChildWaitSelector,
    caller: DetTid,
    options: libc::c_int,
) -> ChildWaitSpec {
    let exit_class = if options & libc::__WALL != 0 {
        ChildWaitExitClass::Any
    } else if options & libc::__WCLONE != 0 {
        ChildWaitExitClass::Clone
    } else {
        ChildWaitExitClass::Sigchld
    };
    ChildWaitSpec {
        selector,
        owner: (options & libc::__WNOTHREAD != 0).then_some(caller),
        exit_class,
    }
}

fn child_wait_can_retry_after_stale(spec: ChildWaitSpec) -> bool {
    !matches!(spec.selector, ChildWaitSelector::Exact(_))
}

pub(super) type KernelSigset = u64;
pub(super) const KERNEL_SIGSET_SIZE: usize = std::mem::size_of::<KernelSigset>();

fn signal_is_blocked(mask: &KernelSigset, signal: SigWrapper) -> bool {
    let raw_signal = signal.raw();
    (1..=KernelSigset::BITS as i32).contains(&raw_signal)
        && mask & (1_u64 << (raw_signal as u32 - 1)) != 0
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct KernelSigaction {
    pub(super) handler: u64,
    pub(super) flags: u64,
    pub(super) restorer: u64,
    pub(super) mask: KernelSigset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WaitSignalDisposition {
    Interrupt,
    Restart,
}

fn signal_default_disposition_does_not_interrupt_child_wait(signal: SigWrapper) -> bool {
    matches!(
        signal.raw(),
        libc::SIGCHLD | libc::SIGCONT | libc::SIGURG | libc::SIGWINCH
    )
}

fn signal_has_uncatchable_default_disposition(signal: SigWrapper) -> bool {
    matches!(signal.raw(), libc::SIGKILL | libc::SIGSTOP)
}

pub(super) async fn wait_signal_disposition<G, T>(
    guest: &mut G,
    status: ResumeStatus,
    guest_signal_mask: &KernelSigset,
    action_addr: AddrMut<'_, KernelSigaction>,
    inspect_action: bool,
) -> Result<Option<WaitSignalDisposition>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let ResumeStatus::Signaled(signals) = status else {
        return Ok(None);
    };
    let Some(mut signals) = signals else {
        return Ok(Some(WaitSignalDisposition::Interrupt));
    };
    signals.sort_by_key(|signal| signal.raw());
    for signal in signals {
        if signal_is_blocked(guest_signal_mask, signal) {
            continue;
        }
        if signal_has_uncatchable_default_disposition(signal) {
            return Ok(Some(WaitSignalDisposition::Interrupt));
        }
        if !inspect_action {
            return Ok(Some(WaitSignalDisposition::Interrupt));
        }
        let call = syscalls::RtSigaction::new()
            .with_signum(signal.raw())
            .with_action(None)
            .with_old_action(Some(action_addr.cast()))
            .with_sigsetsize(std::mem::size_of::<u64>());
        guest.inject_with_retry(call).await?;
        let action: KernelSigaction = guest.memory().read_value(action_addr)?;
        if action.handler == libc::SIG_IGN as u64
            || action.handler == libc::SIG_DFL as u64
                && signal_default_disposition_does_not_interrupt_child_wait(signal)
        {
            continue;
        }
        return Ok(Some(
            if action.handler != libc::SIG_DFL as u64 && action.flags & libc::SA_RESTART as u64 != 0
            {
                WaitSignalDisposition::Restart
            } else {
                WaitSignalDisposition::Interrupt
            },
        ));
    }
    Ok(None)
}

pub(super) fn blocked_signal_mask() -> KernelSigset {
    // Preserve libc's definition of the blockable set (notably its reserved
    // NPTL signals) while converting the result to the kernel's one-word ABI.
    let mut libc_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigfillset(&mut libc_mask);
        libc::sigdelset(&mut libc_mask, reverie::PERF_EVENT_SIGNAL as i32);
    }
    (1..=KernelSigset::BITS as i32).fold(0, |mask, raw_signal| {
        if unsafe { libc::sigismember(&libc_mask, raw_signal) } == 1 {
            mask | (1_u64 << (raw_signal as u32 - 1))
        } else {
            mask
        }
    })
}

pub(super) async fn block_signals_for_disposition<G, T>(
    guest: &mut G,
    blocked_mask_addr: Addr<'_, KernelSigset>,
    old_mask_addr: AddrMut<'_, KernelSigset>,
) -> Result<KernelSigset, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let block_signals = syscalls::RtSigprocmask::new()
        .with_how(libc::SIG_SETMASK)
        .with_set(
            (!guest
                .config()
                .backend_requires_thread_directed_process_signals)
                .then_some(blocked_mask_addr.cast()),
        )
        .with_oldset(Some(old_mask_addr.cast()))
        .with_sigsetsize(KERNEL_SIGSET_SIZE);
    guest.inject_with_retry(block_signals).await?;
    Ok(guest.memory().read_value(old_mask_addr)?)
}

pub(super) async fn restore_signals_after_disposition<G, T>(
    guest: &mut G,
    old_mask_addr: AddrMut<'_, KernelSigset>,
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if !guest
        .config()
        .backend_requires_thread_directed_process_signals
    {
        let old_mask: Addr<'_, KernelSigset> = old_mask_addr.into();
        let restore_signals = syscalls::RtSigprocmask::new()
            .with_how(libc::SIG_SETMASK)
            .with_set(Some(old_mask.cast()))
            .with_oldset(None)
            .with_sigsetsize(KERNEL_SIGSET_SIZE);
        guest.inject_with_retry(restore_signals).await?;
    }
    Ok(())
}

async fn interrupted_child_wait_result<G, T, S>(
    guest: &mut G,
    call: S,
    disposition: WaitSignalDisposition,
) -> Result<i64, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
    S: SyscallInfo,
{
    if !guest
        .config()
        .backend_requires_thread_directed_process_signals
    {
        return Err(Errno::ERESTARTSYS.into());
    }
    if disposition == WaitSignalDisposition::Interrupt {
        return Err(Errno::EINTR.into());
    }

    guest.tail_inject(call).await
}

fn snapshot_process_group(pid: Pid) -> Result<libc::pid_t, Errno> {
    let pgrp = Process::new(pid.as_raw())
        .and_then(|process| process.stat())
        .map(|stat| stat.pgrp)
        .map_err(|_| Errno::ESRCH)?;
    if pgrp == 0 {
        Err(Errno::EOPNOTSUPP)
    } else {
        Ok(pgrp)
    }
}

fn guest_fd_status_flags(pid: Pid, fd: libc::c_int) -> Result<libc::c_int, Errno> {
    let path = format!("/proc/{}/fdinfo/{}", pid.as_raw(), fd);
    let contents = std::fs::read_to_string(path).map_err(|_| Errno::EBADF)?;
    let flags = contents
        .lines()
        .find_map(|line| line.strip_prefix("flags:"))
        .map(str::trim)
        .ok_or(Errno::EINVAL)?;
    libc::c_int::from_str_radix(flags, 8).map_err(|_| Errno::EINVAL)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FutexTimeout {
    Relative(u64),
    Absolute(LogicalTime),
}

fn parse_futex_timeout(futex_op: i32, timeout: Timespec) -> Result<FutexTimeout, Errno> {
    let seconds = u64::try_from(timeout.tv_sec).map_err(|_| Errno::EINVAL)?;
    let nanoseconds = u64::try_from(timeout.tv_nsec).map_err(|_| Errno::EINVAL)?;
    if nanoseconds >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }

    let timeout_nanos = seconds
        .checked_mul(1_000_000_000)
        .and_then(|nanos| nanos.checked_add(nanoseconds))
        .ok_or(Errno::EINVAL)?;
    // Mask off FUTEX_PRIVATE_FLAG / FUTEX_CLOCK_REALTIME before matching the
    // command: FUTEX_WAIT_BITSET measures its timeout as an *absolute* deadline,
    // whereas plain FUTEX_WAIT uses a *relative* one. A private-flagged
    // FUTEX_WAIT_BITSET (e.g. 0x89) must still be recognized as the BITSET
    // command; comparing the raw op would misclassify it as relative and add
    // the absolute deadline to the current time (leaking the epoch).
    if futex_op & libc::FUTEX_CMD_MASK == libc::FUTEX_WAIT_BITSET {
        Ok(FutexTimeout::Absolute(LogicalTime::from_nanos(
            timeout_nanos,
        )))
    } else {
        Ok(FutexTimeout::Relative(timeout_nanos))
    }
}

fn rebase_absolute_timeout(
    deadline: LogicalTime,
    clock_now: LogicalTime,
    logical_now: LogicalTime,
) -> LogicalTime {
    logical_now + Duration::from_nanos(deadline.as_nanos().saturating_sub(clock_now.as_nanos()))
}

fn absolute_timeout_uses_host_clock(
    deadline: LogicalTime,
    host_clock_now: LogicalTime,
    logical_now: LogicalTime,
) -> bool {
    deadline.as_nanos().abs_diff(host_clock_now.as_nanos())
        < deadline.as_nanos().abs_diff(logical_now.as_nanos())
}

impl<T: RecordOrReplay> Detcore<T> {
    async fn futex_timeout_deadline<G: Guest<Self>>(
        &self,
        guest: &mut G,
        futex_flags: i32,
        timeout: Option<Addr<'_, Timespec>>,
    ) -> Result<Option<LogicalTime>, Error> {
        let Some(timeout) = timeout else {
            return Ok(None);
        };
        let timeout = parse_futex_timeout(futex_flags, guest.memory().read_value(timeout)?)?;
        match timeout {
            FutexTimeout::Relative(nanos) => {
                let now = thread_observe_time(guest).await;
                Ok(Some(now + Duration::from_nanos(nanos)))
            }
            FutexTimeout::Absolute(deadline)
                if self.cfg.virtualize_time && !self.cfg.detect_host_clock_futex_timeouts =>
            {
                Ok(Some(deadline))
            }
            FutexTimeout::Absolute(deadline) => {
                let clockid = if futex_flags & libc::FUTEX_CLOCK_REALTIME != 0 {
                    syscalls::ClockId::CLOCK_REALTIME
                } else {
                    syscalls::ClockId::CLOCK_MONOTONIC
                };

                let mut stack = guest.stack().await;
                let clock_output = syscalls::TimespecMutPtr(stack.reserve());
                let _stack_guard = stack.commit()?;
                let clock_call = syscalls::ClockGettime::new()
                    .with_clockid(clockid)
                    .with_tp(Some(clock_output));
                if self.cfg.virtualize_time && self.cfg.detect_host_clock_futex_timeouts {
                    // Read the same live host clock as a direct guest vDSO call. Replaying a
                    // recorded value here would compare this run's host-domain deadline with the
                    // previous run's clock and turn a short timeout into an arbitrary long one.
                    guest.inject(Syscall::from(clock_call)).await?;
                } else {
                    self.record_or_replay(guest, clock_call).await?;
                }
                let clock_now = match parse_futex_timeout(
                    libc::FUTEX_WAIT_BITSET,
                    guest.memory().read_value(clock_output.0)?,
                )? {
                    FutexTimeout::Absolute(time) => time,
                    FutexTimeout::Relative(_) => unreachable!(),
                };
                let logical_now = thread_observe_time(guest).await;
                if self.cfg.virtualize_time
                    && !absolute_timeout_uses_host_clock(deadline, clock_now, logical_now)
                {
                    return Ok(Some(deadline));
                }
                Ok(Some(rebase_absolute_timeout(
                    deadline,
                    clock_now,
                    logical_now,
                )))
            }
        }
    }

    /// Clone, clone3, fork, vfork system calls
    pub async fn handle_clone_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        clone_family: syscalls::family::CloneFamily,
    ) -> Result<i64, Error> {
        let flags = clone_family.flags(&guest.memory());
        let exit_signal = match clone_family {
            #[cfg(not(target_arch = "aarch64"))]
            syscalls::family::CloneFamily::Fork(_) | syscalls::family::CloneFamily::Vfork(_) => {
                libc::SIGCHLD
            }
            syscalls::family::CloneFamily::Clone(clone) => {
                (clone.flags().bits() & 0xff) as libc::c_int
            }
            syscalls::family::CloneFamily::Clone3(clone) => clone
                .args()
                .and_then(|address| guest.memory().read_value(address).ok())
                .map_or(0, |args: syscalls::CloneArgs| {
                    args.exit_signal as libc::c_int
                }),
        };
        let ctid = child_tid_clear_address(flags, clone_family.child_tid(&guest.memory()));
        let is_vfork = flags.contains(CloneFlags::CLONE_VFORK);
        let parent_blocks_for_child = is_vfork
            || (self.cfg.backend_serializes_fork_children
                && !flags.contains(CloneFlags::CLONE_THREAD));
        let backend_uninstrumented_thread =
            flags.contains(CloneFlags::CLONE_THREAD) && !self.cfg.backend_dispatches_thread_tools;

        let ts = guest.thread_state_mut();
        assert_eq!(ts.clone_flags, None);
        assert!(ts.pending_vfork.is_none());
        ts.clone_flags = Some(flags);

        let parent_dettid = ts.dettid;
        let child_priority_entropy = if parent_blocks_for_child
            && self.cfg.chaos
            && self.cfg.replay_preemptions_from.is_none()
            && self.cfg.replay_schedule_from.is_none()
        {
            let mut parent_chaos_prng = ts.chaos_prng.clone();
            Some(parent_chaos_prng.next_u64())
        } else {
            None
        };
        if parent_blocks_for_child {
            ts.pending_vfork = Some(PendingVfork {
                parent_dettid,
                parent_detpid: ts.detpid.expect("detpid unset"),
                child_tid_addr: ctid,
                flags,
                exit_signal,
                child_priority_entropy,
            });
        }

        trace!("[detcore, dtid {}] parent invoking clone.", parent_dettid);
        let blocking_child_op_id =
            ExternalOpId::new(parent_dettid, guest.thread_state().stats.syscall_count);

        // A CLONE_VFORK parent and a backend-serialized fork parent cannot
        // resume until the child exits. Relinquish the parent's scheduler turn
        // before entering either blocking operation.
        if parent_blocks_for_child && self.cfg.sequentialize_threads {
            let mut resources = Resources::new(parent_dettid);
            resources.insert(
                ResourceID::BlockingVfork(blocking_child_op_id),
                Permission::RW,
            );
            resources.fyi(if is_vfork {
                "clone_vfork"
            } else {
                "clone_serialized_child"
            });
            resource_request(guest, resources).await;
        }

        let maybe_res = guest.inject(Syscall::from(clone_family)).await;

        if parent_blocks_for_child && self.cfg.sequentialize_threads {
            let mut resources = Resources::new(parent_dettid);
            if maybe_res.is_err() {
                // TODO-HUMAN-REVIEW(PR-1152): Review failed deferred-vfork cancellation.
                // A deferred-spawn backend cannot infer failure from the absence of a registered
                // child: a successful child also registers after this continuation. Report the
                // known injected-syscall outcome explicitly so the scheduler can cancel the
                // barrier and re-admit this parent before we propagate the original error below.
                resources.insert(
                    ResourceID::VforkFailed(blocking_child_op_id),
                    Permission::RW,
                );
                resources.fyi(if is_vfork {
                    "clone_vfork_failed"
                } else {
                    "clone_serialized_child_failed"
                });
            } else {
                resources.insert(
                    ResourceID::BlockedExternalContinue(blocking_child_op_id),
                    Permission::RW,
                );
                resources.fyi(if is_vfork {
                    "clone_vfork"
                } else {
                    "clone_serialized_child"
                });
            }
            resource_request(guest, resources).await;
        }

        let ts = guest.thread_state_mut();
        ts.clone_flags = None; // Unset, now that it has been read by the child.
        ts.pending_vfork = None;

        let res = maybe_res?;

        if !flags.contains(CloneFlags::CLONE_THREAD) {
            // Only a successful process clone can let another process mutate
            // inherited open file descriptions. Failed clone-family calls leave
            // the previously observed flock state authoritative.
            guest.thread_state().forget_flock_modes();
        }

        // Match ordinary clone: the parent consumes the priority entropy after
        // the child has inherited the parent state.
        if parent_blocks_for_child
            && self.cfg.chaos
            && self.cfg.replay_preemptions_from.is_none()
            && self.cfg.replay_schedule_from.is_none()
        {
            let _ = guest
                .thread_state_mut()
                .chaos_prng_next_u64("child_priority");
        }

        let child_tid = Pid::from_raw(res as i32);
        let child_dettid = DetTid::from_raw(child_tid.into()); // TODO(T78538674), virtualized tid/pid
        trace!(
            "[detcore] dtid {} cloned, continuing parent + register new thread.",
            child_dettid
        );

        if !parent_blocks_for_child && !backend_uninstrumented_thread {
            create_child_thread(guest, child_dettid, ctid, Some(flags), exit_signal, None).await;
        }

        {
            // The child will have updated their pedigree, we update ours before continuing.
            let parent_pedigree = &mut guest.thread_state_mut().pedigree;
            let child_pedigree = parent_pedigree.fork_mut();
            debug!(
                "[dtid {}] after creating child thread (tid {}, pedigree {}) parents pedigree becomes {}",
                parent_dettid, child_dettid, child_pedigree, parent_pedigree,
            );
        }

        Ok(child_dettid.as_raw() as i64)
    }

    // TODO-HUMAN-REVIEW(PR-2985): Review scheduler tracking of set_tid_address.
    /// `set_tid_address` system call.
    ///
    /// Linux owns the guest-visible registration and return value. Detcore
    /// mirrors the accepted address into the scheduler so its modeled
    /// CHILD_CLEARTID wake targets the same word as the backend.
    pub async fn handle_set_tid_address<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SetTidAddress,
    ) -> Result<i64, Error> {
        let address = call.tidptr().map_or(0, |pointer| pointer.as_raw());
        let result = self
            .record_or_replay(guest, Syscall::SetTidAddress(call))
            .await?;
        if guest.config().sequentialize_threads {
            set_child_tid_address(guest, address).await;
        }
        trace!(
            "[detcore, dtid {}] child TID clear address registered: {address:#x}",
            guest.thread_state().dettid,
        );
        Ok(result)
    }

    /// `set_robust_list` system call.
    ///
    /// Still a pass-through: Linux owns the registration and supplies the
    /// result, and Detcore only records the head address so it can replay
    /// `exit_robust_list()` when the thread dies (see
    /// `Self::run_robust_list_owner_death`). Recording happens only after the
    /// kernel accepts the call, so a rejected length or address never becomes
    /// Detcore state.
    ///
    /// The call goes through `record_or_replay`, like every other pass-through.
    /// Using `Guest::inject` directly would keep the classification but drop the
    /// behavior it implies: the syscall would vanish from a `hermit record`
    /// trace, and a log recorded by a build that did record it would no longer
    /// replay.
    // AUTONOMOUS-BOT-IMPLEMENTED
    pub async fn handle_set_robust_list<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SetRobustList,
    ) -> Result<i64, Error> {
        let head = call.head().map(AddrMut::as_raw);
        let len = call.len();
        let res = self
            .record_or_replay(guest, Syscall::SetRobustList(call))
            .await?;
        // TODO-HUMAN-REVIEW(PR-2223): Review robust-list
        // head tracking used to drive owner-death wakeups.
        let recorded = match head {
            // `set_robust_list(NULL, ...)` unregisters the list.
            None => None,
            // Fail closed, and not dead code: `len` is the guest's own argument,
            // so a 32-bit guest (or a bug) can present the 12-byte
            // `compat_robust_list_head`, whose fields sit at different offsets.
            // The 64-bit walk would misread it, so refuse to record it at all
            // rather than walk a layout we cannot parse.
            Some(_) if len != robust_list::ROBUST_LIST_HEAD_LEN => None,
            Some(addr) => Some(addr),
        };
        guest.thread_state_mut().record_robust_list_head(recorded);
        trace!(
            "[detcore, dtid {}] robust-list head registered: {:?}",
            guest.thread_state().dettid,
            recorded,
        );
        Ok(res)
    }

    /// Replay Linux's `exit_robust_list()` for the calling thread.
    ///
    /// Linux runs this from `mm_release()` while a task dies. Detcore performs
    /// the same walk and issues the wake against its own modeled waiter pool,
    /// which is where precise-mode futex waiters actually live. Ptrace leaves
    /// the atomic word transition to Linux; backends without that ordering do
    /// not enter this path. The algorithm is in
    /// [`crate::syscalls::robust_list`]; this function only supplies guest
    /// memory and the scheduler wake.
    ///
    /// Only the precise futex model needs this. Polling and external modes park
    /// waiters in the host kernel, which performs its own robust-list cleanup;
    /// and without thread sequentialization Detcore does not model futexes at
    /// all.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-2223): Review owner-death wakeup
    // emulation, which changes how a dying thread's peers are scheduled.
    async fn run_robust_list_owner_death<G: Guest<Self>>(&self, guest: &mut G) {
        if !self.cfg.backend_runs_exit_robust_list
            || !self.cfg.sequentialize_threads
            || self.cfg.debug_futex_mode != BlockingMode::Precise
        {
            return;
        }
        let Some(head) = guest.thread_state().robust_list_head else {
            return;
        };
        let dettid = guest.thread_state().dettid;
        // Which TID goes in the comparison? Not one Detcore hands out: `gettid`
        // is a reviewed pass-through, and glibc puts the value the kernel
        // supplied through `CLONE_PARENT_SETTID` into the lock word, not a
        // `gettid` result. The comparison is valid for a narrower reason —
        // `DetTid` currently *is* the raw namespaced `Tid`. `init_thread_state`
        // builds every one with `DetPid::from_raw(tid.into())` (detcore/src/lib.rs,
        // `lib.rs:1257` at this commit), four lines under
        // `// TODO(T78538674): virtualize tid, extend tid<=>dettid mapping here.`
        // Completing that TODO breaks this comparison, which would then have to
        // map back to the guest-visible TID.
        // `hermit-cli/tests/robust_futex_owner_death.rs` is the tripwire: it
        // fails the moment the two diverge.
        let owner_tid = dettid.as_raw() as u32;

        let mut effects = GuestRobustEffects::<'_, G, T> {
            guest,
            dettid,
            defer_owner_death_to_backend: true,
            staged_wakes: None,
            tool: PhantomData,
        };
        let outcome = robust_list::exit_robust_list(&mut effects, head, owner_tid).await;
        if outcome.head_unreadable {
            trace!(
                "[detcore, dtid {}] unreadable robust-list head at {:#x}; no owner-death wakeups",
                dettid, head,
            );
        } else if outcome.aborted || outcome.next_faulted || outcome.truncated {
            trace!(
                "[detcore, dtid {}] robust-list walk stopped early after {} entr(ies): {:?}",
                dettid, outcome.entries_visited, outcome,
            );
        }
    }

    /// Read every registered robust list in the current thread group while
    /// its address space still exists, but hold its modeled wakes until the
    /// corresponding ptrace exit callback confirms Linux has completed the
    /// atomic owner-word update.
    pub(crate) async fn stage_thread_group_robust_list_wakes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        reason: RobustListExit,
    ) {
        if !self.cfg.backend_runs_exit_robust_list
            || !self.cfg.sequentialize_threads
            || self.cfg.debug_futex_mode != BlockingMode::Precise
        {
            return;
        }

        let heads = guest.thread_state().robust_list_heads();
        let mut staged = Vec::with_capacity(heads.len());
        for (owner, head) in heads {
            let mut wakes = Vec::new();
            let mut effects = GuestRobustEffects::<'_, G, T> {
                guest,
                dettid: owner,
                defer_owner_death_to_backend: true,
                staged_wakes: Some(&mut wakes),
                tool: PhantomData,
            };
            let outcome =
                robust_list::exit_robust_list(&mut effects, head, owner.as_raw() as u32).await;
            if outcome.head_unreadable || outcome.aborted {
                trace!(
                    "[detcore, dtid {}] could not stage complete robust-list owner-death effects: {:?}",
                    owner, outcome,
                );
            }
            staged.push((owner, wakes));
        }
        guest.thread_state().stage_robust_list_wakes(reason, staged);
    }

    /// Exit system call
    pub async fn handle_exit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Exit,
    ) -> Result<i64, Error> {
        let request = guest.thread_state().mk_request(
            ResourceID::Exit {
                group: false,
                process: guest.thread_state().detpid.expect("detpid unset"),
                mm: guest.thread_state().mm_id,
            },
            Permission::RW,
        );
        resource_request(guest, request).await;
        self.run_robust_list_owner_death(guest).await;
        // It's ok here that we skip running the posthook:
        guest.tail_inject(call).await
    }

    /// Exit_group system call
    pub async fn handle_exit_group<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ExitGroup,
    ) -> Result<i64, Error> {
        let request = guest.thread_state().mk_request(
            ResourceID::Exit {
                group: true,
                process: guest.thread_state().detpid.expect("detpid unset"),
                mm: guest.thread_state().mm_id,
            },
            Permission::RW,
        );
        resource_request(guest, request).await;
        self.stage_thread_group_robust_list_wakes(guest, RobustListExit::ExitGroup)
            .await;
        // It's ok here that we skip running the posthook:
        guest.tail_inject(call).await
    }

    /// Futex system call, which can block.
    pub async fn handle_futex<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Futex,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let ptr = match call.uaddr() {
            None => {
                // null pointer error:
                return Ok(guest.inject(call).await?);
            }
            Some(x) => x,
        };
        let init_val = guest.memory().read_value(ptr)?;
        trace!(
            "[detcore, dtid {}] futex op with memory address containing value {}",
            &dettid, init_val
        );

        if !self.cfg.sequentialize_threads {
            Ok(guest.inject(call).await?)
        } else {
            match self.cfg.debug_futex_mode {
                BlockingMode::Precise => self.handle_futex_blocking(guest, call, init_val).await,
                BlockingMode::Polling => self.handle_futex_polling(guest, call, init_val).await,
                BlockingMode::External => self.record_or_replay_blocking(guest, call.into()).await,
            }
        }
    }

    /// Blocking (precise) Futex implementation.
    /// Here we use a two-phase request to the scheduler: before and after the futex wait/wake
    /// side effects. We EMULATE futex calls and NEVER run them inside the kernel.
    pub async fn handle_futex_blocking<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Futex,
        init_val: i32,
    ) -> Result<i64, Error> {
        let ptr = call.uaddr().unwrap();
        let futexid = guest.thread_state().futex_id(
            AddrMut::as_raw(ptr),
            call.futex_op() & libc::FUTEX_PRIVATE_FLAG != 0,
        );
        let futex_op = call.futex_op() & libc::FUTEX_CMD_MASK;
        let bitset = match futex_op {
            libc::FUTEX_WAKE_BITSET | libc::FUTEX_WAIT_BITSET => call.val3() as u32,
            _ => u32::MAX,
        };
        if bitset == 0 {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let dettid = guest.thread_state().dettid;
        match futex_op {
            libc::FUTEX_WAKE | libc::FUTEX_WAKE_BITSET => {
                let num = match futex_action(
                    guest,
                    FutexAction::WakeRequest(call.val()),
                    &futexid,
                    init_val,
                    bitset,
                )
                .await
                .expect("futex wake must return value")
                {
                    SchedValue::Value(num) => num,
                    SchedValue::TimeOut => panic!("impossible, futex wake doesn't have a timeout"),
                };
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#845): Review exited-thread futex diagnostics.
                match guest.memory().read_value(ptr) {
                    Ok(observed) => trace!(
                        "[detcore, dtid {}] emulated futex wake committed, memory value is {}, expected {}",
                        &dettid,
                        observed,
                        call.val(),
                    ),
                    Err(error) => trace!(
                        "[detcore, dtid {}] skipped post-wake futex memory diagnostic: {}",
                        &dettid, error,
                    ),
                }
                let _ = futex_action(
                    guest,
                    FutexAction::WakeFinished(0),
                    &futexid,
                    init_val,
                    bitset,
                )
                .await;
                Ok(num as i64)
            }
            libc::FUTEX_WAIT | libc::FUTEX_WAIT_BITSET => {
                if init_val != call.val() {
                    info!(
                        "[detcore, dtid {}] Futex wait running immediately because it will fizzle ({} != {}).",
                        &dettid,
                        init_val,
                        call.val()
                    );
                    Err(Error::Errno(Errno::EAGAIN))
                } else {
                    let maybe_timeout_lt = self
                        .futex_timeout_deadline(guest, call.futex_op(), call.timeout())
                        .await?;
                    let ans = futex_action(
                        guest,
                        FutexAction::WaitRequest(maybe_timeout_lt),
                        &futexid,
                        init_val,
                        bitset,
                    )
                    .await;
                    let res = if ans != Some(SchedValue::TimeOut) {
                        let expected = call.val();
                        // AUTONOMOUS-BOT-IMPLEMENTED
                        // TODO-HUMAN-REVIEW(#845): Review exited-thread futex diagnostics.
                        match guest.memory().read_value(ptr) {
                            Ok(observed) => {
                                trace!(
                                    "[detcore, dtid {}] after (emulated) futex wait, memory value is {}, expected {}",
                                    &dettid, observed, expected,
                                );
                                if expected == observed {
                                    debug!(
                                        "WARNING: fishy that the futex value did not change before wakeup. Weird application-level protocol.\n"
                                    );
                                }
                            }
                            Err(error) => trace!(
                                "[detcore, dtid {}] skipped post-wait futex memory diagnostic: {}",
                                &dettid, error,
                            ),
                        }
                        Ok(0)
                    } else {
                        trace!("[detcore, dtid {}] futex wait timed out", &dettid);
                        Err(Error::Errno(Errno::ETIMEDOUT))
                    };
                    futex_action(guest, FutexAction::WaitFinished, &futexid, init_val, bitset)
                        .await;
                    res
                }
            }
            libc::FUTEX_FD => {
                panic!("[detcore] refusing to execute FUTEX_FD, which was removed in Linux 2.6.26.")
            }
            other => {
                panic!("[detcore] futex op not handled yet: {}", other);
            }
        }
    }

    /// Futex system call, alternative implemenattion where we treat futexes as InternalIOPolling
    /// operations.
    pub async fn handle_futex_polling<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Futex,
        init_val: i32,
    ) -> Result<i64, Error> {
        fn make_futex_wake_request(dettid: DetTid) -> Resources {
            let mut rsrc = Resources::new(dettid);
            rsrc.fyi("futex_wake");
            rsrc
        }

        fn make_futex_wait_request(dettid: DetTid) -> Resources {
            let mut rsrc = Resources::new(dettid);
            rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
            rsrc.fyi("futex_wait");
            rsrc
        }

        let dettid = guest.thread_state().dettid;
        let futex_op = call.futex_op() & libc::FUTEX_CMD_MASK;
        match futex_op {
            libc::FUTEX_WAKE | libc::FUTEX_WAKE_BITSET => {
                let rsrc = make_futex_wake_request(dettid);
                resource_request(guest, rsrc.clone()).await; // Linearize this operation as a separate COMMIT.
                let res = guest.inject(call).await;
                // FIXME: With the non-blocking version of futex_wait, `res` will always be 0.  It
                // is quite difficult to tell how many polling waiters we unblocked with a given
                // wake, without going back to modeling futexes like `handle_futex_blocking` does.
                Ok(res?)
            }
            libc::FUTEX_WAIT | libc::FUTEX_WAIT_BITSET => {
                if init_val != call.val() {
                    info!(
                        "[detcore, dtid {}] Futex wait running immediately because it will fizzle ({} != {}).",
                        dettid,
                        init_val,
                        call.val()
                    );
                    let res = guest.inject(call).await;
                    Ok(res?)
                } else {
                    let rsrc = make_futex_wait_request(dettid);
                    let deadline = self
                        .futex_timeout_deadline(guest, call.futex_op(), call.timeout())
                        .await?;
                    let res =
                        retry_nonblocking_syscall_with_timeout(guest, call, rsrc, deadline).await?;
                    trace!(
                        "[detcore, dtid {}] after futex wait, memory value is {}",
                        &dettid,
                        guest.memory().read_value(call.uaddr().unwrap()).unwrap()
                    );
                    Ok(res)
                }
            }
            libc::FUTEX_FD => {
                panic!("[detcore] refusing to execute FUTEX_FD, which was removed in Linux 2.6.26.")
            }
            other => {
                panic!("[detcore] futex op not handled yet: {}", other);
            }
        }
    }

    /// Execveat system call.  Doesn't return if successful.
    pub async fn handle_execveat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Execveat,
    ) -> Result<i64, Error> {
        let (old_metadata, old_memory_metadata, table_is_shared, dettid, detpid, old_mm_id) = {
            let thread_state = guest.thread_state();
            (
                Arc::clone(&thread_state.file_metadata),
                Arc::clone(&thread_state.memory_metadata),
                Arc::strong_count(&thread_state.file_metadata) > 1,
                thread_state.dettid,
                thread_state.detpid.expect("detpid unset"),
                thread_state.mm_id,
            )
        };
        let (new_metadata, closed_open_files, exec_fd_blocking) = {
            let metadata = old_metadata.lock().unwrap();
            let new_metadata = metadata.for_exec(dettid);
            (
                new_metadata.clone(),
                metadata.open_files_closed_on_exec(table_is_shared),
                new_metadata.exec_blocking_overrides(),
            )
        };
        let preserve_exec_fd_status = guest.thread_state().discover_live_file_metadata;

        prepare_exec(
            guest,
            old_mm_id,
            if preserve_exec_fd_status {
                exec_fd_blocking
            } else {
                Default::default()
            },
        )
        .await;

        let mut released_ports = Vec::new();
        for open_file_id in closed_open_files {
            if let Some(port) = self.release_port_for_open_file(guest, open_file_id).await {
                released_ports.push((open_file_id, port));
            }
        }

        // A successful execve replaces the address space, and Linux clears
        // `task->robust_list` with it; the new image re-registers its own.
        let old_robust_list_head;

        {
            let thread_state = guest.thread_state_mut();
            thread_state.file_metadata = Arc::new(Mutex::new(new_metadata));
            thread_state.memory_metadata = Arc::new(Mutex::new(MemoryMetadata::new()));
            thread_state.mm_id = old_mm_id.for_exec(detpid);
            old_robust_list_head = thread_state.take_robust_list_for_exec();
        }

        // execve(2) doesn't return upon success.
        let errno = self.record_or_replay(guest, call).await.unwrap_err();

        {
            let thread_state = guest.thread_state_mut();
            thread_state.file_metadata = old_metadata;
            thread_state.memory_metadata = old_memory_metadata;
            thread_state.mm_id = old_mm_id;
            thread_state.restore_robust_list_after_failed_exec(old_robust_list_head);
        }

        cancel_exec(guest).await;
        for (open_file_id, port) in released_ports {
            self.restore_port_for_open_file(guest, open_file_id, port)
                .await;
        }

        Err(errno.into())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#258): Confirm one-turn exclusion semantics across scheduler modes.
    /// End the current logical timeslice for a sequentialized sched_yield.
    pub async fn handle_sched_yield<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedYield,
    ) -> Result<i64, Error> {
        if self.cfg.sequentialize_threads {
            // In chaos mode, thread-interleaving diversity (and thus fairness)
            // comes entirely from re-randomizing thread priorities at
            // preemption-timer expirations. When timer preemption is disabled
            // (`--max-timeslice disabled`), priorities are fixed at thread
            // creation and never change. A plain yield only re-enqueues the
            // caller at the back of its own (fixed) priority level, so a thread
            // that spins on sched_yield while holding the numerically-lowest
            // priority is always reselected first and starves every thread it is
            // waiting on (GH #81). Treat sched_yield as an explicit chaos
            // reprioritization point: draw a fresh random priority for the
            // caller so it cedes the CPU and other runnable threads can make
            // progress. This mirrors what `end_timeslice` does at a timer-driven
            // preemption point, and is recorded for chaos replay.
            if self.cfg.chaos && self.cfg.max_timeslice.is_none() {
                let change_time = guest.thread_state().thread_logical_time.as_nanos();
                let request = Self::random_priority_changepoint_request(guest, change_time);
                resource_request(guest, request).await;
            } else if !self.cfg.chaos && self.cfg.replay_preemptions_from.is_some() {
                if self.cfg.max_timeslice.is_some() {
                    guest
                        .thread_state_mut()
                        .reset_timeslice_for_explicit_yield();
                }
                let request = Self::sched_yield_request(guest);
                resource_request(guest, request).await;
            } else if self.cfg.chaos || self.cfg.replay_schedule_from.is_some() {
                let request = Self::yield_request(guest);
                resource_request(guest, request).await;
            } else {
                self.end_timeslice_for_sched_yield(guest).await;
            }
            trace!("sched_yield yielded to the scheduler; NOT performing actual syscall");
            Ok(0)
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// wait4 system call
    /// This is handled by the scheduler and not passed to the record/replay layer.
    // TODO-HUMAN-REVIEW(PR-587): Confirm wait4 rusage canonicalization boundaries.
    pub async fn handle_wait4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Wait4,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let mut rsrc = Resources::new(dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("wait4");

        let parent = guest.thread_state().detpid.expect("detpid unset");
        // Stop/continue events need a backend waitability callback. Non-SIGCHLD
        // process clones also remain legacy until backends distinguish
        // PTRACE_EVENT_CLONE from CLONE_THREAD. The common matcher already
        // carries those filters so activation does not require another model.
        let selector = if call.options().intersects(
            WaitPidFlag::WUNTRACED
                | WaitPidFlag::WCONTINUED
                | WaitPidFlag::__WCLONE
                | WaitPidFlag::__WALL,
        ) {
            None
        } else {
            match call.pid() {
                pid if pid > 0 => Some(ChildWaitSelector::Exact(DetPid::from_raw(pid))),
                -1 => Some(ChildWaitSelector::Any),
                0 => process_group(guest, parent)
                    .await
                    .map(ChildWaitSelector::ProcessGroup),
                pid if pid < -1 => Some(ChildWaitSelector::ProcessGroup(DetPid::from_raw(-pid))),
                _ => unreachable!(),
            }
        };
        let spec = selector
            .map(|selector| terminal_child_wait_spec(selector, dettid, call.options().bits()));
        let complete_lineage = guest.config().backend_tracks_process_children;
        let managed_spec = if let Some(spec) = spec {
            let (_, has_child) = ready_child_wait(guest, spec).await;
            (has_child || complete_lineage).then_some(spec)
        } else {
            None
        };

        let value = if call.options().contains(WaitPidFlag::WNOHANG) {
            resource_request(guest, rsrc.clone()).await;
            info!(
                "[dtid {}] Executing non-blocking wait4 in one shot.",
                dettid
            );
            if let Some(spec) = spec {
                'select_child: loop {
                    let (ready, has_child) = ready_child_wait(guest, spec).await;
                    let Some(child) = ready else {
                        break if has_child {
                            0
                        } else if complete_lineage {
                            return Err(Errno::ECHILD.into());
                        } else {
                            guest.inject_with_retry(call).await?
                        };
                    };
                    let _ = await_exact_child_physical_exit(guest, child).await;
                    let exact_call = call.with_pid(child.as_raw());
                    loop {
                        match guest.inject_with_retry(exact_call).await {
                            Ok(value) if value != 0 => break 'select_child value,
                            Ok(_) => yield_once().await,
                            Err(Errno::ECHILD) => {
                                let _ = consume_child_wait(guest, child).await;
                                if child_wait_can_retry_after_stale(spec) {
                                    continue 'select_child;
                                }
                                return Err(Errno::ECHILD.into());
                            }
                            Err(errno) => return Err(errno.into()),
                        }
                    }
                }
            } else {
                guest.inject_with_retry(call).await?
            }
        } else if let Some(spec) = managed_spec {
            {
                // The ptrace backend must block ordinary signals until child
                // readiness is resolved. DBT already delays application signal
                // delivery while this callback is active, and replacing its
                // application mask here also hides those signals from
                // rt_sigpending. Read that mask without changing it instead.
                let blocked_mask = blocked_signal_mask();
                let mut stack = guest.stack().await;
                let blocked_mask_addr = stack.push(blocked_mask);
                let old_mask_addr = stack.reserve::<KernelSigset>();
                let action_addr = stack.reserve::<KernelSigaction>();
                let _mask_guard = stack.commit()?;
                let guest_signal_mask =
                    block_signals_for_disposition(guest, blocked_mask_addr, old_mask_addr).await?;
                let inspect_signal_action = guest
                    .config()
                    .backend_requires_thread_directed_process_signals;

                let poll_call = call.with_options(call.options() | WaitPidFlag::WNOHANG);
                let mut pending_signal = None;
                let result: Result<i64, Error> = loop {
                    let status = wait_for_child_lifecycle(guest, spec).await;
                    if pending_signal.is_none() {
                        pending_signal = wait_signal_disposition(
                            guest,
                            status,
                            &guest_signal_mask,
                            action_addr,
                            inspect_signal_action,
                        )
                        .await?;
                    }
                    let (ready, has_child) = ready_child_wait(guest, spec).await;
                    if let Some(child) = ready {
                        let _ = await_exact_child_physical_exit(guest, child).await;
                        match guest.inject_with_retry(call.with_pid(child.as_raw())).await {
                            Ok(value) => break Ok(value),
                            Err(Errno::ECHILD) => {
                                let _ = consume_child_wait(guest, child).await;
                                if child_wait_can_retry_after_stale(spec) {
                                    let (next_ready, _) = ready_child_wait(guest, spec).await;
                                    if stale_any_wait_must_interrupt(
                                        pending_signal.is_some(),
                                        next_ready,
                                    ) {
                                        break interrupted_child_wait_result(
                                            guest,
                                            call,
                                            pending_signal.expect("signal checked above"),
                                        )
                                        .await;
                                    }
                                    continue;
                                }
                                break Err(Errno::ECHILD.into());
                            }
                            Err(errno) => break Err(errno.into()),
                        }
                    }
                    if !has_child {
                        break Err(Errno::ECHILD.into());
                    }
                    match guest.inject(poll_call).await {
                        Ok(value) => {
                            if value > 0 {
                                break Ok(value);
                            }
                            if let Some(disposition) = pending_signal {
                                break interrupted_child_wait_result(guest, call, disposition)
                                    .await;
                            }
                        }
                        Err(errno) => break Err(errno.into()),
                    }
                };

                restore_signals_after_disposition(guest, old_mask_addr).await?;
                result?
            }
        } else {
            // wait4 is a scheduler poll, not a record/replay data read (see doc above),
            // so it is not routed through the record/replay subtool.
            retry_nonblocking_syscall(guest, call, rsrc, None).await?
        };
        let consumed_termination = if value <= 0 {
            false
        } else if let Some(status) = call.wstatus() {
            wait_status_is_termination(guest.memory().read_value(status)?)
        } else {
            guest
                .thread_state()
                .has_exited_child_process_cpu_time(DetPid::from_raw(value as i32))
        };
        if consumed_termination {
            guest
                .thread_state_mut()
                .reap_child_process_cpu_time(DetPid::from_raw(value as i32));
            let _ = consume_child_wait(guest, DetPid::from_raw(value as i32)).await;
        }
        if value > 0
            && let Some(rusage) = call.rusage()
        {
            // Host CPU and scheduling counters are not deterministic.
            let usage: libc::rusage = unsafe { std::mem::zeroed() };
            guest.memory().write_value(rusage, &usage)?;
        }
        Ok(value)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#274): Review waitid polling and compatibility boundaries.
    // TODO-HUMAN-REVIEW(#2246): Review waitid progress and child-ready signal precedence.
    /// waitid system call
    /// This is handled by the scheduler and not passed to the record/replay layer.
    pub async fn handle_waitid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Waitid,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let mut rsrc = Resources::new(dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("waitid");

        let event_options = libc::WEXITED | libc::WSTOPPED | libc::WCONTINUED;
        let allowed_options = event_options
            | libc::WNOHANG
            | libc::WNOWAIT
            | libc::__WNOTHREAD
            | libc::__WALL
            | libc::__WCLONE;
        if call.options() & event_options == 0 || call.options() & !allowed_options != 0 {
            return Err(Errno::EINVAL.into());
        }

        // POSIX requires non-null infop. Linux accepts null, but that form can
        // expose host rusage and requires backend-neutral scratch memory for
        // deterministic polling. Reject it uniformly instead of diverging or
        // panicking on DBT's unsupported scratch stack.
        if call.info().is_none() {
            return Err(Errno::EFAULT.into());
        }

        // Keep clone-class waits on the legacy path until ptrace and KVM both
        // register non-SIGCHLD clone children as processes rather than threads.
        let terminal_events_only = call.options() & libc::WEXITED != 0
            && call.options() & (libc::WSTOPPED | libc::WCONTINUED | libc::__WCLONE | libc::__WALL)
                == 0;

        // The parked nonterminal path still uses kernel polling. Preserve its
        // P_PGID(0) entry snapshot until backend lifecycle events replace it.
        if !terminal_events_only && call.which() == libc::P_PGID as i32 && call.pid() == 0 {
            call = call.with_pid(snapshot_process_group(guest.pid())?);
        }

        // A blocking waitid on an O_NONBLOCK pidfd must return EAGAIN rather
        // than being converted to WNOHANG. Acquire the scheduler resource first,
        // then snapshot fdinfo and issue the one-shot wait without another yield.
        let pidfd_nonblocking =
            if call.which() == libc::P_PIDFD as i32 && call.options() & libc::WNOHANG == 0 {
                resource_request(guest, rsrc.clone()).await;
                guest_fd_status_flags(guest.pid(), call.pid())? & libc::O_NONBLOCK != 0
            } else {
                false
            };
        if call.which() == libc::P_PIDFD as i32
            && call.options() & libc::WNOHANG == 0
            && !pidfd_nonblocking
        {
            // Polling a numeric pidfd cannot preserve Linux's held file
            // reference if another thread closes and reuses the descriptor.
            // Reject the blocking form until Detcore can retain that identity.
            return Err(Errno::EOPNOTSUPP.into());
        }
        let info = call.info().expect("waitid infop checked above");

        // Unlike wait4, waitid returns zero both when it reports a child event and
        // when WNOHANG finds nothing. Polling must inspect si_pid to distinguish
        // those cases.
        // Known limitation: without backend-neutral scratch memory, an invalid
        // non-null infop faults on the first physical poll rather than after a
        // child becomes waitable.
        // siginfo_t has no portable initializer. An all-zero value is the
        // waitid WNOHANG sentinel defined by POSIX and Linux.
        let empty_info: libc::siginfo_t = unsafe { std::mem::zeroed() };

        // The lifecycle scheduler currently models terminal child events for
        // exact and any-child selectors. Group membership and stop/continue
        // state remain on the legacy kernel-polling path.
        let terminal_spec = if terminal_events_only {
            let selector = match call.which() {
                which if which == libc::P_PID as i32 => {
                    Some(ChildWaitSelector::Exact(DetPid::from_raw(call.pid())))
                }
                which if which == libc::P_ALL as i32 => Some(ChildWaitSelector::Any),
                which if which == libc::P_PGID as i32 => {
                    let group = if call.pid() == 0 {
                        process_group(guest, guest.thread_state().detpid.expect("detpid unset"))
                            .await
                    } else {
                        Some(DetPid::from_raw(call.pid()))
                    };
                    group.map(ChildWaitSelector::ProcessGroup)
                }
                _ => None,
            };
            selector.map(|selector| terminal_child_wait_spec(selector, dettid, call.options()))
        } else {
            None
        };
        let complete_lineage = guest.config().backend_tracks_process_children;
        let managed_terminal_spec = if let Some(spec) = terminal_spec {
            let (_, has_child) = ready_child_wait(guest, spec).await;
            (has_child || complete_lineage).then_some(spec)
        } else {
            None
        };

        if call.options() & libc::WNOHANG != 0 || pidfd_nonblocking {
            if !pidfd_nonblocking {
                resource_request(guest, rsrc).await;
            }
            info!(
                "[dtid {}] Executing non-blocking waitid in one shot.",
                dettid
            );
            'select_child: loop {
                let selected = if let Some(spec) = terminal_spec {
                    let (ready, has_child) = ready_child_wait(guest, spec).await;
                    if ready.is_none() && has_child {
                        guest.memory().write_value(info, &empty_info)?;
                        return Ok(0);
                    }
                    if ready.is_none() && complete_lineage {
                        return Err(Errno::ECHILD.into());
                    }
                    ready
                } else {
                    None
                };
                if let Some(child) = selected {
                    let _ = await_exact_child_physical_exit(guest, child).await;
                }
                let effective_call = selected.map_or(call, |child| {
                    call.with_which(libc::P_PID as i32).with_pid(child.as_raw())
                });
                loop {
                    guest.memory().write_value(info, &empty_info)?;
                    let value = match guest.inject_with_retry(effective_call).await {
                        Ok(value) => value,
                        Err(Errno::ECHILD) if selected.is_some() => {
                            let child = selected.expect("selected child checked above");
                            let _ = consume_child_wait(guest, child).await;
                            if terminal_spec.is_some_and(child_wait_can_retry_after_stale) {
                                continue 'select_child;
                            }
                            return Err(Errno::ECHILD.into());
                        }
                        Err(errno) => return Err(errno.into()),
                    };
                    let info_value: libc::siginfo_t = guest.memory().read_value(info)?;
                    let child_pid = unsafe { info_value.si_pid() };
                    if child_pid == 0 && selected.is_some() {
                        yield_once().await;
                        continue;
                    }
                    let consumed = child_pid != 0
                        && call.options() & libc::WNOWAIT == 0
                        && waitid_code_is_termination(info_value.si_code);
                    let result = finish_waitid_result(guest, call, value, info_value)?;
                    if consumed {
                        let _ = consume_child_wait(guest, DetPid::from_raw(child_pid)).await;
                    }
                    return Ok(result);
                }
            }
        }

        {
            // A signal can arrive after the scheduler wakes this logical wait but
            // before the zero-timeout kernel probe that resolves Linux's
            // child-ready-versus-interrupt precedence. The ptrace backend blocks
            // ordinary signals across that probe, then restores the guest's exact
            // mask before returning. DBT reads the mask without replacing it because
            // DynamoRIO already delays application delivery while this callback runs.
            // The tracer's private preemption signal must remain unblocked.
            let blocked_mask = blocked_signal_mask();
            let mut stack = guest.stack().await;
            let blocked_mask_addr = stack.push(blocked_mask);
            let old_mask_addr = stack.reserve::<KernelSigset>();
            let action_addr = stack.reserve::<KernelSigaction>();
            let _mask_guard = stack.commit()?;
            let guest_signal_mask =
                block_signals_for_disposition(guest, blocked_mask_addr, old_mask_addr).await?;
            let inspect_signal_action = guest
                .config()
                .backend_requires_thread_directed_process_signals;

            let poll_call = call.with_options(call.options() | libc::WNOHANG);
            let mut pending_signal = None;
            let result: Result<i64, Error> = loop {
                // Match the polling protocol used by wait4: the first request with
                // poll_attempt zero establishes an ordinary runnable turn, while later
                // nonzero attempts receive the scheduler's poller backoff. Omitting the
                // first request starts directly as a poller and can keep the run queue
                // nonempty forever, preventing logical time from reaching a pending
                // signal's deadline.
                //
                // Do not return on Signaled yet. Linux lets an already-waitable child
                // status win over an interrupt, so the zero-timeout kernel probe below
                // remains authoritative when readiness and a signal coincide.
                let managed_spec = managed_terminal_spec;
                // Both ways of parking inside waitid -- the scheduler-managed child
                // wait and the legacy kernel-polling loop -- can now be resumed with
                // the signals that woke the thread, so both consult the guest mask.
                let status = if let Some(spec) = managed_spec {
                    wait_for_child_lifecycle(guest, spec).await
                } else {
                    resource_request(guest, rsrc.clone()).await
                };
                if pending_signal.is_none() {
                    pending_signal = wait_signal_disposition(
                        guest,
                        status,
                        &guest_signal_mask,
                        action_addr,
                        inspect_signal_action,
                    )
                    .await?;
                }
                let (ready, has_child) = if let Some(spec) = managed_spec {
                    ready_child_wait(guest, spec).await
                } else {
                    (None, true)
                };
                if let Some(child) = ready {
                    let _ = await_exact_child_physical_exit(guest, child).await;
                    if let Err(error) = guest.memory().write_value(info, &empty_info) {
                        break Err(error.into());
                    }
                    let exact_call = call.with_which(libc::P_PID as i32).with_pid(child.as_raw());
                    match guest.inject_with_retry(exact_call).await {
                        Ok(value) => {
                            let info_value = match guest.memory().read_value(info) {
                                Ok(value) => value,
                                Err(error) => break Err(error.into()),
                            };
                            break finish_waitid_result(guest, call, value, info_value);
                        }
                        Err(Errno::ECHILD) => {
                            let _ = consume_child_wait(guest, child).await;
                            if managed_spec.is_some_and(child_wait_can_retry_after_stale) {
                                let (next_ready, _) =
                                    ready_child_wait(guest, managed_spec.expect("managed spec"))
                                        .await;
                                if stale_any_wait_must_interrupt(
                                    pending_signal.is_some(),
                                    next_ready,
                                ) {
                                    break interrupted_child_wait_result(
                                        guest,
                                        call,
                                        pending_signal.expect("signal checked above"),
                                    )
                                    .await;
                                }
                                continue;
                            }
                            break Err(Errno::ECHILD.into());
                        }
                        Err(errno) => break Err(errno.into()),
                    }
                }
                if managed_spec.is_some() && !has_child {
                    break Err(Errno::ECHILD.into());
                }

                if let Err(error) = guest.memory().write_value(info, &empty_info) {
                    break Err(error.into());
                }
                let result = guest.inject(poll_call).await;
                match result {
                    Ok(value) => {
                        let info_value: libc::siginfo_t = match guest.memory().read_value(info) {
                            Ok(value) => value,
                            Err(error) => break Err(error.into()),
                        };
                        // waitid writes the SIGCHLD variant of siginfo_t. A zeroed
                        // structure is used only for the no-event WNOHANG result.
                        let child_pid = unsafe { info_value.si_pid() };
                        match exact_wait_poll_decision(
                            child_pid != 0,
                            pending_signal.is_some(),
                            None,
                        ) {
                            ExactWaitPollDecision::ChildReady => {
                                break finish_waitid_result(guest, call, value, info_value);
                            }
                            ExactWaitPollDecision::Interrupted => {
                                break interrupted_child_wait_result(
                                    guest,
                                    call,
                                    pending_signal.expect("signal checked above"),
                                )
                                .await;
                            }
                            ExactWaitPollDecision::Retry => {}
                            ExactWaitPollDecision::AwaitPhysicalExit
                            | ExactWaitPollDecision::ReapAfterLogicalExit => unreachable!(),
                        }
                        if managed_spec.is_some() {
                            if !has_child {
                                break Ok(value);
                            }
                            continue;
                        }
                        rsrc.poll_attempt += 1;
                        trace!(
                            "Retry #{} for waitid because no child state is ready",
                            rsrc.poll_attempt
                        );
                        record_retry_event(guest, poll_call).await;
                    }
                    Err(Errno::ERESTARTSYS) if pending_signal.is_some() => {
                        break Err(Errno::EINTR.into());
                    }
                    Err(errno) => break Err(errno.into()),
                }
            };

            restore_signals_after_disposition(guest, old_mask_addr).await?;
            if result.is_ok() && call.options() & libc::WNOWAIT == 0 {
                let info_value: libc::siginfo_t = guest.memory().read_value(info)?;
                let child_pid = unsafe { info_value.si_pid() };
                if child_pid != 0 && waitid_code_is_termination(info_value.si_code) {
                    let _ = consume_child_wait(guest, DetPid::from_raw(child_pid)).await;
                }
            }
            result
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#546)
    /// Accept valid affinity masks without changing the host scheduler.
    pub async fn handle_sched_setaffinity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedSetaffinity,
    ) -> Result<i64, Error> {
        let size_bytes = call.len() as usize;
        if size_bytes == 0 {
            return Err(Errno::EINVAL.into());
        }

        let mask = call.mask().ok_or(Errno::EFAULT)?;
        let mask: Addr<u8> = mask.cast();
        let mut requested = [0u8; VIRTUAL_CPUSET_BYTES];
        let bytes_to_read = size_bytes.min(VIRTUAL_CPUSET_BYTES);
        guest
            .memory()
            .read_exact(mask, &mut requested[..bytes_to_read])?;
        info!(
            "Suppressing sched_setaffinity mask {:?}; affinity remains virtual CPU 0",
            requested
        );
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#546)
    /// Report that we are on cpu 0, irrespective of what physical CPU we are on.
    pub async fn handle_sched_getaffinity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedGetaffinity,
    ) -> Result<i64, Error> {
        let size_bytes: usize = call.len() as usize;
        if size_bytes < VIRTUAL_CPUSET_BYTES
            || !size_bytes.is_multiple_of(std::mem::size_of::<libc::c_ulong>())
        {
            return Err(Errno::EINVAL.into());
        }

        // N.B. we can't use an opaque, type-safe representation such as
        // nix::sched::CpuSet currently.  The problem is that the
        // SchedGetAffinity syscall treats this field as a u64.
        let mut cpu_set = [0u8; VIRTUAL_CPUSET_BYTES];
        cpu_set[0] = 1;

        info!(
            "Suppressing sched_getaffinity and returning {}-byte virtualized result, {:?}",
            VIRTUAL_CPUSET_BYTES, cpu_set
        );
        if let Some(mask) = call.mask() {
            let mask: AddrMut<u8> = mask.cast();
            guest.memory().write_exact(mask, &cpu_set)?;
            // From the man page:
            // > On success, the raw sched_getaffinity() system call returns the size (in bytes) of
            // > the cpumask_t data type that is used internally by the kernel to represent the CPU
            // > set bit mask.
            Ok(VIRTUAL_CPUSET_BYTES as i64)
        } else {
            Err(Error::Errno(Errno::EFAULT))
        }
    }

    /// sched_getparam under Hermit. Detcore replaces the Linux scheduler with its
    /// own deterministic one, so a thread's Linux scheduling parameters are
    /// inert. Report a fixed SCHED_OTHER priority of 0. The value is emulated
    /// (never injected), so it is identical across --verify runs and
    /// record/replay.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#720)
    pub async fn handle_sched_getparam<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedGetparam,
    ) -> Result<i64, Error> {
        if let Some(param) = call.param() {
            let p = libc::sched_param { sched_priority: 0 };
            guest.memory().write_value(param, &p)?;
        }
        Ok(0)
    }

    /// sched_rr_get_interval under Hermit. The round-robin quantum is a property
    /// of the Linux scheduler, which Detcore does not use, so report a fixed zero
    /// interval. Being a constant, it is deterministic across --verify and
    /// record/replay.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#720)
    pub async fn handle_sched_rr_get_interval<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedRrGetInterval,
    ) -> Result<i64, Error> {
        if let Some(tp) = call.tp() {
            let t = Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            guest.memory().write_value(tp, &t)?;
        }
        Ok(0)
    }

    /// sched_getattr under Hermit. Detcore replaces the Linux scheduler with its
    /// own deterministic one, so a thread's Linux scheduling attributes are inert.
    /// Report a fixed SCHED_OTHER policy with zeroed nice/priority/flags. The
    /// value is emulated (never injected), so it is identical across --verify runs
    /// and record/replay. Re-enables `chrt` under --strict.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#791)
    pub async fn handle_sched_getattr<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedGetattr,
    ) -> Result<i64, Error> {
        // `flags` is reserved and must be zero; the kernel rejects anything else.
        if call.flags() != 0 {
            return Err(Errno::EINVAL.into());
        }
        // The caller must provide room for at least the base sched_attr (VER0).
        let attr_size = std::mem::size_of::<libc::sched_attr>();
        if (call.size() as usize) < attr_size {
            return Err(Errno::EINVAL.into());
        }
        let attr = call.attr().ok_or(Errno::EINVAL)?;
        // SAFETY: sched_attr is a plain-old-data struct; an all-zero bit pattern is
        // a valid SCHED_OTHER descriptor (nice/priority/flags/runtime/... all 0).
        let mut sa: libc::sched_attr = unsafe { std::mem::zeroed() };
        sa.size = attr_size as u32;
        sa.sched_policy = libc::SCHED_OTHER as u32;
        // SAFETY: reinterpret the POD struct as its raw bytes to copy it into the
        // guest's buffer.
        let bytes = unsafe {
            std::slice::from_raw_parts(&sa as *const libc::sched_attr as *const u8, attr_size)
        };
        let dst: AddrMut<u8> = attr.cast();
        guest.memory().write_exact(dst, bytes)?;
        info!(
            "Emulating sched_getattr(pid={}): fixed SCHED_OTHER, nice 0, priority 0",
            call.pid()
        );
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-841): Review virtual sched_setattr no-op policy.
    /// Linux scheduler attributes cannot affect Detcore's replacement
    /// scheduler, so a *well-formed* request is accepted as a deterministic
    /// no-op, matching the existing sched_setscheduler and sched_setparam
    /// policy.
    ///
    /// Suppressing the effect is not the same as accepting arguments Linux
    /// refuses, nor as refusing arguments Linux accepts. Both directions are
    /// guest-visible: a probe that expects EINVAL and sees success takes the
    /// wrong branch, and so does one that expects success and sees E2BIG.
    ///
    /// The order below is the kernel's, and it is guest-visible when two
    /// arguments are wrong at once, because the kernel returns the *first*
    /// applicable error. `sched_setattr()` screens `uattr`, `pid` and `flags`
    /// together; `sched_copy_attr()` then handles the size, the trailing bytes
    /// and the util-clamp size rule; the signed-policy test follows; the target
    /// pid is resolved *there*, in the middle; and only then does
    /// `__sched_setscheduler()` judge the policy, the flags, the priority and
    /// the deadline parameters. Putting any of that last group before the pid
    /// lookup makes a request against a nonexistent pid report EINVAL where
    /// Linux reports ESRCH.
    ///
    /// # Determinism
    ///
    /// Every check is a pure function of the guest's own arguments. The one
    /// piece of state consulted is the pid lookup, which asks the scheduler's
    /// own task table via `thread_is_live` -- Detcore state, replayed
    /// identically -- rather than the host's process table, which would leak
    /// unrelated host processes into a guest-visible answer.
    ///
    /// Specifically NOT `tool_global::resolve_kill_targets`, which
    /// models `kill(2)` and so recognises only thread-group leaders; asking it
    /// this question reports ESRCH for a live non-leader thread.
    ///
    /// # Deliberately not emulated
    ///
    /// Three behaviours are excluded, under one rule: **an answer that depends
    /// on the host's kernel configuration or the caller's privileges is not
    /// reproduced, because a deterministic sandbox must not vary with the
    /// machine underneath it.** Each was measured natively and would differ on
    /// a differently-built or differently-privileged host:
    ///
    /// * EPERM for a real-time priority, a SCHED_DEADLINE admission, or a
    ///   negative nice, which depends on `CAP_SYS_NICE` and `RLIMIT_RTPRIO`.
    /// * EOPNOTSUPP for util-clamp on a VER1 buffer, which depends on
    ///   `CONFIG_UCLAMP_TASK`.
    /// * The `sysctl_sched_dl_period_{min,max}` bound on SCHED_DEADLINE
    ///   periods, which is runtime-tunable.
    ///
    /// In all three Hermit accepts the request as the same no-op as any other
    /// well-formed one. The same rule is why SCHED_EXT (policy 7) is accepted
    /// unconditionally even though `valid_policy()` admits it only with
    /// `CONFIG_SCHED_CLASS_EXT`: picking one answer keeps the sandbox stable
    /// across hosts, and accepting is the choice consistent with suppressing
    /// the effect rather than refusing the request.
    ///
    /// The bracketed regression test in `hermit-cli/tests/sched_setattr_abi.rs`
    /// compares Hermit against the running kernel case by case, and therefore
    /// deliberately omits exactly these cases -- including them would make its
    /// verdict depend on the host it runs on.
    pub async fn handle_sched_setattr<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedSetattr,
    ) -> Result<i64, Error> {
        // `if (!uattr || pid < 0 || flags)` -- one clause in the kernel, so all
        // three are EINVAL and none of them is ordered against the others. A
        // null pointer is EINVAL rather than EFAULT because it is refused
        // before any access is attempted.
        if call.flags() != 0 || call.pid() < 0 {
            return Err(Errno::EINVAL.into());
        }
        let attr = call.attr().ok_or(Errno::EINVAL)?;

        // `get_user(size, &uattr->size)` -- the declared size is read before
        // anything else, and a fault here is EFAULT with no store back.
        // `AddrMut` is not `Copy` and `cast` consumes it, so each access below
        // re-derives the address from the syscall argument.
        // A fault reading guest memory is EFAULT for this syscall. The
        // backend's memory layer reports a failed peek as EIO, which is not an
        // errno `sched_setattr` can return, so it is mapped here; measured
        // native, an unmapped `uattr` gives EFAULT.
        let declared: u32 = guest
            .memory()
            .read_value(attr.cast())
            .map_err(|_| Errno::EFAULT)?;

        // Both E2BIG exits below run the kernel's `err_size` path, which stores
        // the kernel's own struct size into `uattr->size` before returning. A
        // guest that reads the field back learns the size it should have sent,
        // which is the entire point of the store; the kernel ignores whether it
        // succeeds, and so do we.
        fn refuse_too_big<S: MemoryAccess>(mut memory: S, attr: AddrMut<libc::c_void>) -> Error {
            let _ = memory.write_value(attr.cast::<u32>(), &SCHED_ATTR_KERNEL_SIZE);
            Errno::E2BIG.into()
        }

        let size = match sched_attr_effective_size(declared) {
            Ok(size) => size,
            Err(()) => {
                let back = call.attr().ok_or(Errno::EINVAL)?;
                return Err(refuse_too_big(guest.memory(), back));
            }
        };

        // `copy_struct_from_user` with a user size past the kernel's struct:
        // every trailing byte must be zero, or the guest is sending a field
        // this kernel does not know and must not silently drop. A fault while
        // scanning is EFAULT and skips the store back -- only the not-all-zero
        // verdict is E2BIG.
        if size > SCHED_ATTR_KERNEL_SIZE {
            let base: AddrMut<u8> = call.attr().ok_or(Errno::EINVAL)?.cast();
            match scan_tail_is_zeroed(
                &guest.memory(),
                base,
                SCHED_ATTR_KERNEL_SIZE as usize,
                (size - SCHED_ATTR_KERNEL_SIZE) as usize,
            ) {
                TailVerdict::AllZero => {}
                TailVerdict::NotZeroed => {
                    let back = call.attr().ok_or(Errno::EINVAL)?;
                    return Err(refuse_too_big(guest.memory(), back));
                }
                TailVerdict::Faulted => return Err(Errno::EFAULT.into()),
            }
        }

        // Copy the interoperable prefix, zero-filling anything the guest's
        // buffer is too short to carry, exactly as `copy_struct_from_user`
        // does for a short read.
        let copied = std::cmp::min(size, SCHED_ATTR_KERNEL_SIZE) as usize;
        let mut raw = [0u8; SCHED_ATTR_KERNEL_SIZE as usize];
        let base: AddrMut<u8> = call.attr().ok_or(Errno::EINVAL)?.cast();
        guest
            .memory()
            .read_exact(base, &mut raw[..copied])
            .map_err(|_| Errno::EFAULT)?;
        let field32 = |offset: usize| -> u32 {
            u32::from_ne_bytes(raw[offset..offset + 4].try_into().expect("4 bytes"))
        };
        let field64 = |offset: usize| -> u64 {
            u64::from_ne_bytes(raw[offset..offset + 8].try_into().expect("8 bytes"))
        };
        let fields = SchedAttrFields {
            policy: field32(SCHED_ATTR_OFF_POLICY),
            sched_flags: field64(SCHED_ATTR_OFF_FLAGS),
            priority: field32(SCHED_ATTR_OFF_PRIORITY),
            runtime: field64(SCHED_ATTR_OFF_RUNTIME),
            deadline: field64(SCHED_ATTR_OFF_DEADLINE),
            period: field64(SCHED_ATTR_OFF_PERIOD),
        };

        // Everything the kernel decides before it resolves the pid.
        validate_sched_attr_before_lookup(size, &fields)?;

        // `find_process_by_pid()` sits here, between the two groups of checks,
        // and the position is guest-visible: a request that is malformed only
        // in a way judged below reports ESRCH rather than EINVAL when the pid
        // does not exist. pid 0 means the calling thread, which always exists;
        // otherwise ask the scheduler's own task table, so the answer comes
        // from Detcore's state rather than from the host's process table.
        // `find_task_by_vpid` resolves ANY LIVE TASK, not just a thread-group
        // leader. The kill-target resolver is the wrong question here: it
        // models `kill(2)`, whose first act is to refuse anything that is not a
        // leader, so a perfectly live non-leader thread's tid reported ESRCH.
        if call.pid() != 0 && !thread_is_live(guest, DetTid::from_raw(call.pid())).await {
            return Err(Errno::ESRCH.into());
        }

        // And everything `__sched_setscheduler` decides after it.
        validate_sched_attr_after_lookup(&fields)?;

        info!(
            "Suppressing sched_setattr(pid={}, flags={}); Linux scheduler attributes are virtual",
            call.pid(),
            call.flags()
        );
        Ok(0)
    }

    /// ioprio_set under Hermit. Detcore serializes guest threads onto one virtual
    /// CPU, so the block-layer I/O scheduling class and priority cannot change
    /// guest-visible computation. Accept and suppress the request as a
    /// deterministic no-op success, mirroring how sched_setaffinity is handled.
    /// Re-enables `ionice` under --strict.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#791)
    pub async fn handle_ioprio_set<G: Guest<Self>>(
        &self,
        _guest: &mut G,
        call: syscalls::IoprioSet,
    ) -> Result<i64, Error> {
        info!(
            "Suppressing ioprio_set(which={}, who={}, priority={}); I/O priority is virtual",
            call.which(),
            call.who(),
            call.priority()
        );
        Ok(0)
    }

    /// ioprio_get under Hermit. I/O priority is inert under Detcore's serialized
    /// scheduler, so process queries observe the fixed raw IOPRIO_CLASS_NONE
    /// value while group/user queries observe the effective SCHED_OTHER default
    /// of IOPRIO_CLASS_BE/4, without consulting host block-scheduler state.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-881)
    pub async fn handle_ioprio_get<G: Guest<Self>>(
        &self,
        _guest: &mut G,
        call: syscalls::IoprioGet,
    ) -> Result<i64, Error> {
        let priority = virtual_ioprio(call.which())?;

        info!(
            "Emulating ioprio_get(which={}, who={}): fixed priority {}",
            call.which(),
            call.who(),
            priority
        );
        Ok(priority)
    }
}

/// The one real implementation of [`robust_list::RobustDeathEffects`]: guest
/// memory plus a wake against Detcore's modeled futex waiter pool.
///
/// It exists so the walk itself never mentions `Guest`, and can therefore be
/// unit-tested against fake memory.
struct GuestRobustEffects<'a, G, T> {
    guest: &'a mut G,
    dettid: DetTid,
    defer_owner_death_to_backend: bool,
    staged_wakes: Option<&'a mut Vec<RobustListWake>>,
    tool: PhantomData<T>,
}

impl<G, T> robust_list::RobustDeathEffects for GuestRobustEffects<'_, G, T>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    fn read_u64(&mut self, address: usize) -> Option<u64> {
        let at = Addr::<u64>::from_raw(address)?;
        self.guest.memory().read_value::<_, u64>(at).ok()
    }

    fn read_u32(&mut self, address: usize) -> Option<u32> {
        let at = Addr::<u32>::from_raw(address)?;
        self.guest.memory().read_value::<_, u32>(at).ok()
    }

    fn compare_and_swap(
        &mut self,
        address: usize,
        expected: u32,
        desired: u32,
    ) -> robust_list::FutexCasOutcome {
        use robust_list::FutexCasOutcome;

        let (Some(read_at), Some(write_at)) = (
            Addr::<u32>::from_raw(address),
            AddrMut::<u32>::from_raw(address),
        ) else {
            return FutexCasOutcome::Faulted;
        };
        // Reverie's guest-memory interface offers reads and writes, not a
        // cross-address-space cmpxchg, so this re-reads the word immediately
        // before storing and refuses to store when the value has moved.
        //
        // The dying task keeps the scheduler turn for the whole walk, so no
        // modeled thread can run between this read and the write. That does not
        // make the operations atomic against a process outside the model. Only
        // ptrace can safely take the deferred branch below; DBT and SaBRe run
        // their tool-exit callbacks before native cleanup, and KVM has none.
        let observed = match self.guest.memory().read_value::<_, u32>(read_at) {
            Ok(value) => value,
            Err(_) => return FutexCasOutcome::Faulted,
        };
        if observed != expected {
            return FutexCasOutcome::Changed(observed);
        }
        if self.defer_owner_death_to_backend {
            // Ptrace keeps this syscall handler pending through the native
            // exit and does not release another scheduler turn until Linux has
            // repeated the owner check and changed the word atomically. Do not
            // perform a separate write here: a process outside Hermit's
            // scheduler can share the mapping and acquire the mutex between
            // this read and that write.
            debug!(
                "[detcore, dtid {}] robust-list owner death: leaving futex word {:#x} for backend exit cleanup",
                self.dettid, address,
            );
            return FutexCasOutcome::Deferred;
        }
        // `MemoryAccess::write_value` writes through to the guest immediately
        // (`write_exact`); it is not the scratch-stack path, which buffers
        // until `commit()`.
        if self.guest.memory().write_value(write_at, &desired).is_err() {
            return FutexCasOutcome::Faulted;
        }
        // Deliberately DEBUG, not INFO: this line carries a raw guest address,
        // and INFO is the surface `--verify-strict` compares. The
        // KVM diagnostics read this from `--log=debug`; keep it below INFO
        // because the address is not a deterministic observation.
        debug!(
            "[detcore, dtid {}] robust-list owner death: futex word {:#x} {:#x} -> {:#x}",
            self.dettid, address, expected, desired,
        );
        FutexCasOutcome::Stored
    }

    async fn wake_one(&mut self, address: usize, observed: u32) {
        // glibc always issues robust-mutex futex operations with the shared
        // flag, and so does the kernel's owner-death wake; resolve the same key.
        let futexid = self.guest.thread_state().futex_id(address, false);
        if let Some(wakes) = self.staged_wakes.as_mut() {
            wakes.push(RobustListWake { futex: futexid });
            debug!(
                "[detcore, dtid {}] staged robust-list owner-death wake until physical exit",
                self.dettid,
            );
            return;
        }
        let woken = match futex_action(
            self.guest,
            FutexAction::WakeRequest(1),
            &futexid,
            observed as i32,
            u32::MAX,
        )
        .await
        {
            Some(SchedValue::Value(count)) => count,
            // A wake never carries a timeout, and a cancelled RPC wakes nobody.
            Some(SchedValue::TimeOut) | None => 0,
        };
        // Guest-level identities only: dettid, the modeled futex key, and a
        // count. No host pointers and no iteration order leak into this line,
        // which is compared exactly under `--verify-strict`.
        info!(
            "[detcore, dtid {}] robust-list owner death woke {} waiter(s) on futex {:?}",
            self.dettid, woken, futexid,
        );
        let _ = futex_action(
            self.guest,
            FutexAction::WakeFinished(0),
            &futexid,
            observed as i32,
            u32::MAX,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_blocked_mask_preserves_libc_signal_membership() {
        let mask = blocked_signal_mask();
        let mut libc_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigfillset(&mut libc_mask);
            libc::sigdelset(&mut libc_mask, reverie::PERF_EVENT_SIGNAL as i32);
        }
        for raw_signal in 1..=KernelSigset::BITS as i32 {
            assert_eq!(
                signal_is_blocked(&mask, SigWrapper(raw_signal)),
                unsafe { libc::sigismember(&libc_mask, raw_signal) == 1 },
                "signal {raw_signal} membership changed while converting to the kernel ABI"
            );
        }
    }

    #[test]
    fn clone_without_child_cleartid_does_not_register_the_pointer() {
        assert_eq!(
            child_tid_clear_address(CloneFlags::CLONE_CHILD_SETTID, 0x1234),
            0
        );
    }

    #[test]
    fn clone_with_child_cleartid_registers_the_pointer() {
        assert_eq!(
            child_tid_clear_address(CloneFlags::CLONE_CHILD_CLEARTID, 0x1234),
            0x1234
        );
    }

    #[test]
    fn linux_default_dispositions_that_do_not_interrupt_child_waits() {
        for signal in [libc::SIGCHLD, libc::SIGCONT, libc::SIGURG, libc::SIGWINCH] {
            assert!(signal_default_disposition_does_not_interrupt_child_wait(
                SigWrapper(signal)
            ));
        }
        for signal in [libc::SIGALRM, libc::SIGSTOP, libc::SIGUSR1] {
            assert!(!signal_default_disposition_does_not_interrupt_child_wait(
                SigWrapper(signal)
            ));
        }
    }

    #[test]
    fn uncatchable_signals_do_not_require_a_sigaction_query() {
        assert!(signal_has_uncatchable_default_disposition(SigWrapper(
            libc::SIGKILL
        )));
        assert!(signal_has_uncatchable_default_disposition(SigWrapper(
            libc::SIGSTOP
        )));
        assert!(!signal_has_uncatchable_default_disposition(SigWrapper(
            libc::SIGUSR1
        )));
    }

    #[test]
    fn waitid_ready_child_wins_when_scheduler_also_reports_a_signal() {
        assert_eq!(
            exact_wait_poll_decision(true, true, Some(ExactChildWaitState::Running)),
            ExactWaitPollDecision::ChildReady
        );
        assert_eq!(
            exact_wait_poll_decision(false, true, Some(ExactChildWaitState::LogicallyExited)),
            ExactWaitPollDecision::ReapAfterLogicalExit
        );
        assert_eq!(
            exact_wait_poll_decision(false, true, Some(ExactChildWaitState::PhysicalExitPending)),
            ExactWaitPollDecision::AwaitPhysicalExit
        );
        assert_eq!(
            exact_wait_poll_decision(false, true, Some(ExactChildWaitState::Running)),
            ExactWaitPollDecision::Interrupted
        );
        assert_eq!(
            exact_wait_poll_decision(false, false, Some(ExactChildWaitState::Running)),
            ExactWaitPollDecision::Retry
        );
    }

    #[test]
    fn stale_any_child_preserves_interrupt_until_no_ready_child_remains() {
        let next_child = DetPid::from_raw(200);

        assert!(
            !stale_any_wait_must_interrupt(true, Some(next_child)),
            "another ready child must retain child-ready precedence"
        );
        assert!(
            stale_any_wait_must_interrupt(true, None),
            "a pending signal must interrupt before the wait parks again"
        );
        assert!(!stale_any_wait_must_interrupt(false, None));
    }

    #[test]
    fn ioprio_query_reports_fixed_raw_and_effective_defaults() {
        assert_eq!(virtual_ioprio(IOPRIO_WHO_PROCESS), Ok(0));
        assert_eq!(
            virtual_ioprio(IOPRIO_WHO_PGRP),
            Ok(i64::from(IOPRIO_DEFAULT_EFFECTIVE))
        );
        assert_eq!(
            virtual_ioprio(IOPRIO_WHO_USER),
            Ok(i64::from(IOPRIO_DEFAULT_EFFECTIVE))
        );
        assert_eq!(virtual_ioprio(0), Err(Errno::EINVAL));
        assert_eq!(virtual_ioprio(4), Err(Errno::EINVAL));
    }

    #[test]
    fn waitid_siginfo_canonicalization_clears_only_cpu_accounting() {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        info.si_signo = libc::SIGCHLD;
        info.si_code = libc::CLD_EXITED;
        // SAFETY: This uses the same Linux SIGCHLD layout mirror validated by
        // canonicalize_waitid_siginfo.
        let fields = unsafe {
            &mut (*(std::ptr::addr_of_mut!(info)).cast::<WaitidSiginfoHead>())
                .fields
                .sigchld
        };
        fields.pid = 123;
        fields.uid = 456;
        fields.status = 7;
        fields.utime = 8;
        fields.stime = 9;

        canonicalize_waitid_siginfo(&mut info);

        assert_eq!(info.si_signo, libc::SIGCHLD);
        assert_eq!(info.si_code, libc::CLD_EXITED);
        assert_eq!(unsafe { info.si_pid() }, 123);
        assert_eq!(unsafe { info.si_uid() }, 456);
        assert_eq!(unsafe { info.si_status() }, 7);
        assert_eq!(unsafe { info.si_utime() }, 0);
        assert_eq!(unsafe { info.si_stime() }, 0);
    }

    #[test]
    fn wait_status_rollup_only_accepts_process_termination() {
        assert!(wait_status_is_termination(0));
        assert!(wait_status_is_termination(libc::SIGTERM));
        assert!(!wait_status_is_termination((libc::SIGSTOP << 8) | 0x7f));
        assert!(!wait_status_is_termination(0xffff));

        assert!(waitid_code_is_termination(libc::CLD_EXITED));
        assert!(waitid_code_is_termination(libc::CLD_KILLED));
        assert!(waitid_code_is_termination(libc::CLD_DUMPED));
        assert!(!waitid_code_is_termination(libc::CLD_STOPPED));
        assert!(!waitid_code_is_termination(libc::CLD_CONTINUED));
        assert!(!waitid_code_is_termination(libc::CLD_TRAPPED));
    }

    #[test]
    fn futex_timeout_units_and_modes_match_linux() {
        let timeout = Timespec {
            tv_sec: 2,
            tv_nsec: 3,
        };
        assert_eq!(
            parse_futex_timeout(libc::FUTEX_WAIT, timeout),
            Ok(FutexTimeout::Relative(2_000_000_003))
        );
        assert_eq!(
            parse_futex_timeout(libc::FUTEX_WAIT_BITSET, timeout),
            Ok(FutexTimeout::Absolute(LogicalTime::from_nanos(
                2_000_000_003
            )))
        );
        // The command bits must be matched after masking off FUTEX_PRIVATE_FLAG
        // (and FUTEX_CLOCK_REALTIME): a private FUTEX_WAIT_BITSET still uses an
        // absolute deadline, and a private FUTEX_WAIT still uses a relative one.
        assert_eq!(
            parse_futex_timeout(libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG, timeout),
            Ok(FutexTimeout::Absolute(LogicalTime::from_nanos(
                2_000_000_003
            )))
        );
        assert_eq!(
            parse_futex_timeout(libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG, timeout),
            Ok(FutexTimeout::Relative(2_000_000_003))
        );
    }

    #[test]
    fn absolute_futex_timeout_is_rebased_to_logical_time() {
        let logical_now = LogicalTime::from_secs(100);
        let clock_now = LogicalTime::from_secs(5_000);
        let deadline = clock_now + Duration::from_millis(100);
        assert_eq!(
            rebase_absolute_timeout(deadline, clock_now, logical_now),
            logical_now + Duration::from_millis(100)
        );
        assert_eq!(
            rebase_absolute_timeout(
                clock_now - LogicalTime::from_nanos(1),
                clock_now,
                logical_now
            ),
            logical_now
        );
    }

    #[test]
    fn absolute_futex_timeout_detects_host_and_logical_clock_domains() {
        let host_monotonic_now = LogicalTime::from_secs(374_766);
        let logical_now = LogicalTime::from_secs(1_640_995_199);
        let delta = Duration::from_millis(100);

        assert!(absolute_timeout_uses_host_clock(
            host_monotonic_now + delta,
            host_monotonic_now,
            logical_now
        ));
        assert!(!absolute_timeout_uses_host_clock(
            logical_now + delta,
            host_monotonic_now,
            logical_now
        ));

        let host_realtime_now = LogicalTime::from_secs(1_785_142_800);
        assert!(absolute_timeout_uses_host_clock(
            host_realtime_now + delta,
            host_realtime_now,
            logical_now
        ));
    }

    #[test]
    fn futex_timeout_rejects_invalid_timespecs() {
        assert_eq!(
            parse_futex_timeout(
                libc::FUTEX_WAIT,
                Timespec {
                    tv_sec: -1,
                    tv_nsec: 0,
                },
            ),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            parse_futex_timeout(
                libc::FUTEX_WAIT_BITSET,
                Timespec {
                    tv_sec: 0,
                    tv_nsec: 1_000_000_000,
                },
            ),
            Err(Errno::EINVAL)
        );
    }

    // The expectations below are not guesses at the kernel's behaviour: each is
    // a row measured natively on this host (Linux 6.19, x86-64) with a raw
    // `sched_setattr` probe. Where a row differs from what the handler used to
    // do, the difference is called out.

    /// A well-formed VER0 SCHED_OTHER descriptor, as the decoded fields.
    fn plain_attr() -> SchedAttrFields {
        SchedAttrFields {
            policy: 0,
            sched_flags: 0,
            priority: 0,
            runtime: 0,
            deadline: 0,
            period: 0,
        }
    }

    #[test]
    fn size_zero_is_a_well_formed_ver0_request() {
        // ABI compatibility quirk: `if (!size) size = SCHED_ATTR_SIZE_VER0;`.
        // Measured native: ret 0. PR #2288 returned E2BIG.
        assert_eq!(sched_attr_effective_size(0), Ok(SCHED_ATTR_SIZE_VER0));
    }

    #[test]
    fn size_below_ver0_or_past_a_page_is_too_big() {
        // Measured native: size 1, 47 and 4097 all give E2BIG.
        assert_eq!(sched_attr_effective_size(1), Err(()));
        assert_eq!(sched_attr_effective_size(SCHED_ATTR_SIZE_VER0 - 1), Err(()));
        assert_eq!(sched_attr_effective_size(SCHED_ATTR_MAX_SIZE + 1), Err(()));
    }

    #[test]
    fn size_from_ver0_through_one_page_is_accepted_unchanged() {
        // Measured native: 48, 56, 57 and 4096 all give ret 0.
        for size in [
            SCHED_ATTR_SIZE_VER0,
            SCHED_ATTR_SIZE_VER1,
            SCHED_ATTR_SIZE_VER1 + 1,
            SCHED_ATTR_MAX_SIZE,
        ] {
            assert_eq!(sched_attr_effective_size(size), Ok(size), "size {}", size);
        }
    }

    #[test]
    fn sched_ext_is_a_valid_policy_and_sched_iso_is_not() {
        // Measured native: policy 7 gives ret 0; policy 4 gives EINVAL. PR
        // #2288 refused 7.
        assert!(is_valid_sched_policy(SCHED_EXT), "SCHED_EXT is accepted");
        assert!(!is_valid_sched_policy(4), "SCHED_ISO is reserved");
        for policy in [0, 1, 2, 3, 5, 6] {
            assert!(is_valid_sched_policy(policy), "policy {}", policy);
        }
        // Measured native: policy 8 and 99 both give EINVAL.
        for policy in [8, 99] {
            assert!(!is_valid_sched_policy(policy), "policy {}", policy);
        }
    }

    // ---- which side of the pid lookup each rule falls on --------------------
    // The kernel resolves the pid between `sched_setattr()` and
    // `__sched_setscheduler()`, so a request naming a nonexistent pid reports
    // ESRCH for every rule on the far side and its own errno for every rule on
    // the near side. Measured natively with pid 0x3fffffff:
    //     bad sched_flags    -> ESRCH    (far side)
    //     bad priority       -> ESRCH    (far side)
    //     negative policy    -> EINVAL   (near side)
    //     util-clamp size 48 -> EINVAL   (near side)
    // The split between the two functions below is exactly that boundary.

    #[test]
    fn util_clamp_size_rule_is_decided_before_the_pid_lookup() {
        // Measured native: EINVAL even with a nonexistent pid.
        let mut attr = plain_attr();
        attr.sched_flags = SCHED_FLAG_UTIL_CLAMP_MIN;
        assert_eq!(
            validate_sched_attr_before_lookup(SCHED_ATTR_SIZE_VER0, &attr),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            validate_sched_attr_before_lookup(SCHED_ATTR_SIZE_VER1, &attr),
            Ok(())
        );
        // ...and it is not re-decided on the far side.
        assert_eq!(validate_sched_attr_after_lookup(&attr), Ok(()));
    }

    #[test]
    fn a_negative_policy_is_decided_before_the_pid_lookup() {
        // Measured native: EINVAL even with a nonexistent pid, and EINVAL with
        // KEEP_POLICY too -- the signed test sits above the substitution.
        let mut attr = plain_attr();
        attr.policy = 0x8000_0000;
        assert_eq!(
            validate_sched_attr_before_lookup(SCHED_ATTR_SIZE_VER0, &attr),
            Err(Errno::EINVAL)
        );
        attr.sched_flags = SCHED_FLAG_KEEP_POLICY;
        assert_eq!(
            validate_sched_attr_before_lookup(SCHED_ATTR_SIZE_VER0, &attr),
            Err(Errno::EINVAL),
            "KEEP_POLICY must not hide a negative policy"
        );
    }

    #[test]
    fn policy_and_flag_rules_are_decided_after_the_pid_lookup() {
        // Measured native: with a nonexistent pid both of these report ESRCH,
        // so neither may be decided on the near side.
        let mut bad_policy = plain_attr();
        bad_policy.policy = 99;
        assert_eq!(
            validate_sched_attr_before_lookup(SCHED_ATTR_SIZE_VER0, &bad_policy),
            Ok(()),
            "an undefined policy must survive the near side so ESRCH can win"
        );
        assert_eq!(
            validate_sched_attr_after_lookup(&bad_policy),
            Err(Errno::EINVAL)
        );

        let mut bad_flag = plain_attr();
        bad_flag.sched_flags = 0x80;
        assert_eq!(
            validate_sched_attr_before_lookup(SCHED_ATTR_SIZE_VER0, &bad_flag),
            Ok(()),
            "an undefined sched_flags bit must survive the near side"
        );
        assert_eq!(
            validate_sched_attr_after_lookup(&bad_flag),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn keep_policy_makes_the_policy_field_irrelevant() {
        // Measured native: policy 99 alone is EINVAL, but policy 99 with
        // SCHED_FLAG_KEEP_POLICY is ret 0 -- the kernel overwrites the field
        // with SETPARAM_POLICY and never runs valid_policy(). PR #2288 refused
        // it.
        let mut attr = plain_attr();
        attr.policy = 99;
        assert_eq!(validate_sched_attr_after_lookup(&attr), Err(Errno::EINVAL));
        attr.sched_flags = SCHED_FLAG_KEEP_POLICY;
        assert_eq!(validate_sched_attr_after_lookup(&attr), Ok(()));
    }

    #[test]
    fn defined_sched_flags_bits_are_accepted_and_undefined_ones_are_not() {
        // Measured native: sched_flags 0x80 gives EINVAL; RESET_ON_FORK,
        // KEEP_PARAMS, KEEP_ALL, RECLAIM and DL_OVERRUN are all ret 0 at
        // size 48. PR #2288 ignored sched_flags entirely.
        let mut attr = plain_attr();
        attr.sched_flags = 0x80;
        assert_eq!(validate_sched_attr_after_lookup(&attr), Err(Errno::EINVAL));
        for flag in [
            SCHED_FLAG_RESET_ON_FORK,
            SCHED_FLAG_RECLAIM,
            SCHED_FLAG_DL_OVERRUN,
            SCHED_FLAG_KEEP_PARAMS,
            SCHED_FLAG_KEEP_POLICY,
        ] {
            let mut ok = plain_attr();
            ok.sched_flags = flag;
            assert_eq!(
                validate_sched_attr_after_lookup(&ok),
                Ok(()),
                "flag {:#x}",
                flag
            );
        }
        // The whole mask at once is refused on the NEAR side, and for the
        // util-clamp size reason rather than the flag-validity one: measured
        // native EINVAL at size 48.
        let mut all = plain_attr();
        all.sched_flags = SCHED_FLAG_ALL;
        assert_eq!(
            validate_sched_attr_before_lookup(SCHED_ATTR_SIZE_VER0, &all),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            validate_sched_attr_before_lookup(SCHED_ATTR_SIZE_VER1, &all),
            Ok(())
        );
        assert_eq!(validate_sched_attr_after_lookup(&all), Ok(()));
    }

    #[test]
    fn priority_must_agree_with_the_policy() {
        // The kernel states this as `rt_policy(policy) != (prio != 0)`.
        // Measured native, all EINVAL: OTHER/BATCH/IDLE with priority 1, and
        // FIFO/RR with priority 0. PR #2288 accepted every one of them.
        for policy in [0u32, 3, 5] {
            let mut attr = plain_attr();
            attr.policy = policy;
            attr.priority = 1;
            assert_eq!(
                validate_sched_attr_after_lookup(&attr),
                Err(Errno::EINVAL),
                "policy {} with priority 1",
                policy
            );
            attr.priority = 0;
            assert_eq!(
                validate_sched_attr_after_lookup(&attr),
                Ok(()),
                "policy {} with priority 0",
                policy
            );
        }
        for policy in [SCHED_FIFO, SCHED_RR] {
            let mut attr = plain_attr();
            attr.policy = policy;
            attr.priority = 0;
            assert_eq!(
                validate_sched_attr_after_lookup(&attr),
                Err(Errno::EINVAL),
                "rt policy {} with priority 0",
                policy
            );
            // A real-time priority is accepted here; whether the *caller* may
            // ask for it is EPERM, which depends on privilege and is not
            // emulated.
            attr.priority = 1;
            assert_eq!(validate_sched_attr_after_lookup(&attr), Ok(()));
        }
    }

    #[test]
    fn a_priority_past_max_rt_prio_is_refused() {
        // Measured native: FIFO with priority 100 is EINVAL rather than the
        // EPERM that priorities 1..=99 give, so the range test runs first.
        let mut attr = plain_attr();
        attr.policy = SCHED_FIFO;
        attr.priority = MAX_RT_PRIO;
        assert_eq!(validate_sched_attr_after_lookup(&attr), Err(Errno::EINVAL));
        attr.priority = MAX_RT_PRIO - 1;
        assert_eq!(validate_sched_attr_after_lookup(&attr), Ok(()));
    }

    #[test]
    fn deadline_parameters_are_checked() {
        // Measured native: SCHED_DEADLINE with all-zero parameters is EINVAL,
        // and with runtime > deadline is EINVAL. PR #2288 accepted both.
        let mut zeroed = plain_attr();
        zeroed.policy = SCHED_DEADLINE;
        assert_eq!(
            validate_sched_attr_after_lookup(&zeroed),
            Err(Errno::EINVAL),
            "all-zero deadline parameters"
        );

        let mut inverted = plain_attr();
        inverted.policy = SCHED_DEADLINE;
        inverted.runtime = 90_000_000;
        inverted.deadline = 30_000_000;
        inverted.period = 30_000_000;
        assert_eq!(
            validate_sched_attr_after_lookup(&inverted),
            Err(Errno::EINVAL),
            "runtime > deadline"
        );

        // Sane parameters pass the ABI rules. Natively this host answers EPERM,
        // which is a privilege question rather than an ABI one.
        let mut sane = plain_attr();
        sane.policy = SCHED_DEADLINE;
        sane.runtime = 10_000_000;
        sane.deadline = 30_000_000;
        sane.period = 30_000_000;
        assert_eq!(validate_sched_attr_after_lookup(&sane), Ok(()));
    }

    #[test]
    fn deadline_parameter_edges_follow_checkparam_dl() {
        // deadline == 0 is refused whatever else is set.
        assert!(!deadline_params_are_valid(1 << 20, 0, 0));
        // runtime below the DL_SCALE truncation floor (1 << 10) is refused.
        assert!(!deadline_params_are_valid((1 << 10) - 1, 1 << 20, 1 << 20));
        assert!(deadline_params_are_valid(1 << 10, 1 << 20, 1 << 20));
        // The MSB is reserved on both deadline and period.
        assert!(!deadline_params_are_valid(1 << 20, 1 << 63, 0));
        assert!(!deadline_params_are_valid(1 << 20, 1 << 20, 1 << 63));
        // A zero period means "same as the deadline", so this is runtime <=
        // deadline <= deadline and is accepted.
        assert!(deadline_params_are_valid(1 << 20, 1 << 21, 0));
        // deadline > period is refused.
        assert!(!deadline_params_are_valid(1 << 20, 1 << 22, 1 << 21));
    }

    #[test]
    fn the_size_written_back_on_refusal_is_the_kernel_struct_not_the_libc_mirror() {
        // Measured native: every E2BIG row leaves uattr->size holding 56, not
        // 48. Deriving that from the libc crate's `sched_attr` would report 48
        // and tell the guest to retry with a size the kernel already accepted.
        assert_eq!(SCHED_ATTR_KERNEL_SIZE, 56);
        assert_eq!(std::mem::size_of::<libc::sched_attr>(), 48);
        assert_ne!(
            SCHED_ATTR_KERNEL_SIZE as usize,
            std::mem::size_of::<libc::sched_attr>()
        );
    }

    /// A `MemoryAccess` whose readable region ends at `readable_len`, recording
    /// every read length it is asked for.
    struct BoundedMemory {
        bytes: Vec<u8>,
        readable_len: usize,
        reads: std::cell::RefCell<Vec<usize>>,
    }

    impl reverie::syscalls::MemoryAccess for BoundedMemory {
        fn read_vectored(
            &self,
            read_from: &[std::io::IoSlice<'_>],
            write_to: &mut [std::io::IoSliceMut<'_>],
        ) -> Result<usize, Errno> {
            let start = read_from[0].as_ptr() as usize;
            let want = read_from.iter().map(|slice| slice.len()).sum::<usize>();
            self.reads.borrow_mut().push(want);
            if start >= self.readable_len {
                return Err(Errno::EFAULT);
            }
            let avail = (self.readable_len - start).min(want);
            let mut copied = 0;
            for out in write_to.iter_mut() {
                if copied == avail {
                    break;
                }
                let take = out.len().min(avail - copied);
                out[..take].copy_from_slice(&self.bytes[start + copied..start + copied + take]);
                copied += take;
            }
            Ok(copied)
        }

        fn write_vectored(
            &mut self,
            _read_from: &[std::io::IoSlice<'_>],
            _write_to: &mut [std::io::IoSliceMut<'_>],
        ) -> Result<usize, Errno> {
            unimplemented!("this fixture never writes")
        }
    }

    fn bounded(bytes: Vec<u8>, readable_len: usize) -> BoundedMemory {
        BoundedMemory {
            bytes,
            readable_len,
            reads: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn tail_base() -> AddrMut<'static, u8> {
        AddrMut::<u8>::from_raw(1).expect("nonzero base")
    }

    /// ITEM 2 REGRESSION, ORDERING HALF: a non-zero byte BEFORE an unreadable
    /// page is E2BIG, not EFAULT.
    ///
    /// `check_zeroed_user` scans forward and stops at the first thing it finds,
    /// so it never reaches the fault. Reading the whole tail up front and
    /// judging afterwards turns this into EFAULT and changes the errno the
    /// guest sees.
    #[test]
    fn a_nonzero_byte_before_a_fault_is_e2big_not_efault() {
        let off = SCHED_ATTR_KERNEL_SIZE as usize;
        let mut bytes = vec![0u8; off + 4096];
        bytes[off + 1] = 0xAA; // non-zero, well before the boundary
        let readable = off + 512; // everything past this faults
        let memory = bounded(bytes, readable);
        assert_eq!(
            scan_tail_is_zeroed(&memory, tail_base(), off, 4096),
            TailVerdict::NotZeroed,
            "a non-zero byte the scan reaches first must win over a later fault"
        );
    }

    /// ITEM 2 REGRESSION, the other side of the same order: a fault with
    /// nothing non-zero before it is EFAULT.
    #[test]
    fn a_fault_before_any_nonzero_byte_is_efault() {
        let off = SCHED_ATTR_KERNEL_SIZE as usize;
        let bytes = vec![0u8; off + 4096];
        let memory = bounded(bytes, off + 512);
        assert_eq!(
            scan_tail_is_zeroed(&memory, tail_base(), off, 4096),
            TailVerdict::Faulted,
            "an unreadable byte reached before anything non-zero must be EFAULT"
        );
    }

    /// ITEM 2 REGRESSION, PROTECTION HALF: never issue a read of eight bytes or
    /// fewer.
    ///
    /// safeptrace serves those through `PTRACE_PEEKDATA`, which reads a whole
    /// aligned word and bypasses guest page protections, so a small tail would
    /// be readable under Hermit where Linux reports EFAULT. The assertion is on
    /// the READ LENGTHS ASKED FOR, because that is the mechanism; the returned
    /// verdict cannot show it.
    #[test]
    fn tail_reads_are_never_small_enough_to_bypass_guest_protection() {
        let off = SCHED_ATTR_KERNEL_SIZE as usize;
        for tail_len in 1..=16usize {
            let bytes = vec![0u8; off + tail_len + 64];
            let memory = bounded(bytes, off + tail_len + 64);
            assert_eq!(
                scan_tail_is_zeroed(&memory, tail_base(), off, tail_len),
                TailVerdict::AllZero
            );
            let reads = memory.reads.borrow().clone();
            assert!(!reads.is_empty(), "tail_len {tail_len} issued no read");
            for length in reads {
                assert!(
                    length > std::mem::size_of::<u64>(),
                    "tail_len {tail_len} issued a {length}-byte read, which safeptrace \
                     serves with PTRACE_PEEKDATA and which therefore bypasses guest \
                     page protections"
                );
            }
        }
    }

    /// ITEM 3 REGRESSION: KEEP_POLICY reuses the CURRENT policy; it does not
    /// switch the policy-dependent rules off.
    ///
    /// Detcore's current policy is the one `handle_sched_getattr` reports for
    /// every thread, SCHED_OTHER, and a non-real-time policy requires priority
    /// 0. Skipping the rules accepted this.
    #[test]
    fn keep_policy_validates_against_the_virtual_current_policy() {
        let attr = SchedAttrFields {
            policy: 99, // ignored under KEEP_POLICY, and deliberately invalid
            sched_flags: SCHED_FLAG_KEEP_POLICY,
            priority: 1,
            runtime: 0,
            deadline: 0,
            period: 0,
        };
        assert_eq!(
            validate_sched_attr_after_lookup(&attr),
            Err(Errno::EINVAL),
            "priority 1 under the virtual SCHED_OTHER current policy must be refused"
        );

        // The companion that must keep passing: the ignored policy field really
        // is ignored, so the same request at priority 0 is accepted.
        let ok = SchedAttrFields {
            priority: 0,
            ..attr
        };
        assert_eq!(
            validate_sched_attr_after_lookup(&ok),
            Ok(()),
            "KEEP_POLICY must still ignore the policy field itself"
        );
    }

    /// ITEM 3 COHERENCE: the substituted policy is the one the guest can
    /// actually observe, so the two sites cannot drift apart silently.
    #[test]
    fn the_virtual_current_policy_is_what_sched_getattr_reports() {
        assert_eq!(
            VIRTUAL_CURRENT_POLICY,
            libc::SCHED_OTHER as u32,
            "handle_sched_getattr writes SCHED_OTHER into sched_policy for every thread; \
             KEEP_POLICY must substitute that same value"
        );
    }
}
