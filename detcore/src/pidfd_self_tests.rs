/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The physical spelling of `pidfd_send_signal`'s self sentinels
//! (`pidfd_self`): the preflight's decisions, the respelling adapter, the
//! slot's ownership, and the handler's refusals.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::InjectionPurpose;
use reverie::Pid;
use reverie::PidfdSelfSpelling;
use reverie::Subscription;
use reverie::Tool;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::Config;
use crate::Detcore;
use crate::GlobalState;
use crate::PidfdSelfQualificationRefusal;
use crate::ThreadState;
use crate::pidfd_self::ArmedRespelling;
use crate::pidfd_self::PhysicalRespelling;
use crate::pidfd_self::PhysicalSpelling;
use crate::pidfd_self::SentinelPreflight;
use crate::pidfd_self::preflight;
use crate::record_or_replay::NoopTool;
use crate::record_or_replay::RecordOrReplay;
use crate::syscall_hot_path_tests::UnusedStack;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::types::DetPid;

const TID: i32 = 4343;
const THREAD: i32 = -10000;
const GROUP: i32 = -10001;
const LEGACY_GROUP: i32 = -20000;

/// The register a C caller's `syscall(SYS_pidfd_send_signal, -10001, ...)`
/// passes: the `int`, sign-extended.
fn register(pidfd: i32) -> usize {
    pidfd as isize as usize
}

fn pidfd_send_signal(arg0: usize, signal: usize, info: usize, flags: usize) -> Syscall {
    Syscall::from_raw(
        Sysno::pidfd_send_signal,
        SyscallArgs::new(arg0, signal, info, flags, 0x5a5a, 0xa5a5),
    )
}

/// How the scripted thread was asked to run a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Inject,
    Purpose(InjectionPurpose),
    Restartable,
    Tail,
}

/// A thread that logs every call injected through it, and how, and answers
/// each with the next scripted reply (success 0 when the script is empty). It
/// answers the two RPCs a refusal sends and declares `spelling`.
struct ScriptGuest<T: RecordOrReplay<ThreadState = ()>> {
    config: Config,
    thread: ThreadState<()>,
    spelling: PidfdSelfSpelling,
    replies: Mutex<VecDeque<Result<i64, Errno>>>,
    injected: Mutex<Vec<(Route, Sysno, SyscallArgs)>>,
    rpcs: Mutex<Vec<GlobalRequest>>,
    _tool: std::marker::PhantomData<T>,
}

impl<T: RecordOrReplay<ThreadState = ()>> ScriptGuest<T> {
    fn new(spelling: PidfdSelfSpelling) -> Self {
        Self::with_config(spelling, Config::default())
    }

    fn with_config(spelling: PidfdSelfSpelling, config: Config) -> Self {
        let mut thread = ThreadState::new(DetPid::from_raw(TID), &config, ());
        thread.detpid = Some(DetPid::from_raw(TID));
        Self {
            config,
            thread,
            spelling,
            replies: Mutex::new(VecDeque::new()),
            injected: Mutex::new(Vec::new()),
            rpcs: Mutex::new(Vec::new()),
            _tool: std::marker::PhantomData,
        }
    }

    fn replying(self, replies: impl IntoIterator<Item = Result<i64, Errno>>) -> Self {
        self.replies.lock().unwrap().extend(replies);
        self
    }

    fn log<S: SyscallInfo>(&self, route: Route, call: S) -> Result<i64, Errno> {
        let (number, args) = call.into_parts();
        self.injected.lock().unwrap().push((route, number, args));
        self.replies.lock().unwrap().pop_front().unwrap_or(Ok(0))
    }

    fn injected(&self) -> Vec<(Route, Sysno, SyscallArgs)> {
        self.injected.lock().unwrap().clone()
    }

    fn rpcs(&self) -> Vec<GlobalRequest> {
        self.rpcs.lock().unwrap().clone()
    }
}

