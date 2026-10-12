/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls dealing with signals.

use std::time::Duration;

use detcore_model::schedule::SigWrapper;
use nix::sys::signal::Signal;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sysno;
use reverie::syscalls::Timespec;
use tracing::info;

use crate::Detcore;
use crate::fd::FdType;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::InGuestSleepSigalrm;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::resources::SigalrmSleepDisposition;
use crate::scheduler::HostTimedSignalScope;
use crate::scheduler::OwnProcessSignalInputs;
use crate::scheduler::SiblingSignalState;
use crate::syscalls::helpers::REAL_TIME_SIGNALS;
use crate::syscalls::helpers::act_on_pending_real_time;
use crate::syscalls::helpers::observe_under_mask;
use crate::syscalls::helpers::pending_real_time_among;
use crate::syscalls::helpers::retry_nonblocking_syscall_with_timeout;
use crate::syscalls::helpers::unblocked_by;
use crate::syscalls::threads::KERNEL_SIGSET_SIZE;
use crate::syscalls::threads::KernelSigaction;
use crate::syscalls::threads::KernelSigset;
use crate::syscalls::threads::kernel_sigset_bit;
use crate::syscalls::threads::read_thread_wait_signal_state;
use crate::tool_global::ResumeStatus;
use crate::tool_global::SigalrmControl;
use crate::tool_global::alarm_remaining;
use crate::tool_global::commit_sigkill;
use crate::tool_global::host_timed_signals;
use crate::tool_global::notify_signal_pending;
use crate::tool_global::own_process_signal_inputs;
use crate::tool_global::record_host_timed_signals;
use crate::tool_global::refuse_sigalrm;
use crate::tool_global::register_alarm;
use crate::tool_global::registered_process_of;
use crate::tool_global::reserve_sigkill;
use crate::tool_global::reserve_sigkill_group;
use crate::tool_global::resolve_kill_targets_and_exit;
use crate::tool_global::resource_request;
use crate::tool_global::set_sigchld_disposition;
use crate::tool_global::set_sigtimedwait_set;
use crate::tool_global::sigalrm_refuses;
use crate::tool_global::signal_target_process;
use crate::tool_global::thread_observe_time;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;

/// Signals hermit takes from the guest's namespace, and what a guest loses.
///
/// ⚠️ ENUMERATED BY MEASUREMENT, NOT BY READING (2026-08-25). Every signal 1..64
/// was run under two delivery paths -- self-directed `raise` and a sibling
/// thread's `pthread_kill` -- with a native run as the control for each. Exactly
/// TWO differ from native, and they fail in different ways at different points:
///
///   SIGSTKFLT (16)  `rt_sigaction` is NO-OPED below, so the handler is never
///                   installed and the default disposition terminates the guest:
///                   observed exit 144 (= 128 + 16).
///   SIGTRAP    (5)  `rt_sigaction` passes through and the handler IS installed,
///                   but ptrace consumes every SIGTRAP (syscall stops, seccomp
///                   stops, breakpoints), so the handler never runs. ⚠️ THE GUEST
///                   THEN EXITS 0 WITH NO DIAGNOSTIC AT ALL -- a clean pass that
///                   behaved differently from native, which no cell can catch.
///
/// Everything else in 1..31 matches native exactly; 9/19 and 32/33 refuse
/// `sigaction` natively too and are not hermit's. Realtime 34..64 are
/// delivered and handled like any other signal under ptrace
/// (<https://github.com/rrnewton/reverie/issues/207>); they are not
/// appropriation.
const APPROPRIATED_SIGNALS: [(i32, &str); 2] = [
    (
        libc::SIGTRAP,
        "ptrace consumes every SIGTRAP (syscall/seccomp stops, breakpoints)",
    ),
    (
        libc::SIGSTKFLT,
        "reverie uses it as PERF_EVENT_SIGNAL, the PMU preemption timer",
    ),
];

/// Say, once per installation, that a guest handler will never run.
///
/// ⚠️ WHY A DIAGNOSTIC AND NOT A REFUSAL. Returning `EINVAL` from `sigaction`
/// was considered and deliberately rejected for SIGSTKFLT -- see the comment at
/// the no-op below: the Go runtime registers that handler, and refusing would
/// break every Go guest at startup. That reasoning generalises: installing a
/// handler defensively is common, actually raising these signals is rare, so
/// refusal breaks MORE programs than the current behaviour. The contract
/// question of what a guest is owed here is genuinely open.
///
/// What is NOT open is that hermit currently says NOTHING. This line commits to
/// no policy, breaks no conforming program, and turns a silent wrong answer into
/// a visible one -- the same reasoning as the `HERMIT_INTERNAL_FAILURE` marker.
/// The decision, separated from the reporting so a test can exercise THIS and
/// not a copy of it. A unit test that re-implements a predicate keeps passing
/// when the real one is gutted -- measured in this project's own pipe work,
/// where deleting the production wiring left every unit test green.
fn appropriated_reason(signum: i32, handler: u64) -> Option<&'static str> {
    // SIG_DFL (0) and SIG_IGN (1) lose nothing: the guest is not asking to be
    // called back, so there is no expectation to disappoint.
    if handler <= 1 {
        return None;
    }
    APPROPRIATED_SIGNALS
        .iter()
        .find(|(s, _)| *s == signum)
        .map(|(_, why)| *why)
}

fn warn_appropriated_signal(signum: i32, handler: u64) {
    if let Some(why) = appropriated_reason(signum, handler) {
        tracing::warn!(
            "HERMIT_APPROPRIATED_SIGNAL signum={signum} effect=handler-installed-but-never-invoked reason={why}"
        );
    }
}

// NB: the kernel uses an eight-byte signal mask in its raw signal syscalls on
// x86_64. `libc::sigset_t` is the 128-byte userspace wrapper type and must not
// be used to access these buffers. See:
// https://elixir.bootlin.com/linux/latest/source/include/uapi/asm-generic/signal.h#L75
fn validate_kernel_sigset_size(sigsetsize: usize) -> Result<(), Errno> {
    if sigsetsize == KERNEL_SIGSET_SIZE {
        Ok(())
    } else {
        Err(Errno::EINVAL)
    }
}

/// The mask Linux installs when a call names `mask` as the signal mask to sleep
/// under (`rt_sigsuspend`, `ppoll`, `pselect6`): `set_current_blocked` drops
/// `SIGKILL` and `SIGSTOP`, which can never be blocked.
pub(crate) fn kernel_installed_signal_mask(mask: KernelSigset) -> KernelSigset {
    let unblockable = (1_u64 << (libc::SIGKILL - 1)) | (1_u64 << (libc::SIGSTOP - 1));
    mask & !unblockable
}

fn without_perf_event_signal(mask: KernelSigset) -> KernelSigset {
    let bit = (reverie::PERF_EVENT_SIGNAL as u32) - 1;
    mask & !(1_u64 << bit)
}

/// Read one raw kernel signal mask while preserving the kernel's user-access check.
///
/// `safeptrace::Stopped::read` deliberately uses `PTRACE_PEEKDATA` for reads of
/// eight bytes or less. That operation can read a `PROT_NONE` page, unlike the
/// kernel's `copy_from_user`, so using `MemoryAccess::read_value` alone would
/// turn an `EFAULT` from a raw signal syscall into success. An invalid `how`
/// value makes `rt_sigprocmask` copy exactly the kernel-sized input and then
/// return `EINVAL` without changing the mask. Use that as a permission probe,
/// then read the already-validated word while the guest is stopped.
/// The x86-64 System V red zone below the stack pointer, which a leaf function
/// may use and an injected call therefore must not.
pub(super) const STACK_RED_ZONE: usize = 128;

/// The two eight-byte scratch cells `handle_rt_sigsuspend` places below the
/// red zone: the rt_sigpending result and the copy of the call's mask.
const SCRATCH_CELLS_SIZE: usize = 2 * std::mem::size_of::<u64>();

pub(super) async fn read_kernel_sigset<G, T>(
    guest: &mut G,
    address: Addr<'_, libc::sigset_t>,
) -> Result<KernelSigset, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let validation = syscalls::RtSigprocmask::new()
        .with_how(-1)
        .with_set(Some(address))
        .with_oldset(None)
        .with_sigsetsize(KERNEL_SIGSET_SIZE);
    match guest.inject(validation).await {
        Err(Errno::EINVAL) => {}
        Err(errno) => return Err(errno.into()),
        Ok(_) => {
            // Both Linux and the KVM syscall implementation reject an unknown
            // operation. Success means the backend did not validate the probe.
            return Err(Errno::EIO.into());
        }
    }
    Ok(guest.memory().read_value(address.cast())?)
}

/// Signal phase 1: the guest's SIGALRM disposition (a guest handler or not)
/// and SIGALRM blocked bit, as its runtime keeps them virtual, read back with
/// a query `rt_sigaction` and `rt_sigprocmask` that the runtime answers from
/// its virtual state.
async fn virtual_sigalrm_state<G, T>(guest: &mut G) -> Result<(bool, bool), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let state = read_sigalrm_sleep_state(guest).await?;
    Ok((
        state.disposition == SigalrmSleepDisposition::Handled,
        state.blocked,
    ))
}

/// Query through the runtime, which reports the guest's action and mask even
/// when it owns a physical SIGALRM handler. These queries run in an owned turn;
/// a scheduler-side read of the callback's physical mask is not a substitute.
async fn read_sigalrm_sleep_state<G, T>(guest: &mut G) -> Result<InGuestSleepSigalrm, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut stack = guest.stack().await;
    let action = stack.reserve::<KernelSigaction>();
    let mask = stack.reserve::<KernelSigset>();
    let _stack_guard = stack.commit()?;
    guest
        .inject(
            syscalls::RtSigaction::new()
                .with_signum(libc::SIGALRM)
                .with_action(None)
                .with_old_action(Some(action.cast()))
                .with_sigsetsize(KERNEL_SIGSET_SIZE),
        )
        .await?;
    guest
        .inject(
            syscalls::RtSigprocmask::new()
                .with_how(libc::SIG_BLOCK)
                .with_set(None)
                .with_oldset(Some(mask.cast()))
                .with_sigsetsize(KERNEL_SIGSET_SIZE),
        )
        .await?;
    let action: KernelSigaction = guest.memory().read_value(action)?;
    let mask: KernelSigset = guest.memory().read_value(mask)?;
    let disposition = if action.handler == libc::SIG_DFL as u64 {
        SigalrmSleepDisposition::Default
    } else if action.handler == libc::SIG_IGN as u64 {
        SigalrmSleepDisposition::Ignored
    } else {
        SigalrmSleepDisposition::Handled
    };
    Ok(InGuestSleepSigalrm {
        disposition,
        blocked: mask & kernel_sigset_bit(libc::SIGALRM) != 0,
    })
}

/// The guest's virtual SIGALRM blocked bit, read from the runtime (which
/// answers the query from its virtual state).
pub(crate) async fn virtual_sigalrm_blocked<G, T>(guest: &mut G) -> Result<bool, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut stack = guest.stack().await;
    let mask = stack.reserve::<KernelSigset>();
    let _stack_guard = stack.commit()?;
    guest
        .inject(
            syscalls::RtSigprocmask::new()
                .with_how(libc::SIG_BLOCK)
                .with_set(None)
                .with_oldset(Some(mask.cast()))
                .with_sigsetsize(KERNEL_SIGSET_SIZE),
        )
        .await?;
    let mask: KernelSigset = guest.memory().read_value(mask)?;
    Ok(mask & kernel_sigset_bit(libc::SIGALRM) != 0)
}

