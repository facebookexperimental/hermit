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
/// A process that has exited and been reaped: a signal still takes its
/// leader's id as naming it, a CPU clock not.
const REAPED: i32 = 41;
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
    /// Whether the sibling has exited since the test began.
    sibling_exited: std::sync::atomic::AtomicBool,
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
            GlobalRequest::CpuClockProcess(named) => {
                let sibling_exited = self
                    .sibling_exited
                    .load(std::sync::atomic::Ordering::SeqCst);
                let answer = match named.as_raw() {
                    SIBLING if sibling_exited => None,
                    PROCESS | CALLER | SIBLING => Some(DetPid::from_raw(PROCESS)),
                    OTHER_PROCESS => Some(named),
                    _ => None,
                };
                ("process-of", GlobalResponse::CpuClockProcess(answer))
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
        sibling_exited: std::sync::atomic::AtomicBool::new(false),
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

/// `clock_getres` on a CPU clock checks the task it names and reports the
/// clock kind's resolution: 1 ns for SCHED, one tick of a 250 Hz kernel for
/// PROF and VIRT (`posix_cpu_clock_getres`). Every other clock keeps 10 us.
#[tokio::test]
async fn clock_getres_validates_the_task_a_cpu_clock_names() {
    let (tool, mut guest, _) = setup();
    let getres = |id: i32, res: Option<&mut Timespec>| {
        ClockGetres::new()
            .with_clockid(ClockId::from_raw(id as _))
            .with_res(res.map(|res| AddrMut::from_ptr(res as *mut Timespec).unwrap()))
    };
    for (id, nanos) in [
        (libc::CLOCK_THREAD_CPUTIME_ID, 1),
        (libc::CLOCK_PROCESS_CPUTIME_ID, 1),
        (thread_clock(CALLER, 2), 1),
        (thread_clock(SIBLING, 0), 4_000_000),
        (thread_clock(0, 1), 4_000_000),
        (process_clock(PROCESS, 2), 1),
        (process_clock(0, 0), 4_000_000),
        // A clock of another process is valid; only reading it is refused.
        (process_clock(OTHER_PROCESS, 1), 4_000_000),
        (libc::CLOCK_MONOTONIC, 10_000),
    ] {
        assert_eq!(
            tool.handle_clock_getres(&mut guest, getres(id, None))
                .await
                .unwrap(),
            0,
            "{id}"
        );
        let mut res = Timespec {
            tv_sec: -1,
            tv_nsec: -1,
        };
        assert_eq!(
            tool.handle_clock_getres(&mut guest, getres(id, Some(&mut res)))
                .await
                .unwrap(),
            0,
            "{id}"
        );
        assert_eq!((res.tv_sec, res.tv_nsec), (0, nanos), "{id}");
    }
    for id in [
        thread_clock(NOBODY, 2),
        process_clock(NOBODY, 2),
        // A reaped process names no clock, though a signal still names it.
        process_clock(REAPED, 2),
        // Only clock_gettime takes the caller's thread id for its process.
        process_clock(CALLER, 2),
        thread_clock(0, 3),
    ] {
        assert_eq!(
            errno(tool.handle_clock_getres(&mut guest, getres(id, None)).await),
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

fn cpu_sleep(id: i32, flags: i32, req: &Timespec) -> ClockNanosleep {
    ClockNanosleep::new()
        .with_clockid(ClockId::from_raw(id as _))
        .with_flags(flags)
        .with_req(Some(Addr::from_ptr(req as *const Timespec).unwrap()))
        .with_rem(None)
}

/// A CPU-clock sleep returns at once where its deadline has been reached and
/// is refused where it would have to wait: Linux ends it only when the
/// clock's task has used the CPU time, and Detcore's sleeps count down on the
/// virtual clock, which any process advances, so an unrelated process
/// burning CPU would end it. None of these calls waits on the virtual clock
/// (no sleep request reaches the scheduler).
#[tokio::test]
async fn a_cpu_clock_sleep_that_would_wait_is_refused() {
    let (tool, mut guest, (sibling_user, sibling_system)) = setup();
    let (caller_user, caller_system) = guest.thread.thread_cpu_time();
    let process = caller_user + caller_system + sibling_user + sibling_system;
    let at = |time: LogicalTime| Timespec {
        tv_sec: (time.as_nanos() / 1_000_000_000) as i64,
        tv_nsec: (time.as_nanos() % 1_000_000_000) as i64,
    };
    let zero = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let micro = Timespec {
        tv_sec: 0,
        tv_nsec: 1_000,
    };
    let passed = at(process);
    let future = at(process + LogicalTime::from_nanos(1_000));
    for (id, flags, req) in [
        (libc::CLOCK_PROCESS_CPUTIME_ID, 0, &zero),
        (libc::CLOCK_PROCESS_CPUTIME_ID, libc::TIMER_ABSTIME, &passed),
        (process_clock(PROCESS, 2), libc::TIMER_ABSTIME, &zero),
        (thread_clock(SIBLING, 2), 0, &zero),
        // A zero request arms nothing, so it returns at once even on a clock
        // Detcore cannot read.
        (process_clock(OTHER_PROCESS, 2), libc::TIMER_ABSTIME, &zero),
        (process_clock(OTHER_PROCESS, 2), 0, &zero),
    ] {
        assert_eq!(
            tool.handle_nanosleep_family(&mut guest, cpu_sleep(id, flags, req).into())
                .await
                .unwrap(),
            0,
            "{id} {flags}"
        );
    }
    for (id, flags, req) in [
        (libc::CLOCK_PROCESS_CPUTIME_ID, 0, &micro),
        (libc::CLOCK_PROCESS_CPUTIME_ID, libc::TIMER_ABSTIME, &future),
        (process_clock(0, 0), 0, &micro),
        (thread_clock(SIBLING, 2), 0, &micro),
        (
            thread_clock(SIBLING, 2),
            libc::TIMER_ABSTIME,
            &at(sibling_user + sibling_system + LogicalTime::from_nanos(1)),
        ),
        (process_clock(OTHER_PROCESS, 2), 0, &micro),
        (process_clock(OTHER_PROCESS, 2), libc::TIMER_ABSTIME, &micro),
    ] {
        assert_eq!(
            errno(
                tool.handle_nanosleep_family(&mut guest, cpu_sleep(id, flags, req).into())
                    .await
            ),
            Errno::ENOSYS,
            "{id} {flags}"
        );
    }
    // A reaped process names no clock.
    assert_eq!(
        errno(
            tool.handle_nanosleep_family(
                &mut guest,
                cpu_sleep(process_clock(REAPED, 2), 0, &micro).into()
            )
            .await
        ),
        Errno::EINVAL
    );
    assert!(
        take_requests(&guest)
            .iter()
            .all(|name| *name == "process-of"),
        "only membership questions, never a sleep"
    );
}

async fn try_create_timer(
    tool: &Detcore,
    guest: &mut CpuClockGuest,
    clock: i32,
) -> Result<i32, Error> {
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
    tool.handle_timer_create(guest, call).await?;
    Ok(id)
}

/// `timer_create` on a CPU clock checks the task the clock names, as
/// `posix_cpu_timer_create` does: EINVAL for no such task, a reaped process,
/// a thread of another process, or clock bits 3, and no timer is made.
#[tokio::test]
async fn timer_create_checks_the_task_a_cpu_clock_names() {
    let (tool, mut guest, _) = setup();
    for clock in [
        thread_clock(NOBODY, 2),
        process_clock(NOBODY, 2),
        process_clock(REAPED, 0),
        thread_clock(OTHER_PROCESS, 2),
        process_clock(SIBLING, 2),
    ] {
        assert_eq!(
            errno(try_create_timer(&tool, &mut guest, clock).await),
            Errno::EINVAL,
            "{clock}"
        );
    }
    assert!(!guest.thread.posix_timers.lock().unwrap().contains(0));
    // The caller's own thread clock is a valid timer clock, unlike a sleep's.
    assert_eq!(
        try_create_timer(&tool, &mut guest, thread_clock(CALLER, 2))
            .await
            .unwrap(),
        0
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

/// An absolute expiration on a CPU-clock timer is a CPU time of the task the
/// timer was created for. One that clock has already passed, with no reload,
/// fires at once, as `posix_cpu_timer_set` fires it. One that would wait for
/// CPU time still to come, or reload for more, is refused and arms nothing:
/// Detcore's timers count down on the virtual clock, which an unrelated
/// process would advance. The reading of another process's clock is refused
/// too.
#[tokio::test]
async fn an_absolute_cpu_timer_expiration_is_served_only_once_passed() {
    let (tool, mut guest, (sibling_user, sibling_system)) = setup();
    let (caller_user, caller_system) = guest.thread.thread_cpu_time();
    let sibling = sibling_user + sibling_system;
    let mut timers = Vec::new();
    for (clock, reading) in [
        (
            libc::CLOCK_PROCESS_CPUTIME_ID,
            caller_user + caller_system + sibling,
        ),
        (libc::CLOCK_THREAD_CPUTIME_ID, caller_user + caller_system),
        (thread_clock(SIBLING, 2), sibling),
        (process_clock(PROCESS, 1), caller_user + sibling_user),
    ] {
        timers.push((create_timer(&tool, &mut guest, clock).await, reading));
    }
    let wall_timer = create_timer(&tool, &mut guest, libc::CLOCK_MONOTONIC).await;
    let other = create_timer(&tool, &mut guest, process_clock(OTHER_PROCESS, 2)).await;
    {
        let posix = guest.thread.posix_timers.lock().unwrap();
        assert!(timers.iter().all(|(id, _)| posix.cpu_clock(*id).is_some()));
        assert!(posix.cpu_clock(wall_timer).is_none());
    }
    take_requests(&guest);
    let now = LogicalTime::from_nanos(GUEST_CLOCK_NANOS);
    let spec = |nanos: u64| libc::timespec {
        tv_sec: (nanos / 1_000_000_000) as i64,
        tv_nsec: (nanos % 1_000_000_000) as i64,
    };
    let arm = |id: i32, value: &libc::itimerspec| {
        TimerSettime::new()
            .with_timerid(id)
            .with_flags(libc::TIMER_ABSTIME)
            .with_new_value(Some(
                Addr::from_ptr(value as *const libc::itimerspec).unwrap(),
            ))
            .with_old_value(None)
    };
    for (id, reading) in timers {
        // Passed, or reached exactly, with no reload: fires at once.
        for expiration in [reading, LogicalTime::from_nanos(1)] {
            let value = libc::itimerspec {
                it_interval: spec(0),
                it_value: spec(expiration.as_nanos()),
            };
            assert_eq!(
                tool.handle_timer_settime(&mut guest, arm(id, &value))
                    .await
                    .unwrap(),
                0,
                "{id} {expiration:?}"
            );
            assert_eq!(
                guest.thread.posix_timers.lock().unwrap().gettime(id, now),
                Some((0, 0)),
                "{id} {expiration:?}"
            );
        }
        // Disarm, then try what would need CPU time still to come: nothing
        // is armed.
        let disarm = libc::itimerspec {
            it_interval: spec(0),
            it_value: spec(0),
        };
        tool.handle_timer_settime(&mut guest, arm(id, &disarm))
            .await
            .unwrap();
        for (value, interval) in [
            ((reading + LogicalTime::from_nanos(5_000_000)).as_nanos(), 0),
            (reading.as_nanos(), 1_000_000),
        ] {
            let value = libc::itimerspec {
                it_interval: spec(interval),
                it_value: spec(value),
            };
            assert_eq!(
                errno(tool.handle_timer_settime(&mut guest, arm(id, &value)).await),
                Errno::ENOSYS,
                "{id}"
            );
            assert_eq!(
                guest.thread.posix_timers.lock().unwrap().gettime(id, now),
                Some((0, 0)),
                "{id}: a refused arming arms nothing"
            );
        }
    }
    let value = libc::itimerspec {
        it_interval: spec(0),
        it_value: spec(5_000_000),
    };
    assert_eq!(
        errno(
            tool.handle_timer_settime(&mut guest, arm(other, &value))
                .await
        ),
        Errno::ENOSYS
    );
    // The clock reads the armings make, and, for a timer on another task's
    // clock, whether that task is still there; never a timer registration.
    assert!(
        take_requests(&guest)
            .iter()
            .all(|name| *name == "global-time" || *name == "process-of")
    );
}

/// `timer_settime` on a CPU-clock timer whose task is gone answers ESRCH,
/// arming or disarming nothing, as `posix_cpu_timer_set` does once the task
/// has been reaped: here a sibling thread that has exited since the timer was
/// made, for an absolute, a relative and a disarming value alike.
#[tokio::test]
async fn timer_settime_on_a_timer_whose_thread_exited_is_esrch() {
    let (tool, mut guest, _) = setup();
    let id = create_timer(&tool, &mut guest, thread_clock(SIBLING, 2)).await;
    guest
        .sibling_exited
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for (flags, nanos) in [(libc::TIMER_ABSTIME, 5_000_000), (0, 5_000_000), (0, 0)] {
        let value = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: 0,
                tv_nsec: nanos,
            },
        };
        let call = TimerSettime::new()
            .with_timerid(id)
            .with_flags(flags)
            .with_new_value(Some(
                Addr::from_ptr(&value as *const libc::itimerspec).unwrap(),
            ))
            .with_old_value(None);
        assert_eq!(
            errno(tool.handle_timer_settime(&mut guest, call).await),
            Errno::ESRCH,
            "{flags} {nanos}"
        );
    }
    let now = LogicalTime::from_nanos(GUEST_CLOCK_NANOS);
    assert_eq!(
        guest.thread.posix_timers.lock().unwrap().gettime(id, now),
        Some((0, 0)),
        "nothing was armed"
    );
}
