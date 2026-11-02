/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Linux's CPU-time clocks, read from Detcore's logical CPU accounting.
//!
//! A CPU clock id is one of the two fixed ids, `CLOCK_PROCESS_CPUTIME_ID` (2)
//! and `CLOCK_THREAD_CPUTIME_ID` (3), or a negative id that encodes a task and
//! what to measure (include/linux/posix-timers_types.h:20-32 at v6.16):
//! `CPUCLOCK_PID(id) = ~(id >> 3)` names the task, bit 2
//! (`CPUCLOCK_PERTHREAD_MASK`) makes it a thread's clock rather than a
//! process's, and the low two bits (`CPUCLOCK_WHICH`) choose PROF (0), VIRT (1)
//! or SCHED (2). The low three bits 3 are a clock descriptor (`CLOCKFD`), not a
//! CPU clock. `pthread_getcpuclockid` and `clock_getcpuclockid` build these ids.
//!
//! Detcore models a thread's CPU time as the user and system time its own
//! logical clock has charged since the thread was created: retired branches
//! (and RDTSC/CPUID) as user time, intercepted syscalls at their modeled cost
//! as system time ([`DetTime::user_cpu_time`], [`DetTime::system_cpu_time`],
//! `ThreadState::thread_cpu_time`). That is the accounting `getrusage(2)` and
//! `times(2)` already report, so every CPU-time view of a task agrees.
//!
//! [`DetTime::user_cpu_time`]: crate::types::DetTime::user_cpu_time
//! [`DetTime::system_cpu_time`]: crate::types::DetTime::system_cpu_time

use reverie::Error;
use reverie::Guest;
use reverie::syscalls::Errno;
use reverie::syscalls::Sysno;

use crate::record_or_replay::RecordOrReplay;
use crate::scheduler::pid_namespaces::GuestPidAnswer;
use crate::tool_local::Detcore;
use crate::types::DetTid;
use crate::types::LogicalTime;

/// What a CPU clock measures: `CPUCLOCK_WHICH` (`cpu_clock_sample`,
/// kernel/time/posix-cpu-timers.c:194-212 at v6.16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CpuClockKind {
    /// `CPUCLOCK_PROF`: user plus system time.
    Prof,
    /// `CPUCLOCK_VIRT`: user time.
    Virt,
    /// `CPUCLOCK_SCHED`: the scheduler's runtime, which is what
    /// `CLOCK_THREAD_CPUTIME_ID` and `CLOCK_PROCESS_CPUTIME_ID` read. Linux
    /// scales `getrusage`'s and `times`'s user and system time so that they
    /// add up to it (`cputime_adjust`, kernel/sched/cputime.c:536-541), so in
    /// Detcore's model it is user plus system time too.
    Sched,
}

impl CpuClockKind {
    /// The clock's reading for a task that has used `user` and `system` time.
    pub(crate) fn sample(self, user: LogicalTime, system: LogicalTime) -> LogicalTime {
        match self {
            Self::Virt => user,
            Self::Prof | Self::Sched => user + system,
        }
    }
}

/// Whose CPU time a clock measures, by the pid its id carries. Pid 0 names
/// the caller: its own thread for a thread's clock, its own process for a
/// process's (`pid_for_clock`, kernel/time/posix-cpu-timers.c:66-71).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CpuClockOwner {
    Thread(i32),
    Process(i32),
}

/// A CPU clock id, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CpuClock {
    pub(crate) owner: CpuClockOwner,
    pub(crate) kind: CpuClockKind,
}

/// What a clock id is, as far as the id alone decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClockIdClass {
    /// Not a CPU clock: a fixed clock other than 2 and 3, a clock descriptor,
    /// or an id that names no clock at all.
    Other,
    /// A CPU clock of a thread or a process.
    Cpu(CpuClock),
    /// A negative thread-clock id whose clock bits are `CPUCLOCK_MAX` (3):
    /// no CPU clock. `pid_for_clock` refuses it from the bits alone
    /// (kernel/time/posix-cpu-timers.c:63-64), so every call answers EINVAL.
    NoCpuClock,
}