/// Validate the entire action through the kernel before copying it privately.
/// A partial `read_value` can fall back to ptrace for its final eight bytes,
/// which would read through a protected page. SIGKILL accepts no new action,
/// but both Linux and KVM copy the complete input before rejecting it.
async fn read_kernel_sigaction<G, T>(
    guest: &mut G,
    address: Addr<'_, libc::sigaction>,
) -> Result<KernelSigaction, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let validation = syscalls::RtSigaction::new()
        .with_signum(libc::SIGKILL)
        .with_action(Some(address))
        .with_old_action(None)
        .with_sigsetsize(KERNEL_SIGSET_SIZE);
    match guest.inject(validation).await {
        Err(Errno::EINVAL) => {}
        Err(errno) => return Err(errno.into()),
        Ok(_) => return Err(Errno::EIO.into()),
    }
    Ok(guest.memory().read_value(address.cast())?)
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#663)
fn timeval_to_logical_time(value: libc::timeval) -> Result<LogicalTime, Errno> {
    let seconds = u64::try_from(value.tv_sec).map_err(|_| Errno::EINVAL)?;
    let micros = u64::try_from(value.tv_usec).map_err(|_| Errno::EINVAL)?;
    if micros >= 1_000_000 {
        return Err(Errno::EINVAL);
    }
    let nanos = seconds
        .checked_mul(1_000_000_000)
        .and_then(|nanos| nanos.checked_add(micros * 1_000))
        .ok_or(Errno::EINVAL)?;
    Ok(LogicalTime::from_nanos(nanos))
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#663)
fn logical_time_to_timeval(value: LogicalTime) -> libc::timeval {
    libc::timeval {
        tv_sec: value.as_secs() as libc::time_t,
        tv_usec: value.subsec_micros() as libc::suseconds_t,
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#663)
fn logical_time_to_alarm_seconds(value: LogicalTime) -> i64 {
    value.as_nanos().div_ceil(1_000_000_000) as i64
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#663)
/// A pid no task can have: above the largest `pid_max` Linux allows (2^22).
/// `rt_sigqueueinfo` to it makes the kernel check the siginfo exactly as for
/// any pid and then answer ESRCH, which is Linux's answer for a gone process
/// (<https://github.com/rrnewton/hermit/issues/4124>).
pub(crate) const NO_SUCH_PID: libc::pid_t = libc::pid_t::MAX;

/// The 128 siginfo bytes at `info`. A page the kernel may write a siginfo to
/// can lack read permission (`PROT_WRITE` alone), which `process_vm_readv`
/// refuses; the tracer then reads it through `/proc/<tid>/mem`, as ptrace
/// access does, which needs no read permission on the mapping.
fn read_siginfo<T, G: Guest<Detcore<T>>>(
    guest: &mut G,
    info: AddrMut<libc::siginfo_t>,
) -> Option<[u8; 128]>
where
    T: RecordOrReplay,
{
    if let Ok(bytes) = guest.memory().read_value(info.cast::<[u8; 128]>()) {
        return Some(bytes);
    }
    use std::os::unix::fs::FileExt;
    let mem = std::fs::File::open(format!("/proc/{}/mem", guest.tid().as_raw())).ok()?;
    let mut bytes = [0u8; 128];
    mem.read_exact_at(&mut bytes, info.as_raw() as u64).ok()?;
    Some(bytes)
}

/// The thread a process-directed signal goes to. `zombie` is the process's
/// exit state (`Scheduler::exited_process_is_zombie`). `Ok(None)` is a zombie:
/// no live threads, but its parent can still wait for it, so Linux still finds
/// the process and the send succeeds (https://github.com/rrnewton/hermit/issues/4091).
fn deterministic_kill_target(
    targets: &[DetTid],
    zombie: Option<bool>,
    sig: libc::c_int,
) -> Result<Option<DetTid>, Errno> {
    match targets {
        [] if zombie == Some(true) => Ok(None),
        [] => Err(Errno::ESRCH),
        [target] => Ok(Some(*target)),
        [target, ..] if sig == 0 => Ok(Some(*target)),
        _ => Err(Errno::ENOSYS),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1119): Review unmaskable process-group SIGKILL forwarding.
fn can_forward_process_group_signal(
    pid: libc::pid_t,
    sig: libc::c_int,
    backend_requires_pid_translation: bool,
) -> bool {
    pid < -1 && sig == libc::SIGKILL && !backend_requires_pid_translation
}

/// Whether one of Linux's three ordinary signal syscalls names the calling
/// task exactly and asks for the one signal that cannot return successfully.
///
/// Keep process-group and broadcast spellings out of this predicate. Even when
/// the caller belongs to the named group, KVM can refuse a group containing a
/// second process; reserving an exit before that refusal would strand the
/// scheduler's terminal barrier.
fn self_sigkill_targets_current_task(
    signal: libc::c_int,
    target_process: Option<DetPid>,
    target_thread: Option<DetTid>,
    current_process: DetPid,
    current_thread: DetTid,
) -> bool {
    signal == libc::SIGKILL
        && (target_process.is_some() || target_thread.is_some())
        && target_process.is_none_or(|target| target == current_process)
        && target_thread.is_none_or(|target| target == current_thread)
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Bind a supported in-guest emulated sleep to its guest disposition/mask.
    /// Ordinary admitted creation does not share signal dispositions between
    /// processes, and each supported in-guest process has one guest thread.
    /// The snapshot therefore stays current until this request resumes.
    pub(crate) async fn in_guest_sleep_sigalrm_snapshot<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Option<InGuestSleepSigalrm> {
        if !self.cfg.backend.virtualizes_guest_sigalrm {
            return None;
        }
        match read_sigalrm_sleep_state(guest).await {
            Ok(state) => Some(state),
            Err(error) => {
                self.stop_after_in_guest_sleep_signal_loss(
                    guest,
                    format!("could not query its SIGALRM action/mask before sleeping: {error}"),
                )
                .await
            }
        }
    }

    /// A typed wake means the scheduler generated a one-shot SIGALRM for this
    /// exact sleep without sending it physically. Stage it through the frozen
    /// runtime self-signal path, which runs signal and exit callbacks before
    /// Linux kills the process. The caller must immediately return its handler:
    /// it must neither resume guest code nor copy nanosleep's remainder out.
    pub(crate) async fn stage_in_guest_fatal_alarm<G: Guest<Self>>(&self, guest: &mut G) {
        if !self.cfg.backend.virtualizes_guest_sigalrm {
            self.stop_after_in_guest_sleep_signal_loss(
                guest,
                "received an in-guest fatal alarm on another backend".into(),
            )
            .await;
        }
        match read_sigalrm_sleep_state(guest).await {
            Ok(InGuestSleepSigalrm {
                disposition: SigalrmSleepDisposition::Default,
                blocked: false,
            }) => {}
            other => {
                self.stop_after_in_guest_sleep_signal_loss(
                    guest,
                    format!("its default/unblocked SIGALRM sleep snapshot changed: {other:?}"),
                )
                .await;
            }
        }
        let call = syscalls::Tgkill::new()
            .with_tgid(guest.pid().as_raw())
            .with_tid(guest.tid().as_raw())
            .with_sig(libc::SIGALRM);
        if guest.inject(call).await != Ok(0) {
            self.stop_after_in_guest_sleep_signal_loss(
                guest,
                "could not stage its default-fatal SIGALRM after a sleep wake".into(),
            )
            .await;
        }
    }

    async fn stop_after_in_guest_sleep_signal_loss<G: Guest<Self>>(
        &self,
        guest: &mut G,
        reason: String,
    ) -> ! {
        let reason = format!(
            "thread {} cannot complete its in-guest SIGALRM sleep delivery: {reason}",
            guest.thread_state().dettid,
        );
        crate::tool_global::report_determinism_loss(guest, reason).await;
        match guest
            .tail_inject(
                syscalls::ExitGroup::new().with_status(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            )
            .await {}
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// We send the alarms to the global scheduler to handle.
    pub async fn handle_alarm<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Alarm,
    ) -> Result<i64, Error> {
        if guest.config().sequentialize_threads {
            let remaining = register_alarm(
                guest,
                LogicalTime::from_secs(call.seconds() as u64),
                LogicalTime::ZERO,
                Signal::SIGALRM,
            )
            .await;
            Ok(logical_time_to_alarm_seconds(remaining.0))
        } else {
            info!(
                "[dtid {}] Running without scheduler, so letting alarm call through...",
                guest.thread_state().dettid
            );
            Ok(guest.inject(call).await?)
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(#869)
    /// Schedule a one-shot or periodic real-time interval timer on Detcore logical time.
    pub async fn handle_setitimer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setitimer,
    ) -> Result<i64, Error> {
        if !guest.config().sequentialize_threads {
            info!(
                "[dtid {}] Running without scheduler, so letting setitimer call through...",
                guest.thread_state().dettid
            );
            return Ok(guest.inject(call).await?);
        }
        if call.which() != libc::ITIMER_REAL {
            return Err(Error::Errno(Errno::ENOSYS));
        }

        let value = call.value().ok_or(Errno::EFAULT)?;
        let timer: libc::itimerval = guest.memory().read_value(value)?;
        let interval = timeval_to_logical_time(timer.it_interval)?;
        let duration = timeval_to_logical_time(timer.it_value)?;
        // A zero value disarms the timer, whatever its interval.
        if duration.as_nanos() != 0 && interval.as_nanos() != 0 {
            refuse_sigalrm(guest, SigalrmControl::ArmRecurringTimer).await?;
        }
        let (remaining, old_interval) =
            register_alarm(guest, duration, interval, Signal::SIGALRM).await;
        if let Some(old_value) = call.ovalue() {
            let old_timer = libc::itimerval {
                it_interval: logical_time_to_timeval(old_interval),
                it_value: logical_time_to_timeval(remaining),
            };
            guest.memory().write_value(old_value, &old_timer)?;
        }
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-892)
    /// Return interval-timer state from Detcore's logical scheduler.
    pub async fn handle_getitimer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getitimer,
    ) -> Result<i64, Error> {
        if !guest.config().sequentialize_threads {
            info!(
                "[dtid {}] Running without scheduler, so letting getitimer call through...",
                guest.thread_state().dettid
            );
            return Ok(guest.inject(call).await?);
        }

        let snapshot = match call.which() {
            libc::ITIMER_REAL => alarm_remaining(guest).await,
            libc::ITIMER_VIRTUAL | libc::ITIMER_PROF => {
                crate::scheduler::real_timer::ItimerSnapshot::default()
            }
            _ => return Err(Errno::EINVAL.into()),
        };
        let value = call.value().ok_or(Errno::EFAULT)?;
        let timer = libc::itimerval {
            it_interval: logical_time_to_timeval(snapshot.interval),
            it_value: logical_time_to_timeval(snapshot.remaining),
        };
        guest.memory().write_value(value, &timer)?;
        Ok(0)
    }

    /// A pause is really just an unbounded sleep.
    pub async fn handle_pause<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pause,
    ) -> Result<i64, Error> {
        if guest.config().sequentialize_threads {
            // `pause` has no deadline: it returns only when a signal is delivered.
            // `LogicalTime::INDEFINITE` records that, and the scheduler refuses to
            // fast-forward virtual time onto it (see `step2d_handle_empty_queue`),
            // so the `Normal` arm below stays unreachable.
            loop {
                // A signal that already ended the call before it parked: one sent
                // while this thread was stopped short of filing the request, as
                // when its timeslice ended in the syscall's prehook.
                if sleep_signal(guest, SleepCheck::BeforePark, true).await? == SleepSignal::Pending
                {
                    return Err(Errno::ERESTARTNOHAND.into());
                }
                let mut req = Self::sleep_request_abs(guest, LogicalTime::INDEFINITE).await;
                req.fyi(crate::resources::PAUSE_FYI);
                req.in_guest_sleep_sigalrm = self.in_guest_sleep_sigalrm_snapshot(guest).await;
                match crate::tool_global::parked_wait_request(
                    guest,
                    req,
                    crate::scheduler::parked::ParkedWaitPolicy::PauseNoHandlerRestart,
                )
                .await
                {
                    ResumeStatus::InGuestFatalAlarm => {
                        self.stage_in_guest_fatal_alarm(guest).await;
                        return Err(Errno::EINTR.into());
                    }
                    ResumeStatus::Normal => {
                        panic!(
                            "Internal violation: pause should never return from the scheduler except by interruption!"
                        )
                    }
                    ResumeStatus::Signaled(signals) => {
                        match sleep_signal(guest, SleepCheck::AfterWake(signals), true).await? {
                            SleepSignal::Pending => return Err(Errno::ERESTARTNOHAND.into()),
                            SleepSignal::Held => return Err(Errno::EINTR.into()),
                            SleepSignal::None => {
                                info!(
                                    "[dtid {}] pause woken for a signal no longer pending for it; waiting again",
                                    guest.thread_state().dettid
                                );
                            }
                        }
                    }
                }
            }
        } else {
            info!(
                "[dtid {}] Running without scheduler, so letting pause call through...",
                guest.thread_state().dettid
            );
            Ok(guest.inject(call).await?)
        }
    }

    /// Run rt_sigsuspend without holding the deterministic scheduler turn.
    ///
    /// The kernel must perform the temporary mask swap atomically and restore the
    /// original mask after signal delivery, so execute the real blocking syscall
    /// while marking this thread as blocked outside the runnable set.
    pub async fn handle_rt_sigsuspend<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigsuspend,
    ) -> Result<i64, Error> {
        // Invalid arguments return immediately from the kernel and therefore are
        // not signal-only waits.
        validate_kernel_sigset_size(call.sigsetsize())?;
        let Some(mask_addr) = call.mask() else {
            return Err(Errno::EFAULT.into());
        };

        let temporary_mask = read_kernel_sigset(guest, mask_addr).await?;
        // The temporary mask lets the kernel take a pending real-time timer
        // expiration that every thread blocked when it expired
        // (https://github.com/rrnewton/hermit/issues/4089).
        observe_under_mask(guest, temporary_mask).await?;
        // The real call sleeps under a private copy of the mask read here, not
        // under the guest's buffer. The scheduler decides from this copy which
        // signals can end the call (`Resources::blocked_signal_mask`), and
        // another process sharing the buffer (MAP_SHARED) may rewrite it before
        // the call runs; Linux copies the mask when the call starts, and from
        // the guest's view the call has already started.
        //
        // The copy and the rt_sigpending result live in two cells just below
        // the stack's red zone, the scratch area an injected call may use, but
        // never on the caller's buffer: a guest may keep its mask there while
        // it blocks every signal, and a copy placed on that very buffer would
        // be the original again, while the pending cell would overwrite the
        // guest's mask. If the buffer overlaps the two cells, they move to just
        // below it. They are written directly rather than through a scratch
        // stack, whose commit writes one contiguous region down from the red
        // zone and so would cover a buffer inside it.
        let caller_mask = mask_addr.as_raw()..mask_addr.as_raw() + KERNEL_SIGSET_SIZE;
        let below_red_zone = (guest.regs().await.rsp as usize).wrapping_sub(STACK_RED_ZONE);
        let cells = (below_red_zone - SCRATCH_CELLS_SIZE)..below_red_zone;
        let cells_start = if cells.start < caller_mask.end && caller_mask.start < cells.end {
            (caller_mask.start - SCRATCH_CELLS_SIZE) & !(std::mem::align_of::<u64>() - 1)
        } else {
            cells.start
        };
        let pending_addr = Addr::<u64>::from_raw(cells_start).ok_or(Errno::EFAULT)?;
        let mask_copy = AddrMut::<u64>::from_raw(cells_start + std::mem::size_of::<u64>())
            .ok_or(Errno::EFAULT)?;
        guest.memory().write_value(mask_copy, &temporary_mask)?;
        let pending_out = AddrMut::<libc::sigset_t>::from_raw(pending_addr.as_raw())
            .expect("stack address must be non-null");
        let pending_call = syscalls::RtSigpending::new()
            .with_set(Some(pending_out))
            .with_sigsetsize(KERNEL_SIGSET_SIZE);
        guest.inject_with_retry(pending_call).await?;
        let pending: u64 = guest.memory().read_value(pending_addr)?;
        let call = call.with_mask(Some(Addr::from_raw(mask_copy.as_raw()).expect("non-null")));

        // The scheduler records the mask the call sleeps under, read here in
        // this thread's turn, to choose a SIGCHLD target while the thread is
        // outside the runnable set (`Resources::blocked_signal_mask`).
        let installed_mask = kernel_installed_signal_mask(temporary_mask);
        if pending & !temporary_mask != 0 {
            // The kernel will consume an already-pending signal as soon as it
            // atomically installs the temporary mask. Keep this immediate case
            // out of the terminal-wait classification; the real syscall still
            // performs delivery and restores the old mask.
            self.record_or_replay_blocking_with_mask(guest, call.into(), Some(installed_mask))
                .await
        } else {
            self.record_or_replay_rt_sigsuspend(guest, call, installed_mask)
                .await
        }
    }

    /// rt_sigaction
    pub async fn handle_rt_sigaction<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigaction,
    ) -> Result<i64, Error> {
        // Linux rejects an invalid size before inspecting either user pointer.
        // Preserve that EINVAL-before-EFAULT ordering even for the signal that
        // Hermit reserves for deterministic preemption.
        validate_kernel_sigset_size(call.sigsetsize())?;

        // Copy the complete kernel object before inspecting the signal number or
        // writing old_action. This preserves Linux's action-before-old_action
        // pointer ordering, including when both arguments alias.
        let kernel_action = match call.action() {
            Some(action) => Some(read_kernel_sigaction(guest, action).await?),
            None => None,
        };

        // Both appropriated signals are reported here, at the one point where the
        // guest states its expectation. SIGTRAP falls through to the ordinary
        // path below (its handler really is installed; ptrace just eats the
        // signal), so this must run before the SIGSTKFLT early return.
        if let Some(action) = kernel_action {
            warn_appropriated_signal(call.signum(), action.handler);
        }

        // PERF_EVENT_SIGNAL is reserved.
        if call.signum() == reverie::PERF_EVENT_SIGNAL as i32 {
            // The go runtime attempts to register this (unused) signal handler.  We will never
            // deliver signals of this kind to the guest, so we just turn this action into a noop
            // rather than returning `Err(Errno::EINVAL.into())`. Preserve that established
            // policy while still honoring the raw syscall's pointer accesses: the virtual old
            // disposition is the default action, never Reverie's private preemption handler.
            if call.old_action().is_some() {
                // SIGKILL's disposition cannot be changed by userspace, so its
                // query copies the same four zero words as our virtual default.
                // Let the kernel copy them: a short write_value can finish with
                // PTRACE_POKEDATA and overwrite a protected final word. This also
                // preserves the kernel's partial-copy effects on EFAULT without
                // exposing Reverie's private preemption action.
                return Ok(guest
                    .inject(call.with_signum(libc::SIGKILL).with_action(None))
                    .await?);
            }
            return Ok(0);
        }
        // Signal phase 1 (step I3): on a backend that keeps a handled SIGALRM
        // virtual, the scheduler's ledger must agree with the runtime.
        let phase1 = self.cfg.backend.virtualizes_guest_sigalrm
            && call.signum() == libc::SIGALRM
            && kernel_action.is_some();
        let was_handled = guest.thread_state().sigalrm_handled;
        let to_handler = kernel_action.is_some_and(|action| {
            action.handler != libc::SIG_DFL as u64 && action.handler != libc::SIG_IGN as u64
        });
        if phase1 {
            if to_handler {
                self.refuse_sigalrm_handler(guest).await?;
            } else if was_handled
                && kernel_action.is_some_and(|action| action.handler == libc::SIG_DFL as u64)
            {
                refuse_sigalrm(guest, SigalrmControl::HandlerToDefault).await?;
            }
        }
        let result = if let Some(kernel_action) = kernel_action {
            // The kernel treats `action` as input. Sanitize a private copy instead
            // of writing through the guest pointer: the input may be read-only,
            // and changing it would make the syscall wrapper observable.
            let mut kernel_action = kernel_action;
            kernel_action.mask = without_perf_event_signal(kernel_action.mask);
            let mut stack = guest.stack().await;
            let sanitized_action = stack.push(kernel_action);
            let _stack_guard = stack.commit()?;
            guest
                .inject(call.with_action(Some(sanitized_action.cast())))
                .await
        } else {
            guest.inject(call).await
        };
        // Publish the state the runtime now holds whenever the disposition may
        // have changed: from a handler, or to one the runtime did not refuse.
        // Any result counts, since an EFAULT copying the old action out comes
        // after the change. A default or ignored action set while none is
        // handled changes nothing the scheduler records, so it costs nothing.
        if phase1 && (was_handled || (to_handler && result != Err(Errno::EPERM))) {
            self.publish_virtual_sigalrm(guest).await?;
        }
        // Linux reaps a process's children automatically at exit when its
        // SIGCHLD action is SIG_IGN or carries SA_NOCLDWAIT, and a signal to
        // such a child then finds no zombie. Mirror the new action for the
        // scheduler's exit model, with the same success rule as above
        // (https://github.com/rrnewton/hermit/issues/4091).
        if call.signum() == libc::SIGCHLD
            && guest.config().sequentialize_threads
            && let Some(action) = kernel_action
            && matches!(result, Ok(_) | Err(Errno::EFAULT))
        {
            let dettid = guest.thread_state().dettid;
            if let Some(process) = registered_process_of(guest, dettid).await {
                set_sigchld_disposition(
                    guest,
                    process,
                    action.handler == libc::SIG_IGN as u64,
                    action.flags & libc::SA_NOCLDWAIT as u64 != 0,
                )
                .await;
            }
        }
        // Linux flushes the pending instances of a signal whose disposition
        // becomes SIG_IGN (`do_sigaction`), among them a pending real-time timer
        // expiration (https://github.com/rrnewton/hermit/issues/4089).
        if result.is_ok()
            && kernel_action.is_some_and(|action| action.handler == libc::SIG_IGN as u64)
            && (1..=64).contains(&call.signum())
        {
            let bit = kernel_sigset_bit(call.signum());
            let pending = pending_real_time_among(guest, bit).await;
            act_on_pending_real_time(guest, pending, 0, bit).await;
        }
        // The same flush takes a `SIGEV_THREAD_ID` timer's pending copy of a
        // standard signal, for SIG_IGN or for SIG_DFL on a signal whose
        // default is to ignore it (`sig_handler_ignored`), so it no longer
        // counts as the timer's own (https://github.com/rrnewton/hermit/issues/4164).
        let ignored = |handler: u64| {
            handler == libc::SIG_IGN as u64
                || (handler == libc::SIG_DFL as u64
                    && [libc::SIGCONT, libc::SIGCHLD, libc::SIGWINCH, libc::SIGURG]
                        .contains(&call.signum()))
        };
        // The kernel reads the new action from Detcore's sanitized copy, so an
        // EFAULT can only come from writing `old_action`, after the new action
        // is installed.
        if (result.is_ok() || matches!(result, Err(Errno::EFAULT)))
            && kernel_action.is_some_and(|action| ignored(action.handler))
            && (1..32).contains(&call.signum())
            && crate::scheduler::thread_timer_copies_recorded()
        {
            crate::tool_global::forget_thread_timer_copies(guest, call.signum()).await;
        }
        Ok(result?)
    }

    /// Signal phase 1's Detcore-side refusals of a new guest SIGALRM handler,
    /// each EPERM before anything changes (design section 6 and closure 5):
    /// threads not sequentialized (`alarm` would arm a kernel timer), a
    /// signalfd in the process's descriptor table, and a kernel producer of
    /// SIGALRM armed anywhere in the run.
    async fn refuse_sigalrm_handler<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Error> {
        if !guest.config().sequentialize_threads {
            return Err(Errno::EPERM.into());
        }
        // The model's own signalfds, and any the kernel's table holds that
        // the model labels otherwise (an inherited descriptor).
        let holds_signalfd = guest
            .thread_state()
            .file_metadata
            .lock()
            .expect("file metadata mutex poisoned")
            .file_handles
            .values()
            .any(|fd| fd.ty() == FdType::Signalfd)
            || crate::sigalrm_phase1::process_holds_signalfd(guest.pid().as_raw());
        if holds_signalfd {
            return Err(Errno::EPERM.into());
        }
        Ok(refuse_sigalrm(guest, SigalrmControl::InstallHandler).await?)
    }

    /// Read the guest's SIGALRM disposition and blocked bit back from the
    /// runtime (which answers these queries from its virtual state), then
    /// publish them to the scheduler and record the disposition in the thread
    /// state, inside this call's turn.
    async fn publish_virtual_sigalrm<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Error> {
        let (handled, blocked) = virtual_sigalrm_state(guest).await?;
        sigalrm_refuses(guest, SigalrmControl::Publish { handled, blocked }).await;
        guest.thread_state_mut().sigalrm_handled = handled;
        guest.thread_state_mut().sigalrm_blocked_published = blocked;
        Ok(())
    }

    /// After a guest call that may have changed its virtual mask in a process
    /// that handles SIGALRM, publish its SIGALRM blocked bit.
    async fn publish_virtual_sigalrm_blocked<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<(), Error> {
        let blocked = virtual_sigalrm_blocked(guest).await?;
        sigalrm_refuses(guest, SigalrmControl::PublishBlocked(blocked)).await;
        guest.thread_state_mut().sigalrm_blocked_published = blocked;
        Ok(())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#1046): Review retrying interrupted signal-mask injections.
    /// rt_sigprocmask
    pub async fn handle_rt_sigprocmask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigprocmask,
    ) -> Result<i64, Error> {
        // The kernel checks sigsetsize before copying from either user pointer.
        validate_kernel_sigset_size(call.sigsetsize())?;

        let result = self.rt_sigprocmask_inner(guest, call).await;
        if call.how() != libc::SIG_BLOCK && call.set().is_some() {
            // The new mask may unblock a pending real-time timer expiration
            // that every thread blocked when it expired, which the kernel then
            // delivers (https://github.com/rrnewton/hermit/issues/4089). Whatever
            // the call returned: an EFAULT copying the old set out comes after
            // the change. A mask that cannot be read could unblock any of them.
            let pending = pending_real_time_among(guest, REAL_TIME_SIGNALS).await;
            if pending != 0 {
                let (observable, discarded) =
                    match read_thread_wait_signal_state(guest.pid(), guest.tid()) {
                        Ok(state) => unblocked_by(&state, state.blocked),
                        Err(_) => (REAL_TIME_SIGNALS, 0),
                    };
                act_on_pending_real_time(guest, pending, observable, discarded).await;
            }
        }
        if self.cfg.backend.virtualizes_guest_sigalrm && guest.thread_state().sigalrm_handled {
            // Signal phase 1: the scheduler's blocked bit follows the virtual
            // mask, whatever the call returned (an EFAULT copying the old set
            // out comes after the change).
            self.publish_virtual_sigalrm_blocked(guest).await?;
        }
        result
    }

    async fn rt_sigprocmask_inner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigprocmask,
    ) -> Result<i64, Error> {
        if call.how() != libc::SIG_BLOCK && call.how() != libc::SIG_SETMASK {
            Ok(guest.inject_with_retry(call).await?)
        } else if let Some(set) = call.set() {
            let set_mask = read_kernel_sigset(guest, set).await?;
            let mut stack = guest.stack().await;
            let new_set = stack.push(without_perf_event_signal(set_mask));
            let _stack_guard = stack.commit()?;
            let modified_call = syscalls::RtSigprocmask::new()
                .with_how(call.how())
                .with_set(Some(new_set.cast()))
                .with_oldset(call.oldset())
                .with_sigsetsize(call.sigsetsize());
            // Keep returning to the handler so post_handler_hook can run, but
            // do not expose a tracer preemption as ERESTARTSYS to the guest.
            Ok(guest.inject_with_retry(modified_call).await?)
        } else {
            Ok(guest.inject_with_retry(call).await?)
        }
    }

    /// rt_sigtimedwait system call
    ///
    /// This is handled by the scheduler and not passed to the record/replay layer,
    /// because currently signals are not recorded.
    pub async fn handle_rt_sigtimedwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigtimedwait,
    ) -> Result<i64, Error> {
        // Linux rejects an invalid mask width before inspecting its pointers.
        validate_kernel_sigset_size(call.sigsetsize())?;
        // Linux keeps this entry snapshot for the whole wait. The retry helper
        // currently injects from the caller's pointer again, so another thread
        // can change the set between retries. Fixing that requires sharing one
        // scratch-stack guard with the helper's zero-timeout object; do not hide
        // that remaining difference by treating this validation read as a snapshot.

        let dettid = guest.thread_state().dettid;

        let maybe_timeout = if let Some(timeout) = call.timeout() {
            let ts: Timespec = guest.memory().read_value(timeout)?;
            let ns_delta =
                Duration::from_secs(ts.tv_sec as u64) + Duration::from_nanos(ts.tv_nsec as u64);
            let base_time = thread_observe_time(guest).await;
            let target_time = base_time + ns_delta;
            Some(target_time)
        } else {
            None
        };
        let mut rsrc = Resources::new(dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("rt_sigtimedwait");
        // While it polls, a signal in the set is taken at the next poll
        // whatever the thread's mask, which a process-directed signal another
        // thread sends must account for
        // (`Scheduler::own_process_signal_inputs`). An unreadable set fails in
        // the kernel at the first poll.
        let waited = match call.set() {
            Some(set) => read_kernel_sigset(guest, set.into()).await.ok(),
            None => None,
        };
        if waited.is_some() {
            set_sigtimedwait_set(guest, waited).await;
        }
        // Linux writes the siginfo only for a signal it returns. A dropped
        // copy's siginfo is replaced by what the buffer held before.
        let before = match call.info() {
            Some(info) if self.cfg.backend.reports_child_exit_publication => {
                read_siginfo(guest, info)
            }
            _ => None,
        };
        let result = loop {
            let result =
                retry_nonblocking_syscall_with_timeout(guest, call, rsrc.clone(), maybe_timeout)
                    .await;
            // A consumed child-exit copy that the notification ledger drops
            // is not returned: the wait goes on, to the same deadline.
            if self.synchronous_sigchld_stands(guest, call, &result).await {
                break result;
            }
            if let (Some(info), Some(bytes)) = (call.info(), before.as_ref()) {
                let _ = guest.memory().write_value(info.cast::<[u8; 128]>(), bytes);
            }
        };
        if waited.is_some() {
            set_sigtimedwait_set(guest, None).await;
        }
        result
    }

    /// The `signalfd_siginfo` records a successful signalfd `read` returned,
    /// `len` bytes at `buf`, reconciled with the child-exit ledger
    /// (`reconcile_signalfd_extents`). Returns the bytes kept.
    pub(crate) async fn reconcile_signalfd_records<G: Guest<Self>>(
        &self,
        guest: &mut G,
        buf: AddrMut<'_, u8>,
        len: usize,
    ) -> usize {
        self.reconcile_signalfd_extents(guest, &[(buf.as_raw(), len)], len)
            .await
    }

    /// The `signalfd_siginfo` records a successful signalfd read returned:
    /// the first `read` bytes of the guest buffers `extents` (address,
    /// length), in order, as `read`, `readv`, `preadv` and `preadv2` scatter
    /// them. They are reconciled with the child-exit ledger as a SIGCHLD
    /// delivered to a handler (`Detcore::child_exit_sigchld_delivers`) or
    /// consumed by `rt_sigtimedwait` (`synchronous_sigchld_stands`) is: the
    /// scheduler's copy gets the siginfo Linux gives the parent, the kernel's
    /// coalesced copy for a SIGKILL cohort names the cohort's lowest pid, and
    /// a copy for an exit a SIGCHLD already stood for is dropped. Other
    /// records are kept as read. The kept records are scattered back over the
    /// same extents, in order, with zeros over the bytes of dropped ones.
    /// Returns the bytes kept, a whole number of records.
    pub(crate) async fn reconcile_signalfd_extents<G: Guest<Self>>(
        &self,
        guest: &mut G,
        extents: &[(usize, usize)],
        read: usize,
    ) -> usize {
        let len = read - read % SIGNALFD_RECORD;
        let pieces = signalfd_extent_pieces(extents, len);
        let mut bytes = Vec::with_capacity(len);
        for (address, length) in &pieces {
            let mut piece = vec![0u8; *length];
            let Some(address_mut) = AddrMut::<u8>::from_raw(*address) else {
                return len;
            };
            if guest.memory().read_exact(address_mut, &mut piece).is_err() {
                use std::os::unix::fs::FileExt;
                let read = std::fs::File::open(format!("/proc/{}/mem", guest.tid().as_raw()))
                    .and_then(|mem| mem.read_exact_at(&mut piece, *address as u64));
                if read.is_err() {
                    // The kernel has just written these bytes, so the buffer
                    // is mapped and writable; `/proc/<tid>/mem` reads any
                    // such page.
                    return len;
                }
            }
            bytes.extend_from_slice(&piece);
        }
        let controls = signalfd_record_controls(&bytes, std::process::id());
        let mut answers = Vec::with_capacity(controls.len());
        for control in controls {
            answers.push(match control {
                Some(control) => Some(crate::tool_global::child_exit_sigchld(guest, control).await),
                None => None,
            });
        }
        let Some(rewritten) = apply_signalfd_answers(&bytes, &answers) else {
            return len;
        };
        let mut offset = 0;
        for (address, length) in &pieces {
            let written = AddrMut::<u8>::from_raw(*address).map(|address| {
                guest
                    .memory()
                    .write_exact(address, &rewritten.bytes[offset..offset + length])
            });
            if !matches!(written, Some(Ok(()))) {
                tracing::warn!(
                    "[dtid {}] the signalfd records at {:#x} keep the kernel's siginfo",
                    guest.thread_state().dettid,
                    address,
                );
                return len;
            }
            offset += length;
        }
        rewritten.kept
    }

    /// A child-exit SIGCHLD that a synchronous wait consumed goes through the
    /// same notification ledger as one delivered to a handler
    /// (`Detcore::child_exit_sigchld_delivers`), on a backend where both the
    /// kernel and the scheduler send it (`scheduler::child_exit_sigchld`).
    /// The scheduler's copy is returned with the siginfo Linux gives the
    /// parent, and a copy for an exit that a SIGCHLD already stood for is
    /// dropped. Returns whether the consumed signal stands.
    async fn synchronous_sigchld_stands<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigtimedwait,
        result: &Result<i64, Error>,
    ) -> bool {
        use crate::scheduler::child_exit_sigchld::ChildExitSigchldAnswer;
        use crate::scheduler::child_exit_sigchld::OwnCopy;
        if !self.cfg.backend.reports_child_exit_publication
            || !matches!(result, Ok(signal) if *signal == libc::SIGCHLD as i64)
        {
            return true;
        }
        // Without a siginfo buffer the guest sees only the signal number,
        // which every copy shares.
        let Some(info) = call.info() else {
            return true;
        };
        let Some(bytes) = read_siginfo(guest, info) else {
            return true;
        };
        let Some(control) =
            crate::scheduler::child_exit_sigchld::control_for(&bytes, std::process::id())
        else {
            return true;
        };
        match crate::tool_global::child_exit_sigchld(guest, control).await {
            ChildExitSigchldAnswer::Claimed(dropped) => !dropped,
            ChildExitSigchldAnswer::OwnCopy(OwnCopy::Duplicate) => false,
            ChildExitSigchldAnswer::OwnCopy(OwnCopy::Deliver(own))
            | ChildExitSigchldAnswer::ClaimedFor(own) => {
                if let Err(error) = guest
                    .memory()
                    .write_value(info.cast::<[u8; 128]>(), &own.to_bytes())
                {
                    tracing::warn!(
                        "[dtid {}] the SIGCHLD for child {} keeps its siginfo: {}",
                        guest.thread_state().dettid,
                        own.pid,
                        error
                    );
                }
                true
            }
            _ => true,
        }
    }

    /// Fence an exact self-SIGKILL at the scheduler-selected syscall turn.
    ///
    /// This applies only when Detcore holds the backend's process signal
    /// control (`BackendSignalControlMode::ToolControlled`). A backend that
    /// installed that control commits this syscall as an immediate,
    /// nonreturning process exit. It therefore has no later return boundary at
    /// which the ordinary pending signal path could acquire a delivery permit.
    /// Without this preflight, the backend's terminal child callback arrives
    /// with no generation-bound reservation and must fail closed.
    /// `ResourceID::Exit` is the same fence used by `exit_group`. A run without
    /// the installed control (`Unchanged`, including every non-sequentialized
    /// run) keeps its ordinary signal path and reserves nothing.
    pub(crate) async fn reserve_controlled_self_sigkill_exit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        signal: libc::c_int,
        target_process: Option<DetPid>,
        target_thread: Option<DetTid>,
    ) -> bool {
        if guest.signal_control_mode() != reverie::BackendSignalControlMode::ToolControlled {
            return false;
        }
        let (current_thread, mm) = {
            let state = guest.thread_state();
            (state.dettid, state.mm_id)
        };
        if !self_sigkill_targets_current_task(
            signal,
            target_process,
            target_thread,
            self.detpid,
            current_thread,
        ) {
            return false;
        }
        let request = guest.thread_state().mk_request(
            ResourceID::Exit {
                group: true,
                process: self.detpid,
                mm,
            },
            Permission::RW,
        );
        resource_request(guest, request).await;
        true
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(PR-1058): Review process-pending signal preservation.
    // TODO-HUMAN-REVIEW(PR-1119): Review unmaskable process-group SIGKILL forwarding.
    /// Resolve signal-zero existence checks in the fixed PID namespace, then route an
    /// unambiguous positive-PID process signal through the backend. Backends that can execute
    /// with guest PIDs preserve process-directed delivery; DBT translates it to the sole live
    /// thread because its native process uses a host PID. An unmaskable SIGKILL to a specific
    /// process group is also safe to preserve on backends whose guests use real namespace PIDs;
    /// other process-group and broadcast delivery remains refused until Detcore models eligible
    /// signal masks.
    pub async fn handle_kill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Kill,
    ) -> Result<i64, Error> {
        self.record_orphaned_group_signals(guest, call.sig()).await;
        if !guest.config().sequentialize_threads {
            // Without sequentialization every other kill goes to the kernel as
            // is, but never one to a process group or to every process: the
            // guest's process group is inherited from Hermit's, so it can hold
            // Hermit and the user's shell or test runner, which a group kill
            // would reach (https://github.com/rrnewton/hermit/issues/4057).
            // Signal 0 sends nothing, and INT_MIN is ESRCH before any lookup.
            if call.sig() != 0 && call.pid() <= 0 && call.pid() != libc::pid_t::MIN {
                return self
                    .refuse_group_signal(guest, call.pid(), call.sig())
                    .await;
            }
            if self
                .drop_signal_to_hermit(guest, Sysno::kill, None, call.pid(), call.sig())
                .await
            {
                return Ok(0);
            }
            return Ok(self.record_or_replay(guest, call).await?);
        }

        // Linux's `kill_something_info` answers ESRCH for pid INT_MIN before
        // anything else ("-INT_MIN is undefined"), so the kernel's answer is
        // deterministic and sends nothing; negating it here would overflow
        // (https://github.com/rrnewton/hermit/issues/4046).
        if call.sig() == 0 && call.pid() > 0 {
            // An existence check answers from the same model as a signal, so
            // that one pid never answers 0 to one and ESRCH to the other: a
            // process with no live threads is a zombie (0) or gone (ESRCH)
            // whenever Detcore saw it exit, whatever the host has reaped by
            // now (https://github.com/rrnewton/hermit/issues/4091).
            let (targets, zombie) =
                resolve_kill_targets_and_exit(guest, DetPid::from_raw(call.pid())).await;
            if targets.is_empty() {
                match zombie {
                    Some(true) => return Ok(0),
                    Some(false) => return Err(Errno::ESRCH.into()),
                    None => {}
                }
            }
        }
        if call.sig() == 0 || call.pid() == libc::pid_t::MIN {
            return Ok(self.record_or_replay(guest, call).await?);
        }

        let tgid = call.pid();
        // A process-group or broadcast SIGALRM is refused by name below. The
        // scheduler resolves a thread id to its process itself.
        if call.sig() == libc::SIGALRM && tgid > 0 {
            refuse_sigalrm(guest, SigalrmControl::SendTo(DetTid::from_raw(tgid))).await?;
        }
        if self
            .drop_signal_to_hermit(guest, Sysno::kill, None, tgid, call.sig())
            .await
        {
            return Ok(0);
        }
        if can_forward_process_group_signal(
            tgid,
            call.sig(),
            guest
                .config()
                .backend
                .requires_thread_directed_process_signals,
        ) {
            // The scheduler is not told about each killed process, but it must
            // know the group was killed: a held process in it will die, so a
            // wait for it is not part of a deadlock
            // (https://github.com/rrnewton/hermit/issues/3904). Told before
            // the kill, inside this committed turn: the sender may be in the
            // group and die before the call returns. A kill that then fails
            // only suppresses a deadlock report, which is the safe direction.
            // `kill(INT_MIN)` names no group; Linux refuses it with ESRCH.
            let group = tgid.checked_neg().map(DetPid::from_raw);
            if group.is_some() {
                crate::tool_global::notify_unresolved_sigkill(guest, group).await;
            }
            // Every member is retired at this turn
            // (https://github.com/rrnewton/hermit/issues/3994).
            if guest.config().sequentialize_threads
                && let Some(group) = group
                && let Some(reservation) = reserve_sigkill_group(guest, group).await
            {
                return self.send_reserved_sigkill(guest, reservation, call).await;
            }
            return Ok(self.record_or_replay(guest, call).await?);
        }
        if tgid <= 0 {
            return self.refuse_group_signal(guest, tgid, call.sig()).await;
        }
        // A kill may name the process by any of its live threads' ids (Linux's
        // kill_pid_info looks the id up as a task); everything below that acts
        // on the process uses its leader
        // (https://github.com/rrnewton/hermit/issues/4094).
        let process = signal_target_process(guest, DetPid::from_raw(tgid))
            .await
            .map_or(tgid, |process| process.as_raw());
        // Exact self-SIGKILL is group-fatal, so it has no recipient-selection
        // ambiguity even when the process has several live threads. Reserve it
        // before the generic process-signal path rejects that thread set.
        if self
            .reserve_controlled_self_sigkill_exit(
                guest,
                call.sig(),
                Some(DetPid::from_raw(process)),
                None,
            )
            .await
        {
            return Ok(self.record_or_replay(guest, call).await?);
        }
        // A SIGKILL to another process retires every thread of it at this
        // turn (https://github.com/rrnewton/hermit/issues/3994). It needs no
        // recipient, so a multithreaded target is accepted.
        if call.sig() == libc::SIGKILL
            && let Some(reservation) = reserve_sigkill(guest, DetTid::from_raw(tgid)).await
        {
            return self.send_reserved_sigkill(guest, reservation, call).await;
        }
        match self
            .own_process_signal(guest, Sysno::kill, tgid, process, call.sig())
            .await?
        {
            OwnProcessSend::Admitted => {
                return self
                    .forward_own_process_signal(guest, call.sig(), call)
                    .await;
            }
            OwnProcessSend::Refused(answer) => return answer,
            OwnProcessSend::NotOwn => {}
        }
        let Some(tid) = self
            .process_signal_target(guest, Sysno::kill, DetPid::from_raw(process), call.sig())
            .await?
        else {
            // A zombie: exited, and its parent can still wait for it. Linux's
            // kill_pid_info still finds it and the send succeeds, discarding
            // the signal, so answer 0 without a system call: the host process
            // may already be gone if a backend reaped it, and the logical
            // answer must not depend on when
            // (https://github.com/rrnewton/hermit/issues/4091). Linux checks
            // the signal number after finding the task, so an invalid one is
            // EINVAL.
            if !(1..=libc::SIGRTMAX()).contains(&call.sig()) {
                return Err(Errno::EINVAL.into());
            }
            return Ok(0);
        };
        let value = if !guest
            .config()
            .backend
            .requires_thread_directed_process_signals
        {
            self.record_or_replay(guest, call).await?
        } else {
            let targeted = syscalls::Tgkill::new()
                .with_tgid(process)
                .with_tid(tid.as_raw())
                .with_sig(call.sig());
            self.record_or_replay(guest, targeted).await?
        };
        self.notify_cross_task_signal(guest, tid, call.sig(), Some(DetPid::from_raw(process)))
            .await;
        Ok(value)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/4046)
    /// Refuse by name a `kill` to a process group (`pid` 0 for the caller's own,
    /// `-pgid` for another) or to every process it may signal (`pid` -1).
    ///
    /// Linux sends the signal to each process in the set
    /// (`__kill_pgrp_info`, `kill_something_info` in kernel/signal.c), and each
    /// process then picks its receiving thread as a process-directed signal
    /// does. Detcore models neither the set's membership nor that choice, so
    /// the call goes through the unsupported-operation policy: a fail-closed
    /// run stops with the policy-refusal status and
    /// `--allow-unsupported-syscalls` returns ENOSYS, as the call did before.
    /// A SIGKILL to another group is forwarded before this point
    /// (`can_forward_process_group_signal`) when threads are sequentialized,
    /// except on a backend that needs thread-directed process signals (DBT),
    /// where it is refused here too. Without sequentialization every such kill
    /// is refused here, for host safety: the guest's process group is
    /// inherited from Hermit's (https://github.com/rrnewton/hermit/issues/4057).
    async fn refuse_group_signal<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pid: libc::pid_t,
        sig: libc::c_int,
    ) -> Result<i64, Error> {
        let target = match pid {
            0 => "its own process group".to_owned(),
            -1 => "every process it may signal".to_owned(),
            pid => format!("process group {}", pid.unsigned_abs()),
        };
        let reason = if guest.config().sequentialize_threads {
            "Detcore does not model process-group membership or each process's receiving \
             thread (https://github.com/rrnewton/hermit/issues/4046)"
        } else {
            "with threads not sequentialized (--no-sequentialize-threads, --strace-only) it \
             would go to the host kernel as is, and the guest's process group is inherited from \
             Hermit's, so it can reach Hermit and the processes that started it \
             (https://github.com/rrnewton/hermit/issues/4057)"
        };
        tracing::error!(
            "[detcore, dtid {}] signal {} to {} by kill is not supported: {}",
            guest.thread_state().dettid,
            sig,
            target,
            reason,
        );
        self.refuse_unserviceable_operation(guest, Sysno::kill, Errno::ENOSYS)
            .await
    }

    /// Decide a process-directed `sig` that the calling thread sends by `sysno`
    /// to `pid` when `pid` names its own process (`process`, the process that
    /// `pid` names: any of its live threads' ids names it), and the process
    /// has another live thread (https://github.com/rrnewton/hermit/issues/4034).
    /// Only on ptrace, where Detcore reads every input from the schedule.
    async fn own_process_signal<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        pid: libc::pid_t,
        process: libc::pid_t,
        sig: libc::c_int,
    ) -> Result<OwnProcessSend, Error> {
        if !guest
            .config()
            .backend_supports_blocked_wait_signal_interruption
        {
            return Ok(OwnProcessSend::NotOwn);
        }
        let sender = guest.thread_state().dettid;
        if process != self.detpid.as_raw() {
            return Ok(OwnProcessSend::NotOwn);
        }
        // Linux prefers the thread `pid` names when it takes the signal; a
        // sibling's id, like the process's, leaves the sender as the receiver
        // only when every sibling blocks it.
        let names_sender = pid == sender.as_raw();
        let inputs = own_process_signal_inputs(guest).await;
        if inputs
            .siblings
            .iter()
            .all(|(_, state)| *state == SiblingSignalState::ExitGranted)
        {
            return Ok(OwnProcessSend::NotOwn);
        }
        let state = read_thread_wait_signal_state(guest.pid(), guest.tid())?;
        let send = OwnProcessSignal {
            sig,
            names_sender,
            blocked: state.blocked,
            thread_pending: state.thread_pending,
            ignored: state.ignored,
            caught: state.caught,
            inputs,
            record_replay: self.cfg.recordreplay_modes,
            schedule_replay: self.cfg.replay_schedule_from.is_some(),
            stacktrace_signal: self.cfg.stacktrace_signal.is_some(),
            posthook_anchor: !guest.thread_state().stats.hb_pending_posthook.is_empty(),
            signalfd: guest.thread_state().holds_a_signalfd(),
        };
        tracing::debug!(
            "[detcore, dtid {}] signal {} to its own process: {:?}",
            sender,
            sig,
            send
        );
        let Some(refusal) = own_process_signal_refusal(&send) else {
            return Ok(OwnProcessSend::Admitted);
        };
        tracing::error!(
            "[detcore, dtid {}] process-directed signal {} to process {} by {} is not supported: \
             {} (https://github.com/rrnewton/hermit/issues/4034)",
            sender,
            sig,
            self.detpid,
            sysno,
            refusal,
        );
        Ok(OwnProcessSend::Refused(
            self.refuse_unserviceable_operation(guest, sysno, Errno::ENOSYS)
                .await,
        ))
    }

    /// Forward an admitted process-directed signal to the sender's own process
    /// unchanged, holding the sender's turn until it takes the signal
    /// (`own_signal_turn_hold`). A call the kernel fails sends nothing and
    /// holds nothing.
    async fn forward_own_process_signal<G: Guest<Self>, C>(
        &self,
        guest: &mut G,
        sig: libc::c_int,
        call: C,
    ) -> Result<i64, Error>
    where
        C: Into<syscalls::Syscall>,
        syscalls::Syscall: From<C>,
    {
        guest.thread_state_mut().own_signal_turn_hold = Some(sig);
        let result = self.record_or_replay(guest, call).await;
        if result.is_err() {
            guest.thread_state_mut().own_signal_turn_hold = None;
        }
        Ok(result?)
    }

    /// What an `rt_sigqueueinfo` whose `siginfo_t` Hermit may not be able to
    /// read comes to, decided before admission (`do_rt_sigqueueinfo` in
    /// kernel/signal.c). No call that Linux could deliver reaches the kernel
    /// without admission (Codex re-check 2 of
    /// https://github.com/rrnewton/hermit/pull/4116).
    ///
    /// The kernel always copies the first 48 bytes (`kernel_siginfo_t`) and
    /// the other 80 only for a `si_code` whose layout it does not know
    /// (`copy_siginfo_from_user`). Its `copy_from_user` reads what user mode
    /// can: on x86 a page with any of read, write or execute permission; it
    /// faults on a `PROT_NONE` page; and a fault in a hole just below a
    /// grow-down mapping can grow that mapping and succeed. The tracer's own
    /// reads say nothing about that (`process_vm_readv` refuses a write-only
    /// page, and `/proc/<tid>/mem` force-reads a `PROT_NONE` one), so the
    /// mappings of `/proc/<tid>/smaps` decide first:
    /// - the first byte of the 48 that user mode cannot read lies in a
    ///   `PROT_NONE` mapping, or in a hole whose next mapping above is not
    ///   grow-down (`VmFlags` `gd`; only that mapping can grow into it):
    ///   EFAULT is certain, and Hermit answers it without the kernel;
    /// - any other byte of the 128 that user mode cannot read (a hole below a
    ///   grow-down mapping, a tail running into a hole or a `PROT_NONE` page),
    ///   or a listing that cannot be read: `Unreadable`, refused by name;
    /// - all 128 user-readable: the tracer reads them (`process_vm_readv`, then
    ///   `/proc/<tid>/mem` for a write-only page), and a page neither reaches
    ///   (a `VM_IO`/`VM_PFNMAP` page such as the vDSO's `[vvar]`) is
    ///   `Unreadable`. A `si_code` only the target thread itself may use
    ///   (`si_code >= 0` or `SI_TKILL`, from a caller that is not `pid`) is
    ///   then `KernelFails`: after a complete copy Linux fails it with EPERM,
    ///   or E2BIG first, and sends nothing, so it goes to the kernel unchanged.
    fn rt_sigqueueinfo_precheck<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &syscalls::RtSigqueueinfo,
    ) -> RtSigqueueinfoPrecheck {
        const SIZE: usize = std::mem::size_of::<libc::siginfo_t>();
        // `sizeof(kernel_siginfo_t)` on x86_64: the part every copy reads.
        const KERNEL_SIGINFO: usize = 48;
        let address = call.siginfo().map_or(0, |address| address.as_raw());
        let Ok(smaps) = std::fs::read_to_string(format!("/proc/{}/smaps", guest.tid().as_raw()))
        else {
            return RtSigqueueinfoPrecheck::Unreadable;
        };
        let mappings = parse_smaps(&smaps);
        match first_user_unreadable(&mappings, address, KERNEL_SIGINFO) {
            None => {}
            Some(Unreadable::NoPermission)
            | Some(Unreadable::Hole {
                below_growsdown: false,
            }) => {
                return RtSigqueueinfoPrecheck::Efault;
            }
            Some(Unreadable::Hole {
                below_growsdown: true,
            }) => {
                return RtSigqueueinfoPrecheck::Unreadable;
            }
        }
        if first_user_unreadable(&mappings, address, SIZE).is_some() {
            return RtSigqueueinfoPrecheck::Unreadable;
        }
        let read = call.siginfo().and_then(|info| {
            guest.memory().read_value(info).ok().or_else(|| {
                use std::os::unix::fs::FileExt;
                let mut bytes = [0u8; SIZE];
                std::fs::File::open(format!("/proc/{}/mem", guest.tid().as_raw()))
                    .and_then(|mem| mem.read_exact_at(&mut bytes, address as u64))
                    .ok()?;
                // SAFETY: `siginfo_t` is plain data, and `bytes` holds exactly
                // one, read from the guest.
                Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::siginfo_t>()) })
            })
        });
        let Some(info) = read else {
            return RtSigqueueinfoPrecheck::Unreadable;
        };
        let code = info.si_code;
        if (code >= 0 || code == libc::SI_TKILL)
            && guest.thread_state().dettid.as_raw() != call.tgid()
        {
            RtSigqueueinfoPrecheck::KernelFails
        } else {
            RtSigqueueinfoPrecheck::Proceeds
        }
    }

    /// Record, in the sending call's turn and before it reaches the kernel, the
    /// signals a send of a default-stop signal lets Linux send to an orphaned
    /// process group later (`orphaned_group_signals`), for every process of
    /// the container, whatever the target and its disposition and whether the
    /// send then succeeds: no gated wait ends for them, and a process-directed
    /// send the sender does not block them for is refused
    /// (https://github.com/rrnewton/hermit/issues/4034). A send of SIGSTKFLT,
    /// Reverie's timer marker, is recorded too
    /// (`Scheduler::record_guest_marker_signal`).
    pub(crate) async fn record_orphaned_group_signals<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sig: libc::c_int,
    ) {
        if sig == libc::SIGSTKFLT {
            crate::tool_global::record_guest_marker_signal(guest).await;
        }
        let signals = orphaned_group_signals(sig);
        if signals != 0
            && guest
                .config()
                .backend_supports_blocked_wait_signal_interruption
        {
            record_host_timed_signals(guest, HostTimedSignalScope::Container, signals).await;
        }
    }

    /// Whether to drop, answering 0 and sending nothing, a signal `sig` the
    /// guest addresses to `target` (a pid, or a tid in thread group `tgid` for
    /// the calls that name one) that is one of Hermit's own tasks: its container
    /// init, PID 1 of the guest's PID namespace, or another thread of that
    /// process. A forwarded SIGTERM ended Hermit's run
    /// (https://github.com/rrnewton/hermit/issues/4066).
    ///
    /// This is what Linux does for a signal sent from inside a PID namespace to
    /// its init when the init has no handler for it: `sig_task_ignored` drops
    /// it (the init is `SIGNAL_UNKILLABLE`, and a sender in its own namespace
    /// does not force it), SIGKILL included, and the call returns 0. To the
    /// guest Hermit is such an init; the handlers Hermit installs are for
    /// supervisors outside the namespace. Signal 0, an invalid signal number
    /// and a nonpositive target go to the caller's own path, as do targets
    /// that are guest tasks ([`crate::tool_global::hermit_task_named`]).
    async fn drop_signal_to_hermit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        tgid: Option<libc::pid_t>,
        target: libc::pid_t,
        sig: libc::c_int,
    ) -> bool {
        if !(1..=libc::SIGRTMAX()).contains(&sig) || target <= 0 {
            return false;
        }
        if !crate::tool_global::names_hermit_task(guest, tgid, target).await {
            return false;
        }
        tracing::info!(
            "[detcore, dtid {}] {} of signal {} to task {}, one of Hermit's own: dropped and \
             answered 0, as Linux drops a signal from inside a PID namespace to its init \
             (https://github.com/rrnewton/hermit/issues/4066)",
            guest.thread_state().dettid,
            sysno,
            sig,
            target,
        );
        true
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/4034)
    /// The thread, of `tgid`'s live `targets`, that a process-directed signal
    /// `sig` is routed to.
    ///
    /// With several live threads the call is refused by name: Linux's
    /// `complete_signal` (kernel/signal.c) gives the signal to any thread that
    /// `wants_signal` (one that does not block it, is not exiting and is not
    /// stopped), which depends on the threads' masks and run states, and Detcore
    /// does not model that choice. The refusal goes through the
    /// unsupported-operation policy, so a fail-closed run stops with the
    /// policy-refusal status and `--allow-unsupported-syscalls` returns ENOSYS.
    pub(crate) async fn process_signal_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        tgid: DetPid,
        sig: libc::c_int,
    ) -> Result<Option<DetTid>, Error> {
        let (targets, zombie) = resolve_kill_targets_and_exit(guest, tgid).await;
        match deterministic_kill_target(&targets, zombie, sig) {
            Err(Errno::ENOSYS) => {
                tracing::error!(
                    "[detcore, dtid {}] process-directed signal {} to process {} by {} is not supported: \
                     the process has {} live threads, and Detcore does not choose the receiving \
                     thread as Linux's complete_signal does \
                     (https://github.com/rrnewton/hermit/issues/4034)",
                    guest.thread_state().dettid,
                    sig,
                    tgid,
                    sysno,
                    targets.len(),
                );
                self.refuse_unserviceable_operation(guest, sysno, Errno::ENOSYS)
                    .await
                    .and(Err(Errno::ENOSYS.into()))
            }
            target => Ok(target?),
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Send a thread-directed signal through the kernel. Guest PID/TID values are
    /// stable in the fresh PID namespace and delivery is scheduler-serialized.
    pub async fn handle_tgkill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Tgkill,
    ) -> Result<i64, Error> {
        self.record_orphaned_group_signals(guest, call.sig()).await;
        if call.sig() == libc::SIGALRM {
            refuse_sigalrm(guest, SigalrmControl::SendTo(DetTid::from_raw(call.tid()))).await?;
        }
        if self
            .drop_signal_to_hermit(
                guest,
                Sysno::tgkill,
                Some(call.tgid()),
                call.tid(),
                call.sig(),
            )
            .await
        {
            return Ok(0);
        }
        let _reserved = self
            .reserve_controlled_self_sigkill_exit(
                guest,
                call.sig(),
                Some(DetPid::from_raw(call.tgid())),
                Some(DetTid::from_raw(call.tid())),
            )
            .await;
        // A SIGKILL to a thread of another process ends that whole process,
        // and is retired at this turn, like `kill`'s. Only a valid pair is
        // reserved: Linux rejects a thread outside the named group (ESRCH) or a
        // non-positive id (EINVAL) before anything happens, so such a send
        // takes the ordinary path and gets the kernel's errno.
        if call.sig() == libc::SIGKILL
            && guest.config().sequentialize_threads
            && call.tgid() > 0
            && call.tid() > 0
            && registered_process_of(guest, DetTid::from_raw(call.tid())).await
                == Some(DetPid::from_raw(call.tgid()))
            && let Some(reservation) = reserve_sigkill(guest, DetTid::from_raw(call.tid())).await
        {
            return self.send_reserved_sigkill(guest, reservation, call).await;
        }
        let value = self.record_or_replay(guest, call).await?;
        // `pthread_kill` lowers to `tgkill`, so this is the ordinary way one
        // guest THREAD signals a sibling. Like `kill`, a successful cross-task
        // send must tell the scheduler, or a target parked in a child wait is
        // never woken and the wait hangs.
        self.notify_cross_task_signal(guest, DetTid::from_raw(call.tid()), call.sig(), None)
            .await;
        Ok(value)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#812)
    /// Send a thread-directed signal through the older two-argument `tkill`.
    /// Like its `tgkill` sibling, the target thread is addressed by a guest TID
    /// that is stable in the fresh PID namespace and delivery is
    /// scheduler-serialized, so forwarding the kernel call is deterministic.
    pub async fn handle_tkill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Tkill,
    ) -> Result<i64, Error> {
        self.record_orphaned_group_signals(guest, call.sig()).await;
        if call.sig() == libc::SIGALRM {
            refuse_sigalrm(guest, SigalrmControl::SendTo(DetTid::from_raw(call.tid()))).await?;
        }
        if self
            .drop_signal_to_hermit(guest, Sysno::tkill, None, call.tid(), call.sig())
            .await
        {
            return Ok(0);
        }
        let _reserved = self
            .reserve_controlled_self_sigkill_exit(
                guest,
                call.sig(),
                None,
                Some(DetTid::from_raw(call.tid())),
            )
            .await;
        // A non-positive tid is EINVAL in Linux, before anything happens.
        if call.sig() == libc::SIGKILL
            && guest.config().sequentialize_threads
            && call.tid() > 0
            && let Some(reservation) = reserve_sigkill(guest, DetTid::from_raw(call.tid())).await
        {
            return self.send_reserved_sigkill(guest, reservation, call).await;
        }
        let value = self.record_or_replay(guest, call).await?;
        // Same wakeup obligation as `tgkill`; `tkill` is the older two-argument
        // spelling of the same thread-directed send.
        self.notify_cross_task_signal(guest, DetTid::from_raw(call.tid()), call.sig(), None)
            .await;
        Ok(value)
    }

    /// Complete a reserved SIGKILL (https://github.com/rrnewton/hermit/issues/3994).
    ///
    /// A send whose victims exclude the caller's own process returns: it is
    /// sent, and then committed with its real result. One that includes the
    /// caller's process cannot return, because the kernel ends the caller:
    /// the caller records its own exit status for its parent (`CLD_KILLED`,
    /// `SIGKILL`), takes the `exit_group` grant for its own process, as
    /// `exit_group` does, commits the other victims inside that exit turn, and
    /// then sends. The commit comes after the grant: it installs the physical
    /// fence, which waits for victims that only the send itself kills. The
    /// reservation validated the target against the scheduler's record, so the
    /// send cannot fail in a way the caller sees.
    pub(crate) async fn send_reserved_sigkill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        reservation: crate::scheduler::sigkill_retirement::SigkillReservation,
        call: impl Into<syscalls::Syscall> + Copy,
    ) -> Result<i64, Error> {
        if !reservation.includes_sender {
            let result = self.record_or_replay(guest, call).await;
            commit_sigkill(guest, reservation.token, result.is_ok()).await;
            return Ok(result?);
        }
        if self.cfg.backend.reports_child_exit_publication {
            let uid = crate::syscalls::real_uid(guest.tid().as_raw())
                .unwrap_or_else(|| unsafe { libc::getuid() });
            crate::tool_global::child_exit_sigchld(
                guest,
                crate::scheduler::child_exit_sigchld::ChildExitSigchldControl::ExitStatus {
                    code: libc::CLD_KILLED,
                    status: libc::SIGKILL,
                    uid,
                },
            )
            .await;
        }
        let request = guest.thread_state().mk_request(
            ResourceID::Exit {
                group: true,
                process: guest.thread_state().detpid.expect("detpid unset"),
                mm: guest.thread_state().mm_id,
            },
            Permission::RW,
        );
        resource_request(guest, request).await;
        commit_sigkill(guest, reservation.token, true).await;
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// Tell the scheduler that a successful thread-directed send left a signal
    /// physically pending for another task.
    ///
    /// Shared by `kill`, `tgkill` and `tkill` so the three cannot drift: a fix
    /// applied to one spelling of "signal another task" must apply to all of
    /// them, which is exactly the gap that let a `pthread_kill` from a sibling
    /// thread hang a `waitid` that a `kill` from a sibling process could
    /// interrupt. Self-directed signals are excluded: the sender is running, so
    /// it is not parked waiting to be woken.
    pub(crate) async fn notify_cross_task_signal<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target: DetTid,
        raw_signal: i32,
        target_process: Option<DetPid>,
    ) {
        // ⚠️ NO `Signal::try_from` GATE. It used to stand here, and because
        // `nix`'s `Signal` models only 1..=31 it rejected EVERY realtime signal:
        // measured in-tree, the gate admitted exactly 1..=31 and zero of the 31
        // realtime signals. The notification was skipped silently, so a target
        // parked on `ResourceID::WaitChild` was never woken and the wait hung.
        // `SigWrapper` now carries the raw number, so every deliverable signal
        // can be represented and none is dropped for being unnameable. Signal
        // zero is only an existence/permission probe and queues no signal.
        if should_notify_cross_task_signal(guest.thread_state().dettid, target, raw_signal) {
            notify_signal_pending(guest, target, SigWrapper(raw_signal), target_process).await;
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#812)
    /// Queue a thread-directed signal with an accompanying `siginfo_t`. Like
    /// `tgkill`, the target is a specific thread named by stable guest TGID/TID
    /// and delivery is scheduler-serialized; the guest-supplied siginfo is
    /// deterministic input, so forwarding the kernel call is deterministic.
    pub async fn handle_rt_tgsigqueueinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtTgsigqueueinfo,
    ) -> Result<i64, Error> {
        self.record_orphaned_group_signals(guest, call.sig()).await;
        if call.sig() == libc::SIGALRM {
            refuse_sigalrm(guest, SigalrmControl::SendTo(DetTid::from_raw(call.tid()))).await?;
        }
        if self
            .drop_signal_to_hermit(
                guest,
                Sysno::rt_tgsigqueueinfo,
                Some(call.tgid()),
                call.tid(),
                call.sig(),
            )
            .await
        {
            return Ok(0);
        }
        let value = self.record_or_replay(guest, call).await?;
        // Thread-directed like `tgkill`, so it carries the same wakeup
        // obligation. `sigqueue`/`pthread_sigqueue` reach a parked sibling
        // through here.
        self.notify_cross_task_signal(guest, DetTid::from_raw(call.tid()), call.sig(), None)
            .await;
        Ok(value)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#812)
    // TODO-HUMAN-REVIEW(PR-1058): Review queued process-signal preservation.
    /// Queue a process-directed signal with an accompanying `siginfo_t`. Mirrors `handle_kill`:
    /// preserve process-directed delivery when the backend accepts guest PIDs, otherwise route an
    /// unambiguous positive-PID target to its sole live thread via `rt_tgsigqueueinfo`. Ambiguous
    /// multithreaded process-directed delivery is refused until Detcore models eligible masks.
    pub async fn handle_rt_sigqueueinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigqueueinfo,
    ) -> Result<i64, Error> {
        self.record_orphaned_group_signals(guest, call.sig()).await;
        if !guest.config().sequentialize_threads {
            if self
                .drop_signal_to_hermit(guest, Sysno::rt_sigqueueinfo, None, call.tgid(), call.sig())
                .await
            {
                return Ok(0);
            }
            return Ok(self.record_or_replay(guest, call).await?);
        }

        let tgid = call.tgid();
        if tgid <= 0 {
            // POSIX.1b has no process groups: Linux's `do_rt_sigqueueinfo` looks
            // the pid up with `find_vpid`, which finds no process for 0 or a
            // negative pid, so the call fails (EFAULT for an unreadable
            // `siginfo_t`, EPERM for a forged `si_code`, E2BIG for an unknown
            // `si_code` with nonzero expansion bytes, otherwise ESRCH) and
            // sends nothing. The kernel's own answer is therefore deterministic
            // (https://github.com/rrnewton/hermit/issues/4046).
            return Ok(self.record_or_replay(guest, call).await?);
        }
        if call.sig() == 0 {
            // An existence check answers from the same exit model as a signal
            // (https://github.com/rrnewton/hermit/issues/4124): a process
            // Detcore saw exit and that is gone is ESRCH after the siginfo
            // checks, whatever the host has reaped by now.
            let (targets, zombie) =
                resolve_kill_targets_and_exit(guest, DetPid::from_raw(tgid)).await;
            if targets.is_empty() && zombie == Some(false) {
                return Ok(self
                    .record_or_replay(guest, call.with_tgid(NO_SUCH_PID))
                    .await?);
            }
            return Ok(self.record_or_replay(guest, call).await?);
        }
        if call.sig() == libc::SIGALRM {
            refuse_sigalrm(guest, SigalrmControl::SendTo(DetTid::from_raw(tgid))).await?;
        }
        if self
            .drop_signal_to_hermit(guest, Sysno::rt_sigqueueinfo, None, tgid, call.sig())
            .await
        {
            return Ok(0);
        }
        // A call the kernel fails without sending, after a complete copy of
        // its `siginfo_t` (EPERM for a `si_code` only the target thread itself
        // may use), goes to it unchanged; a copy that certainly faults is
        // answered EFAULT here; and a `siginfo_t` Hermit cannot read whole is
        // refused by name (`rt_sigqueueinfo_precheck`).
        if guest
            .config()
            .backend_supports_blocked_wait_signal_interruption
        {
            match self.rt_sigqueueinfo_precheck(guest, &call) {
                RtSigqueueinfoPrecheck::KernelFails => {
                    return Ok(self.record_or_replay(guest, call).await?);
                }
                RtSigqueueinfoPrecheck::Efault => return Err(Errno::EFAULT.into()),
                RtSigqueueinfoPrecheck::Unreadable => {
                    tracing::error!(
                        "[detcore, dtid {}] rt_sigqueueinfo with a siginfo_t at {:#x} is not \
                         supported: Hermit cannot read all 128 bytes of it to decide the call \
                         (https://github.com/rrnewton/hermit/issues/4034)",
                        guest.thread_state().dettid,
                        call.siginfo().map_or(0, |info| info.as_raw()),
                    );
                    return self
                        .refuse_unserviceable_operation(
                            guest,
                            Sysno::rt_sigqueueinfo,
                            Errno::ENOSYS,
                        )
                        .await;
                }
                RtSigqueueinfoPrecheck::Proceeds => {}
            }
        }
        // As for kill, a live non-leader thread's id names its process
        // (https://github.com/rrnewton/hermit/issues/4094).
        let process = signal_target_process(guest, DetPid::from_raw(tgid))
            .await
            .map_or(tgid, |process| process.as_raw());
        match self
            .own_process_signal(guest, Sysno::rt_sigqueueinfo, tgid, process, call.sig())
            .await?
        {
            OwnProcessSend::Admitted => {
                return self
                    .forward_own_process_signal(guest, call.sig(), call)
                    .await;
            }
            OwnProcessSend::Refused(answer) => return answer,
            OwnProcessSend::NotOwn => {}
        }
        let target = match self
            .process_signal_target(
                guest,
                Sysno::rt_sigqueueinfo,
                DetPid::from_raw(process),
                call.sig(),
            )
            .await
        {
            // Gone: Linux checks the siginfo before it looks the process up
            // (EFAULT, then E2BIG for an unknown code with padding, then EPERM
            // for a forged code), and only then answers ESRCH. Let the kernel
            // make those checks against a pid no task can have, so its answer
            // cannot depend on whether the host has reaped the real one yet
            // (https://github.com/rrnewton/hermit/issues/4124).
            Err(Error::Errno(Errno::ESRCH)) => {
                return Ok(self
                    .record_or_replay(guest, call.with_tgid(NO_SUCH_PID))
                    .await?);
            }
            target => target?,
        };
        let Some(tid) = target else {
            // A zombie: its parent can still wait for it, so Linux still
            // finds it. Unlike kill, the kernel checks the siginfo first
            // (EFAULT, EPERM for a forged si_code, E2BIG), so let it answer:
            // it validates, finds the zombie, and returns 0. A backend that
            // must name a receiving thread has none to name, and keeps ESRCH
            // (https://github.com/rrnewton/hermit/issues/4091).
            if guest
                .config()
                .backend
                .requires_thread_directed_process_signals
            {
                return Err(Errno::ESRCH.into());
            }
            return Ok(self.record_or_replay(guest, call).await?);
        };
        let value = if !guest
            .config()
            .backend
            .requires_thread_directed_process_signals
        {
            self.record_or_replay(guest, call).await?
        } else {
            let targeted = syscalls::RtTgsigqueueinfo::new()
                .with_tgid(process)
                .with_tid(tid.as_raw())
                .with_sig(call.sig())
                .with_siginfo(call.siginfo());
            self.record_or_replay(guest, targeted).await?
        };
        self.notify_cross_task_signal(guest, tid, call.sig(), Some(DetPid::from_raw(process)))
            .await;
        Ok(value)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Read the kernel pending-signal mask after Detcore has serialized all signal
    /// generation and delivery events that can change it.
    pub async fn handle_rt_sigpending<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigpending,
    ) -> Result<i64, Error> {
        // Signal phase 1 refuses rt_sigpending in a process that handles
        // SIGALRM (`sigalrm_phase1`), so the ledger's entry never needs to be
        // shown here. The pending set would show a real-time timer expiration
        // that every thread blocked when it expired, which the kernel never
        // queued (https://github.com/rrnewton/hermit/issues/4089).
        let pending = pending_real_time_among(guest, REAL_TIME_SIGNALS).await;
        act_on_pending_real_time(guest, pending, REAL_TIME_SIGNALS, 0).await;
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// A delivered signal may run a handler, whose return restores, through an
    /// `rt_sigreturn` that ptrace does not stop at, a mask Detcore cannot read
    /// here: the thread's own mask, or for a handler that interrupts a call
    /// with a temporary mask, the mask saved before the call. So every
    /// delivery records every real-time signal as possibly unblocked by a
    /// handler's return (`Scheduler::handler_restore_unblocked`), and a
    /// real-time timer expiration of the process is never kept pending after
    /// it (https://github.com/rrnewton/hermit/issues/4089). Only where real-time
    /// expirations are kept pending at all (`models_signal_targets`), and once
    /// per thread.
    pub(crate) async fn record_handler_restore_mask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        signal: SigWrapper,
    ) {
        if !(self.cfg.sequentialize_threads
            && self.cfg.backend_supports_blocked_wait_signal_interruption)
            || guest.thread_state().handler_restore_rt_reported == REAL_TIME_SIGNALS
        {
            return;
        }
        // Only a caught signal runs a handler that returns. The dispositions
        // are the process's, so they say that whatever mask the delivery
        // interrupted; a signal acted on by default (the teardown's own
        // termination signals among them) records nothing.
        if let Ok(state) = read_thread_wait_signal_state(guest.pid(), guest.tid())
            && state.caught & kernel_sigset_bit(signal.raw()) == 0
        {
            return;
        }
        let Some(detpid) = guest.thread_state().detpid else {
            return;
        };
        crate::tool_global::record_handler_restore_mask(guest, detpid, REAL_TIME_SIGNALS).await;
        guest.thread_state_mut().handler_restore_rt_reported = REAL_TIME_SIGNALS;
    }

    /// rt_sigreturn: the mask it restores, from the frame's `uc_sigmask`, may
    /// unblock a pending real-time timer expiration that every thread blocked
    /// when it expired, which the kernel then delivers as the call returns
    /// (https://github.com/rrnewton/hermit/issues/4089). The call itself
    /// passes through.
    pub(crate) async fn observe_sigreturn_mask<G: Guest<Self>>(&self, guest: &mut G) {
        let pending = pending_real_time_among(guest, REAL_TIME_SIGNALS).await;
        if pending == 0 {
            return;
        }
        // x86-64 `rt_sigreturn` reads `frame = sp - sizeof(long)` and restores
        // `frame->uc.uc_sigmask`; `uc` starts at `sp`.
        let sp = guest.regs().await.rsp as usize;
        let restored = Addr::<u64>::from_raw(sp.wrapping_add(UC_SIGMASK_OFFSET))
            .and_then(|addr| guest.memory().read_value(addr).ok());
        let (observable, discarded) = match (
            restored,
            read_thread_wait_signal_state(guest.pid(), guest.tid()),
        ) {
            (Some(mask), Ok(state)) => unblocked_by(&state, mask),
            // A frame the kernel cannot read either kills the guest with
            // SIGSEGV, which delivers nothing pending.
            (None, _) => (0, 0),
            // A state that cannot be read could hide any delivery.
            (Some(_), Err(_)) => (REAL_TIME_SIGNALS, 0),
        };
        act_on_pending_real_time(guest, pending, observable, discarded).await;
    }
}

