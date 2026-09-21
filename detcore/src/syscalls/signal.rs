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
use reverie::syscalls::Timespec;
use tracing::info;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::helpers::retry_nonblocking_syscall_with_timeout;
use crate::syscalls::threads::KERNEL_SIGSET_SIZE;
use crate::syscalls::threads::KernelSigaction;
use crate::syscalls::threads::KernelSigset;
use crate::tool_global::ResumeStatus;
use crate::tool_global::alarm_remaining;
use crate::tool_global::notify_signal_pending;
use crate::tool_global::register_alarm;
use crate::tool_global::resolve_kill_targets;
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
fn deterministic_kill_target(targets: &[DetTid], sig: libc::c_int) -> Result<DetTid, Errno> {
    match targets {
        [] => Err(Errno::ESRCH),
        [target] => Ok(*target),
        [target, ..] if sig == 0 => Ok(*target),
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

impl<T: RecordOrReplay> Detcore<T> {
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
            let req = Self::sleep_request_abs(guest, LogicalTime::INDEFINITE).await;
            match crate::tool_global::parked_wait_request(
                guest,
                req,
                crate::scheduler::parked::ParkedWaitPolicy::PauseNoHandlerRestart,
            )
            .await
            {
                ResumeStatus::Normal => {
                    panic!(
                        "Internal violation: pause should never return from the scheduler except by interruption!"
                    )
                }
                ResumeStatus::Signaled(_) => Err(reverie::Error::Errno(Errno::EINTR)),
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
        let mut stack = guest.stack().await;
        let pending_addr = stack.push(0_u64);
        let pending_guard = stack.commit()?;
        let pending_out = AddrMut::<libc::sigset_t>::from_raw(pending_addr.as_raw())
            .expect("stack address must be non-null");
        let pending_call = syscalls::RtSigpending::new()
            .with_set(Some(pending_out))
            .with_sigsetsize(KERNEL_SIGSET_SIZE);
        guest.inject_with_retry(pending_call).await?;
        let pending: u64 = guest.memory().read_value(pending_addr)?;
        drop(pending_guard);

        if pending & !temporary_mask != 0 {
            // The kernel will consume an already-pending signal as soon as it
            // atomically installs the temporary mask. Keep this immediate case
            // out of the terminal-wait classification; the real syscall still
            // performs delivery and restores the old mask.
            self.record_or_replay_blocking(guest, call.into()).await
        } else {
            self.record_or_replay_rt_sigsuspend(guest, call).await
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
        Ok(if let Some(kernel_action) = kernel_action {
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
                .await?
        } else {
            guest.inject(call).await?
        })
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
            return Ok(self.record_or_replay(guest, call).await?);
        }

        if call.sig() == 0 {
            return Ok(self.record_or_replay(guest, call).await?);
        }

        let tgid = call.pid();
        if can_forward_process_group_signal(
            tgid,
            call.sig(),
            guest
                .config()
                .backend_requires_thread_directed_process_signals,
        ) {
            return Ok(self.record_or_replay(guest, call).await?);
        }
        if tgid <= 0 {
            return Err(Errno::ENOSYS.into());
        }
        let targets = resolve_kill_targets(guest, DetPid::from_raw(tgid)).await;
        let tid = deterministic_kill_target(&targets, call.sig())?;
        let value = if !guest
            .config()
            .backend_requires_thread_directed_process_signals
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
    // TODO-HUMAN-REVIEW(#663)
    /// Send a thread-directed signal through the kernel. Guest PID/TID values are
    /// stable in the fresh PID namespace and delivery is scheduler-serialized.
    pub async fn handle_tgkill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Tgkill,
    ) -> Result<i64, Error> {
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
    async fn notify_cross_task_signal<G: Guest<Self>>(
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
            return Ok(self.record_or_replay(guest, call).await?);
        }

        if call.sig() == 0 {
            return Ok(self.record_or_replay(guest, call).await?);
        }

        let tgid = call.tgid();
        if tgid <= 0 {
            return Err(Errno::ENOSYS.into());
        }
        let targets = resolve_kill_targets(guest, DetPid::from_raw(tgid)).await;
        let tid = deterministic_kill_target(&targets, call.sig())?;
        let value = if !guest
            .config()
            .backend_requires_thread_directed_process_signals
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
        Ok(self.record_or_replay(guest, call).await?)
    }
}

fn should_notify_cross_task_signal(sender: DetTid, target: DetTid, raw_signal: i32) -> bool {
    raw_signal != 0 && target != sender
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
            deterministic_kill_target(&[], libc::SIGUSR1),
            Err(Errno::ESRCH)
        );
        assert_eq!(
            deterministic_kill_target(&[first], libc::SIGUSR1),
            Ok(first)
        );
        assert_eq!(
            deterministic_kill_target(&[first, second], libc::SIGUSR1),
            Err(Errno::ENOSYS)
        );
        assert_eq!(deterministic_kill_target(&[first, second], 0), Ok(first));
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