/// Decode `clockid` (include/linux/posix-timers_types.h:20-32 at v6.16).
pub(crate) fn classify(clockid: i32) -> ClockIdClass {
    const PERTHREAD: i32 = 4;
    const WHICH: i32 = 3;
    let clock = |owner| {
        ClockIdClass::Cpu(CpuClock {
            owner,
            kind: CpuClockKind::Sched,
        })
    };
    match clockid {
        libc::CLOCK_PROCESS_CPUTIME_ID => clock(CpuClockOwner::Process(0)),
        libc::CLOCK_THREAD_CPUTIME_ID => clock(CpuClockOwner::Thread(0)),
        id if id >= 0 => ClockIdClass::Other,
        id => {
            let thread = id & PERTHREAD != 0;
            let kind = match id & WHICH {
                0 => CpuClockKind::Prof,
                1 => CpuClockKind::Virt,
                2 => CpuClockKind::Sched,
                _ if thread => return ClockIdClass::NoCpuClock,
                _ => return ClockIdClass::Other,
            };
            let pid = !(id >> 3);
            let owner = if thread {
                CpuClockOwner::Thread(pid)
            } else {
                CpuClockOwner::Process(pid)
            };
            ClockIdClass::Cpu(CpuClock { owner, kind })
        }
    }
}

/// The task a CPU clock names, found as Linux's `pid_for_clock` finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClockTask {
    /// The calling thread.
    OwnThread,
    /// Another live thread of the calling process.
    Sibling(DetTid),
    /// The calling process.
    OwnProcess,
    /// Another live process, whose CPU accounting Detcore keeps in that
    /// process's own threads, out of the caller's reach.
    OtherProcess,
}

/// Which call looks the clock's task up. Only `clock_gettime` also takes the
/// calling thread's own id as naming its process
/// (kernel/time/posix-cpu-timers.c:82-89); `clock_getres` and
/// `clock_nanosleep` require a process's id (`validate_clock_permissions`,
/// :97-106, and `posix_cpu_timer_create`, :386-397).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClockLookup {
    Read,
    Other,
}