/// The offset of `uc_sigmask` in the x86-64 kernel `ucontext` that
/// `rt_sigreturn` restores: after uc_flags (8), uc_link (8), uc_stack (24) and
/// uc_mcontext (256 bytes).
const UC_SIGMASK_OFFSET: usize = 8 + 8 + 24 + 256;

fn should_notify_cross_task_signal(sender: DetTid, target: DetTid, raw_signal: i32) -> bool {
    raw_signal != 0 && target != sender
}

/// Which check [`sleep_signal`] makes for an emulated `pause` or `nanosleep`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SleepCheck {
    /// Before the sleep parks: whether a signal already pending for the thread
    /// ends it at once.
    BeforePark,
    /// After the scheduler woke the sleep for these signals.
    AfterWake(Option<Vec<SigWrapper>>),
}

/// What ends an emulated `pause` or `nanosleep`, per [`sleep_signal`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SleepSignal {
    /// A signal pending for this thread, unblocked and caught or acted on by
    /// default: the kernel delivers it as the call returns. `pause` returns
    /// `ERESTARTNOHAND` for it and `nanosleep` `EINTR`.
    Pending,
    /// The scheduler ended the sleep for a signal whose pending bit cannot
    /// decide (below), or this backend does not report the guest's signal
    /// state: the call returns `EINTR`, as every wake did before.
    Held,
    /// Nothing ends the sleep, and it waits (again) with its original deadline.
    None,
}

