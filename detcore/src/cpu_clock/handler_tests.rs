/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The CPU clocks through the real syscall handlers, with a scripted global
//! scheduler: what `clock_gettime`, `clock_getres`, `clock_nanosleep` and
//! `timer_settime` answer for each kind of CPU clock id, and which requests
//! they make of the scheduler on the way.

use std::sync::Mutex;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::ClockGetres;
use reverie::syscalls::ClockGettime;
use reverie::syscalls::ClockId;
use reverie::syscalls::ClockNanosleep;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::FromToRaw;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::TimerCreate;
use reverie::syscalls::TimerSettime;
use reverie::syscalls::Timespec;
use reverie::syscalls::TimespecMutPtr;

use crate::Config;
use crate::Detcore;
use crate::GlobalState;
use crate::ThreadState;
use crate::scheduler::pid_namespaces::GuestPidAnswer;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::types::DetPid;
use crate::types::LogicalTime;

/// The process, led by a thread the test never runs.
const PROCESS: i32 = 3;
/// The calling thread, which does not lead its process.
const CALLER: i32 = 7;
/// Another live thread of the process.
const SIBLING: i32 = 8;
/// Another live process.
const OTHER_PROCESS: i32 = 40;
/// An id that names no task.
const NOBODY: i32 = 99;
/// The caller's and the sibling's pids in a nested PID namespace, and one the
/// scheduler could not record.
const NESTED_CALLER: i32 = 2;
const NESTED_SIBLING: i32 = 3;
const UNTRANSLATABLE: i32 = 4;
/// What the global clock reads, in nanoseconds.
const GUEST_CLOCK_NANOS: u64 = 1_767_225_600_000_000_000;

fn process_clock(pid: i32, which: i32) -> i32 {
    (!pid << 3) | which
}

fn thread_clock(tid: i32, which: i32) -> i32 {
    (!tid << 3) | 4 | which
}

struct CpuClockGuest {
    config: Config,
    thread: ThreadState<()>,
    requests: Mutex<Vec<&'static str>>,
}

