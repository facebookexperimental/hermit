/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `--replay-edited-preemptions` through Detcore's own post-hook and
//! slice-ending path, with scheduler grants the harness answers.
//!
//! The mutation it catches: the post-hook keeps the margin and the clock
//! multiplier from before a chaos slowdown the new slice installed.

use std::sync::Mutex;
use std::time::Duration;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::TimerSchedule;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::SyscallInfo;

use crate::CheckIn;
use crate::Config;
use crate::Detcore;
use crate::GlobalState;
use crate::ThreadState;
use crate::preemptions::ThreadHistory;
use crate::resources::ChaosEpochTransition;
use crate::scheduler::runqueue::DEFAULT_PRIORITY;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::tool_global::ResumeStatus;
use crate::types::DetPid;
use crate::types::RcbTimeMultiplier;

const THREAD: i32 = 5;

/// A guest thread whose scheduler grants the harness answers (an ordinary
/// grant leaves the thread's clock where it was), and that logs its requests
/// and timers.
struct GrantGuest {
    config: Config,
    thread: ThreadState<()>,
    requests: Mutex<usize>,
    /// The RCB timers set, in order.
    timers: Vec<u64>,
}

impl GrantGuest {
    fn timer(&mut self, schedule: TimerSchedule) -> Result<(), Error> {
        let TimerSchedule::Rcbs(rcbs) = schedule else {
            panic!("ending a slice sets only RCB timers");
        };
        self.timers.push(rcbs);
        Ok(())
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
        panic!("ending a slice needs no scratch stack")
    }
    fn capacity(&self) -> usize {
        panic!("ending a slice needs no scratch stack")
    }
    fn push<'a, T>(&mut self, _: T) -> Addr<'a, T> {
        panic!("ending a slice needs no scratch stack")
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("ending a slice needs no scratch stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("ending a slice needs no scratch stack")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for GrantGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        match message.2 {
            GlobalRequest::RequestResources(..) => {
                *self.requests.lock().unwrap() += 1;
                (None, GlobalResponse::RequestResources(ResumeStatus::Normal))
            }
            request => panic!("ending a slice sent an unexpected RPC: {request:?}"),
        }
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for GrantGuest {
    type Memory = LocalMemory;
    type Stack = UnusedStack;
    fn pid(&self) -> Pid {
        Pid::from_raw(THREAD)
    }
    fn tid(&self) -> Pid {
        Pid::from_raw(THREAD)
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
        panic!("ending a slice needs no registers")
    }
    async fn stack(&mut self) -> UnusedStack {
        panic!("ending a slice needs no scratch stack")
    }
    async fn daemonize(&mut self) {
        panic!("ending a slice must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        panic!("ending a slice must not inject: {:?}", call.number())
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("ending a slice must not tail-inject")
    }
    fn set_timer(&mut self, schedule: TimerSchedule) -> Result<(), Error> {
        self.timer(schedule)
    }
    fn set_timer_precise(&mut self, schedule: TimerSchedule) -> Result<(), Error> {
        self.timer(schedule)
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        Ok(self.thread.committed_clock_value)
    }
}

fn edited_config() -> Config {
    Config {
        sequentialize_threads: true,
        target_timeslice: std::num::NonZeroU64::new(10_000),
        max_timeslice: std::num::NonZeroU64::new(100_000),
        replay_preemptions_from: Some("edited.preempts".into()),
        replay_edited_preemptions: true,
        ..Config::default()
    }
}

/// Detcore and a thread replaying `history`, at the start of its run.
fn setup(config: Config, history: ThreadHistory) -> (Detcore, GrantGuest) {
    let mut thread = ThreadState::new(DetPid::from_raw(THREAD), &config, ());
    thread.detpid = Some(DetPid::from_raw(THREAD));
    thread.preemption_points = Some(history.into_iter());
    let tool = <Detcore as Tool>::new(Pid::from_raw(THREAD), &config);
    (
        tool,
        GrantGuest {
            config,
            thread,
            requests: Mutex::new(0),
            timers: Vec::new(),
        },
    )
}

/// The post-hook ends a slice whose maximum is within one RCB, 20 ns at a
/// 2 times slowdown, though its end has not passed. Ending it installs the
/// recorded epoch of the next slice, at 1 times, and keeps that slice, 18 ns
/// ahead: past one RCB at the new slowdown but within one at the old. The
/// post-hook checks it against the new margin and programs the timer at the
/// new slowdown (one RCB). With the margin and multiplier from before the
/// slice ended it panicked ("Ended time slice, but current time ... is
/// still beyond PMU maximum").
#[tokio::test(flavor = "current_thread")]
async fn the_post_hook_checks_a_kept_slice_at_the_slowdown_it_installed() {
    let config =
        edited_config().with_backend(|backend| backend.delivers_preemption_timer_events = true);
    assert!(config.use_preemption_timer());
    let start = ThreadState::new(DetPid::from_raw(THREAD), &config, ())
        .thread_logical_time
        .as_nanos();
    let at = |ns: u64| start + Duration::from_nanos(ns);
    let history = ThreadHistory::new()
        .with_prio_changes(vec![
            (at(515), DEFAULT_PRIORITY + 1),
            (at(518), DEFAULT_PRIORITY + 2),
            (at(5000), DEFAULT_PRIORITY + 3),
        ])
        .with_chaos_epochs(vec![
            ChaosEpochTransition {
                logical_time: at(0),
                epoch: 0,
                factor: RcbTimeMultiplier::from_f64(2.0),
            },
            ChaosEpochTransition {
                logical_time: at(515),
                epoch: 1,
                factor: RcbTimeMultiplier::ONE,
            },
        ]);
    let (tool, mut guest) = setup(config, history);
    // The slice being ended, chosen at the start as a replay would: 2 times
    // slowdown, ending at 515 ns. The thread then runs to 500 ns.
    let thread = &mut guest.thread;
    assert_eq!(
        thread.next_timeslice(&guest.config),
        Some(DEFAULT_PRIORITY + 1)
    );
    assert_eq!(thread.end_of_timeslice, Some(at(515)));
    assert_eq!(thread.max_timeslice_end, Some(at(515)));
    thread
        .thread_logical_time
        .add_rcbs_with_multiplier(50, RcbTimeMultiplier::ONE);
    assert_eq!(thread.thread_logical_time.as_nanos(), at(500));
    assert!(!thread.timeslice_expired());
    assert_eq!(thread.pmu_epsilon(&guest.config), Duration::from_nanos(20));

    tool.post_handler_hook(&mut guest, CheckIn::Event).await;

    assert_eq!(*guest.requests.lock().unwrap(), 1);
    assert_eq!(guest.thread.chaos_slowdown_factor, RcbTimeMultiplier::ONE);
    assert_eq!(guest.thread.max_timeslice_end, Some(at(518)));
    assert_eq!(guest.timers, [1]);
}