/// Decides, in this thread's own turn, whether a signal ends an emulated
/// `pause` or `nanosleep` (<https://github.com/rrnewton/hermit/issues/3982>).
///
/// Only on a backend whose kernel reports the guest's signal state; on any
/// other, a sleep never checks before it parks, and a wake always ends it
/// (`Held`). A signal ends the sleep when it is pending for this thread,
/// unblocked, and caught or acted on by default
/// (`KernelSignalState::pending_interrupting`). Before the sleep parks, a stop
/// signal does so only when `stops_end_it`, which `nanosleep` leaves unset
/// because Detcore does not keep its deadline for the kernel's restart after a
/// stop, so the stop takes effect when the sleep ends, as before. After a wake,
/// which only the scheduler's own sends (a timer, a child-exit `SIGCHLD`) can
/// commit for a stop on a `nanosleep`, a pending stop ends it, as every wake
/// did before, so that the stop takes effect promptly.
///
/// `SIGCHLD` and the signals a host-timed source armed by a guest can post
/// (`host_timed_signals`) are held: the kernel can post them at a moment set by
/// host timing, and `/proc` cannot tell that copy from one a guest sent, so
/// their pending bits are never read. They never end a sleep before it parks,
/// nor does the scheduler record a guest's cross-task send of one for a sleep;
/// a wake the scheduler committed for one of its own, a child-exit `SIGCHLD`,
/// ends it with `Held`, as before. Like the precise futex
/// wait's check, this reads the pending set before the sleep parks as well as
/// after a wake: a signal sent while the thread was stopped short of filing
/// its request, as when its timeslice ended in the syscall's prehook, is
/// pending and was never recorded for its sleep. After a wake whose
/// non-held signals are no longer pending, a `SIGCONT` having cancelled a stop
/// or `sigaction(SIG_IGN)` having flushed it, a Linux sleep would still be
/// asleep, so this returns `None`.
///
/// A state that cannot be read is handled as a wait's first read is
/// (`read_wait_signal_state`, here `read_thread_wait_signal_state`, since
/// Reverie can report the creator's pid for a process a raw `clone` made): a
/// thread that no longer exists ends the call
/// with `ERESTARTNOINTR`, which nothing observes, and otherwise the run ends
/// with a diagnostic.
pub(crate) async fn sleep_signal<G, T>(
    guest: &mut G,
    check: SleepCheck,
    stops_end_it: bool,
) -> Result<SleepSignal, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if !guest
        .config()
        .backend_supports_blocked_wait_signal_interruption
    {
        return Ok(match check {
            SleepCheck::BeforePark => SleepSignal::None,
            SleepCheck::AfterWake(_) => SleepSignal::Held,
        });
    }
    let held = kernel_sigset_bit(libc::SIGCHLD) | host_timed_signals(guest).await;
    let before_park = check == SleepCheck::BeforePark;
    let (deciding, held_wake) = match check {
        SleepCheck::BeforePark => (!held, false),
        SleepCheck::AfterWake(None) => (0, true),
        SleepCheck::AfterWake(Some(woken)) => {
            let woken = woken
                .iter()
                .fold(0, |set, signal| set | kernel_sigset_bit(signal.raw()));
            (woken & !held, woken & held != 0)
        }
    };
    if deciding != 0 {
        let state = read_thread_wait_signal_state(guest.pid(), guest.tid())?;
        let mut ending = state.pending_interrupting(state.blocked, false) & deciding;
        if !stops_end_it && before_park {
            ending &= !(state.default_job_control_stops() | kernel_sigset_bit(libc::SIGSTOP));
        }
        if ending != 0 {
            return Ok(SleepSignal::Pending);
        }
    }
    Ok(if held_wake {
        SleepSignal::Held
    } else {
        SleepSignal::None
    })
}

