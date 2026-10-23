/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1b: under the in-guest delivery gate
//! (`Config::in_guest_signal_delivery`), every request a thread files
//! publishes the guest's signal mask (`Resources::guest_signal_mask`), and
//! the scheduler records it (`Scheduler::record_request_guest_mask`), so
//! that the scheduler can read the guest's mask while the kernel's `SigBlk`
//! is the in-guest runtime's (design 18.16.1 section 6,
//! <https://github.com/rrnewton/hermit/issues/4485>).
//!
//! The gate is forced on in the Detcore `Config`; nothing in Hermit sets it
//! before step 3c.1f. The test thread plays a guest thread "before the
//! runtime mask exists": the backend's record of the guest's mask
//! (`Guest::resume_signal_mask`) is this thread's own kernel mask, which each
//! test first sets to a distinctive value. So the mask a request publishes for
//! a thread sleeping under its own mask must equal the `SigBlk` that
//! `/proc/thread-self/status` reports at that same request, and for a
//! temporary-mask call it must be the call's temporary mask.
//!
//! One parent per state of the table in design 18.16 ("the effective mask,
//! per state of `t`"), each filed through the Detcore function its handler
//! uses where that function is reachable from a unit test:
//! - internal poll (`InternalIOPolling` retry; a polled scalar read lent to
//!   the parked protocol), and a signalfd read run outside the schedule: own;
//! - `ppoll`/`pselect6`/`epoll_pwait` with a mask: the masked `pselect6` that
//!   files `BlockingExternalIO` with its temporary mask. The internally polled
//!   masked forms publish through `publish_wait_mask` (tested below) and are
//!   step 3c.1d's (D39, D50);
//! - `rt_sigtimedwait`: step 3c.3's (it publishes the pre-call mask), not
//!   here;
//! - `wait4`/`waitid` parked (`wait_for_child_lifecycle`): own;
//! - `pause` and `nanosleep` (`parked_wait_request`, parked and not): own. A
//!   futex wait files no `Resources` request (its `FutexAction::WaitRequest`
//!   carries its own `FutexSignalWatch`), so it is reader 9 of D50, step
//!   3c.1c;
//! - `rt_sigsuspend` filed: the temporary mask;
//! - an out-of-schedule external call: own, at the call's request;
//! - ready, request filed (a yield): own;
//! - readmitted before the continuation turn: the temporary mask, or own once
//!   the call restored it;
//!
//! plus two states 18.16.1 section 2 adds: a vfork parent (own) and a
//! delivery in flight (`InboundSignal`, own).
//!
//! Each assertion names the mutation it catches:
//! - "publishing None": no request publishes, and every row fails;
//! - "the own mask for a temporary-mask call": the temporary-mask rows fail;
//! - "publication at parks only": the rows filed with `RequestResources`
//!   fail;
//! - "the virtual SIGALRM bit dropped": `phase1_publishes_the_virtual_sigalrm_bit`
//!   fails.

use std::sync::Mutex;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::SyscallInfo;

use super::*;
use crate::ThreadState;
use crate::scheduler::Scheduler;
use crate::scheduler::parked::ParkedWaitPolicy;
use crate::tool_global::ResourceReply;
use crate::tool_global::parked_wait_request;
use crate::tool_global::polled_read_request;
use crate::tool_global::resource_request;
use crate::tool_global::wait_for_child_lifecycle;
use crate::types::ChildWaitExitClass;
use crate::types::ChildWaitSelector;
use crate::types::ChildWaitSpec;

/// The guest thread, as the scheduler numbers it.
const THREAD: DetTid = DetTid::from_raw(23);

/// The signals the test thread blocks before each case: SIGUSR2, SIGWINCH
/// and SIGRTMIN+2, none of which the test harness itself blocks.
fn distinctive_signals() -> [libc::c_int; 3] {
    [libc::SIGUSR2, libc::SIGWINCH, libc::SIGRTMIN() + 2]
}

/// A temporary mask unlike the thread's own: SIGUSR1, SIGTERM, SIGCHLD.
const TEMPORARY: u64 =
    (1 << (libc::SIGUSR1 - 1)) | (1 << (libc::SIGTERM - 1)) | (1 << (libc::SIGCHLD - 1));

