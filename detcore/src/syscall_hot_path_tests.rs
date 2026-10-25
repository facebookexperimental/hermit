/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The per-syscall hot path: what a syscall's forwarding to the record or
//! replay tool does, and how large the futures it builds are.

use std::future::Future;
use std::sync::Mutex;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::Subscription;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
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
use crate::ThreadState;
use crate::record_or_replay::NoopTool;
use crate::record_or_replay::RecordOrReplay;
use crate::types::DetPid;

const TID: i32 = 4242;

/// A thread that records every syscall injected through it and answers each
/// with `reply`. Nothing else it offers is expected to be used.
struct InjectGuest<T: RecordOrReplay<ThreadState = ()>> {
    config: Config,
    thread: ThreadState<()>,
    reply: Result<i64, Errno>,
    injected: Mutex<Vec<(Sysno, SyscallArgs)>>,
    _tool: std::marker::PhantomData<T>,
}

impl<T: RecordOrReplay<ThreadState = ()>> InjectGuest<T> {
    fn new(reply: Result<i64, Errno>) -> Self {
        let config = Config::default();
        let thread = ThreadState::new(DetPid::from_raw(TID), &config, ());
        Self {
            config,
            thread,
            reply,
            injected: Mutex::new(Vec::new()),
            _tool: std::marker::PhantomData,
        }
    }

    fn injected(&self) -> Vec<(Sysno, SyscallArgs)> {
        self.injected.lock().unwrap().clone()
    }
}

