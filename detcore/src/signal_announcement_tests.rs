/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The two-phase signal delivery (`Tool::announce_signal_event`,
//! `Tool::commit_signal_event`, `Tool::withdraw_signal_event`), through
//! Detcore's own implementation: the announcement takes the delivery's turn,
//! the commit takes none, and an announcement settles once.

use std::cell::Cell;
use std::cell::RefCell;
use std::sync::Mutex;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::SignalAnnouncement;
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
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::types::DetPid;

thread_local! {
    /// The parts of a delivery Detcore ran on this thread, in order
    /// (`Detcore::note_test_phase`).
    static PHASES: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    /// Whether the next delivered part grants the thread a turn, as a part
    /// filing a scheduler request would.
    static GRANT_DURING_COMMIT: Cell<bool> = const { Cell::new(false) };
    /// Whether the next delivery check grants the thread a turn, as a
    /// `Guest::deliver_signal` that waited for one would.
    static GRANT_DURING_CHECK: Cell<bool> = const { Cell::new(false) };
}

/// Records that Detcore ran `phase` of a delivery on this thread.
pub(crate) fn note_phase(phase: &'static str) {
    PHASES.with(|phases| phases.borrow_mut().push(phase));
}

/// Counts a scheduler grant in `thread`, once, if a test asked for one.
pub(crate) fn grant_during_commit_if_asked<T>(thread: &mut ThreadState<T>) {
    if GRANT_DURING_COMMIT.with(|ask| ask.replace(false)) {
        thread.scheduler_grants += 1;
    }
}

/// Counts a scheduler grant in `thread`, once, if a test asked for one during
/// the delivery check.
pub(crate) fn grant_during_check_if_asked<T>(thread: &mut ThreadState<T>) {
    if GRANT_DURING_CHECK.with(|ask| ask.replace(false)) {
        thread.scheduler_grants += 1;
    }
}

fn take_phases() -> Vec<&'static str> {
    PHASES.with(|phases| std::mem::take(&mut *phases.borrow_mut()))
}

/// A thread whose scheduler grants every request at once and records, per
/// RPC, whether it was a resource request and for which resources.
struct AnnounceGuest {
    config: Config,
    thread: ThreadState<()>,
    requests: Mutex<Vec<Vec<ResourceID>>>,
    continues: Mutex<Vec<bool>>,
}