/// The size of a `struct signalfd_siginfo`, which has ssi_signo at 0,
/// ssi_code at 8, ssi_pid at 12, ssi_uid at 16, ssi_status at 40, ssi_utime at
/// 56 and ssi_stime at 64.
const SIGNALFD_RECORD: usize = 128;

fn signalfd_field(record: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(record[at..at + 4].try_into().expect("four bytes"))
}

/// For each `signalfd_siginfo` record in `bytes`, the child-exit ledger
/// question it raises (`Detcore::reconcile_signalfd_records`): a SIGCHLD with
/// a child-exit code is the kernel's copy for that child, a SIGCHLD with
/// `SI_USER` from `tracer` is the scheduler's copy, and any other record raises
/// none.
fn signalfd_record_controls(
    bytes: &[u8],
    tracer: u32,
) -> Vec<Option<crate::scheduler::child_exit_sigchld::ChildExitSigchldControl>> {
    use crate::scheduler::child_exit_sigchld::ChildExitSigchldControl;
    bytes
        .chunks(SIGNALFD_RECORD)
        .map(|record| {
            if signalfd_field(record, 0) != libc::SIGCHLD as u32 {
                return None;
            }
            let code = signalfd_field(record, 8) as i32;
            let pid = signalfd_field(record, 12);
            if crate::scheduler::child_exit_sigchld::is_child_exit_code(code) {
                Some(ChildExitSigchldControl::ClaimKernelCopy {
                    child: DetPid::from_raw(pid as i32),
                })
            } else if code == libc::SI_USER && pid == tracer {
                Some(ChildExitSigchldControl::TakeOwnCopy)
            } else {
                None
            }
        })
        .collect()
}