impl<T: RecordOrReplay> Detcore<T> {
    /// The task `owner` names for the calling thread (`pid_for_clock`,
    /// kernel/time/posix-cpu-timers.c:57-95 at v6.16): EINVAL for an id that
    /// names no live thread of the caller's process (a thread's clock) or no
    /// live process (a process's clock). Which process a task belongs to is
    /// the scheduler's thread tree, the answer a `SIGEV_THREAD_ID` timer's
    /// thread and a signal's target get.
    pub(crate) async fn cpu_clock_task<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        owner: CpuClockOwner,
        lookup: ClockLookup,
    ) -> Result<ClockTask, Error> {
        let (named, thread) = match owner {
            CpuClockOwner::Thread(pid) => (pid, true),
            CpuClockOwner::Process(pid) => (pid, false),
        };
        if named == 0 {
            return Ok(if thread {
                ClockTask::OwnThread
            } else {
                ClockTask::OwnProcess
            });
        }
        let named = self.cpu_clock_pid_task(guest, sysno, named).await?;
        let dettid = guest.thread_state().dettid;
        let detpid = guest.thread_state().detpid.expect("detpid unset");
        if thread && named == dettid {
            return Ok(ClockTask::OwnThread);
        }
        if !thread && (named == detpid || (lookup == ClockLookup::Read && named == dettid)) {
            return Ok(ClockTask::OwnProcess);
        }
        // A thread's clock needs a thread of the caller's process
        // (`same_thread_group`, :77-80), and a process's clock a process's
        // id: a thread's id names its process for a signal, but not for a
        // clock (`pid_has_task(pid, PIDTYPE_TGID)`, :91-94).
        match crate::tool_global::signal_target_process(guest, named).await {
            Some(process) if thread && process == detpid => Ok(ClockTask::Sibling(named)),
            Some(process) if !thread && process == named => Ok(ClockTask::OtherProcess),
            _ => Err(Errno::EINVAL.into()),
        }
    }

    /// The task the positive `pid` of a CPU clock id names. At depth 0 the pid
    /// is the DetTid. In a nested PID namespace it is a pid of that namespace,
    /// translated through the scheduler's record (`scheduler::pid_namespaces`):
    /// a pid no task of the namespace has is EINVAL, as `pid_for_clock`'s
    /// `find_vpid` answers (:73-75); one the scheduler could not record is
    /// refused.
    async fn cpu_clock_pid_task<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        pid: i32,
    ) -> Result<DetTid, Error> {
        if !guest.thread_state().nested_pid_namespace {
            return Ok(DetTid::from_raw(pid));
        }
        match crate::tool_global::guest_pid(guest, pid).await {
            GuestPidAnswer::Task { task, .. } => Ok(task),
            GuestPidAnswer::NoSuchPid => Err(Errno::EINVAL.into()),
            GuestPidAnswer::Untranslatable(reason) => Err(self
                .refuse_cpu_clock(
                    guest,
                    sysno,
                    &format!(
                        "a CPU clock id naming pid {pid} in a nested PID namespace: {reason} (https://github.com/rrnewton/hermit/issues/4155)"
                    ),
                )
                .await),
        }
    }

    /// The current reading of `clock` for the calling thread, its task found
    /// as `clock_gettime` finds it (`posix_cpu_clock_get`,
    /// kernel/time/posix-cpu-timers.c:358-379 at v6.16).
    pub(crate) async fn read_cpu_clock<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        clock: CpuClock,
    ) -> Result<LogicalTime, Error> {
        let task = self
            .cpu_clock_task(guest, sysno, clock.owner, ClockLookup::Read)
            .await?;
        self.cpu_clock_reading(guest, sysno, task, clock.kind).await
    }

    /// What a `kind` clock of `task` reads now: the calling thread's own CPU
    /// time or its process's, or a sibling thread's as of that thread's last
    /// accounting, which with threads sequentialized is where it last stopped
    /// running. A process's time includes its exited threads'
    /// (`thread_group_cputime`, kernel/sched/cputime.c:310-350). Another
    /// process's clock is refused by the unsupported-operation policy.
    pub(crate) async fn cpu_clock_reading<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        task: ClockTask,
        kind: CpuClockKind,
    ) -> Result<LogicalTime, Error> {
        let (user, system) = match task {
            ClockTask::OwnThread => guest.thread_state().thread_cpu_time(),
            ClockTask::Sibling(thread) => guest.thread_state().sibling_cpu_time(thread),
            ClockTask::OwnProcess => {
                let process = guest.thread_state_mut().process_cpu_time();
                (process.user, process.system)
            }
            ClockTask::OtherProcess => {
                return Err(self
                    .refuse_cpu_clock(
                        guest,
                        sysno,
                        "reading another process's CPU clock: Detcore keeps a process's CPU accounting in that process's own threads",
                    )
                    .await);
            }
        };
        Ok(kind.sample(user, system))
    }

    /// Refuse a CPU-clock operation Detcore cannot serve deterministically,
    /// by the unsupported-operation policy (`refuse_unserviceable_operation`):
    /// a fail-closed run ends naming `reason`; with
    /// `--allow-unsupported-syscalls` the guest gets ENOSYS.
    pub(crate) async fn refuse_cpu_clock<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        reason: &str,
    ) -> Error {
        self.refuse_unserviceable_operation_because(guest, sysno, Errno::ENOSYS, reason)
            .await
            .expect_err("an unserviceable operation is never served")
    }
}

