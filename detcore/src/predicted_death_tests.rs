/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `Tool::exit_for_predicted_death` through Detcore's implementation, against
//! a real scheduler: the dying thread keeps its turn, then files its group's
//! `Exit`, so a peer that was already runnable takes no turn before that
//! `Exit` is granted (A5 design 16.30).
//!
//! The mutation it catches: the hold removed, which lets the runnable peer
//! go first.

use std::sync::Mutex;

use reverie::Errno;
use reverie::Error;
use reverie::ExitStatus;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::SyscallInfo;

use crate::Config;
use crate::Detcore;
use crate::GlobalState;
use crate::ThreadState;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::Scheduler;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::tool_global::ResumeStatus;
use crate::types::DetPid;
use crate::types::DetTid;

/// The dying thread, and a peer already queued with a request.
const DECIDER: DetTid = DetTid::from_raw(43);
const PEER: DetTid = DetTid::from_raw(41);

/// What the dying thread sent, in order.
#[derive(Debug, PartialEq, Eq)]
enum Sent {
    Hold(DetTid),
    Request(Vec<ResourceID>),
}

/// The dying thread, whose RPCs reach a real scheduler: a hold is answered
/// by it, and a resource request is filed in it.
struct DyingGuest {
    config: Config,
    thread: ThreadState<()>,
    scheduler: Mutex<Scheduler>,
    sent: Mutex<Vec<Sent>>,
}

struct UnusedStack;
struct UnusedStackGuard;
impl Drop for UnusedStackGuard {
    fn drop(&mut self) {}
}
impl Stack for UnusedStack {
    type StackGuard = UnusedStackGuard;
    fn size(&self) -> usize {
        panic!("a predicted death needs no scratch stack")
    }
    fn capacity(&self) -> usize {
        panic!("a predicted death needs no scratch stack")
    }
    fn push<'a, T>(&mut self, _: T) -> Addr<'a, T> {
        panic!("a predicted death needs no scratch stack")
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("a predicted death needs no scratch stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("a predicted death needs no scratch stack")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for DyingGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let mut scheduler = self.scheduler.lock().unwrap();
        let response = match message.2 {
            GlobalRequest::HoldTurnForGroupExit(dettid) => {
                self.sent.lock().unwrap().push(Sent::Hold(dettid));
                GlobalResponse::HoldTurnForGroupExit(scheduler.hold_turn_for_group_exit(dettid))
            }
            GlobalRequest::RequestResources(request, _) => {
                self.sent
                    .lock()
                    .unwrap()
                    .push(Sent::Request(request.resources.keys().cloned().collect()));
                scheduler.queue_request_for_test(request.tid, request);
                GlobalResponse::RequestResources(ResumeStatus::Normal)
            }
            request => panic!("a predicted death sent an unexpected RPC: {request:?}"),
        };
        (None, response)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for DyingGuest {
    type Memory = LocalMemory;
    type Stack = UnusedStack;
    fn pid(&self) -> Pid {
        Pid::from_raw(DECIDER.as_raw())
    }
    fn tid(&self) -> Pid {
        Pid::from_raw(DECIDER.as_raw())
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
        panic!("a predicted death needs no registers")
    }
    async fn stack(&mut self) -> UnusedStack {
        panic!("a predicted death needs no scratch stack")
    }
    async fn daemonize(&mut self) {
        panic!("a predicted death must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        panic!("a predicted death must not inject: {:?}", call.number())
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("a predicted death must not tail-inject")
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

/// Detcore and the dying thread of an in-guest run under the delivery gate
/// (`gate`), with a peer queued ahead of it, and the dying thread holding
/// the serial grant when `holds_turn`.
fn setup(gate: bool, holds_turn: bool) -> (Detcore, DyingGuest) {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: gate,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    let mut thread = ThreadState::new(DetPid::from_raw(DECIDER.as_raw()), &config, ());
    thread.detpid = Some(DetPid::from_raw(DECIDER.as_raw()));
    let mut scheduler = Scheduler::new(&config);
    for dettid in [PEER, DECIDER] {
        scheduler.register_thread_for_test(dettid);
    }
    scheduler.queue_request_for_test(PEER, Resources::new(PEER));
    if holds_turn {
        scheduler.set_running_for_test(DECIDER);
    }
    let tool = <Detcore as Tool>::new(Pid::from_raw(DECIDER.as_raw()), &config);
    (
        tool,
        DyingGuest {
            config,
            thread,
            scheduler: Mutex::new(scheduler),
            sent: Mutex::new(Vec::new()),
        },
    )
}

fn group_exit(guest: &DyingGuest) -> ResourceID {
    ResourceID::Exit {
        group: true,
        process: DetPid::from_raw(DECIDER.as_raw()),
        mm: guest.thread.mm_id,
    }
}

/// The dying thread holds its turn, then files its group's `Exit`, and the
/// scheduler's next turn is that `Exit`, ahead of the peer queued before it.
#[tokio::test]
async fn a_predicted_death_is_granted_before_a_runnable_peer() {
    for status in [
        ExitStatus::signaled(libc::SIGTERM, false),
        ExitStatus::signaled(libc::SIGQUIT, false),
        ExitStatus::signaled(40, false),
    ] {
        let (tool, mut guest) = setup(true, true);
        assert_eq!(
            tool.exit_for_predicted_death(&mut guest, status).await,
            Ok(()),
            "{status:?}"
        );
        assert_eq!(
            *guest.sent.lock().unwrap(),
            vec![Sent::Hold(DECIDER), Sent::Request(vec![group_exit(&guest)])],
            "{status:?}"
        );
        let (chosen, ..) = guest
            .scheduler
            .lock()
            .unwrap()
            .select_test_turn()
            .expect("a turn is ready");
        assert_eq!(
            chosen, DECIDER,
            "{status:?}: the runnable peer went before the group exit"
        );
    }
}

/// Without the gate, the run does not take in-guest delivery: refused
/// before anything is sent.
#[tokio::test]
async fn a_predicted_death_outside_the_gate_is_refused_before_any_request() {
    let (tool, mut guest) = setup(false, true);
    assert_eq!(
        tool.exit_for_predicted_death(&mut guest, ExitStatus::signaled(libc::SIGTERM, false))
            .await,
        Err(Errno::EPROTONOSUPPORT)
    );
    assert!(guest.sent.lock().unwrap().is_empty());
}

/// A thread that does not hold the turn cannot protect the decision: the
/// hold is refused, and no `Exit` is filed.
#[tokio::test]
async fn a_predicted_death_without_the_turn_files_no_exit() {
    let (tool, mut guest) = setup(true, false);
    assert_eq!(
        tool.exit_for_predicted_death(&mut guest, ExitStatus::signaled(libc::SIGTERM, false))
            .await,
        Err(Errno::EPROTONOSUPPORT)
    );
    assert_eq!(*guest.sent.lock().unwrap(), vec![Sent::Hold(DECIDER)]);
    let (chosen, ..) = guest.scheduler.lock().unwrap().select_test_turn().unwrap();
    assert_eq!(chosen, PEER);
}

/// A status that is not a death by a signal is not a predicted death.
#[tokio::test]
async fn an_exit_status_is_not_a_predicted_death() {
    let (tool, mut guest) = setup(true, true);
    assert_eq!(
        tool.exit_for_predicted_death(&mut guest, ExitStatus::Exited(0))
            .await,
        Err(Errno::EINVAL)
    );
    assert!(guest.sent.lock().unwrap().is_empty());
}