/// The pieces of the guest buffers `extents` (address, length) that hold the
/// first `len` bytes a vectored read scattered, in order; empty extents are
/// skipped.
pub(crate) fn signalfd_extent_pieces(
    extents: &[(usize, usize)],
    len: usize,
) -> Vec<(usize, usize)> {
    let mut left = len;
    let mut pieces = Vec::new();
    for (address, length) in extents {
        if left == 0 {
            break;
        }
        let take = (*length).min(left);
        if take > 0 {
            pieces.push((*address, take));
            left -= take;
        }
    }
    pieces
}

/// Signalfd records rewritten by the child-exit ledger's answers.
#[derive(Debug, PartialEq, Eq)]
struct RewrittenSignalfdRecords {
    /// The buffer to write back: the kept records in order, then zeros over
    /// the bytes of dropped ones, so no dropped record stays readable.
    bytes: Vec<u8>,
    /// How many bytes the read returns.
    kept: usize,
}

/// Apply the ledger's `answers`, one per record of `bytes`
/// (`signalfd_record_controls`): a claimed duplicate or a duplicate scheduler
/// copy is dropped, a copy that stands for an owned child is given that
/// child's siginfo with zero CPU times, and every other record is kept as read.
/// `None` when no record changed.
fn apply_signalfd_answers(
    bytes: &[u8],
    answers: &[Option<crate::scheduler::child_exit_sigchld::ChildExitSigchldAnswer>],
) -> Option<RewrittenSignalfdRecords> {
    use crate::scheduler::child_exit_sigchld::ChildExitSigchldAnswer;
    use crate::scheduler::child_exit_sigchld::OwnCopy;
    let mut kept = Vec::with_capacity(bytes.len());
    let mut changed = false;
    for (record, answer) in bytes.chunks(SIGNALFD_RECORD).zip(answers) {
        let mut record = record.to_vec();
        match answer {
            Some(ChildExitSigchldAnswer::Claimed(true))
            | Some(ChildExitSigchldAnswer::OwnCopy(OwnCopy::Duplicate)) => {
                changed = true;
                continue;
            }
            Some(ChildExitSigchldAnswer::OwnCopy(OwnCopy::Deliver(own)))
            | Some(ChildExitSigchldAnswer::ClaimedFor(own)) => {
                record[8..12].copy_from_slice(&own.code.to_ne_bytes());
                record[12..16].copy_from_slice(&(own.pid as u32).to_ne_bytes());
                record[16..20].copy_from_slice(&own.uid.to_ne_bytes());
                record[40..44].copy_from_slice(&own.status.to_ne_bytes());
                record[56..72].fill(0);
                changed = true;
            }
            _ => {}
        }
        kept.extend_from_slice(&record);
    }
    if !changed {
        return None;
    }
    let kept_len = kept.len();
    kept.resize(bytes.len() - bytes.len() % SIGNALFD_RECORD, 0);
    Some(RewrittenSignalfdRecords {
        bytes: kept,
        kept: kept_len,
    })
}

#[cfg(test)]
mod signalfd_record_tests {
    use super::*;
    use crate::scheduler::child_exit_sigchld::ChildExitSigchldAnswer;
    use crate::scheduler::child_exit_sigchld::ChildExitSigchldControl;
    use crate::scheduler::child_exit_sigchld::ChildExitSiginfo;
    use crate::scheduler::child_exit_sigchld::OwnCopy;

    const TRACER: u32 = 4242;

    fn record(signo: i32, code: i32, pid: u32, status: i32) -> Vec<u8> {
        let mut record = vec![0u8; SIGNALFD_RECORD];
        record[0..4].copy_from_slice(&(signo as u32).to_ne_bytes());
        record[8..12].copy_from_slice(&code.to_ne_bytes());
        record[12..16].copy_from_slice(&pid.to_ne_bytes());
        record[40..44].copy_from_slice(&status.to_ne_bytes());
        record[56..72].fill(7);
        record
    }

    /// A kernel child-exit copy asks the ledger about its child, the
    /// scheduler's `SI_USER` copy asks for its own copy, and anything else,
    /// including a guest's `SI_USER` SIGCHLD, asks nothing.
    #[test]
    fn each_record_asks_the_ledger_only_about_a_child_exit_copy() {
        let bytes = [
            record(libc::SIGCHLD, libc::CLD_KILLED, 31, libc::SIGKILL),
            record(libc::SIGCHLD, libc::SI_USER, TRACER, 0),
            record(libc::SIGCHLD, libc::SI_USER, 77, 0),
            record(libc::SIGUSR1, libc::SI_USER, TRACER, 0),
        ]
        .concat();
        assert_eq!(
            signalfd_record_controls(&bytes, TRACER),
            vec![
                Some(ChildExitSigchldControl::ClaimKernelCopy {
                    child: DetPid::from_raw(31)
                }),
                Some(ChildExitSigchldControl::TakeOwnCopy),
                None,
                None,
            ]
        );
    }

    /// A dropped record leaves the read, the records after it move up, and
    /// zeros cover its old bytes; a standing copy gets its owned child's
    /// siginfo, a cohort's lowest pid included (`ClaimedFor`), with zero CPU
    /// times.
    #[test]
    fn dropped_records_leave_and_standing_ones_take_the_ledgers_siginfo() {
        let own = |pid| ChildExitSiginfo {
            code: libc::CLD_KILLED,
            pid,
            uid: 0,
            status: libc::SIGKILL,
        };
        let first = record(libc::SIGCHLD, libc::CLD_KILLED, 31, libc::SIGKILL);
        let second = record(libc::SIGCHLD, libc::SI_USER, TRACER, 0);
        let third = record(libc::SIGCHLD, libc::CLD_KILLED, 33, libc::SIGKILL);
        let bytes = [first, second, third].concat();
        let rewritten = apply_signalfd_answers(
            &bytes,
            &[
                Some(ChildExitSigchldAnswer::Claimed(true)),
                Some(ChildExitSigchldAnswer::OwnCopy(OwnCopy::Deliver(own(32)))),
                Some(ChildExitSigchldAnswer::ClaimedFor(own(30))),
            ],
        )
        .expect("records changed");
        assert_eq!(rewritten.kept, 2 * SIGNALFD_RECORD);
        assert_eq!(rewritten.bytes.len(), 3 * SIGNALFD_RECORD);
        let expect = |pid: u32| {
            let mut wanted = record(libc::SIGCHLD, libc::CLD_KILLED, pid, libc::SIGKILL);
            wanted[56..72].fill(0);
            wanted
        };
        assert_eq!(&rewritten.bytes[..SIGNALFD_RECORD], &expect(32)[..]);
        assert_eq!(
            &rewritten.bytes[SIGNALFD_RECORD..2 * SIGNALFD_RECORD],
            &expect(30)[..]
        );
        assert!(
            rewritten.bytes[2 * SIGNALFD_RECORD..]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    /// A vectored read's returned bytes are found across its buffers in
    /// order, a record split between two buffers included, and only as far
    /// as the read returned.
    #[test]
    fn a_vectored_reads_records_are_found_across_its_buffers() {
        let extents = [(0x1000, 100), (0x2000, 0), (0x3000, 200), (0x4000, 128)];
        assert_eq!(
            signalfd_extent_pieces(&extents, 256),
            vec![(0x1000, 100), (0x3000, 156)]
        );
        assert_eq!(
            signalfd_extent_pieces(&extents, 428),
            vec![(0x1000, 100), (0x3000, 200), (0x4000, 128)]
        );
        assert_eq!(signalfd_extent_pieces(&extents, 0), vec![]);
    }

    /// A read whose every record is dropped returns nothing and leaves only
    /// zeros; a read the ledger leaves alone is not rewritten.
    #[test]
    fn a_fully_dropped_read_leaves_no_record_and_an_untouched_read_is_not_rewritten() {
        let bytes = [
            record(libc::SIGCHLD, libc::CLD_KILLED, 31, libc::SIGKILL),
            record(libc::SIGCHLD, libc::SI_USER, TRACER, 0),
        ]
        .concat();
        let rewritten = apply_signalfd_answers(
            &bytes,
            &[
                Some(ChildExitSigchldAnswer::Claimed(true)),
                Some(ChildExitSigchldAnswer::OwnCopy(OwnCopy::Duplicate)),
            ],
        )
        .expect("records changed");
        assert_eq!(rewritten.kept, 0);
        assert!(rewritten.bytes.iter().all(|byte| *byte == 0));
        assert_eq!(
            apply_signalfd_answers(
                &bytes,
                &[
                    Some(ChildExitSigchldAnswer::Claimed(false)),
                    Some(ChildExitSigchldAnswer::OwnCopy(OwnCopy::Unowned)),
                ],
            ),
            None
        );
    }
}

/// The signals a thread may send to its own multi-threaded process with `kill`
/// or `rt_sigqueueinfo` (https://github.com/rrnewton/hermit/issues/4034): each
/// is numbered below SIGCHLD, so a child-exit SIGCHLD, whenever the kernel
/// posts it, is dequeued after it; none is in the kernel's synchronous set
/// (`SYNCHRONOUS_MASK`); and none is a signal Reverie gives a meaning
/// (SIGTRAP, its stop signal, and SIGSTKFLT, its timer marker).
const OWN_PROCESS_SIGNALS: [libc::c_int; 7] = [
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGPIPE,
    libc::SIGTERM,
];

/// Why a process-directed signal a thread sends to its own multi-threaded
/// process is refused, each a named refusal of the design for
/// https://github.com/rrnewton/hermit/issues/4034.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnProcessSignalRefusal {
    /// The signal is not one of `OWN_PROCESS_SIGNALS`.
    SignalNotAdmitted,
    /// The process leaves the signal at its default, fatal, disposition.
    DefaultDisposition,
    /// The run records or replays.
    RecordReplay,
    /// The run replays a recorded schedule (`--replay-schedule-from`).
    ScheduleReplay,
    /// A guest has sent SIGSTKFLT, which Reverie uses as its timer marker.
    GuestMarkerSignal,
    /// `--stacktrace-signal` is configured.
    StacktraceSignal,
    /// The sender blocks the signal.
    SenderBlocks,
    /// A thread of the process runs or sleeps outside the schedule.
    OutOfSchedule(DetTid),
    /// A thread of the process has an unknown mask.
    UnknownMask(DetTid),
    /// The sender is not the thread the call names, and a live thread does
    /// not block the signal.
    NotSoleReceiver(DetTid),
    /// The sender's private queue holds these signals, which it does not block.
    PrivatePending(u64),
    /// These host-timed sources, which the sender does not block, are armed.
    HostTimedSource(u64),
    /// Unreaped children have these exit signals, which the sender does not block.
    ForeignExitSignal(u64),
    /// The scheduler sent these signals, at or below the one sent, and no
    /// thread has taken them yet.
    UndeliveredSchedulerSignal(u64),
    /// A happens-before anchor is set on this call's completion.
    PosthookAnchor,
    /// The process holds a signalfd.
    Signalfd,
    /// An exec is in flight in the process.
    ExecInFlight,
}

impl std::fmt::Display for OwnProcessSignalRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignalNotAdmitted => write!(
                f,
                "only SIGHUP, SIGINT, SIGQUIT, SIGUSR1, SIGUSR2, SIGPIPE and SIGTERM are delivered \
                 deterministically to the sender"
            ),
            Self::DefaultDisposition => write!(
                f,
                "the signal has its default disposition, which ends the process"
            ),
            Self::RecordReplay => write!(f, "the run records or replays"),
            Self::ScheduleReplay => write!(
                f,
                "the run replays a recorded schedule (--replay-schedule-from), whose trace turn \
                 after the send can hand the turn to another thread"
            ),
            Self::GuestMarkerSignal => write!(
                f,
                "a guest sent SIGSTKFLT, which Reverie also uses as its timer marker, so a pending \
                 copy cannot be told from Reverie's"
            ),
            Self::StacktraceSignal => write!(f, "--stacktrace-signal is configured"),
            Self::SenderBlocks => write!(f, "the sending thread blocks the signal"),
            Self::OutOfSchedule(tid) => write!(
                f,
                "thread {tid} runs or sleeps outside the schedule, at moments host timing sets"
            ),
            Self::UnknownMask(tid) => write!(f, "thread {tid}'s signal mask is unknown"),
            Self::NotSoleReceiver(tid) => write!(
                f,
                "thread {tid} does not block the signal, so Linux could give it to either thread"
            ),
            Self::PrivatePending(set) => write!(
                f,
                "the sending thread has signals pending ({set:#x}) that it would take first"
            ),
            Self::HostTimedSource(set) => write!(
                f,
                "a source that sends signals {set:#x} at moments host timing sets is armed"
            ),
            Self::ForeignExitSignal(set) => write!(
                f,
                "a child with exit signal {set:#x} is unreaped, and the kernel sends that signal at \
                 a moment host timing sets"
            ),
            Self::UndeliveredSchedulerSignal(set) => write!(
                f,
                "the scheduler sent signals {set:#x} that no thread has taken yet"
            ),
            Self::PosthookAnchor => {
                write!(f, "a happens-before anchor is set on the call's completion")
            }
            Self::Signalfd => write!(f, "the process holds a signalfd"),
            Self::ExecInFlight => write!(f, "an exec is in flight in the process"),
        }
    }
}

