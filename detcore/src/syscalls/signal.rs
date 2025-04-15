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
use crate::tool_global::resource_request;
use crate::tool_global::thread_observe_time;
use crate::types::LogicalTime;

// NB: note kernel has different notation of sigaction, we cannot
// use libc's sigaction here unfortunately. See:
// https://elixir.bootlin.com/linux/latest/source/include/uapi/asm-generic/signal.h#L75
const SA_MASK_OFFET: usize = 3 * std::mem::size_of::<u64>();

impl<T: RecordOrReplay> Detcore<T> {
    /// We send the alarms to the global scheduler to handle.
    pub async fn handle_alarm<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Alarm,
    ) -> Result<i64, Error> {
        if guest.config().sequentialize_threads {
            let remaining = register_alarm(guest, call.seconds(), Signal::SIGALRM).await;
            Ok(remaining as i64)
        } else {
            info!(
                "[dtid {}] Running without scheduler, so letting alarm call through...",
                guest.thread_state().dettid
            );
            Ok(guest.inject(call).await?)
        }
    }

    /// A pause is really just an unbounded sleep.
    pub async fn handle_pause<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pause,
    ) -> Result<i64, Error> {
        if guest.config().sequentialize_threads {
            let req = Self::sleep_request_abs(guest, LogicalTime::from_nanos(std::u64::MAX)).await;
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
}
