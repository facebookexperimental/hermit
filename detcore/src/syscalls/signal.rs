/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls dealing with signals.

use std::time::Duration;

use nix::sys::signal::Signal;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
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
use crate::tool_global::ResumeStatus;
use crate::tool_global::register_alarm;
use crate::tool_global::resolve_kill_targets;
use crate::tool_global::resource_request;
use crate::tool_global::thread_observe_time;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;

// NB: note kernel has different notation of sigaction, we cannot
// use libc's sigaction here unfortunately. See:
// https://elixir.bootlin.com/linux/latest/source/include/uapi/asm-generic/signal.h#L75
const SA_MASK_OFFET: usize = 3 * std::mem::size_of::<u64>();

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
                Signal::SIGALRM,
            )
            .await;
            Ok(logical_time_to_alarm_seconds(remaining))
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
    /// Schedule a one-shot real-time interval timer on Detcore logical time.
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
        if timeval_to_logical_time(timer.it_interval)? != LogicalTime::ZERO {
            return Err(Error::Errno(Errno::ENOSYS));
        }

        let duration = timeval_to_logical_time(timer.it_value)?;
        let remaining = register_alarm(guest, duration, Signal::SIGALRM).await;
        if let Some(old_value) = call.ovalue() {
            let old_timer = libc::itimerval {
                it_interval: logical_time_to_timeval(LogicalTime::ZERO),
                it_value: logical_time_to_timeval(remaining),
            };
            guest.memory().write_value(old_value, &old_timer)?;
        }
        Ok(0)
    }

    /// A pause is really just an unbounded sleep.
    pub async fn handle_pause<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pause,
    ) -> Result<i64, Error> {
        if guest.config().sequentialize_threads {
            let req = Self::sleep_request_abs(guest, LogicalTime::from_nanos(u64::MAX)).await;
            match resource_request(guest, req).await {
                ResumeStatus::Normal => {
                    panic!(
                        "Internal violation: pause should never return from the scheduler except by interruption!"
                    )
                }
                ResumeStatus::Signaled => Err(reverie::Error::Errno(Errno::EINTR)),
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
        self.record_or_replay_blocking(guest, call.into()).await
    }

    /// rt_sigaction
    pub async fn handle_rt_sigaction<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigaction,
    ) -> Result<i64, Error> {
        // PERF_EVENT_SIGNAL is reserved.
        if call.signum() == reverie::PERF_EVENT_SIGNAL as i32 {
            // The go runtime attempts to register this (unused) signal handler.  We will never
            // deliver signals of this kind to the guest, so we just turn this action into a noop
            // rather than returning `Err(Errno::EINVAL.into())`.
            return Ok(0);
        }
        Ok(if let Some(action) = call.action() {
            let mut memory = guest.memory();
            let sa_mask: AddrMut<libc::sigset_t> =
                AddrMut::from_raw(SA_MASK_OFFET + action.as_raw()).unwrap();
            let mut mask = memory.read_value(sa_mask)?;
            unsafe { libc::sigdelset(&mut mask as *mut _, reverie::PERF_EVENT_SIGNAL as i32) };
            memory.write_value(sa_mask, &mask)?;
            guest.inject(call).await?
        } else {
            guest.inject(call).await?
        })
    }

    /// rt_sigprocmask
    pub async fn handle_rt_sigprocmask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigprocmask,
    ) -> Result<i64, Error> {
        if call.how() != libc::SIG_BLOCK && call.how() != libc::SIG_SETMASK {
            Ok(guest.inject(call).await?)
        } else if let Some(set) = call.set() {
            let memory = guest.memory();
            let mut stack = guest.stack().await;
            let mut set_mask = memory.read_value(set)?;
            unsafe { libc::sigdelset(&mut set_mask as *mut _, reverie::PERF_EVENT_SIGNAL as i32) };
            let new_set = stack.push(set_mask);
            stack.commit()?;
            let modified_call = syscalls::RtSigprocmask::new()
                .with_how(call.how())
                .with_set(Some(new_set))
                .with_oldset(call.oldset())
                .with_sigsetsize(call.sigsetsize());
            // Using inject (intead of tail_inject) here so that
            // post_handler_hook can be called.
            Ok(guest.inject(modified_call).await?)
        } else {
            Ok(guest.inject(call).await?)
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
    /// Resolve signal-zero existence checks in the fixed PID namespace, then route an
    /// unambiguous positive-PID process signal to its sole live thread. Multithreaded
    /// process-directed delivery, process-group delivery, and broadcast delivery are
    /// refused until Detcore models eligible signal masks.
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
        if tgid <= 0 {
            return Err(Errno::ENOSYS.into());
        }
        let targets = resolve_kill_targets(guest, DetPid::from_raw(tgid)).await;
        let tid = deterministic_kill_target(&targets, call.sig())?;
        let targeted = syscalls::Tgkill::new()
            .with_tgid(tgid)
            .with_tid(tid.as_raw())
            .with_sig(call.sig());
        Ok(self.record_or_replay(guest, targeted).await?)
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
        Ok(self.record_or_replay(guest, call).await?)
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
}