/// Everything a process-directed signal `sig` that a thread sends to its own
/// multi-threaded process is decided from, all fixed by the schedule at the
/// sender's turn (`own_process_signal_refusal`).
#[derive(Clone, Debug, Default)]
pub(crate) struct OwnProcessSignal {
    pub(crate) sig: libc::c_int,
    /// Whether the call names the sender itself (S1 of the design).
    pub(crate) names_sender: bool,
    /// The sender's mask (`SigBlk`).
    pub(crate) blocked: u64,
    /// The sender's private queue (`SigPnd`).
    pub(crate) thread_pending: u64,
    /// The process's `SIG_IGN` dispositions (`SigIgn`).
    pub(crate) ignored: u64,
    /// The process's handlers (`SigCgt`).
    pub(crate) caught: u64,
    pub(crate) inputs: OwnProcessSignalInputs,
    pub(crate) record_replay: bool,
    pub(crate) schedule_replay: bool,
    pub(crate) stacktrace_signal: bool,
    pub(crate) posthook_anchor: bool,
    pub(crate) signalfd: bool,
}

/// The refusal for `send`, or `None` when the kernel's choice of receiver and
/// the sender's dequeue are forced: the sender takes the signal on its way back
/// to user mode, before any other thread of the process can run
/// (https://github.com/rrnewton/hermit/issues/4034).
pub(crate) fn own_process_signal_refusal(
    send: &OwnProcessSignal,
) -> Option<OwnProcessSignalRefusal> {
    use OwnProcessSignalRefusal as R;
    let bit = kernel_sigset_bit(send.sig);
    if !OWN_PROCESS_SIGNALS.contains(&send.sig) {
        return Some(R::SignalNotAdmitted);
    }
    if (send.ignored | send.caught) & bit == 0 {
        return Some(R::DefaultDisposition);
    }
    if send.record_replay {
        return Some(R::RecordReplay);
    }
    if send.schedule_replay {
        return Some(R::ScheduleReplay);
    }
    if send.stacktrace_signal {
        return Some(R::StacktraceSignal);
    }
    if send.blocked & bit != 0 {
        return Some(R::SenderBlocks);
    }
    for (tid, state) in &send.inputs.siblings {
        if *state == SiblingSignalState::OutOfSchedule {
            return Some(R::OutOfSchedule(*tid));
        }
    }
    for (tid, state) in &send.inputs.siblings {
        if *state == SiblingSignalState::InSchedule(None) {
            return Some(R::UnknownMask(*tid));
        }
    }
    if !send.names_sender {
        for (tid, state) in &send.inputs.siblings {
            if matches!(state, SiblingSignalState::InSchedule(Some(mask)) if mask & bit == 0) {
                return Some(R::NotSoleReceiver(*tid));
            }
        }
    }
    let unblocked = !send.blocked;
    let private = without_perf_event_signal(send.thread_pending) & unblocked;
    if private != 0 {
        return Some(R::PrivatePending(private));
    }
    let host_timed = send.inputs.host_timed & unblocked;
    if host_timed != 0 {
        return Some(R::HostTimedSource(host_timed));
    }
    let foreign = send.inputs.foreign_exit_signals & unblocked;
    if foreign != 0 {
        return Some(R::ForeignExitSignal(foreign));
    }
    // A signal numbered at or below `sig` is dequeued first or coalesces.
    let at_or_below = bit | (bit - 1);
    let undelivered = send.inputs.undelivered_scheduler_signals & unblocked & at_or_below;
    if undelivered != 0 {
        return Some(R::UndeliveredSchedulerSignal(undelivered));
    }
    if send.posthook_anchor {
        return Some(R::PosthookAnchor);
    }
    if send.signalfd {
        return Some(R::Signalfd);
    }
    if send.inputs.exec_in_flight {
        return Some(R::ExecInFlight);
    }
    if send.inputs.guest_marker_signal {
        return Some(R::GuestMarkerSignal);
    }
    None
}

/// How a process-directed signal the calling thread sends goes on
/// (`Detcore::own_process_signal`).
enum OwnProcessSend {
    /// The call does not name the sender's own multi-threaded process: the
    /// ordinary path decides.
    NotOwn,
    /// Admitted: forward the call unchanged, holding the sender's turn.
    Admitted,
    /// Refused by name, with the policy's answer.
    Refused(Result<i64, Error>),
}

/// What `Detcore::rt_sigqueueinfo_precheck` found.
enum RtSigqueueinfoPrecheck {
    /// The kernel fails the call without sending (EPERM, or E2BIG, after a
    /// complete copy): forward it unchanged.
    KernelFails,
    /// The kernel's copy certainly faults: Hermit answers EFAULT itself.
    Efault,
    /// The call goes on to admission.
    Proceeds,
    /// Hermit cannot read the whole `siginfo_t`, and cannot prove that the
    /// kernel's copy faults: `[vvar]`, which neither tracer read reaches; a
    /// tail running into a gap; a first 48 bytes in the gap below a grow-down
    /// mapping, which the kernel's copy may grow into; or an unreadable
    /// `/proc/<tid>/smaps`.
    Unreadable,
}

/// One mapping of a `/proc/<pid>/smaps` listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mapping {
    start: usize,
    end: usize,
    /// Whether user mode can read it: on x86 any of read, write or execute
    /// permission allows a read; none (`---`) faults.
    user_readable: bool,
    /// `VmFlags` `gd`: a fault just below it can grow it downwards.
    growsdown: bool,
}

/// The mappings of a `/proc/<pid>/smaps` listing, in address order.
fn parse_smaps(smaps: &str) -> Vec<Mapping> {
    let mut mappings: Vec<Mapping> = Vec::new();
    for line in smaps.lines() {
        if let Some(flags) = line.strip_prefix("VmFlags:") {
            if let Some(last) = mappings.last_mut() {
                last.growsdown = flags.split_whitespace().any(|flag| flag == "gd");
            }
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some((start, end)) = fields.next().and_then(|range| range.split_once('-')) else {
            continue;
        };
        let (Ok(start), Ok(end), Some(perms)) = (
            usize::from_str_radix(start, 16),
            usize::from_str_radix(end, 16),
            fields.next(),
        ) else {
            continue;
        };
        if perms.len() < 4 {
            continue;
        }
        mappings.push(Mapping {
            start,
            end,
            user_readable: perms.chars().take(3).any(|perm| perm != '-'),
            growsdown: false,
        });
    }
    mappings.sort_by_key(|mapping| mapping.start);
    mappings
}

/// Why a byte is not readable by user mode (`first_user_unreadable`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unreadable {
    /// In a mapping without read, write or execute permission.
    NoPermission,
    /// In no mapping; `below_growsdown` when the next mapping above it can
    /// grow down into it.
    Hole { below_growsdown: bool },
}

/// Where the first byte of `[address, address + len)` that user mode cannot
/// read lies, or `None` when user mode can read them all. A range that
/// overflows the address space counts as a hole.
fn first_user_unreadable(mappings: &[Mapping], address: usize, len: usize) -> Option<Unreadable> {
    let Some(end) = address.checked_add(len) else {
        return Some(Unreadable::Hole {
            below_growsdown: false,
        });
    };
    let mut next = address;
    while next < end {
        match mappings.iter().find(|mapping| mapping.end > next) {
            Some(mapping) if mapping.start <= next => {
                if !mapping.user_readable {
                    return Some(Unreadable::NoPermission);
                }
                next = mapping.end;
            }
            above => {
                return Some(Unreadable::Hole {
                    below_growsdown: above.is_some_and(|mapping| mapping.growsdown),
                });
            }
        }
    }
    None
}

/// The signals a send of `sig` lets Linux send later at a moment set by host
/// timing, as a kernel sigset; 0 for every signal but the four that stop by
/// default. A stopped member makes its process group one that an exit can
/// orphan with a stopped job, and Linux then sends that whole group SIGHUP and
/// SIGCONT at the exiting process's physical exit (`kill_orphaned_pgrp` in
/// kernel/exit.c). Apart from a guest's send, only a terminal stops a guest
/// process, and a terminal's signals, these two among them, are recorded with
/// `terminal_signals`.
fn orphaned_group_signals(sig: libc::c_int) -> u64 {
    match sig {
        libc::SIGSTOP | libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU => {
            kernel_sigset_bit(libc::SIGHUP) | kernel_sigset_bit(libc::SIGCONT)
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeval(seconds: libc::time_t, micros: libc::suseconds_t) -> libc::timeval {
        libc::timeval {
            tv_sec: seconds,
            tv_usec: micros,
        }
    }

    /// A send of each of the four default-stop signals arms the orphaned
    /// process group's SIGHUP and SIGCONT; no other signal arms anything.
    #[test]
    fn only_a_default_stop_signal_arms_the_orphaned_group_signals() {
        let hup_cont = kernel_sigset_bit(libc::SIGHUP) | kernel_sigset_bit(libc::SIGCONT);
        for sig in 0..=65 {
            let stops = matches!(
                sig,
                libc::SIGSTOP | libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU
            );
            assert_eq!(
                orphaned_group_signals(sig),
                if stops { hup_cont } else { 0 },
                "signal {sig}"
            );
        }
    }

    #[test]
    fn raw_kernel_signal_masks_are_exactly_one_u64() {
        assert_eq!(KERNEL_SIGSET_SIZE, 8);
        assert_eq!(std::mem::size_of::<KernelSigset>(), 8);
        assert!(std::mem::size_of::<libc::sigset_t>() > KERNEL_SIGSET_SIZE);
    }

    /// The mask `rt_sigsuspend` (and the scheduler's record of it) sleeps
    /// under can never block SIGKILL or SIGSTOP; every other bit is kept.
    #[test]
    fn the_installed_mask_cannot_block_kill_or_stop() {
        let kill = 1_u64 << (libc::SIGKILL - 1);
        let stop = 1_u64 << (libc::SIGSTOP - 1);
        assert_eq!(kernel_installed_signal_mask(u64::MAX), !(kill | stop));
        assert_eq!(kernel_installed_signal_mask(kill | stop), 0);
        let alrm_and_rt64 = (1_u64 << (libc::SIGALRM - 1)) | (1_u64 << 63);
        assert_eq!(kernel_installed_signal_mask(alrm_and_rt64), alrm_and_rt64);
    }

    #[test]
    fn raw_signal_size_validation_rejects_before_pointer_processing() {
        assert_eq!(validate_kernel_sigset_size(7), Err(Errno::EINVAL));
        assert_eq!(validate_kernel_sigset_size(8), Ok(()));
        assert_eq!(validate_kernel_sigset_size(16), Err(Errno::EINVAL));
    }

    #[test]
    fn reserved_signal_is_removed_from_the_kernel_sized_mask_only() {
        let reserved = 1_u64 << (reverie::PERF_EVENT_SIGNAL as u32 - 1);
        let usr1 = 1_u64 << (libc::SIGUSR1 as u32 - 1);
        assert_eq!(without_perf_event_signal(reserved | usr1), usr1);
        assert_eq!(without_perf_event_signal(usr1), usr1);
    }

    #[test]
    fn timeval_conversion_preserves_subsecond_precision() {
        let logical_time =
            timeval_to_logical_time(timeval(2, 345_678)).expect("valid timeval should convert");
        assert_eq!(
            logical_time,
            LogicalTime::from_nanos(2_345_678_000),
            "timeval conversion should preserve microsecond precision"
        );

        let round_trip = logical_time_to_timeval(logical_time);
        assert_eq!(round_trip.tv_sec, 2, "round trip should preserve seconds");
        assert_eq!(
            round_trip.tv_usec, 345_678,
            "round trip should preserve microseconds"
        );
    }

    #[test]
    fn timeval_conversion_rejects_invalid_values() {
        for invalid in [
            timeval(-1, 0),
            timeval(0, -1),
            timeval(0, 1_000_000),
            timeval(libc::time_t::MAX, 0),
        ] {
            assert_eq!(
                timeval_to_logical_time(invalid),
                Err(Errno::EINVAL),
                "invalid timeval should return EINVAL"
            );
        }
    }

    #[test]
    fn alarm_remaining_seconds_round_up() {
        assert_eq!(logical_time_to_alarm_seconds(LogicalTime::ZERO), 0);
        assert_eq!(logical_time_to_alarm_seconds(LogicalTime::from_nanos(1)), 1);
        assert_eq!(
            logical_time_to_alarm_seconds(LogicalTime::from_nanos(999_999_999)),
            1
        );
        assert_eq!(
            logical_time_to_alarm_seconds(LogicalTime::from_nanos(1_000_000_000)),
            1
        );
        assert_eq!(
            logical_time_to_alarm_seconds(LogicalTime::from_nanos(1_000_000_001)),
            2
        );
    }

    #[test]
    fn kill_targets_only_unambiguous_process_delivery() {
        let first = DetTid::from_raw(42);
        let second = DetTid::from_raw(43);
        assert_eq!(
            deterministic_kill_target(&[], None, libc::SIGUSR1),
            Err(Errno::ESRCH)
        );
        assert_eq!(
            deterministic_kill_target(&[], Some(true), libc::SIGUSR1),
            Ok(None)
        );
        assert_eq!(
            deterministic_kill_target(&[], Some(false), libc::SIGUSR1),
            Err(Errno::ESRCH)
        );
        assert_eq!(
            deterministic_kill_target(&[first], None, libc::SIGUSR1),
            Ok(Some(first))
        );
        assert_eq!(
            deterministic_kill_target(&[first, second], None, libc::SIGUSR1),
            Err(Errno::ENOSYS)
        );
        assert_eq!(
            deterministic_kill_target(&[first, second], None, 0),
            Ok(Some(first))
        );

        let process = DetPid::from_raw(41);
        assert!(self_sigkill_targets_current_task(
            libc::SIGKILL,
            Some(process),
            None,
            process,
            first,
        ));
        assert!(self_sigkill_targets_current_task(
            libc::SIGKILL,
            None,
            Some(first),
            process,
            first,
        ));
        assert!(self_sigkill_targets_current_task(
            libc::SIGKILL,
            Some(process),
            Some(first),
            process,
            first,
        ));
        for (signal, target_process, target_thread) in [
            (libc::SIGTERM, Some(process), Some(first)),
            (libc::SIGKILL, Some(DetPid::from_raw(40)), Some(first)),
            (libc::SIGKILL, Some(process), Some(second)),
            (libc::SIGKILL, None, None),
        ] {
            assert!(!self_sigkill_targets_current_task(
                signal,
                target_process,
                target_thread,
                process,
                first,
            ));
        }
    }

    #[test]
    fn process_group_forwarding_is_limited_to_unmaskable_sigkill() {
        assert!(can_forward_process_group_signal(-42, libc::SIGKILL, false));
        assert!(!can_forward_process_group_signal(-42, libc::SIGTERM, false));
        assert!(!can_forward_process_group_signal(0, libc::SIGKILL, false));
        assert!(!can_forward_process_group_signal(-1, libc::SIGKILL, false));
        assert!(!can_forward_process_group_signal(-42, libc::SIGKILL, true));
    }

    #[test]
    fn signal_zero_never_notifies_a_target() {
        let sender = DetTid::from_raw(42);
        let target = DetTid::from_raw(43);
        assert!(!should_notify_cross_task_signal(sender, target, 0));
        assert!(should_notify_cross_task_signal(
            sender,
            target,
            libc::SIGUSR1
        ));
        assert!(!should_notify_cross_task_signal(
            sender,
            sender,
            libc::SIGUSR1
        ));
    }
}

#[cfg(test)]
mod appropriated_signal_tests {
    use super::*;

    /// The set is closed, and it is closed BY MEASUREMENT: every signal 1..64 was
    /// run under two delivery paths against a native control, and exactly these
    /// two differ. If a third is ever appropriated it must be added here, because
    /// the diagnostic is the only thing that makes the loss visible.
    #[test]
    fn the_appropriated_set_is_exactly_sigtrap_and_sigstkflt() {
        let signums: Vec<i32> = APPROPRIATED_SIGNALS.iter().map(|(s, _)| *s).collect();
        assert_eq!(signums, vec![libc::SIGTRAP, libc::SIGSTKFLT]);
        // SIGSTKFLT is appropriated because reverie uses it as the PMU timer, so
        // the two must not drift apart.
        assert_eq!(libc::SIGSTKFLT, reverie::PERF_EVENT_SIGNAL as i32);
    }

    /// ⚠️ SIG_DFL and SIG_IGN LOSE NOTHING. A guest that is not asking to be
    /// called back has no expectation to disappoint, and warning there would make
    /// the marker noise instead of signal -- Go registers SIGSTKFLT routinely.
    #[test]
    fn only_a_real_handler_is_reported() {
        for signum in [libc::SIGTRAP, libc::SIGSTKFLT] {
            assert!(!reports(signum, 0), "SIG_DFL must not warn");
            assert!(!reports(signum, 1), "SIG_IGN must not warn");
            assert!(reports(signum, 0x4000_1234), "a real handler must warn");
        }
    }

    /// An ordinary signal is delivered normally and must never be reported.
    #[test]
    fn an_unappropriated_signal_is_never_reported() {
        for signum in [libc::SIGUSR1, libc::SIGTERM, libc::SIGINT, 10, 30] {
            assert!(
                !reports(signum, 0x4000_1234),
                "signal {signum} is not appropriated"
            );
        }
    }

    /// Calls the REAL predicate. Deliberately not a re-implementation: a
    /// mirrored copy would keep passing if `appropriated_reason` were gutted.
    fn reports(signum: i32, handler: u64) -> bool {
        appropriated_reason(signum, handler).is_some()
    }
}

#[cfg(test)]
mod rt_sigsuspend_tests {
    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Pid;
    use reverie::Tool;
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::Syscall;
    use reverie::syscalls::SyscallInfo;

    use super::*;
    use crate::Config;
    use crate::GlobalState;
    use crate::ThreadState;
    use crate::syscalls::threads::kernel_sigset_bit;

    /// The guest's stack, in this process's memory: `regs` reports a stack
    /// pointer `STACK_WORD` words in, so the cells below its red zone are words
    /// `COPY_WORD - 1` (the pending set) and `COPY_WORD` (the copy).
    const ARENA_WORDS: usize = 64;
    const STACK_WORD: usize = 48;
    const COPY_WORD: usize = STACK_WORD - STACK_RED_ZONE / 8 - 1;

    /// rt_sigsuspend writes its cells directly; it takes no scratch stack.
    struct NoStack;

    struct NoStackGuard;

    impl Drop for NoStackGuard {
        fn drop(&mut self) {}
    }

    impl reverie::Stack for NoStack {
        type StackGuard = NoStackGuard;

        fn size(&self) -> usize {
            panic!("rt_sigsuspend takes no scratch stack")
        }
        fn capacity(&self) -> usize {
            panic!("rt_sigsuspend takes no scratch stack")
        }
        fn push<'stack, T>(&mut self, _: T) -> Addr<'stack, T> {
            panic!("rt_sigsuspend takes no scratch stack")
        }
        fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
            panic!("rt_sigsuspend takes no scratch stack")
        }
        fn commit(self) -> Result<Self::StackGuard, Errno> {
            panic!("rt_sigsuspend takes no scratch stack")
        }
    }

    /// A guest whose `rt_sigsuspend` caller's mask lives at `caller_mask`. The
    /// call's `rt_sigpending` probe, which runs after Detcore read that mask
    /// and before the real call, rewrites it to `rewritten` if one is given,
    /// as another process sharing the buffer could while the call waits for
    /// its turn. Every mask a real `rt_sigsuspend` is given is kept in
    /// `suspended_under`.
    struct SuspendGuest {
        config: Config,
        thread: ThreadState<()>,
        arena: Box<[u64; ARENA_WORDS]>,
        caller_mask: usize,
        rewritten: Option<KernelSigset>,
        suspended_under: Vec<KernelSigset>,
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for SuspendGuest {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            panic!(
                "an unsequentialized rt_sigsuspend sends no RPC: {:?}",
                message.2
            )
        }
        fn config(&self) -> &Config {
            &self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for SuspendGuest {
        type Memory = LocalMemory;
        type Stack = NoStack;

        fn tid(&self) -> Pid {
            Pid::from_raw(1)
        }
        fn pid(&self) -> Pid {
            Pid::from_raw(1)
        }
        fn ppid(&self) -> Option<Pid> {
            None
        }
        fn memory(&self) -> Self::Memory {
            LocalMemory::new()
        }
        fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
            &mut self.thread
        }
        fn thread_state(&self) -> &ThreadState<()> {
            &self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            // SAFETY: user_regs_struct is plain integers; all zeroes is valid.
            let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
            regs.rsp = (self.arena.as_ptr() as usize + STACK_WORD * 8) as u64;
            regs
        }
        async fn stack(&mut self) -> Self::Stack {
            NoStack
        }
        async fn daemonize(&mut self) {
            panic!("rt_sigsuspend must not daemonize")
        }
        async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
            let (number, args) = syscall.into_parts();
            match Syscall::from_raw(number, args) {
                // read_kernel_sigset's validation probe.
                Syscall::RtSigprocmask(call) if call.how() == -1 => Err(Errno::EINVAL),
                Syscall::RtSigpending(call) => {
                    let set = call.set().expect("rt_sigpending without a set");
                    LocalMemory::new().write_value(set.cast::<KernelSigset>(), &0)?;
                    if let Some(rewritten) = self.rewritten {
                        // SAFETY: `caller_mask` is the test's live, aligned buffer.
                        unsafe {
                            std::ptr::write(self.caller_mask as *mut KernelSigset, rewritten)
                        };
                    }
                    Ok(0)
                }
                Syscall::RtSigsuspend(call) => {
                    let mask = call.mask().expect("rt_sigsuspend without a mask");
                    self.suspended_under
                        .push(LocalMemory::new().read_value(mask.cast::<KernelSigset>())?);
                    Err(Errno::EINTR)
                }
                other => panic!("rt_sigsuspend injected {other:?}"),
            }
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
            panic!("rt_sigsuspend must not retire the guest")
        }
        fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("rt_sigsuspend must not set a timer")
        }
        fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("rt_sigsuspend must not set a timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("rt_sigsuspend must not read a clock")
        }
    }

    /// The real `rt_sigsuspend` sleeps under the mask Detcore read when it
    /// handled the call, the one the scheduler is given, even if the guest's
    /// buffer changes before the call runs. Linux copies the mask when the call
    /// starts, and from the guest's view it has started. Before, the call ran
    /// with the guest's pointer: another process sharing the buffer could make
    /// it block a signal the scheduler had counted on to end it, and the
    /// scheduler would hold every other thread waiting for that wake.
    #[tokio::test(flavor = "current_thread")]
    async fn rt_sigsuspend_sleeps_under_the_mask_detcore_read() {
        let config = Config::default();
        assert!(!config.sequentialize_threads);
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
        let unblocks_sigchld: KernelSigset = kernel_sigset_bit(libc::SIGUSR2);
        let blocks_sigchld = unblocks_sigchld | kernel_sigset_bit(libc::SIGCHLD);
        let mut caller_mask: Box<KernelSigset> = Box::new(unblocks_sigchld);
        let caller_address = &mut *caller_mask as *mut KernelSigset as usize;
        let mut guest = SuspendGuest {
            thread: ThreadState::new(DetPid::from_raw(1), &config, ()),
            config: config.clone(),
            arena: Box::new([u64::MAX; ARENA_WORDS]),
            caller_mask: caller_address,
            rewritten: Some(blocks_sigchld),
            suspended_under: Vec::new(),
        };
        let call = syscalls::RtSigsuspend::new()
            .with_mask(Addr::from_raw(caller_address))
            .with_sigsetsize(KERNEL_SIGSET_SIZE);

        let result = tool.handle_rt_sigsuspend(&mut guest, call).await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::EINTR))),
            "{result:?}"
        );
        assert_eq!(*caller_mask, blocks_sigchld, "the buffer was rewritten");
        assert_eq!(guest.suspended_under, [unblocks_sigchld]);
    }

    /// A caller's mask that lies on the cells Detcore places below the red
    /// zone, as a raw syscall may pass one there in a shared stack: the copy
    /// must not be the caller's buffer, or a rewrite of the shared buffer
    /// reaches the real call again, and the rt_sigpending cell must not
    /// overwrite it. The cells move below the buffer, which keeps its own
    /// contents. Before, the copy slot was the caller's buffer, and the
    /// pending slot's buffer was overwritten with the pending set.
    #[tokio::test(flavor = "current_thread")]
    async fn rt_sigsuspend_scratch_never_lands_on_the_callers_mask() {
        let config = Config::default();
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
        let original: KernelSigset = kernel_sigset_bit(libc::SIGUSR2);
        let rewritten = original | kernel_sigset_bit(libc::SIGCHLD);
        // The copy's slot, rewritten by a peer; then the pending cell's slot,
        // which nobody rewrites.
        for (slot, peer_rewrite) in [(COPY_WORD, Some(rewritten)), (COPY_WORD - 1, None)] {
            let mut arena = Box::new([u64::MAX; ARENA_WORDS]);
            arena[slot] = original;
            let caller_address = &mut arena[slot] as *mut u64 as usize;
            let mut guest = SuspendGuest {
                thread: ThreadState::new(DetPid::from_raw(1), &config, ()),
                config: config.clone(),
                arena,
                caller_mask: caller_address,
                rewritten: peer_rewrite,
                suspended_under: Vec::new(),
            };
            let call = syscalls::RtSigsuspend::new()
                .with_mask(Addr::from_raw(caller_address))
                .with_sigsetsize(KERNEL_SIGSET_SIZE);

            let result = tool.handle_rt_sigsuspend(&mut guest, call).await;
            assert!(
                matches!(result, Err(Error::Errno(Errno::EINTR))),
                "slot {slot}: {result:?}"
            );
            assert_eq!(guest.suspended_under, [original], "slot {slot}");
            assert_eq!(
                guest.arena[slot],
                peer_rewrite.unwrap_or(original),
                "slot {slot}: the caller's buffer holds only what the guest wrote"
            );
        }
    }
}