struct UnusedStack;
struct UnusedStackGuard;
impl Drop for UnusedStackGuard {
    fn drop(&mut self) {}
}
impl Stack for UnusedStack {
    type StackGuard = UnusedStackGuard;
    fn size(&self) -> usize {
        panic!("the hot path needs no scratch stack")
    }
    fn capacity(&self) -> usize {
        panic!("the hot path needs no scratch stack")
    }
    fn push<'a, V>(&mut self, _: V) -> Addr<'a, V> {
        panic!("the hot path needs no scratch stack")
    }
    fn reserve<'a, V>(&mut self) -> AddrMut<'a, V> {
        panic!("the hot path needs no scratch stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("the hot path needs no scratch stack")
    }
}

#[reverie::tool]
impl<T: RecordOrReplay<ThreadState = ()>> GlobalRPC<GlobalState> for InjectGuest<T> {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        panic!("forwarding a syscall sent an RPC: {:?}", message.2)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl<T: RecordOrReplay<ThreadState = ()>> Guest<Detcore<T>> for InjectGuest<T> {
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
        panic!("forwarding a syscall needs no registers")
    }
    async fn stack(&mut self) -> UnusedStack {
        panic!("forwarding a syscall needs no scratch stack")
    }
    async fn daemonize(&mut self) {
        panic!("forwarding a syscall must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        let (number, args) = call.into_parts();
        self.injected.lock().unwrap().push((number, args));
        self.reply
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("forwarding a syscall must not tail-inject")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("forwarding a syscall arms no timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("forwarding a syscall arms no timer")
    }
    fn read_clock(&mut self) -> Result<u64, reverie::Error> {
        panic!("forwarding a syscall reads no clock")
    }
}

fn detcore<T: RecordOrReplay<ThreadState = ()>>() -> Detcore<T> {
    <Detcore<T> as Tool>::new(Pid::from_raw(TID), &Config::default())
}

fn calls() -> Vec<Syscall> {
    vec![
        Syscall::from_raw(Sysno::getppid, SyscallArgs::new(0, 0, 0, 0, 0, 0)),
        Syscall::from_raw(Sysno::getpid, SyscallArgs::new(0, 0, 0, 0, 0, 0)),
        // A call with arguments, none of them a path: the arguments must
        // reach the injection unchanged.
        Syscall::from_raw(Sysno::lseek, SyscallArgs::new(3, 17, 1, 0, 0, 0)),
    ]
}

/// Detcore's forwarding of a syscall with no recorder or replayer gives the
/// same result, and injects exactly the same calls, as `NoopTool`'s own
/// `handle_syscall_event` reached through `into_guest`, the route it took
/// before forwarding injected directly. Both a success and an errno.
#[test]
fn forwarding_without_a_record_tool_matches_noop_tool() {
    futures::executor::block_on(async {
        for reply in [Ok(42), Ok(0), Err(Errno::EBADF), Err(Errno::EINTR)] {
            for call in calls() {
                let tool = detcore::<NoopTool>();
                let mut direct = InjectGuest::<NoopTool>::new(reply);
                let forwarded = tool
                    .record_or_replay_preserving_tool_errors(&mut direct, call)
                    .await;

                let mut through_tool = InjectGuest::<NoopTool>::new(reply);
                let expected = NoopTool
                    .handle_syscall_event(&mut through_tool.into_guest(), call)
                    .await;

                assert_eq!(
                    forwarded.map_err(|error| error.into_errno().unwrap()),
                    expected.map_err(|error| error.into_errno().unwrap()),
                    "{call:?} answered {reply:?}"
                );
                assert_eq!(direct.injected(), through_tool.injected(), "{call:?}");
                assert_eq!(direct.injected().len(), 1, "{call:?}");
            }
        }
    })
}

/// A custom record or replay tool that answers every call itself, as a
/// replayer does, without injecting it. It has no `RecordOrReplay` impl of
/// its own: the blanket impl makes every qualifying Tool one, as before.
#[derive(Debug, Default, Serialize, Deserialize)]
struct AnsweringTool;

#[reverie::tool]
impl Tool for AnsweringTool {
    type GlobalState = GlobalState;
    type ThreadState = ();

    fn subscriptions(_cfg: &Config) -> Subscription {
        Subscription::none()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        _guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        Ok(1000 + call.number() as i64)
    }
}

/// A custom tool needs no explicit `RecordOrReplay` impl to sit under
/// Detcore (this compiles only through the blanket impl), and only
/// `NoopTool` is bypassed: any other record or replay tool, zero-sized like
/// `NoopTool` or not, still answers every forwarded call itself.
#[test]
fn a_custom_record_tool_without_an_impl_still_answers_forwarded_calls() {
    assert!(crate::record_or_replay::injects_only::<NoopTool>());
    assert!(!crate::record_or_replay::injects_only::<AnsweringTool>());
    assert!(!crate::record_or_replay::injects_only::<u64>());
    futures::executor::block_on(async {
        for call in calls() {
            let tool = detcore::<AnsweringTool>();
            let mut guest = InjectGuest::<AnsweringTool>::new(Ok(7));
            let result = tool
                .record_or_replay_preserving_tool_errors(&mut guest, call)
                .await
                .unwrap();
            assert_eq!(result, 1000 + call.number() as i64, "{call:?}");
            assert!(guest.injected().is_empty(), "{call:?}");
        }
    })
}

fn future_size<F: Future>(_: &F) -> usize {
    std::mem::size_of::<F>()
}

/// The size in bytes of the futures one forwarded syscall builds. Every one
/// is moved at least once (into its box, or into its parent's state), so a
/// smaller future is a shorter copy on every syscall.
fn hot_path_future_sizes() -> Vec<(&'static str, usize)> {
    let tool = detcore::<NoopTool>();
    let mut guest = InjectGuest::<NoopTool>::new(Ok(0));
    let call = calls()[0];
    let handler = <Detcore as Tool>::handle_syscall_event(&tool, &mut guest, call);
    let handler_size = std::mem::size_of_val(&*handler);
    drop(handler);
    let forward = tool.record_or_replay_preserving_tool_errors(&mut guest, call);
    let forward_size = future_size(&forward);
    drop(forward);
    let passthrough = tool.passthrough(&mut guest, call);
    let passthrough_size = future_size(&passthrough);
    drop(passthrough);
    vec![
        ("Detcore::handle_syscall_event", handler_size),
        (
            "Detcore::record_or_replay_preserving_tool_errors",
            forward_size,
        ),
        ("Detcore::passthrough", passthrough_size),
    ]
}

/// Prints the sizes (run with `--nocapture`) and bounds them, so that a
/// change that grows the per-syscall futures back is noticed.
#[test]
fn hot_path_futures_stay_small() {
    // Measured with the pinned toolchain: 7072, 344 and 424 bytes. Before
    // the open family was boxed and the `/proc` translation was gated, the
    // same three were 8384, 1768 and 1848.
    let bounds = [7424, 512, 512];
    let sizes = hot_path_future_sizes();
    for ((name, size), bound) in sizes.iter().zip(bounds) {
        eprintln!("future size: {name} = {size} bytes (bound {bound})");
        assert!(
            *size <= bound,
            "{name} grew to {size} bytes: box the arm or call that made it larger"
        );
    }
}