struct UnusedStack;
struct UnusedStackGuard;
impl Drop for UnusedStackGuard {
    fn drop(&mut self) {}
}
impl Stack for UnusedStack {
    type StackGuard = UnusedStackGuard;
    fn size(&self) -> usize {
        panic!("a delivery needs no scratch stack")
    }
    fn capacity(&self) -> usize {
        panic!("a delivery needs no scratch stack")
    }
    fn push<'a, T>(&mut self, _: T) -> Addr<'a, T> {
        panic!("a delivery needs no scratch stack")
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("a delivery needs no scratch stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("a delivery needs no scratch stack")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for AnnounceGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let response = match message.2 {
            GlobalRequest::RequestResources(request, _) => {
                self.requests
                    .lock()
                    .unwrap()
                    .push(request.resources.keys().cloned().collect());
                self.continues
                    .lock()
                    .unwrap()
                    .push(request.continues_sigreturn);
                GlobalResponse::RequestResources(crate::tool_global::ResumeStatus::Normal)
            }
            request => panic!("a delivery of SIGUSR1 sent an unexpected RPC: {request:?}"),
        };
        (None, response)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for AnnounceGuest {
    type Memory = LocalMemory;
    type Stack = UnusedStack;
    // The test thread itself, so that what Detcore reads of the kernel's
    // signal state (`delivery_runs_handler`) is a real thread's: this one,
    // which catches neither SIGUSR1 nor SIGUSR2 nor signal 40.
    fn pid(&self) -> Pid {
        Pid::from_raw(std::process::id() as i32)
    }
    fn tid(&self) -> Pid {
        Pid::from_raw(unsafe { libc::gettid() })
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
        panic!("a delivery needs no registers")
    }
    async fn stack(&mut self) -> UnusedStack {
        panic!("a delivery needs no scratch stack")
    }
    async fn daemonize(&mut self) {
        panic!("a delivery must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        panic!("a delivery must not inject: {:?}", call.number())
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("a delivery must not tail-inject")
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

/// Detcore and a thread of the in-guest backend, with no timeslice, as it runs.
fn setup() -> (Detcore, AnnounceGuest) {
    setup_with(|_| {})
}

fn setup_with(adjust: impl FnOnce(&mut Config)) -> (Detcore, AnnounceGuest) {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        ..Config::default()
    };
    // The in-guest backend's capabilities, the backend this protocol is for.
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    adjust(&mut config);
    // The test thread's own IDs (see `AnnounceGuest::tid`).
    let pid = std::process::id() as i32;
    let tid = unsafe { libc::gettid() };
    let mut thread = ThreadState::new(DetPid::from_raw(tid), &config, ());
    thread.detpid = Some(DetPid::from_raw(pid));
    let tool = <Detcore as Tool>::new(Pid::from_raw(tid), &config);
    (
        tool,
        AnnounceGuest {
            config,
            thread,
            requests: Mutex::new(Vec::new()),
            continues: Mutex::new(Vec::new()),
        },
    )
}

fn turns(guest: &AnnounceGuest) -> Vec<Vec<ResourceID>> {
    guest.requests.lock().unwrap().clone()
}

async fn announce(tool: &Detcore, guest: &mut AnnounceGuest, signal: i32) -> SignalAnnouncement {
    tool.announce_signal_event(guest, signal)
        .await
        .unwrap()
        .expect("Detcore never suppresses at an announcement")
}

/// The announcement files the delivery's `InboundSignal` request and does
/// nothing only a delivery justifies; the commit files no request, does that
/// (the sender's hold for the signal ends, the delivery is counted); the
/// announcement is then settled, so a second commit of it is `EINVAL`.
#[tokio::test]
async fn announce_takes_the_turn_and_commit_delivers_without_one() {
    let (tool, mut guest) = setup();
    guest.thread.own_signal_turn_hold = Some(libc::SIGUSR1);
    let count = guest.thread.stats.signal_count;
    let announcement = announce(&tool, &mut guest, libc::SIGUSR1).await;
    assert_eq!(announcement.signal(), libc::SIGUSR1);
    let after_announce = turns(&guest);
    assert!(
        after_announce
            .iter()
            .any(|resources| resources.contains(&ResourceID::InboundSignal(
                crate::types::SigWrapper(libc::SIGUSR1)
            ))),
        "the announcement took no InboundSignal turn: {after_announce:?}"
    );
    assert_eq!(guest.thread.signal_announcements.len(), 1);
    assert_eq!(guest.thread.own_signal_turn_hold, Some(libc::SIGUSR1));
    assert_eq!(guest.thread.stats.signal_count, count);
    assert!(!guest.thread.delivered_since_request);

    tool.commit_signal_event(&mut guest, announcement)
        .await
        .unwrap();
    assert_eq!(
        turns(&guest),
        after_announce,
        "the commit filed a scheduler request"
    );
    assert!(guest.thread.signal_announcements.is_empty());
    assert_eq!(guest.thread.own_signal_turn_hold, None);
    assert_eq!(guest.thread.stats.signal_count, count + 1);
    // This thread does not catch SIGUSR1, so no handler continues.
    assert!(!guest.thread.delivered_since_request);
    assert_eq!(
        tool.commit_signal_event(&mut guest, announcement).await,
        Err(Errno::EINVAL)
    );
}

/// A withdrawn announcement is settled: it can no longer be committed,
/// nothing only a delivery justifies happened for it, and withdrawing it
/// touches no other announcement.
#[tokio::test]
async fn withdraw_settles_only_its_own_announcement() {
    let (tool, mut guest) = setup();
    let first = announce(&tool, &mut guest, libc::SIGUSR1).await;
    let second = announce(&tool, &mut guest, libc::SIGUSR2).await;
    assert_ne!(first.token(), second.token());
    guest.thread.own_signal_turn_hold = Some(libc::SIGUSR1);
    let count = guest.thread.stats.signal_count;
    // A withdrawal with the right token but another signal is not this one.
    tool.withdraw_signal_event(
        &mut guest.thread,
        SignalAnnouncement::new(libc::SIGUSR2, first.token()),
    );
    assert_eq!(guest.thread.signal_announcements.len(), 2);
    tool.withdraw_signal_event(&mut guest.thread, first);
    assert_eq!(guest.thread.signal_announcements.len(), 1);
    assert_eq!(guest.thread.own_signal_turn_hold, Some(libc::SIGUSR1));
    assert_eq!(guest.thread.stats.signal_count, count);
    assert_eq!(
        tool.commit_signal_event(&mut guest, first).await,
        Err(Errno::EINVAL)
    );
    tool.commit_signal_event(&mut guest, second).await.unwrap();
    assert!(guest.thread.signal_announcements.is_empty());
}

/// An announcement whose signal does not match its token's is not this
/// thread's: `EINVAL`, and nothing is settled.
#[tokio::test]
async fn a_mismatched_announcement_is_refused() {
    let (tool, mut guest) = setup();
    let held = announce(&tool, &mut guest, libc::SIGUSR1).await;
    let forged = SignalAnnouncement::new(libc::SIGUSR2, held.token());
    assert_eq!(
        tool.commit_signal_event(&mut guest, forged).await,
        Err(Errno::EINVAL)
    );
    assert_eq!(guest.thread.signal_announcements.len(), 1);
}

/// A realtime signal, which `Signal` cannot name, goes through both halves.
#[tokio::test]
async fn a_realtime_signal_is_announced_and_committed() {
    let (tool, mut guest) = setup();
    let announcement = announce(&tool, &mut guest, 40).await;
    tool.commit_signal_event(&mut guest, announcement)
        .await
        .unwrap();
    assert!(guest.thread.signal_announcements.is_empty());
}

/// A number that is not a signal is `EINVAL`, before any turn.
#[tokio::test]
async fn a_number_outside_1_to_64_is_refused_before_any_turn() {
    let (tool, mut guest) = setup();
    for signal in [0, 65, -1] {
        assert_eq!(
            tool.announce_signal_event(&mut guest, signal).await,
            Err(Errno::EINVAL)
        );
    }
    assert!(turns(&guest).is_empty());
    assert!(guest.thread.signal_announcements.is_empty());
}

/// Every configuration in which a commit could file a scheduler request, or
/// would read the thread's queues or the delivery's siginfo, is refused at
/// the announcement with `EPROTONOSUPPORT` (Detcore has opted in, so not
/// `EOPNOTSUPP`), before any turn.
#[tokio::test]
async fn configurations_the_commit_cannot_keep_are_refused_before_any_turn() {
    type Adjust = fn(&mut Config);
    let refused: [(&str, Adjust); 8] = [
        ("discarded-at-send record", |config| {
            config.backend.kernel_signal_targets_are_predictable = true;
        }),
        ("kernel child-exit signals", |config| {
            config.backend.reports_kernel_child_exit_signals = true;
        }),
        ("thread-directed process signals", |config| {
            config.backend.requires_thread_directed_process_signals = true;
        }),
        ("a maximum timeslice", |config| {
            config.max_timeslice = std::num::NonZeroU64::new(1_000_000);
        }),
        ("a target timeslice alone", |config| {
            config.target_timeslice = std::num::NonZeroU64::new(1_000_000);
        }),
        ("a recorded schedule", |config| {
            config.record_preemptions = true;
        }),
        ("a replayed schedule", |config| {
            config.replay_schedule_from = Some(std::path::PathBuf::from("/nonexistent/schedule"));
        }),
        ("pipe-wake latency", |config| {
            config.pipe_wake_latency = Some(1_000);
            assert!(config.pipe_wake_latency_in_effect().is_some());
        }),
    ];
    for (name, adjust) in refused {
        let (tool, mut guest) = setup_with(adjust);
        assert!(
            super::two_phase_refusal(&guest.config).is_some(),
            "{name}: not refused"
        );
        assert_eq!(
            tool.announce_signal_event(&mut guest, libc::SIGUSR1).await,
            Err(Errno::EPROTONOSUPPORT),
            "{name}"
        );
        assert!(turns(&guest).is_empty(), "{name}: a turn was taken");
    }
    let (_, guest) = setup();
    assert_eq!(super::two_phase_refusal(&guest.config), None);
}

/// A thread holding a claimed `SIGCHLD`, whose placement must come before its
/// delivery's turn, is refused before any turn, with `EPROTONOSUPPORT`.
#[tokio::test]
async fn a_claimed_sigchld_is_refused_before_any_turn() {
    let (tool, mut guest) = setup();
    guest.thread.claimed_sigchld = true;
    assert_eq!(
        tool.announce_signal_event(&mut guest, libc::SIGCHLD).await,
        Err(Errno::EPROTONOSUPPORT)
    );
    assert!(turns(&guest).is_empty());
    assert!(guest.thread.claimed_sigchld);
}

extern "C" fn ignore_signal(_: libc::c_int) {}

/// Two deliveries announced at one return continue as on ptrace: the second
/// announcement's request continues the first's turn when the first's
/// delivery runs a handler (ptrace's second stop finds the first's
/// `delivered_since_request`), and not when it does not.
#[tokio::test]
async fn a_second_announcement_continues_a_first_whose_handler_runs() {
    // The test process catches SIGURG (whose default is to ignore it, so
    // nothing else in the test binary depends on its action), and catches
    // neither SIGUSR1 nor SIGUSR2.
    let action = libc::sigaction {
        sa_sigaction: ignore_signal as *const () as libc::sighandler_t,
        sa_mask: unsafe { std::mem::zeroed() },
        sa_flags: 0,
        sa_restorer: None,
    };
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGURG, &action, std::ptr::null_mut()) },
        0
    );
    let (tool, mut guest) = setup();
    let first = announce(&tool, &mut guest, libc::SIGURG).await;
    announce(&tool, &mut guest, libc::SIGUSR1).await;
    assert_eq!(*guest.continues.lock().unwrap(), [false, true]);
    tool.withdraw_signal_event(&mut guest.thread, first);
    announce(&tool, &mut guest, libc::SIGUSR2).await;
    // The outstanding SIGUSR1 runs no handler, so SIGUSR2 continues nothing.
    assert_eq!(*guest.continues.lock().unwrap(), [false, true, false]);
}

/// The commit runs the delivery's parts in ptrace's order: what the dequeue
/// settles, what the granted delivery settles, the delivery check, then the
/// record. The announcement runs none of them.
#[tokio::test]
async fn commit_runs_the_dequeue_the_delivery_and_the_record_in_order() {
    let (tool, mut guest) = setup();
    take_phases();
    let announcement = announce(&tool, &mut guest, libc::SIGUSR1).await;
    assert_eq!(take_phases(), Vec::<&str>::new());
    tool.commit_signal_event(&mut guest, announcement)
        .await
        .unwrap();
    assert_eq!(
        take_phases(),
        ["dequeued", "delivered", "checked", "recorded"]
    );
}

/// A commit during which the thread is granted a turn fails closed with
/// `EDEADLK` before the delivery is recorded: the backend's final read no
/// longer holds.
#[tokio::test]
async fn a_turn_granted_during_a_commit_fails_it_before_the_record() {
    let (tool, mut guest) = setup();
    let announcement = announce(&tool, &mut guest, libc::SIGUSR1).await;
    take_phases();
    GRANT_DURING_COMMIT.with(|ask| ask.set(true));
    assert_eq!(
        tool.commit_signal_event(&mut guest, announcement).await,
        Err(Errno::EDEADLK)
    );
    assert_eq!(take_phases(), ["dequeued", "delivered"]);
    assert!(guest.thread.signal_announcements.is_empty());
}

/// A turn granted during the delivery check (as a `Guest::deliver_signal`
/// that waited for one would) fails the commit with `EDEADLK` before the
/// delivery is recorded.
///
/// The grant is simulated: in the configurations a commit runs in today the
/// kernel's signal state is not the guest's, so the check accepts the delivery
/// without calling `Guest::deliver_signal`, and nothing reaches it from a
/// commit until in-guest Detcore keeps that copy (A5 3c.0).
#[tokio::test]
async fn a_turn_granted_during_the_check_fails_the_commit_before_the_record() {
    let (tool, mut guest) = setup();
    let announcement = announce(&tool, &mut guest, libc::SIGUSR1).await;
    take_phases();
    GRANT_DURING_CHECK.with(|ask| ask.set(true));
    assert_eq!(
        tool.commit_signal_event(&mut guest, announcement).await,
        Err(Errno::EDEADLK)
    );
    assert_eq!(take_phases(), ["dequeued", "delivered", "checked"]);
    assert!(guest.thread.signal_announcements.is_empty());
}