#[cfg(test)]
mod own_process_signal_tests {
    use super::*;

    const SENDER_SIBLING: i32 = 11;

    /// The main thread sends SIGUSR1, which it catches, to its own process
    /// while one other thread is parked with an empty mask: admitted (S1).
    fn admitted_s1() -> OwnProcessSignal {
        OwnProcessSignal {
            sig: libc::SIGUSR1,
            names_sender: true,
            caught: kernel_sigset_bit(libc::SIGUSR1),
            inputs: OwnProcessSignalInputs {
                siblings: vec![(
                    DetTid::from_raw(SENDER_SIBLING),
                    SiblingSignalState::InSchedule(Some(0)),
                )],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn refusal(edit: impl FnOnce(&mut OwnProcessSignal)) -> Option<OwnProcessSignalRefusal> {
        let mut send = admitted_s1();
        edit(&mut send);
        own_process_signal_refusal(&send)
    }

    /// The first byte user mode cannot read is found across contiguous
    /// readable, write-only and execute-only mappings, and is a `PROT_NONE`
    /// mapping, a hole, or a hole below a grow-down mapping.
    #[test]
    fn the_first_user_unreadable_byte_is_classified_by_its_mapping() {
        let smaps = "1000-2000 r--p 0 0:0 0\nVmFlags: rd mr\n\
                     2000-3000 -w-p 0 0:0 0\nVmFlags: wr mw\n\
                     3000-4000 ---p 0 0:0 0\nVmFlags: mr\n\
                     6000-7000 --xp 0 0:0 0 [vvar]\nVmFlags: ex\n\
                     9000-a000 rw-p 0 0:0 0 [stack]\nVmFlags: rd wr gd\n";
        let mappings = parse_smaps(smaps);
        assert_eq!(mappings.len(), 5);
        assert!(mappings[4].growsdown && !mappings[0].growsdown);
        assert_eq!(
            first_user_unreadable(&mappings, 0x1fc0, 128),
            None,
            "crosses from read-only into write-only"
        );
        assert_eq!(
            first_user_unreadable(&mappings, 0x6000, 128),
            None,
            "execute-only"
        );
        assert_eq!(
            first_user_unreadable(&mappings, 0x2fc0, 128),
            Some(Unreadable::NoPermission),
            "runs into PROT_NONE"
        );
        assert_eq!(
            first_user_unreadable(&mappings, 0x4000, 48),
            Some(Unreadable::Hole {
                below_growsdown: false
            }),
            "a hole below a fixed mapping"
        );
        assert_eq!(
            first_user_unreadable(&mappings, 0x8000, 48),
            Some(Unreadable::Hole {
                below_growsdown: true
            }),
            "a hole below the stack"
        );
        assert_eq!(
            first_user_unreadable(&mappings, 0xb000, 48),
            Some(Unreadable::Hole {
                below_growsdown: false
            }),
            "above every mapping"
        );
        assert_eq!(
            first_user_unreadable(&mappings, 0, 48),
            Some(Unreadable::Hole {
                below_growsdown: false
            })
        );
        assert_eq!(
            first_user_unreadable(&mappings, usize::MAX - 10, 48),
            Some(Unreadable::Hole {
                below_growsdown: false
            })
        );
    }

    #[test]
    fn the_sender_named_by_its_call_is_admitted_whatever_its_siblings_masks() {
        assert_eq!(refusal(|_| {}), None);
    }

    /// S2: the call names the leader but another thread sends; admitted only
    /// when every other live thread blocks the signal or has its exit granted.
    #[test]
    fn another_sender_is_admitted_only_when_every_live_sibling_blocks() {
        let usr1 = kernel_sigset_bit(libc::SIGUSR1);
        let leader = DetTid::from_raw(SENDER_SIBLING);
        assert_eq!(
            refusal(|send| send.names_sender = false),
            Some(OwnProcessSignalRefusal::NotSoleReceiver(leader))
        );
        assert_eq!(
            refusal(|send| {
                send.names_sender = false;
                send.inputs.siblings = vec![(leader, SiblingSignalState::InSchedule(Some(usr1)))];
            }),
            None
        );
        assert_eq!(
            refusal(|send| {
                send.names_sender = false;
                send.inputs.siblings = vec![(leader, SiblingSignalState::ExitGranted)];
            }),
            None
        );
    }

    /// Only the seven admitted signals pass; each other signal is refused
    /// whatever its disposition.
    #[test]
    fn only_the_seven_signals_below_sigchld_are_admitted() {
        for sig in 1..=64 {
            let admitted = OWN_PROCESS_SIGNALS.contains(&sig);
            let got = refusal(|send| {
                send.sig = sig;
                send.caught = kernel_sigset_bit(sig);
            });
            if admitted {
                assert!(sig < libc::SIGCHLD, "signal {sig}");
                assert_eq!(got, None, "signal {sig}");
            } else {
                assert_eq!(
                    got,
                    Some(OwnProcessSignalRefusal::SignalNotAdmitted),
                    "signal {sig}"
                );
            }
        }
        for sig in [
            libc::SIGCHLD,
            libc::SIGCONT,
            libc::SIGTSTP,
            libc::SIGRTMIN(),
            libc::SIGSEGV,
            libc::SIGTRAP,
            libc::SIGSTKFLT,
            libc::SIGKILL,
            libc::SIGALRM,
        ] {
            assert!(!OWN_PROCESS_SIGNALS.contains(&sig), "signal {sig}");
        }
    }

    #[test]
    fn a_default_disposition_is_refused_and_sig_ign_is_admitted() {
        assert_eq!(
            refusal(|send| send.caught = 0),
            Some(OwnProcessSignalRefusal::DefaultDisposition)
        );
        assert_eq!(
            refusal(|send| {
                send.caught = 0;
                send.ignored = kernel_sigset_bit(libc::SIGUSR1);
            }),
            None
        );
    }

    #[test]
    fn each_mode_and_sender_state_refuses_by_name() {
        use OwnProcessSignalRefusal as R;
        let sibling = DetTid::from_raw(SENDER_SIBLING);
        assert_eq!(
            refusal(|send| send.record_replay = true),
            Some(R::RecordReplay)
        );
        assert_eq!(
            refusal(|send| send.schedule_replay = true),
            Some(R::ScheduleReplay)
        );
        assert_eq!(
            refusal(|send| send.inputs.guest_marker_signal = true),
            Some(R::GuestMarkerSignal)
        );
        assert_eq!(
            refusal(|send| send.stacktrace_signal = true),
            Some(R::StacktraceSignal)
        );
        assert_eq!(
            refusal(|send| send.blocked = kernel_sigset_bit(libc::SIGUSR1)),
            Some(R::SenderBlocks)
        );
        assert_eq!(
            refusal(|send| send.inputs.siblings[0].1 = SiblingSignalState::OutOfSchedule),
            Some(R::OutOfSchedule(sibling))
        );
        assert_eq!(
            refusal(|send| send.inputs.siblings[0].1 = SiblingSignalState::InSchedule(None)),
            Some(R::UnknownMask(sibling))
        );
        assert_eq!(
            refusal(|send| send.posthook_anchor = true),
            Some(R::PosthookAnchor)
        );
        assert_eq!(refusal(|send| send.signalfd = true), Some(R::Signalfd));
        assert_eq!(
            refusal(|send| send.inputs.exec_in_flight = true),
            Some(R::ExecInFlight)
        );
    }

    /// The private queue refuses on any signal the sender does not block,
    /// except Reverie's timer marker.
    #[test]
    fn a_private_pending_signal_refuses_unless_blocked_or_the_timer_marker() {
        let hup = kernel_sigset_bit(libc::SIGHUP);
        let marker = kernel_sigset_bit(reverie::PERF_EVENT_SIGNAL as i32);
        assert_eq!(
            refusal(|send| send.thread_pending = hup),
            Some(OwnProcessSignalRefusal::PrivatePending(hup))
        );
        assert_eq!(
            refusal(|send| {
                send.thread_pending = hup;
                send.blocked = hup;
            }),
            None
        );
        assert_eq!(refusal(|send| send.thread_pending = marker), None);
    }

    /// An armed host-timed source or a foreign exit signal refuses whatever its
    /// number, unless the sender blocks it.
    #[test]
    fn armed_sources_refuse_unless_the_sender_blocks_them() {
        let io = kernel_sigset_bit(libc::SIGIO);
        let usr2 = kernel_sigset_bit(libc::SIGUSR2);
        assert_eq!(
            refusal(|send| send.inputs.host_timed = io),
            Some(OwnProcessSignalRefusal::HostTimedSource(io))
        );
        assert_eq!(
            refusal(|send| {
                send.inputs.host_timed = io;
                send.blocked = io;
            }),
            None
        );
        assert_eq!(
            refusal(|send| send.inputs.foreign_exit_signals = usr2),
            Some(OwnProcessSignalRefusal::ForeignExitSignal(usr2))
        );
        assert_eq!(
            refusal(|send| {
                send.inputs.foreign_exit_signals = usr2;
                send.blocked = usr2;
            }),
            None
        );
    }

    /// An undelivered scheduler signal refuses when numbered at or below the
    /// one sent, which it would precede or coalesce with, and not above it.
    #[test]
    fn an_undelivered_scheduler_signal_refuses_only_at_or_below_the_sent_one() {
        let term = libc::SIGTERM;
        for (pending, refuses) in [
            (libc::SIGHUP, true),
            (libc::SIGALRM, true),
            (libc::SIGTERM, true),
            (libc::SIGSTKFLT, false),
            (libc::SIGRTMIN(), false),
        ] {
            let bit = kernel_sigset_bit(pending);
            let got = refusal(|send| {
                send.sig = term;
                send.caught = kernel_sigset_bit(term);
                send.inputs.undelivered_scheduler_signals = bit;
            });
            assert_eq!(
                got,
                refuses.then_some(OwnProcessSignalRefusal::UndeliveredSchedulerSignal(bit)),
                "pending {pending}"
            );
        }
    }
}
