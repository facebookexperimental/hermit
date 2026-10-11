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
use crate::tool_global::host_timed_signals;
use crate::tool_global::notify_signal_pending;
use crate::tool_global::refuse_sigalrm;
use crate::tool_global::register_alarm;
use crate::tool_global::registered_process_of;
use crate::tool_global::resolve_kill_targets_and_exit;
use crate::tool_global::resource_request;
use crate::tool_global::set_sigchld_disposition;
use crate::tool_global::sigalrm_refuses;
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
/// `sigaction` natively too and are not hermit's. Realtime 34..64 fail by a
/// different mechanism (they cannot be represented at the reverie ptrace
/// boundary) and are tracked separately -- they are not appropriation.
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
        retry_nonblocking_syscall_with_timeout(guest, call, rsrc, maybe_timeout).await
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
    async fn reserve_controlled_self_sigkill_exit<G: Guest<Self>>(
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
        // A process-group or broadcast SIGALRM is refused by name below.
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
            crate::tool_global::notify_unresolved_sigkill(guest, Some(DetPid::from_raw(-tgid)))
                .await;
            return Ok(self.record_or_replay(guest, call).await?);
        }
        if tgid <= 0 {
            return self.refuse_group_signal(guest, tgid, call.sig()).await;
        }
        // Exact self-SIGKILL is group-fatal, so it has no recipient-selection
        // ambiguity even when the process has several live threads. Reserve it
        // before the generic process-signal path rejects that thread set.
        if self
            .reserve_controlled_self_sigkill_exit(
                guest,
                call.sig(),
                Some(DetPid::from_raw(tgid)),
                None,
            )
            .await
        {
            return Ok(self.record_or_replay(guest, call).await?);
        }
        let Some(tid) = self
            .process_signal_target(guest, Sysno::kill, DetPid::from_raw(tgid), call.sig())
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
                .with_tgid(tgid)
                .with_tid(tid.as_raw())
                .with_sig(call.sig());
            self.record_or_replay(guest, targeted).await?
        };
        self.notify_cross_task_signal(guest, tid, call.sig(), Some(DetPid::from_raw(tgid)))
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
    /// The thread that a process-directed signal `sig` to `tgid` is routed to.
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
        let value = self.record_or_replay(guest, call).await?;
        // Same wakeup obligation as `tgkill`; `tkill` is the older two-argument
        // spelling of the same thread-directed send.
        self.notify_cross_task_signal(guest, DetTid::from_raw(call.tid()), call.sig(), None)
            .await;
        Ok(value)
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
        if !guest.config().sequentialize_threads {
            if self
                .drop_signal_to_hermit(guest, Sysno::rt_sigqueueinfo, None, call.tgid(), call.sig())
                .await
            {
                return Ok(0);
            }
            return Ok(self.record_or_replay(guest, call).await?);
        }

        if call.sig() == 0 {
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
        if call.sig() == libc::SIGALRM {
            refuse_sigalrm(guest, SigalrmControl::SendTo(DetTid::from_raw(tgid))).await?;
        }
        if self
            .drop_signal_to_hermit(guest, Sysno::rt_sigqueueinfo, None, tgid, call.sig())
            .await
        {
            return Ok(0);
        }
        // A zombie keeps its old answer here, ESRCH: unlike kill, the kernel
        // checks the siginfo before it looks the process up.
        let tid = self
            .process_signal_target(
                guest,
                Sysno::rt_sigqueueinfo,
                DetPid::from_raw(tgid),
                call.sig(),
            )
            .await?
            .ok_or(Errno::ESRCH)?;
        let value = if !guest
            .config()
            .backend
            .requires_thread_directed_process_signals
        {
            self.record_or_replay(guest, call).await?
        } else {
            let targeted = syscalls::RtTgsigqueueinfo::new()
                .with_tgid(tgid)
                .with_tid(tid.as_raw())
                .with_sig(call.sig())
                .with_siginfo(call.siginfo());
            self.record_or_replay(guest, targeted).await?
        };
        self.notify_cross_task_signal(guest, tid, call.sig(), Some(DetPid::from_raw(tgid)))
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

    /// Before a caught signal is delivered, the thread's mask is the one its
    /// handler's return restores, through an `rt_sigreturn` that ptrace does
    /// not stop at. Record the real-time signals that mask leaves unblocked, so
    /// an expiration of one, while the handler's `sa_mask` blocks it, is never
    /// kept pending (`Scheduler::real_time_expiration_fate`,
    /// https://github.com/rrnewton/hermit/issues/4089). Only where real-time
    /// expirations are kept pending at all (`models_signal_targets`), and only
    /// what this thread has not recorded yet. A state that cannot be read
    /// records every real-time signal.
    pub(crate) async fn record_handler_restore_mask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        signal: Signal,
    ) {
        if !(self.cfg.sequentialize_threads
            && self.cfg.backend_supports_blocked_wait_signal_interruption)
        {
            return;
        }
        let reported = guest.thread_state().handler_restore_rt_reported;
        if reported == REAL_TIME_SIGNALS {
            return;
        }
        let unblocked = match read_thread_wait_signal_state(guest.pid(), guest.tid()) {
            Ok(state) if state.caught & kernel_sigset_bit(signal as i32) == 0 => {
                return;
            }
            Ok(state) => !state.blocked & REAL_TIME_SIGNALS,
            Err(_) => REAL_TIME_SIGNALS,
        } & !reported;
        if unblocked == 0 {
            return;
        }
        let Some(detpid) = guest.thread_state().detpid else {
            return;
        };
        crate::tool_global::record_handler_restore_mask(guest, detpid, unblocked).await;
        guest.thread_state_mut().handler_restore_rt_reported |= unblocked;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn timeval(seconds: libc::time_t, micros: libc::suseconds_t) -> libc::timeval {
        libc::timeval {
            tv_sec: seconds,
            tv_usec: micros,
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