#[reverie::tool]
impl<T: RecordOrReplay<ThreadState = ()>> GlobalRPC<GlobalState> for ScriptGuest<T> {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let request = message.2;
        self.rpcs.lock().unwrap().push(request.clone());
        let response = match request {
            GlobalRequest::PidfdSelfQualificationRefusal(_) => GlobalResponse::PolicyRefusal(()),
            GlobalRequest::ReportUnsupportedSyscall(_) => {
                GlobalResponse::ReportUnsupportedSyscall(())
            }
            // A forwarded send names its own process.
            GlobalRequest::SignalTargetProcess(named) => {
                GlobalResponse::SignalTargetProcess(Some(named))
            }
            request => panic!("the preflight sent an unexpected RPC: {request:?}"),
        };
        (None, response)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl<T: RecordOrReplay<ThreadState = ()>> Guest<Detcore<T>> for ScriptGuest<T> {
    type Memory = LocalMemory;
    type Stack = UnusedStack;
    fn pid(&self) -> Pid {
        Pid::from_raw(TID)
    }
    fn tid(&self) -> Pid {
        Pid::from_raw(TID)
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
        panic!("a respelled call needs no registers")
    }
    async fn stack(&mut self) -> UnusedStack {
        panic!("a respelled call needs no scratch stack")
    }
    async fn daemonize(&mut self) {
        panic!("a respelled call must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        self.log(Route::Inject, call)
    }
    async fn inject_with_purpose<S: SyscallInfo>(
        &mut self,
        call: S,
        purpose: InjectionPurpose,
    ) -> Result<i64, Error> {
        match self.log(Route::Purpose(purpose), call) {
            // A typed backend failure, not an errno, for the pass-through test.
            Err(Errno::ENOTRECOVERABLE) => Err(Error::Tool(anyhow::anyhow!("typed refusal"))),
            result => result.map_err(Error::from),
        }
    }
    async fn inject_restartable<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        self.log(Route::Restartable, call)
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, call: S) -> reverie::Never {
        let _ = self.log(Route::Tail, call);
        loop {
            futures::future::pending::<()>().await;
        }
    }
    fn injected_syscalls(&self) -> Option<u64> {
        Some(self.injected.lock().unwrap().len() as u64)
    }
    fn pidfd_self_spelling(&self) -> PidfdSelfSpelling {
        self.spelling
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

/// A record tool that records the call it is given and injects it, as the
/// recorder does for a call it does not model.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RecordingTool;

static RECORDED: Mutex<Vec<(Sysno, SyscallArgs)>> = Mutex::new(Vec::new());

#[reverie::tool]
impl Tool for RecordingTool {
    type GlobalState = GlobalState;
    type ThreadState = ();

    fn subscriptions(_cfg: &Config) -> Subscription {
        Subscription::none()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        RECORDED.lock().unwrap().push(call.into_parts());
        Ok(guest.inject_with_retry(call).await?)
    }
}

fn detcore<T: RecordOrReplay<ThreadState = ()>>(config: &Config) -> Detcore<T> {
    <Detcore<T> as Tool>::new(Pid::from_raw(TID), config)
}

fn respelling(call: Syscall) -> PhysicalRespelling {
    match preflight(PidfdSelfSpelling::Legacy, call, GROUP) {
        SentinelPreflight::Respell(respelling) => respelling,
        other => panic!("Legacy group sentinel preflight answered {other:?}"),
    }
}

fn args_with_arg0(call: Syscall, arg0: usize) -> SyscallArgs {
    SyscallArgs {
        arg0,
        ..call.into_parts().1
    }
}

/// Every spelling a backend can declare, for both sentinels: only a qualified
/// spelling that differs from the guest's is respelled, and only the
/// descriptor changes.
#[test]
fn preflight_decides_from_the_declared_spelling() {
    let thread = pidfd_send_signal(register(THREAD), 0, 0, 0);
    let group = pidfd_send_signal(register(GROUP), 0, 0, 0);
    assert_eq!(
        preflight(PidfdSelfSpelling::Modern, group, GROUP),
        SentinelPreflight::Canonical
    );
    assert_eq!(
        preflight(PidfdSelfSpelling::Modern, thread, THREAD),
        SentinelPreflight::Canonical
    );
    assert_eq!(
        preflight(PidfdSelfSpelling::Legacy, thread, THREAD),
        SentinelPreflight::Canonical
    );
    let respelled = respelling(group);
    assert_eq!(
        respelled.physical(group),
        Some(Syscall::from_raw(
            Sysno::pidfd_send_signal,
            args_with_arg0(group, register(LEGACY_GROUP))
        ))
    );
    for (call, pidfd) in [(thread, THREAD), (group, GROUP)] {
        assert_eq!(
            preflight(PidfdSelfSpelling::Unsupported, call, pidfd),
            SentinelPreflight::Unsupported
        );
        assert_eq!(
            preflight(PidfdSelfSpelling::Unknown, call, pidfd),
            SentinelPreflight::Unknown
        );
    }
}

/// The respelling applies only to exactly the guest's call: another syscall,
/// or the same one with any other register different, is injected as it is.
/// The descriptor's upper register half is kept, whichever way the caller
/// extended its `int`.
#[test]
fn respelling_matches_all_six_registers_and_keeps_the_upper_half() {
    let group = pidfd_send_signal(register(GROUP), 0, 0, 0);
    let respelled = respelling(group);
    let (_, args) = group.into_parts();
    for changed in [
        SyscallArgs { arg1: 1, ..args },
        SyscallArgs { arg2: 8, ..args },
        SyscallArgs { arg3: 1, ..args },
        SyscallArgs { arg4: 0, ..args },
        SyscallArgs { arg5: 0, ..args },
        SyscallArgs {
            arg0: GROUP as u32 as usize,
            ..args
        },
    ] {
        let call = Syscall::from_raw(Sysno::pidfd_send_signal, changed);
        assert_eq!(respelled.physical(call), None, "{changed:?}");
    }
    let kill = Syscall::from_raw(Sysno::kill, args);
    assert_eq!(respelled.physical(kill), None);

    let zero_extended = pidfd_send_signal(GROUP as u32 as usize, 0, 0, 0);
    let respelled = respelling(zero_extended);
    let (_, physical) = respelled.physical(zero_extended).unwrap().into_parts();
    assert_eq!(physical.arg0, LEGACY_GROUP as u32 as usize);
    assert_eq!(physical.arg0 as i32, LEGACY_GROUP);
}

/// Every injection route of the guest's call reaches the kernel respelled,
/// with its purpose and its typed error unchanged: plain, purposed, each
/// retry of a retried injection, the backend's own restartable injection, and
/// a tail injection (whose future is dropped unfinished). Another call goes
/// through unchanged, and queries reach the wrapped thread.
#[test]
fn the_adapter_respells_every_injection_route_of_the_guest_call() {
    let group = pidfd_send_signal(register(GROUP), 0, 0, 0);
    let respelled = respelling(group);
    let physical = args_with_arg0(group, register(LEGACY_GROUP));
    let canonical = group.into_parts().1;
    let getpid = Syscall::from_raw(Sysno::getpid, SyscallArgs::new(0, 0, 0, 0, 0, 0));
    futures::executor::block_on(async {
        let mut inner = ScriptGuest::<NoopTool>::new(PidfdSelfSpelling::Legacy).replying([
            Ok(0),
            Ok(0),
            Err(Errno::ENOTRECOVERABLE),
            Err(Errno::EINTR),
            Err(Errno::ERESTARTSYS),
            Ok(0),
            Ok(0),
            Ok(42),
        ]);
        {
            let mut adapter = PhysicalSpelling::<_, Detcore>::new(&mut inner, respelled);
            assert_eq!(adapter.inject(group).await, Ok(0));
            assert_eq!(
                adapter
                    .inject_with_purpose(group, InjectionPurpose::Application)
                    .await
                    .unwrap(),
                0
            );
            let typed = adapter
                .inject_with_purpose(group, InjectionPurpose::Application)
                .await
                .unwrap_err();
            assert!(matches!(typed, Error::Tool(_)), "{typed:?}");
            assert_eq!(adapter.inject_with_retry(group).await, Ok(0));
            assert_eq!(adapter.inject_restartable(group).await, Ok(0));
            assert_eq!(adapter.inject(getpid).await, Ok(42));
            assert_eq!(adapter.pidfd_self_spelling(), PidfdSelfSpelling::Legacy);
            assert_eq!(adapter.take_unsupported_refusal(), None);
            assert_eq!(adapter.injected_syscalls(), Some(8));

            let mut tail = Box::pin(adapter.tail_inject(group));
            let waker = futures::task::noop_waker();
            assert!(matches!(
                tail.as_mut().poll(&mut Context::from_waker(&waker)),
                Poll::Pending
            ));
            drop(tail);
            assert_eq!(adapter.injected_syscalls(), Some(9));
        }
        let pidfd = Sysno::pidfd_send_signal;
        assert_eq!(
            inner.injected(),
            vec![
                (Route::Inject, pidfd, physical),
                (
                    Route::Purpose(InjectionPurpose::Application),
                    pidfd,
                    physical
                ),
                (
                    Route::Purpose(InjectionPurpose::Application),
                    pidfd,
                    physical
                ),
                (Route::Inject, pidfd, physical),
                (Route::Inject, pidfd, physical),
                (Route::Inject, pidfd, physical),
                (Route::Restartable, pidfd, physical),
                (Route::Inject, Sysno::getpid, getpid.into_parts().1),
                (Route::Tail, pidfd, physical),
            ]
        );
        assert!(
            inner
                .injected()
                .iter()
                .all(|(_, _, args)| *args != canonical)
        );
    })
}

/// Through Detcore's forwarding, with the slot armed: the record tool
/// records the guest's canonical call and the kernel is given the physical
/// one; with no tool the injection is respelled the same way; and with the
/// slot empty nothing is respelled.
#[test]
fn forwarding_records_the_canonical_call_and_injects_the_physical_one() {
    let group = pidfd_send_signal(register(GROUP), 0, 0, 0);
    let respelled = respelling(group);
    let physical = args_with_arg0(group, register(LEGACY_GROUP));
    let canonical = group.into_parts().1;
    let pidfd = Sysno::pidfd_send_signal;
    futures::executor::block_on(async {
        RECORDED.lock().unwrap().clear();
        let tool = detcore::<RecordingTool>(&Config::default());
        let mut guest = ScriptGuest::<RecordingTool>::new(PidfdSelfSpelling::Legacy)
            .replying([Err(Errno::EINTR), Ok(0)]);
        guest.thread.pidfd_respelling = Some(respelled);
        assert_eq!(
            tool.record_or_replay_preserving_tool_errors(&mut guest, group)
                .await
                .unwrap(),
            0
        );
        assert_eq!(*RECORDED.lock().unwrap(), vec![(pidfd, canonical)]);
        assert_eq!(
            guest.injected(),
            vec![
                (Route::Inject, pidfd, physical),
                (Route::Inject, pidfd, physical)
            ]
        );

        let tool = detcore::<NoopTool>(&Config::default());
        let mut guest = ScriptGuest::<NoopTool>::new(PidfdSelfSpelling::Legacy);
        guest.thread.pidfd_respelling = Some(respelled);
        tool.record_or_replay_preserving_tool_errors(&mut guest, group)
            .await
            .unwrap();
        assert_eq!(guest.injected(), vec![(Route::Inject, pidfd, physical)]);

        let mut guest = ScriptGuest::<NoopTool>::new(PidfdSelfSpelling::Legacy);
        tool.record_or_replay_preserving_tool_errors(&mut guest, group)
            .await
            .unwrap();
        assert_eq!(guest.injected(), vec![(Route::Inject, pidfd, canonical)]);
    })
}

/// The slot is held exactly as long as its guard: dropping the guard, also
/// by dropping a future that holds it across an await, restores what was
/// there before, including an outer arming. A new thread state starts with
/// none, and the slot is not serialized: a thread state encodes the same
/// with it armed.
#[test]
fn the_slot_is_restored_when_its_guard_or_future_is_dropped() {
    let group = pidfd_send_signal(register(GROUP), 0, 0, 0);
    let respelled = respelling(group);
    let mut guest = ScriptGuest::<NoopTool>::new(PidfdSelfSpelling::Legacy);
    assert_eq!(guest.thread.pidfd_respelling, None);
    {
        let mut armed = ArmedRespelling::<NoopTool, _>::arm(&mut guest, Some(respelled));
        assert_eq!(armed.thread_state().pidfd_respelling, Some(respelled));
        {
            let inner = ArmedRespelling::<NoopTool, _>::arm(&mut *armed, None);
            assert_eq!(inner.thread_state().pidfd_respelling, None);
        }
        assert_eq!(armed.thread_state().pidfd_respelling, Some(respelled));
    }
    assert_eq!(guest.thread.pidfd_respelling, None);

    let mut held = Box::pin(async {
        let armed = ArmedRespelling::<NoopTool, _>::arm(&mut guest, Some(respelled));
        futures::future::pending::<()>().await;
        drop(armed);
    });
    let waker = futures::task::noop_waker();
    assert!(matches!(
        held.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));
    drop(held);
    assert_eq!(guest.thread.pidfd_respelling, None);

    let encode = |thread: &ThreadState<()>| {
        bincode::serde::encode_to_vec(thread, bincode::config::standard()).unwrap()
    };
    let empty = encode(&guest.thread);
    guest.thread.pidfd_respelling = Some(respelled);
    assert_eq!(encode(&guest.thread), empty);
}

/// Detcore with the unsupported-syscall policy `panic`, on a thread whose
/// configuration has `exit` and `shutdown` set as given.
fn refusing(
    spelling: PidfdSelfSpelling,
    panic: bool,
    exit: bool,
) -> (Detcore<NoopTool>, ScriptGuest<NoopTool>) {
    let config = Config {
        sequentialize_threads: false,
        panic_on_unsupported_syscalls: panic,
        exit_on_unsupported_syscall: exit,
        shutdown_on_unsupported_syscall: false,
        ..Config::default()
    };
    (
        detcore::<NoopTool>(&config),
        ScriptGuest::with_config(spelling, config),
    )
}

async fn send(
    tool: &Detcore<NoopTool>,
    guest: &mut ScriptGuest<NoopTool>,
    pidfd: i32,
    signal: i32,
    flags: u32,
) -> Result<i64, Error> {
    let call = pidfd_send_signal(register(pidfd), signal as usize, 0, flags as usize);
    tool.handle_pidfd_send_signal(guest, call, pidfd, signal, flags)
        .await
}

/// A backend that could not qualify the spelling gets the typed refusal for
/// either sentinel, whatever the unsupported-syscall policy: filed with the
/// coordinator once, nothing injected, no other RPC (so no exit, SIGKILL,
/// discard or send), and the slot left empty.
#[test]
fn an_unqualified_sentinel_is_a_typed_refusal_with_no_effect() {
    futures::executor::block_on(async {
        for panic in [false, true] {
            for pidfd in [THREAD, GROUP] {
                let (tool, mut guest) = refusing(PidfdSelfSpelling::Unknown, panic, panic);
                let error = send(&tool, &mut guest, pidfd, libc::SIGUSR1, 0)
                    .await
                    .unwrap_err();
                let Error::Tool(error) = error else {
                    panic!("not a Tool error: {error:?}");
                };
                let refusal = PidfdSelfQualificationRefusal { sentinel: pidfd };
                assert_eq!(
                    error.downcast_ref::<PidfdSelfQualificationRefusal>(),
                    Some(&refusal)
                );
                assert_eq!(
                    guest.rpcs(),
                    vec![GlobalRequest::PidfdSelfQualificationRefusal(refusal)]
                );
                assert!(guest.injected().is_empty());
                assert_eq!(guest.thread.pidfd_respelling, None);
            }
        }
    })
}

/// The checks Linux makes before it looks at the descriptor, and a negative
/// descriptor that is not a sentinel in the guest's ABI (Linux 6.16's group
/// spelling, and one past the guest's), keep their answers with a provider
/// that could not qualify the spelling: the preflight comes after them.
#[test]
fn earlier_answers_come_before_the_preflight() {
    futures::executor::block_on(async {
        for (pidfd, flags, answer) in [
            (GROUP, 3, Errno::EINVAL),
            (THREAD, 8, Errno::EINVAL),
            (LEGACY_GROUP, 0, Errno::EBADF),
            (-10002, 0, Errno::EBADF),
        ] {
            let (tool, mut guest) = refusing(PidfdSelfSpelling::Unknown, true, true);
            let error = send(&tool, &mut guest, pidfd, 0, flags).await.unwrap_err();
            assert_eq!(error.into_errno().unwrap(), answer, "{pidfd} {flags}");
            assert!(guest.rpcs().is_empty(), "{pidfd} {flags}");
            assert!(guest.injected().is_empty(), "{pidfd} {flags}");
        }
    })
}

/// A backend whose executor does not perform the call: reported as an
/// unsupported syscall, then the policy, unchanged. Allowed, the guest gets
/// ENOSYS; with the typed exit, the error is `UnsupportedSyscallError`, still
/// found by type, and carries the reason. Nothing is injected.
#[test]
fn an_unsupported_sentinel_is_named_and_keeps_the_policy() {
    futures::executor::block_on(async {
        let (tool, mut guest) = refusing(PidfdSelfSpelling::Unsupported, false, false);
        let error = send(&tool, &mut guest, GROUP, 0, 0).await.unwrap_err();
        assert_eq!(error.into_errno().unwrap(), Errno::ENOSYS);
        assert_eq!(
            guest.rpcs(),
            vec![GlobalRequest::ReportUnsupportedSyscall(
                "pidfd_send_signal".to_owned()
            )]
        );
        assert!(guest.injected().is_empty());

        let (tool, mut guest) = refusing(PidfdSelfSpelling::Unsupported, true, true);
        let error = send(&tool, &mut guest, THREAD, 0, 0).await.unwrap_err();
        let Error::Tool(error) = error else {
            panic!("not a Tool error: {error:?}");
        };
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<crate::UnsupportedSyscallError>())
        );
        assert!(
            format!("{error:#}")
                .contains("pidfd_send_signal through the self sentinel -10000 cannot be serviced"),
            "{error:#}"
        );
        assert!(guest.injected().is_empty());
    })
}

/// A qualified spelling forwards: respelled where it differs, unchanged where
/// it does not, and the slot is empty again once the handler returns.
#[test]
fn a_qualified_sentinel_forwards_in_the_physical_spelling() {
    let pidfd = Sysno::pidfd_send_signal;
    futures::executor::block_on(async {
        for (spelling, sentinel, physical) in [
            (PidfdSelfSpelling::Legacy, GROUP, LEGACY_GROUP),
            (PidfdSelfSpelling::Legacy, THREAD, THREAD),
            (PidfdSelfSpelling::Modern, GROUP, GROUP),
        ] {
            let (tool, mut guest) = refusing(spelling, true, true);
            assert_eq!(send(&tool, &mut guest, sentinel, 0, 0).await.unwrap(), 0);
            let call = pidfd_send_signal(register(sentinel), 0, 0, 0);
            assert_eq!(
                guest.injected(),
                vec![(
                    Route::Inject,
                    pidfd,
                    args_with_arg0(call, register(physical))
                )],
                "{spelling:?} {sentinel}"
            );
            // Only the forwarding's own process lookup; no refusal is filed.
            assert_eq!(
                guest.rpcs(),
                vec![GlobalRequest::SignalTargetProcess(DetPid::from_raw(TID))]
            );
            assert_eq!(guest.thread.pidfd_respelling, None);
        }
    })
}

/// The unsupported-sentinel line reaches stderr with no tracing subscriber
/// and no report descriptor: the run's only diagnostic channel then. Run in a
/// child process so that no subscriber another test installed is present.
#[test]
fn an_unsupported_sentinel_is_named_without_a_subscriber() {
    const CHILD: &str = "DETCORE_PIDFD_SELF_UNSUPPORTED_CHILD";
    const TEST: &str = "pidfd_self_tests::an_unsupported_sentinel_is_named_without_a_subscriber";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!(
                "hermit: [detcore, dtid {TID}] pidfd_send_signal through the self sentinel \
                 -10001 cannot be serviced"
            )),
            "{stderr}"
        );
        return;
    }
    assert!(!tracing::dispatcher::has_been_set());
    futures::executor::block_on(async {
        let (tool, mut guest) = refusing(PidfdSelfSpelling::Unsupported, false, false);
        assert_eq!(guest.config.unsupported_syscall_report_fd, None);
        let error = send(&tool, &mut guest, GROUP, 0, 0).await.unwrap_err();
        assert_eq!(error.into_errno().unwrap(), Errno::ENOSYS);
    })
}
