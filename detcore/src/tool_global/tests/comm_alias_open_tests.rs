/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Opens of `..` spellings of a task's `comm` through `Detcore::handle_openat`
//! where Detcore renders `comm` from its record (package K slice A+): the
//! backend declares `provides_process_signal_control`, as KVM does, and the
//! guest's requests reach a real global tool whose process table holds the
//! tasks. The guest records every injected call.

use nix::fcntl::OFlag;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use super::*;

const LEADER: DetTid = DetTid::from_raw(17);
const WORKER: DetTid = DetTid::from_raw(19);
/// A number no task of the table holds.
const GONE: i32 = 23;

/// A guest thread of process `LEADER` whose requests reach `global` and
/// whose every injected call is recorded and answered with `reply`.
struct AliasGuest<'a> {
    config: &'a Config,
    global: &'a GlobalState,
    thread: crate::ThreadState<()>,
    sender: Tid,
    reply: Result<i64, Errno>,
    injected: Vec<(Sysno, reverie::syscalls::SyscallArgs)>,
}

struct NoStack;
struct NoStackGuard;
impl Drop for NoStackGuard {
    fn drop(&mut self) {}
}
impl reverie::Stack for NoStack {
    type StackGuard = NoStackGuard;
    fn size(&self) -> usize {
        panic!("these opens use no guest stack")
    }
    fn capacity(&self) -> usize {
        panic!("these opens use no guest stack")
    }
    fn push<'stack, T>(&mut self, _: T) -> Addr<'stack, T> {
        panic!("these opens use no guest stack")
    }
    fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
        panic!("these opens use no guest stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("these opens use no guest stack")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for AliasGuest<'_> {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.global.receive_rpc(self.sender, message).await
    }
    fn config(&self) -> &Config {
        self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for AliasGuest<'_> {
    type Memory = reverie::syscalls::LocalMemory;
    type Stack = NoStack;

    fn tid(&self) -> Tid {
        self.sender
    }
    fn pid(&self) -> Tid {
        Tid::from_raw(LEADER.as_raw())
    }
    fn ppid(&self) -> Option<Tid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        reverie::syscalls::LocalMemory::new()
    }
    fn thread_state(&self) -> &crate::ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("these opens read no registers")
    }
    async fn stack(&mut self) -> Self::Stack {
        panic!("these opens use no guest stack")
    }
    async fn daemonize(&mut self) {
        panic!("these opens do not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        self.injected.push(call.into_parts());
        self.reply
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("these opens do not retire the guest")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("these opens set no timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("these opens set no timer")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("these opens read no clock")
    }
}

/// The record-sourced configuration and a global tool whose table holds
/// process `LEADER` with its thread `WORKER`, both named.
fn record_sourced_state() -> (Config, GlobalState) {
    let config = Config {
        sequentialize_threads: false,
        ..Config::default()
    }
    .with_backend(|backend| backend.provides_process_signal_control = true);
    let state = GlobalState::initialize(&config, false);
    {
        let mut sched = state.sched.lock().unwrap();
        sched.thread_tree.add_child(LEADER, LEADER, true);
        sched.thread_tree.add_child(LEADER, WORKER, false);
        sched.set_task_comm(LEADER, crate::procfs::task_stat::TaskComm::new(b"probe"));
        sched.set_task_comm(WORKER, crate::procfs::task_stat::TaskComm::new(b"worker"));
    }
    for tid in [LEADER, WORKER] {
        install_test_registration(&state, tid, Ivar::new());
    }
    (config, state)
}

/// `handle_openat` of `path` with `flags` by thread `WORKER`, its injected
/// calls answered with `reply`: the answer and the calls injected.
async fn open(
    path: &str,
    flags: OFlag,
    reply: Result<i64, Errno>,
) -> (
    Result<i64, Errno>,
    Vec<(Sysno, reverie::syscalls::SyscallArgs)>,
) {
    let (config, global) = record_sourced_state();
    let tool = <Detcore as reverie::Tool>::new(Tid::from_raw(LEADER.as_raw()), &config);
    let mut thread = crate::ThreadState::new(WORKER, &config, ());
    thread.detpid = Some(LEADER);
    let mut guest = AliasGuest {
        config: &config,
        global: &global,
        thread,
        sender: Tid::from_raw(WORKER.as_raw()),
        reply,
        injected: Vec::new(),
    };
    let spelled = std::ffi::CString::new(path).unwrap();
    let call = reverie::syscalls::Openat::new()
        .with_dirfd(libc::AT_FDCWD)
        .with_path(reverie::syscalls::PathPtr::from_ptr(spelled.as_ptr()))
        .with_flags(flags);
    let answer = tool
        .handle_openat(&mut guest, call)
        .await
        .map_err(|error| error.into_errno().expect("an errno, not a tool error"));
    (answer, guest.injected)
}

/// A valid `..` spelling of the caller's own task's `comm`, through a live
/// task and through `thread-self`, opened for writing: Detcore's EACCES,
/// with no injected call (the executor's answer to the spelling would be a
/// host lookup's).
#[tokio::test]
async fn a_writable_open_of_a_valid_comm_alias_is_refused_with_nothing_injected() {
    for path in [
        format!("/proc/self/task/{0}/../{0}/comm", WORKER.as_raw()),
        format!("/proc/thread-self/../{}/comm", WORKER.as_raw()),
        format!("/proc/{0}/task/../task/{0}/comm", LEADER.as_raw()),
        "/proc/../proc/self/comm".to_owned(),
    ] {
        for flags in [
            OFlag::O_RDWR,
            OFlag::O_WRONLY,
            OFlag::O_RDONLY | OFlag::O_TRUNC,
        ] {
            let (answer, injected) = open(&path, flags, Ok(3)).await;
            assert_eq!(answer, Err(Errno::EACCES), "{path} {flags:?}");
            assert!(injected.is_empty(), "{path} {flags:?}: {injected:?}");
        }
    }
}

/// An alias through a task the table does not hold fails as Linux's lookup
/// does, with ENOENT, for reading and writing alike, and a component after
/// `comm` with ENOTDIR; nothing is injected.
#[tokio::test]
async fn an_alias_through_a_dead_task_or_past_comm_fails_as_linux_with_nothing_injected() {
    for (path, errno) in [
        (
            format!("/proc/self/task/{GONE}/../{}/comm", WORKER.as_raw()),
            Errno::ENOENT,
        ),
        (format!("/proc/{GONE}/../self/comm"), Errno::ENOENT),
        ("/proc/self/task/x/../comm".to_owned(), Errno::ENOENT),
        ("/proc/self/comm/../comm".to_owned(), Errno::ENOTDIR),
    ] {
        for flags in [OFlag::O_RDWR, OFlag::O_RDONLY] {
            let (answer, injected) = open(&path, flags, Ok(3)).await;
            assert_eq!(answer, Err(errno), "{path} {flags:?}");
            assert!(injected.is_empty(), "{path} {flags:?}: {injected:?}");
        }
    }
}

/// A writable open of a `..` spelling that passes a component Detcore does
/// not model before reaching `comm` is refused by name (EOPNOTSUPP without
/// a fail-closed policy), not forwarded; nothing is injected.
#[tokio::test]
async fn a_writable_open_of_an_undetermined_comm_alias_is_refused_by_name() {
    for path in ["/proc/self/attr/../comm", "/proc/self/cwd/../comm"] {
        let (answer, injected) = open(path, OFlag::O_RDWR, Ok(3)).await;
        assert_eq!(answer, Err(Errno::EOPNOTSUPP), "{path}");
        assert!(injected.is_empty(), "{path}: {injected:?}");
    }
}

/// A `..` spelling of any other file stays the kernel beneath's, as before:
/// the open is injected with the guest's own path, and its answer is the
/// guest's.
#[tokio::test]
async fn a_dotdot_spelling_of_another_file_is_forwarded_unchanged() {
    let path = "/proc/self/task/../stat";
    let (answer, injected) = open(path, OFlag::O_RDONLY, Err(Errno::ENOENT)).await;
    assert_eq!(answer, Err(Errno::ENOENT));
    let (number, _) = injected.first().expect("the open is forwarded");
    assert_eq!(*number, Sysno::openat, "{injected:?}");
}
