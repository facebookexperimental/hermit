/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls for dealing with threads and concurrency.

use std::sync::Arc;
use std::sync::Mutex;

use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::Timespec;
use reverie::syscalls::WaitPidFlag;
use reverie::Error;
use reverie::Guest;
use reverie::Pid;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::trace;

use crate::config::BlockingMode;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::SchedValue;
use crate::syscalls::helpers::nanos_duration_to_absolute_timeout;
use crate::syscalls::helpers::retry_nonblocking_syscall;
use crate::syscalls::helpers::retry_nonblocking_syscall_with_timeout;
use crate::tool_global::create_child_thread;
use crate::tool_global::futex_action;
use crate::tool_global::resource_request;
use crate::tool_global::FutexAction;
use crate::tool_local::Detcore;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::FileMetadata;

impl<T: RecordOrReplay> Detcore<T> {
    /// Clone, clone3, fork, vfork system calls
    pub async fn handle_clone_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        clone_family: syscalls::family::CloneFamily,
    ) -> Result<i64, Error> {
        let flags = clone_family.flags(&guest.memory());
        let ctid = clone_family.child_tid(&guest.memory());

        let ts = guest.thread_state_mut();
        assert_eq!(ts.clone_flags, None);
        ts.clone_flags = Some(flags);

        // TODO(T94530014):
        if flags.contains(CloneFlags::CLONE_VFORK) {
            error!(
                "hermit: clone() with CLONE_VFORK argument.  This is not currently supported and will not work."
            )
        }

        let parent_dettid = ts.dettid;
        trace!("[detcore, dtid {}] parent invoking clone.", parent_dettid);
        let maybe_res = guest.inject(Syscall::from(clone_family)).await;
        guest.thread_state_mut().clone_flags = None; // Unset, now that it has been read by the child.

        let res = maybe_res?;
        let child_tid = Pid::from_raw(res as i32);
        let child_dettid = DetTid::from_raw(child_tid.into()); // TODO(T78538674), virtualized tid/pid
        trace!(
            "[detcore] dtid {} cloned, continuing parent + register new thread.",
            child_dettid
        );

        create_child_thread(guest, child_dettid, ctid, Some(flags)).await;
        Ok(child_dettid.as_raw() as i64)
    }

    /// Exit system call
    pub async fn handle_exit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Exit,
    ) -> Result<i64, Error> {
        let request = guest
            .thread_state()
            .mk_request(ResourceID::Exit(false), Permission::RW);
        resource_request(guest, request).await;
        // It's ok here that we skip running the posthook:
        guest.tail_inject(call).await
    }

    /// Exit_group system call
    pub async fn handle_exit_group<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ExitGroup,
    ) -> Result<i64, Error> {
        let request = guest
            .thread_state()
            .mk_request(ResourceID::Exit(true), Permission::RW);
        resource_request(guest, request).await;
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
            &dettid,
            init_val
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
        let detpid = DetPid::from_raw(guest.pid().into()); // TODO(T78538674): virtualize pid/tid
        let ptr = call.uaddr().unwrap();
        let futexid = (detpid, AddrMut::as_raw(ptr));
        let futex_op = call.futex_op() & libc::FUTEX_CMD_MASK;
        let mask = match futex_op {
            libc::FUTEX_WAKE_BITSET | libc::FUTEX_WAIT_BITSET => call.val3(),
            _ => !0,
        };
        let dettid = guest.thread_state().dettid;
        match futex_op {
            libc::FUTEX_WAKE | libc::FUTEX_WAKE_BITSET => {
                let num = match futex_action(
                    guest,
                    FutexAction::WakeRequest(call.val()),
                    &futexid,
                    init_val,
                    mask,
                )
                .await
                .expect("futex wake must return value")
                {
                    SchedValue::Value(num) => num,
                    SchedValue::TimeOut => panic!("impossible, futex wake doesn't have a timeout"),
                };
                trace!(
                    "[detcore, dtid {}] emulated futex wake committed, memory value is {}, expected {}",
                    &dettid,
                    guest.memory().read_value(ptr).unwrap(),
                    call.val(),
                );
                let _ = futex_action(
                    guest,
                    FutexAction::WakeFinished(0),
                    &futexid,
                    init_val,
                    mask,
                )
                .await;
                Ok(num as i64)
            }
            libc::FUTEX_WAIT | libc::FUTEX_WAIT_BITSET => {
                // For futex_wait, timeout is a RELATIVE value.
                let ts_ptr: Option<Addr<Timespec>> = call.timeout();
                let timeout_nanos: Option<u128> = if let Some(addr) = ts_ptr {
                    let ts = guest.memory().read_value(addr)?;
                    Some(ts.tv_sec as u128 * 1000 + ts.tv_nsec as u128)
                } else {
                    None
                };

                if init_val != call.val() {
                    info!(
                        "[detcore, dtid {}] Futex wait running immediately because it will fizzle ({} != {}).",
                        &dettid,
                        init_val,
                        call.val()
                    );
                    Err(Error::Errno(Errno::EAGAIN))
                } else {
                    let maybe_timeout_lt = if let Some(ns) = timeout_nanos {
                        nanos_duration_to_absolute_timeout(guest, ns).await
                    } else {
                        None
                    };
                    let ans = futex_action(
                        guest,
                        FutexAction::WaitRequest(maybe_timeout_lt),
                        &futexid,
                        init_val,
                        mask,
                    )
                    .await;
                    let res = if ans != Some(SchedValue::TimeOut) {
                        let expected = call.val();
                        let observed = guest.memory().read_value(ptr).unwrap();
                        trace!(
                            "[detcore, dtid {}] after (emulated) futex wait, memory value is {}, expected {}",
                            &dettid,
                            observed,
                            expected,
                        );
                        if expected == observed {
                            debug!(
                                "WARNING: fishy that the futex value did not change before wakeup. Weird application-level protocol.\n"
                            );
                        }
                        Ok(0)
                    } else {
                        trace!("[detcore, dtid {}] futex wait timed out", &dettid);
                        Err(Error::Errno(Errno::ETIMEDOUT))
                    };
                    futex_action(guest, FutexAction::WaitFinished, &futexid, init_val, mask).await;
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
                    // For futex_wait, timeout is a RELATIVE value.
                    let ts_ptr: Option<Addr<Timespec>> = call.timeout();
                    let timeout_nanos: Option<u128> = if let Some(addr) = ts_ptr {
                        let ts = guest.memory().read_value(addr)?;
                        Some(ts.tv_sec as u128 * 1000 + ts.tv_nsec as u128)
                    } else {
                        None
                    };

                    if timeout_nanos == Some(0) {
                        info!(
                            "[detcore, dtid {}] Letting Futex wait through because it's nonblocking ({} != {}).",
                            dettid,
                            init_val,
                            call.val()
                        );
                        Ok(guest.inject(call).await?) // Already non-blocking.
                    } else {
                        let rsrc = make_futex_wait_request(dettid);
                        let maybe_timeout_ns = if let Some(ns) = timeout_nanos {
                            nanos_duration_to_absolute_timeout(guest, ns).await
                        } else {
                            None
                        };
                        let res = retry_nonblocking_syscall_with_timeout(
                            guest,
                            call,
                            rsrc,
                            maybe_timeout_ns,
                        )
                        .await?;
                        trace!(
                            "[detcore, dtid {}] after futex wait, memory value is {}",
                            &dettid,
                            guest.memory().read_value(call.uaddr().unwrap()).unwrap()
                        );
                        Ok(res)
                    }
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
        let metadata: FileMetadata = {
            let guard = guest.thread_state().file_metadata.lock().unwrap();
            (*guard).clone()
        };
        let new_metadata = FileMetadata {
            file_handles: metadata
                .file_handles
                .iter()
                .filter_map(|(&k, v)| {
                    if v.flags & libc::O_CLOEXEC == libc::O_CLOEXEC {
                        None
                    } else {
                        Some((k, v.clone()))
                    }
                })
                .collect(),
        };

        // close fds with O_CLOEXEC
        guest.thread_state_mut().file_metadata = Arc::new(Mutex::new(new_metadata));

        // execve(2) doesn't return upon success.
        let errno = self.record_or_replay(guest, call).await.unwrap_err();

        // execve failed, restore fds
        guest.thread_state_mut().file_metadata = Arc::new(Mutex::new(metadata));

        Err(errno.into())
    }

    /// sched_yield system call
    pub async fn handle_sched_yield<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedYield,
    ) -> Result<i64, Error> {
        if self.cfg.sequentialize_threads {
            let resource = ResourceID::SleepUntil(LogicalTime::from_nanos(0));
            let request = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, request).await;
            trace!("sched_yield resources granted from scheduler; NOT performing actual syscall",);
            Ok(0)
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// wait4 system call
    /// This is handled by the scheduler and not passed to the record/replay layer.
    pub async fn handle_wait4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Wait4,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let mut rsrc = Resources::new(dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("wait4");

        let opts1 = call.options();
        if opts1.contains(WaitPidFlag::WNOHANG) {
            resource_request(guest, rsrc.clone()).await;
            info!(
                "[dtid {}] Executing non-blocking wait4 in one shot.",
                dettid
            );
            Ok(guest.inject_with_retry(call).await?)
        } else {
            retry_nonblocking_syscall(guest, call, rsrc).await
        }
    }

    /// Ignore requests to set affinity.
    pub async fn handle_sched_setaffinity<G: Guest<Self>>(
        &self,
        _guest: &mut G,
        _call: syscalls::SchedSetaffinity,
    ) -> Result<i64, Error> {
        // TODO: we could keep track of what the user sets the affinity to in
        // the global state, and then report back, consistently, what they have
        // written.
        Ok(0)
    }

    /// Report that we are on cpu 0, irrespective of what physical CPU we are on.
    pub async fn handle_sched_getaffinity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedGetaffinity,
    ) -> Result<i64, Error> {
        const MAX_BYTES: usize = 1024;

        // N.B. we can't use an opaque, type-safe representation such as
        // nix::sched::CpuSet currently.  The problem is that the
        // SchedGetAffinity syscall treats this field as a u64.
        let mut cpu_set: [u8; MAX_BYTES] = [0; MAX_BYTES];
        cpu_set[0] = 1;

        let mut size_bytes: usize = call.len() as usize;
        if size_bytes > MAX_BYTES {
            tracing::error!(
                "sched_getaffinity: Not expecting request for more than {} byte cpu mask, actual was {} bytes",
                MAX_BYTES,
                size_bytes,
            );
            size_bytes = MAX_BYTES;
        }
        let cpu_set = &cpu_set[..size_bytes]; // Crop it down to what was requested.
        info!(
            "Suppressing sched_getaffinity and returning {}-byte virtualized result, {:?}",
            size_bytes, cpu_set
        );
        if let Some(mask) = call.mask() {
            let mask: AddrMut<u8> = mask.cast();
            guest.memory().write_exact(mask, cpu_set)?;
            // From the man page:
            // > On success, the raw sched_getaffinity() system call returns the size (in bytes) of
            // > the cpumask_t data type that is used internally by the kernel to represent the CPU
            // > set bit mask.
            Ok(16)
        } else {
            Err(Error::Errno(Errno::EFAULT))
        }
    }
}