struct UnusedStack;
struct UnusedStackGuard;
impl Drop for UnusedStackGuard {
    fn drop(&mut self) {}
}
impl Stack for UnusedStack {
    type StackGuard = UnusedStackGuard;
    fn size(&self) -> usize {
        panic!("CPU clocks need no scratch stack")
    }
    fn capacity(&self) -> usize {
        panic!("CPU clocks need no scratch stack")
    }
    fn push<'a, T>(&mut self, _: T) -> Addr<'a, T> {
        panic!("CPU clocks need no scratch stack")
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("CPU clocks need no scratch stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("CPU clocks need no scratch stack")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for CpuClockGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let request = match message.2 {
            GlobalRequest::WithForwardedCount(_, request) => *request,
            request => request,
        };
        let (name, response) = match request {
            GlobalRequest::GlobalTimeLowerBound => (
                "global-time",
                GlobalResponse::GlobalTimeLowerBound(LogicalTime::from_nanos(GUEST_CLOCK_NANOS)),
            ),
            GlobalRequest::GuestPid(caller, pid) => {
                assert_eq!(caller, DetPid::from_raw(CALLER));
                let answer = match pid {
                    NESTED_CALLER => GuestPidAnswer::Task {
                        task: DetPid::from_raw(CALLER),
                        own_init_caught: None,
                    },
                    NESTED_SIBLING => GuestPidAnswer::Task {
                        task: DetPid::from_raw(SIBLING),
                        own_init_caught: None,
                    },
                    UNTRANSLATABLE => GuestPidAnswer::Untranslatable("unrecorded".to_owned()),
                    _ => GuestPidAnswer::NoSuchPid,
                };
                ("guest-pid", GlobalResponse::GuestPid(answer))
            }
            GlobalRequest::SignalTargetProcess(named) => {
                let answer = match named.as_raw() {
                    PROCESS | CALLER | SIBLING => Some(DetPid::from_raw(PROCESS)),
                    OTHER_PROCESS => Some(named),
                    _ => None,
                };
                ("process-of", GlobalResponse::SignalTargetProcess(answer))
            }
            request => panic!("unexpected request: {request:?}"),
        };
        self.requests.lock().unwrap().push(name);
        (None, response)
    }

    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for CpuClockGuest {
    type Memory = LocalMemory;
    type Stack = UnusedStack;
    fn pid(&self) -> Pid {
        Pid::from_raw(PROCESS)
    }
    fn tid(&self) -> Pid {
        Pid::from_raw(CALLER)
    }
    fn ppid(&self) -> Option<Pid> {
        None
    }
    fn memory(&self) -> LocalMemory {
        LocalMemory::new()
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("CPU clocks need no registers")
    }
    async fn stack(&mut self) -> UnusedStack {
        panic!("CPU clocks need no scratch stack")
    }
    async fn daemonize(&mut self) {
        panic!("CPU clocks must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        panic!("CPU clocks are not injected: {:?}", call.number())
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("CPU clocks are not injected")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("no timer is configured")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("no timer is configured")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("no PMU clock is configured")
    }
}

/// The calling thread with some work, a sibling it created with some work
/// of its own, accounted, and the tool.
fn setup() -> (Detcore, CpuClockGuest, (LogicalTime, LogicalTime)) {
    let config = Config {
        virtualize_time: true,
        sequentialize_threads: true,
        panic_on_unsupported_syscalls: false,
        ..Config::default()
    };
    let tool = <Detcore as Tool>::new(Pid::from_raw(PROCESS), &config);
    let mut caller = ThreadState::new(DetPid::from_raw(CALLER), &config, ());
    caller.detpid = Some(DetPid::from_raw(PROCESS));
    caller.thread_logical_time.add_rcbs(1_000);
    caller.thread_logical_time.add_syscall_with_cost(30_000);
    caller.clone_flags = Some(CloneFlags::CLONE_THREAD);
    let mut sibling = <Detcore as Tool>::init_thread_state(
        &tool,
        Tid::from_raw(SIBLING),
        Some((Tid::from_raw(CALLER), &caller)),
    );
    caller.clone_flags = None;
    sibling.detpid = Some(DetPid::from_raw(PROCESS));
    sibling.thread_logical_time.add_rcbs(400);
    sibling.thread_logical_time.add_syscall_with_cost(5_000);
    sibling.account_process_cpu_time();
    let sibling_time = sibling.thread_cpu_time();
    // Work the sibling did after its last accounting, which no other thread
    // can see until it accounts again.
    sibling.thread_logical_time.add_rcbs(1_000_000);
    let guest = CpuClockGuest {
        config,
        thread: caller,
        requests: Mutex::new(Vec::new()),
    };
    (tool, guest, sibling_time)
}

fn take_requests(guest: &CpuClockGuest) -> Vec<&'static str> {
    std::mem::take(&mut *guest.requests.lock().unwrap())
}

async fn gettime(tool: &Detcore, guest: &mut CpuClockGuest, id: i32) -> Result<LogicalTime, Error> {
    let mut out = Timespec {
        tv_sec: -1,
        tv_nsec: -1,
    };
    let call = ClockGettime::new()
        .with_clockid(ClockId::from_raw(id as _))
        .with_tp(Some(TimespecMutPtr(
            AddrMut::from_ptr(&mut out as *mut Timespec).unwrap(),
        )));
    tool.handle_clock_gettime(guest, call).await?;
    Ok(LogicalTime::from_secs(out.tv_sec as u64) + LogicalTime::from_nanos(out.tv_nsec as u64))
}

fn errno(result: Result<impl std::fmt::Debug, Error>) -> Errno {
    match result {
        Err(Error::Errno(errno)) => errno,
        other => panic!("expected an errno, got {other:?}"),
    }
}

#[tokio::test]
async fn cpu_clocks_read_the_logical_cpu_accounting() {
    let (tool, mut guest, (sibling_user, sibling_system)) = setup();
    let (caller_user, caller_system) = guest.thread.thread_cpu_time();
    assert!(caller_user > LogicalTime::ZERO && caller_system > LogicalTime::ZERO);
    let caller = caller_user + caller_system;
    let sibling = sibling_user + sibling_system;

    for (id, expected) in [
        (libc::CLOCK_THREAD_CPUTIME_ID, caller),
        (thread_clock(0, 2), caller),
        (thread_clock(CALLER, 2), caller),
        (thread_clock(CALLER, 0), caller),
        (thread_clock(CALLER, 1), caller_user),
        (libc::CLOCK_PROCESS_CPUTIME_ID, caller + sibling),
        (process_clock(0, 2), caller + sibling),
        (process_clock(PROCESS, 1), caller_user + sibling_user),
        // clock_gettime alone takes the caller's own thread id for its
        // process (kernel/time/posix-cpu-timers.c:88-89).
        (process_clock(CALLER, 2), caller + sibling),
        (
            libc::CLOCK_REALTIME,
            LogicalTime::from_nanos(GUEST_CLOCK_NANOS),
        ),
        (
            libc::CLOCK_MONOTONIC,
            LogicalTime::from_nanos(GUEST_CLOCK_NANOS),
        ),
    ] {
        assert_eq!(
            gettime(&tool, &mut guest, id).await.unwrap(),
            expected,
            "{id}"
        );
        // Every clock read keeps the one global request it always made.
        assert_eq!(take_requests(&guest), ["global-time"], "{id}");
    }

    // A sibling's clock reads its time as of its last accounting.
    for (id, expected) in [
        (thread_clock(SIBLING, 2), sibling),
        (thread_clock(SIBLING, 1), sibling_user),
    ] {
        assert_eq!(
            gettime(&tool, &mut guest, id).await.unwrap(),
            expected,
            "{id}"
        );
        assert_eq!(take_requests(&guest), ["global-time", "process-of"], "{id}");
    }
}

#[tokio::test]
async fn cpu_clock_ids_naming_no_task_answer_as_linux_does() {
    let (tool, mut guest, _) = setup();
    for id in [
        thread_clock(NOBODY, 2),
        // A thread of another process is not a thread of the caller's.
        thread_clock(OTHER_PROCESS, 2),
    ] {
        assert_eq!(
            errno(gettime(&tool, &mut guest, id).await),
            Errno::EINVAL,
            "{id}"
        );
        assert_eq!(take_requests(&guest), ["global-time", "process-of"]);
    }
    for id in [
        process_clock(NOBODY, 2),
        // A sibling thread's id is not a process id.
        process_clock(SIBLING, 2),
    ] {
        assert_eq!(
            errno(gettime(&tool, &mut guest, id).await),
            Errno::EINVAL,
            "{id}"
        );
        assert_eq!(take_requests(&guest), ["global-time", "process-of"]);
    }
    // Clock bits 3 with the per-thread bit set name no clock.
    assert_eq!(
        errno(gettime(&tool, &mut guest, thread_clock(0, 3)).await),
        Errno::EINVAL
    );
    // Another process's clock is unserviceable: ENOSYS where the run allows
    // unsupported operations.
    assert_eq!(
        errno(gettime(&tool, &mut guest, process_clock(OTHER_PROCESS, 2)).await),
        Errno::ENOSYS
    );
}

/// In a nested PID namespace a clock id carries a pid of that namespace,
/// which the scheduler translates before the id is checked.
#[tokio::test]
async fn a_nested_namespace_pid_is_translated_first() {
    let (tool, mut guest, (sibling_user, sibling_system)) = setup();
    guest.thread.nested_pid_namespace = true;
    let (caller_user, caller_system) = guest.thread.thread_cpu_time();
    assert_eq!(
        gettime(&tool, &mut guest, thread_clock(NESTED_SIBLING, 2))
            .await
            .unwrap(),
        sibling_user + sibling_system
    );
    assert_eq!(
        take_requests(&guest),
        ["global-time", "guest-pid", "process-of"]
    );
    // The caller's own nested pid is the caller: no membership question.
    assert_eq!(
        gettime(&tool, &mut guest, thread_clock(NESTED_CALLER, 1))
            .await
            .unwrap(),
        caller_user
    );
    assert_eq!(take_requests(&guest), ["global-time", "guest-pid"]);
    assert_eq!(
        errno(gettime(&tool, &mut guest, thread_clock(NOBODY, 2)).await),
        Errno::EINVAL
    );
    assert_eq!(take_requests(&guest), ["global-time", "guest-pid"]);
    assert_eq!(
        errno(gettime(&tool, &mut guest, process_clock(UNTRANSLATABLE, 2)).await),
        Errno::ENOSYS
    );
    take_requests(&guest);
    // Pid 0 is the caller in every namespace.
    assert_eq!(
        gettime(&tool, &mut guest, thread_clock(0, 2))
            .await
            .unwrap(),
        caller_user + caller_system
    );
    assert_eq!(take_requests(&guest), ["global-time"]);
}

#[tokio::test]
async fn clock_getres_validates_the_task_a_cpu_clock_names() {
    let (tool, mut guest, _) = setup();
    let getres = |id: i32| {
        ClockGetres::new()
            .with_clockid(ClockId::from_raw(id as _))
            .with_res(None)
    };
    for id in [
        libc::CLOCK_THREAD_CPUTIME_ID,
        thread_clock(CALLER, 2),
        thread_clock(SIBLING, 0),
        process_clock(PROCESS, 2),
        // A clock of another process is valid; only reading it is refused.
        process_clock(OTHER_PROCESS, 2),
    ] {
        assert_eq!(
            tool.handle_clock_getres(&mut guest, getres(id))
                .await
                .unwrap(),
            0,
            "{id}"
        );
    }
    for id in [
        thread_clock(NOBODY, 2),
        process_clock(NOBODY, 2),
        // Only clock_gettime takes the caller's thread id for its process.
        process_clock(CALLER, 2),
        thread_clock(0, 3),
    ] {
        assert_eq!(
            errno(tool.handle_clock_getres(&mut guest, getres(id)).await),
            Errno::EINVAL,
            "{id}"
        );
    }
}

#[tokio::test]
async fn clock_nanosleep_refuses_the_callers_own_thread_clock() {
    let (tool, mut guest, _) = setup();
    let request = Timespec {
        tv_sec: 0,
        tv_nsec: 1_000,
    };
    let sleep = |id: i32, req: Option<&Timespec>| {
        ClockNanosleep::new()
            .with_clockid(ClockId::from_raw(id as _))
            .with_flags(0)
            .with_req(req.map(|req| Addr::from_ptr(req as *const Timespec).unwrap()))
            .with_rem(None)
    };
    // `clock_thread` has no sleep: EOPNOTSUPP before the request is read.
    assert_eq!(
        errno(
            tool.handle_nanosleep_family(
                &mut guest,
                sleep(libc::CLOCK_THREAD_CPUTIME_ID, None).into()
            )
            .await
        ),
        Errno::EOPNOTSUPP
    );
    for id in [
        thread_clock(0, 2),
        thread_clock(CALLER, 2),
        thread_clock(NOBODY, 2),
        thread_clock(0, 3),
    ] {
        assert_eq!(
            errno(
                tool.handle_nanosleep_family(&mut guest, sleep(id, Some(&request)).into())
                    .await
            ),
            Errno::EINVAL,
            "{id}"
        );
    }
    assert!(
        take_requests(&guest)
            .iter()
            .all(|name| *name != "global-time")
    );
}

async fn create_timer(tool: &Detcore, guest: &mut CpuClockGuest, clock: i32) -> i32 {
    // SAFETY: `sigevent` is plain data, valid when zeroed.
    let mut event: libc::sigevent = unsafe { std::mem::zeroed() };
    event.sigev_notify = libc::SIGEV_NONE;
    let mut id: libc::c_int = -1;
    let call = TimerCreate::new()
        .with_clockid(ClockId::from_raw(clock as _))
        .with_sevp(Some(
            AddrMut::from_ptr(&mut event as *mut libc::sigevent).unwrap(),
        ))
        .with_timerid(Some(
            AddrMut::from_ptr(&mut id as *mut libc::c_int).unwrap(),
        ));
    assert_eq!(tool.handle_timer_create(guest, call).await.unwrap(), 0);
    id
}

/// A timer created on a CPU clock keeps counting down on the virtual clock
/// for a relative expiration; an absolute one, a CPU time of the task the
/// timer was created for, is refused.
#[tokio::test]
async fn an_absolute_expiration_on_a_cpu_clock_timer_is_refused() {
    let (tool, mut guest, _) = setup();
    let mut cpu_timers = Vec::new();
    for clock in [
        libc::CLOCK_PROCESS_CPUTIME_ID,
        libc::CLOCK_THREAD_CPUTIME_ID,
        thread_clock(SIBLING, 2),
        process_clock(PROCESS, 0),
    ] {
        cpu_timers.push(create_timer(&tool, &mut guest, clock).await);
    }
    let wall_timer = create_timer(&tool, &mut guest, libc::CLOCK_MONOTONIC).await;
    {
        let timers = guest.thread.posix_timers.lock().unwrap();
        assert!(cpu_timers.iter().all(|id| timers.on_cpu_clock(*id)));
        assert!(!timers.on_cpu_clock(wall_timer));
    }
    let value = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: 0,
            tv_nsec: 5_000_000,
        },
    };
    for id in cpu_timers {
        let call = TimerSettime::new()
            .with_timerid(id)
            .with_flags(libc::TIMER_ABSTIME)
            .with_new_value(Some(
                Addr::from_ptr(&value as *const libc::itimerspec).unwrap(),
            ))
            .with_old_value(None);
        assert_eq!(
            errno(tool.handle_timer_settime(&mut guest, call).await),
            Errno::ENOSYS,
            "{id}"
        );
    }
    assert!(take_requests(&guest).is_empty());
}
