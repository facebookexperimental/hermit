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
use crate::tool_global::FutexAction;
use crate::tool_global::ResumeStatus;
use crate::tool_global::create_child_thread;
use crate::tool_global::futex_action;
use crate::tool_global::resource_request;
use crate::tool_global::thread_observe_time;
use crate::tool_local::Detcore;
use crate::tool_local::PendingVfork;
use crate::types::DetTid;
use crate::types::LogicalTime;

// Preserve the historical Detcore ABI while hiding the host's configured CPU
// count. This represents one virtual CPU in a fixed 128-bit kernel mask.
const VIRTUAL_CPUSET_BYTES: usize = 16;

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
            FutexTimeout::Absolute(deadline) if self.cfg.virtualize_time => Ok(Some(deadline)),
            FutexTimeout::Absolute(deadline) => {
                let clockid = if futex_flags & libc::FUTEX_CLOCK_REALTIME != 0 {
                    syscalls::ClockId::CLOCK_REALTIME
                } else {
                    syscalls::ClockId::CLOCK_MONOTONIC
                };

                let mut stack = guest.stack().await;
                let clock_output = syscalls::TimespecMutPtr(stack.reserve());
                let _stack_guard = stack.commit()?;
                self.record_or_replay(
                    guest,
                    syscalls::ClockGettime::new()
                        .with_clockid(clockid)
                        .with_tp(Some(clock_output)),
                )
                .await?;
                let clock_now = match parse_futex_timeout(
                    libc::FUTEX_WAIT_BITSET,
                    guest.memory().read_value(clock_output.0)?,
                )? {
                    FutexTimeout::Absolute(time) => time,
                    FutexTimeout::Relative(_) => unreachable!(),
                };
                let logical_now = thread_observe_time(guest).await;
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
        let ctid = clone_family.child_tid(&guest.memory());
        let is_vfork = flags.contains(CloneFlags::CLONE_VFORK);

        let ts = guest.thread_state_mut();
        assert_eq!(ts.clone_flags, None);
        assert!(ts.pending_vfork.is_none());
        ts.clone_flags = Some(flags);

        let parent_dettid = ts.dettid;
        let child_priority_entropy = if is_vfork
            && self.cfg.chaos
            && self.cfg.replay_preemptions_from.is_none()
            && self.cfg.replay_schedule_from.is_none()
        {
            let mut parent_chaos_prng = ts.chaos_prng.clone();
            Some(parent_chaos_prng.next_u64())
        } else {
            None
        };
        if is_vfork {
            ts.pending_vfork = Some(PendingVfork {
                parent_dettid,
                parent_detpid: ts.detpid.expect("detpid unset"),
                child_tid_addr: ctid,
                flags,
                child_priority_entropy,
            });
        }

        trace!("[detcore, dtid {}] parent invoking clone.", parent_dettid);
        let vfork_op_id =
            ExternalOpId::new(parent_dettid, guest.thread_state().stats.syscall_count);

        // The kernel blocks a CLONE_VFORK parent until its child execs or exits.
        // Remove it from Detcore's run queue before entering that blocking call.
        if is_vfork && self.cfg.sequentialize_threads {
            let mut resources = Resources::new(parent_dettid);
            resources.insert(ResourceID::BlockingExternalIO(vfork_op_id), Permission::RW);
            resources.fyi("clone_vfork");
            resource_request(guest, resources).await;
        }

        let maybe_res = guest.inject(Syscall::from(clone_family)).await;

        if is_vfork && self.cfg.sequentialize_threads {
            let mut resources = Resources::new(parent_dettid);
            resources.insert(
                ResourceID::BlockedExternalContinue(vfork_op_id),
                Permission::RW,
            );
            resources.fyi("clone_vfork");
            resource_request(guest, resources).await;
        }

        let ts = guest.thread_state_mut();
        ts.clone_flags = None; // Unset, now that it has been read by the child.
        ts.pending_vfork = None;

        let res = maybe_res?;

        // Match ordinary clone: the parent consumes the priority entropy after
        // the child has inherited the parent state.
        if is_vfork
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

        if !is_vfork {
            create_child_thread(guest, child_dettid, ctid, Some(flags)).await;
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
                        let observed = guest.memory().read_value(ptr).unwrap();
                        trace!(
                            "[detcore, dtid {}] after (emulated) futex wait, memory value is {}, expected {}",
                            &dettid, observed, expected,
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
        let (old_metadata, old_memory_metadata, table_is_shared, dettid, old_mm_id) = {
            let thread_state = guest.thread_state();
            (
                Arc::clone(&thread_state.file_metadata),
                Arc::clone(&thread_state.memory_metadata),
                Arc::strong_count(&thread_state.file_metadata) > 1,
                thread_state.dettid,
                thread_state.mm_id,
            )
        };
        let (new_metadata, closed_open_files) = {
            let metadata = old_metadata.lock().unwrap();
            (
                metadata.for_exec(dettid),
                metadata.open_files_closed_on_exec(table_is_shared),
            )
        };

        {
            let thread_state = guest.thread_state_mut();
            thread_state.file_metadata = Arc::new(Mutex::new(new_metadata));
            thread_state.memory_metadata = Arc::new(Mutex::new(MemoryMetadata::new()));
            thread_state.mm_id = old_mm_id.for_exec(dettid);
        }

        let mut released_ports = Vec::new();
        for open_file_id in closed_open_files {
            if let Some(port) = self.release_port_for_open_file(guest, open_file_id).await {
                released_ports.push((open_file_id, port));
            }
        }

        // execve(2) doesn't return upon success.
        let errno = self.record_or_replay(guest, call).await.unwrap_err();

        for (open_file_id, port) in released_ports {
            self.restore_port_for_open_file(guest, open_file_id, port)
                .await;
        }

        {
            let thread_state = guest.thread_state_mut();
            thread_state.file_metadata = old_metadata;
            thread_state.memory_metadata = old_memory_metadata;
            thread_state.mm_id = old_mm_id;
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
            if self.cfg.chaos && self.cfg.preemption_timeout.is_none() {
                let change_time = guest.thread_state().thread_logical_time.as_nanos();
                let request = Self::random_priority_changepoint_request(guest, change_time);
                resource_request(guest, request).await;
            } else if !self.cfg.chaos && self.cfg.replay_preemptions_from.is_some() {
                if self.cfg.preemption_timeout.is_some() {
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
            // wait4 is a scheduler poll, not a record/replay data read (see doc above),
            // so it is not routed through the record/replay subtool.
            retry_nonblocking_syscall(guest, call, rsrc, None).await
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#274): Review waitid polling and compatibility boundaries.
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
        // panicking on DBI's unsupported scratch stack.
        if call.info().is_none() {
            return Err(Errno::EFAULT.into());
        }

        // Linux snapshots P_PGID with id 0 at syscall entry. Preserve that
        // identity across polling calls without issuing a guest-visible syscall.
        if call.which() == libc::P_PGID as i32 && call.pid() == 0 {
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

        if call.options() & libc::WNOHANG != 0 || pidfd_nonblocking {
            if !pidfd_nonblocking {
                resource_request(guest, rsrc).await;
            }
            info!(
                "[dtid {}] Executing non-blocking waitid in one shot.",
                dettid
            );
            guest.memory().write_value(info, &empty_info)?;
            let value = guest.inject_with_retry(call).await?;
            let mut info_value: libc::siginfo_t = guest.memory().read_value(info)?;
            // SAFETY: waitid writes either zeroed output or the SIGCHLD
            // siginfo_t variant, for which libc exposes si_pid.
            if unsafe { info_value.si_pid() } != 0 {
                canonicalize_waitid_siginfo(&mut info_value);
                guest.memory().write_value(info, &info_value)?;
                if let Some(rusage) = call.rusage() {
                    // Host CPU and scheduling counters are not deterministic.
                    let usage: libc::rusage = unsafe { std::mem::zeroed() };
                    guest.memory().write_value(rusage, &usage)?;
                }
            }
            return Ok(value);
        }

        let poll_call = call.with_options(call.options() | libc::WNOHANG);
        let mut first_poll = true;
        loop {
            let signaled = !first_poll
                && resource_request(guest, rsrc.clone()).await == ResumeStatus::Signaled;
            first_poll = false;

            guest.memory().write_value(info, &empty_info)?;
            let result = guest.inject_with_retry(poll_call).await;
            match result {
                Ok(value) => {
                    let mut info_value: libc::siginfo_t = guest.memory().read_value(info)?;
                    // waitid writes the SIGCHLD variant of siginfo_t. A zeroed
                    // structure is used only for the no-event WNOHANG result.
                    let child_pid = unsafe { info_value.si_pid() };
                    if child_pid != 0 {
                        canonicalize_waitid_siginfo(&mut info_value);
                        guest.memory().write_value(info, &info_value)?;
                        if let Some(rusage) = call.rusage() {
                            // Host CPU and scheduling counters are not deterministic.
                            let usage: libc::rusage = unsafe { std::mem::zeroed() };
                            guest.memory().write_value(rusage, &usage)?;
                        }
                        return Ok(value);
                    }

                    if signaled {
                        return Err(Errno::ERESTARTSYS.into());
                    }
                    rsrc.poll_attempt += 1;
                    trace!(
                        "Retry #{} for waitid because no child state is ready",
                        rsrc.poll_attempt
                    );
                    record_retry_event(guest, poll_call).await;
                }
                Err(errno) => return Err(errno.into()),
            }
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