#[cfg(test)]
mod handler_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// `MAKE_PROCESS_CPUCLOCK(pid, clock)`, include/linux/posix-timers.h.
    fn process_clock(pid: i32, which: i32) -> i32 {
        (!pid << 3) | which
    }

    /// `MAKE_THREAD_CPUCLOCK(tid, clock)`.
    fn thread_clock(tid: i32, which: i32) -> i32 {
        (!tid << 3) | 4 | which
    }

    fn cpu(owner: CpuClockOwner, kind: CpuClockKind) -> ClockIdClass {
        ClockIdClass::Cpu(CpuClock { owner, kind })
    }

    #[test]
    fn fixed_cpu_clocks_name_the_caller() {
        assert_eq!(
            classify(libc::CLOCK_PROCESS_CPUTIME_ID),
            cpu(CpuClockOwner::Process(0), CpuClockKind::Sched)
        );
        assert_eq!(
            classify(libc::CLOCK_THREAD_CPUTIME_ID),
            cpu(CpuClockOwner::Thread(0), CpuClockKind::Sched)
        );
    }

    #[test]
    fn other_fixed_clocks_are_not_cpu_clocks() {
        for id in [
            libc::CLOCK_REALTIME,
            libc::CLOCK_MONOTONIC,
            libc::CLOCK_MONOTONIC_RAW,
            libc::CLOCK_REALTIME_COARSE,
            libc::CLOCK_MONOTONIC_COARSE,
            libc::CLOCK_BOOTTIME,
            libc::CLOCK_TAI,
            10,
            12,
            1000,
        ] {
            assert_eq!(classify(id), ClockIdClass::Other, "{id}");
        }
    }

    /// The ids glibc's `pthread_getcpuclockid` and `clock_getcpuclockid`
    /// build, and the PROF and VIRT clocks a raw id can name.
    #[test]
    fn encoded_ids_carry_their_task_and_clock() {
        for pid in [0, 1, 2, 77, 4_194_304] {
            assert_eq!(
                classify(process_clock(pid, 2)),
                cpu(CpuClockOwner::Process(pid), CpuClockKind::Sched),
                "{pid}"
            );
            assert_eq!(
                classify(thread_clock(pid, 2)),
                cpu(CpuClockOwner::Thread(pid), CpuClockKind::Sched),
                "{pid}"
            );
            assert_eq!(
                classify(process_clock(pid, 0)),
                cpu(CpuClockOwner::Process(pid), CpuClockKind::Prof),
                "{pid}"
            );
            assert_eq!(
                classify(thread_clock(pid, 1)),
                cpu(CpuClockOwner::Thread(pid), CpuClockKind::Virt),
                "{pid}"
            );
        }
    }

    /// `FD_TO_CLOCKID(fd)` is `((~fd) << 3) | CLOCKFD`: a clock descriptor,
    /// whose answer this module leaves alone; the same bits with the
    /// per-thread bit set name no clock.
    #[test]
    fn clock_descriptors_and_clock_bits_three_are_not_cpu_clocks() {
        for fd in [0, 3, 1023] {
            assert_eq!(classify(process_clock(fd, 3)), ClockIdClass::Other, "{fd}");
            assert_eq!(
                classify(thread_clock(fd, 3)),
                ClockIdClass::NoCpuClock,
                "{fd}"
            );
        }
    }

    #[test]
    fn a_sample_reads_the_clocks_share_of_the_task_time() {
        let user = LogicalTime::from_nanos(7_000);
        let system = LogicalTime::from_nanos(30_000);
        assert_eq!(CpuClockKind::Virt.sample(user, system), user);
        assert_eq!(
            CpuClockKind::Prof.sample(user, system),
            LogicalTime::from_nanos(37_000)
        );
        assert_eq!(
            CpuClockKind::Sched.sample(user, system),
            LogicalTime::from_nanos(37_000)
        );
    }
}