const SIGALRM_BIT: u64 = 1 << (libc::SIGALRM - 1);

/// The calling thread's kernel mask, bit `n - 1` for signal `n`.
fn current_thread_mask() -> u64 {
    let mut current: libc::sigset_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut current) };
    assert_eq!(rc, 0);
    // glibc's `sigset_t` holds signals 1 to 64 in its first word.
    unsafe { *(&current as *const libc::sigset_t).cast::<u64>() }
}

/// `SigBlk` of the calling thread, as `/proc` reports it.
fn proc_sigblk() -> u64 {
    let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("SigBlk:"))
        .expect("a SigBlk line");
    u64::from_str_radix(line.trim(), 16).unwrap()
}

/// Sets the calling thread's mask to the distinctive one plus `extra`, and
/// puts the previous mask back when dropped.
struct MaskGuard(libc::sigset_t);

impl MaskGuard {
    fn block(extra: &[libc::c_int]) -> MaskGuard {
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut set) };
        for signal in distinctive_signals().iter().chain(extra) {
            unsafe { libc::sigaddset(&mut set, *signal) };
        }
        let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &set, &mut old) };
        assert_eq!(rc, 0);
        MaskGuard(old)
    }
}

impl Drop for MaskGuard {
    fn drop(&mut self) {
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut()) };
    }
}

/// What the thread filed with one request.
#[derive(Debug, Clone)]
struct Filed {
    fyi: String,
    /// Filed as a `ParkedRequest` (the parked protocol), not as an ordinary
    /// `RequestResources`.
    parked: bool,
    /// The mask the request published.
    published: Option<u64>,
    /// `SigBlk` read while the request was being filed.
    sigblk: u64,
    /// What the scheduler held for the thread once it had taken the request.
    recorded: Option<u64>,
    /// A rejoin (`Resources::is_rejoin`), which the scheduler records when it
    /// readmits the thread, not when it is filed.
    rejoin: bool,
}

/// A guest thread whose requests reach a real scheduler, which records the
/// mask each one publishes, and are granted at once.
struct PublishingGuest {
    config: Config,
    thread: ThreadState<()>,
    scheduler: Mutex<Scheduler>,
    /// The parked-signal site the backend reports, when it has one.
    site: Option<reverie::CallbackSignalSite>,
    /// Whether the backend keeps the guest's mask apart from the kernel's
    /// (`Guest::resume_signal_mask` is `Some`). Before step 3c.1e none does.
    keeps_guest_mask: bool,
    /// The guest mask the backend records, in place of the thread's kernel
    /// mask: what a runtime with a mask of its own would report.
    resume_override: Option<u64>,
    filed: Mutex<Vec<Filed>>,
    /// The masks `publish_wait_mask` sent (`SetSavedGuestSigmask`).
    wait_masks: Mutex<Vec<Option<u64>>>,
}

struct UnusedStack;
struct UnusedStackGuard;
impl Drop for UnusedStackGuard {
    fn drop(&mut self) {}
}
impl Stack for UnusedStack {
    type StackGuard = UnusedStackGuard;
    fn size(&self) -> usize {
        panic!("a request needs no scratch stack")
    }
    fn capacity(&self) -> usize {
        panic!("a request needs no scratch stack")
    }
    fn push<'a, T>(&mut self, _: T) -> Addr<'a, T> {
        panic!("a request needs no scratch stack")
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("a request needs no scratch stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("a request needs no scratch stack")
    }
}

impl PublishingGuest {
    fn file(&self, resources: &Resources, parked: bool) {
        let sigblk = proc_sigblk();
        let mut scheduler = self.scheduler.lock().unwrap();
        scheduler.record_request_guest_mask(resources.tid, resources);
        self.filed.lock().unwrap().push(Filed {
            fyi: resources.fyi.clone(),
            parked,
            published: resources.guest_signal_mask,
            sigblk,
            recorded: scheduler.published_guest_sigmask(resources.tid),
            rejoin: resources.is_rejoin(),
        });
    }

