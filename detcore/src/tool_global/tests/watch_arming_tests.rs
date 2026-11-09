/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `--watch` arming at a thread's stop (`crate::watch`), against a guest that
//! answers `set_watchpoints` and `watch_hits_keep_timer_events` as told.

use std::collections::VecDeque;

use clap::Parser;
use reverie::Errno;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Tid;
use reverie::Tool;
use reverie::WatchError;
use reverie::WatchSpec;

use super::ExternalRegistrationStack;
use crate::Config;
use crate::Detcore;
use crate::GlobalState;
use crate::ThreadState;
use crate::watch::WatchOrder;
use crate::watch::WatchStatus;
use crate::watch::try_adopt;

const TID: i32 = 23;

struct WatchGuest {
    config: Config,
    thread: ThreadState<()>,
    keeps_timer_events: bool,
    answers: VecDeque<Result<(), WatchError>>,
    armed: Vec<Vec<WatchSpec>>,
}

impl WatchGuest {
    fn new(answers: Vec<Result<(), WatchError>>) -> Self {
        let config = Config::parse_from(["detcore", "--watch=0x404010:8:w", "--watch=0x404020:4"]);
        let tool: Detcore = Detcore::new(Tid::from_raw(TID), &config);
        let thread = tool.init_thread_state(Tid::from_raw(TID), None);
        Self {
            config,
            thread,
            keeps_timer_events: true,
            answers: answers.into(),
            armed: Vec::new(),
        }
    }

    fn status(&self) -> WatchStatus {
        self.thread.watch.status()
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for WatchGuest {
    async fn send_rpc(
        &self,
        _: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        panic!("arming sends no request")
    }

    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for WatchGuest {
    type Memory = reverie::syscalls::LocalMemory;
    type Stack = ExternalRegistrationStack;

    fn tid(&self) -> Tid {
        Tid::from_raw(TID)
    }
    fn pid(&self) -> Tid {
        self.tid()
    }
    fn ppid(&self) -> Option<Tid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        reverie::syscalls::LocalMemory::new()
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("arming reads no register")
    }
    async fn stack(&mut self) -> Self::Stack {
        panic!("arming uses no guest stack")
    }
    async fn daemonize(&mut self) {
        panic!("arming does not daemonize")
    }
    async fn inject<S: reverie::syscalls::SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
        panic!("arming injects no syscall")
    }
    async fn tail_inject<S: reverie::syscalls::SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("arming injects no syscall")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("arming sets no timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("arming sets no timer")
    }
    fn read_clock(&mut self) -> Result<u64, reverie::Error> {
        panic!("arming reads no clock")
    }
    fn set_watchpoints(&mut self, specs: &[WatchSpec]) -> Result<(), WatchError> {
        let answer = self.answers.pop_front().expect("an answer for each arming");
        if answer.is_ok() {
            self.armed.push(specs.to_vec());
        }
        answer
    }
    fn watch_hits_keep_timer_events(&self) -> bool {
        self.keeps_timer_events
    }
}

const DUE: WatchOrder = WatchOrder { epoch: 1 };

/// Nothing is armed before the watches are due. A thread that blocks SIGTRAP
/// stays unarmed and says so, then arms at a later stop, each watch asking
/// for the stacks of its first 64 hits; once armed it arms no more.
#[test]
fn a_thread_arms_when_due_and_retries_while_it_blocks_sigtrap() {
    let mut guest = WatchGuest::new(vec![Err(WatchError::SigtrapBlocked), Ok(())]);
    try_adopt(&mut guest, WatchOrder { epoch: 0 }).unwrap();
    assert_eq!(guest.status(), WatchStatus::Pending);

    try_adopt(&mut guest, DUE).unwrap();
    assert_eq!(guest.status(), WatchStatus::SigtrapBlocked);
    try_adopt(&mut guest, DUE).unwrap();
    assert_eq!(guest.status(), WatchStatus::Armed);
    assert_eq!(guest.armed.len(), 1);
    assert_eq!(
        guest.armed[0]
            .iter()
            .map(|spec| (spec.addr, spec.len, spec.backtraces))
            .collect::<Vec<_>>(),
        vec![(0x404010, 8, 64), (0x404020, 4, 64)]
    );
    try_adopt(&mut guest, DUE).unwrap();
    try_adopt(&mut guest, DUE).unwrap();
    assert_eq!(guest.armed.len(), 1, "an armed thread arms no more");

    // After exec the thread arms again at once, as the scheduler had told
    // it the watches are due.
    guest.thread.watch.exec();
    assert_eq!(guest.status(), WatchStatus::Pending);
    guest.answers.push_back(Ok(()));
    crate::watch::rearm_after_exec(&mut guest);
    assert_eq!(guest.status(), WatchStatus::Armed);
    assert_eq!(guest.armed.len(), 2);

    // A thread never told the watches are due does not arm at exec.
    let mut fresh = WatchGuest::new(vec![]);
    fresh.thread.watch.exec();
    crate::watch::rearm_after_exec(&mut fresh);
    assert!(fresh.armed.is_empty());
    assert_eq!(fresh.status(), WatchStatus::Pending);
}

/// A thread whose hits could move a timer event is refused before anything
/// is armed, and so is one whose watches cannot be written.
#[test]
fn a_watch_that_could_move_the_schedule_or_cannot_be_written_is_refused() {
    let mut guest = WatchGuest::new(vec![]);
    guest.keeps_timer_events = false;
    let refusal = try_adopt(&mut guest, DUE).unwrap_err();
    assert!(
        refusal.contains("could move its next preemption")
            && refusal.contains(&format!("thread {TID}")),
        "{refusal}"
    );
    assert!(guest.armed.is_empty());

    let mut guest = WatchGuest::new(vec![Err(WatchError::TooMany {
        requested: 2,
        slots: 1,
    })]);
    let refusal = try_adopt(&mut guest, DUE).unwrap_err();
    assert!(refusal.contains("could not arm its watches"), "{refusal}");
}
