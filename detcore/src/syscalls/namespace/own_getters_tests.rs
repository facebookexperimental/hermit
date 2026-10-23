/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Exercise the actual outer dispatch, including its accounting and inner
//! Tool call. A helper-only test would miss a getter left in PassThrough.

use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::{self};
use serde::Deserialize;
use serde::Serialize;

use crate::Config;
use crate::Detcore;
use crate::GlobalState;
use crate::ThreadState;
use crate::types::DetPid;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum Answer {
    Value(i64),
    Errno(i32),
    ToolFailure,
    IoFailure,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct InnerState {
    answer: Option<Answer>,
    calls: Vec<i32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct InnerTool;

#[reverie::tool]
impl Tool for InnerTool {
    type GlobalState = GlobalState;
    type ThreadState = InnerState;

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let state = guest.thread_state_mut();
        state.calls.push(call.number() as i32);
        match state.answer.expect("the test supplies an inner answer") {
            Answer::Value(value) => Ok(value),
            Answer::Errno(errno) => Err(Errno::new(errno).into()),
            Answer::ToolFailure => Err(Error::Tool(anyhow::anyhow!("inner getter failed"))),
            Answer::IoFailure => Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into()),
        }
    }
}

struct GetterGuest {
    config: Config,
    thread: ThreadState<InnerState>,
    physical_pid: Pid,
    physical_tid: Pid,
}

struct UnusedStack;
struct UnusedStackGuard;
impl Drop for UnusedStackGuard {
    fn drop(&mut self) {}
}
impl Stack for UnusedStack {
    type StackGuard = UnusedStackGuard;
    fn size(&self) -> usize {
        panic!("own getters need no scratch stack")
    }
    fn capacity(&self) -> usize {
        panic!("own getters need no scratch stack")
    }
    fn push<'a, T>(&mut self, _: T) -> Addr<'a, T> {
        panic!("own getters need no scratch stack")
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("own getters need no scratch stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("own getters need no scratch stack")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for GetterGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        panic!("own getter projection must not add an RPC: {:?}", message.2)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore<InnerTool>> for GetterGuest {
    type Memory = LocalMemory;
    type Stack = UnusedStack;
    fn pid(&self) -> Pid {
        self.physical_pid
    }
    fn tid(&self) -> Pid {
        self.physical_tid
    }
    fn ppid(&self) -> Option<Pid> {
        None
    }
    fn memory(&self) -> LocalMemory {
        LocalMemory::new()
    }
    fn thread_state(&self) -> &ThreadState<InnerState> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<InnerState> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("own getters need no registers")
    }
    async fn stack(&mut self) -> UnusedStack {
        panic!("own getters need no scratch stack")
    }
    async fn daemonize(&mut self) {
        panic!("own getters must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        panic!(
            "own getter must keep the inner answer, not inject again: {:?}",
            call.number()
        )
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("own getters must return through the posthook")
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

fn setup(pid: i32, tid: i32, nested: bool) -> (Detcore<InnerTool>, GetterGuest) {
    let mut config = Config {
        sequentialize_threads: false,
        max_timeslice: None,
        ..Config::default()
    };
    config.backend.virtualizes_syscall_clobbers = true;
    let mut thread = ThreadState::new(DetPid::from_raw(tid), &config, InnerState::default());
    thread.detpid = Some(DetPid::from_raw(pid));
    thread.physical_tid = Some(509);
    thread.nested_pid_namespace = nested;
    // Keep the original process-local Tool ID even when this is a child.
    let tool = Detcore::<InnerTool>::new(Pid::from_raw(3), &config);
    (
        tool,
        GetterGuest {
            config,
            thread,
            physical_pid: Pid::from_raw(501),
            physical_tid: Pid::from_raw(509),
        },
    )
}

async fn dispatch(
    tool: &Detcore<InnerTool>,
    guest: &mut GetterGuest,
    call: Syscall,
    answer: Answer,
) -> Result<i64, Error> {
    let before_count = guest.thread.stats.syscall_count;
    let before_time = guest.thread.thread_logical_time.as_nanos();
    guest.thread.record_or_replay.answer = Some(answer);
    guest.thread.record_or_replay.calls.clear();
    let result = tool.handle_syscall_event(guest, call).await;
    assert_eq!(guest.thread.record_or_replay.calls, [call.number() as i32]);
    assert_eq!(guest.thread.stats.syscall_count, before_count + 1);
    assert!(guest.thread.thread_logical_time.as_nanos() > before_time);
    result
}

#[tokio::test]
async fn own_getters_dispatch_projects_the_current_thread_record() {
    // Ordinary thread and a copied child whose Tool still holds parent 3.
    // Both physical backend identities and a deliberate inner sentinel differ
    // from the shared identities; native projection cannot mask a missing route.
    for (pid, tid) in [(3, 7), (9, 10)] {
        let (tool, mut guest) = setup(pid, tid, false);
        for (call, expected, physical) in [
            (Syscall::Getpid(syscalls::Getpid::new()), pid, 501),
            (Syscall::Gettid(syscalls::Gettid::new()), tid, 509),
        ] {
            for inner in [physical, 4321] {
                assert_eq!(
                    dispatch(&tool, &mut guest, call, Answer::Value(inner))
                        .await
                        .unwrap(),
                    i64::from(expected),
                    "{call:?}, current process {pid}, thread {tid}, inner {inner}"
                );
            }
        }
    }
}

#[tokio::test]
async fn own_getters_dispatch_preserves_nested_answers_and_failures() {
    for nested in [false, true] {
        let (tool, mut guest) = setup(9, 10, nested);
        for call in [
            Syscall::Getpid(syscalls::Getpid::new()),
            Syscall::Gettid(syscalls::Gettid::new()),
        ] {
            for value in [0, -1, -4095] {
                assert_eq!(
                    dispatch(&tool, &mut guest, call, Answer::Value(value))
                        .await
                        .unwrap(),
                    value
                );
            }
            for errno in [Errno::EACCES, Errno::ERESTARTSYS] {
                let error = dispatch(&tool, &mut guest, call, Answer::Errno(errno.into_raw()))
                    .await
                    .unwrap_err();
                assert!(matches!(error, Error::Errno(actual) if actual == errno));
            }
            let error = dispatch(&tool, &mut guest, call, Answer::ToolFailure)
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Tool(ref error) if error.to_string() == "inner getter failed")
            );
            let error = dispatch(&tool, &mut guest, call, Answer::IoFailure)
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Io(ref error) if error.raw_os_error() == Some(libc::ENOSPC))
            );
            if nested {
                assert_eq!(
                    dispatch(&tool, &mut guest, call, Answer::Value(4321))
                        .await
                        .unwrap(),
                    4321
                );
            }
        }
    }
}