    fn last(&self) -> Filed {
        self.filed
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a filed request")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for PublishingGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let response = match message.2 {
            GlobalRequest::RequestResources(resources, _) => {
                self.file(&resources, false);
                GlobalResponse::RequestResources(ResumeStatus::Normal)
            }
            GlobalRequest::ParkedRequest(resources, ..) => {
                self.file(&resources, true);
                GlobalResponse::ParkedRequest(ResourceReply::Grant(ResumeStatus::Normal))
            }
            GlobalRequest::SetSavedGuestSigmask(mask) => {
                self.wait_masks.lock().unwrap().push(mask);
                self.scheduler
                    .lock()
                    .unwrap()
                    .set_saved_guest_sigmask(THREAD, mask);
                GlobalResponse::SetSavedGuestSigmask(())
            }
            GlobalRequest::PublishedGuestSigmask(thread) => GlobalResponse::PublishedGuestSigmask(
                self.scheduler
                    .lock()
                    .unwrap()
                    .published_guest_sigmask(thread),
            ),
            request => panic!("a request sent an unexpected RPC: {request:?}"),
        };
        (None, response)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for PublishingGuest {
    type Memory = LocalMemory;
    type Stack = UnusedStack;
    fn pid(&self) -> Pid {
        Pid::from_raw(THREAD.as_raw())
    }
    fn tid(&self) -> Pid {
        Pid::from_raw(THREAD.as_raw())
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
    fn parked_signal_site(&self) -> Option<reverie::CallbackSignalSite> {
        self.site
    }
    fn polled_read_signal_site(
        &self,
        _: reverie::syscalls::Read,
    ) -> Option<reverie::CallbackSignalSite> {
        self.site
    }
    fn resume_signal_mask(&self) -> Option<u64> {
        // Before the runtime mask exists, the guest's mask is the kernel's.
        self.keeps_guest_mask
            .then(|| self.resume_override.unwrap_or_else(current_thread_mask))
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("a request needs no registers")
    }
    async fn stack(&mut self) -> UnusedStack {
        panic!("a request needs no scratch stack")
    }
    async fn daemonize(&mut self) {
        panic!("a request must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        panic!("a request must not inject: {:?}", call.number())
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("a request must not tail-inject")
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

/// An in-guest thread with the delivery gate `gate`, whose backend reports a
/// parked-signal site when `parked`.
fn publishing_guest(gate: bool, parked: bool) -> PublishingGuest {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: gate,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    let detpid = DetPid::from_raw(THREAD.as_raw());
    let mut thread = ThreadState::new(detpid, &config, ());
    thread.detpid = Some(detpid);
    let mut scheduler = Scheduler::new(&config);
    scheduler.register_thread_for_test(THREAD);
    let site = parked.then_some(reverie::CallbackSignalSite {
        process: reverie::SignalProcessId {
            tgid: Pid::from_raw(THREAD.as_raw()),
            generation: 1,
        },
        tid: Pid::from_raw(THREAD.as_raw()),
        task_generation: 1,
        callback_nonce: 1,
        boundary_nonce: 1,
    });
    PublishingGuest {
        config,
        thread,
        scheduler: Mutex::new(scheduler),
        site,
        keeps_guest_mask: true,
        resume_override: None,
        filed: Mutex::new(Vec::new()),
        wait_masks: Mutex::new(Vec::new()),
    }
}

fn request(rid: ResourceID, fyi: &str) -> Resources {
    let mut resources = Resources::new(THREAD);
    resources.insert(rid, Permission::RW);
    resources.fyi(fyi);
    resources
}

/// The masks a parent's request is expected to publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// The thread's own mask: `SigBlk` at the same request.
    Own,
    /// The call's temporary mask.
    Temporary,
}

/// One parent of 18.16's table, filed through the Detcore function its
/// handler uses, on `guest`.
async fn file_parent(guest: &mut PublishingGuest, parent: &str) -> Expect {
    let op = ExternalOpId::new(THREAD, 1);
    match parent {
        "internal poll" => {
            let mut poll = request(ResourceID::InternalIOPolling, "read");
            poll.poll_attempt = 1;
            resource_request(guest, poll).await;
            Expect::Own
        }
        "polled scalar read" => {
            let mut poll = request(ResourceID::InternalIOPolling, "read");
            poll.poll_attempt = 1;
            polled_read_request(guest, reverie::syscalls::Read::new(), poll).await;
            Expect::Own
        }
        "signalfd read outside the schedule" => {
            let mut read = request(ResourceID::BlockingExternalIO(op), "read");
            read.reads_signalfd = true;
            resource_request(guest, read).await;
            Expect::Own
        }
        "masked pselect6" => {
            let mut select = request(ResourceID::BlockingExternalIO(op), "pselect6");
            select.blocked_signal_mask = Some(TEMPORARY);
            resource_request(guest, select).await;
            Expect::Temporary
        }
        "wait4 parked" => {
            let spec = ChildWaitSpec {
                selector: ChildWaitSelector::Any,
                owner: None,
                exit_class: ChildWaitExitClass::Any,
            };
            wait_for_child_lifecycle(guest, spec).await;
            Expect::Own
        }
        "pause" => {
            let mut pause = request(ResourceID::SleepUntil(LogicalTime::INDEFINITE), "");
            pause.fyi(crate::resources::PAUSE_FYI);
            parked_wait_request(guest, pause, ParkedWaitPolicy::PauseNoHandlerRestart).await;
            Expect::Own
        }
        "nanosleep" => {
            let deadline = LogicalTime::from_nanos(5_000_000_000);
            let sleep = request(
                ResourceID::SleepUntil(deadline),
                crate::resources::NANOSLEEP_FYI,
            );
            parked_wait_request(
                guest,
                sleep,
                ParkedWaitPolicy::NanosleepNoHandlerRestart {
                    absolute_deadline: deadline,
                },
            )
            .await;
            Expect::Own
        }
        "rt_sigsuspend filed" => {
            let mut suspend = request(ResourceID::BlockingRtSigsuspend(op), "rt_sigsuspend");
            suspend.blocked_signal_mask = Some(TEMPORARY);
            resource_request(guest, suspend).await;
            Expect::Temporary
        }
        "out-of-schedule external call" => {
            resource_request(
                guest,
                request(ResourceID::BlockingExternalIO(op), "recvfrom"),
            )
            .await;
            Expect::Own
        }
        "ready, request filed" => {
            let detpid = DetPid::from_raw(THREAD.as_raw());
            resource_request(guest, request(ResourceID::MemAddrSpace(detpid), "ready")).await;
            Expect::Own
        }
        "readmitted under the temporary mask" => {
            let mut continuation =
                request(ResourceID::BlockedExternalContinue(op), "rt_sigsuspend");
            continuation.blocked_signal_mask = Some(TEMPORARY);
            resource_request(guest, continuation).await;
            Expect::Temporary
        }
        "readmitted with the mask restored" => {
            resource_request(
                guest,
                request(ResourceID::BlockedExternalContinue(op), "read"),
            )
            .await;
            Expect::Own
        }
        "vfork parent" => {
            resource_request(guest, request(ResourceID::BlockingVfork(op), "vfork")).await;
            Expect::Own
        }
        "delivery in flight" => {
            let inbound = request(
                ResourceID::InboundSignal(crate::types::SigWrapper(libc::SIGUSR1)),
                "inbound",
            );
            resource_request(guest, inbound).await;
            Expect::Own
        }
        other => panic!("no parent {other}"),
    }
}

const PARENTS: &[&str] = &[
    "internal poll",
    "polled scalar read",
    "signalfd read outside the schedule",
    "masked pselect6",
    "wait4 parked",
    "pause",
    "nanosleep",
    "rt_sigsuspend filed",
    "out-of-schedule external call",
    "ready, request filed",
    "readmitted under the temporary mask",
    "readmitted with the mask restored",
    "vfork parent",
    "delivery in flight",
];

/// Every parent publishes, at its request, the mask Linux's `blocked` holds
/// for it there, and the scheduler records it: its own mask, which before the
/// runtime mask exists is the `SigBlk` of that same request, or the call's
/// temporary mask. Run with the backend reporting a parked-signal site, so
/// that `pause`, `nanosleep` and the polled read go through the parked
/// protocol, and without one, so that they go through an ordinary request.
#[tokio::test]
async fn every_parent_publishes_its_mask_at_its_request() {
    let _mask = MaskGuard::block(&[]);
    let own = current_thread_mask();
    for signal in distinctive_signals() {
        assert_ne!(own & (1 << (signal - 1)), 0, "the distinctive mask is set");
    }
    assert_ne!(own, TEMPORARY);
    let mut parked_rows = 0;
    let mut ordinary_rows = 0;
    for parked in [true, false] {
        for parent in PARENTS {
            let mut guest = publishing_guest(true, parked);
            let expect = file_parent(&mut guest, parent).await;
            let filed = guest.last();
            let wanted = match expect {
                Expect::Own => {
                    // "before the runtime mask exists, the published mask
                    // equals SigBlk at the same request"
                    assert_eq!(filed.sigblk, own, "{parent}: SigBlk is the thread's mask");
                    filed.sigblk
                }
                Expect::Temporary => TEMPORARY,
            };
            // Mutations "publishing None", "the own mask for a temporary-mask
            // call", "publication at parks only".
            assert_eq!(
                filed.published,
                Some(wanted),
                "{parent} ({}; parked site: {parked}, filed as parked: {}) published {:?}, wanted {wanted:#x}",
                filed.fyi,
                filed.parked,
                filed.published
            );
            if filed.rejoin {
                // Filed at a host-timed moment: recorded when step 2c
                // readmits the thread, and what the readers see between
                // readmission and the continuation turn is checked by
                // scheduler::test::under_the_gate_a_rejoin_publishes_at_its_readmission_not_at_its_filing;
                // so a fresh scheduler still holds nothing here.
                assert_eq!(filed.recorded, None, "{parent}: not recorded at its filing");
            } else {
                assert_eq!(
                    filed.recorded,
                    Some(wanted),
                    "{parent}: the scheduler records it"
                );
            }
            if filed.parked {
                parked_rows += 1;
            } else {
                ordinary_rows += 1;
            }
        }
    }
    // Both kinds of request are covered: pause, nanosleep and the polled read
    // park when the backend reports a site.
    assert_eq!(parked_rows, 3, "{:?}", parked_rows);
    assert_eq!(ordinary_rows, 2 * PARENTS.len() - 3);
}

/// The same parents with the gate off publish nothing, and the scheduler
/// records nothing even for a request that carries a mask: a run without the
/// gate files exactly the requests it filed before step 3c.1b.
#[tokio::test]
async fn with_the_gate_off_no_parent_publishes() {
    let _mask = MaskGuard::block(&[]);
    for parked in [true, false] {
        for parent in PARENTS {
            let mut guest = publishing_guest(false, parked);
            file_parent(&mut guest, parent).await;
            let filed = guest.last();
            assert_eq!(filed.published, None, "{parent}");
            assert_eq!(filed.recorded, None, "{parent}");
        }
    }
    let guest = publishing_guest(false, false);
    let mut carrying = Resources::new(THREAD);
    carrying.guest_signal_mask = Some(TEMPORARY);
    let mut scheduler = guest.scheduler.lock().unwrap();
    assert!(!scheduler.record_request_guest_mask(THREAD, &carrying));
    assert_eq!(scheduler.published_guest_sigmask(THREAD), None);
}

/// A backend that keeps no guest mask apart (every backend before step
/// 3c.1e) publishes nothing for an own-mask parent, and a request that
/// publishes nothing leaves the scheduler holding none for the thread, not
/// the mask an earlier request published. A call whose temporary mask could
/// not be read publishes nothing either.
#[tokio::test]
async fn nothing_is_published_without_a_known_mask() {
    let _mask = MaskGuard::block(&[]);
    let mut guest = publishing_guest(true, false);
    guest.keeps_guest_mask = false;
    file_parent(&mut guest, "masked pselect6").await;
    assert_eq!(guest.last().published, Some(TEMPORARY));
    file_parent(&mut guest, "out-of-schedule external call").await;
    let filed = guest.last();
    assert_eq!(filed.published, None);
    assert_eq!(
        filed.recorded, None,
        "no mask an earlier request published is kept"
    );

    let mut guest = publishing_guest(true, false);
    let mut unknown = request(
        ResourceID::BlockingExternalIO(ExternalOpId::new(THREAD, 1)),
        "pselect6",
    );
    unknown.blocked_signal_mask_unknown = true;
    resource_request(&mut guest, unknown).await;
    assert_eq!(guest.last().published, None);
}

/// In a process whose SIGALRM is a signal-phase-1 guest handler, the
/// published SIGALRM bit is the runtime's virtual one
/// (`ThreadState::sigalrm_blocked_published`), not the kernel's, in both
/// directions; every other bit is the thread's mask. Mutation: "the virtual
/// SIGALRM bit dropped".
#[tokio::test]
async fn phase1_publishes_the_virtual_sigalrm_bit() {
    for (physically_blocked, virtually_blocked) in [(false, true), (true, false)] {
        let extra: &[libc::c_int] = if physically_blocked {
            &[libc::SIGALRM]
        } else {
            &[]
        };
        let _mask = MaskGuard::block(extra);
        let mut guest = publishing_guest(true, false);
        assert!(guest.config.backend.virtualizes_guest_sigalrm);
        guest.thread.sigalrm_handled = true;
        guest.thread.sigalrm_blocked_published = virtually_blocked;
        file_parent(&mut guest, "ready, request filed").await;
        let filed = guest.last();
        assert_eq!(filed.sigblk & SIGALRM_BIT != 0, physically_blocked);
        let virtual_bit = if virtually_blocked { SIGALRM_BIT } else { 0 };
        assert_eq!(
            filed.published,
            Some((filed.sigblk & !SIGALRM_BIT) | virtual_bit),
            "physically blocked {physically_blocked}, virtually blocked {virtually_blocked}"
        );
        assert_eq!(filed.recorded, filed.published);
    }
    // Outside a handling process the physical bit is published as it is.
    let _mask = MaskGuard::block(&[libc::SIGALRM]);
    let mut guest = publishing_guest(true, false);
    guest.thread.sigalrm_blocked_published = false;
    file_parent(&mut guest, "ready, request filed").await;
    let filed = guest.last();
    assert_eq!(filed.published, Some(filed.sigblk));
    assert_ne!(filed.sigblk & SIGALRM_BIT, 0);
}

/// `publish_wait_mask` publishes a wait's effective mask under the gate as
/// under signal-target modelling, and not with both off: a reader moved to
/// "or the gate".
#[tokio::test]
async fn publish_wait_mask_publishes_under_the_gate() {
    for gate in [true, false] {
        let mut guest = publishing_guest(gate, false);
        assert!(!guest.config.models_signal_targets());
        let mut published = crate::syscalls::helpers::PublishedWaitMask::default();
        crate::syscalls::helpers::publish_wait_mask(&mut guest, Some(TEMPORARY), &mut published)
            .await;
        crate::syscalls::helpers::publish_wait_mask(&mut guest, None, &mut published).await;
        let sent = guest.wait_masks.lock().unwrap().clone();
        if gate {
            assert_eq!(sent, vec![Some(TEMPORARY), None]);
        } else {
            assert!(sent.is_empty(), "{sent:?}");
        }
    }
}

/// Which configurations publish guest masks at all, and so report a published
/// `SigBlk` in `/proc` status: signal-target modelling, as before, and the
/// in-guest delivery gate, which is new.
#[test]
fn guest_masks_are_published_under_modelling_or_the_gate() {
    let mut in_guest = Config {
        sequentialize_threads: true,
        ..Config::default()
    };
    in_guest.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    assert!(!crate::syscalls::helpers::publishes_guest_masks(&in_guest));
    in_guest.in_guest_signal_delivery = true;
    assert!(crate::syscalls::helpers::publishes_guest_masks(&in_guest));
    let ptrace = Config {
        sequentialize_threads: true,
        ..Config::default()
    };
    assert_eq!(
        crate::syscalls::helpers::publishes_guest_masks(&ptrace),
        ptrace.models_signal_targets()
    );
}

/// Through the real RPC: the coordinator records the mask a
/// `RequestResources` publishes before the scheduler can act on the request,
/// and records nothing with the gate off. A `ParkedRequest` is filed through
/// the same function (`GlobalState::recv_resources_with_origin`).
#[tokio::test]
async fn the_coordinator_records_a_published_mask_as_the_request_is_filed() {
    for gate in [true, false] {
        let mut config = Config {
            sequentialize_threads: true,
            max_timeslice: None,
            in_guest_signal_delivery: gate,
            ..Config::default()
        };
        config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
        let state = GlobalState::initialize(&config, false);
        let detpid = DetPid::from_raw(THREAD.as_raw());
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(THREAD, THREAD, true);
        install_test_registration(&state, THREAD, Ivar::new());
        let mm = MmId::initial(detpid);
        let mut publishing = Resources::new(THREAD);
        publishing.insert(ResourceID::MemAddrSpace(detpid), Permission::RW);
        publishing.guest_signal_mask = Some(TEMPORARY);
        let mut filed = Box::pin(state.receive_rpc(
            Tid::from_raw(THREAD.as_raw()),
            (
                DetTime::new(&config),
                mm,
                GlobalRequest::RequestResources(publishing, detpid),
            ),
        ));
        assert!(futures::poll!(&mut filed).is_pending());
        let held = state.sched.lock().unwrap().published_guest_sigmask(THREAD);
        assert_eq!(held, gate.then_some(TEMPORARY), "gate {gate}");
    }
}

/// The `/proc` status reader under the gate (`status_published_sigblk`, the
/// decision `Detcore::initialize_procfs_snapshot` makes for a status file),
/// fed into the snapshot sanitizer that snapshot uses
/// (`ProcfsFile::initialize`), on the test thread's real
/// `/proc/thread-self/status`, its `Pid` line set to the guest thread's
/// number as the guest's own status shows it. The backend records a guest mask
/// unlike the kernel's, as a runtime with a mask of its own would. Under the
/// gate, after a request that published that mask, `SigBlk` shows it; after a
/// request that published nothing, the kernel's `SigBlk`; with the gate off,
/// the kernel's. Mutation: that reader's condition reverted alone to
/// `discarded_at_send::records`, which shows the kernel's mask under the gate.
#[tokio::test]
async fn proc_status_sigblk_shows_the_published_mask_under_the_gate() {
    let _mask = MaskGuard::block(&[]);
    let kernel = current_thread_mask();
    assert_ne!(kernel, TEMPORARY);
    let raw = std::fs::read_to_string("/proc/thread-self/status").unwrap();
    let contents: String = raw
        .lines()
        .map(|line| {
            if line.starts_with("Pid:") {
                format!("Pid:\t{}\n", THREAD.as_raw())
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    let contents = contents.into_bytes();
    for (gate, published, wanted) in [
        (true, Some(TEMPORARY), TEMPORARY),
        (true, None, kernel),
        (false, Some(TEMPORARY), kernel),
    ] {
        let case = format!("gate {gate}, published {published:?}");
        let mut guest = publishing_guest(gate, false);
        guest.keeps_guest_mask = published.is_some();
        guest.resume_override = published;
        file_parent(&mut guest, "ready, request filed").await;
        let mut file =
            crate::procfs::ProcfsFile::from_path(std::path::Path::new("/proc/thread-self/status"))
                .expect("a status file");
        file.bind_thread_identity(THREAD.as_raw(), THREAD.as_raw(), 1);
        let mask_thread = file.status_mask_thread(&contents);
        assert_eq!(mask_thread, Some(THREAD.as_raw()), "{case}");
        let published_sigblk =
            crate::syscalls::status_published_sigblk(&mut guest, mask_thread).await;
        file.initialize(
            contents.clone(),
            crate::procfs::ProcfsSnapshotContext {
                published_sigblk,
                ..Default::default()
            },
        );
        let shown = String::from_utf8(file.take_at(0, usize::MAX).expect("a snapshot")).unwrap();
        let sigblk = shown
            .lines()
            .find_map(|line| line.strip_prefix("SigBlk:"))
            .map(|value| u64::from_str_radix(value.trim(), 16).unwrap())
            .expect("a SigBlk line");
        assert_eq!(sigblk, wanted, "{case}: {shown}");
    }
}
