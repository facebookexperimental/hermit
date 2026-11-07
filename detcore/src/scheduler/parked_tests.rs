/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

use reverie::BackendChildWaitEvent;
use reverie::BackendChildWaitState;
use reverie::BackendSignalControl;
use reverie::BackendSignalControlMode;
use reverie::ChildExitCompletion;
use reverie::ChildExitPublication;
use reverie::ChildExitPublicationEffect;
use reverie::ChildExitPublicationResult;
use reverie::ExitStatus;
use reverie::OrphanPlacement;
use reverie::OrphanPlacementReceipt;
use reverie::OrphanPlacementResult;
use reverie::OrphanReaper;
use reverie::PlacedOrphan;
use reverie::PlacedOrphanEffect;
use reverie::ProcessSignalControl;
use reverie::ProcessSignalPublication;
use reverie::ProcessSignalPublicationResult;
use reverie::SignalBoundaryOutcome;
use reverie::SignalBoundaryReceipt;
use reverie::SignalDeliveryPermit;
use reverie::SignalProcessId;
use reverie::SignalRecipient;
use reverie::SignalTaskIdentity;

use super::parked::*;
use super::real_timer::TimerFailure;
use super::signal_state_reader::TemporaryMaskUnknown;
use super::*;
use crate::config::SchedHeuristic;
use crate::tool_global::SigalrmControl;

#[derive(Default)]
struct Backend {
    child_effect: Mutex<Option<ChildExitPublicationEffect>>,
    child_failures: Mutex<Vec<ChildExitPublication>>,
    child_publication_probe: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    child_publications: Mutex<Vec<ChildExitCompletion>>,
    fail_child_publication: std::sync::atomic::AtomicBool,
    fail_publication: std::sync::atomic::AtomicBool,
    /// Every alarm and timer publication reports its signal already pending.
    coalesce: std::sync::atomic::AtomicBool,
    fail_recipients: Mutex<Option<SignalProcessId>>,
    fail_reservation: std::sync::atomic::AtomicBool,
    failure_processes: Mutex<Vec<SignalProcessId>>,
    failure_probe: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    recipients: Mutex<Vec<SignalRecipient>>,
    /// Recipients that join `recipients` when their process's alarm is
    /// published, as a published SIGALRM makes the process's parked threads
    /// alarm recipients.
    recipients_on_alarm: Mutex<Vec<SignalRecipient>>,
    publications: Mutex<Vec<(SignalProcessId, reverie::SignalEvent)>>,
    timer_publications: Mutex<Vec<(SignalProcessId, reverie::SignalEvent)>>,
    reject_child_publication: std::sync::atomic::AtomicBool,
    permits: Mutex<Vec<SignalDeliveryPermit>>,
    /// Whether the Tool opted in to orphan placement.
    orphan_placement_enabled: std::sync::atomic::AtomicBool,
    /// Every `place_orphans` call, in order.
    placements: Mutex<Vec<(SignalProcessId, Vec<OrphanPlacement>)>>,
    /// Reapers that reap their children automatically in the backend's
    /// records: a zombie placed with one is reaped at once.
    auto_reaping_reapers: Mutex<Vec<SignalProcessId>>,
    /// Refuse every placement before committing it.
    reject_placement: std::sync::atomic::AtomicBool,
    /// Report every live orphan as a zombie, a receipt that disagrees.
    misreport_placement: std::sync::atomic::AtomicBool,
    /// Children the backend holds for every exit, whatever Detcore names:
    /// one it is not told about goes to the namespace init and is named in
    /// the receipt after the placements, as LinuxEmu's does.
    held_orphans: Mutex<Vec<SignalProcessId>>,
    /// Every `publish_parent_death_signal` call, in order.
    parent_death_publications: Mutex<Vec<(SignalProcessId, reverie::SignalEvent)>>,
    /// The recipients `signal_recipients` reports for each signal other than
    /// `SIGALRM`, whose recipients stay `recipients`.
    signal_recipient_list: Mutex<Vec<(i32, SignalRecipient)>>,
    /// The signal sets `signal_sets` reports, per thread ID; a thread with
    /// none is not reported (`NotProvided`).
    signal_sets: Mutex<std::collections::BTreeMap<i32, reverie::SignalSets>>,
    /// Each task's signal sets as `signal_sets` reports them, by exact task
    /// lifetime (#4929 slice 2's tests); asked before `signal_sets`.
    task_signal_sets: Mutex<Vec<(SignalTaskIdentity, reverie::SignalSets)>>,
    /// Whether `publish_alarm` reports its copy coalesced with one the
    /// shared queue already held.
    coalesce_alarm: std::sync::atomic::AtomicBool,
}
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend").finish_non_exhaustive()
    }
}
impl ProcessSignalControl for Backend {
    fn publish_alarm(
        &self,
        process: SignalProcessId,
        event: reverie::SignalEvent,
    ) -> ProcessSignalPublicationResult {
        self.publications.lock().unwrap().push((process, event));
        let (now, later): (Vec<_>, Vec<_>) = self
            .recipients_on_alarm
            .lock()
            .unwrap()
            .drain(..)
            .partition(|r| r.task.process == process);
        *self.recipients_on_alarm.lock().unwrap() = later;
        self.recipients.lock().unwrap().extend(now);
        let receipt = ProcessSignalPublication {
            process,
            pending_generation: 0,
            coalesced: self.coalesce.load(std::sync::atomic::Ordering::Relaxed)
                || self
                    .coalesce_alarm
                    .load(std::sync::atomic::Ordering::Relaxed),
            disposition: reverie::ProcessAlarmSignalDisposition::Caught,
        };
        if self
            .fail_publication
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            ProcessSignalPublicationResult::FailedAfterCommit {
                receipt,
                errno: reverie::Errno::EBADF,
            }
        } else {
            ProcessSignalPublicationResult::Committed(receipt)
        }
    }
    fn publish_timer_expiry(
        &self,
        process: SignalProcessId,
        event: reverie::SignalEvent,
    ) -> ProcessSignalPublicationResult {
        let alarm = event.signal() == libc::SIGALRM;
        self.timer_publications
            .lock()
            .unwrap()
            .push((process, event));
        // A SIGALRM copy is pending for the threads `recipients_on_alarm`
        // names, as `publish_alarm`'s is, and is coalesced as it is.
        let mut coalesced = false;
        if alarm {
            let (now, later): (Vec<_>, Vec<_>) = self
                .recipients_on_alarm
                .lock()
                .unwrap()
                .drain(..)
                .partition(|r| r.task.process == process);
            *self.recipients_on_alarm.lock().unwrap() = later;
            self.recipients.lock().unwrap().extend(now);
            coalesced = self
                .coalesce_alarm
                .load(std::sync::atomic::Ordering::Relaxed);
        }
        ProcessSignalPublicationResult::Committed(ProcessSignalPublication {
            process,
            pending_generation: 0,
            coalesced: coalesced || self.coalesce.load(std::sync::atomic::Ordering::Relaxed),
            disposition: reverie::ProcessAlarmSignalDisposition::Caught,
        })
    }
    fn publish_child_exit(&self, completion: ChildExitCompletion) -> ChildExitPublicationResult {
        if let Some(probe) = self.child_publication_probe.lock().unwrap().as_ref() {
            probe();
        }
        self.child_publications.lock().unwrap().push(completion);
        if self
            .reject_child_publication
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return ChildExitPublicationResult::RejectedBeforeCommit(reverie::Errno::EBADF);
        }
        let receipt = ChildExitPublication {
            completion,
            pending_generation: 11,
            effect: self
                .child_effect
                .lock()
                .unwrap()
                .unwrap_or(ChildExitPublicationEffect::Queued),
        };
        if self
            .fail_child_publication
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            ChildExitPublicationResult::FailedAfterCommit {
                receipt,
                errno: reverie::Errno::EBADF,
            }
        } else {
            ChildExitPublicationResult::Committed(receipt)
        }
    }
    fn signal_recipients(
        &self,
        process: SignalProcessId,
        signal: i32,
    ) -> Result<Vec<SignalRecipient>, reverie::Errno> {
        if *self.fail_recipients.lock().unwrap() == Some(process) {
            return Err(reverie::Errno::EBADF);
        }
        if signal != libc::SIGALRM {
            return Ok(self
                .signal_recipient_list
                .lock()
                .unwrap()
                .iter()
                .filter(|(listed, r)| *listed == signal && r.task.process == process)
                .map(|(_, r)| *r)
                .collect());
        }
        Ok(self
            .recipients
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.task.process == process)
            .copied()
            .collect())
    }
    fn enable_orphan_placement(&self) -> Result<(), reverie::Errno> {
        self.orphan_placement_enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    fn place_orphans(
        &self,
        exited: SignalProcessId,
        placements: &[OrphanPlacement],
    ) -> OrphanPlacementResult {
        self.placements
            .lock()
            .unwrap()
            .push((exited, placements.to_vec()));
        if self
            .reject_placement
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return OrphanPlacementResult::RejectedBeforeCommit(reverie::Errno::EINVAL);
        }
        // A child whose exit was published is a zombie in these records.
        let exited_children: Vec<SignalProcessId> = self
            .child_publications
            .lock()
            .unwrap()
            .iter()
            .map(|completion| completion.child)
            .collect();
        let misreport = self
            .misreport_placement
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut placed: Vec<PlacedOrphan> = placements
            .iter()
            .map(|placement| {
                let zombie = misreport || exited_children.contains(&placement.child);
                let effect = match placement.reaper {
                    OrphanReaper::NamespaceInit => PlacedOrphanEffect::NamespaceInit,
                    OrphanReaper::Process(_) if !zombie => PlacedOrphanEffect::Live,
                    OrphanReaper::Process(reaper)
                        if self.auto_reaping_reapers.lock().unwrap().contains(&reaper) =>
                    {
                        PlacedOrphanEffect::AutoReaped
                    }
                    OrphanReaper::Process(_) => {
                        PlacedOrphanEffect::Zombie(ChildExitPublicationEffect::Queued)
                    }
                };
                PlacedOrphan {
                    child: placement.child,
                    reaper: placement.reaper,
                    effect,
                }
            })
            .collect();
        for child in self.held_orphans.lock().unwrap().iter() {
            if !placements.iter().any(|placement| placement.child == *child) {
                placed.push(PlacedOrphan {
                    child: *child,
                    reaper: OrphanReaper::NamespaceInit,
                    effect: PlacedOrphanEffect::NamespaceInit,
                });
            }
        }
        OrphanPlacementResult::Committed(OrphanPlacementReceipt { exited, placed })
    }
    fn publish_parent_death_signal(
        &self,
        process: SignalProcessId,
        event: reverie::SignalEvent,
    ) -> ProcessSignalPublicationResult {
        self.parent_death_publications
            .lock()
            .unwrap()
            .push((process, event));
        ProcessSignalPublicationResult::Committed(ProcessSignalPublication {
            process,
            pending_generation: 0,
            coalesced: false,
            disposition: reverie::ProcessAlarmSignalDisposition::DefaultFatal,
        })
    }
    fn alarm_recipients(
        &self,
        process: SignalProcessId,
    ) -> Result<Vec<SignalRecipient>, reverie::Errno> {
        if *self.fail_recipients.lock().unwrap() == Some(process) {
            return Err(reverie::Errno::EBADF);
        }
        Ok(self
            .recipients
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.task.process == process)
            .copied()
            .collect())
    }
    fn signal_sets(
        &self,
        task: SignalTaskIdentity,
    ) -> Result<reverie::SignalSets, reverie::FactError> {
        if let Some(sets) = self
            .task_signal_sets
            .lock()
            .unwrap()
            .iter()
            .find(|(reported, _)| *reported == task)
            .map(|(_, sets)| *sets)
        {
            return Ok(sets);
        }
        self.signal_sets
            .lock()
            .unwrap()
            .get(&task.tid.as_raw())
            .copied()
            .ok_or(reverie::FactError::NotProvided)
    }
    fn reserve_delivery(&self, permit: SignalDeliveryPermit) -> Result<(), reverie::Errno> {
        if self
            .fail_reservation
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(reverie::Errno::EBADF);
        }
        self.permits.lock().unwrap().push(permit);
        Ok(())
    }
    fn release_delivery(&self, permit: SignalDeliveryPermit) -> Result<(), reverie::Errno> {
        self.permits.lock().unwrap().retain(|p| *p != permit);
        Ok(())
    }
    fn finish_publication_failure(&self, process: SignalProcessId) -> Result<(), reverie::Errno> {
        self.failure_processes.lock().unwrap().push(process);
        if let Some(probe) = self.failure_probe.lock().unwrap().as_ref() {
            probe();
        }
        Ok(())
    }
    fn finish_child_exit_publication_failure(
        &self,
        receipt: ChildExitPublication,
    ) -> Result<(), reverie::Errno> {
        self.child_failures.lock().unwrap().push(receipt);
        if let Some(probe) = self.failure_probe.lock().unwrap().as_ref() {
            probe();
        }
        Ok(())
    }
}
fn at(n: u64) -> LogicalTime {
    LogicalTime::from_nanos(n)
}

/// Direct maintenance tests set committed_time in a zero-based clock without
/// a running GlobalTime. Supply that same intended instant to timer dispatch.
fn committed_clock(s: &Scheduler) -> Arc<Mutex<GlobalTime>> {
    let mut global = GlobalTime::new(&Config {
        epoch: chrono::DateTime::UNIX_EPOCH,
        ..Config::default()
    });
    global.add_extra_time(std::time::Duration::from_nanos(s.committed_time.as_nanos()));
    Arc::new(Mutex::new(global))
}
fn task(pid: i32, tid: i32) -> SignalTaskIdentity {
    SignalTaskIdentity {
        process: SignalProcessId {
            tgid: reverie::Pid::from_raw(pid),
            generation: 1,
        },
        tid: reverie::Pid::from_raw(tid),
        task_generation: tid as u64,
    }
}
fn fixture() -> (Scheduler, Arc<Backend>) {
    fixture_with_control(true)
}
/// A scheduler for a backend that reports the KVM capabilities. With
/// `installed`, it holds a fake installed process signal control, as a
/// sequentialized run on that backend does; without, the backend offered
/// nothing and the run is not controlled.
fn fixture_with_control(installed: bool) -> (Scheduler, Arc<Backend>) {
    let mut s = Scheduler::new(&Config {
        sequentialize_threads: true,
        ..Config::default().with_backend(|backend| {
            // The two behaviours the old `backend_is_kvm` identity flag
            // selected that remain capabilities.
            backend.provides_process_signal_control = true;
            backend.emulates_child_waits = true;
            backend.needs_killed_thread_rpc_cancellation = true;
        })
    });
    let backend = Arc::new(Backend::default());
    if installed {
        assert_eq!(
            s.install_signal_control(
                Some(BackendSignalControl {
                    process: backend.clone(),
                }),
                true,
            )
            .unwrap(),
            BackendSignalControlMode::ToolControlled
        );
    }
    assert_eq!(s.signal_control_installed(), installed);
    (s, backend)
}
/// A controlled backend's guest tasks are reached only through the control,
/// so a process-directed POSIX timer's expiration is published there, as
/// Linux queues it: the timer's signal with `SI_TIMER` and the timer's id.
/// Nothing is sent to a host process, and the `ITIMER_REAL` publication is
/// not used.
#[test]
fn controlled_posix_timer_expiry_is_published_not_sent_to_a_host_process() {
    let (mut s, backend) = fixture();
    let (tid, _, _) = add(&mut s, 10, 10);
    let deadline = LogicalTime::from_nanos(80);
    s.dispatch_timed_signal(
        deadline,
        timed_waiters::SignalTimerId::Posix(DetPid::from_raw(10), 3),
        timed_waiters::SignalTarget::Process(tid),
        timed_waiters::TimerSignal::Send(nix::sys::signal::Signal::SIGUSR1),
        true,
        deadline,
    );
    assert_eq!(s.host_signal_attempts, 0);
    assert!(backend.publications.lock().unwrap().is_empty());
    let published = backend.timer_publications.lock().unwrap();
    let [(process, event)] = published.as_slice() else {
        panic!("one timer publication expected, got {published:?}");
    };
    assert_eq!(*process, task(10, 10).process);
    assert_eq!(event.signal(), libc::SIGUSR1);
    assert_eq!(
        event.target(),
        reverie::SignalTarget::Process {
            pid: reverie::Pid::from_raw(10)
        }
    );
    let info = event.siginfo();
    let field = |at: usize| i32::from_ne_bytes(info[at..at + 4].try_into().unwrap());
    assert_eq!(
        (field(0), field(8), field(16), field(20)),
        (libc::SIGUSR1, libc::SI_TIMER, 3, 0)
    );
    assert_eq!(s.parked.failure, None);
}

#[test]
fn async_exit_completion_refuses_signal_control() {
    // The controlled turn loop releases the scheduler mutex between phases,
    // which would let an exit barrier escape the asynchronous-exit hold; a
    // backend reporting both capabilities is refused rather than run.
    let config = Config::default().with_backend(|backend| {
        backend.provides_process_signal_control = true;
        backend.process_exits_complete_asynchronously = true;
    });
    let mut s = Scheduler::new(&config);
    let backend = Arc::new(Backend::default());
    assert!(
        s.install_signal_control(Some(BackendSignalControl { process: backend }), true)
            .is_err()
    );
    assert!(!s.signal_control_installed());
}

fn add(s: &mut Scheduler, pid: i32, tid: i32) -> (DetTid, MmId, reverie::CallbackSignalSite) {
    add_with_mm(s, pid, tid, MmId::initial(DetTid::from_raw(pid)))
}
fn add_with_mm(
    s: &mut Scheduler,
    pid: i32,
    tid: i32,
    mm: MmId,
) -> (DetTid, MmId, reverie::CallbackSignalSite) {
    let (pid, tid) = (DetTid::from_raw(pid), DetTid::from_raw(tid));
    s.thread_tree.add_child(pid, tid, pid == tid);
    if pid == tid {
        // The root inherits its process group from outside the namespace; this
        // test's root leads its own, as after setpgid(0, 0), so its children's
        // group is its pid.
        s.thread_tree.set_process_group(pid, pid);
    }
    s.priorities.insert(tid, DEFAULT_PRIORITY);
    s.next_turns.insert(
        tid,
        ThreadNextTurn {
            dettid: tid,
            child_tid_addr: 0,
            req: Ivar::new(),
            resp: Ivar::new(),
            protocol: Default::default(),
        },
    );
    let identity = task(pid.as_raw(), tid.as_raw());
    s.real_timers.bind(pid, tid, mm, identity).unwrap();
    (
        tid,
        mm,
        reverie::CallbackSignalSite {
            process: identity.process,
            tid: identity.tid,
            task_generation: identity.task_generation,
            callback_nonce: 1,
            boundary_nonce: 1,
        },
    )
}

fn add_process_child(
    s: &mut Scheduler,
    parent: i32,
    child: i32,
) -> (DetTid, MmId, reverie::CallbackSignalSite) {
    let parent = DetTid::from_raw(parent);
    let child = DetTid::from_raw(child);
    let mm = MmId::initial(child);
    s.thread_tree.add_child(parent, child, true);
    s.priorities.insert(child, DEFAULT_PRIORITY);
    s.next_turns.insert(
        child,
        ThreadNextTurn {
            dettid: child,
            child_tid_addr: 0,
            req: Ivar::new(),
            resp: Ivar::new(),
            protocol: Default::default(),
        },
    );
    let identity = task(child.as_raw(), child.as_raw());
    s.real_timers.bind(child, child, mm, identity).unwrap();
    (
        child,
        mm,
        reverie::CallbackSignalSite {
            process: identity.process,
            tid: identity.tid,
            task_generation: identity.task_generation,
            callback_nonce: 1,
            boundary_nonce: 1,
        },
    )
}

fn child_exit_event(
    parent: SignalProcessId,
    child: SignalProcessId,
    status: ExitStatus,
    waitable: bool,
) -> BackendChildWaitEvent {
    BackendChildWaitEvent {
        parent,
        child,
        state: BackendChildWaitState::Exited {
            status,
            waitable,
            uid: 1000,
            user_ticks: 13,
            system_ticks: 17,
        },
    }
}

fn poll_child_exit(
    future: &mut Pin<Box<super::signal_control::ChildExitPublicationFuture>>,
) -> Poll<Result<(), reverie::Error>> {
    let waker = futures::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    Future::poll(future.as_mut(), &mut context)
}
fn sleep(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    site: reverie::CallbackSignalSite,
    deadline: u64,
) -> Ivar<SchedResponse> {
    let mut r = Resources::new(tid);
    r.insert(ResourceID::SleepUntil(at(deadline)), Permission::RW);
    s.install_resource_origin(
        tid,
        ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm,
            control: ControlCapability::ParkedWait {
                policy: ParkedWaitPolicy::NanosleepNoHandlerRestart {
                    absolute_deadline: at(deadline),
                },
                site,
            },
        },
    )
    .unwrap();
    let turn = &s.next_turns[&tid];
    turn.req.put(Ok(r));
    let response = turn.resp.clone();
    s.blocked.timed_waiters.insert(at(deadline), tid);
    response
}
fn selected(response: &Ivar<SchedResponse>) -> AlarmControl {
    match response.try_read().expect("observation transport") {
        SchedResponse::ObserveSignal(c) => *c,
        other => panic!("{other:?}"),
    }
}

#[test]
fn real_expiry_publishes_without_any_borrowed_callback() {
    let (mut s, b) = fixture();
    let (leader, _, _) = add(&mut s, 100, 100);
    let (worker, _, _) = add(&mut s, 100, 101);
    // An arming worker does not own the process timer's later lifetime.
    s.replace_real_timer(leader, worker, at(0), at(10), at(7), Signal::SIGALRM)
        .unwrap();
    s.real_timers.retire_task(leader, worker);
    s.committed_time = at(10);
    s.step2b_process_timed(&committed_clock(&s));
    assert!(!s.backend_failed());
    assert_eq!(s.host_signal_attempts, 0);
    let p = b.publications.lock().unwrap();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].0, task(100, 100).process);
    assert_eq!(
        i32::from_ne_bytes(p[0].1.siginfo()[8..12].try_into().unwrap()),
        libc::SI_KERNEL
    );
    assert_eq!(
        s.real_timers.snapshot(leader, at(100)).unwrap().remaining,
        at(0)
    );
    assert!(s.blocked.timed_waiters.is_empty());
}

/// A dequeue of process 100's `ITIMER_REAL` copy, number `sequence` of that
/// process, by `consumer`.
fn alarm_dequeue(sequence: u64, consumer: reverie::SignalConsumer) -> reverie::SignalDequeue {
    alarm_dequeue_of(100, sequence, consumer)
}

/// A dequeue of process `pid`'s `ITIMER_REAL` copy, number `sequence` of that
/// process, by `consumer`.
fn alarm_dequeue_of(
    pid: i32,
    sequence: u64,
    consumer: reverie::SignalConsumer,
) -> reverie::SignalDequeue {
    let process = task(pid, pid).process;
    reverie::SignalDequeue {
        process,
        sequence,
        consumer,
        domain: reverie::PendingDomain::Process,
        event: reverie::SignalEvent::new(
            libc::SIGALRM,
            super::discarded_at_send::itimer_real_siginfo(),
            reverie::SignalTarget::Process { pid: process.tgid },
        )
        .unwrap(),
    }
}

/// Process 100, single-threaded, with a controlled periodic `ITIMER_REAL`
/// of `period` armed at 0, whose first expiry has been popped and published
/// at committed time `period`.
fn published_periodic_alarm(period: u64) -> (Scheduler, Arc<Backend>, DetTid) {
    let (mut s, b) = fixture();
    let (leader, _, _) = add(&mut s, 100, 100);
    s.replace_real_timer(
        leader,
        leader,
        at(0),
        at(period),
        at(period),
        Signal::SIGALRM,
    )
    .unwrap();
    s.committed_time = at(period);
    assert!(s.step2b_process_timed(&committed_clock(&s)));
    assert_eq!(b.publications.lock().unwrap().len(), 1);
    assert!(s.blocked.timed_waiters.is_empty());
    (s, b, leader)
}

/// https://github.com/rrnewton/hermit/issues/4929: a copy taken at a return
/// to user mode restarts the controlled timer at the next quiesced boundary
/// (O3), on its grid after that boundary's committed time, as the traced
/// path restarts its own. The turn that took the copy has been charged by
/// then, so a period shorter than that charge does not expire again inside
/// it: here the copy of a 100 us timer is taken at 100 us and the boundary
/// comes after a 500 us charge, so the timer restarts at 700 us and that
/// boundary pops nothing. A restart at the dequeue instant would have queued
/// 200 us, due at once. Without a dequeue the boundary restarts nothing.
#[test]
fn a_dequeued_controlled_alarm_restarts_at_the_next_quiesced_boundary() {
    for dequeued in [false, true] {
        let (mut s, b, leader) = published_periodic_alarm(100_000);
        if dequeued {
            assert_eq!(
                s.apply_signal_dequeue(
                    leader,
                    leader,
                    alarm_dequeue(1, reverie::SignalConsumer::ReturnToUser)
                ),
                Ok(super::real_timer::DequeueAck::Applied { sequence: 1 })
            );
            assert!(
                s.blocked.timed_waiters.is_empty(),
                "the dequeue itself restarts nothing"
            );
        }
        s.committed_time = at(600_005);
        s.reconcile_inactive_alarms(s.committed_time);
        if dequeued {
            assert_eq!(s.blocked.timed_waiters.next_deadline(), Some(at(700_000)));
            assert!(!s.step2b_process_timed(&committed_clock(&s)));
            assert_eq!(b.publications.lock().unwrap().len(), 1);
            assert_eq!(
                s.real_timers
                    .snapshot(leader, at(600_005))
                    .unwrap()
                    .remaining,
                at(99_995)
            );
        } else {
            assert!(s.blocked.timed_waiters.is_empty());
            assert_eq!(
                s.real_timers
                    .snapshot(leader, at(600_005))
                    .unwrap()
                    .remaining,
                at(0)
            );
        }
        assert!(s.real_timers.dequeued_processes().is_empty());
        assert!(!s.backend_failed());
    }
}

/// https://github.com/rrnewton/hermit/issues/4929: a copy a signalfd read
/// or `rt_sigtimedwait` takes in the reader's own turn restarts the
/// controlled timer at that call's reconcile point (`ReconcileAlarm`, O2), at
/// the reader's current time, and the RPC hints the new deadline as the
/// reader's slice end (`note_arming_hint`), as the traced path does. Before
/// the dequeue, and again after the restart, a reconcile point restarts
/// nothing. A `setitimer` reconciles first (O1), so its old value reports
/// the restart.
#[test]
fn an_in_turn_dequeue_restarts_the_controlled_alarm_at_its_reconcile_point() {
    let (mut s, _, leader) = published_periodic_alarm(5_000_000);
    assert_eq!(s.reconcile_alarm(leader, at(5_100_000)), None);
    s.apply_signal_dequeue(
        leader,
        leader,
        alarm_dequeue(1, reverie::SignalConsumer::SignalFd),
    )
    .unwrap();
    assert!(s.blocked.timed_waiters.is_empty());
    let now = at(7_300_000);
    assert_eq!(s.reconcile_alarm(leader, now), Some(at(10_000_000)));
    assert_eq!(
        s.blocked.timed_waiters.next_deadline(),
        Some(at(10_000_000))
    );
    assert_eq!(
        s.itimer_snapshot(leader, now).unwrap(),
        super::real_timer::ItimerSnapshot {
            remaining: at(2_700_000),
            interval: at(5_000_000),
        }
    );
    s.note_arming_hint(leader, now);
    assert_eq!(
        s.take_deadline_hint(leader),
        Some(Duration::from_nanos(2_700_000))
    );
    assert_eq!(s.reconcile_alarm(leader, at(7_400_000)), None);
    assert_eq!(
        s.blocked.timed_waiters.next_deadline(),
        Some(at(10_000_000))
    );

    // O1 then the replacement, as `GlobalRequest::RegisterAlarm` runs them.
    let (mut s, _, leader) = published_periodic_alarm(5_000_000);
    s.apply_signal_dequeue(
        leader,
        leader,
        alarm_dequeue(1, reverie::SignalConsumer::ReturnToUser),
    )
    .unwrap();
    assert_eq!(s.reconcile_alarm(leader, now), Some(at(10_000_000)));
    assert_eq!(
        s.replace_real_timer(leader, leader, now, at(0), at(0), Signal::SIGALRM),
        Ok((at(2_700_000), at(5_000_000)))
    );
    assert!(s.blocked.timed_waiters.is_empty());
    assert!(!s.backend_failed());
}

/// https://github.com/rrnewton/hermit/issues/4929: as on Linux, only a
/// SIGALRM taken from the process's shared queue restarts the controlled
/// timer. `dequeue_signal` re-arms `ITIMER_REAL` only on that path (v6.17
/// kernel/signal.c:632-636). A copy taken from a thread's private queue (a
/// `tgkill` SIGALRM, line 629) and another signal taken from the shared
/// queue are acknowledged and leave the timer inactive, so no reconcile point
/// restarts it; the shared SIGALRM after them does.
#[test]
fn only_a_shared_queue_sigalrm_dequeue_restarts_the_controlled_alarm() {
    let (mut s, b, leader) = published_periodic_alarm(100_000);
    let process = task(100, 100).process;
    let siginfo = |signal: i32, code: i32| {
        let mut info = [0; reverie::SIGNAL_INFO_SIZE];
        info[..4].copy_from_slice(&signal.to_ne_bytes());
        info[8..12].copy_from_slice(&code.to_ne_bytes());
        info
    };
    let private = reverie::SignalDequeue {
        domain: reverie::PendingDomain::Thread,
        event: reverie::SignalEvent::new(
            libc::SIGALRM,
            siginfo(libc::SIGALRM, libc::SI_TKILL),
            reverie::SignalTarget::Thread {
                pid: process.tgid,
                tid: reverie::Tid::from_raw(100),
            },
        )
        .unwrap(),
        ..alarm_dequeue(1, reverie::SignalConsumer::ReturnToUser)
    };
    let other = reverie::SignalDequeue {
        event: reverie::SignalEvent::new(
            libc::SIGUSR1,
            siginfo(libc::SIGUSR1, libc::SI_USER),
            reverie::SignalTarget::Process { pid: process.tgid },
        )
        .unwrap(),
        ..alarm_dequeue(2, reverie::SignalConsumer::ReturnToUser)
    };
    for (dequeue, sequence) in [(private, 1), (other, 2)] {
        assert_eq!(
            s.apply_signal_dequeue(leader, leader, dequeue),
            Ok(super::real_timer::DequeueAck::Applied { sequence })
        );
        assert!(s.real_timers.dequeued_processes().is_empty());
    }
    s.committed_time = at(600_005);
    s.reconcile_inactive_alarms(s.committed_time);
    assert_eq!(s.reconcile_alarm(leader, s.committed_time), None);
    assert!(s.blocked.timed_waiters.is_empty());
    assert_eq!(
        s.real_timers
            .snapshot(leader, at(600_005))
            .unwrap()
            .remaining,
        at(0)
    );
    assert_eq!(
        s.apply_signal_dequeue(
            leader,
            leader,
            alarm_dequeue(3, reverie::SignalConsumer::ReturnToUser)
        ),
        Ok(super::real_timer::DequeueAck::Applied { sequence: 3 })
    );
    s.reconcile_inactive_alarms(s.committed_time);
    assert_eq!(s.blocked.timed_waiters.next_deadline(), Some(at(700_000)));
    assert_eq!(b.publications.lock().unwrap().len(), 1);
    assert!(!s.backend_failed());
}

/// https://github.com/rrnewton/hermit/issues/4929: a controlled periodic
/// `ITIMER_REAL` of at most two turn charges hints no slice end, as a traced
/// one does (`fast_timer_period`); a period one nanosecond longer does, and
/// so does a one-shot timer of the same length.
#[test]
fn a_fast_controlled_periodic_alarm_hints_no_slice_end() {
    let fast = fixture().0.fast_timer_period.as_nanos();
    assert!(fast > 0);
    for (value, interval, hinted) in [
        (fast, fast, false),
        (fast + 1, fast + 1, true),
        (fast, 0, true),
    ] {
        let (mut s, _) = fixture();
        let (leader, _, _) = add(&mut s, 100, 100);
        s.replace_real_timer(
            leader,
            leader,
            at(0),
            at(value),
            at(interval),
            Signal::SIGALRM,
        )
        .unwrap();
        assert_eq!(
            s.deadline_remaining_from(leader, at(0)),
            hinted.then_some(Duration::from_nanos(value)),
            "value {value}, interval {interval}"
        );
    }
}

/// Files `tid`'s parked wait at a controlled site, as `parked_wait_request`
/// files a `pause` (`SleepUntil(INDEFINITE)`, `handle_pause`) or a
/// `nanosleep` (`SleepUntil(deadline)`), without registering it: the
/// scheduler registers a queued sleep itself.
fn file_parked_wait(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    site: reverie::CallbackSignalSite,
    policy: ParkedWaitPolicy,
) -> Ivar<SchedResponse> {
    let mut r = Resources::new(tid);
    match policy {
        ParkedWaitPolicy::PauseNoHandlerRestart => {
            r.insert(
                ResourceID::SleepUntil(LogicalTime::INDEFINITE),
                Permission::W,
            );
            r.fyi(crate::resources::PAUSE_FYI);
        }
        ParkedWaitPolicy::NanosleepNoHandlerRestart { absolute_deadline } => {
            r.insert(ResourceID::SleepUntil(absolute_deadline), Permission::RW);
        }
    }
    s.install_resource_origin(
        tid,
        ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm,
            control: ControlCapability::ParkedWait { policy, site },
        },
    )
    .unwrap();
    let turn = &s.next_turns[&tid];
    turn.req.put(Ok(r));
    turn.resp.clone()
}

/// Files `tid`'s parked `nanosleep` until `deadline` (`file_parked_wait`).
fn file_parked_sleep(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    site: reverie::CallbackSignalSite,
    deadline: LogicalTime,
) -> Ivar<SchedResponse> {
    file_parked_wait(
        s,
        tid,
        mm,
        site,
        ParkedWaitPolicy::NanosleepNoHandlerRestart {
            absolute_deadline: deadline,
        },
    )
}

/// Parks `tid` in `pause` at a controlled site (`file_parked_wait`),
/// registered with no deadline, as its admission registers it.
fn parked_pause(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    site: reverie::CallbackSignalSite,
) -> Ivar<SchedResponse> {
    let response = file_parked_wait(s, tid, mm, site, ParkedWaitPolicy::PauseNoHandlerRestart);
    s.blocked.timed_waiters.insert(LogicalTime::INDEFINITE, tid);
    response
}

/// Finishes the observation `control` of `tid` as `FinishParkedObservation`
/// does: posts the finish, whose acknowledgement only the daemon's control
/// drain puts.
fn post_finish(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    control: &AlarmControl,
    finish: ObservationFinish,
) -> Ivar<Result<FinishAck, ProtocolFailure>> {
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: control.continuation,
            lease: control.lease,
            site: control.site,
            finish,
            ack: ack.clone(),
        },
    )
    .unwrap();
    ack
}

/// The finish of an observation that took a signal for a caught handler.
fn caught(control: &AlarmControl) -> ObservationFinish {
    ObservationFinish::InterruptForCaught {
        selection: reverie::PreparedSignalToken {
            site: control.site,
            selection_nonce: 1,
        },
        dequeue: None,
    }
}

/// The handler's frame is committed (`consume_signal_boundary`), the backend
/// releases its reservation, and no signal is pending any more.
fn handler_entered(s: &mut Scheduler, backend: &Backend, control: &AlarmControl) {
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: control.permit,
        outcome: SignalBoundaryOutcome::Caught,
    })
    .unwrap();
    backend.release_delivery(control.permit).unwrap();
    backend.recipients.lock().unwrap().clear();
}

/// Polls `ready` every millisecond, for at most 10 s.
fn wait_until(mut ready: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("not reached in 10 s");
}

/// What a controlled turn leaves for the turns after it, with times relative
/// to the run's start: the grant, the queue, every timed event (a restarted
/// `ITIMER_REAL` among them), the granted thread's slice cap, the timer as
/// getitimer reads it, the clocks, and how many SIGALRMs were published.
#[derive(Debug, PartialEq, Eq)]
struct RestartTurn {
    selected: DetTid,
    queued: Vec<DetTid>,
    timed: Vec<(LogicalTime, timed_waiters::TimedEvent)>,
    slice_cap: Option<Duration>,
    timer: super::real_timer::ItimerSnapshot,
    turn: u64,
    committed: LogicalTime,
    global: LogicalTime,
    publications: usize,
}

impl RestartTurn {
    fn record(
        s: &Scheduler,
        backend: &Backend,
        global: &Mutex<GlobalTime>,
        start: LogicalTime,
        selected: DetTid,
        timer: DetPid,
    ) -> Self {
        let since = |t: LogicalTime| {
            if t.is_indefinite() {
                t
            } else {
                LogicalTime::from_nanos(t.as_nanos() - start.as_nanos())
            }
        };
        RestartTurn {
            selected,
            queued: s.run_queue.tids().copied().collect(),
            timed: s
                .blocked
                .timed_waiters
                .iter()
                .map(|(deadline, event)| (since(deadline), event))
                .collect(),
            slice_cap: s.deadline_hints.get(&selected).copied(),
            timer: s.itimer_snapshot(timer, s.committed_time).unwrap(),
            turn: s.turn,
            committed: since(s.committed_time),
            global: since(global.lock().unwrap().as_nanos()),
            publications: backend.publications.lock().unwrap().len(),
        }
    }
}

/// Files `tid`'s request for its delivery of SIGALRM, as the Tool's inbound
/// signal stop does (`resource_request`, ordinary origin). Inside an
/// observation, this delivery hook refills the observation's gate.
fn file_inbound_signal(s: &mut Scheduler, tid: DetTid, mm: MmId) {
    s.install_resource_origin(
        tid,
        ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm,
            control: ControlCapability::None,
        },
    )
    .unwrap();
    let mut r = Resources::new(tid);
    r.insert(
        ResourceID::InboundSignal(SigWrapper::from(Signal::SIGALRM)),
        Permission::RW,
    );
    s.next_turns[&tid].req.put(Ok(r));
}

/// The witness of https://github.com/rrnewton/hermit/issues/4929's review
/// round 1 (Codex P1), run through the real controlled turn loop on its own
/// thread, with the observed worker on another, as reverie-kvm runs a guest
/// thread. Parent R (process 100) is queued with an ordinary request; its
/// child W (process 200) armed a 100 us `ITIMER_REAL` with a 2 ms interval
/// and parked in `pause`. Turn 1 charges R's turn, pops W's expiry,
/// publishes it and observes W.
///
/// The worker does what reverie-kvm's observation does
/// (`observe_parked_signal`): it takes the copy, as `recv_signal_dequeued`
/// records it, and its delivery's hook at once files the `InboundSignal`
/// request, which refills the observation's gate. The observation finishes
/// only after that delivery's turn. `worker_first` decides which side
/// reaches the open gate first. With it, the daemon pauses there
/// (`observation_gate_pause`) until the worker has taken the copy and filed
/// the request, so its next look finds every request filled. Without it, the
/// worker waits until the daemon has looked and is waiting on the gate.
///
/// Turn 1 grants R. Then turn 2 grants W's delivery, in which W finishes its
/// observation (`InterruptForCaught`); turn 3 drains that finish, waits for
/// the handler's frame and W's next request, and grants; three more turns
/// follow. Returns the records of all six turns, and how many times turn 1's
/// loop waited.
fn controlled_restart_witness(worker_first: bool) -> (Vec<RestartTurn>, u64) {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use futures::FutureExt;

    let (mut s, backend) = fixture();
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let start = global.lock().unwrap().as_nanos();
    let (r, _, _) = add(&mut s, 100, 100);
    s.next_turns[&r].req.put(Ok(Resources::new(r)));
    s.runqueue_push_back(r);
    let (w, mm, site) = add_process_child(&mut s, 100, 200);
    let response = parked_pause(&mut s, w, mm, site);
    s.replace_real_timer(w, w, start, at(100_000), at(2_000_000), Signal::SIGALRM)
        .unwrap();
    backend.recipients.lock().unwrap().push(SignalRecipient {
        task: task(200, 200),
    });
    let release = Arc::new(AtomicBool::new(false));
    if worker_first {
        s.observation_gate_pause = Some(release.clone());
    }
    let scheduler = Arc::new(Mutex::new(s));
    let last = Ok(Resources::new(r));
    let (granted, control) = std::thread::scope(|scope| {
        let daemon = scope.spawn(|| {
            futures::executor::block_on(do_a_turn_blocking(
                scheduler.clone(),
                global.clone(),
                &last,
            ))
        });
        let control = match futures::executor::block_on(response.clone()) {
            SchedResponse::ObserveSignal(control) => *control,
            other => panic!("{other:?}"),
        };
        if !worker_first {
            wait_until(|| scheduler.lock().unwrap().controlled_turn_waits > 0);
        }
        // `recv_signal_dequeued`: the copy left the shared queue, at W's time.
        {
            let mut s = scheduler.lock().unwrap();
            assert_eq!(
                s.real_timers
                    .validate_effect(w, w, mm, task(200, 200), task(200, 200).process),
                Ok(true)
            );
            global
                .lock()
                .unwrap()
                .update_global_time(w, start + at(5_000), LogicalTime::ZERO);
            assert_eq!(
                s.apply_signal_dequeue(
                    w,
                    w,
                    alarm_dequeue_of(200, 1, reverie::SignalConsumer::ReturnToUser)
                ),
                Ok(super::real_timer::DequeueAck::Applied { sequence: 1 })
            );
        }
        // The delivery's hook refills the gate before the finish.
        file_inbound_signal(&mut scheduler.lock().unwrap(), w, mm);
        release.store(true, Ordering::Release);
        let granted = daemon.join().unwrap().expect("the turn grants a thread");
        (granted, control)
    });
    let record = |granted: DetTid| {
        let s = scheduler.lock().unwrap();
        assert!(!s.backend_failed());
        RestartTurn::record(&s, &backend, &global, start, granted, w)
    };
    let waits = scheduler.lock().unwrap().controlled_turn_waits;
    let mut turns = vec![record(granted.tid)];
    // Turn 2: R files its next request; W's delivery is granted.
    scheduler.lock().unwrap().next_turns[&r]
        .req
        .put(Ok(Resources::new(r)));
    let last = Ok(granted);
    let delivery = do_a_turn_blocking(scheduler.clone(), global.clone(), &last)
        .now_or_never()
        .expect("every request is filled")
        .expect("the turn grants a thread");
    turns.push(record(delivery.tid));
    // In its delivery's turn, W's observation finishes into its handler.
    let ack = post_finish(
        &mut scheduler.lock().unwrap(),
        w,
        mm,
        &control,
        caught(&control),
    );
    let last = Ok(delivery);
    let mut turn = Box::pin(do_a_turn_blocking(scheduler.clone(), global.clone(), &last));
    assert!(turn.as_mut().now_or_never().is_none(), "W's handler");
    assert_eq!(ack.try_read(), Some(Ok(FinishAck::Interrupted)));
    {
        let mut s = scheduler.lock().unwrap();
        handler_entered(&mut s, &backend, &control);
        s.next_turns[&w].req.put(Ok(Resources::new(w)));
    }
    let mut last = Ok(turn
        .as_mut()
        .now_or_never()
        .expect("every request is filled")
        .expect("the turn grants a thread"));
    drop(turn);
    turns.push(record(last.as_ref().unwrap().tid));
    for _ in 0..3 {
        let tid = last.as_ref().unwrap().tid;
        scheduler.lock().unwrap().next_turns[&tid]
            .req
            .put(Ok(Resources::new(tid)));
        let granted = do_a_turn_blocking(scheduler.clone(), global.clone(), &last)
            .now_or_never()
            .expect("every request is filled")
            .expect("the turn grants a thread");
        turns.push(record(granted.tid));
        last = Ok(granted);
    }
    (turns, waits)
}

/// https://github.com/rrnewton/hermit/issues/4929, review round 1 (Codex
/// P1): a parked SIGALRM's timer restart does not depend on whether the
/// observed worker or the daemon reaches the open observation gate first.
///
/// Reverie-kvm refills the gate from the observed thread's own host thread:
/// its observation takes the copy and its delivery's hook files the
/// `InboundSignal` request, before the observation finishes, so the gate can
/// be full before the daemon looks again. The loop therefore refreshes after
/// every pass that opens a gate, not only after a wait: the refresh (and its
/// reconcile, O3) runs at the first quiesced pass after the hook either way,
/// after the dequeue that precedes the hook on the worker's thread.
///
/// Both orders restart W's timer at 2.1 ms, the first point of its grid after
/// that pass's committed time of 505 us (R's 500 us turn charge and W's
/// 5 us), give R a 1.595 ms slice cap, and schedule the five turns after it
/// alike. Without the refresh after an opened gate, the worker-first order
/// grants R at 500 us with no slice cap and restarts the timer only at the
/// next turn's charge.
#[test]
fn a_parked_alarm_restart_does_not_depend_on_which_side_reaches_the_gate_first() {
    let (worker_first, worker_first_waits) = controlled_restart_witness(true);
    let (daemon_first, daemon_first_waits) = controlled_restart_witness(false);
    assert_eq!(worker_first, daemon_first);
    // Each order was forced. Worker first: the daemon's first look after the
    // gate found it refilled, so it did not wait. Daemon first: it waited on
    // the gate before the worker took the copy.
    assert_eq!(worker_first_waits, 0);
    assert_eq!(daemon_first_waits, 1);
    let first = &worker_first[0];
    assert_eq!(first.selected, DetTid::from_raw(100));
    assert_eq!(first.committed, at(505_000));
    assert_eq!(
        first.timed,
        [(
            at(2_100_000),
            timed_waiters::TimedEvent::SignalEvt(
                timed_waiters::SignalTimerId::Alarm(DetPid::from_raw(200)),
                timed_waiters::SignalTarget::Process(DetTid::from_raw(200)),
                timed_waiters::TimerSignal::Send(Signal::SIGALRM),
            )
        )]
    );
    assert_eq!(first.slice_cap, Some(Duration::from_nanos(1_595_000)));
    assert_eq!(
        first.timer,
        super::real_timer::ItimerSnapshot {
            remaining: at(1_595_000),
            interval: at(2_000_000),
        }
    );
    // R and W alternate; the first charged pass past 2.1 ms pops the expiry.
    assert_eq!(
        worker_first
            .iter()
            .map(|turn| (turn.selected.as_raw(), turn.committed, turn.publications))
            .collect::<Vec<_>>(),
        [
            (100, at(505_000), 1),
            (200, at(1_005_000), 1),
            (100, at(1_505_000), 1),
            (200, at(2_005_000), 1),
            (100, at(2_505_000), 2),
            (200, at(3_005_000), 2),
        ]
    );
}

/// A SIGALRM that `kill` sent to process `pid` (`SI_USER`), taken from the
/// shared queue as number `sequence` of that process.
fn kill_alarm_dequeue_of(pid: i32, sequence: u64) -> reverie::SignalDequeue {
    let mut info = [0; reverie::SIGNAL_INFO_SIZE];
    info[..4].copy_from_slice(&libc::SIGALRM.to_ne_bytes());
    info[8..12].copy_from_slice(&libc::SI_USER.to_ne_bytes());
    reverie::SignalDequeue {
        event: reverie::SignalEvent::new(
            libc::SIGALRM,
            info,
            reverie::SignalTarget::Process {
                pid: task(pid, pid).process.tgid,
            },
        )
        .unwrap(),
        ..alarm_dequeue_of(pid, sequence, reverie::SignalConsumer::ReturnToUser)
    }
}

/// What `recv_signal_dequeued` does when the single thread `tid` of process
/// `tid` takes `dequeue` in an observation: it validates the effect, records
/// the thread's time, `elapsed` nanoseconds after `start`, and acknowledges
/// the dequeue.
fn take_signal(
    s: &mut Scheduler,
    global: &Mutex<GlobalTime>,
    start: LogicalTime,
    (tid, mm): (DetTid, MmId),
    elapsed: u64,
    dequeue: reverie::SignalDequeue,
) {
    let identity = task(tid.as_raw(), tid.as_raw());
    assert_eq!(
        s.real_timers
            .validate_effect(tid, tid, mm, identity, identity.process),
        Ok(true)
    );
    global
        .lock()
        .unwrap()
        .update_global_time(tid, start + at(elapsed), LogicalTime::ZERO);
    let sequence = dequeue.sequence;
    assert_eq!(
        s.apply_signal_dequeue(tid, tid, dequeue),
        Ok(super::real_timer::DequeueAck::Applied { sequence })
    );
}

/// The case of https://github.com/rrnewton/hermit/issues/4929's review
/// round 2 (Claude, first finding): a gate opened by a refresh pass's own
/// selection, not by maintenance stage 1. Run as
/// `controlled_restart_witness` runs, with the real controlled turn loop on
/// one host thread and the observed workers on another.
///
/// Parent R (process 100) is queued with an ordinary request. Two children
/// are parked in `pause`: X (process 300), for which a SIGALRM that `kill`
/// sent before this turn is pending, and W (process 200), which armed a
/// 503 us `ITIMER_REAL` with a 2 ms interval. Turn 1 charges R's 500 us
/// turn. Stage 1 finds nothing due, so the turn's one timed pop is still
/// unused, and observes X, and the loop waits for X's gate. X takes its
/// SIGALRM (it has no timer of its own), reports 5 us and files its
/// delivery's request. The refresh pass after that wait commits 505 us,
/// pops W's expiry, publishes it, which makes W a recipient, and observes W
/// in its own selection. W takes the copy, reports 5 us and files its
/// delivery's request.
///
/// `worker_first` decides which side reaches W's gate first. With it, the
/// daemon pauses at the loop top before that refresh pass
/// (`observation_gate_pause`, armed as X files) and again at the loop top
/// after it (armed while the daemon is paused before it), until W has filed,
/// so its next look finds W's gate refilled. Without it, W waits until the
/// daemon has looked and is waiting on W's gate.
///
/// Returns the records of turn 1, which grants R, and of turn 2, and how
/// many times turn 1's loop waited.
fn refresh_pass_restart_witness(worker_first: bool) -> (Vec<RestartTurn>, u64) {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use futures::FutureExt;

    let (mut s, backend) = fixture();
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let start = global.lock().unwrap().as_nanos();
    let (r, _, _) = add(&mut s, 100, 100);
    s.next_turns[&r].req.put(Ok(Resources::new(r)));
    s.runqueue_push_back(r);
    let (w, w_mm, w_site) = add_process_child(&mut s, 100, 200);
    let w_response = parked_pause(&mut s, w, w_mm, w_site);
    s.replace_real_timer(w, w, start, at(503_000), at(2_000_000), Signal::SIGALRM)
        .unwrap();
    backend
        .recipients_on_alarm
        .lock()
        .unwrap()
        .push(SignalRecipient {
            task: task(200, 200),
        });
    let (x, x_mm, x_site) = add_process_child(&mut s, 100, 300);
    let x_response = parked_pause(&mut s, x, x_mm, x_site);
    backend.recipients.lock().unwrap().push(SignalRecipient {
        task: task(300, 300),
    });
    let before_refresh = Arc::new(AtomicBool::new(false));
    let after_refresh = Arc::new(AtomicBool::new(false));
    let scheduler = Arc::new(Mutex::new(s));
    let observed = |response: &Ivar<SchedResponse>| {
        wait_until(|| response.is_full());
        selected(response);
    };
    let last = Ok(Resources::new(r));
    let granted = std::thread::scope(|scope| {
        let daemon = scope.spawn(|| {
            futures::executor::block_on(do_a_turn_blocking(
                scheduler.clone(),
                global.clone(),
                &last,
            ))
        });
        // Stage 1 observes X, and the loop waits for X's gate.
        observed(&x_response);
        wait_until(|| scheduler.lock().unwrap().controlled_turn_waits > 0);
        {
            let mut s = scheduler.lock().unwrap();
            if worker_first {
                s.observation_gate_pause = Some(before_refresh.clone());
            }
            take_signal(
                &mut s,
                &global,
                start,
                (x, x_mm),
                5_000,
                kill_alarm_dequeue_of(300, 1),
            );
            file_inbound_signal(&mut s, x, x_mm);
        }
        if worker_first {
            // While it pauses, the loop holds its own clone of the flag.
            wait_until(|| Arc::strong_count(&before_refresh) == 3);
            scheduler.lock().unwrap().observation_gate_pause = Some(after_refresh.clone());
            before_refresh.store(true, Ordering::Release);
        }
        // The refresh pass pops W's expiry and observes W.
        observed(&w_response);
        if !worker_first {
            wait_until(|| scheduler.lock().unwrap().controlled_turn_waits > 1);
        }
        {
            let mut s = scheduler.lock().unwrap();
            take_signal(
                &mut s,
                &global,
                start,
                (w, w_mm),
                5_000,
                alarm_dequeue_of(200, 1, reverie::SignalConsumer::ReturnToUser),
            );
            file_inbound_signal(&mut s, w, w_mm);
        }
        after_refresh.store(true, Ordering::Release);
        daemon.join().unwrap().expect("the turn grants a thread")
    });
    let record = |granted: DetTid| {
        let s = scheduler.lock().unwrap();
        assert!(!s.backend_failed());
        RestartTurn::record(&s, &backend, &global, start, granted, w)
    };
    let waits = scheduler.lock().unwrap().controlled_turn_waits;
    let mut turns = vec![record(granted.tid)];
    // Turn 2: R files its next request, and the next thread is granted.
    scheduler.lock().unwrap().next_turns[&r]
        .req
        .put(Ok(Resources::new(r)));
    let last = Ok(granted);
    let next = do_a_turn_blocking(scheduler.clone(), global.clone(), &last)
        .now_or_never()
        .expect("every request is filled")
        .expect("the turn grants a thread");
    turns.push(record(next.tid));
    (turns, waits)
}

/// https://github.com/rrnewton/hermit/issues/4929, review round 2 (Claude,
/// first finding): a parked SIGALRM whose expiry a refresh pass pops, and
/// whose thread that pass's own selection observes, restarts its timer
/// whichever side reaches the gate first (`refresh_pass_restart_witness`).
/// C sets `refresh` after the refresh pass's selection, as after stage 1's.
///
/// Both orders restart W's timer at 2.503 ms, the first point of its grid
/// after the committed time of 510 us (R's 500 us turn charge and X's and
/// W's 5 us each), and give R a 1.993 ms slice cap. Without the refresh
/// after a refresh pass's selection opens a gate, the worker-first order
/// grants R at 505 us with W's timer still dequeued: no restart deadline, so
/// no slice cap from it.
#[test]
fn a_restart_observed_by_a_refresh_pass_does_not_depend_on_which_side_reaches_the_gate_first() {
    let (worker_first, worker_first_waits) = refresh_pass_restart_witness(true);
    let (daemon_first, daemon_first_waits) = refresh_pass_restart_witness(false);
    assert_eq!(worker_first, daemon_first);
    // Each order was forced. Both waited for X's gate; only the daemon-first
    // order also waited for W's.
    assert_eq!(worker_first_waits, 1);
    assert_eq!(daemon_first_waits, 2);
    let restart = [(
        at(2_503_000),
        timed_waiters::TimedEvent::SignalEvt(
            timed_waiters::SignalTimerId::Alarm(DetPid::from_raw(200)),
            timed_waiters::SignalTarget::Process(DetTid::from_raw(200)),
            timed_waiters::TimerSignal::Send(Signal::SIGALRM),
        ),
    )];
    let first = &worker_first[0];
    assert_eq!(first.selected, DetTid::from_raw(100));
    assert_eq!(first.committed, at(510_000));
    assert_eq!(first.timed, restart);
    assert_eq!(first.slice_cap, Some(Duration::from_nanos(1_993_000)));
    assert_eq!(
        first.timer,
        super::real_timer::ItimerSnapshot {
            remaining: at(1_993_000),
            interval: at(2_000_000),
        }
    );
    assert_eq!(first.publications, 1);
    // Turn 2 charges R's turn and grants X's delivery; the restart stays.
    let second = &worker_first[1];
    assert_eq!(second.selected, DetTid::from_raw(300));
    assert_eq!(second.committed, at(1_010_000));
    assert_eq!(second.timed, restart);
    assert_eq!(second.publications, 1);
}

/// A gate opened by the selection after the empty-queue step (maintenance
/// stage 4), the third of the four places C sets `refresh`
/// (https://github.com/rrnewton/hermit/issues/4929, review round 2). W,
/// alone in process 100, is parked in `pause` with a 3 ms `ITIMER_REAL` and
/// a 2 ms interval. Turn 1 charges W's previous 500 us turn, and stage 1
/// finds nothing due. With nothing runnable, the empty-queue step jumps to
/// 3 ms, pops W's expiry and publishes it, which makes W a recipient, and the
/// selection after it observes W. Because the empty-queue step returned
/// `SkipTurn`, the next loop top ends the turn before any quiesced pass, so
/// the `refresh` set there is never read.
///
/// W takes the copy, reports 5 us and files its delivery's request. With
/// `worker_first`, it does so before turn 1's next loop top: the daemon
/// pauses there (`observation_gate_pause`) until W has filed. Without it, W
/// files after turn 2's loop has looked and is waiting on W's gate. Returns
/// turn 2's grant, and how many times turn 2's loop waited.
fn empty_queue_restart_witness(worker_first: bool) -> (RestartTurn, u64) {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use futures::FutureExt;

    let (mut s, backend) = fixture();
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let start = global.lock().unwrap().as_nanos();
    let (w, mm, site) = add(&mut s, 100, 100);
    let response = parked_pause(&mut s, w, mm, site);
    s.replace_real_timer(w, w, start, at(3_000_000), at(2_000_000), Signal::SIGALRM)
        .unwrap();
    backend
        .recipients_on_alarm
        .lock()
        .unwrap()
        .push(SignalRecipient {
            task: task(100, 100),
        });
    let release = Arc::new(AtomicBool::new(false));
    if worker_first {
        s.observation_gate_pause = Some(release.clone());
    }
    let scheduler = Arc::new(Mutex::new(s));
    let take = || {
        let mut s = scheduler.lock().unwrap();
        take_signal(
            &mut s,
            &global,
            start,
            (w, mm),
            5_000,
            alarm_dequeue(1, reverie::SignalConsumer::ReturnToUser),
        );
        file_inbound_signal(&mut s, w, mm);
    };
    let last = Ok(Resources::new(w));
    let turn1 = std::thread::scope(|scope| {
        let daemon = scope.spawn(|| {
            futures::executor::block_on(do_a_turn_blocking(
                scheduler.clone(),
                global.clone(),
                &last,
            ))
        });
        if worker_first {
            wait_until(|| response.is_full());
            take();
            release.store(true, Ordering::Release);
        }
        daemon.join().unwrap()
    });
    assert!(turn1.is_err(), "the empty-queue step ends the turn");
    selected(&response);
    {
        let s = scheduler.lock().unwrap();
        assert!(!s.backend_failed());
        // The jump to 3 ms, and W's 5 us once W has reported them.
        let reported = if worker_first { 5_000 } else { 0 };
        assert_eq!(
            global.lock().unwrap().as_nanos(),
            start + at(3_000_000 + reported)
        );
        assert_eq!(backend.publications.lock().unwrap().len(), 1);
    }
    let waits = scheduler.lock().unwrap().controlled_turn_waits;
    let last = Err(SkipTurn);
    let mut turn2 = Box::pin(do_a_turn_blocking(scheduler.clone(), global.clone(), &last));
    if !worker_first {
        assert!(turn2.as_mut().now_or_never().is_none(), "W's gate");
        take();
    }
    let granted = turn2
        .as_mut()
        .now_or_never()
        .expect("every request is filled")
        .expect("the turn grants a thread");
    drop(turn2);
    let s = scheduler.lock().unwrap();
    assert!(!s.backend_failed());
    (
        RestartTurn::record(&s, &backend, &global, start, granted.tid, w),
        s.controlled_turn_waits - waits,
    )
}

/// https://github.com/rrnewton/hermit/issues/4929, review round 2: a gate
/// opened by the selection after the empty-queue step is reconciled by the
/// next turn's first bump, whichever side reaches it first
/// (`empty_queue_restart_witness`). The empty-queue step ends its turn
/// before any quiesced pass reads `refresh`, so the restart does not depend
/// on C's line at that stage: both orders restart W's timer at 5 ms, the
/// first point of its grid after the committed time of 3.005 ms (the jump to
/// 3 ms and W's 5 us; a turn after a skip is not charged), and grant W's
/// delivery with a 1.995 ms slice cap.
#[test]
fn a_gate_opened_after_the_empty_queue_step_is_reconciled_by_the_next_turn() {
    let (worker_first, worker_first_waits) = empty_queue_restart_witness(true);
    let (daemon_first, daemon_first_waits) = empty_queue_restart_witness(false);
    assert_eq!(worker_first, daemon_first);
    assert_eq!(worker_first_waits, 0);
    assert_eq!(daemon_first_waits, 1);
    assert_eq!(worker_first.selected, DetTid::from_raw(100));
    assert_eq!(worker_first.committed, at(3_005_000));
    assert_eq!(
        worker_first.timed,
        [(
            at(5_000_000),
            timed_waiters::TimedEvent::SignalEvt(
                timed_waiters::SignalTimerId::Alarm(DetPid::from_raw(100)),
                timed_waiters::SignalTarget::Process(DetTid::from_raw(100)),
                timed_waiters::TimerSignal::Send(Signal::SIGALRM),
            )
        )]
    );
    assert_eq!(
        worker_first.slice_cap,
        Some(Duration::from_nanos(1_995_000))
    );
    assert_eq!(
        worker_first.timer,
        super::real_timer::ItimerSnapshot {
            remaining: at(1_995_000),
            interval: at(2_000_000),
        }
    );
    assert_eq!(worker_first.publications, 1);
}

/// https://github.com/rrnewton/hermit/issues/4929 (review round 1): the
/// restart of a parked thread's timer is queued before the empty-queue jump
/// that could pass it. W, alone in process 100, parked in `pause` with a
/// 100 us, 2 ms `ITIMER_REAL`, takes the copy for a handler. Its delivery's
/// hook refills the observation's gate, and the refresh pass after it, at
/// 500 us, restarts the timer at 2.1 ms, the first point of its grid after
/// 500 us. W's delivery is granted, its observation finishes, and its
/// handler then sleeps for 10 ms (a parked `nanosleep`, until 10.55 ms). A
/// queued sleep is registered when step 3 chooses it, so that turn ends
/// without a grant, having registered W's sleep. The next turn finds nothing
/// runnable, and its jump goes to the restarted deadline, not to W's, and
/// pops it.
#[test]
fn a_parked_alarm_restart_is_queued_before_the_empty_queue_jump() {
    use futures::FutureExt;

    let (mut s, backend) = fixture();
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let start = global.lock().unwrap().as_nanos();
    let (w, mm, site) = add(&mut s, 100, 100);
    let response = parked_pause(&mut s, w, mm, site);
    s.replace_real_timer(w, w, start, at(100_000), at(2_000_000), Signal::SIGALRM)
        .unwrap();
    backend.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    let scheduler = Arc::new(Mutex::new(s));
    let timed = |s: &Scheduler| {
        s.blocked
            .timed_waiters
            .iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>()
    };
    let last = Ok(Resources::new(w));
    let mut turn = Box::pin(do_a_turn_blocking(scheduler.clone(), global.clone(), &last));
    assert!(
        turn.as_mut().now_or_never().is_none(),
        "W's observation gate"
    );
    let control = selected(&response);
    {
        let mut s = scheduler.lock().unwrap();
        assert_eq!(s.committed_time, start + at(500_000));
        assert_eq!(backend.publications.lock().unwrap().len(), 1);
        s.apply_signal_dequeue(
            w,
            w,
            alarm_dequeue(1, reverie::SignalConsumer::ReturnToUser),
        )
        .unwrap();
        file_inbound_signal(&mut s, w, mm);
    }
    let delivery = turn
        .as_mut()
        .now_or_never()
        .expect("the hook refilled the gate")
        .expect("W's delivery is granted");
    drop(turn);
    assert_eq!(delivery.tid, w);
    assert_eq!(timed(&scheduler.lock().unwrap()), [start + at(2_100_000)]);
    // In its delivery's turn, W's observation finishes into its handler,
    // which then sleeps.
    let ack = post_finish(
        &mut scheduler.lock().unwrap(),
        w,
        mm,
        &control,
        caught(&control),
    );
    let last = Ok(delivery);
    let mut turn = Box::pin(do_a_turn_blocking(scheduler.clone(), global.clone(), &last));
    assert!(turn.as_mut().now_or_never().is_none(), "W's handler");
    assert_eq!(ack.try_read(), Some(Ok(FinishAck::Interrupted)));
    let wake = start + at(10_550_000);
    {
        let mut s = scheduler.lock().unwrap();
        handler_entered(&mut s, &backend, &control);
        global
            .lock()
            .unwrap()
            .update_global_time(w, start + at(50_000), LogicalTime::ZERO);
        file_parked_sleep(&mut s, w, mm, site, wake);
    }
    let result = turn.as_mut().now_or_never().expect("W's sleep is chosen");
    assert!(result.is_err(), "a blocking sleep is no grant");
    drop(turn);
    {
        let s = scheduler.lock().unwrap();
        assert!(!s.backend_failed());
        assert_eq!(timed(&s), [start + at(2_100_000), wake]);
        assert!(s.run_queue.is_empty());
        assert_eq!(backend.publications.lock().unwrap().len(), 1);
    }
    let result = do_a_turn_blocking(scheduler.clone(), global.clone(), &Err(SkipTurn))
        .now_or_never()
        .expect("nothing is runnable");
    assert!(
        result.is_err(),
        "the empty queue ends the turn without a grant"
    );
    let s = scheduler.lock().unwrap();
    assert!(!s.backend_failed());
    let now = global.lock().unwrap().as_nanos();
    assert_eq!(now, start + at(2_100_000), "the empty-queue jump");
    assert_eq!(backend.publications.lock().unwrap().len(), 2);
    assert_eq!(timed(&s), [wake]);
    assert_eq!(
        s.itimer_snapshot(w, now).unwrap(),
        super::real_timer::ItimerSnapshot {
            remaining: at(0),
            interval: at(2_000_000),
        }
    );
}

/// https://github.com/rrnewton/hermit/issues/4929 (review round 1): a later
/// refresh pass does not move a parked thread's timer restart. As in
/// `a_parked_alarm_restart_does_not_depend_on_which_side_reaches_the_gate_first`,
/// parent R is queued and its child W takes its timer's copy, whose delivery
/// hook refills the gate: the refresh pass at 500 us restarts W's timer at
/// 2.1 ms. That pass also observes X, a third process asleep until 50 ms,
/// which takes no signal and resumes its sleep after 1.8 ms of its own work,
/// so a second refresh pass runs at 2.3 ms. The restart stays at 2.1 ms (a
/// restart there would be 4.1 ms), and the turn's one timed pop is spent, so
/// R is granted with X's deadline as its slice cap. The next charged pass
/// pops the timer.
#[test]
fn a_later_refresh_pass_does_not_move_a_parked_alarm_restart() {
    use futures::FutureExt;

    let (mut s, backend) = fixture();
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let start = global.lock().unwrap().as_nanos();
    let (r, _, _) = add(&mut s, 100, 100);
    s.next_turns[&r].req.put(Ok(Resources::new(r)));
    s.runqueue_push_back(r);
    let (w, wmm, wsite) = add_process_child(&mut s, 100, 200);
    let w_response = parked_pause(&mut s, w, wmm, wsite);
    s.replace_real_timer(w, w, start, at(100_000), at(2_000_000), Signal::SIGALRM)
        .unwrap();
    let (x, xmm, xsite) = add_process_child(&mut s, 100, 300);
    let x_response = sleep(&mut s, x, xmm, xsite, (start + at(50_000_000)).as_nanos());
    backend.recipients.lock().unwrap().push(SignalRecipient {
        task: task(200, 200),
    });
    let scheduler = Arc::new(Mutex::new(s));
    let alarm = |s: &Scheduler| {
        s.blocked
            .timed_waiters
            .iter()
            .filter(|(_, event)| {
                matches!(
                    event,
                    timed_waiters::TimedEvent::SignalEvt(
                        timed_waiters::SignalTimerId::Alarm(_),
                        ..
                    )
                )
            })
            .map(|(deadline, _)| deadline)
            .collect::<Vec<_>>()
    };
    let last = Ok(Resources::new(r));
    let mut turn = Box::pin(do_a_turn_blocking(scheduler.clone(), global.clone(), &last));
    assert!(
        turn.as_mut().now_or_never().is_none(),
        "W's observation gate"
    );
    selected(&w_response);
    {
        let mut s = scheduler.lock().unwrap();
        s.apply_signal_dequeue(
            w,
            w,
            alarm_dequeue_of(200, 1, reverie::SignalConsumer::ReturnToUser),
        )
        .unwrap();
        // The backend now reports a signal pending for process 300.
        backend.recipients.lock().unwrap().clear();
        backend.recipients.lock().unwrap().push(SignalRecipient {
            task: task(300, 300),
        });
        file_inbound_signal(&mut s, w, wmm);
    }
    assert!(
        turn.as_mut().now_or_never().is_none(),
        "X's observation gate"
    );
    let x_control = selected(&x_response);
    let ack = {
        let mut s = scheduler.lock().unwrap();
        assert_eq!(s.committed_time, start + at(500_000));
        assert_eq!(alarm(&s), [start + at(2_100_000)]);
        backend.recipients.lock().unwrap().clear();
        global
            .lock()
            .unwrap()
            .update_global_time(x, start + at(1_800_000), LogicalTime::ZERO);
        post_finish(
            &mut s,
            x,
            xmm,
            &x_control,
            ObservationFinish::ResumeSameWait,
        )
    };
    assert!(turn.as_mut().now_or_never().is_none(), "X's resume barrier");
    let ticket = match ack.try_read().unwrap().unwrap() {
        FinishAck::AwaitResume(ticket) => ticket,
        other => panic!("{other:?}"),
    };
    scheduler
        .lock()
        .unwrap()
        .post_control(
            x,
            xmm,
            ControlIntent::Resume {
                ticket,
                site: xsite,
                response: Ivar::new(),
            },
        )
        .unwrap();
    let granted = turn
        .as_mut()
        .now_or_never()
        .expect("every request is filled")
        .expect("R is granted");
    drop(turn);
    assert_eq!(granted.tid, r);
    {
        let s = scheduler.lock().unwrap();
        assert!(!s.backend_failed());
        assert_eq!(s.committed_time, start + at(2_300_000));
        assert_eq!(alarm(&s), [start + at(2_100_000)]);
        assert_eq!(backend.publications.lock().unwrap().len(), 1);
        assert_eq!(
            s.blocked.timed_waiters.thread_deadline(x),
            Some(start + at(50_000_000))
        );
        assert_eq!(
            s.deadline_hints.get(&r).copied(),
            Some(Duration::from_nanos(50_000_000 - 2_300_000))
        );
    }
    scheduler.lock().unwrap().next_turns[&r]
        .req
        .put(Ok(Resources::new(r)));
    let last = Ok(granted);
    let next = do_a_turn_blocking(scheduler.clone(), global.clone(), &last)
        .now_or_never()
        .expect("every request is filled")
        .expect("a grant");
    assert_eq!(next.tid, w, "W's delivery");
    let s = scheduler.lock().unwrap();
    assert_eq!(s.committed_time, start + at(2_800_000));
    assert_eq!(backend.publications.lock().unwrap().len(), 2);
    assert!(alarm(&s).is_empty());
    assert!(!s.backend_failed());
}

#[test]
fn masked_leader_and_unrelated_process_do_not_prevent_worker_sleep_selection() {
    let (mut s, b) = fixture();
    let (leader, mm, ls) = add(&mut s, 100, 100);
    let (worker, _, ws) = add(&mut s, 100, 101);
    add(&mut s, 200, 200);
    let lr = sleep(&mut s, leader, mm, ls, 100);
    let wr = sleep(&mut s, worker, mm, ws, 100);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 101),
    });
    s.committed_time = at(10);
    s.select_parked_alarm().unwrap();
    assert!(lr.try_read().is_none());
    let c = selected(&wr);
    assert_eq!(c.permit.task, task(100, 101));
    assert_eq!(
        s.blocked.timed_waiters.thread_deadline(leader),
        Some(at(100))
    );
    assert_eq!(s.blocked.timed_waiters.thread_deadline(worker), None);
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[c.permit]);
    // A repeated maintenance pass cannot select a second recipient for the same pending signal.
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    assert_eq!(b.permits.lock().unwrap().len(), 1);
}

#[test]
fn no_handler_reenrolls_original_sleep_and_late_timeout_wins() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let response = sleep(&mut s, tid, mm, site, 100);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.committed_time = at(10);
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site,
            finish: ObservationFinish::ResumeSameWait,
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    let ticket = match ack.try_read().unwrap().unwrap() {
        FinishAck::AwaitResume(t) => t,
        other => panic!("{other:?}"),
    };
    assert!(b.permits.lock().unwrap().is_empty());
    s.committed_time = at(150);
    let resumed = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Resume {
            ticket,
            site,
            response: resumed.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert_eq!(s.blocked.timed_waiters.thread_deadline(tid), Some(at(100)));
    b.recipients.lock().unwrap().clear();
    s.step2b_process_timed(&committed_clock(&s));
    assert_eq!(s.blocked.timed_waiters.thread_deadline(tid), None);
    assert!(resumed.try_read().is_none());
    assert!(s.run_queue.contains_tid(tid));
    assert_eq!(
        s.next_turns[&tid]
            .req
            .try_read()
            .unwrap()
            .unwrap()
            .resources
            .keys()
            .next(),
        Some(&ResourceID::SleepUntil(at(100)))
    );
}

#[test]
fn natural_due_sleep_is_not_rewritten_as_interrupted() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let response = sleep(&mut s, tid, mm, site, 10);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.committed_time = at(10);
    s.step2b_process_timed(&committed_clock(&s));
    s.select_parked_alarm().unwrap();
    assert!(response.try_read().is_none());
    assert!(b.permits.lock().unwrap().is_empty());
    assert!(s.run_queue.contains_tid(tid));
}

#[test]
fn return_permit_requires_real_grant_and_receipt_is_exactly_once() {
    let (mut s, b) = fixture();
    let (tid, _, _) = add(&mut s, 100, 100);
    let (peer, _, _) = add(&mut s, 100, 101);
    s.parked.running = Some(tid);
    assert_eq!(s.authorize_signal_boundary(task(100, 101)).unwrap(), None);
    let permit = s
        .authorize_signal_boundary(task(100, 100))
        .unwrap()
        .unwrap();
    assert_eq!(permit.site, None);
    assert_eq!(b.permits.lock().unwrap().len(), 1);
    let receipt = SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Caught,
    };
    s.consume_signal_boundary(receipt).unwrap();
    s.consume_signal_boundary(receipt).unwrap();
    assert!(
        s.consume_signal_boundary(SignalBoundaryReceipt {
            outcome: SignalBoundaryOutcome::NoHandler,
            ..receipt
        })
        .is_err()
    );
    assert!(s.next_turns.contains_key(&peer));
}

/// Every input a later scheduling decision reads from the signal protocol,
/// with permit and continuation numbering left out: the run queue, each
/// thread's turn owner and transports, which threads own a parked request,
/// the permits by holder and task, exit fences, pending admissions, timed
/// waits, and failure.
fn scheduling_inputs(s: &Scheduler) -> String {
    let owners: Vec<_> = s
        .next_turns
        .iter()
        .map(|(tid, turn)| {
            let owner = match turn.protocol.owner {
                NextTurnOwner::Ordinary => "ordinary",
                NextTurnOwner::Observation { .. } => "observation",
                NextTurnOwner::ReturningCaught { .. } => "returning-caught",
            };
            (
                *tid,
                owner,
                turn.req.is_full(),
                turn.resp.is_full(),
                s.blocked.timed_waiters.thread_deadline(*tid),
            )
        })
        .collect();
    let requests: Vec<_> = s.parked.requests.keys().map(|id| id.dettid).collect();
    let permits: Vec<_> = s
        .parked
        .permits
        .iter()
        .map(|(tid, permit)| (*tid, permit.task, permit.site))
        .collect();
    format!(
        "{:?}",
        (
            s.run_queue.tids().collect::<Vec<_>>(),
            owners,
            requests,
            permits,
            s.parked.exit_fences.keys().collect::<Vec<_>>(),
            &s.pending_run_queue_admissions,
            s.parked.failure,
            s.backend_failed(),
        )
    )
}

/// The round trip reverie-kvm leaves out at an empty user-return boundary,
/// because Detcore opts in to `skip_empty_signal_boundaries`: an
/// authorization inside the holder's own serial grant and a `NoHandler`
/// receipt. Run in one of two identical schedulers and skipped in the other,
/// it publishes nothing and leaves every scheduling input the same. Only the
/// permit counter and the stored receipt differ, and the next alarm selection
/// and a sibling's exit reservation decide the same way in both.
#[test]
fn an_empty_boundary_round_trip_changes_only_the_nonce_and_the_receipt() {
    let world = || {
        let (mut s, b) = fixture();
        let (holder, _, _) = add(&mut s, 100, 100);
        let (sleeper, mm, site) = add(&mut s, 100, 101);
        let (exiting, _, _) = add(&mut s, 100, 102);
        add(&mut s, 200, 200);
        let response = sleep(&mut s, sleeper, mm, site, 100);
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 101),
        });
        // The holder was granted its turn and has not filed its next request.
        s.runqueue_push_back(holder);
        s.parked.running = Some(holder);
        assert!(s.holds_serial_grant(holder));
        (s, b, response, holder, exiting)
    };
    let (mut asked, asked_backend, asked_response, holder, exiting) = world();
    let (mut skipped, skipped_backend, skipped_response, ..) = world();
    assert_eq!(scheduling_inputs(&asked), scheduling_inputs(&skipped));

    let nonce = asked.parked.nonce;
    let permit = asked
        .authorize_signal_boundary(task(100, 100))
        .unwrap()
        .expect("the thread holding the serial grant is authorized");
    assert!(
        asked_backend.publications.lock().unwrap().is_empty()
            && asked_backend.timer_publications.lock().unwrap().is_empty()
            && asked_backend.child_publications.lock().unwrap().is_empty(),
        "authorization publishes nothing"
    );
    let receipt = SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::NoHandler,
    };
    asked.consume_signal_boundary(receipt).unwrap();
    // The backend releases its reservation after the receipt, as reverie-kvm's
    // `finish_signal_boundary` does.
    asked_backend.release_delivery(permit).unwrap();

    assert_eq!(asked.parked.nonce, nonce + 1);
    assert_eq!(skipped.parked.nonce, nonce);
    assert_eq!(asked.parked.completed.get(&holder), Some(&receipt));
    assert!(skipped.parked.completed.is_empty());
    assert_eq!(scheduling_inputs(&asked), scheduling_inputs(&skipped));
    assert!(asked_backend.permits.lock().unwrap().is_empty());
    assert!(skipped_backend.permits.lock().unwrap().is_empty());
    for s in [&asked, &skipped] {
        assert!(s.holds_serial_grant(holder));
    }

    // The next maintenance pass picks the same alarm recipient in both; only
    // the numbering of its permit and continuation differs, still increasing.
    for s in [&mut asked, &mut skipped] {
        s.committed_time = at(10);
        s.select_parked_alarm().unwrap();
    }
    let (a, k) = (selected(&asked_response), selected(&skipped_response));
    assert_eq!(a.permit.task, k.permit.task);
    assert_eq!(a.permit.site, k.permit.site);
    assert_eq!(a.continuation.dettid, k.continuation.dettid);
    assert_eq!(a.permit.sequence, k.permit.sequence + 1);
    assert_eq!(scheduling_inputs(&asked), scheduling_inputs(&skipped));
    // A sibling's exit is reserved the same way in both, beside the observed
    // wait's permit.
    let process = DetPid::from_raw(100);
    let mm = MmId::initial(process);
    assert_eq!(
        asked.reserve_exit_boundary(exiting, process, mm, false),
        Ok(super::signal_control::ExitReserveMode::Controlled)
    );
    assert_eq!(
        skipped.reserve_exit_boundary(exiting, process, mm, false),
        Ok(super::signal_control::ExitReserveMode::Controlled)
    );
    assert_eq!(scheduling_inputs(&asked), scheduling_inputs(&skipped));
}

/// The skipped round trip of an empty boundary while siblings hold permits
/// of their own: one waits for its `InboundSignal` turn after its boundary,
/// and one is an observed parked wait returning into a caught handler. With
/// permits per thread the round trip is authorized beside them, and run in
/// one of two identical schedulers and skipped in the other, it leaves every
/// scheduling input the same. The siblings' receipts, the observed thread's
/// return boundary and another sibling's exit reservation then decide the
/// same way in both.
#[test]
fn an_empty_boundary_round_trip_beside_sibling_permits_changes_only_the_nonce_and_the_receipt() {
    let world = || {
        let (mut s, b) = fixture();
        let (holder, _, _) = add(&mut s, 100, 100);
        let (sibling, _, _) = add(&mut s, 100, 101);
        let (sleeper, mm, site) = add(&mut s, 100, 102);
        let (exiting, _, _) = add(&mut s, 100, 103);
        add(&mut s, 200, 200);
        // The sibling's boundary, waiting for its InboundSignal turn.
        s.parked.running = Some(sibling);
        let sibling_permit = s
            .authorize_signal_boundary(task(100, 101))
            .unwrap()
            .unwrap();
        s.next_turns[&sibling]
            .req
            .put(Ok(inbound_request(sibling, libc::SIGUSR1)));
        s.run_queue.push_back(sibling, DEFAULT_PRIORITY);
        // The sleeper's observed wait, interrupted for a caught alarm.
        let response = sleep(&mut s, sleeper, mm, site, 100);
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 102),
        });
        s.committed_time = at(10);
        s.select_parked_alarm().unwrap();
        let observed = selected(&response);
        let ack = Ivar::new();
        s.post_control(
            sleeper,
            mm,
            ControlIntent::Finish {
                wait: observed.continuation,
                lease: observed.lease,
                site,
                finish: ObservationFinish::InterruptForCaught {
                    selection: reverie::PreparedSignalToken {
                        site,
                        selection_nonce: 1,
                    },
                    dequeue: None,
                },
                ack: ack.clone(),
            },
        )
        .unwrap();
        s.drain_control_intents();
        assert_eq!(ack.try_read(), Some(Ok(FinishAck::Interrupted)));
        // The holder was granted its turn and has not filed its next request.
        s.runqueue_push_back(holder);
        s.parked.running = Some(holder);
        assert!(s.holds_serial_grant(holder));
        (s, b, holder, sibling_permit, observed.permit, exiting, mm)
    };
    let (mut asked, asked_backend, holder, sibling_permit, observed_permit, exiting, mm) = world();
    let (mut skipped, skipped_backend, ..) = world();
    assert_eq!(scheduling_inputs(&asked), scheduling_inputs(&skipped));

    let nonce = asked.parked.nonce;
    let permit = asked
        .authorize_signal_boundary(task(100, 100))
        .unwrap()
        .expect("the holder is authorized beside its siblings' permits");
    let receipt = SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::NoHandler,
    };
    asked.consume_signal_boundary(receipt).unwrap();
    asked_backend.release_delivery(permit).unwrap();
    assert_eq!(asked.parked.nonce, nonce + 1);
    assert_eq!(skipped.parked.nonce, nonce);
    assert_eq!(asked.parked.completed.get(&holder), Some(&receipt));
    assert_eq!(scheduling_inputs(&asked), scheduling_inputs(&skipped));
    assert_eq!(
        asked_backend.permits.lock().unwrap().as_slice(),
        skipped_backend.permits.lock().unwrap().as_slice()
    );
    assert_eq!(
        asked_backend.permits.lock().unwrap().as_slice(),
        &[sibling_permit, observed_permit]
    );

    let process = DetPid::from_raw(100);
    for s in [&mut asked, &mut skipped] {
        assert!(s.holds_serial_grant(holder));
        s.consume_signal_boundary(SignalBoundaryReceipt {
            permit: sibling_permit,
            outcome: SignalBoundaryOutcome::Caught,
        })
        .unwrap();
        assert_eq!(
            s.authorize_signal_boundary(task(100, 102)).unwrap(),
            Some(observed_permit)
        );
        s.consume_signal_boundary(SignalBoundaryReceipt {
            permit: observed_permit,
            outcome: SignalBoundaryOutcome::Caught,
        })
        .unwrap();
        assert_eq!(
            s.reserve_exit_boundary(exiting, process, mm, false),
            Ok(super::signal_control::ExitReserveMode::Controlled)
        );
        assert!(!s.backend_failed());
    }
    assert_eq!(scheduling_inputs(&asked), scheduling_inputs(&skipped));
}

/// Why the receipt's cleanup (`parked.requests` and the turn owner) has
/// nothing to do at a boundary reverie-kvm skips. While a parked wait is
/// observed, its request and `Observation` owner come with a permit; when it
/// returns into a caught handler, its `ReturningCaught` owner keeps that
/// permit. With a permit held, the backend does not skip the boundary. Once the wait resumes, the grant that answers it settles the
/// request, so the empty boundary after that grant has no request or owner
/// left to clean. A leaked request would make `complete_signal_exec` refuse
/// the thread's next exec.
#[test]
fn a_parked_wait_leaves_nothing_for_an_empty_boundary_to_clean() {
    let observe = || {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        let response = sleep(&mut s, tid, mm, site, 100);
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        s.committed_time = at(10);
        s.select_parked_alarm().unwrap();
        let c = selected(&response);
        assert!(s.parked.requests.keys().any(|id| id.dettid == tid));
        assert!(matches!(
            s.next_turns[&tid].protocol.owner,
            NextTurnOwner::Observation { .. }
        ));
        assert_eq!(s.parked.permits.get(&tid), Some(&c.permit));
        assert_eq!(b.permits.lock().unwrap().as_slice(), &[c.permit]);
        (s, b, tid, mm, site, c)
    };

    // Interrupted for a caught handler: the permit stays until the receipt.
    let (mut s, b, tid, mm, site, c) = observe();
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site,
            finish: ObservationFinish::InterruptForCaught {
                selection: reverie::PreparedSignalToken {
                    site,
                    selection_nonce: 1,
                },
                dequeue: None,
            },
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert_eq!(ack.try_read(), Some(Ok(FinishAck::Interrupted)));
    assert!(matches!(
        s.next_turns[&tid].protocol.owner,
        NextTurnOwner::ReturningCaught { .. }
    ));
    assert!(!s.parked.requests.keys().any(|id| id.dettid == tid));
    assert_eq!(s.parked.permits.get(&tid), Some(&c.permit));
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[c.permit]);

    // No handler: the same wait resumes, without a permit, and its grant
    // settles the request.
    let (mut s, b, tid, mm, site, c) = observe();
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site,
            finish: ObservationFinish::ResumeSameWait,
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    let ticket = match ack.try_read().unwrap().unwrap() {
        FinishAck::AwaitResume(t) => t,
        other => panic!("{other:?}"),
    };
    let resumed = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Resume {
            ticket,
            site,
            response: resumed.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert!(s.parked.permits.is_empty());
    assert!(b.permits.lock().unwrap().is_empty());
    assert_eq!(s.next_turns[&tid].protocol.owner, NextTurnOwner::Ordinary);
    assert!(
        s.parked.requests.keys().any(|id| id.dettid == tid),
        "the resumed wait is owned until its grant"
    );
    s.unblock_guest(tid, &resumed).unwrap();
    assert!(resumed.try_read().is_some());
    assert!(s.holds_serial_grant(tid));
    assert!(
        !s.parked.requests.keys().any(|id| id.dettid == tid),
        "the grant settled the resumed wait"
    );
    assert_eq!(s.next_turns[&tid].protocol.owner, NextTurnOwner::Ordinary);

    // So the empty boundary after the grant cleans nothing up.
    let before = scheduling_inputs(&s);
    let permit = s
        .authorize_signal_boundary(task(100, 100))
        .unwrap()
        .unwrap();
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::NoHandler,
    })
    .unwrap();
    b.release_delivery(permit).unwrap();
    assert_eq!(scheduling_inputs(&s), before);
    assert!(b.permits.lock().unwrap().is_empty());
}

/// A restartable polled read (`InternalIOPolling`, ERESTARTSYS) whose parked
/// wait an ignored alarm observed and resumed: the scheduler owns its request
/// again, in the Waiting phase, without a permit. Returns the resumed
/// response transport.
fn resumed_restartable_polled_read(
    s: &mut Scheduler,
    b: &Backend,
    tid: DetTid,
    mm: MmId,
    site: reverie::CallbackSignalSite,
) -> Ivar<SchedResponse> {
    assert!(s.backend.supports_parked_write_signal_interruption);
    let mut r = Resources::new(tid);
    r.insert(ResourceID::InternalIOPolling, Permission::W);
    r.poll_attempt = 1;
    r.set_signal_interrupt_errno(reverie::Errno::ERESTARTSYS);
    s.install_resource_origin(
        tid,
        ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm,
            control: ControlCapability::PolledRead { site },
        },
    )
    .unwrap();
    let response = {
        let turn = &s.next_turns[&tid];
        turn.req.put(Ok(r));
        turn.resp.clone()
    };
    s.run_queue.push_poller(tid, DEFAULT_PRIORITY, 1);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site,
            finish: ObservationFinish::ResumeSameWait,
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    let ticket = match ack.try_read().unwrap().unwrap() {
        FinishAck::AwaitResume(ticket) => ticket,
        other => panic!("{other:?}"),
    };
    let resumed = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Resume {
            ticket,
            site,
            response: resumed.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    b.recipients.lock().unwrap().clear();
    assert!(!s.backend_failed());
    assert!(s.parked.permits.is_empty());
    assert_eq!(s.next_turns[&tid].protocol.owner, NextTurnOwner::Ordinary);
    assert!(s.parked.requests.keys().any(|id| id.dettid == tid));
    resumed
}

/// Whether `tid` still owns any parked request, which makes
/// `complete_signal_exec` refuse its next exec.
fn owns_parked_request(s: &Scheduler, tid: DetTid) -> bool {
    s.parked.requests.keys().any(|id| id.dettid == tid)
}

/// A signal drain that replaces a resumed parked read's request
/// (`force_unblock_thread_at`) retires that request's parked ownership, so
/// the grant of the replacement leaves none behind. Before, the grant settled
/// only ownership of the current request transport, and the old entry
/// survived until a signal-boundary receipt, which reverie-kvm skips at an
/// empty boundary; the thread's next exec was then refused
/// (`complete_signal_exec`, ProtocolFailure::Identity).
#[test]
fn a_signal_replacing_a_resumed_parked_read_leaves_no_ownership_after_its_grant() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let resumed = resumed_restartable_polled_read(&mut s, &b, tid, mm, site);
    // A sibling's signal, which the read's own mask may block: the drain
    // replaces the request with the signal wake.
    s.notify_signal_pending(tid, SigWrapper::from(Signal::SIGUSR2));
    s.drain_pending_cross_task_signals();
    assert!(!s.backend_failed());
    let replaced = s.next_turns[&tid].req.try_read().unwrap().unwrap();
    assert!(
        replaced
            .resources
            .keys()
            .any(|id| matches!(id, ResourceID::WaitidSignals(_)))
    );
    assert!(
        !owns_parked_request(&s, tid),
        "the replacement retires the parked read's ownership"
    );
    assert_eq!(
        s.next_turns[&tid]
            .protocol
            .origin
            .map(|origin| origin.control),
        Some(ControlCapability::None),
        "the parked capability no longer describes the request"
    );
    assert!(s.run_queue.contains_tid(tid));
    s.unblock_guest(tid, &resumed).unwrap();
    assert!(resumed.try_read().is_some());
    assert!(!owns_parked_request(&s, tid));
    assert!(!s.backend_failed());
}

/// The merge site's own retirement: a second signal batch merges into the
/// queued restartable wake in place (`drain_pending_cross_task_signals`), and
/// that substitution retires the parked ownership of the request it replaces.
/// The first wake already retired the read's ownership, and no reachable path
/// owns a signal wake, so the test gives the wake live waiting ownership
/// (`own_current_request_for_test`) before the second batch: the merge must
/// find and retire it, and the grant must leave none.
#[test]
fn a_merged_signal_wake_of_a_resumed_parked_read_leaves_no_ownership_after_its_grant() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let resumed = resumed_restartable_polled_read(&mut s, &b, tid, mm, site);
    s.notify_signal_pending(tid, SigWrapper::from(Signal::SIGUSR2));
    s.drain_pending_cross_task_signals();
    assert!(!owns_parked_request(&s, tid));
    assert!(s.run_queue.contains_tid(tid));
    s.own_current_request_for_test(tid, site);
    assert!(owns_parked_request(&s, tid));
    let first = s.next_turns[&tid].req.clone();
    s.notify_signal_pending(tid, SigWrapper::from(Signal::SIGUSR1));
    s.drain_pending_cross_task_signals();
    assert!(!s.backend_failed());
    assert_ne!(s.next_turns[&tid].req, first, "the second batch merged");
    assert!(
        s.run_queue.contains_tid(tid),
        "merged in place, still queued"
    );
    let merged = s.next_turns[&tid].req.try_read().unwrap().unwrap();
    assert!(merged.resources.keys().any(|id| matches!(
        id,
        ResourceID::WaitidSignals(signals) if signals.len() == 2
    )));
    assert!(
        !owns_parked_request(&s, tid),
        "the merge retired the ownership it replaced"
    );
    assert_eq!(
        s.next_turns[&tid]
            .protocol
            .origin
            .map(|origin| origin.control),
        Some(ControlCapability::None)
    );
    s.unblock_guest(tid, &resumed).unwrap();
    assert!(resumed.try_read().is_some());
    assert!(!owns_parked_request(&s, tid));
    assert!(!s.backend_failed());
}

/// The state a test puts the resumed read's ownership in before the
/// scheduler substitutes its request.
#[derive(Clone, Copy, Debug)]
enum ReplacedOwnership {
    /// A plain waiting ownership, which the substitution retires.
    Qualifying,
    /// The turn's owner is not Ordinary.
    NotOrdinaryOwner,
    /// The thread holds a delivery permit.
    LivePermit,
    /// The turn's response is already answered.
    AnsweredResponse,
    /// The matching entry is not Waiting.
    NotWaiting,
    /// The matching entry names another response transport.
    ForeignResponse,
    /// The matching entry names another origin.
    ForeignOrigin,
}

/// Substitutes the resumed read's request after putting its ownership in
/// `state`, through `force_unblock_thread_at` or `substitute_request`, and
/// checks the outcome: a qualifying ownership is retired with its capability
/// and the request replaced; any other refuses with the named protocol
/// failure and leaves the original request, capability, ownership, run queue
/// and response exactly as they were, with no grant made.
fn check_replaced_ownership(state: ReplacedOwnership, through_force_unblock: bool) {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let resumed = resumed_restartable_polled_read(&mut s, &b, tid, mm, site);
    let wait = *s
        .parked
        .requests
        .keys()
        .find(|id| id.dettid == tid)
        .unwrap();
    match state {
        ReplacedOwnership::Qualifying => {}
        ReplacedOwnership::NotOrdinaryOwner => {
            s.next_turns.get_mut(&tid).unwrap().protocol.owner = NextTurnOwner::ReturningCaught {
                completed_wait: wait,
            };
        }
        ReplacedOwnership::LivePermit => {
            s.parked.permits.insert(
                tid,
                SignalDeliveryPermit {
                    task: task(100, 100),
                    sequence: 999,
                    site: None,
                },
            );
        }
        ReplacedOwnership::AnsweredResponse => resumed.put(SchedResponse::Go(None)),
        ReplacedOwnership::NotWaiting => {
            s.corrupt_owned_request_for_test(tid, OwnedRequestCorruption::NotWaiting)
        }
        ReplacedOwnership::ForeignResponse => {
            s.corrupt_owned_request_for_test(tid, OwnedRequestCorruption::ForeignResponse)
        }
        ReplacedOwnership::ForeignOrigin => {
            s.corrupt_owned_request_for_test(tid, OwnedRequestCorruption::ForeignOrigin)
        }
    }
    if through_force_unblock {
        // As the signal drain does before it force-unblocks a queued poll.
        assert!(s.run_queue.remove_tid(tid));
    }
    let request = s.next_turns[&tid].req.clone();
    let queue = format!("{:?}", s.run_queue);
    let answer = format!("{:?}", resumed.try_read());
    let owned: Vec<_> = s.parked.requests.keys().copied().collect();
    let mut wake = Resources::new(tid);
    wake.insert(
        ResourceID::WaitidSignals(vec![SigWrapper::from(Signal::SIGUSR2)]),
        Permission::W,
    );
    wake.set_signal_interrupt_errno(reverie::Errno::ERESTARTSYS);
    let replaced = if through_force_unblock {
        s.force_unblock_thread_at(tid, wake, false)
    } else {
        s.substitute_request(tid, wake)
    };
    let expected = match state {
        ReplacedOwnership::Qualifying => None,
        ReplacedOwnership::NotOrdinaryOwner
        | ReplacedOwnership::LivePermit
        | ReplacedOwnership::AnsweredResponse => Some(ProtocolFailure::Phase),
        ReplacedOwnership::NotWaiting
        | ReplacedOwnership::ForeignResponse
        | ReplacedOwnership::ForeignOrigin => Some(ProtocolFailure::Identity),
    };
    let label = format!("{state:?} through_force_unblock={through_force_unblock}");
    assert_eq!(s.parked.failure, expected, "{label}");
    assert_eq!(
        format!("{:?}", resumed.try_read()),
        answer,
        "{label}: no grant"
    );
    match expected {
        None => {
            assert!(replaced, "{label}");
            assert_ne!(s.next_turns[&tid].req, request, "{label}");
            assert!(!owns_parked_request(&s, tid), "{label}");
            assert_eq!(
                s.next_turns[&tid]
                    .protocol
                    .origin
                    .map(|origin| origin.control),
                Some(ControlCapability::None),
                "{label}"
            );
            assert!(s.run_queue.contains_tid(tid), "{label}");
        }
        Some(_) => {
            assert!(!replaced, "{label}");
            assert_eq!(s.next_turns[&tid].req, request, "{label}: request kept");
            assert_eq!(
                s.next_turns[&tid]
                    .protocol
                    .origin
                    .map(|origin| origin.control),
                Some(ControlCapability::PolledRead { site }),
                "{label}: capability kept"
            );
            assert_eq!(
                s.parked.requests.keys().copied().collect::<Vec<_>>(),
                owned,
                "{label}: ownership kept"
            );
            assert_eq!(format!("{:?}", s.run_queue), queue, "{label}: queue kept");
        }
    }
}

/// `retire_replaced_parked_request` retires only a plain waiting ownership:
/// owner Ordinary, no permit, the response unanswered (else Phase), and the
/// entry Waiting with the turn's response and origin (else Identity). Each
/// violation is refused before anything changes, through both substitution
/// paths, beside the qualifying case. No reachable path builds these states;
/// the checks guard against a later change that would.
#[test]
fn a_replaced_request_retires_only_plain_waiting_ownership() {
    for through_force_unblock in [true, false] {
        for state in [
            ReplacedOwnership::Qualifying,
            ReplacedOwnership::NotOrdinaryOwner,
            ReplacedOwnership::LivePermit,
            ReplacedOwnership::AnsweredResponse,
            ReplacedOwnership::NotWaiting,
            ReplacedOwnership::ForeignResponse,
            ReplacedOwnership::ForeignOrigin,
        ] {
            check_replaced_ownership(state, through_force_unblock);
        }
    }
}

/// An `InboundSignal` request for `signal`, as Detcore's delivery files it
/// once its thread holds a delivery permit.
fn inbound_request(tid: DetTid, signal: libc::c_int) -> Resources {
    let mut request = Resources::new(tid);
    request.insert(
        ResourceID::InboundSignal(crate::types::SigWrapper(signal)),
        Permission::RW,
    );
    request
}

/// Delivery permits belong to threads, not processes
/// (https://github.com/rrnewton/hermit/issues/4899). While one thread holds a
/// permit and waits for its `InboundSignal` turn, the scheduler grants a
/// sibling, and that sibling's boundary is authorized too; the backend
/// registers both permits, and each receipt releases only its own thread's
/// permit, in either order. A permit of another process changes nothing.
#[test]
fn two_threads_of_one_process_hold_delivery_permits_at_once() {
    for first_released in [100, 101] {
        let (mut s, b) = fixture();
        let (a, _, _) = add(&mut s, 100, 100);
        let (c, _, _) = add(&mut s, 100, 101);
        let (stranger, _, _) = add(&mut s, 200, 200);
        s.parked.running = Some(a);
        let pa = s
            .authorize_signal_boundary(task(100, 100))
            .unwrap()
            .expect("the thread holding the serial grant is authorized");
        // a files its InboundSignal request and waits; c is granted next.
        s.next_turns[&a]
            .req
            .put(Ok(inbound_request(a, libc::SIGUSR2)));
        s.parked.running = Some(c);
        let pc = s
            .authorize_signal_boundary(task(100, 101))
            .unwrap()
            .expect("a sibling's permit is no reason to decline");
        s.parked.running = Some(stranger);
        let ps = s
            .authorize_signal_boundary(task(200, 200))
            .unwrap()
            .expect("another process");
        assert!(pa.sequence < pc.sequence && pc.sequence < ps.sequence);
        assert_eq!(
            s.parked.permits.values().copied().collect::<Vec<_>>(),
            vec![pa, pc, ps]
        );
        assert_eq!(b.permits.lock().unwrap().as_slice(), &[pa, pc, ps]);
        let (first, second) = if first_released == 100 {
            (pa, pc)
        } else {
            (pc, pa)
        };
        s.consume_signal_boundary(SignalBoundaryReceipt {
            permit: first,
            outcome: SignalBoundaryOutcome::Caught,
        })
        .unwrap();
        // Ordered by thread: whichever of a and c remains comes before ps.
        assert_eq!(
            s.parked.permits.values().copied().collect::<Vec<_>>(),
            vec![second, ps]
        );
        s.consume_signal_boundary(SignalBoundaryReceipt {
            permit: second,
            outcome: SignalBoundaryOutcome::Caught,
        })
        .unwrap();
        assert_eq!(
            s.parked.permits.values().copied().collect::<Vec<_>>(),
            vec![ps]
        );
        assert!(!s.backend_failed());
        assert_eq!(s.parked.failure, None);
    }
}

/// A parked wait observed for a process alarm holds its observation permit
/// until its return-boundary receipt, across its `InboundSignal` turn, during
/// which siblings run. A sibling granted then is authorized at its own
/// boundary, its receipt releases only its own permit, and the observed
/// thread's return boundary still gets its own permit back.
#[test]
fn a_sibling_boundary_is_authorized_beside_an_observed_parked_wait() {
    let (mut s, b) = fixture();
    let (sleeper, mm, site) = add(&mut s, 100, 100);
    let (sibling, _, _) = add(&mut s, 100, 101);
    let response = sleep(&mut s, sleeper, mm, site, 100);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.committed_time = at(10);
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    assert_eq!(c.permit.site, Some(site));
    let ack = Ivar::new();
    s.post_control(
        sleeper,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site,
            finish: ObservationFinish::InterruptForCaught {
                selection: reverie::PreparedSignalToken {
                    site,
                    selection_nonce: 1,
                },
                dequeue: None,
            },
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert_eq!(ack.try_read(), Some(Ok(FinishAck::Interrupted)));
    assert_eq!(s.parked.permits.get(&sleeper), Some(&c.permit));

    s.parked.running = Some(sibling);
    let permit = s
        .authorize_signal_boundary(task(100, 101))
        .unwrap()
        .expect("an observed wait's permit is no reason to decline");
    assert_eq!(permit.site, None);
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[c.permit, permit]);
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Caught,
    })
    .unwrap();
    assert_eq!(
        s.parked.permits.values().copied().collect::<Vec<_>>(),
        vec![c.permit]
    );
    assert!(matches!(
        s.next_turns[&sleeper].protocol.owner,
        NextTurnOwner::ReturningCaught { .. }
    ));

    assert_eq!(
        s.authorize_signal_boundary(task(100, 100)).unwrap(),
        Some(c.permit)
    );
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: c.permit,
        outcome: SignalBoundaryOutcome::Caught,
    })
    .unwrap();
    assert!(s.parked.permits.is_empty());
    assert_eq!(
        s.next_turns[&sleeper].protocol.owner,
        NextTurnOwner::Ordinary
    );
    assert!(!s.backend_failed());
}

/// Alarm selection skips a process only while one of its parked waits is
/// observed. A sibling's ordinary boundary permit does not hold back the
/// observation of a parked recipient: Linux wakes a parked thread for a
/// process signal while a sibling is still on its way to deliver its own.
/// A second observation of the process waits for the first one's permit,
/// whatever else is released meanwhile.
#[test]
fn an_observation_begins_beside_a_boundary_permit_but_not_beside_another_observation() {
    let (mut s, b) = fixture();
    let (holder, _, _) = add(&mut s, 100, 100);
    let (first, mm, first_site) = add(&mut s, 100, 101);
    let (second, _, second_site) = add(&mut s, 100, 102);
    s.parked.running = Some(holder);
    let held = s
        .authorize_signal_boundary(task(100, 100))
        .unwrap()
        .unwrap();
    s.next_turns[&holder]
        .req
        .put(Ok(inbound_request(holder, libc::SIGUSR2)));
    let first_response = sleep(&mut s, first, mm, first_site, 100);
    let second_response = sleep(&mut s, second, mm, second_site, 100);
    b.recipients.lock().unwrap().extend([
        SignalRecipient {
            task: task(100, 101),
        },
        SignalRecipient {
            task: task(100, 102),
        },
    ]);
    s.committed_time = at(10);
    s.select_parked_alarm().unwrap();
    let c = selected(&first_response);
    assert_eq!(c.permit.task, task(100, 101));
    assert!(second_response.try_read().is_none());
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[held, c.permit]);

    s.select_parked_alarm().unwrap();
    assert!(
        second_response.try_read().is_none(),
        "one observation of a process at a time"
    );
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: held,
        outcome: SignalBoundaryOutcome::Caught,
    })
    .unwrap();
    s.select_parked_alarm().unwrap();
    assert!(second_response.try_read().is_none());
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[held, c.permit]);
    assert_eq!(
        s.parked.permits.values().copied().collect::<Vec<_>>(),
        vec![c.permit]
    );
}

/// A thread's own exit does not wait for a sibling's delivery permit: the
/// complete `Exit` arm reserves the exiting thread's fence and permit beside
/// the sibling's, the terminal receipt retires only the exiting thread, and
/// the sibling's own receipt then releases its permit. Linux lets a thread
/// exit while a sibling is about to take a signal.
#[test]
fn a_thread_exit_proceeds_while_a_sibling_holds_a_permit() {
    let (mut s, b) = fixture();
    let (holder, _, _) = add(&mut s, 100, 100);
    let (exiting, exiting_mm, _) = add(&mut s, 100, 101);
    s.parked.running = Some(holder);
    let held = s
        .authorize_signal_boundary(task(100, 100))
        .unwrap()
        .unwrap();
    s.next_turns[&holder]
        .req
        .put(Ok(inbound_request(holder, libc::SIGUSR1)));
    // The exiting thread is selected next, as the scheduler does once the
    // holder has filed its request.
    s.runqueue_push_back(exiting);
    assert_eq!(s.run_queue.tentative_pop_tid(exiting), Some(exiting));
    let exit = ResourceID::Exit {
        group: false,
        process: DetPid::from_raw(100),
        mm: exiting_mm,
    };
    assert!(
        s.block_for_one_resource(exiting, &exit, &Permission::RW, None, None, &Ivar::new())
            .is_ok(),
        "the exit is granted, not parked behind the sibling"
    );
    let fence = s.parked.exit_fences[&exiting];
    assert!(!fence.group);
    assert_eq!(s.parked.permits.get(&holder), Some(&held));
    assert_eq!(s.parked.permits.get(&exiting), Some(&fence.permit));
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[held, fence.permit]);
    assert!(s.control_barrier());

    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: fence.permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: false,
            wait_status: 0,
        },
    })
    .unwrap();
    assert!(!s.control_barrier());
    assert!(!s.next_turns.contains_key(&exiting));
    assert!(s.next_turns.contains_key(&holder));
    assert!(!s.thread_is_logically_killed(holder));
    assert_eq!(
        s.parked.permits.values().copied().collect::<Vec<_>>(),
        vec![held]
    );
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: held,
        outcome: SignalBoundaryOutcome::Caught,
    })
    .unwrap();
    assert!(s.parked.permits.is_empty());
    assert!(!s.backend_failed());
}

/// A group exit while siblings hold permits of their own, through the
/// `exit_group` fence or a fatal signal at the issuer's own boundary. The
/// terminal receipt retires every sibling and cancels its pending request.
/// Each sibling's permit stays until that sibling's own receipt settles it,
/// as reverie-kvm sends one for every permit before the sibling's worker
/// ends: `Cancelled`, or the committed group exit itself, which records no
/// second final transition. Only then does the process retire, with no
/// permit, fence or reservation left.
#[test]
fn a_group_exit_retires_permit_holding_siblings_whose_receipts_settle_them() {
    for fatal in [false, true] {
        for follower_terminated in [false, true] {
            let (mut s, b) = fixture();
            let (issuer, mm, _) = add(&mut s, 100, 100);
            let (holder, _, _) = add(&mut s, 100, 101);
            let (sleeper, _, site) = add(&mut s, 100, 102);

            // The holder's boundary: permit, then its InboundSignal request.
            s.parked.running = Some(holder);
            let held = s
                .authorize_signal_boundary(task(100, 101))
                .unwrap()
                .unwrap();
            s.next_turns[&holder]
                .req
                .put(Ok(inbound_request(holder, libc::SIGUSR2)));
            let holder_response = s.next_turns[&holder].resp.clone();
            s.run_queue.push_back(holder, DEFAULT_PRIORITY);

            // The sleeper's parked wait, observed and interrupted for a
            // caught alarm, waiting in its own InboundSignal request.
            let sleep_response = sleep(&mut s, sleeper, mm, site, 1000);
            b.recipients.lock().unwrap().push(SignalRecipient {
                task: task(100, 102),
            });
            s.committed_time = at(10);
            s.select_parked_alarm().unwrap();
            let observed = selected(&sleep_response);
            let ack = Ivar::new();
            s.post_control(
                sleeper,
                mm,
                ControlIntent::Finish {
                    wait: observed.continuation,
                    lease: observed.lease,
                    site,
                    finish: ObservationFinish::InterruptForCaught {
                        selection: reverie::PreparedSignalToken {
                            site,
                            selection_nonce: 1,
                        },
                        dequeue: None,
                    },
                    ack: ack.clone(),
                },
            )
            .unwrap();
            s.drain_control_intents();
            assert_eq!(ack.try_read(), Some(Ok(FinishAck::Interrupted)));
            s.next_turns[&sleeper]
                .req
                .put(Ok(inbound_request(sleeper, libc::SIGALRM)));
            let sleeper_response = s.next_turns[&sleeper].resp.clone();

            let (permit, status, wait_status) = if fatal {
                s.parked.running = Some(issuer);
                let permit = s
                    .authorize_signal_boundary(task(100, 100))
                    .unwrap()
                    .expect("the issuer's boundary beside two sibling permits");
                (permit, ExitStatus::from_raw(libc::SIGTERM), libc::SIGTERM)
            } else {
                s.reserve_exit_boundary(issuer, issuer, mm, true)
                    .expect("the exit beside two sibling permits");
                (
                    s.parked.exit_fences[&issuer].permit,
                    ExitStatus::Exited(7),
                    7 << 8,
                )
            };
            let death = s
                .consume_signal_boundary(SignalBoundaryReceipt {
                    permit,
                    outcome: SignalBoundaryOutcome::Terminated {
                        group: true,
                        wait_status,
                    },
                })
                .unwrap();
            // Only a death no committed exit fenced, a fatal signal's, is
            // reported, with every thread it ended, for its tables to go
            // (`GlobalState::release_tables_at_death`); a committed exit let
            // its table go at its grant.
            assert_eq!(
                death,
                fatal.then(|| super::signal_control::SignalDeath {
                    process: DetPid::from_raw(100),
                    threads: vec![issuer, holder, sleeper],
                })
            );
            for (sibling, response) in [(holder, &holder_response), (sleeper, &sleeper_response)] {
                assert!(s.thread_is_logically_killed(sibling));
                assert!(!s.next_turns.contains_key(&sibling));
                assert!(
                    matches!(response.try_read(), Some(SchedResponse::Signaled(None))),
                    "the sibling's pending request is cancelled"
                );
            }
            assert_eq!(
                s.parked.permits.values().copied().collect::<Vec<_>>(),
                vec![held, observed.permit],
                "a retired sibling's permit waits for its own receipt"
            );
            assert!(s.parked.exit_fences.is_empty());
            assert!(s.control_barrier());
            let key = ProcessGeneration::from_backend(task(100, 100).process);
            let terminal = s.parked.terminal_processes[&key];

            let follower = |permit: SignalDeliveryPermit| SignalBoundaryReceipt {
                permit,
                outcome: if follower_terminated {
                    SignalBoundaryOutcome::Terminated {
                        group: true,
                        wait_status,
                    }
                } else {
                    SignalBoundaryOutcome::Cancelled
                },
            };
            for permit in [held, observed.permit] {
                // A follower settles its permit only: the group's death was
                // reported once, by the receipt that committed it.
                assert_eq!(s.consume_signal_boundary(follower(permit)).unwrap(), None);
                // An exact duplicate is acknowledged, never applied again.
                assert_eq!(s.consume_signal_boundary(follower(permit)).unwrap(), None);
            }
            assert!(s.parked.permits.is_empty());
            assert_eq!(s.parked.terminal_processes.len(), 1);
            assert_eq!(s.parked.terminal_processes[&key], terminal);
            assert!(s.parked.child_exit_reservations.is_empty());
            assert!(s.parked.requests.is_empty());
            assert!(!s.backend_failed());

            s.consume_process_retirement(reverie::BackendProcessRetirement {
                process: task(100, 100).process,
                status,
            })
            .unwrap();
            assert!(!s.control_barrier());
            assert!(!s.backend_failed());
            assert_eq!(s.parked.failure, None);
        }
    }
}

/// The process retires only once every thread's permit is settled. After a
/// group exit retired a sibling that held a permit, a retirement report that
/// arrived before that sibling's receipt would leak the permit, so it is
/// refused, and the run fails rather than lose it.
#[test]
fn process_retirement_refuses_a_sibling_permit_still_outstanding() {
    let (mut s, _) = fixture();
    let (leader, mm, _) = add(&mut s, 100, 100);
    let (holder, _, _) = add(&mut s, 100, 101);
    s.parked.running = Some(holder);
    let held = s
        .authorize_signal_boundary(task(100, 101))
        .unwrap()
        .unwrap();
    s.reserve_exit_boundary(leader, leader, mm, true)
        .expect("the exit beside a sibling permit");
    let permit = s.parked.exit_fences[&leader].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 7 << 8,
        },
    })
    .unwrap();
    assert_eq!(s.parked.permits.get(&holder), Some(&held));
    assert!(
        s.consume_process_retirement(reverie::BackendProcessRetirement {
            process: task(100, 100).process,
            status: ExitStatus::Exited(7),
        })
        .is_err()
    );
    assert!(s.backend_failed());
    assert!(s.control_barrier());
}

/// What still declines, or reuses, with per-thread permits, while siblings
/// hold permits of their own: a thread that already holds a permit gets that
/// same permit back and never a second reservation, and another incarnation
/// of it is refused; a logically killed thread and a thread without the
/// serial grant are declined.
#[test]
fn per_thread_permits_reuse_a_held_permit_and_decline_killed_or_ungranted_threads() {
    let (mut s, b) = fixture();
    let (holder, _, _) = add(&mut s, 100, 100);
    let (granted, _, _) = add(&mut s, 100, 101);
    let (killed, killed_mm, _) = add(&mut s, 100, 102);
    let (_ungranted, _, _) = add(&mut s, 100, 103);
    s.parked.running = Some(holder);
    let held = s
        .authorize_signal_boundary(task(100, 100))
        .unwrap()
        .unwrap();
    s.parked.running = Some(granted);
    let permit = s
        .authorize_signal_boundary(task(100, 101))
        .unwrap()
        .expect("per-thread permit");
    assert_eq!(
        s.authorize_signal_boundary(task(100, 101)).unwrap(),
        Some(permit),
        "the same thread gets its own permit back"
    );
    let mut stale = task(100, 101);
    stale.task_generation += 1;
    assert!(s.authorize_signal_boundary(stale).is_err());
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[held, permit]);

    s.logically_kill_thread(&killed, &DetPid::from_raw(100), killed_mm);
    s.parked.running = Some(killed);
    assert_eq!(s.authorize_signal_boundary(task(100, 102)).unwrap(), None);
    s.parked.running = Some(granted);
    assert_eq!(s.authorize_signal_boundary(task(100, 103)).unwrap(), None);
    assert_eq!(
        s.parked.permits.values().copied().collect::<Vec<_>>(),
        vec![held, permit]
    );
    assert_eq!(b.permits.lock().unwrap().len(), 2);
    assert!(!s.backend_failed());
}

/// A sibling's boundary at each point of a permit holder's delivery: before
/// the holder files its `InboundSignal` request, after it filed it, after
/// that request is granted, and after the holder's receipt. The sibling's
/// authorization depends only on whether the sibling holds the serial grant,
/// never on the holder's permit. In a real run the sibling can be granted
/// only at the second and fourth points (the holder holds the grant at the
/// first and third), and the second is the one
/// https://github.com/rrnewton/hermit/issues/4899 refused.
#[test]
fn a_sibling_boundary_depends_only_on_its_own_grant_through_a_holders_delivery() {
    for point in ["authorized", "filed", "granted", "released"] {
        for sibling_granted in [false, true] {
            let (mut s, _) = fixture();
            let (a, _, _) = add(&mut s, 100, 100);
            let (c, _, _) = add(&mut s, 100, 101);
            s.runqueue_push_back(a);
            s.parked.running = Some(a);
            let held = s
                .authorize_signal_boundary(task(100, 100))
                .unwrap()
                .unwrap();
            if point != "authorized" {
                s.next_turns[&a]
                    .req
                    .put(Ok(inbound_request(a, libc::SIGUSR2)));
            }
            if point == "granted" || point == "released" {
                let response = s.next_turns[&a].resp.clone();
                s.unblock_guest(a, &response).unwrap();
                assert!(s.holds_serial_grant(a));
            }
            if point == "released" {
                s.consume_signal_boundary(SignalBoundaryReceipt {
                    permit: held,
                    outcome: SignalBoundaryOutcome::Caught,
                })
                .unwrap();
            }
            s.parked.running = Some(if sibling_granted { c } else { a });
            let answer = s.authorize_signal_boundary(task(100, 101)).unwrap();
            assert_eq!(answer.is_some(), sibling_granted, "{point}");
            assert_eq!(
                s.parked.permits.contains_key(&a),
                point != "released",
                "{point}"
            );
            assert!(!s.backend_failed(), "{point}");
        }
    }
}

#[test]
fn committed_failure_reenters_only_after_scheduler_terminal_and_unlock() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    let (mut s, b) = fixture();
    let (tid, _, _) = add(&mut s, 100, 100);
    s.replace_real_timer(tid, tid, at(0), at(10), at(0), Signal::SIGALRM)
        .unwrap();
    b.fail_publication.store(true, Ordering::Relaxed);
    let s = Arc::new(Mutex::new(s));
    let weak = Arc::downgrade(&s);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    *b.failure_probe.lock().unwrap() = Some(Box::new(move || {
        let scheduler = weak.upgrade().unwrap();
        let scheduler = scheduler
            .try_lock()
            .expect("publisher must not retain scheduler lock");
        assert!(
            scheduler.backend_failed(),
            "terminal transition precedes backend notification"
        );
        counted.fetch_add(1, Ordering::Relaxed);
    }));
    {
        let mut scheduler = s.lock().unwrap();
        scheduler.committed_time = at(10);
        let global = committed_clock(&scheduler);
        scheduler.step2b_process_timed(&global);
        assert!(scheduler.backend_failed());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(scheduler.host_signal_attempts, 0);
    }
    super::signal_control::flush_signal_failures(&s);
    super::signal_control::flush_signal_failures(&s);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(b.publications.lock().unwrap().len(), 1);
}

fn polled_read(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    site: reverie::CallbackSignalSite,
    resource: ResourceID,
    attempt: u32,
) -> Ivar<SchedResponse> {
    let mut r = Resources::new(tid);
    r.insert(resource, Permission::W);
    r.poll_attempt = attempt;
    s.install_resource_origin(
        tid,
        ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm,
            control: ControlCapability::PolledRead { site },
        },
    )
    .unwrap();
    let turn = &s.next_turns[&tid];
    turn.req.put(Ok(r));
    let response = turn.resp.clone();
    s.run_queue.push_poller(tid, DEFAULT_PRIORITY, attempt);
    response
}

#[test]
fn polled_read_no_handler_restores_exact_request_and_queue_order() {
    assert_polled_read_restoration(false);
}

#[test]
fn promoted_polled_read_restores_exact_ready_request_and_queue_order() {
    assert_polled_read_restoration(true);
}

fn assert_polled_read_restoration(promote: bool) {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let (peer, _, _) = add(&mut s, 200, 200);
    let response = polled_read(&mut s, tid, mm, site, ResourceID::InternalIOPolling, 3);
    if promote {
        let original = s.next_turns[&tid].req.try_read().unwrap().unwrap();
        let before_turn = s.turn;
        let before_time = s.committed_time;
        let (selected, selected_request, selected_response) = s.step3_peek().unwrap();
        assert_eq!(selected, tid);
        assert_eq!(selected_request, s.next_turns[&tid].req);
        assert_eq!(selected_response, response);
        assert!(
            s.step4_resource_block(tid, &original, &selected_response)
                .is_err()
        );
        assert_eq!(s.turn, before_turn + 1);
        assert_eq!(s.committed_time, before_time);
        assert!(!s.run_queue.tentative_pop_in_progress());
        assert_ne!(s.next_turns[&tid].req, selected_request);
        assert!(response.try_read().is_none());
    }
    let request = s.next_turns[&tid].req.clone();
    s.run_queue.push_poller(peer, DEFAULT_PRIORITY, 3);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    assert_ne!(s.next_turns[&tid].req, request);
    assert!(s.next_turns[&tid].req.try_read().is_none());
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[c.permit]);
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site,
            finish: ObservationFinish::ResumeSameWait,
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    let ticket = match ack.try_read().unwrap().unwrap() {
        FinishAck::AwaitResume(ticket) => ticket,
        other => panic!("{other:?}"),
    };
    assert!(b.permits.lock().unwrap().is_empty());
    let resumed = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Resume {
            ticket,
            site,
            response: resumed.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert!(!s.backend_failed());
    assert_eq!(s.next_turns[&tid].req, request);
    assert_ne!(s.next_turns[&tid].resp, response);
    assert_eq!(s.next_turns[&tid].resp, resumed);
    assert!(resumed.try_read().is_none());
    let restored = request.try_read().unwrap().unwrap();
    assert_eq!(restored.poll_attempt, if promote { 0 } else { 3 });
    assert_eq!(restored.resources.len(), 1);
    assert_eq!(
        restored.resources.get(&ResourceID::InternalIOPolling),
        Some(&Permission::W)
    );
    assert!(s.blocked.timed_waiters.thread_deadline(tid).is_none());
    assert_eq!(s.run_queue.tentative_pop_next(), Some(tid));
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    assert_eq!(s.run_queue.tentative_pop_next(), Some(peer));
    s.run_queue.undo_tentative_pop();
    // The old single-use response is never changed into a normal grant.
    assert_eq!(selected(&response), c);
    if promote {
        // A second actual observation after response registration still owns
        // the same ready request. Resume must not discard promotion provenance.
        s.run_queue.push_back(tid, DEFAULT_PRIORITY);
        s.select_parked_alarm().unwrap();
        assert_eq!(selected(&resumed).site, site);
    }
}

#[test]
fn polled_read_rejects_wrong_resource_first_attempt_and_foreign_identity() {
    for (resource, attempt) in [
        (ResourceID::SleepUntil(at(100)), 1),
        (ResourceID::InternalIOPolling, 0),
    ] {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        let response = polled_read(&mut s, tid, mm, site, resource, attempt);
        let request = s.next_turns[&tid].req.clone();
        let queue = format!("{:?}", s.run_queue);
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        assert_eq!(
            s.select_parked_alarm(),
            Err(SelectionFailure {
                pid: DetPid::from_raw(100),
                tid: Some(tid),
                failure: ProtocolFailure::Unsupported,
            })
        );
        assert_eq!(s.next_turns[&tid].req, request);
        assert_eq!(format!("{:?}", s.run_queue), queue);
        assert!(response.try_read().is_none());
        assert!(b.permits.lock().unwrap().is_empty());
    }
    for which in 0..3 {
        let (mut s, _) = fixture();
        let (tid, mm, mut site) = add(&mut s, 100, 100);
        let bad_mm = MmId::initial(DetTid::from_raw(999));
        if which == 0 {
            site.process.generation += 1;
        }
        if which == 1 {
            site.task_generation += 1;
        }
        assert!(
            s.install_resource_origin(
                tid,
                ResourceOrigin {
                    rpc: RpcOrigin::DirectRequestResources,
                    mm: if which == 2 { bad_mm } else { mm },
                    control: ControlCapability::PolledRead { site },
                }
            )
            .is_err()
        );
        assert!(s.next_turns[&tid].protocol.origin.is_none());
    }
}

#[test]
fn polled_read_resume_rejects_stale_or_duplicate_transport() {
    for mutation in 0..4 {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        let response = polled_read(&mut s, tid, mm, site, ResourceID::InternalIOPolling, 1);
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        s.select_parked_alarm().unwrap();
        let c = selected(&response);
        let ack = Ivar::new();
        s.post_control(
            tid,
            mm,
            ControlIntent::Finish {
                wait: c.continuation,
                lease: c.lease,
                site,
                finish: ObservationFinish::ResumeSameWait,
                ack: ack.clone(),
            },
        )
        .unwrap();
        s.drain_control_intents();
        let mut ticket = match ack.try_read().unwrap().unwrap() {
            FinishAck::AwaitResume(ticket) => ticket,
            other => panic!("{other:?}"),
        };
        let mut returned_site = site;
        if mutation == 0 {
            ticket.next_epoch += 1;
        }
        if mutation == 1 {
            ticket.nonce += 1;
        }
        if mutation == 2 {
            returned_site.callback_nonce += 1;
        }
        let resumed = Ivar::new();
        s.post_control(
            tid,
            mm,
            ControlIntent::Resume {
                ticket,
                site: returned_site,
                response: resumed.clone(),
            },
        )
        .unwrap();
        s.drain_control_intents();
        if mutation == 3 {
            assert!(!s.backend_failed());
            s.post_control(
                tid,
                mm,
                ControlIntent::Resume {
                    ticket,
                    site,
                    response: Ivar::new(),
                },
            )
            .unwrap();
            s.drain_control_intents();
        }
        assert!(s.backend_failed(), "mutation {mutation}");
        assert!(resumed.try_read().is_none());
        assert!(b.permits.lock().unwrap().is_empty());
    }
}

#[test]
fn promoted_polled_read_refuses_foreign_request_response_epoch_and_origin() {
    for mutation in 0..4 {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        polled_read(&mut s, tid, mm, site, ResourceID::InternalIOPolling, 2);
        let original = s.next_turns[&tid].req.try_read().unwrap().unwrap();
        s.upgrade_polled_to_runnable(tid, &original);
        let turn = s.next_turns.get_mut(&tid).unwrap();
        match mutation {
            0 => turn.req = Ivar::full(turn.req.try_read().unwrap()),
            1 => turn.resp = Ivar::new(),
            2 => turn.protocol.epoch += 1,
            3 => {
                turn.protocol.origin.as_mut().unwrap().rpc = RpcOrigin::ResumeParkedRequest {
                    continuation: ContinuationId {
                        dettid: tid,
                        nonce: 999,
                    },
                    cycle: 1,
                }
            }
            _ => unreachable!(),
        }
        let req = turn.req.clone();
        let resp = turn.resp.clone();
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        assert_eq!(
            s.select_parked_alarm(),
            Err(SelectionFailure {
                pid: DetPid::from_raw(100),
                tid: Some(tid),
                failure: ProtocolFailure::Unsupported,
            })
        );
        assert_eq!(s.next_turns[&tid].req, req);
        assert!(resp.try_read().is_none());
        assert!(b.permits.lock().unwrap().is_empty());
    }
}

#[test]
fn terminal_boundary_retires_exact_scope_before_pending_rpc_and_preserves_duplicates() {
    for group in [false, true] {
        let (mut s, _) = fixture();
        let (tid, mm, _) = add(&mut s, 100, 100);
        let (peer, _, _) = add(&mut s, 100, 101);
        // A different process sharing this mm is not a member of the group.
        let (other, _, _) = add_with_mm(&mut s, 200, 200, mm);
        let worker_req = s.next_turns[&peer].req.clone();
        let worker_resp = s.next_turns[&peer].resp.clone();
        let leader_req = s.next_turns[&tid].req.clone();
        let mut resources = Resources::new(peer);
        resources.insert(ResourceID::InternalIOPolling, Permission::W);
        resources.poll_attempt = 1;
        worker_req.put(Ok(resources));
        s.run_queue.push_back(tid, DEFAULT_PRIORITY);
        s.run_queue.push_back(peer, DEFAULT_PRIORITY);
        s.run_queue.push_back(other, DEFAULT_PRIORITY);
        s.parked.running = Some(tid);
        let permit = s
            .authorize_signal_boundary(task(100, 100))
            .unwrap()
            .unwrap();
        let receipt = SignalBoundaryReceipt {
            permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group,
                wait_status: 14,
            },
        };
        assert_eq!(s.run_queue.tentative_pop_next(), Some(tid));
        s.consume_signal_boundary(receipt).unwrap();
        assert!(!s.next_turns.contains_key(&tid));
        assert!(matches!(leader_req.try_read(), Some(Err(ThreadExited))));
        assert_eq!(s.next_turns.contains_key(&peer), !group);
        assert_eq!(
            matches!(worker_resp.try_read(), Some(SchedResponse::Signaled(None))),
            group
        );
        assert!(s.next_turns.contains_key(&other));
        assert_eq!(
            s.real_timers
                .task_identity(DetTid::from_raw(100), peer)
                .is_some(),
            !group
        );
        // Retired timer identities cannot invalidate an exact receipt replay.
        s.consume_signal_boundary(receipt).unwrap();
        for altered in [
            SignalBoundaryOutcome::Terminated {
                group: !group,
                wait_status: 14,
            },
            SignalBoundaryOutcome::Terminated {
                group,
                wait_status: 15,
            },
        ] {
            assert!(
                s.consume_signal_boundary(SignalBoundaryReceipt {
                    outcome: altered,
                    ..receipt
                })
                .is_err()
            );
        }
        assert_eq!(s.parked.completed[&tid], receipt);
        assert!(s.run_queue.tentative_pop_in_progress());
        s.run_queue.undo_tentative_pop();
    }
}

#[test]
fn committed_exit_boundary_fences_later_turns_until_exact_terminal_receipt() {
    for group in [false, true] {
        let (mut s, backend) = fixture();
        let (leader, mm, _) = add(&mut s, 100, 100);
        let (peer, _, _) = add(&mut s, 100, 101);
        s.run_queue.push_back(leader, DEFAULT_PRIORITY);
        s.run_queue.push_back(peer, DEFAULT_PRIORITY);

        s.reserve_exit_boundary(leader, DetPid::from_raw(100), mm, group)
            .unwrap();
        let fence = s.parked.exit_fences[&leader];
        assert_eq!(s.parked.permits[&leader], fence.permit);
        assert_eq!(backend.permits.lock().unwrap().as_slice(), &[fence.permit]);
        assert!(s.control_barrier());

        let wrong = SignalBoundaryReceipt {
            permit: fence.permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group: !group,
                wait_status: 7 << 8,
            },
        };
        assert!(s.consume_signal_boundary(wrong).is_err());
        assert_eq!(s.parked.exit_fences[&leader], fence);
        assert!(s.control_barrier());

        let receipt = SignalBoundaryReceipt {
            outcome: SignalBoundaryOutcome::Terminated {
                group,
                wait_status: 7 << 8,
            },
            ..wrong
        };
        assert_eq!(
            s.consume_signal_boundary(receipt).unwrap(),
            None,
            "a committed exit let its table go at its grant"
        );
        assert!(!s.parked.exit_fences.contains_key(&leader));
        assert!(!s.parked.permits.contains_key(&leader));
        assert_eq!(s.control_barrier(), group);
        if group {
            s.consume_process_retirement(reverie::BackendProcessRetirement {
                process: task(100, 100).process,
                status: ExitStatus::Exited(7),
            })
            .unwrap();
        }
        assert!(!s.control_barrier());
        assert!(!s.next_turns.contains_key(&leader));
        assert_eq!(s.next_turns.contains_key(&peer), !group);
        assert!(s.control_waiter().try_read().is_some());
        s.consume_signal_boundary(receipt).unwrap();
    }
}

#[test]
fn child_exit_publication_is_two_poll_and_retains_shadow_for_wnowait() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);

    assert_eq!(
        s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true),
        Ok(super::signal_control::ExitReserveMode::Controlled)
    );
    let fence = s.parked.exit_fences[&child];
    let child_key = ProcessGeneration::from_backend(task(200, 200).process);
    assert!(s.parked.child_exit_reservations.contains_key(&child_key));
    assert!(s.control_barrier(), "stages 1 and 2 form one barrier");

    let receipt = SignalBoundaryReceipt {
        permit: fence.permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 7 << 8,
        },
    };
    s.consume_signal_boundary(receipt).unwrap();
    assert!(!s.parked.exit_fences.contains_key(&child));
    assert!(matches!(
        s.parked.child_exit_reservations[&child_key].phase,
        ChildExitReservationPhase::AwaitingPublication {
            status: ExitStatus::Exited(7)
        }
    ));
    assert!(s.control_barrier(), "stage 2 survives stage-1 receipt");

    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(7),
        true,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let admission_locked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe_scheduler = Arc::downgrade(&scheduler);
    let probe_admission_locked = admission_locked.clone();
    *backend.child_publication_probe.lock().unwrap() = Some(Box::new(move || {
        let scheduler = probe_scheduler.upgrade().unwrap();
        assert!(
            scheduler.try_lock().is_err(),
            "publication must remain inside scheduler admission"
        );
        probe_admission_locked.store(true, std::sync::atomic::Ordering::Relaxed);
    }));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(admission_locked.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
    {
        let s = scheduler.lock().unwrap();
        assert!(matches!(
            s.parked.child_exit_reservations[&child_key].phase,
            ChildExitReservationPhase::Published { .. }
        ));
        assert!(s.control_barrier());
    }
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Ok(()))
    ));
    {
        let mut s = scheduler.lock().unwrap();
        assert!(!s.control_barrier());
        assert_eq!(
            s.parked.completed_child_exits[&child_key].completion,
            event.child_exit_completion().unwrap()
        );
        let retirement = reverie::BackendProcessRetirement {
            process: event.child,
            status: ExitStatus::Exited(7),
        };
        s.consume_process_retirement(retirement).unwrap();
        s.consume_process_retirement(retirement).unwrap();
        assert!(s.parked.process_retirements.is_empty());
        assert!(!s.control_barrier());
        assert_eq!(backend.child_publications.lock().unwrap().len(), 1);

        let spec = crate::types::ChildWaitSpec {
            selector: crate::types::ChildWaitSelector::Exact(DetPid::from_raw(200)),
            owner: None,
            exit_class: crate::types::ChildWaitExitClass::Sigchld,
        };
        assert_eq!(
            s.ready_child_wait(DetPid::from_raw(100), spec),
            Some(DetPid::from_raw(200))
        );
        assert_eq!(
            s.ready_child_wait(DetPid::from_raw(100), spec),
            Some(DetPid::from_raw(200)),
            "WNOWAIT requires the scheduler shadow to remain repeatedly observable"
        );
        assert!(s.consume_child_wait(DetPid::from_raw(100), DetPid::from_raw(200)));
        assert_eq!(s.ready_child_wait(DetPid::from_raw(100), spec), None);
    }

    // The exact duplicate is terminal and does not republish.
    let mut duplicate = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut duplicate), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut duplicate),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);

    // Neither a reused generation nor contradictory status can inherit the
    // terminal receipt.
    for stale in [
        BackendChildWaitEvent {
            child: SignalProcessId {
                generation: event.child.generation + 1,
                ..event.child
            },
            ..event
        },
        child_exit_event(event.parent, event.child, ExitStatus::Exited(8), true),
    ] {
        let mut stale = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
            scheduler.clone(),
            stale,
        ));
        assert!(matches!(poll_child_exit(&mut stale), Poll::Ready(Err(_))));
    }
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
    assert!(scheduler.lock().unwrap().backend_failed());
}

#[test]
fn nonwaitable_child_auto_reaps_shadow_regardless_of_signal_effect() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    *backend.child_effect.lock().unwrap() = Some(ChildExitPublicationEffect::Queued);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 9 << 8,
        },
    })
    .unwrap();

    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(9),
        false,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Ok(()))
    ));
    let s = scheduler.lock().unwrap();
    assert!(!s.control_barrier());
    assert!(
        !s.logically_exited_processes
            .contains(&DetPid::from_raw(200))
    );
    assert_eq!(s.thread_tree.parent_process(&DetPid::from_raw(200)), None);
    assert_eq!(
        backend.child_publications.lock().unwrap()[0],
        event.child_exit_completion().unwrap()
    );
}

#[test]
fn post_commit_shadow_cleanup_failure_is_terminal_before_barrier_release() {
    let (mut s, _) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 11 << 8,
        },
    })
    .unwrap();
    s.thread_tree.process_parent.remove(&DetPid::from_raw(200));

    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(11),
        false,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Err(_))
    ));
    let s = scheduler.lock().unwrap();
    assert!(s.backend_failed());
    assert!(!s.control_barrier());
}

#[test]
fn final_process_classification_uses_live_direct_parent_not_transitive_root() {
    let (mut s, _) = fixture();
    let (root, root_mm, _) = add(&mut s, 100, 100);
    let _ = add_process_child(&mut s, 100, 200);
    let (grandchild, grandchild_mm, _) = add_process_child(&mut s, 200, 300);

    s.reserve_exit_boundary(root, DetPid::from_raw(100), root_mm, true)
        .unwrap();
    let root_permit = s.parked.exit_fences[&root].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: root_permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 0,
        },
    })
    .unwrap();
    assert!(matches!(
        s.parked.terminal_processes[&ProcessGeneration::from_backend(task(100, 100).process)].class,
        FinalProcessClass::Root
    ));
    assert!(s.control_barrier());
    s.consume_process_retirement(reverie::BackendProcessRetirement {
        process: task(100, 100).process,
        status: ExitStatus::SUCCESS,
    })
    .unwrap();

    s.reserve_exit_boundary(grandchild, DetPid::from_raw(300), grandchild_mm, true)
        .unwrap();
    let reservation =
        s.parked.child_exit_reservations[&ProcessGeneration::from_backend(task(300, 300).process)];
    assert_eq!(reservation.parent, task(200, 200).process);
    assert!(matches!(
        reservation.phase,
        ChildExitReservationPhase::AwaitingTerminalStatus
    ));
}

#[test]
fn direct_parent_terminal_child_is_classified_and_auto_reaped_without_callback() {
    let (mut s, backend) = fixture();
    let (root, root_mm, _) = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);

    s.reserve_exit_boundary(root, DetPid::from_raw(100), root_mm, true)
        .unwrap();
    let root_key = ProcessGeneration::from_backend(task(100, 100).process);
    assert!(
        !s.parked.child_exit_reservations.contains_key(&root_key),
        "a root process has no parent callback stage"
    );
    let root_permit = s.parked.exit_fences[&root].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: root_permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 4 << 8,
        },
    })
    .unwrap();
    assert!(matches!(
        s.parked.terminal_processes[&root_key].class,
        FinalProcessClass::Root
    ));
    assert!(s.control_barrier());
    s.consume_process_retirement(reverie::BackendProcessRetirement {
        process: task(100, 100).process,
        status: ExitStatus::Exited(4),
    })
    .unwrap();
    assert!(!s.control_barrier());
    assert!(backend.child_publications.lock().unwrap().is_empty());

    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let child_key = ProcessGeneration::from_backend(task(200, 200).process);
    assert!(
        !s.parked.child_exit_reservations.contains_key(&child_key),
        "a terminal direct parent causes backend teardown auto-reap"
    );
    let child_permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: child_permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 5 << 8,
        },
    })
    .unwrap();
    assert!(matches!(
        s.parked.terminal_processes[&child_key].class,
        FinalProcessClass::DirectParentTerminal { parent }
            if parent == task(100, 100).process
    ));
    assert!(
        !s.logically_exited_processes
            .contains(&DetPid::from_raw(200))
    );
    assert_eq!(s.thread_tree.parent_process(&DetPid::from_raw(200)), None);
    assert!(s.control_barrier());
    s.consume_process_retirement(reverie::BackendProcessRetirement {
        process: task(200, 200).process,
        status: ExitStatus::Exited(5),
    })
    .unwrap();
    assert!(!s.control_barrier());
    assert!(backend.child_publications.lock().unwrap().is_empty());
}

#[test]
fn process_retirement_without_controlled_terminal_preserves_root_and_orphan_state() {
    for terminal_parent in [false, true] {
        for tasks_retired in [false, true] {
            let (mut s, backend) = fixture();
            let (root, root_mm, _) = add(&mut s, 100, 100);
            let (leader, mm) = if terminal_parent {
                let (child, mm, _) = add_process_child(&mut s, 100, 200);
                s.reserve_exit_boundary(root, root, root_mm, true).unwrap();
                s.consume_signal_boundary(SignalBoundaryReceipt {
                    permit: s.parked.exit_fences[&root].permit,
                    outcome: SignalBoundaryOutcome::Terminated {
                        group: true,
                        wait_status: 0,
                    },
                })
                .unwrap();
                s.consume_process_retirement(reverie::BackendProcessRetirement {
                    process: task(100, 100).process,
                    status: ExitStatus::SUCCESS,
                })
                .unwrap();
                (child, mm)
            } else {
                (root, root_mm)
            };
            let (worker, _, _) = add(&mut s, leader.as_raw(), leader.as_raw() + 1);
            if tasks_retired {
                // A previously exited leader can join a final faulting worker
                // after that worker's consuming hook retired the last task.
                s.reserve_exit_boundary(leader, leader, mm, false).unwrap();
                s.consume_signal_boundary(SignalBoundaryReceipt {
                    permit: s.parked.exit_fences[&leader].permit,
                    outcome: SignalBoundaryOutcome::Terminated {
                        group: false,
                        wait_status: 7 << 8,
                    },
                })
                .unwrap();
                assert!(s.thread_is_logically_killed(leader));
                assert!(!s.thread_is_logically_killed(worker));
                s.logically_kill_thread(&worker, &leader, mm);
                assert!(s.real_timers.process_identity(leader).is_err());
            }
            let (other, other_mm, _) = add(&mut s, 900, 900);
            s.reserve_exit_boundary(other, other, other_mm, true)
                .unwrap();
            let foreign_fence = s.parked.exit_fences[&other];
            s.drain_control_intents();
            let waiter = s.control_waiter();
            assert!(waiter.try_read().is_none());
            let tasks_before: Vec<_> = s.next_turns.keys().copied().collect();
            let killed_before = s.logically_killed_threads.clone();
            let terminal_before = s.parked.terminal_processes.clone();
            let pending_before = s.parked.process_retirements.clone();
            let mut completed = s.parked.completed_process_retirements.clone();
            let event = reverie::BackendProcessRetirement {
                process: task(leader.as_raw(), leader.as_raw()).process,
                status: ExitStatus::from_raw(libc::SIGSEGV),
            };
            assert!(
                !s.parked
                    .terminal_processes
                    .contains_key(&ProcessGeneration::from_backend(event.process))
            );
            s.consume_process_retirement(event).unwrap();
            completed.insert(ProcessGeneration::from_backend(event.process), event);
            assert_eq!(s.parked.completed_process_retirements, completed);
            assert_eq!(s.parked.terminal_processes, terminal_before);
            assert_eq!(s.parked.process_retirements, pending_before);
            assert_eq!(
                s.next_turns.keys().copied().collect::<Vec<_>>(),
                tasks_before
            );
            assert_eq!(s.logically_killed_threads, killed_before);
            assert_eq!(s.parked.exit_fences[&other], foreign_fence);
            assert!(s.control_barrier());
            assert!(waiter.try_read().is_none());
            assert!(backend.child_publications.lock().unwrap().is_empty());
            assert!(!s.backend_failed());
            assert_eq!(s.turn, 0);
        }
    }
}

#[test]
fn process_retirement_without_controlled_terminal_preserves_duplicate_status_guard() {
    let (mut s, _) = fixture();
    let (leader, mm, _) = add(&mut s, 100, 100);
    let event = reverie::BackendProcessRetirement {
        process: task(100, 100).process,
        status: ExitStatus::from_raw(libc::SIGSEGV),
    };
    s.consume_process_retirement(event).unwrap();
    s.logically_kill_thread(&leader, &leader, mm);
    s.consume_process_retirement(event).unwrap();
    assert!(!s.backend_failed());
    let completed = s.parked.completed_process_retirements.clone();
    assert!(
        s.consume_process_retirement(reverie::BackendProcessRetirement {
            status: ExitStatus::from_raw(libc::SIGILL),
            ..event
        })
        .is_err()
    );
    assert!(s.backend_failed());
    assert_eq!(s.parked.completed_process_retirements, completed);
    assert!(s.parked.terminal_processes.is_empty());
    assert_eq!(s.turn, 0);
}

#[test]
fn process_retirement_without_controlled_terminal_rejects_unknown_and_reused_generation() {
    for defect in ["unknown", "generation", "reused generation"] {
        let (mut s, _) = fixture();
        let (leader, mm, _) = add(&mut s, 100, 100);
        let mut identity = task(100, 100);
        let mut event = reverie::BackendProcessRetirement {
            process: identity.process,
            status: ExitStatus::from_raw(libc::SIGSEGV),
        };
        match defect {
            "unknown" => event.process.tgid = reverie::Pid::from_raw(999),
            "generation" => event.process.generation += 1,
            "reused generation" => {
                s.logically_kill_thread(&leader, &leader, mm);
                identity.process.generation += 1;
                identity.task_generation += 1;
                s.real_timers.bind(leader, leader, mm, identity).unwrap();
            }
            _ => unreachable!(),
        }
        let (other, other_mm, _) = add(&mut s, 900, 900);
        s.reserve_exit_boundary(other, other, other_mm, true)
            .unwrap();
        let fence = s.parked.exit_fences[&other];
        assert!(s.consume_process_retirement(event).is_err(), "{defect}");
        assert!(s.backend_failed(), "{defect}");
        assert_eq!(s.parked.exit_fences[&other], fence, "{defect}");
        assert!(s.control_barrier(), "{defect}");
        assert!(s.parked.completed_process_retirements.is_empty());
        assert!(s.parked.terminal_processes.is_empty());
        assert_eq!(s.turn, 0);
    }
}

#[test]
fn process_retirement_without_controlled_terminal_rejects_pending_process_obligations() {
    for obligation in ["permit", "exit fence", "child publication", "retirement"] {
        let (mut s, _) = fixture();
        let (leader, mm, _) = add(&mut s, 100, 100);
        let event = reverie::BackendProcessRetirement {
            process: task(100, 100).process,
            status: ExitStatus::from_raw(libc::SIGSEGV),
        };
        let key = ProcessGeneration::from_backend(event.process);
        match obligation {
            "permit" => {
                // Ordinary signal delivery owns a permit without an exit fence.
                s.parked.running = Some(leader);
                s.authorize_signal_boundary(task(100, 100))
                    .unwrap()
                    .unwrap();
                assert!(s.parked.exit_fences.is_empty());
            }
            "exit fence" => {
                s.reserve_exit_boundary(leader, leader, mm, true).unwrap();
            }
            "child publication" => {
                s.parked.child_exit_reservations.insert(
                    key,
                    ChildExitReservation {
                        child: event.process,
                        parent: task(50, 50).process,
                        phase: ChildExitReservationPhase::AwaitingTerminalStatus,
                    },
                );
            }
            "retirement" => {
                s.parked.process_retirements.insert(key, event);
            }
            _ => unreachable!(),
        }
        let permits = s.parked.permits.clone();
        let fences = s.parked.exit_fences.clone();
        let children = s.parked.child_exit_reservations.clone();
        let retirements = s.parked.process_retirements.clone();
        let barrier = s.control_barrier();
        assert!(s.consume_process_retirement(event).is_err(), "{obligation}");
        assert!(s.backend_failed(), "{obligation}");
        assert_eq!(s.parked.permits, permits);
        assert_eq!(s.parked.exit_fences, fences);
        assert_eq!(s.parked.child_exit_reservations, children);
        assert_eq!(s.parked.process_retirements, retirements);
        assert_eq!(s.control_barrier(), barrier);
        assert!(s.parked.completed_process_retirements.is_empty());
        assert!(s.parked.terminal_processes.is_empty());
        assert_eq!(s.turn, 0);
    }
}

#[test]
fn process_retirement_cannot_replace_live_parent_child_publication() {
    let (mut s, _) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, child, mm, true).unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 7 << 8,
        },
    })
    .unwrap();
    let key = ProcessGeneration::from_backend(task(200, 200).process);
    let reservation = s.parked.child_exit_reservations[&key];
    assert!(
        s.consume_process_retirement(reverie::BackendProcessRetirement {
            process: task(200, 200).process,
            status: ExitStatus::Exited(7),
        })
        .is_err()
    );
    assert!(s.backend_failed());
    assert!(s.control_barrier());
    assert_eq!(s.parked.child_exit_reservations[&key], reservation);
    assert!(s.parked.completed_process_retirements.is_empty());
}

#[test]
fn process_retirement_fences_root_and_orphan_group_exit_after_cancelling_peer_rpc() {
    use futures::FutureExt;

    for terminal_parent in [false, true] {
        for fatal in [false, true] {
            for worker_issuer in [false, true] {
                let (mut s, backend) = fixture();
                let (root, root_mm, _) = add(&mut s, 100, 100);
                let (leader, mm) = if terminal_parent {
                    let (child, mm, _) = add_process_child(&mut s, 100, 200);
                    s.reserve_exit_boundary(root, root, root_mm, true).unwrap();
                    let permit = s.parked.exit_fences[&root].permit;
                    s.consume_signal_boundary(SignalBoundaryReceipt {
                        permit,
                        outcome: SignalBoundaryOutcome::Terminated {
                            group: true,
                            wait_status: 0,
                        },
                    })
                    .unwrap();
                    s.consume_process_retirement(reverie::BackendProcessRetirement {
                        process: task(100, 100).process,
                        status: ExitStatus::SUCCESS,
                    })
                    .unwrap();
                    (child, mm)
                } else {
                    (root, root_mm)
                };
                let pid = leader.as_raw();
                let (worker, _, _) = add(&mut s, pid, pid + 1);
                let (issuer, peer) = if worker_issuer {
                    (worker, leader)
                } else {
                    (leader, worker)
                };
                let peer_response = s.next_turns[&peer].resp.clone();
                let mut polling = Resources::new(peer);
                polling.insert(ResourceID::InternalIOPolling, Permission::W);
                polling.poll_attempt = 1;
                s.next_turns[&peer].req.put(Ok(polling));
                let (other, _, _) = add(&mut s, 900, 900);
                s.next_turns[&other].req.put(Ok(Resources::new(other)));
                for tid in [issuer, peer, other] {
                    s.run_queue.push_back(tid, DEFAULT_PRIORITY);
                }

                let permit = if fatal {
                    s.parked.running = Some(issuer);
                    s.authorize_signal_boundary(task(pid, issuer.as_raw()))
                        .unwrap()
                        .unwrap()
                } else {
                    s.reserve_exit_boundary(issuer, leader, mm, true).unwrap();
                    s.parked.exit_fences[&issuer].permit
                };
                let status = if fatal {
                    ExitStatus::from_raw(libc::SIGTERM)
                } else {
                    ExitStatus::Exited(7)
                };
                s.consume_signal_boundary(SignalBoundaryReceipt {
                    permit,
                    outcome: SignalBoundaryOutcome::Terminated {
                        group: true,
                        wait_status: if fatal { libc::SIGTERM } else { 7 << 8 },
                    },
                })
                .unwrap();
                assert!(matches!(
                    peer_response.try_read(),
                    Some(SchedResponse::Signaled(None))
                ));
                assert!(s.thread_is_logically_killed(issuer));
                assert!(s.thread_is_logically_killed(peer));
                assert!(s.control_barrier());
                assert!(s.parked.exit_fences.is_empty());
                assert!(s.parked.child_exit_reservations.is_empty());
                assert!(backend.child_publications.lock().unwrap().is_empty());

                let scheduler = Arc::new(Mutex::new(s));
                let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
                let before = global.lock().unwrap().as_nanos();
                let last = Err(SkipTurn);
                let mut next =
                    Box::pin(do_a_turn_blocking(scheduler.clone(), global.clone(), &last));
                assert!(next.as_mut().now_or_never().is_none());
                assert_eq!(scheduler.lock().unwrap().turn, 0);
                assert_eq!(global.lock().unwrap().as_nanos(), before);
                let retirement = reverie::BackendProcessRetirement {
                    process: task(pid, pid).process,
                    status,
                };
                {
                    let mut s = scheduler.lock().unwrap();
                    s.consume_process_retirement(retirement).unwrap();
                    s.consume_process_retirement(retirement).unwrap();
                    assert!(!s.control_barrier());
                    assert!(!s.backend_failed());
                }
                let resumed = next
                    .as_mut()
                    .now_or_never()
                    .expect("retirement must release the ready unrelated task")
                    .unwrap();
                assert_eq!(resumed.tid, other);
                assert_eq!(scheduler.lock().unwrap().turn, 1);
            }
        }
    }
}

#[test]
fn process_retirement_rejects_early_stale_and_conflicting_receipts_before_release() {
    use futures::FutureExt;

    for defect in ["early", "generation", "status", "unknown", "duplicate"] {
        let (mut s, _) = fixture();
        let (leader, mm, _) = add(&mut s, 100, 100);
        s.reserve_exit_boundary(leader, leader, mm, true).unwrap();
        let permit = s.parked.exit_fences[&leader].permit;
        let exact = reverie::BackendProcessRetirement {
            process: task(100, 100).process,
            status: ExitStatus::Exited(7),
        };
        if defect != "early" {
            s.consume_signal_boundary(SignalBoundaryReceipt {
                permit,
                outcome: SignalBoundaryOutcome::Terminated {
                    group: true,
                    wait_status: 7 << 8,
                },
            })
            .unwrap();
        }
        if defect == "duplicate" {
            s.consume_process_retirement(exact).unwrap();
        }
        let mut wrong = exact;
        match defect {
            "generation" => wrong.process.generation += 1,
            "status" | "duplicate" => wrong.status = ExitStatus::Exited(8),
            "unknown" => wrong.process.tgid = reverie::Pid::from_raw(999),
            "early" => {}
            _ => unreachable!(),
        }
        let pending_before = s.parked.process_retirements.clone();
        let failure = s.backend_failure_waiter();
        assert!(s.consume_process_retirement(wrong).is_err(), "{defect}");
        assert!(s.backend_failed(), "{defect}");
        assert_eq!(s.parked.process_retirements, pending_before, "{defect}");
        assert_eq!(s.control_barrier(), defect != "duplicate", "{defect}");
        assert!(failure.clone().now_or_never().is_none());
        let wakes = s.take_signal_failure_wakes();
        assert_eq!(wakes.len(), 1);
        let scheduler = Arc::new(Mutex::new(s));
        for wake in wakes {
            let _ = wake.send(());
        }
        assert!(failure.now_or_never().unwrap().is_ok());
        // Terminal failure abandons the wait, without turning an incomplete
        // retirement into a successful scheduling continuation.
        let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        assert!(
            do_a_turn_blocking(scheduler.clone(), global, &Err(SkipTurn))
                .now_or_never()
                .unwrap()
                .is_err()
        );
        assert_eq!(scheduler.lock().unwrap().turn, 0);
    }
}

#[test]
fn process_retirement_keeps_empty_run_alive_until_cleanup_or_failure() {
    use futures::FutureExt;

    for fail in [false, true] {
        for group in [false, true] {
            let (mut s, _) = fixture();
            let (leader, mm, _) = add(&mut s, 100, 100);
            s.started_up.put(());
            s.reserve_exit_boundary(leader, leader, mm, group).unwrap();
            let permit = s.parked.exit_fences[&leader].permit;
            s.consume_signal_boundary(SignalBoundaryReceipt {
                permit,
                outcome: SignalBoundaryOutcome::Terminated {
                    group,
                    wait_status: 0,
                },
            })
            .unwrap();
            assert!(s.run_queue.is_empty());
            assert!(s.blocked.is_empty());
            let scheduler = Arc::new(Mutex::new(s));
            let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
            let events = Arc::new(Mutex::new(Vec::new()));
            let observed = events.clone();
            let mut daemon = Box::pin(sched_loop_external(
                scheduler.clone(),
                global,
                Arc::new(move |event| observed.lock().unwrap().push(event)),
            ));
            assert!(daemon.as_mut().now_or_never().is_none());
            assert!(
                !events
                    .lock()
                    .unwrap()
                    .contains(&"run queue empty; scheduler completed")
            );
            if fail {
                let wake = scheduler
                    .lock()
                    .unwrap()
                    .report_backend_failure(reverie::BackendFailure {
                        pid: reverie::Pid::from_raw(100),
                        tid: reverie::Pid::from_raw(100),
                        phase: "injected process cleanup failure",
                    })
                    .unwrap();
                let _ = wake.send(());
            } else {
                scheduler
                    .lock()
                    .unwrap()
                    .consume_process_retirement(reverie::BackendProcessRetirement {
                        process: task(100, 100).process,
                        status: ExitStatus::SUCCESS,
                    })
                    .unwrap();
            }
            assert!(daemon.as_mut().now_or_never().is_some());
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .contains(&"run queue empty; scheduler completed"),
                !fail
            );
            assert_eq!(scheduler.lock().unwrap().control_barrier(), fail);
        }
    }
}

#[test]
fn fatal_signal_final_transition_installs_stage_two_before_stage_one_retires() {
    let (mut s, _) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, _, _) = add_process_child(&mut s, 100, 200);
    s.parked.running = Some(child);
    let permit = s
        .authorize_signal_boundary(task(200, 200))
        .unwrap()
        .unwrap();
    let death = s
        .consume_signal_boundary(SignalBoundaryReceipt {
            permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group: true,
                wait_status: libc::SIGKILL,
            },
        })
        .unwrap();
    assert_eq!(
        death,
        Some(super::signal_control::SignalDeath {
            process: DetPid::from_raw(200),
            threads: vec![child],
        }),
        "a fatal signal's death is reported for its table to go"
    );

    let child_key = ProcessGeneration::from_backend(task(200, 200).process);
    assert!(!s.parked.permits.contains_key(&child));
    assert!(matches!(
        s.parked.child_exit_reservations[&child_key].phase,
        ChildExitReservationPhase::AwaitingPublication { status }
            if status == ExitStatus::from_raw(libc::SIGKILL)
    ));
    assert!(s.control_barrier());

    // A nonreturning self-SIGKILL has no ordinary signal-delivery boundary.
    // Its syscall handler therefore pre-reserves the same controlled Exit
    // fence as exit_group. Exercise that distinct producer path through the
    // real two-poll child publication rather than fabricating a callback pass.
    let (mut reserved, backend) = fixture();
    let _ = add(&mut reserved, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut reserved, 100, 200);
    reserved
        .reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = reserved.parked.exit_fences[&child].permit;
    assert_eq!(
        reserved
            .consume_signal_boundary(SignalBoundaryReceipt {
                permit,
                outcome: SignalBoundaryOutcome::Terminated {
                    group: true,
                    wait_status: libc::SIGKILL,
                },
            })
            .unwrap(),
        None,
        "a self-SIGKILL's fenced exit let its table go at its grant"
    );
    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::from_raw(libc::SIGKILL),
        true,
    );
    let reserved = Arc::new(Mutex::new(reserved));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        reserved.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(
        backend.child_publications.lock().unwrap().as_slice(),
        &[event.child_exit_completion().unwrap()]
    );
    assert!(!reserved.lock().unwrap().control_barrier());
}

#[test]
fn failed_after_commit_is_forwarded_unlocked_exactly_once_and_is_terminal() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 6 << 8,
        },
    })
    .unwrap();
    backend
        .fail_child_publication
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let scheduler = Arc::new(Mutex::new(s));
    let unlocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe_scheduler = Arc::downgrade(&scheduler);
    let probe_unlocked = unlocked.clone();
    *backend.failure_probe.lock().unwrap() = Some(Box::new(move || {
        let scheduler = probe_scheduler.upgrade().unwrap();
        let mut scheduler = scheduler
            .try_lock()
            .expect("failure forwarding must run outside the scheduler lock");
        assert!(
            scheduler.backend_failed(),
            "terminal state must precede outside-lock failure forwarding"
        );
        probe_unlocked.store(true, std::sync::atomic::Ordering::Relaxed);
        let wake = scheduler.report_backend_failure(reverie::BackendFailure {
            pid: reverie::Pid::from_raw(200),
            tid: reverie::Pid::from_raw(200),
            phase: "test child publication failure",
        });
        assert!(wake.is_none(), "failure must already be linearized");
        drop(scheduler);
        if let Some(wake) = wake {
            let _ = wake.send(());
        }
    }));

    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(6),
        true,
    );
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Ok(()))
    ));
    assert!(unlocked.load(std::sync::atomic::Ordering::Relaxed));
    assert!(scheduler.lock().unwrap().backend_failed());
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
    assert_eq!(backend.child_failures.lock().unwrap().len(), 1);

    let mut duplicate = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut duplicate), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut duplicate),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
    assert_eq!(backend.child_failures.lock().unwrap().len(), 1);
}

#[test]
fn rejected_child_publication_is_terminal_before_stage_two_wakes() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 10 << 8,
        },
    })
    .unwrap();
    backend
        .reject_child_publication
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(10),
        true,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    {
        let s = scheduler.lock().unwrap();
        assert!(!s.backend_failed());
        assert!(s.control_barrier());
    }
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Err(_))
    ));
    let s = scheduler.lock().unwrap();
    assert!(s.backend_failed());
    assert!(!s.control_barrier());
    assert!(matches!(
        s.parked.completed_child_exits[&ProcessGeneration::from_backend(task(200, 200).process)]
            .result,
        ChildExitPublicationResult::RejectedBeforeCommit(reverie::Errno::EBADF)
    ));
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
    assert!(backend.child_failures.lock().unwrap().is_empty());
}

#[test]
fn child_failure_does_not_steal_pending_alarm_failure_wake() {
    use futures::FutureExt;

    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 12 << 8,
        },
    })
    .unwrap();

    let alarm_process = task(100, 100).process;
    let alarm_waiter = s.backend_failure_waiter();
    let alarm_wake = s
        .report_backend_failure(reverie::BackendFailure {
            pid: alarm_process.tgid,
            tid: reverie::Pid::from_raw(100),
            phase: "retained alarm publication failure",
        })
        .unwrap();
    s.parked.failures.push(alarm_process);
    s.parked.failure_wakes.push(alarm_wake);
    backend
        .reject_child_publication
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let event = child_exit_event(
        alarm_process,
        task(200, 200).process,
        ExitStatus::Exited(12),
        true,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Err(_))
    ));
    assert!(alarm_waiter.clone().now_or_never().is_none());
    assert!(backend.failure_processes.lock().unwrap().is_empty());

    super::signal_control::flush_signal_failures(&scheduler);
    assert!(matches!(alarm_waiter.now_or_never(), Some(Ok(()))));
    assert_eq!(
        backend.failure_processes.lock().unwrap().as_slice(),
        &[alarm_process]
    );
}

#[test]
fn child_callback_drop_does_not_steal_pending_alarm_failure_wake() {
    use futures::FutureExt;

    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 13 << 8,
        },
    })
    .unwrap();
    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(13),
        true,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));

    let alarm_process = task(100, 100).process;
    let alarm_waiter = {
        let mut s = scheduler.lock().unwrap();
        let alarm_waiter = s.backend_failure_waiter();
        let alarm_wake = s
            .report_backend_failure(reverie::BackendFailure {
                pid: alarm_process.tgid,
                tid: reverie::Pid::from_raw(100),
                phase: "retained alarm publication failure",
            })
            .unwrap();
        s.parked.failures.push(alarm_process);
        s.parked.failure_wakes.push(alarm_wake);
        alarm_waiter
    };

    drop(publication);
    assert!(alarm_waiter.clone().now_or_never().is_none());
    assert!(backend.failure_processes.lock().unwrap().is_empty());
    assert!(!scheduler.lock().unwrap().control_barrier());

    super::signal_control::flush_signal_failures(&scheduler);
    assert!(matches!(alarm_waiter.now_or_never(), Some(Ok(()))));
    assert_eq!(
        backend.failure_processes.lock().unwrap().as_slice(),
        &[alarm_process]
    );
}

#[test]
fn dropping_admitted_child_publication_cleans_barrier_and_fails_run() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 3 << 8,
        },
    })
    .unwrap();
    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(3),
        true,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(scheduler.lock().unwrap().control_barrier());
    drop(publication);
    let s = scheduler.lock().unwrap();
    assert!(!s.control_barrier());
    assert!(s.backend_failed());
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
}

/// A controlled exit whose parent has more than one live thread keeps the
/// backend's own decision: no `ChildExit` timer (SIGSHARE S4 decides only for
/// a parent with one live thread, `a_controlled_exit_to_a_one_thread_parent_is_classified_at_its_grant`).
#[test]
fn controlled_exit_mode_never_enqueues_legacy_timed_sigchld() {
    let (mut s, _) = fixture();
    let _ = add(&mut s, 100, 100);
    let _ = add(&mut s, 100, 101);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    let response = Ivar::new();
    assert!(
        s.block_for_one_resource(
            child,
            &ResourceID::Exit {
                group: true,
                process: DetPid::from_raw(200),
                mm: child_mm,
            },
            &Permission::RW,
            None,
            None,
            &response,
        )
        .is_ok()
    );
    assert!(s.parked.exit_fences.contains_key(&child));
    assert!(s.blocked.timed_waiters.is_empty());

    // The default/ptrace path has no backend control. Exercise the complete
    // Exit resource arm, not only its reservation helper, so the adjacent
    // controlled assertion cannot hide loss of the legacy timed SIGCHLD.
    let mut uncontrolled = Scheduler::new(&Config::default());
    let _ = add(&mut uncontrolled, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut uncontrolled, 100, 200);
    let parent = DetPid::from_raw(100);
    let child_process = DetPid::from_raw(200);
    let deadline = uncontrolled.committed_time + LogicalTime::from_nanos(1);
    assert!(
        uncontrolled
            .block_for_one_resource(
                child,
                &ResourceID::Exit {
                    group: true,
                    process: child_process,
                    mm: child_mm,
                },
                &Permission::RW,
                None,
                None,
                &Ivar::new(),
            )
            .is_ok()
    );
    assert!(uncontrolled.parked.exit_fences.is_empty());
    assert_eq!(
        uncontrolled
            .blocked
            .timed_waiters
            .iter()
            .collect::<Vec<_>>(),
        vec![(
            deadline,
            TimedEvent::SignalEvt(
                timed_waiters::SignalTimerId::ChildExit {
                    child: child_process,
                    parent,
                },
                SignalTarget::Process(parent),
                Signal::SIGCHLD.into(),
            ),
        )]
    );
}

/// A raw `exit` of a process's last live thread ends the process as an
/// `exit_group` does, so its grant registers the parent's `ChildExit` timer
/// too; a raw `exit` of a thread that is not the last ends no process and
/// registers none, nor does one whose backend-named process is not its own.
/// Before, only `group: true` registered the timer, and a
/// parent in `rt_sigsuspend` waiting for such a child's SIGCHLD was reported
/// deadlocked (https://github.com/rrnewton/hermit/issues/3895).
#[test]
fn a_raw_exit_of_the_last_thread_registers_the_child_exit_timer() {
    let raw_exit = |s: &mut Scheduler, thread: DetTid, mm: MmId| {
        assert!(
            s.block_for_one_resource(
                thread,
                &ResourceID::Exit {
                    group: false,
                    process: DetPid::from_raw(200),
                    mm,
                },
                &Permission::RW,
                None,
                None,
                &Ivar::new(),
            )
            .is_ok()
        );
    };
    let parent = DetPid::from_raw(100);
    let child_process = DetPid::from_raw(200);

    // The child's only thread: the timer is registered.
    let mut last = Scheduler::new(&Config::default());
    let _ = add(&mut last, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut last, 100, 200);
    let deadline = last.committed_time + LogicalTime::from_nanos(1);
    raw_exit(&mut last, child, child_mm);
    assert_eq!(
        last.blocked.timed_waiters.iter().collect::<Vec<_>>(),
        vec![(
            deadline,
            TimedEvent::SignalEvt(
                timed_waiters::SignalTimerId::ChildExit {
                    child: child_process,
                    parent,
                },
                SignalTarget::Process(parent),
                Signal::SIGCHLD.into(),
            ),
        )]
    );

    // A second thread of the child leaves first: no timer.
    let mut not_last = Scheduler::new(&Config::default());
    let _ = add(&mut not_last, 100, 100);
    let (_child, child_mm, _) = add_process_child(&mut not_last, 100, 200);
    let (thread, _, _) = add_with_mm(&mut not_last, 200, 201, child_mm);
    raw_exit(&mut not_last, thread, child_mm);
    assert!(not_last.blocked.timed_waiters.is_empty());

    // A raw clone child that is its own process, whose exit the backend
    // names by its creator (https://github.com/rrnewton/hermit/issues/4012),
    // after the creator is gone: the creator's parent was already notified for
    // the creator, so no timer.
    let mut raw_clone = Scheduler::new(&Config::default());
    let _ = add(&mut raw_clone, 100, 100);
    let (creator, _, _) = add_process_child(&mut raw_clone, 100, 200);
    let (raw_child, raw_child_mm, _) = add_process_child(&mut raw_clone, 200, 300);
    raw_clone.next_turns.remove(&creator);
    raw_exit(&mut raw_clone, raw_child, raw_child_mm);
    assert!(raw_clone.blocked.timed_waiters.is_empty());
}

#[test]
fn exit_boundary_reservation_failure_has_no_local_effect() {
    let (mut s, backend) = fixture();
    let (tid, mm, _) = add(&mut s, 100, 100);
    backend
        .fail_reservation
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        s.reserve_exit_boundary(tid, DetPid::from_raw(100), mm, true),
        Err(ProtocolFailure::Identity)
    );
    assert!(s.parked.permits.is_empty());
    assert!(s.parked.exit_fences.is_empty());
    assert!(!s.control_barrier());
}

#[test]
fn selected_exit_reservation_failure_closes_tentative_pop_exactly_once() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.run_queue.push_back(child, DEFAULT_PRIORITY);
    assert_eq!(s.run_queue.tentative_pop_next(), Some(child));
    backend
        .fail_reservation
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let response = Ivar::new();
    assert!(
        s.block_for_one_resource(
            child,
            &ResourceID::Exit {
                group: true,
                process: DetPid::from_raw(200),
                mm: child_mm,
            },
            &Permission::RW,
            None,
            None,
            &response,
        )
        .is_err()
    );
    assert!(s.backend_failed());
    assert!(!s.run_queue.tentative_pop_in_progress());
    assert!(s.run_queue.contains_tid(child));
    assert!(s.parked.exit_fences.is_empty());
    assert!(s.parked.child_exit_reservations.is_empty());
}

#[test]
fn terminal_boundary_refuses_stale_lifetime_and_preserves_shared_failure_authority() {
    for failed in [false, true] {
        let (mut s, _) = fixture();
        let (tid, _, _) = add(&mut s, 100, 100);
        let (peer, _, _) = add(&mut s, 100, 101);
        s.parked.running = Some(tid);
        let permit = s
            .authorize_signal_boundary(task(100, 100))
            .unwrap()
            .unwrap();
        let receipt = SignalBoundaryReceipt {
            permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group: true,
                wait_status: 14,
            },
        };
        let mut stale = receipt;
        stale.permit.task.task_generation += 1;
        assert!(s.consume_signal_boundary(stale).is_err());
        assert!(s.next_turns.contains_key(&tid));
        assert!(s.next_turns.contains_key(&peer));
        let response = s.next_turns[&peer].resp.clone();
        s.next_turns[&peer].req.put(Ok(Resources::new(peer)));
        let failure_wake = failed.then(|| {
            s.report_backend_failure(reverie::BackendFailure {
                pid: reverie::Pid::from_raw(100),
                tid: reverie::Pid::from_raw(101),
                phase: "original failure",
            })
        });
        s.consume_signal_boundary(receipt).unwrap();
        assert_eq!(s.backend_failed(), failed);
        assert_eq!(
            matches!(response.try_read(), Some(SchedResponse::Signaled(None))),
            !failed
        );
        drop(failure_wake);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct TimedTurnObservation {
    selected: DetTid,
    queued: Vec<DetTid>,
    deadlines: Vec<Option<LogicalTime>>,
    turn: u64,
    committed: LogicalTime,
    global: LogicalTime,
}

fn ordinary_sleep(s: &mut Scheduler, tid: DetTid, deadline: LogicalTime) {
    let mut request = Resources::new(tid);
    request.insert(ResourceID::SleepUntil(deadline), Permission::RW);
    s.next_turns[&tid].req.put(Ok(request));
    s.blocked.timed_waiters.insert(deadline, tid);
}

#[test]
fn timed_maintenance_preserves_reference_selection_and_clock() {
    use futures::FutureExt;

    let observe = |controlled| {
        let (mut s, backend) = fixture_with_control(controlled);
        let mut expected_time = GlobalTime::new(&Config::default());
        let start = expected_time.as_nanos();
        let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let (a, _, _) = add(&mut s, 100, 100);
        let (b, _, _) = add(&mut s, 100, 101);
        let (r, _, _) = add(&mut s, 100, 102);
        // Two expired ordinary sleeps share a deadline and priority with R.
        // No process alarm, parked observation or host readiness is involved.
        ordinary_sleep(&mut s, a, start);
        ordinary_sleep(&mut s, b, start);
        s.next_turns[&r].req.put(Ok(Resources::new(r)));
        s.runqueue_push_back(r);
        let scheduler = Arc::new(Mutex::new(s));
        let mut last = Ok(Resources::new(r));
        let mut observations = Vec::new();
        for selected in [a, b] {
            let expected_clock = expected_time.add_scheduler_time().unwrap();
            let result = do_a_turn_blocking(scheduler.clone(), global.clone(), &last)
                .now_or_never()
                .expect("all three requests were quiescent")
                .expect("an expired ordinary sleep must commit");
            let s = scheduler.lock().unwrap();
            let observation = TimedTurnObservation {
                selected: result.tid,
                queued: s.run_queue.tids().copied().collect(),
                deadlines: [a, b]
                    .map(|tid| s.blocked.timed_waiters.thread_deadline(tid))
                    .to_vec(),
                turn: s.turn,
                committed: s.committed_time,
                global: global.lock().unwrap().as_nanos(),
            };
            assert_eq!(observation.selected, selected, "{observation:?}");
            assert_eq!(observation.committed, expected_clock);
            assert_eq!(observation.global, expected_clock);
            assert_eq!(observation.turn, observations.len() as u64 + 1);
            if selected == a {
                assert_eq!(observation.queued, [r, a]);
                assert_eq!(observation.deadlines, [None, Some(start)]);
            } else {
                assert_eq!(observation.queued, [r, a, b]);
                assert_eq!(observation.deadlines, [None, None]);
            }
            assert!(!s.run_queue.tentative_pop_in_progress());
            assert!(!s.backend_failed());
            assert!(backend.publications.lock().unwrap().is_empty());
            assert!(backend.permits.lock().unwrap().is_empty());
            // Model only the next quiescent request, without adding guest work.
            s.next_turns[&selected]
                .req
                .put(Ok(Resources::new(selected)));
            observations.push(observation);
            last = Ok(result);
        }
        observations
    };
    assert_eq!(observe(false), observe(true));
}

#[test]
fn timed_maintenance_budget_survives_alarm_refresh_and_empty_queue() {
    use futures::FutureExt;

    // The first two cases differ only in whether initial maintenance really
    // popped an event. The last case spends its event on the alarm itself and
    // exercises the separate empty-queue wake after a hook crosses the deadline.
    for (initial_due_sleep, empty_queue) in [(true, false), (false, false), (false, true)] {
        let (mut s, backend) = fixture();
        let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let start = global.lock().unwrap().as_nanos();
        let (waiter, mm, site) = add(&mut s, 100, 100);
        let deadline = start + at(if empty_queue { 100 } else { 1_000 });
        let response = sleep(&mut s, waiter, mm, site, deadline.as_nanos());
        s.replace_real_timer(waiter, waiter, start, at(1), at(0), Signal::SIGALRM)
            .unwrap();
        global
            .lock()
            .unwrap()
            .add_extra_time(std::time::Duration::from_nanos(1));
        if !empty_queue {
            // A prior maintenance pass committed this pending alarm. The turn
            // under test therefore starts with either one or zero due sleeps.
            s.committed_time = start + at(1);
            s.step2b_process_timed(&global);
        }
        backend.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        let mut sleepers = None;
        if !empty_queue {
            let (a, _, _) = add(&mut s, 100, 101);
            let (b, _, _) = add(&mut s, 100, 102);
            let (r, _, _) = add(&mut s, 100, 103);
            ordinary_sleep(
                &mut s,
                a,
                start + at(if initial_due_sleep { 1 } else { 200 }),
            );
            ordinary_sleep(&mut s, b, start + at(200));
            s.next_turns[&r].req.put(Ok(Resources::new(r)));
            s.runqueue_push_back(r);
            sleepers = Some((a, b, r));
        }
        let scheduler = Arc::new(Mutex::new(s));
        let last = Err(SkipTurn);
        let mut turn = Box::pin(do_a_turn_blocking(scheduler.clone(), global.clone(), &last));
        assert!(turn.as_mut().now_or_never().is_none());
        let control = selected(&response);
        let ack = Ivar::new();
        {
            let mut s = scheduler.lock().unwrap();
            assert_eq!(s.turn, 0, "observation must not commit a guest turn");
            assert_eq!(s.committed_time, start + at(1));
            assert_eq!(backend.publications.lock().unwrap().len(), 1);
            if let Some((a, b, _)) = sleepers {
                assert_eq!(
                    s.blocked.timed_waiters.thread_deadline(a).is_none(),
                    initial_due_sleep
                );
                assert_eq!(
                    s.blocked.timed_waiters.thread_deadline(b),
                    Some(start + at(200))
                );
            }
            s.post_control(
                waiter,
                mm,
                ControlIntent::Finish {
                    wait: control.continuation,
                    lease: control.lease,
                    site,
                    finish: ObservationFinish::ResumeSameWait,
                    ack: ack.clone(),
                },
            )
            .unwrap();
        }
        // The daemon processes the real control intent, then waits for exact
        // resume registration. No host sleeps or product test hooks are needed.
        assert!(turn.as_mut().now_or_never().is_none());
        let ticket = match ack.try_read().unwrap().unwrap() {
            FinishAck::AwaitResume(ticket) => ticket,
            other => panic!("{other:?}"),
        };
        assert!(backend.permits.lock().unwrap().is_empty());
        global
            .lock()
            .unwrap()
            .add_extra_time(std::time::Duration::from_nanos(299));
        backend.recipients.lock().unwrap().clear();
        let resumed = Ivar::new();
        scheduler
            .lock()
            .unwrap()
            .post_control(
                waiter,
                mm,
                ControlIntent::Resume {
                    ticket,
                    site,
                    response: resumed.clone(),
                },
            )
            .unwrap();
        let result = turn
            .as_mut()
            .now_or_never()
            .expect("resume restored a filled request");
        let mut s = scheduler.lock().unwrap();
        assert!(!s.backend_failed());
        assert!(!s.run_queue.tentative_pop_in_progress());
        assert_eq!(s.committed_time, start + at(300));
        assert_eq!(global.lock().unwrap().as_nanos(), start + at(300));
        if let Some((a, b, r)) = sleepers {
            assert_eq!(result.unwrap().tid, a);
            assert_eq!(s.turn, 1);
            assert_eq!(s.run_queue.tids().copied().collect::<Vec<_>>(), [r, a]);
            assert_eq!(
                s.blocked.timed_waiters.thread_deadline(b),
                Some(start + at(200))
            );
            assert_eq!(
                s.blocked.timed_waiters.thread_deadline(waiter),
                Some(deadline)
            );
            assert!(resumed.try_read().is_none());
        } else {
            assert!(result.is_err(), "empty-queue wake retains SkipTurn");
            assert_eq!(s.turn, 0);
            assert_eq!(s.run_queue.tids().copied().collect::<Vec<_>>(), [waiter]);
            assert!(s.blocked.timed_waiters.is_empty());
            assert!(resumed.try_read().is_none(), "wake must not grant early");
            drop(s);
            let granted = do_a_turn_blocking(scheduler.clone(), global.clone(), &Err(SkipTurn))
                .now_or_never()
                .unwrap()
                .unwrap();
            assert_eq!(granted.tid, waiter);
            s = scheduler.lock().unwrap();
            assert_eq!(s.turn, 1);
            assert_eq!(s.committed_time, start + at(300));
            assert_eq!(global.lock().unwrap().as_nanos(), start + at(300));
            assert!(matches!(resumed.try_read(), Some(SchedResponse::Go(_))));
        }
    }
}

#[test]
fn parked_selection_failure_preserves_process_and_optional_task_identity() {
    use std::sync::atomic::Ordering;

    use futures::FutureExt;

    for task_failure in [false, true] {
        for unrelated_running in [false, true] {
            let (mut s, backend) = fixture();
            let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
            let start = global.lock().unwrap().as_nanos();
            let (leader, _, _) = add(&mut s, 100, 100);
            let (worker, mm, site) = add(&mut s, 100, 101);
            let (foreign, _, _) = add(&mut s, 200, 200);
            for tid in [leader, foreign] {
                s.next_turns[&tid].req.put(Ok(Resources::new(tid)));
                s.runqueue_push_back(tid);
            }
            let response = sleep(&mut s, worker, mm, site, (start + at(1_000)).as_nanos());
            s.replace_real_timer(leader, worker, start, at(500), at(0), Signal::SIGALRM)
                .unwrap();
            s.replace_real_timer(foreign, foreign, start, at(700), at(0), Signal::SIGALRM)
                .unwrap();
            s.parked.running = unrelated_running.then_some(foreign);
            if task_failure {
                backend.recipients.lock().unwrap().push(SignalRecipient {
                    task: task(100, 101),
                });
                backend.fail_reservation.store(true, Ordering::Relaxed);
            } else {
                *backend.fail_recipients.lock().unwrap() = Some(task(100, 100).process);
            }
            let failure_waiter = s.backend_failure_waiter();
            let scheduler = Arc::new(Mutex::new(s));
            let result = do_a_turn_blocking(scheduler.clone(), global.clone(), &Err(SkipTurn))
                .now_or_never()
                .expect("filled requests reach the injected selection failure");
            assert!(result.is_err());
            assert!(
                failure_waiter.now_or_never().unwrap().is_ok(),
                "failure wake is published after unlocking"
            );
            let mut s = scheduler.lock().unwrap();
            let expected = BackendFailureLocation {
                pid: reverie::Pid::from_raw(100),
                tid: task_failure.then_some(reverie::Tid::from_raw(101)),
                phase: if task_failure {
                    "KVM parked signal observation"
                } else {
                    "KVM parked signal process selection"
                },
            };
            assert_eq!(s.backend_failure, Some(expected));
            assert_eq!(s.parked.failure, Some(ProtocolFailure::Identity));
            assert!(s.backend_failed());
            assert!(s.real_timers.snapshot(leader, start).is_err());
            assert_eq!(
                s.real_timers.snapshot(foreign, start).unwrap().remaining,
                at(700)
            );
            assert_eq!(
                s.blocked.timed_waiters.next_deadline(),
                Some(start + at(700))
            );
            assert_eq!(s.turn, 0);
            assert_eq!(s.committed_time, start);
            assert_eq!(global.lock().unwrap().as_nanos(), start);
            assert!(
                response.try_read().is_none(),
                "failed reservation cannot admit an observation"
            );
            assert!(backend.permits.lock().unwrap().is_empty());
            assert!(backend.publications.lock().unwrap().is_empty());
            assert!(s.parked.failure_wakes.is_empty());
            assert!(!s.run_queue.tentative_pop_in_progress());
            assert!(
                s.report_backend_failure(reverie::BackendFailure {
                    pid: reverie::Pid::from_raw(200),
                    tid: reverie::Tid::from_raw(200),
                    phase: "later independent failure",
                })
                .is_none()
            );
            assert_eq!(
                s.backend_failure,
                Some(expected),
                "first failure remains authoritative"
            );
        }
    }
}

// ChildWait uses the actual scheduler request/observation transitions below.
// These state tests do not stand in for backend signal frames or guest copyout.
fn child_wait(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    site: reverie::CallbackSignalSite,
    spec: ChildWaitSpec,
    blocked: bool,
) -> Ivar<SchedResponse> {
    let parent = s.registered_process(tid).unwrap();
    let mut resources = Resources::new(tid);
    resources.insert(ResourceID::WaitChild { parent, spec }, Permission::R);
    resources.fyi("wait-child-lifecycle");
    s.install_resource_origin(
        tid,
        ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm,
            control: ControlCapability::ChildWait { site },
        },
    )
    .unwrap();
    let turn = &s.next_turns[&tid];
    turn.req.put(Ok(resources));
    let response = turn.resp.clone();
    if blocked {
        assert!(
            s.blocked
                .child_waiters
                .insert(tid, (parent, spec))
                .is_none()
        );
    } else {
        s.run_queue.push_back(tid, DEFAULT_PRIORITY);
    }
    response
}

fn child_spec() -> ChildWaitSpec {
    ChildWaitSpec {
        selector: ChildWaitSelector::ProcessGroup(DetPid::from_raw(100)),
        owner: Some(DetTid::from_raw(100)),
        exit_class: ChildWaitExitClass::Sigchld,
    }
}

fn resume_child_observation(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    c: AlarmControl,
) -> Ivar<SchedResponse> {
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site: c.site,
            finish: ObservationFinish::ResumeSameWait,
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    let ticket = match ack.try_read().unwrap().unwrap() {
        FinishAck::AwaitResume(ticket) => ticket,
        other => panic!("{other:?}"),
    };
    let response = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Resume {
            ticket,
            site: c.site,
            response: response.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert!(!s.backend_failed());
    response
}

#[test]
fn blocked_child_wait_observation_restores_exact_membership() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    add_process_child(&mut s, 100, 200);
    let spec = child_spec();
    let response = child_wait(&mut s, tid, mm, site, spec, true);
    let request = s.next_turns[&tid].req.clone();
    let resources = request.try_read().unwrap().unwrap();
    let before_turn = s.turn;
    let before_time = s.committed_time;
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    assert!(!s.blocked.child_waiters.contains_key(&tid));
    assert!(s.run_queue.contains_tid(tid));
    assert!(s.next_turns[&tid].req.try_read().is_none());
    let resumed = resume_child_observation(&mut s, tid, mm, c);
    assert_eq!(s.blocked.child_waiters.get(&tid), Some(&(tid, spec)));
    assert!(!s.run_queue.contains_tid(tid));
    assert_eq!(s.next_turns[&tid].req, request);
    assert_eq!(request.try_read().unwrap().unwrap(), resources);
    assert_eq!(s.next_turns[&tid].resp, resumed);
    assert!(resumed.try_read().is_none());
    assert_eq!(s.turn, before_turn);
    assert_eq!(s.committed_time, before_time);
    assert!(b.permits.lock().unwrap().is_empty());
    // Repeated ignored/suppressed observations retain the same logical wait.
    s.select_parked_alarm().unwrap();
    let second = selected(&resumed);
    assert_eq!(second.continuation, c.continuation);
    assert_ne!(second.lease, c.lease);
    assert_eq!(selected(&response), c);
}

#[test]
fn queued_child_wait_observation_restores_original_queue_order() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    add_process_child(&mut s, 100, 200);
    let (peer, _, _) = add(&mut s, 300, 300);
    let spec = child_spec();
    let response = child_wait(&mut s, tid, mm, site, spec, false);
    let request = s.next_turns[&tid].req.clone();
    s.run_queue.push_back(peer, DEFAULT_PRIORITY);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    let resumed = resume_child_observation(&mut s, tid, mm, c);
    assert_eq!(s.next_turns[&tid].req, request);
    assert!(resumed.try_read().is_none());
    assert!(!s.blocked.child_waiters.contains_key(&tid));
    assert_eq!(s.run_queue.tentative_pop_next(), Some(tid));
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    assert_eq!(s.run_queue.tentative_pop_next(), Some(peer));
    s.run_queue.undo_tentative_pop();
}

#[test]
fn child_wait_ready_and_no_child_precede_alarm_selection() {
    for blocked in [false, true] {
        for change in 0..4 {
            let (mut s, b) = fixture();
            let (tid, mm, site) = add(&mut s, 100, 100);
            let (child, _, _) = add_process_child(&mut s, 100, 200);
            let spec = child_spec();
            let response = child_wait(&mut s, tid, mm, site, spec, blocked);
            let request = s.next_turns[&tid].req.clone();
            match change {
                0 => {
                    s.logically_exited_processes.insert(child);
                }
                1 => {
                    s.logically_exited_processes.insert(child);
                    assert!(s.consume_child_wait(tid, child));
                }
                2 => {
                    assert!(
                        s.thread_tree
                            .set_process_group(child, DetPid::from_raw(999))
                    );
                }
                3 => {
                    s.thread_tree
                        .process_wait
                        .get_mut(&child)
                        .unwrap()
                        .wait_owner = DetTid::from_raw(101);
                }
                _ => unreachable!(),
            }
            // Consumption wakes a blocked waiter through a deferred admission.
            // Run the same prefix as the daemon before alarm selection.
            let pending_wake = blocked && change == 1;
            assert_eq!(
                s.pending_run_queue_admissions.get(&tid).copied(),
                pending_wake.then_some(AdmitIntent::Fixed(AdmitSide::Back))
            );
            assert_eq!(s.run_queue.contains_tid(tid), !blocked);
            assert_eq!(
                s.blocked.child_waiters.contains_key(&tid),
                blocked && !pending_wake
            );
            let before_turn = s.turn;
            let before_time = s.committed_time;
            s.step2_drain_prefix().unwrap();
            assert!(s.pending_run_queue_admissions.is_empty());
            assert_eq!(s.run_queue.contains_tid(tid), !blocked || pending_wake);
            assert_eq!(
                s.blocked.child_waiters.contains_key(&tid),
                blocked && !pending_wake
            );
            assert_eq!(s.turn, before_turn);
            assert_eq!(s.committed_time, before_time);
            b.recipients.lock().unwrap().push(SignalRecipient {
                task: task(100, 100),
            });
            s.select_parked_alarm().unwrap();
            assert!(
                response.try_read().is_none(),
                "blocked {blocked}, change {change}"
            );
            assert!(b.permits.lock().unwrap().is_empty());
            assert_eq!(s.next_turns[&tid].req, request);
            assert!(s.run_queue.contains_tid(tid));
            assert!(!s.blocked.child_waiters.contains_key(&tid));
            assert_eq!(
                s.ready_child_wait(tid, spec),
                (change == 0).then_some(child)
            );
            assert_eq!(s.has_child_wait_target(tid, spec), change == 0);
            let original = request.try_read().unwrap().unwrap();
            assert_eq!(s.step3_peek().unwrap().0, tid);
            assert!(s.step4_resource_block(tid, &original, &response).is_ok());
            s.run_queue.undo_tentative_pop();
        }
    }
}

#[test]
fn child_wait_resume_rechecks_ready_consumed_group_and_owner() {
    for change in 0..5 {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        let (child, _, _) = add_process_child(&mut s, 100, 200);
        let spec = child_spec();
        let response = child_wait(&mut s, tid, mm, site, spec, true);
        let request = s.next_turns[&tid].req.clone();
        let resources = request.try_read().unwrap().unwrap();
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        s.select_parked_alarm().unwrap();
        let c = selected(&response);
        // Model changes while the observation owns an empty execution gate;
        // wake_child_waiters cannot find the removed blocked membership.
        match change {
            0 => {}
            1 => {
                s.logically_exited_processes.insert(child);
                s.wake_child_waiters(tid, child);
            }
            2 => {
                s.logically_exited_processes.insert(child);
                assert!(s.consume_child_wait(tid, child));
            }
            3 => {
                assert!(
                    s.thread_tree
                        .set_process_group(child, DetPid::from_raw(999))
                );
            }
            4 => {
                s.thread_tree
                    .process_wait
                    .get_mut(&child)
                    .unwrap()
                    .wait_owner = DetTid::from_raw(101);
            }
            _ => unreachable!(),
        }
        let resumed = resume_child_observation(&mut s, tid, mm, c);
        assert_eq!(
            s.blocked.child_waiters.get(&tid).copied(),
            (change == 0).then_some((tid, spec))
        );
        assert_eq!(s.run_queue.contains_tid(tid), change != 0);
        assert_eq!(s.next_turns[&tid].req, request);
        assert_eq!(request.try_read().unwrap().unwrap(), resources);
        assert!(resumed.try_read().is_none());
        if change != 0 {
            s.select_parked_alarm().unwrap();
            assert!(resumed.try_read().is_none());
            assert!(b.permits.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn child_wait_rejects_malformed_capability_or_membership_before_reservation() {
    for fault in 0..9 {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        add_process_child(&mut s, 100, 200);
        let spec = child_spec();
        let response = child_wait(&mut s, tid, mm, site, spec, true);
        let mut resources = s.next_turns[&tid].req.try_read().unwrap().unwrap();
        match fault {
            0 => {
                resources.tid = DetTid::from_raw(101);
            }
            1 => {
                resources.poll_attempt = 1;
            }
            2 => {
                resources.insert(ResourceID::InternalIOPolling, Permission::W);
            }
            3 => {
                resources.resources.clear();
                resources.insert(ResourceID::WaitChild { parent: tid, spec }, Permission::W);
            }
            4 => {
                s.run_queue.push_back(tid, DEFAULT_PRIORITY);
            }
            5 => {
                s.blocked.child_waiters.get_mut(&tid).unwrap().1.owner = None;
            }
            6..=8 => {
                let mut wrong = spec;
                let parent = if fault == 6 {
                    DetPid::from_raw(999)
                } else {
                    tid
                };
                if fault == 7 {
                    wrong.owner = Some(DetTid::from_raw(101));
                }
                if fault == 8 {
                    wrong.exit_class = ChildWaitExitClass::Clone;
                }
                resources.resources.clear();
                resources.insert(
                    ResourceID::WaitChild {
                        parent,
                        spec: wrong,
                    },
                    Permission::R,
                );
            }
            _ => unreachable!(),
        }
        let request = Ivar::new();
        request.put(Ok(resources));
        s.next_turns.get_mut(&tid).unwrap().req = request.clone();
        let queue = format!("{:?}", s.run_queue);
        let membership = s.blocked.child_waiters.clone();
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        assert_eq!(
            s.select_parked_alarm(),
            Err(SelectionFailure {
                pid: tid,
                tid: Some(tid),
                failure: if fault < 4 {
                    ProtocolFailure::Unsupported
                } else if fault < 6 {
                    ProtocolFailure::Phase
                } else {
                    ProtocolFailure::Identity
                },
            })
        );
        assert_eq!(s.next_turns[&tid].req, request);
        assert_eq!(s.blocked.child_waiters, membership);
        assert_eq!(format!("{:?}", s.run_queue), queue);
        assert!(response.try_read().is_none());
        assert!(b.permits.lock().unwrap().is_empty());
    }
}

#[test]
fn child_wait_reservation_failure_preserves_blocked_request() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    add_process_child(&mut s, 100, 200);
    let spec = child_spec();
    let response = child_wait(&mut s, tid, mm, site, spec, true);
    let request = s.next_turns[&tid].req.clone();
    b.fail_reservation
        .store(true, std::sync::atomic::Ordering::Relaxed);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    assert_eq!(
        s.select_parked_alarm(),
        Err(SelectionFailure {
            pid: tid,
            tid: Some(tid),
            failure: ProtocolFailure::Identity,
        })
    );
    assert_eq!(s.next_turns[&tid].req, request);
    assert_eq!(s.blocked.child_waiters.get(&tid), Some(&(tid, spec)));
    assert!(!s.run_queue.contains_tid(tid));
    assert!(response.try_read().is_none());
    assert!(b.permits.lock().unwrap().is_empty());
}

#[test]
fn child_wait_caught_selection_wins_later_exit_and_keeps_return_gate() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let (child, _, _) = add_process_child(&mut s, 100, 200);
    let response = child_wait(&mut s, tid, mm, site, child_spec(), true);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    // A nested caught-signal Tool hook publishes readiness after selection.
    // It must not turn the already-selected interruption into a child reap.
    s.logically_exited_processes.insert(child);
    s.wake_child_waiters(tid, child);
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site,
            finish: ObservationFinish::InterruptForCaught {
                selection: reverie::PreparedSignalToken {
                    site,
                    selection_nonce: 1,
                },
                dequeue: None,
            },
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert_eq!(ack.try_read(), Some(Ok(FinishAck::Interrupted)));
    assert_eq!(s.ready_child_wait(tid, child_spec()), Some(child));
    assert_eq!(
        s.next_turns[&tid].protocol.owner,
        NextTurnOwner::ReturningCaught {
            completed_wait: c.continuation
        }
    );
    assert!(s.next_turns[&tid].req.try_read().is_none());
    assert!(s.next_turns[&tid].resp.try_read().is_none());
    assert!(s.run_queue.contains_tid(tid));
    assert!(!s.blocked.child_waiters.contains_key(&tid));
    assert_eq!(s.parked.permits.get(&tid), Some(&c.permit));
    s.select_parked_alarm().unwrap();
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[c.permit]);
    let receipt = SignalBoundaryReceipt {
        permit: c.permit,
        outcome: SignalBoundaryOutcome::Caught,
    };
    s.consume_signal_boundary(receipt).unwrap();
    s.consume_signal_boundary(receipt).unwrap();
    assert_eq!(s.next_turns[&tid].protocol.owner, NextTurnOwner::Ordinary);
    assert!(!s.parked.permits.contains_key(&tid));
    assert_eq!(s.ready_child_wait(tid, child_spec()), Some(child));
}

#[test]
fn child_wait_cancellation_and_terminal_receipt_cannot_restore_membership() {
    for observed in [false, true] {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        add_process_child(&mut s, 100, 200);
        let response = child_wait(&mut s, tid, mm, site, child_spec(), true);
        if observed {
            b.recipients.lock().unwrap().push(SignalRecipient {
                task: task(100, 100),
            });
            s.select_parked_alarm().unwrap();
            let c = selected(&response);
            let ack = Ivar::new();
            s.post_control(
                tid,
                mm,
                ControlIntent::Finish {
                    wait: c.continuation,
                    lease: c.lease,
                    site,
                    finish: ObservationFinish::Terminate {
                        selection: reverie::PreparedSignalToken {
                            site,
                            selection_nonce: 2,
                        },
                    },
                    ack: ack.clone(),
                },
            )
            .unwrap();
            s.drain_control_intents();
            assert_eq!(ack.try_read(), Some(Ok(FinishAck::Terminate)));
            assert_eq!(s.parked.permits.get(&tid), Some(&c.permit));
            let receipt = SignalBoundaryReceipt {
                permit: c.permit,
                outcome: SignalBoundaryOutcome::Terminated {
                    group: false,
                    wait_status: 14,
                },
            };
            s.consume_signal_boundary(receipt).unwrap();
            s.consume_signal_boundary(receipt).unwrap();
        } else {
            s.logically_kill_thread(&tid, &tid, mm);
            assert!(matches!(
                response.try_read(),
                Some(SchedResponse::Signaled(None))
            ));
        }
        assert!(!s.next_turns.contains_key(&tid));
        assert!(!s.blocked.child_waiters.contains_key(&tid));
        // Logical retirement is immediate; physical queue removal is deferred.
        assert_eq!(
            s.pending_run_queue_removals.get(&tid),
            Some(&RemovalDisposition::Retire)
        );
        assert_eq!(s.run_queue.contains_tid(tid), observed);
        assert!(!s.parked.permits.contains_key(&tid));
        assert!(s.parked.requests.keys().all(|wait| wait.dettid != tid));
        assert!(s.thread_is_logically_killed(tid));
        let before_turn = s.turn;
        let before_time = s.committed_time;
        s.step2_drain_prefix().unwrap();
        assert!(s.pending_run_queue_removals.is_empty());
        assert!(s.pending_run_queue_admissions.is_empty());
        assert_eq!(s.turn, before_turn);
        assert_eq!(s.committed_time, before_time);
        assert!(!s.next_turns.contains_key(&tid));
        assert!(!s.blocked.child_waiters.contains_key(&tid));
        assert!(!s.run_queue.contains_tid(tid));
        assert!(!s.parked.permits.contains_key(&tid));
        assert!(s.parked.requests.keys().all(|wait| wait.dettid != tid));
        assert!(s.thread_is_logically_killed(tid));
    }
}

#[test]
fn child_wait_cross_signal_drain_retires_capability_before_due_alarm() {
    // Blocked, queued, resumed after ignored observation, and pending child
    // admission are all real positions at the prefix drain (before admissions).
    for position in 0..4 {
        for alarm_masked in [false, true] {
            let (mut s, b) = fixture();
            let (tid, mm, site) = add(&mut s, 100, 100);
            let (child, _, _) = add_process_child(&mut s, 100, 200);
            let spec = child_spec();
            let mut response = child_wait(&mut s, tid, mm, site, spec, position != 1);
            if position == 2 {
                b.recipients.lock().unwrap().push(SignalRecipient {
                    task: task(100, 100),
                });
                s.select_parked_alarm().unwrap();
                response = resume_child_observation(&mut s, tid, mm, selected(&response));
                assert_eq!(s.parked.requests.len(), 1);
                b.recipients.lock().unwrap().clear();
            }
            if position == 3 {
                s.logically_exited_processes.insert(child);
                s.wake_child_waiters(tid, child);
                assert!(s.pending_run_queue_admissions.contains_key(&tid));
            }
            let original = s.next_turns[&tid].req.clone();
            s.replace_real_timer(tid, tid, at(0), at(10), at(0), Signal::SIGALRM)
                .unwrap();
            for signal in [
                Signal::SIGUSR2,
                Signal::SIGURG,
                Signal::SIGALRM,
                Signal::SIGUSR1,
                Signal::SIGUSR2,
            ] {
                s.notify_signal_pending(tid, SigWrapper::from(signal));
            }
            let before_turn = s.turn;
            s.step2_drain_prefix().unwrap();
            assert!(!s.backend_failed());
            assert!(s.pending_cross_task_signals.is_empty());
            assert!(s.pending_run_queue_admissions.is_empty());
            assert!(!s.blocked.child_waiters.contains_key(&tid));
            assert!(s.run_queue.contains_tid(tid));
            assert_ne!(s.next_turns[&tid].req, original);
            assert_eq!(
                s.next_turns[&tid].protocol.origin.unwrap().control,
                ControlCapability::None
            );
            assert!(s.parked.requests.is_empty());
            assert!(response.try_read().is_none());
            assert_eq!(s.turn, before_turn);
            // A second notification merges with the ordinary legacy batch.
            s.notify_signal_pending(tid, SigWrapper::from(Signal::SIGWINCH));
            s.step2_drain_prefix().unwrap();
            let signals = vec![
                Signal::SIGUSR1,
                Signal::SIGUSR2,
                Signal::SIGALRM,
                Signal::SIGURG,
                Signal::SIGWINCH,
            ]
            .into_iter()
            .map(SigWrapper::from)
            .collect::<Vec<_>>();
            let resources = s.next_turns[&tid].req.try_read().unwrap().unwrap();
            assert_eq!(resources.resources.len(), 1);
            assert_eq!(
                resources
                    .resources
                    .get(&ResourceID::WaitidSignals(signals.clone())),
                Some(&Permission::W)
            );
            // The shared alarm is published after the signal prefix and before
            // selection, exactly where the stale ChildWait origin used to fail.
            s.committed_time = at(10);
            assert!(s.step2b_process_timed(&committed_clock(&s)));
            assert_eq!(b.publications.lock().unwrap().len(), 1);
            if !alarm_masked {
                b.recipients.lock().unwrap().push(SignalRecipient {
                    task: task(100, 100),
                });
            }
            s.select_parked_alarm().unwrap();
            assert!(b.permits.lock().unwrap().is_empty());
            assert!(response.try_read().is_none());
            let (selected, _, selected_response) = s.step3_peek().unwrap();
            assert_eq!(selected, tid);
            assert!(
                s.step4_resource_block(tid, &resources, &selected_response)
                    .is_ok()
            );
            assert!(
                s.step5_guest_unblock(tid, &resources, &selected_response)
                    .is_ok()
            );
            assert!(
                matches!(response.try_read(), Some(SchedResponse::Signaled(Some(actual))) if actual == signals)
            );
            // This is the ordinary grant/return-boundary route, never an
            // unowned observation or an execution gate silently discarded.
            let permit = s
                .authorize_signal_boundary(task(100, 100))
                .unwrap()
                .unwrap();
            assert_eq!(permit.site, None);
            assert_eq!(s.parked.permits.get(&tid), Some(&permit));
            s.run_queue.undo_tentative_pop();
        }
    }
}

#[test]
fn child_wait_repark_after_legacy_signal_keeps_shared_alarm_eligible() {
    for alarm_masked in [false, true] {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        add_process_child(&mut s, 100, 200);
        let spec = child_spec();
        let response = child_wait(&mut s, tid, mm, site, spec, true);
        s.notify_signal_pending(tid, SigWrapper::from(Signal::SIGURG));
        s.step2_drain_prefix().unwrap();
        let resources = s.next_turns[&tid].req.try_read().unwrap().unwrap();
        let (_, _, selected_response) = s.step3_peek().unwrap();
        assert!(
            s.step4_resource_block(tid, &resources, &selected_response)
                .is_ok()
        );
        assert!(
            s.step5_guest_unblock(tid, &resources, &selected_response)
                .is_ok()
        );
        assert!(
            matches!(response.try_read(), Some(SchedResponse::Signaled(Some(signals))) if signals == vec![SigWrapper::from(Signal::SIGURG)])
        );
        s.step6_reenquue(tid, Requeue::Back);
        // The actual helper rechecks an ignored/blocked legacy signal and asks
        // for the same ChildWait again. Drive its normal request/park here.
        s.install_resource_origin(
            tid,
            ResourceOrigin {
                rpc: RpcOrigin::DirectRequestResources,
                mm,
                control: ControlCapability::ChildWait { site },
            },
        )
        .unwrap();
        let mut resources = Resources::new(tid);
        resources.insert(ResourceID::WaitChild { parent: tid, spec }, Permission::R);
        resources.fyi("wait-child-lifecycle");
        s.next_turns[&tid].req.put(Ok(resources.clone()));
        let new_response = s.next_turns[&tid].resp.clone();
        assert_eq!(s.step3_peek().unwrap().0, tid);
        assert!(
            s.step4_resource_block(tid, &resources, &new_response)
                .is_err()
        );
        assert_eq!(s.blocked.child_waiters.get(&tid), Some(&(tid, spec)));
        if !alarm_masked {
            b.recipients.lock().unwrap().push(SignalRecipient {
                task: task(100, 100),
            });
        }
        s.select_parked_alarm().unwrap();
        if alarm_masked {
            assert!(new_response.try_read().is_none());
            assert!(b.permits.lock().unwrap().is_empty());
            assert_eq!(s.blocked.child_waiters.get(&tid), Some(&(tid, spec)));
        } else {
            assert_eq!(selected(&new_response).site, site);
            assert!(!s.blocked.child_waiters.contains_key(&tid));
            assert!(s.run_queue.contains_tid(tid));
        }
    }
}

#[test]
fn child_wait_cross_signal_rejects_conflicting_ownership_before_rewrite() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    add_process_child(&mut s, 100, 200);
    let response = child_wait(&mut s, tid, mm, site, child_spec(), true);
    let request = s.next_turns[&tid].req.clone();
    // A pending admission cannot also own a still-blocked waiter. Keep the
    // malformed-state guard rather than accepting any origin mismatch.
    s.admit_to_run_queue(tid, AdmitIntent::Fixed(AdmitSide::Back));
    s.notify_signal_pending(tid, SigWrapper::from(Signal::SIGUSR1));
    s.drain_pending_cross_task_signals();
    assert!(s.backend_failed());
    assert_eq!(s.next_turns[&tid].req, request);
    assert_eq!(
        s.next_turns[&tid].protocol.origin.unwrap().control,
        ControlCapability::ChildWait { site }
    );
    assert!(response.try_read().is_none());
    assert!(b.permits.lock().unwrap().is_empty());
}

/// A scheduler for `backend` with sequentialized threads set as given.
/// KVM's capabilities with a synthetic report of the guest's signal state
/// (production KVM reports none), under which the scheduler registers queued
/// sleeps at the first pass (`Config::reads_sleeper_signal_state`).
fn kvm_reading_signal_state() -> BackendCapabilities {
    let mut backend = BackendCapabilities::KVM;
    backend.kernel_signal_targets_are_predictable = true;
    backend
}

fn scheduler_for(backend: BackendCapabilities, sequentialize_threads: bool) -> Scheduler {
    Scheduler::new(&Config {
        sequentialize_threads,
        backend,
        ..Config::default()
    })
}

/// The decision is the backend's offer and Detcore's own sequentialization:
/// an offer in a sequentialized run is installed, and no offer, or a run that
/// is not sequentialized, leaves the run unchanged. An offer that contradicts
/// the backend's reported capabilities is refused rather than resolved either
/// way, because Reverie bounds dequeue observation by those capabilities.
/// Refusing a missing control is reverie-kvm's job, not this one.
#[test]
fn signal_control_is_installed_when_offered_in_a_sequentialized_run() {
    for (name, backend) in [
        ("PTRACE", BackendCapabilities::PTRACE),
        ("E9PATCH", BackendCapabilities::E9PATCH),
        ("LITEINST_IN_GUEST", BackendCapabilities::LITEINST_IN_GUEST),
        ("SABRE", BackendCapabilities::SABRE),
        ("DBT", BackendCapabilities::DBT),
        ("KVM", BackendCapabilities::KVM),
    ] {
        // Only KVM reports the capability, so it is the only backend whose
        // offer can be coherent.
        assert_eq!(
            backend.provides_process_signal_control,
            name == "KVM",
            "{name}"
        );
        for sequentialize in [false, true] {
            for offered in [false, true] {
                let mut s = scheduler_for(backend, sequentialize);
                let control = offered.then(|| BackendSignalControl {
                    process: Arc::new(Backend::default()),
                });
                let result = s.install_signal_control(control, sequentialize);
                let case = format!("{name} sequentialize={sequentialize} offered={offered}");
                let installs = offered && sequentialize;
                match (installs, backend.provides_process_signal_control) {
                    (true, true) => {
                        assert_eq!(
                            result.unwrap(),
                            BackendSignalControlMode::ToolControlled,
                            "{case}"
                        );
                    }
                    (true, false) => {
                        assert!(
                            matches!(
                                result,
                                Err(reverie::Error::Errno(reverie::syscalls::Errno::EINVAL))
                            ),
                            "{case}: {result:?}"
                        );
                    }
                    (false, _) => {
                        assert_eq!(
                            result.unwrap(),
                            BackendSignalControlMode::Unchanged,
                            "{case}"
                        );
                    }
                }
                assert_eq!(
                    s.signal_control_installed(),
                    installs && backend.provides_process_signal_control,
                    "{case}"
                );
            }
        }
    }
}

/// The installation is decided once, before the first turn: a second offer,
/// or an offer after a guest thread registered or a turn ran, is refused and
/// leaves the earlier answer in place.
#[test]
fn signal_control_cannot_be_installed_after_the_run_started() {
    let offer = || {
        Some(BackendSignalControl {
            process: Arc::new(Backend::default()),
        })
    };
    let einval = |result: Result<BackendSignalControlMode, reverie::Error>| {
        matches!(
            result,
            Err(reverie::Error::Errno(reverie::syscalls::Errno::EINVAL))
        )
    };

    let (mut s, _) = fixture();
    assert!(einval(s.install_signal_control(offer(), true)));
    assert!(s.signal_control_installed());

    type StartTheRun = fn(&mut Scheduler);
    let late: [(&str, StartTheRun); 3] = [
        ("a pending turn", |s| {
            add(s, 100, 100);
        }),
        ("a completed turn", |s| s.turn = 1),
        ("a registered guest thread", |s| {
            s.started_up.try_put(());
        }),
    ];
    for (name, start) in late {
        let mut s = scheduler_for(BackendCapabilities::KVM, true);
        start(&mut s);
        assert!(einval(s.install_signal_control(offer(), true)), "{name}");
        assert!(!s.signal_control_installed(), "{name}");
    }
}

/// A run on a backend without process signal control never enters the
/// controlled turn loop, even when it is sequentialized and a control is
/// offered: the offer is refused and the turn takes the ordinary path. KVM,
/// the one backend with the capability, is the positive control.
#[test]
fn backends_without_signal_control_never_enter_the_controlled_loop() {
    use futures::FutureExt;

    for (name, backend, controlled) in [
        ("PTRACE", BackendCapabilities::PTRACE, false),
        ("E9PATCH", BackendCapabilities::E9PATCH, false),
        (
            "LITEINST_IN_GUEST",
            BackendCapabilities::LITEINST_IN_GUEST,
            false,
        ),
        ("SABRE", BackendCapabilities::SABRE, false),
        ("DBT", BackendCapabilities::DBT, false),
        ("KVM", BackendCapabilities::KVM, true),
    ] {
        let mut s = scheduler_for(backend, true);
        let result = s.install_signal_control(
            Some(BackendSignalControl {
                process: Arc::new(Backend::default()),
            }),
            true,
        );
        if controlled {
            assert_eq!(
                result.unwrap(),
                BackendSignalControlMode::ToolControlled,
                "{name}"
            );
        } else {
            // The incoherent offer is refused; the run goes on uncontrolled.
            assert!(
                matches!(
                    result,
                    Err(reverie::Error::Errno(reverie::syscalls::Errno::EINVAL))
                ),
                "{name}: {result:?}"
            );
        }
        assert_eq!(s.signal_control_installed(), controlled, "{name}");
        let (r, _, _) = add(&mut s, 100, 100);
        s.next_turns[&r].req.put(Ok(Resources::new(r)));
        s.runqueue_push_back(r);
        let scheduler = Arc::new(Mutex::new(s));
        let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let result = do_a_turn_blocking(scheduler.clone(), global, &Ok(Resources::new(r)))
            .now_or_never()
            .expect("a quiescent request must not block the turn")
            .expect("the only runnable thread must commit");
        assert_eq!(result.tid, r, "{name}");
        let s = scheduler.lock().unwrap();
        assert_eq!(s.turn, 1, "{name}");
        assert_eq!(s.controlled_turn_entries, u64::from(controlled), "{name}");
    }
}

/// The controlled turn loop registers a queued plain sleep as a timed waiter
/// before its first pop, as the ordinary loop's step 2 does: a sleep queued
/// ahead of a runnable thread leaves the run queue and is a timed waiter at
/// its deadline after one turn, which the runnable thread takes.
#[test]
fn the_controlled_loop_registers_a_queued_sleep() {
    use futures::FutureExt;

    // Synthetic: model a backend that reports the guest's signal state
    // (production KVM does not set this field), so first-pass registration
    // applies in the controlled loop (`Config::reads_sleeper_signal_state`).
    let mut s = scheduler_for(kvm_reading_signal_state(), true);
    s.install_signal_control(
        Some(BackendSignalControl {
            process: Arc::new(Backend::default()),
        }),
        true,
    )
    .expect("KVM takes the control");
    assert!(s.signal_control_installed());
    let (sleeper, _, _) = add(&mut s, 100, 100);
    let (runner, _, _) = add(&mut s, 100, 101);
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    // Well past the turn's charge, so the sleep is not yet due.
    let deadline = global.lock().unwrap().as_nanos() + LogicalTime::from_nanos(1_000_000_000);
    let mut sleep = Resources::new(sleeper);
    sleep.insert(ResourceID::SleepUntil(deadline), Permission::W);
    s.next_turns[&sleeper].req.put(Ok(sleep));
    s.runqueue_push_back(sleeper);
    s.next_turns[&runner].req.put(Ok(Resources::new(runner)));
    s.runqueue_push_back(runner);
    let scheduler = Arc::new(Mutex::new(s));
    let result = do_a_turn_blocking(scheduler.clone(), global, &Ok(Resources::new(runner)))
        .now_or_never()
        .expect("a quiescent request must not block the turn")
        .expect("the runnable thread must commit");
    assert_eq!(result.tid, runner);
    let s = scheduler.lock().unwrap();
    assert_eq!(s.controlled_turn_entries, 1);
    assert_eq!(
        s.blocked.timed_waiters.thread_deadline(sleeper),
        Some(deadline)
    );
    assert!(!s.run_queue.contains_tid(sleeper));
}

/// The controlled loop's refresh registers a sleep filed after its first
/// maintenance step, before the refresh can pop a timed event: here a thread
/// the drain admitted with no request yet holds the loop until it files a
/// sleep, which the refresh makes a timed waiter while the runnable thread
/// takes the turn. On KVM first-pass registration is asked for: the scheduler
/// reads no sleeper's signal state there, so it is not the default
/// (https://github.com/rrnewton/hermit/pull/4682).
#[test]
fn the_controlled_loop_refresh_registers_a_sleep_filed_during_maintenance() {
    // Synthetic: model a backend that reports the guest's signal state
    // (production KVM does not set this field), so first-pass registration
    // applies in the controlled loop (`Config::reads_sleeper_signal_state`).
    let mut s = scheduler_for(kvm_reading_signal_state(), true);
    s.install_signal_control(
        Some(BackendSignalControl {
            process: Arc::new(Backend::default()),
        }),
        true,
    )
    .expect("KVM takes the control");
    let (runner, _, _) = add(&mut s, 100, 100);
    let (late, _, _) = add(&mut s, 100, 101);
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let deadline = global.lock().unwrap().as_nanos() + LogicalTime::from_nanos(1_000_000_000);
    s.next_turns[&runner].req.put(Ok(Resources::new(runner)));
    s.runqueue_push_back(runner);
    // Admitted by the first maintenance step's drain, its request still to come.
    s.admit_to_run_queue(late, AdmitIntent::Fixed(AdmitSide::Back));
    let scheduler = Arc::new(Mutex::new(s));
    let last_turn = Ok(Resources::new(runner));
    let mut turn = Box::pin(do_a_turn_blocking(scheduler.clone(), global, &last_turn));
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(
        turn.as_mut().poll(&mut cx).is_pending(),
        "the loop waits for the admitted thread's request"
    );
    let mut sleep = Resources::new(late);
    sleep.insert(ResourceID::SleepUntil(deadline), Permission::W);
    scheduler.lock().unwrap().next_turns[&late]
        .req
        .put(Ok(sleep));
    let Poll::Ready(result) = turn.as_mut().poll(&mut cx) else {
        panic!("a quiescent request must not block the turn");
    };
    let result = result.expect("the runnable thread must commit");
    assert_eq!(result.tid, runner);
    let s = scheduler.lock().unwrap();
    assert_eq!(
        s.blocked.timed_waiters.thread_deadline(late),
        Some(deadline)
    );
    assert!(!s.run_queue.contains_tid(late));
}

/// The controlled loop's first maintenance step registers a sleep its drain
/// admits (a new thread whose first call is a sleep), before step 3 can
/// choose by priority, as the ordinary loop's step 2 does
/// (https://github.com/rrnewton/hermit/pull/4682).
#[test]
fn the_controlled_loop_registers_a_sleep_its_drain_admits() {
    use futures::FutureExt;

    // Synthetic: model a backend that reports the guest's signal state
    // (production KVM does not set this field), so first-pass registration
    // applies in the controlled loop (`Config::reads_sleeper_signal_state`).
    let mut s = scheduler_for(kvm_reading_signal_state(), true);
    s.install_signal_control(
        Some(BackendSignalControl {
            process: Arc::new(Backend::default()),
        }),
        true,
    )
    .expect("KVM takes the control");
    let (runner, _, _) = add(&mut s, 100, 100);
    let (newborn, _, _) = add(&mut s, 100, 101);
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let deadline = global.lock().unwrap().as_nanos() + LogicalTime::from_nanos(1_000_000_000);
    s.next_turns[&runner].req.put(Ok(Resources::new(runner)));
    s.runqueue_push_back(runner);
    let mut sleep = Resources::new(newborn);
    sleep.insert(ResourceID::SleepUntil(deadline), Permission::W);
    s.next_turns[&newborn].req.put(Ok(sleep));
    s.admit_to_run_queue(newborn, AdmitIntent::Fixed(AdmitSide::Back));
    let scheduler = Arc::new(Mutex::new(s));
    let result = do_a_turn_blocking(scheduler.clone(), global, &Ok(Resources::new(runner)))
        .now_or_never()
        .expect("a quiescent request must not block the turn")
        .expect("the runnable thread must commit");
    assert_eq!(result.tid, runner);
    let s = scheduler.lock().unwrap();
    assert_eq!(
        s.blocked.timed_waiters.thread_deadline(newborn),
        Some(deadline)
    );
    assert!(!s.run_queue.contains_tid(newborn));
}

/// Where a real timer goes is decided by the installed control alone. Without
/// one, Detcore's alarm model serves it, whatever the backend's capabilities.
/// A backend with process signal control never runs a sequentialized guest
/// without the control: reverie-kvm refuses that run before any guest code,
/// because Detcore reports the need through
/// `reverie::Tool::requires_signal_control`. So a capable scheduler without a
/// control is reached only in this test, and it gets no refusal of its own.
/// An installed control drives SIGALRM and refuses any other timer signal.
#[test]
fn real_timers_follow_the_installed_control() {
    let pid = DetTid::from_raw(100);

    // No installed control: Detcore's own alarm model, with or without the
    // capability, sequentialized or not.
    for (name, mut s) in [
        ("KVM capability, no control", fixture_with_control(false).0),
        ("PTRACE", scheduler_for(BackendCapabilities::PTRACE, false)),
        (
            "PTRACE sequentialized",
            scheduler_for(BackendCapabilities::PTRACE, true),
        ),
    ] {
        assert!(!s.signal_control_installed(), "{name}");
        assert_eq!(
            s.replace_real_timer(pid, pid, at(0), at(10), at(3), Signal::SIGALRM),
            Ok((at(0), at(0))),
            "{name}"
        );
        assert_eq!(
            s.blocked.timed_waiters.alarm_state(pid),
            Some((at(10), at(3))),
            "{name}"
        );
        assert_eq!(
            s.itimer_snapshot(pid, at(4)),
            Ok(super::real_timer::ItimerSnapshot {
                remaining: at(6),
                interval: at(3),
            }),
            "{name}"
        );
    }

    // An installed control: the process's real timer, SIGALRM only.
    let (mut s, _) = fixture();
    let (tid, _, _) = add(&mut s, 100, 100);
    assert_eq!(
        s.replace_real_timer(pid, tid, at(0), at(10), at(3), Signal::SIGVTALRM),
        Err(TimerFailure::Unsupported)
    );
    assert_eq!(
        s.replace_real_timer(pid, tid, at(0), at(10), at(3), Signal::SIGALRM),
        Ok((at(0), at(0)))
    );
    assert_eq!(s.blocked.timed_waiters.alarm_state(pid), None);
    assert_eq!(
        s.itimer_snapshot(pid, at(4)),
        Ok(super::real_timer::ItimerSnapshot {
            remaining: at(6),
            interval: at(3),
        })
    );
}

/// A sequentialized scheduler for in-guest LiteInst, the backend the SIGALRM
/// ledger serves.
fn liteinst_scheduler() -> Scheduler {
    scheduler_for(BackendCapabilities::LITEINST_IN_GUEST, true)
}

/// Files `tid`'s request for `resource` as in-guest LiteInst does: an ordinary
/// request with no parked origin (the backend offers no parked signal sites),
/// through the ledger's view of `request_put`. The runtime has published the
/// thread's "SIGALRM virtually blocked" bit `blocked`.
fn file(s: &mut Scheduler, tid: DetTid, resource: ResourceID, blocked: bool) {
    file_attempt(s, tid, resource, 0, blocked);
}

/// [`file`] with a polling attempt count.
fn file_attempt(
    s: &mut Scheduler,
    tid: DetTid,
    resource: ResourceID,
    poll_attempt: u32,
    blocked: bool,
) {
    let mut r = Resources::new(tid);
    r.insert(resource, Permission::W);
    r.poll_attempt = poll_attempt;
    file_request(s, r, blocked);
}

fn file_request(s: &mut Scheduler, r: Resources, blocked: bool) {
    let tid = r.tid;
    s.set_sigalrm_blocked(tid, blocked);
    s.next_turns[&tid].req.put(Ok(r));
}

/// Files `tid`'s polling retry and lets step 4 defer it as a poller, which
/// resets the stored request's attempt count.
fn defer_polling_retry(s: &mut Scheduler, tid: DetTid) {
    let mut r = Resources::new(tid);
    r.insert(ResourceID::InternalIOPolling, Permission::W);
    r.poll_attempt = 1;
    file_request(s, r.clone(), false);
    s.runqueue_push_back(tid);
    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
    assert!(s.step4_resource_block(tid, &r, &Ivar::new()).is_err());
    let stored = s.next_turns[&tid].req.try_read().unwrap().unwrap();
    assert_eq!(stored.poll_attempt, 0, "the deferral reset the count");
    assert!(s.run_queue.contains_tid(tid));
}

/// Files `tid`'s emulated `pause` as `handle_pause` does, without parking it.
fn file_pause(s: &mut Scheduler, tid: DetTid, blocked: bool) {
    let mut r = Resources::new(tid);
    r.insert(
        ResourceID::SleepUntil(LogicalTime::INDEFINITE),
        Permission::W,
    );
    r.fyi(crate::resources::PAUSE_FYI);
    file_request(s, r, blocked);
}

/// Parks `tid` in an emulated `pause`, as `handle_pause` and its admission do:
/// a filed `SleepUntil(INDEFINITE)` registered with no deadline.
fn pause(s: &mut Scheduler, tid: DetTid, blocked: bool) {
    file_pause(s, tid, blocked);
    s.blocked.timed_waiters.insert(LogicalTime::INDEFINITE, tid);
}

/// Publishes `tid`'s SIGALRM bit as its runtime would.
fn eligibility(s: &mut Scheduler, tid: DetTid, blocked: bool) {
    s.set_sigalrm_blocked(tid, blocked);
}

/// The filed request of `tid` now carries an `InboundSignal(SIGALRM)`, which
/// its grant answers with `Signaled` (`unblock_guest`).
fn woken_by_sigalrm(s: &Scheduler, tid: DetTid) -> bool {
    s.inbound_signals(tid) == vec![SigWrapper::from(Signal::SIGALRM)]
}

/// While a process handles SIGALRM, an emulated expiry is pending in the
/// scheduler's ledger instead of reaching the kernel, and it wakes the process
/// parked in `pause`, which then takes it once.
#[test]
fn a_handled_sigalrm_expiry_goes_to_the_ledger_and_wakes_a_parked_pause() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    pause(&mut s, tid, false);
    let attempts = s.host_signal_attempts;

    s.fire_alarm(pid, tid, Signal::SIGALRM);

    assert_eq!(s.host_signal_attempts, attempts, "no host signal");
    assert!(s.sigalrm_pending(pid));
    assert!(woken_by_sigalrm(&s, tid));
    assert!(s.run_queue.contains_tid(tid));
    assert!(s.blocked.timed_waiters.is_empty());
    assert!(s.take_sigalrm(pid, tid));
    assert!(!s.sigalrm_pending(pid));
    assert!(!s.take_sigalrm(pid, tid));
}

/// Without a published handler, and for any other signal, the expiry takes
/// the host path as before. (Neither process is registered, so the host path
/// stops at target selection instead of signalling a real thread.)
#[test]
fn an_unhandled_sigalrm_expiry_still_reaches_the_host() {
    let mut s = liteinst_scheduler();
    let (unhandled, handled) = (DetPid::from_raw(100), DetPid::from_raw(101));
    s.set_sigalrm_handled(handled, true);
    let attempts = s.host_signal_attempts;

    s.fire_alarm(unhandled, DetTid::from_raw(100), Signal::SIGALRM);
    assert_eq!(s.host_signal_attempts, attempts + 1);
    s.fire_alarm(handled, DetTid::from_raw(101), Signal::SIGUSR1);
    assert_eq!(s.host_signal_attempts, attempts + 2);
    assert!(!s.sigalrm_pending(unhandled));
    assert!(!s.sigalrm_pending(handled));
}

/// A second expiry while one is pending coalesces; a blocked SIGALRM stays
/// pending and wakes nothing; once the bit clears, the sweep wakes the parked
/// pause.
#[test]
fn a_pending_sigalrm_coalesces_and_waits_until_it_is_unblocked() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    pause(&mut s, tid, true);

    s.fire_alarm(pid, tid, Signal::SIGALRM);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    assert!(s.sigalrm_pending(pid));
    s.step2_sigalrm_ledger();
    assert!(!woken_by_sigalrm(&s, tid));
    assert!(!s.run_queue.contains_tid(tid));
    assert!(!s.sigalrm_loss_recorded(pid));
    assert!(!s.take_sigalrm(pid, tid), "a blocked SIGALRM is not taken");

    eligibility(&mut s, tid, false);
    s.step2_sigalrm_ledger();
    assert!(woken_by_sigalrm(&s, tid));
    assert!(s.take_sigalrm(pid, tid));
    assert!(!s.take_sigalrm(pid, tid), "two expiries coalesced into one");
}

/// A pause selected with its entry already due is resolved in that same
/// selected turn: the admission grants it with `Signaled(SIGALRM)` instead of
/// parking it, so no other thread queued behind it commits first.
#[test]
fn a_pause_admitted_with_a_due_sigalrm_returns_in_its_own_turn() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let (other, _, _) = add(&mut s, 200, 200);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    eligibility(&mut s, tid, false);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    assert!(s.sigalrm_pending(pid));

    file_pause(&mut s, tid, false);
    s.runqueue_push_back(tid);
    s.runqueue_push_back(other);
    // The entry is pending and due, but the pause has not been admitted yet:
    // not a loss, and not woken.
    s.step2_sigalrm_ledger();
    assert!(!s.sigalrm_loss_recorded(pid));
    assert!(!woken_by_sigalrm(&s, tid));

    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
    let pause = ResourceID::SleepUntil(LogicalTime::INDEFINITE);
    assert!(
        s.block_for_one_resource(tid, &pause, &Permission::W, None, None, &Ivar::new())
            .is_ok(),
        "granted in its own selected turn"
    );
    assert!(woken_by_sigalrm(&s, tid));
    assert!(s.blocked.timed_waiters.is_empty(), "never parked");
    assert!(s.take_sigalrm(pid, tid));
}

/// A pause admitted while SIGALRM is blocked, or with nothing pending, is
/// not resolved by the ledger.
#[test]
fn a_pause_without_a_due_sigalrm_is_not_resolved_at_admission() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    file_pause(&mut s, tid, false);
    assert!(!s.admit_pause_with_due_sigalrm(tid), "nothing pending");
    assert!(s.next_turns[&tid].req.try_read().is_some(), "pause kept");
    assert!(!woken_by_sigalrm(&s, tid));

    eligibility(&mut s, tid, true);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    assert!(!s.admit_pause_with_due_sigalrm(tid), "blocked");
    assert!(s.next_turns[&tid].req.try_read().is_some(), "pause kept");
    assert!(!woken_by_sigalrm(&s, tid));
    assert!(s.sigalrm_pending(pid));
}

/// A due SIGALRM that finds its thread blocked in a wait phase 1 cannot
/// interrupt (a nanosleep here) records one determinism loss and wakes
/// nothing; the sweep that opens step 2 records it for a sleep that blocked
/// with its entry already due.
#[test]
fn a_due_sigalrm_in_a_blocked_wait_records_one_loss() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    file(&mut s, tid, ResourceID::SleepUntil(at(1_000_000)), false);
    s.blocked.timed_waiters.insert(at(1_000_000), tid);

    s.fire_alarm(pid, tid, Signal::SIGALRM);
    assert!(s.sigalrm_loss_recorded(pid));
    assert!(!woken_by_sigalrm(&s, tid));
    assert!(s.next_turns[&tid].resp.try_read().is_none(), "not answered");
    assert!(!s.run_queue.contains_tid(tid), "still blocked");
    // Reported once, however many sweeps see it.
    s.step2_sigalrm_ledger();
    assert!(s.sigalrm_loss_recorded(pid));

    // A second process whose sleep blocked after its entry became due.
    let (late, _, _) = add(&mut s, 300, 300);
    let late_pid = DetPid::from_raw(300);
    s.set_sigalrm_handled(late_pid, true);
    eligibility(&mut s, late, false);
    s.fire_alarm(late_pid, late, Signal::SIGALRM);
    assert!(!s.sigalrm_loss_recorded(late_pid), "running: not a loss");
    file(&mut s, late, ResourceID::SleepUntil(at(1_000_000)), false);
    s.blocked.timed_waiters.insert(at(1_000_000), late);
    s.step2_sigalrm_ledger();
    assert!(s.sigalrm_loss_recorded(late_pid));
}

/// The sweep opens step 2, before the timed events: a sleep that blocked with
/// its entry already due and whose deadline has now passed is recorded as
/// lost before step 2b wakes it. Linux would have interrupted it.
#[test]
fn the_sigalrm_sweep_sees_an_expiring_sleep_before_the_timed_events_wake_it() {
    let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    eligibility(&mut s, tid, false);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    file(&mut s, tid, ResourceID::SleepUntil(at(5)), false);
    s.blocked.timed_waiters.insert(at(5), tid);
    s.committed_time = at(10);

    let _ = s.step2_process_blocked(&global_time);

    assert!(s.run_queue.contains_tid(tid), "step 2b woke the sleep");
    assert!(s.sigalrm_loss_recorded(pid));
}

/// A filed request that has not blocked is no loss: a yield
/// (`SleepUntil(0)`), granted at once, or a sleep not yet admitted. The entry
/// waits for the call's completion.
#[test]
fn a_due_sigalrm_with_a_filed_request_that_has_not_blocked_is_no_loss() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    file(&mut s, tid, ResourceID::SleepUntil(at(0)), false);
    s.runqueue_push_back(tid);

    s.fire_alarm(pid, tid, Signal::SIGALRM);
    s.step2_sigalrm_ledger();
    assert!(s.sigalrm_pending(pid));
    assert!(!s.sigalrm_loss_recorded(pid));
    assert!(!woken_by_sigalrm(&s, tid));
}

/// A pipe read's polling request as `execute_nonblockable_fd_syscall`
/// and `retry_nonblocking_syscall_helper` file it: `InternalIOPolling`
/// alone, restartable (`ERESTARTSYS`), naming its `read_fd`.
fn pipe_read_request(tid: DetTid, attempt: u32) -> Resources {
    let mut r = Resources::new(tid);
    r.insert(ResourceID::InternalIOPolling, Permission::W);
    r.fyi("read");
    r.poll_attempt = attempt;
    r.set_signal_interrupt_errno(reverie::syscalls::Errno::ERESTARTSYS);
    r.read_fd = Some((detcore_model::fd::FilesId::initial(tid), 4));
    // As a plain run's guest-held pipe end files its retries
    // (`refresh_guest_poll_state` sets both from the held mark).
    r.polls_guest_state_only = true;
    r.relies_on_guest_held_marks = true;
    r.private_sigchld_decides = true;
    r
}

/// Files a pipe read's polling retry number `attempt`.
fn file_pipe_read_attempt(s: &mut Scheduler, tid: DetTid, attempt: u32) {
    file_request(s, pipe_read_request(tid, attempt), false);
}

/// Files the successor request the probe's syscall pass produced: the
/// next retry of the same logical read, at the attempt lineage the
/// probe advanced, with the probe's progress bit.
fn file_probe_successor(s: &mut Scheduler, tid: DetTid, attempt: u32, progress: bool) {
    let mut r = pipe_read_request(tid, attempt);
    r.previous_attempt_made_progress = progress;
    file_request(s, r, false);
}

/// Files a follow-up syscall request of the original shape after a probe
/// completed: its presence is the R1 post-result state.
fn file_post_result(s: &mut Scheduler, tid: DetTid) {
    file(s, tid, ResourceID::SleepUntil(at(0)), false);
}

/// Whether `tid` still has a due SIGALRM reserved for its reserved probe.
fn sigalrm_reserved_for(s: &Scheduler, tid: DetTid) -> bool {
    s.sigalrm_reserved_poll_target() == Some(tid)
}

/// The stored request's (poll_attempt, upgraded_from_attempt) pair:
/// the upgrade resets the former to 0 and keeps the lineage in the
/// latter.
fn stored_attempts(s: &Scheduler, tid: DetTid) -> (u32, u32) {
    let stored = s.next_turns[&tid].req.try_read().unwrap().unwrap();
    (stored.poll_attempt, stored.upgraded_from_attempt)
}

/// dh-ru056 slice 1 (rev.2 section 5.1): a due SIGALRM in an
/// `InternalIOPolling` pipe-read wait is no longer a loss. The entry is
/// reserved to the owner, its request is normalized by the same
/// upgrade step 4 applies, and nothing is substituted until the written
/// probe proves no progress; then the `InboundSignal` wake replaces the
/// successor retry. This is the filed-but-ungranted substate.
#[test]
fn a_due_sigalrm_reserves_a_filed_pipe_read_and_wakes_after_a_no_progress_probe() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    file_pipe_read_attempt(&mut s, tid, 1);
    s.runqueue_push_back(tid);

    s.fire_alarm(pid, tid, Signal::SIGALRM);
    s.step2_sigalrm_ledger();

    assert!(sigalrm_reserved_for(&s, tid), "reserved, not lost");
    assert!(!s.sigalrm_loss_recorded(pid));
    assert!(s.sigalrm_pending(pid), "still pending until taken");
    assert!(
        !woken_by_sigalrm(&s, tid),
        "no substitution before the probe"
    );
    assert_eq!(
        stored_attempts(&s, tid),
        (0, 1),
        "normalized by the upgrade"
    );
    // Placed precedence: step 3 resumes the unrun probe first (None
    // before any alarm fires).
    assert_eq!(
        s.reserved_probe_to_resume(),
        Some((pid, tid)),
        "the probe is placed"
    );

    // The probe is selected (popped), grants, and files the next
    // retry: no bytes moved.
    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    s.clear_nextturn(tid).unwrap();
    file_probe_successor(&mut s, tid, 1, false);
    s.step2_sigalrm_ledger();

    assert!(woken_by_sigalrm(&s, tid), "wake substitutes the successor");
    assert!(s.sigalrm_pending(pid));
    assert!(!sigalrm_reserved_for(&s, tid));
}

/// Same transition from the deferred-poller substate: step 4 already
/// deferred the retry and reset its count; reservation restores the
/// probe without rewriting the stored request.
#[test]
fn a_due_sigalrm_reserves_a_deferred_pipe_poller_and_wakes_after_a_no_progress_probe() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    file_pipe_read_attempt(&mut s, tid, 1);
    s.runqueue_push_back(tid);
    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
    let stored = s.next_turns[&tid].req.try_read().unwrap().unwrap();
    assert!(s.step4_resource_block(tid, &stored, &Ivar::new()).is_err());
    let stored = s.next_turns[&tid].req.try_read().unwrap().unwrap();
    assert_eq!(stored.poll_attempt, 0, "deferral reset the count");

    s.fire_alarm(pid, tid, Signal::SIGALRM);
    s.step2_sigalrm_ledger();

    assert!(sigalrm_reserved_for(&s, tid));
    assert!(!s.sigalrm_loss_recorded(pid));
    assert_eq!(
        s.reserved_probe_to_resume(),
        Some((pid, tid)),
        "poller restored and placed for its probe"
    );

    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    s.clear_nextturn(tid).unwrap();
    file_probe_successor(&mut s, tid, 1, false);
    s.step2_sigalrm_ledger();
    assert!(woken_by_sigalrm(&s, tid));
    assert!(!sigalrm_reserved_for(&s, tid));
}

/// R1: a writer whose commit precedes the expiry has its data in the
/// pipe when the reserved probe runs: the probe completes, its result
/// stands untouched, and the entry stays pending for the ordinary
/// post-result completion delivery — never an EINTR substitution.
#[test]
fn a_reserved_pipe_probe_that_completes_keeps_its_result_and_stays_pending() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    file_pipe_read_attempt(&mut s, tid, 1);
    s.runqueue_push_back(tid);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    s.step2_sigalrm_ledger();
    assert!(sigalrm_reserved_for(&s, tid));

    // The probe is selected (popped) and completes the read (data
    // already there); the thread's next request is an unrelated
    // follow-up syscall.
    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    s.clear_nextturn(tid).unwrap();
    file_post_result(&mut s, tid);
    s.runqueue_push_back(tid);
    s.step2_sigalrm_ledger();

    assert!(!woken_by_sigalrm(&s, tid), "no wake substituted a result");
    assert!(s.sigalrm_pending(pid), "delivery remains post-result");
    assert!(!sigalrm_reserved_for(&s, tid), "reservation consumed");
}

/// G1: a single blocking pipe read that moved bytes returns; it
/// cannot still be polling the same read. A progressed-but-repolls
/// successor therefore means a second attached probe ran between
/// reservation and classification — a protocol violation slice 1
/// keeps unrepresentable. The reservation is consumed into the
/// ordinary due-entry loss, never carried across retries.
#[test]
fn a_reserved_probe_that_progressed_but_repolls_is_a_g1_loss() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    file_pipe_read_attempt(&mut s, tid, 1);
    s.runqueue_push_back(tid);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    s.step2_sigalrm_ledger();
    assert!(sigalrm_reserved_for(&s, tid));

    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    s.clear_nextturn(tid).unwrap();
    file_probe_successor(&mut s, tid, 1, true);
    s.runqueue_push_back(tid);
    s.step2_sigalrm_ledger();

    assert!(
        !sigalrm_reserved_for(&s, tid),
        "violation consumes the reservation"
    );
    assert!(!woken_by_sigalrm(&s, tid));
    assert!(s.sigalrm_loss_recorded(pid));
}

/// Failed wake (review gate G2): if the reserved target exits
/// before its probe grants, retiring the thread must tear the
/// reservation down with it — no stale reservation may outlive
/// its target and block every later reserve.
#[test]
fn retiring_the_reserved_target_leaves_no_stale_reservation() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    file_pipe_read_attempt(&mut s, tid, 1);
    s.runqueue_push_back(tid);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    s.step2_sigalrm_ledger();
    assert!(sigalrm_reserved_for(&s, tid));

    s.retire_sigalrm_thread(tid, pid);

    assert!(!sigalrm_reserved_for(&s, tid), "no stale reservation");
    assert_eq!(s.sigalrm_reserved_poll_target(), None);
}

/// G2 position: the completion check runs before the sweep acts on
/// any other entry. When one process's probe has filed its successor
/// and, in the same pass, the sweep finds another process's
/// already-pending entry in a polling read, the first reservation
/// settles (its wake substitutes) before the sweep reaches the second,
/// which is then reserved rather than charged phase 1's loss. A check
/// placed after the sweep leaves the first reservation outstanding
/// there and charges the loss.
#[test]
fn the_completion_check_settles_a_probe_before_the_sweep_reaches_another_process() {
    let (mut s, [(first, first_pid), (second, second_pid)]) = two_polling_readers();
    // The second process's entry fell due while it ran: pending, no loss.
    eligibility(&mut s, second, false);
    s.fire_alarm(second_pid, second, Signal::SIGALRM);
    assert!(!s.sigalrm_loss_recorded(second_pid));
    file_pipe_read_attempt(&mut s, first, 1);
    s.runqueue_push_back(first);
    s.fire_alarm(first_pid, first, Signal::SIGALRM);
    assert!(sigalrm_reserved_for(&s, first));

    // Production order: step 3 resumes the first probe (its grant spends
    // the alarm priority of https://github.com/rrnewton/hermit/issues/4929,
    // which does not keep another process's entry from being reserved), and
    // the probe files its no-progress successor; the second process's read
    // polls by the next pass's sweep.
    assert_eq!(grant_selected(&mut s), first);
    assert_eq!(s.parked.alarm_priority_served, Some(first_pid));
    file_probe_successor(&mut s, first, 1, false);
    file_pipe_read_attempt(&mut s, second, 1);
    s.runqueue_push_back(second);
    s.step2_sigalrm_ledger();

    assert!(woken_by_sigalrm(&s, first), "first probe settled");
    assert!(sigalrm_reserved_for(&s, second), "second reserved");
    assert!(!s.sigalrm_loss_recorded(second_pid), "no loss for second");
    assert!(!s.sigalrm_loss_recorded(first_pid));
}

/// Only a restartable read is reserved (design rev.2 section 5.1:
/// `signal_interrupt_errno` is `ERESTARTSYS`). A polled wait that
/// returns its own errno when a signal ends it (a deadline wait's
/// EINTR), or one that names none, must never take the restart
/// table's outcome: reservation refuses it and the due-entry loss is
/// preserved exactly as before this machinery.
#[test]
fn a_reserved_probe_refuses_a_poll_that_is_not_restartable() {
    for errno in [Some(libc::EINTR), Some(libc::ETIMEDOUT), None] {
        let mut s = liteinst_scheduler();
        let (tid, _, _) = add(&mut s, 100, 100);
        let pid = DetPid::from_raw(100);
        s.set_sigalrm_handled(pid, true);
        let mut r = pipe_read_request(tid, 1);
        r.signal_interrupt_errno = errno;
        file_request(&mut s, r, false);
        s.runqueue_push_back(tid);

        s.fire_alarm(pid, tid, Signal::SIGALRM);
        s.step2_sigalrm_ledger();

        assert!(!sigalrm_reserved_for(&s, tid), "{errno:?}");
        assert!(s.sigalrm_loss_recorded(pid), "{errno:?}: out of scope");
    }
}

/// A pipe read with no `read_fd` (no request production files for
/// a pipe read now) is not a bound read: the loss stands.
#[test]
fn a_reserved_probe_refuses_a_poll_without_a_read_fd() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    let mut r = pipe_read_request(tid, 1);
    r.read_fd = None;
    file_request(&mut s, r, false);
    s.runqueue_push_back(tid);

    s.fire_alarm(pid, tid, Signal::SIGALRM);
    s.step2_sigalrm_ledger();

    assert!(!sigalrm_reserved_for(&s, tid));
    assert!(s.sigalrm_loss_recorded(pid));
}

/// Two single-threaded handling processes, each in a polling pipe
/// read, with `first` (DetPid 100) and `second` (DetPid 300) both
/// handling SIGALRM.
fn two_polling_readers() -> (Scheduler, [(DetTid, DetPid); 2]) {
    let mut s = liteinst_scheduler();
    let (first, _, _) = add(&mut s, 100, 100);
    let (second, _, _) = add(&mut s, 300, 300);
    let first_pid = DetPid::from_raw(100);
    let second_pid = DetPid::from_raw(300);
    for pid in [first_pid, second_pid] {
        s.set_sigalrm_handled(pid, true);
    }
    (s, [(first, first_pid), (second, second_pid)])
}

/// Single flight across processes, decided by the schedule alone.
/// Slice 1 holds one reservation, so of two entries that fall due
/// while their owners poll pipe reads, the first to fall due at a
/// step-2 point is reserved and the other keeps phase 1's loss — no
/// panic (the reserve used to assert). Two orders decide "first":
/// expiries diverted at step 2b act in timed-pop order (here the
/// higher DetPid fires first and wins); entries already pending when
/// the sweep finds both polling act in the sweep's ledger order
/// (ascending DetPid). Either way the reserved probe still wakes.
#[test]
fn a_second_process_due_in_the_same_sweep_keeps_the_phase1_loss() {
    // Diverted expiries: timed-pop order, second fired first.
    let (mut s, [(first, first_pid), (second, second_pid)]) = two_polling_readers();
    for (tid, pid) in [(second, second_pid), (first, first_pid)] {
        file_pipe_read_attempt(&mut s, tid, 1);
        s.runqueue_push_back(tid);
        s.fire_alarm(pid, tid, Signal::SIGALRM);
    }
    s.step2_sigalrm_ledger();
    assert!(sigalrm_reserved_for(&s, second), "first to fall due");
    assert!(!s.sigalrm_loss_recorded(second_pid));
    assert!(
        s.sigalrm_loss_recorded(first_pid),
        "the other keeps the loss"
    );
    assert!(!woken_by_sigalrm(&s, first));

    // The reserved probe runs, moves no bytes, and is woken as usual.
    assert_eq!(s.run_queue.tentative_pop_tid(second), Some(second));
    assert_eq!(s.run_queue.commit_tentative_pop(), second);
    s.clear_nextturn(second).unwrap();
    file_probe_successor(&mut s, second, 1, false);
    s.step2_sigalrm_ledger();
    assert!(woken_by_sigalrm(&s, second));
    // The completion check settled before the sweep, but the other
    // entry's loss is already recorded, so it is not reserved again: the
    // loss stands and the entry waits for a completion's delivery.
    assert_eq!(s.sigalrm_reserved_poll_target(), None);
    assert!(s.sigalrm_loss_recorded(first_pid));

    // Entries pending before either read polled: the sweep's
    // ascending-DetPid order, whatever order they were filed in.
    let (mut s, [(first, first_pid), (second, second_pid)]) = two_polling_readers();
    for (tid, pid) in [(second, second_pid), (first, first_pid)] {
        eligibility(&mut s, tid, false);
        s.fire_alarm(pid, tid, Signal::SIGALRM);
        assert!(!s.sigalrm_loss_recorded(pid), "running: not a loss");
    }
    for tid in [second, first] {
        file_pipe_read_attempt(&mut s, tid, 1);
        s.runqueue_push_back(tid);
    }
    s.step2_sigalrm_ledger();
    assert!(
        sigalrm_reserved_for(&s, first),
        "lowest DetPid in the sweep"
    );
    assert!(!s.sigalrm_loss_recorded(first_pid));
    assert!(s.sigalrm_loss_recorded(second_pid));
}

/// A due SIGALRM finds its thread in a polling wait, an `InternalIOPolling`
/// operation retried after EAGAIN, which stays on the run queue between
/// retries: a loss, whether step 4 has yet to see the filed retry or has
/// deferred it and reset its count. A first attempt, which may complete at
/// once, is not.
#[test]
fn a_due_sigalrm_in_a_polling_wait_records_one_loss() {
    let mut s = liteinst_scheduler();
    let (first, _, _) = add(&mut s, 100, 100);
    let first_pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(first_pid, true);
    file(&mut s, first, ResourceID::InternalIOPolling, false);
    s.runqueue_push_back(first);
    s.fire_alarm(first_pid, first, Signal::SIGALRM);
    assert!(
        !s.sigalrm_loss_recorded(first_pid),
        "a first attempt is not a wait"
    );
    // The attempt saw EAGAIN; its retry is filed but not yet seen by step 4.
    s.clear_nextturn(first).unwrap();
    file_attempt(&mut s, first, ResourceID::InternalIOPolling, 1, false);
    s.step2_sigalrm_ledger();
    assert!(s.sigalrm_loss_recorded(first_pid));

    let (deferred, _, _) = add(&mut s, 200, 200);
    let deferred_pid = DetPid::from_raw(200);
    s.set_sigalrm_handled(deferred_pid, true);
    defer_polling_retry(&mut s, deferred);
    s.fire_alarm(deferred_pid, deferred, Signal::SIGALRM);
    assert!(s.sigalrm_loss_recorded(deferred_pid));
}

/// A polling wait ends when its request is granted: the thread's next wait,
/// a futex wait here, woken before its own grant, is no polling wait.
#[test]
fn a_granted_polling_retry_ends_the_sigalrm_polling_wait() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    defer_polling_retry(&mut s, tid);
    // Its retry is granted and completes; it then waits on a futex, and
    // another process's wake leaves it runnable with the request filed.
    s.clear_nextturn(tid).unwrap();
    file(&mut s, tid, ResourceID::FutexWait, false);

    s.fire_alarm(pid, tid, Signal::SIGALRM);
    s.step2_sigalrm_ledger();
    assert!(s.sigalrm_pending(pid));
    assert!(!s.sigalrm_loss_recorded(pid));
}

/// A `nanosleep` whose deadline saturates files `SleepUntil(INDEFINITE)` too,
/// but without the pause tag: a due entry neither resolves it at admission
/// nor wakes it parked; it is a lost wait, like any other sleep.
#[test]
fn a_saturated_nanosleep_with_a_due_sigalrm_is_a_loss_not_a_pause() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    eligibility(&mut s, tid, false);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    file(
        &mut s,
        tid,
        ResourceID::SleepUntil(LogicalTime::INDEFINITE),
        false,
    );
    assert!(
        !s.admit_pause_with_due_sigalrm(tid),
        "not admitted as a pause"
    );
    assert!(!woken_by_sigalrm(&s, tid));

    s.blocked.timed_waiters.insert(LogicalTime::INDEFINITE, tid);
    s.step2_sigalrm_ledger();
    assert!(!woken_by_sigalrm(&s, tid));
    assert!(s.sigalrm_loss_recorded(pid));
}

/// A polling wait that another signal resolves, through the request
/// substitution of `force_unblock_thread`, is no longer a polling wait: a
/// later due SIGALRM is not a loss.
#[test]
fn a_polling_wait_resolved_by_another_signal_is_not_a_sigalrm_loss() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    defer_polling_retry(&mut s, tid);
    assert!(s.run_queue.remove_tid(tid));
    let mut sigchld = Resources::new(tid);
    sigchld.insert(
        ResourceID::InboundSignal(SigWrapper::from(Signal::SIGCHLD)),
        Permission::W,
    );
    s.force_unblock_thread(tid, sigchld);

    s.fire_alarm(pid, tid, Signal::SIGALRM);
    s.step2_sigalrm_ledger();
    assert!(s.sigalrm_pending(pid));
    assert!(!s.sigalrm_loss_recorded(pid));
}

/// Changing the disposition away from the handler discards a pending entry, as
/// Linux discards a pending signal that becomes ignored; the process's exit
/// forgets its ledger state.
#[test]
fn an_ignored_or_exited_process_drops_its_pending_sigalrm() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    eligibility(&mut s, tid, true);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    assert!(s.sigalrm_pending(pid));
    s.set_sigalrm_handled(pid, false);
    assert!(!s.sigalrm_pending(pid));

    s.set_sigalrm_handled(pid, true);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    s.retire_sigalrm_thread(tid, pid);
    assert!(!s.sigalrm_pending(pid));
    assert!(!s.sigalrm_handled(pid));
}

/// While no process handles SIGALRM, the ledger refuses no control message,
/// from any thread, and records an allowed producer arming.
#[test]
fn an_empty_sigalrm_ledger_refuses_nothing_and_records_a_producer() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    for control in [
        SigalrmControl::SendTo(tid),
        SigalrmControl::SendToUnnamed,
        SigalrmControl::ArmRecurringTimer,
    ] {
        assert!(!s.sigalrm_control(tid, control), "{control:?}");
    }
    assert!(!s.sigalrm_producer_armed());
    assert!(!s.sigalrm_control(tid, SigalrmControl::ArmProducer));
    assert!(s.sigalrm_producer_armed());
}

/// While a process handles SIGALRM: a SIGALRM send that reaches it is refused
/// (its own included), one that reaches another process is not, a send to a
/// process the caller cannot name is, every producer arming is (and is not
/// recorded), and a recurring `ITIMER_REAL` is refused in the handling
/// process only.
#[test]
fn a_sigalrm_handling_process_refuses_sends_to_it_and_every_producer() {
    let mut s = liteinst_scheduler();
    let (handler, _, _) = add(&mut s, 100, 100);
    let (other, _, _) = add(&mut s, 200, 200);
    s.set_sigalrm_handled(DetPid::from_raw(100), true);

    s.set_running_for_test(other);
    assert!(s.sigalrm_control(other, SigalrmControl::SendTo(handler)));
    assert!(!s.sigalrm_control(other, SigalrmControl::SendTo(other)));
    assert!(s.sigalrm_control(other, SigalrmControl::SendToUnnamed));
    assert!(s.sigalrm_control(other, SigalrmControl::ArmProducer));
    assert!(
        !s.sigalrm_producer_armed(),
        "a refused arming is not recorded"
    );
    assert!(!s.sigalrm_control(other, SigalrmControl::ArmRecurringTimer));

    s.set_running_for_test(handler);
    assert!(s.sigalrm_control(handler, SigalrmControl::SendTo(handler)));
    assert!(s.sigalrm_control(handler, SigalrmControl::ArmRecurringTimer));
}

/// The runtime's publications (phase 1 step I3) record its process's
/// disposition and its thread's blocked bit; a handler installation is
/// refused once any producer was armed; a change to `SIG_DFL` is refused, and
/// the pending question answered, by the process's own pending entry.
#[test]
fn the_runtimes_sigalrm_publications_and_questions() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    let (other, _, _) = add(&mut s, 200, 200);
    assert!(!s.sigalrm_control(tid, SigalrmControl::InstallHandler));
    assert!(!s.sigalrm_control(
        tid,
        SigalrmControl::Publish {
            handled: true,
            blocked: true
        }
    ));
    assert!(s.sigalrm_handled(pid));
    s.set_running_for_test(tid);
    assert!(!s.sigalrm_control(tid, SigalrmControl::PendingEntry));
    assert!(!s.sigalrm_control(tid, SigalrmControl::HandlerToDefault));
    // Blocked: an expiry adds an entry that is not due.
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    assert!(s.sigalrm_pending(pid));
    assert!(s.sigalrm_control(tid, SigalrmControl::PendingEntry));
    assert!(s.sigalrm_control(tid, SigalrmControl::HandlerToDefault));
    assert!(!s.sigalrm_control(tid, SigalrmControl::PublishBlocked(false)));
    // Another process has no entry of its own.
    s.set_running_for_test(other);
    assert!(!s.sigalrm_control(other, SigalrmControl::PendingEntry));
    // Unhandled again: the entry is discarded with the disposition.
    s.set_running_for_test(tid);
    assert!(!s.sigalrm_control(
        tid,
        SigalrmControl::Publish {
            handled: false,
            blocked: false
        }
    ));
    assert!(!s.sigalrm_handled(pid));
    assert!(!s.sigalrm_pending(pid));
    // A recurring ITIMER_REAL armed in the caller's process refuses a
    // handler installation; a one-shot one does not, and neither does
    // another process's recurring timer.
    s.set_running_for_test(tid);
    s.register_alarm(
        pid,
        tid,
        LogicalTime::ZERO,
        LogicalTime::from_secs(5),
        LogicalTime::ZERO,
        Signal::SIGALRM,
    );
    assert!(!s.sigalrm_control(tid, SigalrmControl::InstallHandler));
    s.register_alarm(
        pid,
        tid,
        LogicalTime::ZERO,
        LogicalTime::from_secs(5),
        LogicalTime::from_secs(1),
        Signal::SIGALRM,
    );
    assert!(s.sigalrm_control(tid, SigalrmControl::InstallHandler));
    s.set_running_for_test(other);
    assert!(!s.sigalrm_control(other, SigalrmControl::InstallHandler));
    s.set_running_for_test(tid);
    s.register_alarm(
        pid,
        tid,
        LogicalTime::ZERO,
        LogicalTime::ZERO,
        LogicalTime::ZERO,
        Signal::SIGALRM,
    );
    assert!(!s.sigalrm_control(tid, SigalrmControl::InstallHandler));
    // A producer armed while nothing is handled is recorded, and from then
    // on every handler installation is refused.
    assert!(!s.sigalrm_control(other, SigalrmControl::ArmProducer));
    assert!(s.sigalrm_control(tid, SigalrmControl::InstallHandler));
}

/// Signal phase 1, step I4: DueEntry answers whether the caller's process
/// holds an entry due for it, and changes nothing; TakeEntry removes it only
/// while it is pending, handled and due, so a blocked entry stays pending, as
/// Linux leaves it, and a second take finds nothing.
#[test]
fn a_due_sigalrm_entry_is_taken_once_and_only_when_due() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_running_for_test(tid);
    // Nothing handled or pending: nothing is due or taken.
    assert!(!s.sigalrm_control(tid, SigalrmControl::DueEntry));
    assert!(!s.sigalrm_control(tid, SigalrmControl::TakeEntry));
    assert!(!s.sigalrm_control(
        tid,
        SigalrmControl::Publish {
            handled: true,
            blocked: true
        }
    ));
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    assert!(s.sigalrm_pending(pid));
    // Blocked: pending but not due; a take leaves it pending.
    assert!(!s.sigalrm_control(tid, SigalrmControl::DueEntry));
    assert!(!s.sigalrm_control(tid, SigalrmControl::TakeEntry));
    assert!(s.sigalrm_pending(pid));
    // Unblocked: due. The question changes nothing; the take removes it once.
    assert!(!s.sigalrm_control(tid, SigalrmControl::PublishBlocked(false)));
    assert!(s.sigalrm_control(tid, SigalrmControl::DueEntry));
    assert!(s.sigalrm_control(tid, SigalrmControl::DueEntry));
    assert!(s.sigalrm_pending(pid));
    assert!(s.sigalrm_control(tid, SigalrmControl::TakeEntry));
    assert!(!s.sigalrm_pending(pid));
    assert!(!s.sigalrm_control(tid, SigalrmControl::DueEntry));
    assert!(!s.sigalrm_control(tid, SigalrmControl::TakeEntry));
    // A taken entry the runtime cannot deliver is a loss, recorded here.
    assert!(!s.sigalrm_loss_recorded(pid));
    assert!(!s.sigalrm_control(tid, SigalrmControl::DeliveryLost));
    assert!(s.sigalrm_loss_recorded(pid));
}

/// While a process handles SIGALRM, a sender that does not hold the serial
/// grant is refused whatever it asks: its question could race a commit.
#[test]
fn a_sigalrm_control_without_the_serial_grant_is_refused() {
    let mut s = liteinst_scheduler();
    let (handler, _, _) = add(&mut s, 100, 100);
    let (granted, _, _) = add(&mut s, 200, 200);
    let (outside, _, _) = add(&mut s, 300, 300);
    s.set_sigalrm_handled(DetPid::from_raw(100), true);
    s.set_running_for_test(granted);

    assert!(!s.sigalrm_control(granted, SigalrmControl::SendTo(granted)));
    assert!(s.sigalrm_control(outside, SigalrmControl::SendTo(granted)));
    assert!(s.sigalrm_control(outside, SigalrmControl::ArmRecurringTimer));
    let _ = handler;
}

/// With `--scheduler-turn-cost`, a charge refused because it would pass the
/// end of representable virtual time ends a controlled turn at once: no
/// maintenance runs, no due alarm is published to a blocked thread and no
/// signal observation starts, so the terminal report can exit without waiting
/// on any guest (https://github.com/rrnewton/hermit/pull/3885). Thread 100 has
/// just finished an ordinary turn and has its next request filled; thread 101
/// is blocked in nanosleep with a real alarm due. Before the fix the turn
/// published `ObserveSignal` to 101 and waited on its new callback request.
/// Adapted from Codex's review fixture.
#[test]
fn a_refused_turn_charge_returns_before_a_controlled_observation() {
    use futures::FutureExt;

    let config = Config {
        sequentialize_threads: true,
        virtualize_time: true,
        max_timeslice: None,
        epoch: "2554-07-21T23:34:32.709051615Z".parse().unwrap(),
        scheduler_turn_cost: std::num::NonZeroU64::new(1_000_000_000),
        ..Config::default().with_backend(|backend| {
            backend.provides_process_signal_control = true;
            backend.emulates_child_waits = true;
            backend.needs_killed_thread_rpc_cancellation = true;
        })
    };
    config.validate_invariants();
    let mut s = Scheduler::new(&config);
    let backend = Arc::new(Backend::default());
    assert_eq!(
        s.install_signal_control(
            Some(BackendSignalControl {
                process: backend.clone()
            }),
            true,
        )
        .unwrap(),
        BackendSignalControlMode::ToolControlled,
    );
    let (ready, _, _) = add(&mut s, 100, 100);
    let (tid, mm, site) = add(&mut s, 100, 101);

    // One accepted charge leaves 500,615 ns of representable time.
    let mut time = GlobalTime::new(&config);
    let now = time.add_scheduler_time().unwrap();
    assert_eq!(now.as_nanos(), u64::MAX - 500_615);
    s.committed_time = now;
    s.replace_real_timer(ready, ready, now, at(1_000), at(0), Signal::SIGALRM)
        .unwrap();
    let original_response = sleep(&mut s, tid, mm, site, now.as_nanos() + 100_000);
    let original_request = s.next_turns[&tid].req.clone();
    backend.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 101),
    });
    s.next_turns[&ready].req.put(Ok(Resources::new(ready)));
    s.runqueue_push_back(ready);
    // Observed work makes the alarm due; the sleep's deadline is still ahead.
    let current = time.add_extra_time(std::time::Duration::from_nanos(2_000));
    assert!(s.blocked.timed_waiters.thread_deadline(tid).unwrap() > current);
    assert!(s.are_all_quiesced().is_none());
    assert!(s.terminal_clock_exhaustion.is_none());

    let scheduler = Arc::new(Mutex::new(s));
    let global = Arc::new(Mutex::new(time));
    let last = Ok(Resources::new(ready));
    let outcome = do_a_turn_blocking(scheduler.clone(), global.clone(), &last).now_or_never();
    let state = scheduler.lock().unwrap();
    assert!(state.terminal_clock_exhaustion.is_some());
    assert_eq!(
        global.lock().unwrap().as_nanos(),
        current,
        "a refused charge adds nothing"
    );
    assert!(
        matches!(outcome, Some(Err(SkipTurn))),
        "a refused turn must return to the terminal report before starting a callback"
    );
    assert!(
        original_response.try_read().is_none(),
        "no signal observation started"
    );
    assert!(backend.publications.lock().unwrap().is_empty());
    assert!(backend.permits.lock().unwrap().is_empty());
    assert_eq!(state.next_turns[&tid].req, original_request);
    assert_eq!(
        state.next_turns[&tid].protocol.owner,
        NextTurnOwner::Ordinary
    );
    assert!(!state.run_queue.tentative_pop_in_progress());
}

// ---- poll deadlines (https://github.com/rrnewton/hermit/issues/3952) ----

/// Queue `tid` as a backed-off poller through `step4_resource_block`'s real
/// polling branch, retrying a timed wait whose deadline is `deadline`.
fn backed_off_poller(s: &mut Scheduler, tid: DetTid, deadline: Option<LogicalTime>) -> Resources {
    backed_off_poller_after(s, tid, deadline, 1)
}

/// As `backed_off_poller`, after `attempt` retries. From attempt 4 on, the
/// backed-off priority is capped at `LAST_PRIORITY`.
fn backed_off_poller_after(
    s: &mut Scheduler,
    tid: DetTid,
    deadline: Option<LogicalTime>,
    attempt: u32,
) -> Resources {
    let mut request = Resources::new(tid);
    request.insert(ResourceID::InternalIOPolling, Permission::W);
    request.poll_attempt = attempt;
    request.poll_deadline = deadline;
    s.next_turns[&tid].req.put(Ok(request.clone()));
    s.run_queue.push_front(tid, DEFAULT_PRIORITY);
    assert_eq!(s.run_queue.tentative_pop_next(), Some(tid));
    let response = s.next_turns[&tid].resp.clone();
    assert!(s.step4_resource_block(tid, &request, &response).is_err());
    request
}

/// A thread with a filled ordinary request, queued at the back.
fn runnable(s: &mut Scheduler, tid: DetTid) {
    s.next_turns[&tid].req.put(Ok(Resources::new(tid)));
    s.runqueue_push_back(tid);
}

fn next_selection(s: &mut Scheduler) -> Option<DetTid> {
    let next = s.run_queue.tentative_pop_next();
    if next.is_some() {
        s.run_queue.undo_tentative_pop();
    }
    next
}

/// The spinner's turn ends at a preemption and it is requeued at the back.
fn requeue_after_turn(s: &mut Scheduler, tid: DetTid) {
    assert_eq!(s.run_queue.tentative_pop_next(), Some(tid));
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    s.runqueue_push_back(tid);
}

fn uncontrolled() -> Scheduler {
    Scheduler::new(&Config {
        sequentialize_threads: true,
        ..Config::default()
    })
}

/// A backed-off poller loses every selection to a runnable thread until
/// committed time reaches its deadline. Then the ordinary loop's step2 restores
/// its normal priority at the back of that level: the spinner, queued first,
/// runs once more, and once requeued at its preemption it is behind the
/// poller. A second pass at the same committed time changes nothing.
#[test]
fn a_due_poll_deadline_restores_the_backed_off_poller_before_selection() {
    let mut s = uncontrolled();
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let (spinner, _, _) = add(&mut s, 100, 100);
    let (poller, _, _) = add(&mut s, 100, 101);
    s.committed_time = at(50);
    backed_off_poller(&mut s, poller, Some(at(100)));
    // The spinner is requeued after the poller, as at every preemption.
    runnable(&mut s, spinner);
    assert_eq!(s.poll_deadline_of.get(&poller), Some(&at(100)));

    s.step2b_restore_due_pollers();
    assert_eq!(next_selection(&mut s), Some(spinner), "not due yet");

    s.committed_time = at(100);
    s.step2_process_blocked(&global).unwrap();
    assert_eq!(s.run_queue.queued_priority(poller), Some(DEFAULT_PRIORITY));
    assert!(s.poll_deadlines.is_empty() && s.poll_deadline_of.is_empty());
    let after = format!("{:?}", s.run_queue);
    s.step2b_restore_due_pollers();
    assert_eq!(format!("{:?}", s.run_queue), after, "idempotent");
    requeue_after_turn(&mut s, spinner);
    assert_eq!(next_selection(&mut s), Some(poller));
}

/// A due entry that no longer describes a waiting poller changes nothing: the
/// thread started another request, a poll upgrade already restored it, or it
/// was removed. A thread whose suspended entry a parked observation holds keeps
/// its entry until the entry is back, and is restored then.
#[test]
fn stale_poll_deadlines_are_dropped_and_suspended_ones_kept() {
    // The thread's request no longer carries the deadline.
    let mut s = uncontrolled();
    let (spinner, _, _) = add(&mut s, 100, 100);
    let (poller, _, _) = add(&mut s, 100, 101);
    backed_off_poller(&mut s, poller, Some(at(100)));
    runnable(&mut s, spinner);
    let mut newer = Resources::new(poller);
    newer.insert(ResourceID::InternalIOPolling, Permission::W);
    s.next_turns.get_mut(&poller).unwrap().req = Ivar::full(Ok(newer));
    s.committed_time = at(100);
    s.step2b_restore_due_pollers();
    assert!(s.poll_deadline_of.is_empty());
    assert!(
        s.run_queue
            .queued_priority(poller)
            .is_some_and(|priority| priority > DEFAULT_PRIORITY),
        "still backed off"
    );

    // Queued, but a poll upgrade already restored it.
    let mut s = uncontrolled();
    let (spinner, _, _) = add(&mut s, 100, 100);
    let (poller, _, _) = add(&mut s, 100, 101);
    backed_off_poller(&mut s, poller, Some(at(100)));
    runnable(&mut s, spinner);
    assert_eq!(
        s.run_queue.restore_poller_priority(poller),
        PollerRestore::Restored
    );
    let before = format!("{:?}", s.run_queue);
    s.committed_time = at(100);
    s.step2b_restore_due_pollers();
    assert!(s.poll_deadline_of.is_empty());
    assert_eq!(format!("{:?}", s.run_queue), before);

    // Suspended by a parked observation: kept, then restored once it is back.
    let mut s = uncontrolled();
    let (spinner, _, _) = add(&mut s, 100, 100);
    let (poller, _, _) = add(&mut s, 100, 101);
    backed_off_poller(&mut s, poller, Some(at(100)));
    runnable(&mut s, spinner);
    let saved = s.run_queue.suspend(poller, DEFAULT_PRIORITY).unwrap();
    s.committed_time = at(100);
    s.step2b_restore_due_pollers();
    assert_eq!(s.poll_deadline_of.get(&poller), Some(&at(100)), "kept");
    s.run_queue.restore(saved, DEFAULT_PRIORITY);
    s.step2b_restore_due_pollers();
    assert!(s.poll_deadline_of.is_empty());
    assert_eq!(s.run_queue.queued_priority(poller), Some(DEFAULT_PRIORITY));

    // Removed.
    let mut s = uncontrolled();
    let (poller, _, _) = add(&mut s, 100, 100);
    backed_off_poller(&mut s, poller, Some(at(100)));
    s.remove_blocking_entries(&poller);
    assert!(s.poll_deadlines.is_empty() && s.poll_deadline_of.is_empty());
}

/// No deadline is recorded for a poll without a timeout, under record/replay,
/// or under a heuristic that does not select in priority order.
#[test]
fn poll_deadlines_are_recorded_only_where_they_can_matter() {
    let mut s = uncontrolled();
    let (poller, _, _) = add(&mut s, 100, 100);
    backed_off_poller(&mut s, poller, None);
    assert!(s.poll_deadline_of.is_empty(), "no timeout");

    let mut s = uncontrolled();
    s.recordreplay_modes = true;
    let (poller, _, _) = add(&mut s, 100, 100);
    backed_off_poller(&mut s, poller, Some(at(100)));
    assert!(s.poll_deadline_of.is_empty(), "record/replay");

    for heuristic in [SchedHeuristic::Random, SchedHeuristic::StickyRandom] {
        let mut s = Scheduler::new(&Config {
            sequentialize_threads: true,
            sched_heuristic: heuristic,
            ..Config::default()
        });
        let (poller, _, _) = add(&mut s, 100, 100);
        backed_off_poller(&mut s, poller, Some(at(100)));
        assert!(s.poll_deadline_of.is_empty(), "{heuristic:?}");
    }
}

/// KVM's controlled turn loop restores a due poller in maintenance stage 1:
/// the turn's charge makes the deadline due, and by the time the turn selects,
/// the poller is back at its normal priority, behind the runnable thread that
/// was queued first.
#[test]
fn the_controlled_loop_restores_a_due_poller_in_maintenance() {
    use futures::FutureExt;

    let (mut s, _backend) = fixture();
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let start = global.lock().unwrap().as_nanos();
    let (spinner, _, _) = add(&mut s, 100, 100);
    let (poller, _, _) = add(&mut s, 100, 101);
    backed_off_poller(&mut s, poller, Some(start));
    runnable(&mut s, spinner);
    let scheduler = Arc::new(Mutex::new(s));
    let result = do_a_turn_blocking(scheduler.clone(), global, &Err(SkipTurn))
        .now_or_never()
        .expect("nothing to wait for")
        .expect("a runnable thread commits");
    assert_eq!(result.tid, spinner, "queued before the restore");
    let s = scheduler.lock().unwrap();
    assert_eq!(s.controlled_turn_entries, 1);
    assert!(s.poll_deadline_of.is_empty());
    assert_eq!(s.run_queue.queued_priority(poller), Some(DEFAULT_PRIORITY));
}

/// The controlled loop also restores a poller whose deadline is crossed while
/// the turn waits after maintenance stage 1: a real-timer observation parks the
/// turn, global time advances past the deadline, and only the refresh that
/// follows the resume sees it.
#[test]
fn the_controlled_loop_restores_a_poller_whose_deadline_a_refresh_crosses() {
    use futures::FutureExt;

    let (mut s, backend) = fixture();
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let start = global.lock().unwrap().as_nanos();
    let (waiter, mm, site) = add(&mut s, 100, 100);
    let response = sleep(&mut s, waiter, mm, site, (start + at(1_000)).as_nanos());
    s.replace_real_timer(waiter, waiter, start, at(1), at(0), Signal::SIGALRM)
        .unwrap();
    global
        .lock()
        .unwrap()
        .add_extra_time(std::time::Duration::from_nanos(1));
    s.committed_time = start + at(1);
    s.step2b_process_timed(&global);
    backend.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    let (spinner, _, _) = add(&mut s, 100, 101);
    let (poller, _, _) = add(&mut s, 100, 102);
    backed_off_poller(&mut s, poller, Some(start + at(200)));
    runnable(&mut s, spinner);

    let scheduler = Arc::new(Mutex::new(s));
    let last = Err(SkipTurn);
    let mut turn = Box::pin(do_a_turn_blocking(scheduler.clone(), global.clone(), &last));
    assert!(
        turn.as_mut().now_or_never().is_none(),
        "the observation parks the turn"
    );
    let control = selected(&response);
    let ack = Ivar::new();
    {
        let mut s = scheduler.lock().unwrap();
        assert_eq!(s.committed_time, start + at(1));
        assert_eq!(s.poll_deadline_of.get(&poller), Some(&(start + at(200))));
        s.post_control(
            waiter,
            mm,
            ControlIntent::Finish {
                wait: control.continuation,
                lease: control.lease,
                site,
                finish: ObservationFinish::ResumeSameWait,
                ack: ack.clone(),
            },
        )
        .unwrap();
    }
    assert!(turn.as_mut().now_or_never().is_none());
    let ticket = match ack.try_read().unwrap().unwrap() {
        FinishAck::AwaitResume(ticket) => ticket,
        other => panic!("{other:?}"),
    };
    global
        .lock()
        .unwrap()
        .add_extra_time(std::time::Duration::from_nanos(299));
    backend.recipients.lock().unwrap().clear();
    let resumed = Ivar::new();
    scheduler
        .lock()
        .unwrap()
        .post_control(
            waiter,
            mm,
            ControlIntent::Resume {
                ticket,
                site,
                response: resumed.clone(),
            },
        )
        .unwrap();
    let result = turn
        .as_mut()
        .now_or_never()
        .expect("resume restored a filled request");
    let s = scheduler.lock().unwrap();
    assert_eq!(s.committed_time, start + at(300));
    assert_eq!(result.unwrap().tid, spinner, "queued before the restore");
    assert!(s.poll_deadline_of.is_empty());
    assert_eq!(s.run_queue.queued_priority(poller), Some(DEFAULT_PRIORITY));
}

/// A deferred SIGCHLD and a restored poller: the gate decides as it did while
/// the poller was backed off. A poller backed off to `LAST_PRIORITY` counted
/// as a poller, so the signal's thread is re-admitted ahead of it; one backed
/// off to 2000 (one retry) counted as ordinary work, so the signal stays
/// deferred until the poller has run, as on main.
#[test]
fn a_deadline_restored_poller_gates_a_held_sigchld_as_while_backed_off() {
    for (attempt, readmitted) in [(4, true), (1, false)] {
        let mut s = uncontrolled();
        let (forker, _, _) = add(&mut s, 100, 100);
        let (poller, _, _) = add(&mut s, 100, 101);
        backed_off_poller_after(&mut s, poller, Some(at(100)), attempt);
        s.committed_time = at(100);
        s.step2b_restore_due_pollers();
        assert!(s.poll_deadline_of.is_empty(), "restored");
        s.blocked.sigchld_deferred.insert(forker);

        s.step2e_process_signal_deferred();
        assert_eq!(
            s.blocked.sigchld_deferred.is_empty(),
            readmitted,
            "attempt {attempt}"
        );
        let first = if readmitted { forker } else { poller };
        assert_eq!(next_selection(&mut s), Some(first), "attempt {attempt}");
    }
}

/// A finished background call and a restored poller, in both recording
/// modes: step2c harvests the call ahead of a poller that was at
/// `LAST_PRIORITY` while backed off, and keeps its gate closed for one that
/// was at 2000, as on main. Under record/replay a deadline is never recorded,
/// so the restore is applied directly to cover that branch's gate too.
#[test]
fn a_deadline_restored_poller_gates_a_finished_background_call_as_while_backed_off() {
    for recordreplay in [false, true] {
        for (attempt, harvested) in [(4, true), (1, false)] {
            let mut s = uncontrolled();
            s.recordreplay_modes = recordreplay;
            let (blocker, _, _) = add(&mut s, 100, 100);
            let (poller, _, _) = add(&mut s, 100, 101);
            backed_off_poller_after(&mut s, poller, Some(at(100)), attempt);
            assert_eq!(
                s.run_queue.restore_poller_priority(poller),
                PollerRestore::Restored
            );
            let op_id = ExternalOpId::new(blocker, 7);
            s.blocked.external_io_blockers.insert(blocker, op_id);
            let mut continuation = Resources::new(blocker);
            continuation.insert(ResourceID::BlockedExternalContinue(op_id), Permission::RW);
            s.next_turns[&blocker].req.put(Ok(continuation));

            assert!(s.step2c_process_io_blockers().is_ok());
            let label = format!("recordreplay {recordreplay}, attempt {attempt}");
            assert_eq!(
                !s.blocked.external_io_blockers.contains_key(&blocker),
                harvested,
                "{label}"
            );
            let first = if harvested { blocker } else { poller };
            assert_eq!(next_selection(&mut s), Some(first), "{label}");
        }
    }
}

/// Where the child of [`an_expired_vfork_barrier_ends_the_run_through_the_daemon_loop`]
/// learns which case to run.
const VFORK_VALVE_CHILD: &str = "HERMIT_VFORK_VALVE_CONSUMER_CHILD";

/// The child side of [`an_expired_vfork_barrier_ends_the_run_through_the_daemon_loop`]:
/// one runnable thread whose own vfork is still `Pending` (no child has
/// registered), with or without the KVM-style signal control installed, and
/// step2a's clock either past the valve or just started. It runs the real
/// daemon loop. An expired valve must end the process with
/// `HERMIT_POLICY_REFUSAL_EXIT` from inside the loop; a live one must still be
/// waiting when the child stops looking.
fn run_vfork_valve_child(case: &str) {
    let (controlled, expired) = match case {
        "ordinary-expired" => (false, true),
        "controlled-expired" => (true, true),
        "ordinary-live" => (false, false),
        other => panic!("unknown vfork valve case {other}"),
    };
    let (mut s, _backend) = fixture_with_control(controlled);
    let (parent, _, _) = add(&mut s, 100, 100);
    runnable(&mut s, parent);
    s.vfork_barriers.insert(parent, None);
    s.vfork_barrier_wait_since = Some(if expired {
        std::time::Instant::now()
            .checked_sub(Scheduler::VFORK_BARRIER_VALVE + Duration::from_secs(1))
            .expect("the monotonic clock has run longer than the valve")
    } else {
        std::time::Instant::now()
    });
    s.started_up.try_put(());
    let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let sched = Arc::new(Mutex::new(s));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let looped = sched.clone();
    let outcome = runtime.block_on(async move {
        tokio::time::timeout(Duration::from_secs(3), sched_loop(looped, global)).await
    });
    // Only a live wait should reach this point: it must still be waiting, with
    // the barrier in place, nothing committed and no refusal recorded. An
    // expired wait reaches it only if nothing consumed its refusal.
    assert!(
        outcome.is_err(),
        "the daemon loop returned instead of waiting"
    );
    let s = sched.lock().unwrap();
    assert_eq!(s.vfork_barriers.get(&parent), Some(&None));
    assert_eq!(s.turn, 0, "a turn committed behind a pending vfork barrier");
    assert!(s.vfork_barrier_refusal.is_none());
    println!("VFORK_VALVE_CHILD still waiting");
}

/// An expired vfork barrier valve ends the run through the daemon loop's own
/// consumer, on the ordinary turn function and on the signal-control one:
/// exit `HERMIT_POLICY_REFUSAL_EXIT` with the named refusal on stderr. The
/// step2a unit tests stop at the recorded refusal; this is the part they cannot
/// reach, because the consumer ends the process. A live wait, the control, is
/// still waiting 3 s later rather than refused. Each case runs in a child that
/// stops looking after 3 s: without the consumer an expired wait keeps
/// skipping turns, so the child then finds the refusal recorded but never
/// acted on and fails instead of exiting with the refusal
/// (https://github.com/rrnewton/hermit/issues/3999, review of
/// https://github.com/rrnewton/hermit/pull/4008).
#[test]
fn an_expired_vfork_barrier_ends_the_run_through_the_daemon_loop() {
    if let Ok(case) = std::env::var(VFORK_VALVE_CHILD) {
        run_vfork_valve_child(&case);
        return;
    }
    const TEST: &str =
        "scheduler::parked_tests::an_expired_vfork_barrier_ends_the_run_through_the_daemon_loop";
    for (case, refused) in [
        ("ordinary-expired", true),
        ("controlled-expired", true),
        ("ordinary-live", false),
    ] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .env(VFORK_VALVE_CHILD, case)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // A hang guard only: an expired child refuses at once, and every
        // other child stops itself after 3 s.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "{case}: the run did not end within 60 s\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if refused {
            assert_eq!(
                output.status.code(),
                Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
                "{case}: stdout:\n{stdout}\nstderr:\n{stderr}"
            );
            assert!(
                stderr.contains("hermit refused to continue the run: vfork barriers waited")
                    && stderr.contains(
                        "parent 100 (no child registered) Pending, awaiting the child's registration;"
                    ),
                "{case}: stderr:\n{stderr}"
            );
            assert!(
                !stdout.contains("VFORK_VALVE_CHILD still waiting"),
                "{case}"
            );
        } else {
            assert!(
                output.status.success() && stdout.contains("VFORK_VALVE_CHILD still waiting"),
                "{case}: exited with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                output.status
            );
        }
    }
}

/// The valve's bound is the release barrier's, 30 s; a change to either is a
/// deliberate decision, not a drift.
#[test]
fn the_vfork_barrier_valve_is_thirty_seconds() {
    assert_eq!(Scheduler::VFORK_BARRIER_VALVE, Duration::from_secs(30));
    assert_eq!(
        Scheduler::VFORK_BARRIER_VALVE,
        Scheduler::SIGNALED_BACKGROUND_VALVE
    );
}

/// Process 200 (created by 100) had thread 201, which exited, and Linux gave
/// 201 to a thread of process 300, which lives. The thread tree keeps the
/// edge from 200 to 201. Process signal control is installed, as on a
/// sequentialized run of a backend that provides it.
fn reused_member_family() -> (Scheduler, DetTid, MmId, DetTid) {
    let (mut s, _) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.thread_tree.add_child(child, DetTid::from_raw(201), false);
    let _ = add_process_child(&mut s, 100, 300);
    let (reused, _, _) = add(&mut s, 300, 201);
    assert!(
        s.thread_tree.my_thread_group(&child).contains(&reused),
        "the stale edge this test is about must exist"
    );
    (s, child, child_mm, reused)
}

fn terminated(permit: SignalDeliveryPermit, group: bool) -> SignalBoundaryReceipt {
    SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group,
            wait_status: 7 << 8,
        },
    }
}

/// Under process signal control, the raw `exit` of a process's last thread
/// is its process's final exit: it reserves the child exit for the live
/// parent, and its receipt records the final transition. A live thread of
/// another process whose ID the tree lists under the exiting process is not
/// a surviving sibling: Linux's `exit_notify` sees the thread group empty.
#[test]
fn a_controlled_raw_exit_of_a_last_thread_is_final_beside_a_reused_member_id() {
    let (mut s, child, child_mm, reused) = reused_member_family();
    assert_eq!(
        s.reserve_exit_boundary(child, child, child_mm, false),
        Ok(super::signal_control::ExitReserveMode::Controlled)
    );
    let child_key = ProcessGeneration::from_backend(task(200, 200).process);
    assert!(
        s.parked.child_exit_reservations.contains_key(&child_key),
        "the last thread's raw exit must reserve its process's child exit"
    );
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(terminated(permit, false))
        .unwrap();
    assert!(matches!(
        s.parked.child_exit_reservations[&child_key].phase,
        ChildExitReservationPhase::AwaitingPublication {
            status: ExitStatus::Exited(7)
        }
    ));
    assert!(!s.next_turns.contains_key(&child));
    assert!(s.next_turns.contains_key(&reused));
}

/// Under process signal control, a group exit's receipt retires the
/// exiting process's own live threads, as Linux's `zap_other_threads` kills
/// its own thread group, and records its final transition. A live thread of
/// another process whose ID the tree lists under it is neither retired nor
/// a reason to refuse the receipt.
#[test]
fn a_controlled_group_exit_retires_only_its_own_threads_beside_a_reused_member_id() {
    let (mut s, child, child_mm, reused) = reused_member_family();
    let (sibling, _, _) = add(&mut s, 200, 202);
    assert_eq!(
        s.reserve_exit_boundary(child, child, child_mm, true),
        Ok(super::signal_control::ExitReserveMode::Controlled)
    );
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(terminated(permit, true))
        .expect("a legitimate group exit's receipt");
    assert!(!s.next_turns.contains_key(&child));
    assert!(!s.next_turns.contains_key(&sibling));
    assert!(s.next_turns.contains_key(&reused));
    let child_key = ProcessGeneration::from_backend(task(200, 200).process);
    assert!(matches!(
        s.parked.child_exit_reservations[&child_key].phase,
        ChildExitReservationPhase::AwaitingPublication {
            status: ExitStatus::Exited(7)
        }
    ));
}
/// The KVM controlled alarm path: a timer's published signal that an observed
/// `nanosleep`'s own observation removes and catches interrupts the sleep at
/// the timer's due time, however late the observation's finish. The finish
/// notes that due time, taken once, so the sleep reports its remaining time
/// from it: deadline N = 100, due D = 10, with committed time already past N
/// at the finish, the remaining time is N - D = 90. An outcome that does not
/// interrupt the sleep with exactly that removal leaves no note: no eligible
/// signal (the removal was ignored), a caught signal the selection removed
/// another way, a removal some other thread made, and a fatal one. A
/// process-directed POSIX timer's publication is noted the same way
/// (https://github.com/rrnewton/hermit/issues/4931).
#[test]
fn a_caught_timer_interruption_of_an_observed_nanosleep_notes_the_timers_due_time() {
    for case in [
        "caught",
        "caught POSIX timer",
        "no eligible signal",
        "caught another removal",
        "removed by another thread",
        "fatal",
    ] {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        let (other, _, _) = add(&mut s, 100, 101);
        let response = sleep(&mut s, tid, mm, site, 100);
        s.committed_time = at(10);
        let (process, event) = if case == "caught POSIX timer" {
            s.dispatch_timed_signal(
                at(10),
                timed_waiters::SignalTimerId::Posix(DetPid::from_raw(100), 3),
                timed_waiters::SignalTarget::Process(tid),
                timed_waiters::TimerSignal::Send(Signal::SIGUSR1),
                true,
                at(10),
            );
            b.timer_publications.lock().unwrap()[0]
        } else {
            s.replace_real_timer(tid, tid, at(0), at(10), at(0), Signal::SIGALRM)
                .unwrap();
            s.step2b_process_timed(&committed_clock(&s));
            b.publications.lock().unwrap()[0]
        };
        assert!(!s.backend_failed(), "{case}");
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        s.select_parked_alarm().unwrap();
        let observed = selected(&response);
        // Other work carries committed time past the sleep's deadline before
        // the observation finishes.
        s.committed_time = at(150);
        let remover = if case == "removed by another thread" {
            other
        } else {
            tid
        };
        let removal = reverie::SignalDequeue {
            process,
            sequence: 1,
            consumer: reverie::SignalConsumer::ReturnToUser,
            domain: reverie::PendingDomain::Process,
            event,
        };
        s.apply_signal_dequeue(DetPid::from_raw(100), remover, removal)
            .unwrap();
        let selection = reverie::PreparedSignalToken {
            site,
            selection_nonce: 1,
        };
        let finish = match case {
            "no eligible signal" => ObservationFinish::ResumeSameWait,
            "caught another removal" => ObservationFinish::InterruptForCaught {
                selection,
                dequeue: Some(reverie::DequeueId {
                    process,
                    sequence: 2,
                }),
            },
            "fatal" => ObservationFinish::Terminate { selection },
            _ => ObservationFinish::InterruptForCaught {
                selection,
                dequeue: Some(removal.id()),
            },
        };
        let ack = Ivar::new();
        s.post_control(
            tid,
            mm,
            ControlIntent::Finish {
                wait: observed.continuation,
                lease: observed.lease,
                site,
                finish,
                ack: ack.clone(),
            },
        )
        .unwrap();
        s.drain_control_intents();
        assert!(matches!(ack.try_read(), Some(Ok(_))), "{case}");
        assert!(s.parked.observed_timer_dequeues.is_empty(), "{case}");
        if case.starts_with("caught") && case != "caught another removal" {
            let due = s.take_sleep_interruption(tid);
            assert_eq!(due, Some(at(10)), "{case}");
            assert_eq!(
                at(100).as_nanos() - due.unwrap().as_nanos(),
                90,
                "the remaining time"
            );
            assert_eq!(s.take_sleep_interruption(tid), None, "taken once");
        } else {
            assert_eq!(s.take_sleep_interruption(tid), None, "{case}");
        }
    }
}

/// Dispatches process 100's POSIX timer 3 expiration due at `due`, aimed at
/// the process, through the control.
fn dispatch_posix_expiry(s: &mut Scheduler, tid: DetTid, due: u64) {
    s.dispatch_timed_signal(
        at(due),
        timed_waiters::SignalTimerId::Posix(DetPid::from_raw(100), 3),
        timed_waiters::SignalTarget::Process(tid),
        timed_waiters::TimerSignal::Send(Signal::SIGUSR1),
        true,
        at(due),
    );
}

/// A coalesced timer publication installs nothing: Linux keeps the first
/// pending event. A recurring process-directed POSIX timer published at 10 and
/// again, coalesced, at 20 before the signal is dequeued keeps the due time
/// 10, and a caught interruption of an observed `nanosleep` by that event
/// reports its remaining time from 10
/// (https://github.com/rrnewton/hermit/issues/4931).
#[test]
fn a_coalesced_timer_publication_keeps_the_pending_events_first_due_time() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let response = sleep(&mut s, tid, mm, site, 100);
    s.committed_time = at(20);
    dispatch_posix_expiry(&mut s, tid, 10);
    b.coalesce.store(true, std::sync::atomic::Ordering::Relaxed);
    dispatch_posix_expiry(&mut s, tid, 20);
    let rows: Vec<_> = s.parked.timer_publications.iter().map(|p| p.due).collect();
    assert_eq!(rows, vec![at(10)]);
    let (process, event) = b.timer_publications.lock().unwrap()[0];
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    let observed = selected(&response);
    let removal = reverie::SignalDequeue {
        process,
        sequence: 1,
        consumer: reverie::SignalConsumer::ReturnToUser,
        domain: reverie::PendingDomain::Process,
        event,
    };
    s.apply_signal_dequeue(DetPid::from_raw(100), tid, removal)
        .unwrap();
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: observed.continuation,
            lease: observed.lease,
            site,
            finish: ObservationFinish::InterruptForCaught {
                selection: reverie::PreparedSignalToken {
                    site,
                    selection_nonce: 1,
                },
                dequeue: Some(removal.id()),
            },
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert_eq!(ack.try_read(), Some(Ok(FinishAck::Interrupted)));
    assert_eq!(s.take_sleep_interruption(tid), Some(at(10)));
}

/// A coalesced publication whose pending signal no tracked timer publication
/// installed (here an `ITIMER_REAL` expiry whose `SIGALRM` was already pending
/// from elsewhere) creates no entry, so no timer due time is ever attributed
/// to that signal.
#[test]
fn a_coalesced_publication_with_no_tracked_timer_records_nothing() {
    let (mut s, b) = fixture();
    let (tid, _, _) = add(&mut s, 100, 100);
    b.coalesce.store(true, std::sync::atomic::Ordering::Relaxed);
    s.replace_real_timer(tid, tid, at(0), at(10), at(0), Signal::SIGALRM)
        .unwrap();
    s.committed_time = at(10);
    s.step2b_process_timed(&committed_clock(&s));
    assert!(!s.backend_failed());
    assert_eq!(b.publications.lock().unwrap().len(), 1);
    assert!(s.parked.timer_publications.is_empty());
    dispatch_posix_expiry(&mut s, tid, 10);
    assert_eq!(b.timer_publications.lock().unwrap().len(), 1);
    assert!(s.parked.timer_publications.is_empty());
}

/// A publication that installed its event replaces an identical entry, which
/// can only be stale: no identical event was pending.
#[test]
fn a_fresh_timer_publication_replaces_a_stale_identical_entry() {
    let (mut s, _) = fixture();
    let (tid, _, _) = add(&mut s, 100, 100);
    dispatch_posix_expiry(&mut s, tid, 10);
    dispatch_posix_expiry(&mut s, tid, 20);
    let rows: Vec<_> = s.parked.timer_publications.iter().map(|p| p.due).collect();
    assert_eq!(rows, vec![at(20)]);
}

/// A scheduler for a backend that also places orphans where Detcore names
/// and publishes the parent-death signals Detcore decides (KVM's
/// `places_orphans` and `publishable_parent_death_signals`), with its control
/// installed. Installing it opts in to the placement.
fn placing_fixture() -> (Scheduler, Arc<Backend>) {
    let mut s = Scheduler::new(&Config {
        sequentialize_threads: true,
        ..Config::default().with_backend(|backend| {
            backend.provides_process_signal_control = true;
            backend.emulates_child_waits = true;
            backend.needs_killed_thread_rpc_cancellation = true;
            backend.places_orphans = true;
            backend.publishable_parent_death_signals =
                BackendCapabilities::KVM.publishable_parent_death_signals;
        })
    });
    let backend = Arc::new(Backend::default());
    assert_eq!(
        s.install_signal_control(
            Some(BackendSignalControl {
                process: backend.clone(),
            }),
            true,
        )
        .unwrap(),
        BackendSignalControlMode::ToolControlled
    );
    assert!(
        backend
            .orphan_placement_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    (s, backend)
}

/// Ends process `pid`, whose only thread is `pid`, with an exit its grant
/// fenced, through its terminal receipt, then completes what the backend
/// reports after it: a child of a live parent's waitable exit publication, or
/// a root's or a terminal parent's child's retirement.
fn exit_process(s: Scheduler, pid: i32, status: i32) -> Scheduler {
    let mut s = s;
    let (tid, process) = (DetTid::from_raw(pid), DetPid::from_raw(pid));
    let (mm, _) = s.real_timers.task_identity(process, tid).unwrap();
    s.reserve_exit_boundary(tid, process, mm, true).unwrap();
    let permit = s.parked.exit_fences[&tid].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: status << 8,
        },
    })
    .unwrap();
    let key = ProcessGeneration::from_backend(task(pid, pid).process);
    let retirement = reverie::BackendProcessRetirement {
        process: task(pid, pid).process,
        status: ExitStatus::Exited(status),
    };
    if let Some(reservation) = s.parked.child_exit_reservations.get(&key).copied() {
        let event = child_exit_event(
            reservation.parent,
            reservation.child,
            ExitStatus::Exited(status),
            true,
        );
        let scheduler = Arc::new(Mutex::new(s));
        let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
            scheduler.clone(),
            event,
        ));
        assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
        assert!(matches!(
            poll_child_exit(&mut publication),
            Poll::Ready(Ok(()))
        ));
        drop(publication);
        s = Arc::try_unwrap(scheduler).unwrap().into_inner().unwrap();
    }
    if s.parked.child_exit_reservations.contains_key(&key)
        || s.parked.completed_child_exits.contains_key(&key)
        || s.parked.process_retirements.contains_key(&key)
    {
        s.consume_process_retirement(retirement).unwrap();
    }
    s
}

/// On a placing backend (KVM), the exit of a process whose child lives on
/// moves the backend's record of that orphan to the subreaper Detcore chose
/// (`find_child_reaper`), at the exit's terminal receipt, in one call naming
/// the exited process's exact generation; Detcore's own record moves the same
/// way. The backend decides nothing.
#[test]
fn a_kvm_exit_places_its_live_orphan_with_detcores_subreaper() {
    let (mut s, backend) = placing_fixture();
    let _ = add(&mut s, 100, 100);
    s.thread_tree
        .set_child_subreaper(DetPid::from_raw(100), true);
    let _ = add_process_child(&mut s, 100, 200);
    let _ = add_process_child(&mut s, 200, 300);
    let s = exit_process(s, 200, 3);
    assert!(!s.backend_failed());
    assert_eq!(
        *backend.placements.lock().unwrap(),
        vec![(
            task(200, 200).process,
            vec![OrphanPlacement {
                child: task(300, 300).process,
                reaper: OrphanReaper::Process(task(100, 100).process),
            }],
        )]
    );
    assert_eq!(
        s.thread_tree.parent_process(&DetPid::from_raw(300)),
        Some(DetPid::from_raw(100))
    );
}

/// Without a subreaper among its ancestors, an orphan goes to the namespace
/// init, in Detcore's record and, by name, in the backend's.
#[test]
fn a_kvm_orphan_without_a_subreaper_is_placed_with_the_namespace_init() {
    let (mut s, backend) = placing_fixture();
    let _ = add(&mut s, 100, 100);
    let _ = add_process_child(&mut s, 100, 200);
    let _ = add_process_child(&mut s, 200, 300);
    let s = exit_process(s, 200, 3);
    assert!(!s.backend_failed());
    assert_eq!(
        *backend.placements.lock().unwrap(),
        vec![(
            task(200, 200).process,
            vec![OrphanPlacement {
                child: task(300, 300).process,
                reaper: OrphanReaper::NamespaceInit,
            }],
        )]
    );
    assert!(s.thread_tree.is_init_orphan(DetPid::from_raw(300)));
}

/// A zombie orphan (its exit published to its parent, not yet waited for) is
/// placed with the subreaper, which can then wait for it: the backend
/// announces it there, and Detcore's record makes it the subreaper's ready
/// child.
#[test]
fn a_kvm_zombie_orphan_becomes_the_subreapers_waitable_child() {
    let (mut s, backend) = placing_fixture();
    let _ = add(&mut s, 100, 100);
    s.thread_tree
        .set_child_subreaper(DetPid::from_raw(100), true);
    let _ = add_process_child(&mut s, 100, 200);
    let _ = add_process_child(&mut s, 200, 300);
    let s = exit_process(s, 300, 7);
    let mut s = exit_process(s, 200, 3);
    assert!(!s.backend_failed());
    // The childless exit of 300 places nothing; 200's places 300.
    assert_eq!(
        backend
            .placements
            .lock()
            .unwrap()
            .iter()
            .map(|(exited, placements)| (*exited, placements.len()))
            .collect::<Vec<_>>(),
        vec![(task(300, 300).process, 0), (task(200, 200).process, 1)]
    );
    let spec = crate::types::ChildWaitSpec {
        selector: crate::types::ChildWaitSelector::Exact(DetPid::from_raw(300)),
        owner: None,
        exit_class: crate::types::ChildWaitExitClass::Sigchld,
    };
    assert_eq!(
        s.ready_child_wait(DetPid::from_raw(100), spec),
        Some(DetPid::from_raw(300))
    );
    assert!(s.consume_child_wait(DetPid::from_raw(100), DetPid::from_raw(300)));
}

/// A subreaper that reaps its children automatically (it ignores SIGCHLD)
/// takes no zombie: the backend reaps it at once, and Detcore consumes its
/// shadow status, so no wait can find it.
#[test]
fn a_kvm_zombie_orphan_of_an_auto_reaping_subreaper_is_consumed() {
    let (mut s, backend) = placing_fixture();
    let _ = add(&mut s, 100, 100);
    s.thread_tree
        .set_child_subreaper(DetPid::from_raw(100), true);
    let _ = add_process_child(&mut s, 100, 200);
    let _ = add_process_child(&mut s, 200, 300);
    let mut s = exit_process(s, 300, 7);
    s.thread_tree
        .set_sigchld_disposition(DetPid::from_raw(100), true, false);
    backend
        .auto_reaping_reapers
        .lock()
        .unwrap()
        .push(task(100, 100).process);
    let s = exit_process(s, 200, 3);
    assert!(!s.backend_failed());
    assert_eq!(
        backend
            .placements
            .lock()
            .unwrap()
            .iter()
            .map(|(exited, placements)| (*exited, placements.len()))
            .collect::<Vec<_>>(),
        vec![(task(300, 300).process, 0), (task(200, 200).process, 1)]
    );
    assert_eq!(s.thread_tree.parent_process(&DetPid::from_raw(300)), None);
    assert!(
        !s.logically_exited_processes
            .contains(&DetPid::from_raw(300))
    );
}

/// Where Detcore decides no orphan for an exit, a placing backend is still
/// asked, once, to place none: one that holds a child of that exit all the
/// same names it in its receipt, and the run ends as a backend failure at
/// that exit rather than leave the child with a parent Detcore does not
/// know. An exit that leaves no child anywhere commits the empty placement
/// and the run goes on, and a later removal of the same exit does not ask
/// again.
#[test]
fn a_kvm_exit_with_held_children_that_detcore_did_not_place_ends_the_run() {
    for held in [false, true] {
        let (mut s, backend) = placing_fixture();
        let _ = add(&mut s, 100, 100);
        let _ = add_process_child(&mut s, 100, 200);
        if held {
            backend
                .held_orphans
                .lock()
                .unwrap()
                .push(task(300, 300).process);
        }
        let (tid, process) = (DetTid::from_raw(200), DetPid::from_raw(200));
        let (mm, _) = s.real_timers.task_identity(process, tid).unwrap();
        s.reserve_exit_boundary(tid, process, mm, true).unwrap();
        let permit = s.parked.exit_fences[&tid].permit;
        s.consume_signal_boundary(SignalBoundaryReceipt {
            permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group: true,
                wait_status: 3 << 8,
            },
        })
        .unwrap();
        assert_eq!(
            *backend.placements.lock().unwrap(),
            vec![(task(200, 200).process, vec![])],
            "held {held}"
        );
        assert_eq!(s.backend_failed(), held);
        s.reparent_orphans(process);
        assert_eq!(backend.placements.lock().unwrap().len(), 1, "held {held}");
    }
}

/// A placement the backend refuses, and a receipt that disagrees with
/// Detcore's record (a live orphan reported as a zombie), end the run as a
/// backend failure at the exit that made them.
#[test]
fn a_refused_or_disagreeing_kvm_placement_ends_the_run() {
    for misreport in [false, true] {
        let (mut s, backend) = placing_fixture();
        let _ = add(&mut s, 100, 100);
        s.thread_tree
            .set_child_subreaper(DetPid::from_raw(100), true);
        let _ = add_process_child(&mut s, 100, 200);
        let _ = add_process_child(&mut s, 200, 300);
        if misreport {
            backend
                .misreport_placement
                .store(true, std::sync::atomic::Ordering::Relaxed);
        } else {
            backend
                .reject_placement
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let (tid, process) = (DetTid::from_raw(200), DetPid::from_raw(200));
        let (mm, _) = s.real_timers.task_identity(process, tid).unwrap();
        s.reserve_exit_boundary(tid, process, mm, true).unwrap();
        let permit = s.parked.exit_fences[&tid].permit;
        s.consume_signal_boundary(SignalBoundaryReceipt {
            permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group: true,
                wait_status: 3 << 8,
            },
        })
        .unwrap();
        assert_eq!(backend.placements.lock().unwrap().len(), 1, "{misreport}");
        assert!(s.backend_failed(), "misreport {misreport}");
    }
}

/// A parent-death signal whose parent thread leaves at its committed exit is
/// published to the whole child process at the next drain, as Linux's
/// `forget_original_parent` sends it: `SI_USER`, `si_pid` the leaving
/// thread's process, `si_uid` 0. Nothing is sent to a host process, and the
/// signal becomes one the parked-wait selection asks about.
#[test]
fn a_kvm_parent_death_signal_is_published_at_the_drain_after_its_parent_thread_leaves() {
    let (mut s, backend) = placing_fixture();
    let _ = add(&mut s, 100, 100);
    let (creator, creator_mm, _) = add(&mut s, 100, 101);
    let _ = add_process_child(&mut s, 101, 200);
    s.set_parent_death_signal(DetTid::from_raw(200), libc::SIGTERM);
    // The creator's exit was granted (`ResourceID::Exit`).
    s.committed_exits.insert(creator);
    s.reserve_exit_boundary(creator, DetPid::from_raw(100), creator_mm, false)
        .unwrap();
    let permit = s.parked.exit_fences[&creator].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: false,
            wait_status: 0,
        },
    })
    .unwrap();
    assert!(backend.parent_death_publications.lock().unwrap().is_empty());
    s.step2_drain_prefix_for_test();
    assert!(!s.backend_failed());
    assert_eq!(s.host_signal_attempts, 0);
    assert!(
        s.determinism_losses.is_empty(),
        "{:?}",
        s.determinism_losses
    );
    let published = backend.parent_death_publications.lock().unwrap();
    let [(process, event)] = published.as_slice() else {
        panic!("one parent-death publication expected, got {published:?}");
    };
    assert_eq!(*process, task(200, 200).process);
    assert_eq!(event.signal(), libc::SIGTERM);
    assert_eq!(
        event.target(),
        reverie::SignalTarget::Process {
            pid: reverie::Pid::from_raw(200)
        }
    );
    let info = event.siginfo();
    let field = |at: usize| i32::from_ne_bytes(info[at..at + 4].try_into().unwrap());
    assert_eq!(
        (field(0), field(8), field(16), field(20)),
        (libc::SIGTERM, libc::SI_USER, 100, 0)
    );
    assert_eq!(
        s.parked.published_parent_death_signals[&DetPid::from_raw(200)],
        kernel_signal_bit(libc::SIGTERM)
    );
    // The child's new parent-death parent is the process's leader.
    assert_eq!(
        s.thread_tree
            .parent_death_parents
            .get(&DetPid::from_raw(200)),
        Some(&DetTid::from_raw(100))
    );
}

/// A child parked in a wait the selection can interrupt is woken for a
/// parent-death signal Detcore published, through the recipients the backend
/// reports for that signal, as a timer's SIGALRM wakes it; one that was never
/// published wakes nothing.
#[test]
fn a_published_parent_death_signal_wakes_a_parked_recipient() {
    for published in [false, true] {
        let (mut s, backend) = placing_fixture();
        let _ = add(&mut s, 100, 100);
        let (child, child_mm, child_site) = add_process_child(&mut s, 100, 200);
        let response = sleep(&mut s, child, child_mm, child_site, 1_000_000);
        backend.signal_recipient_list.lock().unwrap().push((
            libc::SIGUSR1,
            SignalRecipient {
                task: task(200, 200),
            },
        ));
        if published {
            s.publish_parent_death_signal(super::PendingParentDeathSignal {
                child: DetPid::from_raw(200),
                signal: nix::sys::signal::Signal::SIGUSR1,
                sender: DetPid::from_raw(100),
            });
        }
        s.select_parked_alarm().unwrap();
        assert!(!s.backend_failed());
        if published {
            let control = selected(&response);
            assert_eq!(control.request.dettid, child);
            assert_eq!(s.parked.permits[&child].task, task(200, 200));
        } else {
            assert!(response.try_read().is_none());
            assert!(!s.parked.permits.contains_key(&child));
        }
    }
}

/// A process that a fatal signal ends at a delivery boundary Detcore
/// permitted (no exit fence: the signal's default action) dies where the
/// schedule ordered it, as at an `Exit` grant (`signal_boundary_deaths`): its
/// orphan is placed with the subreaper Detcore chose, its child's
/// parent-death signal is published at the next drain with the dying
/// process as the sender, and neither records a determinism loss.
#[test]
fn a_kvm_death_at_a_permitted_signal_boundary_places_and_publishes_as_an_exit_does() {
    let (mut s, backend) = placing_fixture();
    let _ = add(&mut s, 100, 100);
    s.thread_tree
        .set_child_subreaper(DetPid::from_raw(100), true);
    let _ = add_process_child(&mut s, 100, 200);
    let _ = add_process_child(&mut s, 200, 300);
    s.set_parent_death_signal(DetTid::from_raw(300), libc::SIGTERM);
    // Process 200 holds the serial grant and takes SIGTERM's default action.
    s.parked.running = Some(DetTid::from_raw(200));
    let permit = s
        .authorize_signal_boundary(task(200, 200))
        .unwrap()
        .unwrap();
    let death = s
        .consume_signal_boundary(SignalBoundaryReceipt {
            permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group: true,
                wait_status: libc::SIGTERM,
            },
        })
        .unwrap();
    assert_eq!(
        death.map(|death| death.process),
        Some(DetPid::from_raw(200)),
        "a death by a fatal signal"
    );
    assert!(
        s.signal_boundary_deaths.is_empty(),
        "the mark lasts only while the dying process's threads retire"
    );
    s.step2_drain_prefix_for_test();
    assert!(!s.backend_failed());
    assert!(
        s.determinism_losses.is_empty(),
        "{:?}",
        s.determinism_losses
    );
    assert_eq!(
        *backend.placements.lock().unwrap(),
        vec![(
            task(200, 200).process,
            vec![OrphanPlacement {
                child: task(300, 300).process,
                reaper: OrphanReaper::Process(task(100, 100).process),
            }],
        )]
    );
    let published = backend.parent_death_publications.lock().unwrap();
    let [(process, event)] = published.as_slice() else {
        panic!("one parent-death publication expected, got {published:?}");
    };
    assert_eq!(*process, task(300, 300).process);
    assert_eq!(event.signal(), libc::SIGTERM);
    let info = event.siginfo();
    let field = |at: usize| i32::from_ne_bytes(info[at..at + 4].try_into().unwrap());
    assert_eq!(
        (field(0), field(8), field(16), field(20)),
        (libc::SIGTERM, libc::SI_USER, 200, 0)
    );
}

// SIGSHARE S4: the child-exit SIGCHLD of a controlled exit to a parent with
// one live thread is decided by the scheduler (`controlled_child_exit`).

/// A scheduler with KVM's capabilities, sequentialized threads and the fake
/// control installed.
fn kvm_fixture() -> (Scheduler, Arc<Backend>) {
    let mut s = Scheduler::new(&Config {
        sequentialize_threads: true,
        max_timeslice: None,
        ..Config::default().with_backend(|backend| *backend = reverie::BackendCapabilities::KVM)
    });
    // SIGSHARE S3b: KVM's kernel signal targets are predictable.
    assert!(s.models_signal_targets);
    let backend = Arc::new(Backend::default());
    assert_eq!(
        s.install_signal_control(
            Some(BackendSignalControl {
                process: backend.clone(),
            }),
            true,
        )
        .unwrap(),
        BackendSignalControlMode::ToolControlled
    );
    (s, backend)
}

/// SIGSHARE S3b, fix 2 (`Scheduler::signal_sleep`): where the backend owns
/// signal delivery (KVM's installed control) and the scheduler models signal
/// targets, a filed `pause` or `nanosleep` is not a sleep the drain ends: the
/// backend's observation of the parked call ends it and takes the signal in
/// the same pass (`select_parked_alarm`,
/// `observe_parked_thread_woken_by_its_own_signal`). A drain wake as well
/// took the thread out of the park first, and a process signal published in
/// that pass then went to another thread. Without an installed control, the
/// drain still ends both sleeps.
#[test]
fn a_parked_sleep_is_left_to_the_parked_observation_where_the_backend_owns_delivery() {
    for installed in [true, false] {
        let (mut s, _backend) = if installed {
            kvm_fixture()
        } else {
            fixture_with_control(false)
        };
        s.models_signal_targets = true;
        let (tid, _, _) = add(&mut s, 100, 100);
        for (fyi, deadline, sleep) in [
            (
                crate::resources::PAUSE_FYI,
                LogicalTime::INDEFINITE,
                sleep_signal::SignalSleep::Pause,
            ),
            (
                crate::resources::NANOSLEEP_FYI,
                at(50),
                sleep_signal::SignalSleep::Nanosleep { deadline: at(50) },
            ),
        ] {
            let mut request = Resources::new(tid);
            request.insert(ResourceID::SleepUntil(deadline), Permission::W);
            request.fyi(fyi);
            s.next_turns.get_mut(&tid).unwrap().req = Ivar::full(Ok(request));
            assert_eq!(
                s.signal_sleep(tid),
                (!installed).then_some(sleep),
                "{fyi}, control installed: {installed}"
            );
        }
    }
}

/// SIGSHARE S3b, fix 5 (`Scheduler::sigchld_delivery_placed_by_control`):
/// where the backend's signal control is installed and the scheduler models
/// signal targets, a thread's `SIGCHLD` delivery is granted at its turn even
/// with other work runnable: it takes the signal only with the permit the
/// scheduler reserved, so its turn is fixed by the schedule. Without an
/// installed control the delivery is host-timed there, and is deferred
/// behind the runnable work (`sigchld_deferred`), as on ptrace.
#[test]
fn a_controlled_sigchld_delivery_is_not_deferred_behind_runnable_work() {
    for installed in [true, false] {
        let (mut s, _backend) = fixture_with_control(installed);
        s.models_signal_targets = true;
        let (busy, _, _) = add(&mut s, 100, 100);
        let (signaled, _, _) = add(&mut s, 200, 200);
        s.runqueue_push_back(busy);
        s.runqueue_push_back(signaled);
        assert_eq!(s.run_queue.tentative_pop_tid(signaled), Some(signaled));
        let granted = s
            .block_for_one_resource(
                signaled,
                &ResourceID::InboundSignal(SigWrapper::from(Signal::SIGCHLD)),
                &Permission::RW,
                None,
                None,
                &Ivar::new(),
            )
            .is_ok();
        assert_eq!(granted, installed, "control installed: {installed}");
        assert_eq!(
            s.blocked.sigchld_deferred.contains(&signaled),
            !installed,
            "control installed: {installed}"
        );
    }
}

/// The parent's SIGCHLD state: blocked, explicitly ignored, caught.
fn sigchld_sets(blocked: bool, ignored: bool, caught: bool) -> reverie::SignalSets {
    let bit = kernel_signal_bit(libc::SIGCHLD);
    let set = |on: bool| if on { bit } else { 0 };
    reverie::SignalSets::new(0, 0, set(blocked), set(ignored), set(caught))
}

/// `tid`'s restartable pipe read, filed and queued, as a parked read is.
fn file_restartable_read(s: &mut Scheduler, tid: DetTid) {
    let mut request = Resources::new(tid);
    request.insert(ResourceID::InternalIOPolling, Permission::W);
    request.fyi("read");
    request.set_signal_interrupt_errno(reverie::syscalls::Errno::ERESTARTSYS);
    s.next_turns.get_mut(&tid).unwrap().req = Ivar::full(Ok(request));
    s.runqueue_push_back(tid);
}

/// The committed `Exit` grant of `child`'s only thread (an `exit_group`).
fn grant_group_exit(s: &mut Scheduler, child: DetTid, mm: MmId) {
    assert!(
        s.block_for_one_resource(
            child,
            &ResourceID::Exit {
                group: true,
                process: DetPid::from_raw(child.as_raw()),
                mm,
            },
            &Permission::RW,
            None,
            None,
            &Ivar::new(),
        )
        .is_ok()
    );
}

fn child_exit_timers(s: &Scheduler) -> Vec<(DetPid, DetPid)> {
    s.blocked
        .timed_waiters
        .iter()
        .filter_map(|(_, event)| match event {
            TimedEvent::SignalEvt(
                timed_waiters::SignalTimerId::ChildExit { child, parent },
                _,
                _,
            ) => Some((child, parent)),
            _ => None,
        })
        .collect()
}

/// The parent's request is the scheduler's SIGCHLD wake (`InboundSignal`).
fn woken_for_sigchld(s: &Scheduler, tid: DetTid) -> bool {
    s.next_turns
        .get(&tid)
        .and_then(|turn| turn.req.try_read())
        .and_then(Result::ok)
        .is_some_and(|request| {
            request
                .resources
                .contains_key(&ResourceID::InboundSignal(SigWrapper(libc::SIGCHLD)))
        })
}

/// Mutations "the controlled grant decides nothing" (S2b's behaviour: no
/// timer for any row), "a multithreaded parent decided too", "the backend's
/// state not read" (every row Undecided, so the ignored and default rows get
/// a timer) and "classified outside the covered states". The parent (100)
/// waits in a restartable read unless the row says otherwise; its child (200)
/// exits.
#[test]
fn a_controlled_exit_to_a_one_thread_parent_is_classified_at_its_grant() {
    use ChildExitNotification as N;
    for (name, sets, parked, sibling, decision) in [
        (
            "caught",
            sigchld_sets(false, false, true),
            true,
            false,
            Some(N::Deliver),
        ),
        (
            "blocked",
            sigchld_sets(true, false, false),
            true,
            false,
            Some(N::Deliver),
        ),
        (
            "ignored",
            sigchld_sets(false, true, true),
            true,
            false,
            Some(N::None),
        ),
        (
            "default",
            sigchld_sets(false, false, false),
            true,
            false,
            Some(N::Discard),
        ),
        (
            "not parked",
            sigchld_sets(false, true, false),
            false,
            false,
            Some(N::Undecided),
        ),
        // SIGSHARE S3b: KVM models signal targets, so a parent with another
        // live thread is decided too, as on ptrace.
        (
            "two threads",
            sigchld_sets(false, false, true),
            true,
            true,
            Some(N::Deliver),
        ),
    ] {
        let (mut s, backend) = kvm_fixture();
        let (parent, _, _) = add(&mut s, 100, 100);
        if sibling {
            let _ = add(&mut s, 100, 101);
        }
        let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
        backend.signal_sets.lock().unwrap().insert(100, sets);
        if parked {
            file_restartable_read(&mut s, parent);
        }
        grant_group_exit(&mut s, child, child_mm);
        let child_process = DetPid::from_raw(200);
        assert_eq!(
            s.controlled_child_exits_decision(child_process),
            decision,
            "{name}"
        );
        let timer = matches!(decision, Some(N::Deliver | N::Undecided));
        assert_eq!(
            child_exit_timers(&s),
            if timer {
                vec![(child_process, DetPid::from_raw(100))]
            } else {
                vec![]
            },
            "{name}"
        );
        // No hold waits for a kernel publication report on this backend.
        assert!(s.child_exit_publications_pending.is_empty(), "{name}");
    }
    // Where the scheduler does not model signal targets (before S3b), a
    // parent with another live thread keeps the backend's decision.
    let (mut s, backend) = kvm_fixture();
    s.models_signal_targets = false;
    let (parent, _, _) = add(&mut s, 100, 100);
    let _ = add(&mut s, 100, 101);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    backend
        .signal_sets
        .lock()
        .unwrap()
        .insert(100, sigchld_sets(false, false, true));
    file_restartable_read(&mut s, parent);
    grant_group_exit(&mut s, child, child_mm);
    assert_eq!(
        s.controlled_child_exits_decision(DetPid::from_raw(200)),
        None
    );
    assert!(child_exit_timers(&s).is_empty());
}

/// The cross-check table (design S4 and S0 correction 2): the only allowed
/// disagreement is the default-ignored SIGCHLD the backend queues (G8).
/// Mutation "every effect accepted" and "Discard against Queued refused".
#[test]
fn a_publication_effect_is_checked_against_the_grant_decision() {
    use ChildExitNotification as N;
    use ChildExitPublicationEffect as E;

    use super::controlled_child_exit::child_exit_effect_mismatch as mismatch;
    let all = [
        E::Queued,
        E::Coalesced,
        E::SuppressedExplicitIgnore,
        E::DiscardedByDispositionChange,
    ];
    for effect in all {
        for waitable in [false, true] {
            let ok = |decision| mismatch(decision, effect, waitable).is_none();
            assert_eq!(
                ok(N::None),
                effect == E::SuppressedExplicitIgnore && !waitable,
                "None {effect:?} {waitable}"
            );
            assert_eq!(
                ok(N::Discard),
                matches!(effect, E::Queued | E::Coalesced),
                "Discard {effect:?} {waitable}"
            );
            assert_eq!(
                ok(N::Deliver),
                matches!(effect, E::Queued | E::Coalesced),
                "Deliver {effect:?} {waitable}"
            );
            assert!(ok(N::Undecided), "Undecided {effect:?} {waitable}");
        }
    }
}

/// Run a controlled child exit from its grant to its `ChildExit` timer's pop:
/// the terminal receipt, the two-poll publication with `effect`, then one
/// timed pass. Returns the scheduler.
fn controlled_child_exit_to_pop(
    sets: reverie::SignalSets,
    effect: ChildExitPublicationEffect,
    waitable: bool,
) -> (Scheduler, Arc<Backend>, DetTid) {
    controlled_child_exit_to_pop_from(sets, effect, waitable, file_restartable_read)
}

/// `tid`'s `nanosleep` until one second, filed and parked at its deadline, as
/// a parked sleep is: a wait the grant does not classify (Undecided).
fn park_nanosleep(s: &mut Scheduler, tid: DetTid) {
    let deadline = LogicalTime::from_nanos(1_000_000_000);
    let mut request = Resources::new(tid);
    request.insert(ResourceID::SleepUntil(deadline), Permission::RW);
    request.fyi(crate::resources::NANOSLEEP_FYI);
    s.next_turns.get_mut(&tid).unwrap().req = Ivar::full(Ok(request));
    s.blocked.timed_waiters.insert(deadline, tid);
}

/// [`controlled_child_exit_to_pop`] with the parent's wait filed by `file`.
fn controlled_child_exit_to_pop_from(
    sets: reverie::SignalSets,
    effect: ChildExitPublicationEffect,
    waitable: bool,
    file: fn(&mut Scheduler, DetTid),
) -> (Scheduler, Arc<Backend>, DetTid) {
    let (mut s, backend) = kvm_fixture();
    let (parent, _, _) = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    backend.signal_sets.lock().unwrap().insert(100, sets);
    *backend.child_effect.lock().unwrap() = Some(effect);
    file(&mut s, parent);
    grant_group_exit(&mut s, child, child_mm);
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 7 << 8,
        },
    })
    .unwrap();
    assert!(s.control_barrier());
    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(7),
        waitable,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Ok(()))
    ));
    drop(publication);
    let mut s = Arc::try_unwrap(scheduler).unwrap().into_inner().unwrap();
    assert!(!s.control_barrier());
    // The next pass's committed time is past the timer's deadline, one
    // nanosecond after the grant's.
    s.committed_time = s.committed_time + LogicalTime::from_nanos(1);
    let clock = committed_clock(&s);
    s.step2b_process_timed(&clock);
    (s, backend, parent)
}

/// Revastra2's round-2 review of SIGSHARE S3b, F1: a child-exit SIGCHLD that
/// KVM's publication sends nothing for moves no `curr_target`. The parent's
/// leader (the creator) blocks SIGCHLD and sleeps in a wait the grant leaves
/// Undecided (a `nanosleep`, as a futex wait is), and its second thread 101
/// leaves SIGCHLD unblocked, so the walk from the leader chooses 101. Queued
/// (caught), the copy reaches `complete_signal` and `curr_target` moves to
/// 101; suppressed (explicit SIG_IGN: `do_notify_parent` sends nothing,
/// whatever the masks) or coalesced, it reaches none and `curr_target`
/// stays. Before, every effect committed the walk. Mutation "commit for
/// every landed publication". Left SIG_DFL (ignored by default), the copy
/// the blocked creator has queued still reaches `complete_signal`, so
/// `curr_target` moves to 101, which discards it as it dequeues it: the
/// receiver's discard is not one at generation by the named thread (the
/// round-3 reviews, P1; `default-child-curr`). Mutation "a receiver's
/// discard marks no walk".
#[test]
fn a_child_exit_kvm_published_no_copy_for_moves_no_curr_target() {
    let bit = kernel_signal_bit(libc::SIGCHLD);
    let caught = reverie::SignalSets::new(0, 0, bit, 0, bit);
    let ignored = reverie::SignalSets::new(0, 0, bit, bit, 0);
    let defaulted = reverie::SignalSets::new(0, 0, bit, 0, 0);
    for (name, leader, worker_sets, effect, waitable, moved) in [
        (
            "queued",
            caught,
            sigchld_sets(false, false, true),
            ChildExitPublicationEffect::Queued,
            true,
            true,
        ),
        (
            "coalesced",
            caught,
            sigchld_sets(false, false, true),
            ChildExitPublicationEffect::Coalesced,
            true,
            false,
        ),
        (
            "suppressed",
            ignored,
            sigchld_sets(false, true, false),
            ChildExitPublicationEffect::SuppressedExplicitIgnore,
            false,
            false,
        ),
        (
            "default, discarded by its receiver",
            defaulted,
            sigchld_sets(false, false, false),
            ChildExitPublicationEffect::Queued,
            true,
            true,
        ),
    ] {
        let (mut s, backend) = kvm_fixture();
        let (parent, _, _) = add(&mut s, 100, 100);
        let (worker, _, _) = add(&mut s, 100, 101);
        let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
        backend.signal_sets.lock().unwrap().insert(100, leader);
        backend.signal_sets.lock().unwrap().insert(101, worker_sets);
        *backend.child_effect.lock().unwrap() = Some(effect);
        park_nanosleep(&mut s, parent);
        file_restartable_read(&mut s, worker);
        grant_group_exit(&mut s, child, child_mm);
        let permit = s.parked.exit_fences[&child].permit;
        s.consume_signal_boundary(SignalBoundaryReceipt {
            permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group: true,
                wait_status: 7 << 8,
            },
        })
        .unwrap();
        let event = child_exit_event(
            task(100, 100).process,
            task(200, 200).process,
            ExitStatus::Exited(7),
            waitable,
        );
        let scheduler = Arc::new(Mutex::new(s));
        let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
            scheduler.clone(),
            event,
        ));
        assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
        assert!(matches!(
            poll_child_exit(&mut publication),
            Poll::Ready(Ok(()))
        ));
        drop(publication);
        let mut s = Arc::try_unwrap(scheduler).unwrap().into_inner().unwrap();
        s.committed_time = s.committed_time + LogicalTime::from_nanos(1);
        let clock = committed_clock(&s);
        s.step2b_process_timed(&clock);
        assert!(child_exit_timers(&s).is_empty(), "{name}: the timer popped");
        assert_eq!(s.controlled_child_exits.refusal_for_test(), None, "{name}");
        assert_eq!(
            s.signal_curr_targets.get(&DetPid::from_raw(100)),
            moved.then_some(&worker),
            "{name}"
        );
    }
}

/// Mutations "Queued wakes nobody" (design G5: the parent's read is never
/// woken), "Coalesced wakes" (Linux does not queue a standard signal already
/// pending, so no thread is woken) and "the publication's effect not kept for
/// the timer" (the sink then refuses the send).
#[test]
fn a_controlled_child_exit_timer_acts_on_the_publication_effect() {
    // Caught, queued: the parked read is woken as on ptrace.
    let (s, _, parent) = controlled_child_exit_to_pop(
        sigchld_sets(false, false, true),
        ChildExitPublicationEffect::Queued,
        true,
    );
    assert!(child_exit_timers(&s).is_empty());
    assert!(woken_for_sigchld(&s, parent));
    assert_eq!(s.controlled_child_exits.refusal_for_test(), None);

    // Caught, coalesced into a pending SIGCHLD: no thread is woken.
    let (s, _, parent) = controlled_child_exit_to_pop(
        sigchld_sets(false, false, true),
        ChildExitPublicationEffect::Coalesced,
        true,
    );
    assert!(child_exit_timers(&s).is_empty());
    assert!(!woken_for_sigchld(&s, parent));
    assert_eq!(s.controlled_child_exits.refusal_for_test(), None);
}

/// A queued SIGCHLD ends a parked `nanosleep` only where Linux's would (the
/// revastra2 review of https://github.com/rrnewton/hermit/pull/5192, F1): a
/// blocked copy stays pending and wakes nothing; a default-ignored one is
/// discarded at generation, and an explicit SIG_IGN is suppressed at
/// publication, both woken as a discarded send, which commits no
/// `InboundSignal`; a caught one interrupts the sleep. Mutation "Queued wakes
/// whatever the receiver's sets say": the blocked and default rows wake.
#[test]
fn a_queued_child_exit_sigchld_ends_a_parked_sleep_only_where_it_interrupts() {
    for (name, sets, effect, waitable, woken) in [
        (
            "blocked",
            sigchld_sets(true, false, false),
            ChildExitPublicationEffect::Queued,
            true,
            false,
        ),
        (
            "default",
            sigchld_sets(false, false, false),
            ChildExitPublicationEffect::Queued,
            true,
            false,
        ),
        (
            "blocked and caught",
            sigchld_sets(true, false, true),
            ChildExitPublicationEffect::Queued,
            true,
            false,
        ),
        (
            "ignored",
            sigchld_sets(false, true, false),
            ChildExitPublicationEffect::SuppressedExplicitIgnore,
            false,
            false,
        ),
        (
            "caught",
            sigchld_sets(false, false, true),
            ChildExitPublicationEffect::Queued,
            true,
            true,
        ),
    ] {
        let (s, _, parent) =
            controlled_child_exit_to_pop_from(sets, effect, waitable, park_nanosleep);
        assert_eq!(
            s.controlled_child_exits_decision(DetPid::from_raw(200)),
            None,
            "{name}: the decision is consumed at publication"
        );
        assert!(child_exit_timers(&s).is_empty(), "{name}");
        assert_eq!(woken_for_sigchld(&s, parent), woken, "{name}");
        assert_eq!(s.controlled_child_exits.refusal_for_test(), None, "{name}");
    }
}

/// Mutation "a mismatch passes silently": a parent that explicitly ignores
/// SIGCHLD decides `None`, and a backend that queued it anyway ends the run
/// by name. The same with a waitable child under SIG_IGN.
#[test]
fn a_publication_that_contradicts_the_decision_is_refused_by_name() {
    for (effect, waitable) in [
        (ChildExitPublicationEffect::Queued, false),
        (ChildExitPublicationEffect::SuppressedExplicitIgnore, true),
    ] {
        let (s, _, parent) =
            controlled_child_exit_to_pop(sigchld_sets(false, true, false), effect, waitable);
        let refusal = s.controlled_child_exits.refusal_for_test().unwrap();
        assert!(
            refusal.contains("sigchld-decision-mismatch"),
            "{effect:?}: {refusal}"
        );
        assert!(!woken_for_sigchld(&s, parent));
    }
    // The agreeing case: suppressed and reaped. No refusal, no timer, no wake.
    let (s, _, parent) = controlled_child_exit_to_pop(
        sigchld_sets(false, true, false),
        ChildExitPublicationEffect::SuppressedExplicitIgnore,
        false,
    );
    assert_eq!(s.controlled_child_exits.refusal_for_test(), None);
    assert!(child_exit_timers(&s).is_empty());
    assert!(!woken_for_sigchld(&s, parent));
}

/// Mutation "the send latch missing": under a signal control, a scheduler
/// send that no child-exit publication stands for reaches no host process;
/// it is refused by name.
#[test]
fn a_scheduler_send_under_a_signal_control_without_a_publication_is_refused() {
    let (mut s, _) = kvm_fixture();
    let (parent, _, _) = add(&mut s, 100, 100);
    file_restartable_read(&mut s, parent);
    s.send_guest_signal(
        parent,
        Signal::SIGUSR1,
        None,
        None,
        parent,
        discarded_at_send::SendFate::Classify(None),
    );
    assert_eq!(s.guest_send_landed, Some(false));
    assert!(!woken_for_sigchld(&s, parent));
    assert!(
        s.controlled_child_exits
            .refusal_for_test()
            .is_some_and(|refusal| refusal.contains("sigchld-decision-mismatch"))
    );
}

/// SIGALRM's bit in a signal set.
const SIGALRM_BIT: u64 = 1 << (libc::SIGALRM - 1);

/// Reports `sets` for task (`pid`, `tid`) through the fake control's
/// `signal_sets`.
fn report_signal_sets(b: &Backend, pid: i32, tid: i32, sets: reverie::SignalSets) {
    b.task_signal_sets
        .lock()
        .unwrap()
        .push((task(pid, tid), sets));
}

/// Files an ordinary request for `tid`, one that has nothing to do with
/// signals.
fn file_ordinary(s: &mut Scheduler, tid: DetTid) {
    s.next_turns[&tid].req.put(Ok(Resources::new(tid)));
}

/// Files `tid`'s polled `pselect6` whose temporary mask is `mask`, as
/// `handle_internal_pselect6` files it where the backend controls the
/// guest's signals: the request carries the mask, and the EINTR marker.
fn file_masked_pselect6(s: &mut Scheduler, tid: DetTid, mask: u64) {
    let mut r = Resources::new(tid);
    r.insert(ResourceID::InternalIOPolling, Permission::W);
    r.fyi("pselect6");
    r.blocked_signal_mask = Some(mask);
    r.set_signal_interrupt_errno(Errno::EINTR);
    s.next_turns[&tid].req.put(Ok(r));
}

/// Files `tid`'s polled masked `ppoll` whose temporary mask is `mask`, as
/// `Detcore::backend_masked_ppoll_wait` files it where the backend serves the
/// wait (SIGSHARE S2b): the request carries the mask, and the EINTR marker.
fn file_masked_ppoll(s: &mut Scheduler, tid: DetTid, mask: u64) {
    let mut r = Resources::new(tid);
    r.insert(ResourceID::InternalIOPolling, Permission::W);
    r.fyi("ppoll");
    r.blocked_signal_mask = Some(mask);
    r.set_signal_interrupt_errno(Errno::EINTR);
    s.next_turns[&tid].req.put(Ok(r));
}

/// Parks `tid` in an `rt_sigsuspend` under temporary mask `mask`, as
/// `Detcore::handle_rt_sigsuspend` files it where the backend serves the wait
/// (SIGSHARE S2b, `Scheduler::backend_suspend_mask`): an indefinite sleep
/// tagged `RT_SIGSUSPEND_FYI` that carries the mask, held in the timed waiters.
fn file_backend_suspend(s: &mut Scheduler, tid: DetTid, mask: u64) {
    let mut r = Resources::new(tid);
    r.insert(
        ResourceID::SleepUntil(LogicalTime::INDEFINITE),
        Permission::W,
    );
    r.fyi(crate::resources::RT_SIGSUSPEND_FYI);
    r.blocked_signal_mask = Some(mask);
    s.next_turns[&tid].req.put(Ok(r));
    s.blocked.timed_waiters.insert(LogicalTime::INDEFINITE, tid);
    assert_eq!(s.backend_suspend_mask(tid), Some(mask));
}

/// Files `tid`'s retry of a call polled by `retry_nonblocking_syscall_helper`
/// (`fyi`, with the restart marker `marker`, if any) and queues it as a
/// poller, as step 4 defers a retry that is not ready.
fn file_poller(s: &mut Scheduler, tid: DetTid, fyi: &str, marker: Option<Errno>) {
    let mut r = Resources::new(tid);
    r.insert(ResourceID::InternalIOPolling, Permission::W);
    r.fyi(fyi);
    r.poll_attempt = 1;
    if let Some(errno) = marker {
        r.set_signal_interrupt_errno(errno);
    }
    s.next_turns[&tid].req.put(Ok(r));
    s.run_queue.push_poller(tid, DEFAULT_PRIORITY, 1);
}

/// Files `tid`'s `exit` (not `exit_group`) from process 100.
fn file_exit(s: &mut Scheduler, tid: DetTid, mm: MmId) {
    let mut r = Resources::new(tid);
    r.insert(
        ResourceID::Exit {
            group: false,
            process: DetPid::from_raw(100),
            mm,
        },
        Permission::W,
    );
    s.next_turns[&tid].req.put(Ok(r));
}

/// https://github.com/rrnewton/hermit/issues/4929 (E): a controlled
/// `ITIMER_REAL` expiry is decided at generation, as Linux decides it.
/// `it_real_fn` sends SIGALRM to the leader, and `prepare_signal` drops it
/// (`sig_ignored`) when the leader's mask leaves it unblocked and its action
/// is SIG_IGN: nothing is queued, so nothing is published, and the timer
/// stays stopped (`HRTIMER_NORESTART`). The mask is the one Linux's `blocked`
/// holds: a masked wait's temporary mask, from its filed request, where the
/// control's own set holds the thread's own mask. A blocked signal, a handled
/// or a default one, and a leader whose state the scheduler does not read
/// (its exit filed, or nothing reported) are published, as before.
#[test]
fn a_controlled_expiry_the_leader_ignores_is_discarded_at_generation() {
    #[derive(Clone, Copy)]
    enum Filed {
        Ordinary,
        Masked(u64),
        MaskedPpoll(u64),
        Exit,
    }
    // (case, the leader's (blocked, ignored, caught) if reported, its filed
    // request, whether the expiry is discarded)
    let cases = [
        (
            "SIG_IGN, unblocked",
            Some((0, SIGALRM_BIT, 0)),
            Filed::Ordinary,
            true,
        ),
        (
            "SIG_IGN, blocked",
            Some((SIGALRM_BIT, SIGALRM_BIT, 0)),
            Filed::Ordinary,
            false,
        ),
        (
            "handled, unblocked",
            Some((0, 0, SIGALRM_BIT)),
            Filed::Ordinary,
            false,
        ),
        (
            "SIG_DFL, unblocked",
            Some((0, 0, 0)),
            Filed::Ordinary,
            false,
        ),
        (
            "SIG_IGN, unblocked, a wait's temporary mask blocks it",
            Some((0, SIGALRM_BIT, 0)),
            Filed::Masked(SIGALRM_BIT),
            false,
        ),
        (
            "SIG_IGN, blocked, a wait's temporary mask admits it",
            Some((SIGALRM_BIT, SIGALRM_BIT, 0)),
            Filed::Masked(0),
            true,
        ),
        (
            "SIG_IGN, unblocked, a masked ppoll's temporary mask blocks it",
            Some((0, SIGALRM_BIT, 0)),
            Filed::MaskedPpoll(SIGALRM_BIT),
            false,
        ),
        (
            "SIG_IGN, blocked, a masked ppoll's temporary mask admits it",
            Some((SIGALRM_BIT, SIGALRM_BIT, 0)),
            Filed::MaskedPpoll(0),
            true,
        ),
        (
            "SIG_IGN, unblocked, its exit filed",
            Some((0, SIGALRM_BIT, 0)),
            Filed::Exit,
            false,
        ),
        ("nothing reported", None, Filed::Ordinary, false),
    ];
    for (case, sets, filed, discarded) in cases {
        let (mut s, b) = fixture();
        let (leader, mm, _) = add(&mut s, 100, 100);
        if let Some((blocked, ignored, caught)) = sets {
            report_signal_sets(
                &b,
                100,
                100,
                reverie::SignalSets::new(0, 0, blocked, ignored, caught),
            );
        }
        match filed {
            Filed::Ordinary => file_ordinary(&mut s, leader),
            Filed::Masked(mask) => file_masked_pselect6(&mut s, leader, mask),
            Filed::MaskedPpoll(mask) => file_masked_ppoll(&mut s, leader, mask),
            Filed::Exit => file_exit(&mut s, leader, mm),
        }
        s.replace_real_timer(
            leader,
            leader,
            at(0),
            at(100_000),
            at(100_000),
            Signal::SIGALRM,
        )
        .unwrap();
        s.committed_time = at(100_000);
        assert!(s.step2b_process_timed(&committed_clock(&s)), "{case}");
        assert!(!s.backend_failed(), "{case}");
        assert_eq!(
            b.publications.lock().unwrap().len(),
            usize::from(!discarded),
            "{case}"
        );
        // Stopped either way: published, the timer waits for its copy's
        // dequeue; discarded, for any shared-queue SIGALRM's.
        assert_eq!(
            s.real_timers
                .snapshot(leader, at(100_001))
                .unwrap()
                .remaining,
            at(0),
            "{case}"
        );
        assert!(s.blocked.timed_waiters.is_empty(), "{case}");
    }
}

/// https://github.com/rrnewton/hermit/issues/4929 (E): a timer whose expiry
/// was discarded at generation stays stopped until a SIGALRM is taken from
/// the process's shared queue, whoever sent it (`dequeue_signal` restarts
/// it for any such copy, v6.17 kernel/signal.c:632-636), here one the
/// guest's own `kill` sent; it then restarts on its grid. No reconcile point
/// restarts it before that dequeue.
#[test]
fn a_discarded_expiry_waits_for_a_shared_queue_dequeue_to_restart() {
    let (mut s, b) = fixture();
    let (leader, _, _) = add(&mut s, 100, 100);
    report_signal_sets(
        &b,
        100,
        100,
        reverie::SignalSets::new(0, 0, 0, SIGALRM_BIT, 0),
    );
    file_ordinary(&mut s, leader);
    s.replace_real_timer(
        leader,
        leader,
        at(0),
        at(100_000),
        at(100_000),
        Signal::SIGALRM,
    )
    .unwrap();
    s.committed_time = at(100_000);
    assert!(s.step2b_process_timed(&committed_clock(&s)));
    assert!(b.publications.lock().unwrap().is_empty());
    s.committed_time = at(250_000);
    s.reconcile_inactive_alarms(s.committed_time);
    assert_eq!(s.reconcile_alarm(leader, s.committed_time), None);
    assert!(s.blocked.timed_waiters.is_empty());
    assert_eq!(
        s.apply_signal_dequeue(leader, leader, kill_alarm_dequeue_of(100, 1)),
        Ok(super::real_timer::DequeueAck::Applied { sequence: 1 })
    );
    s.committed_time = at(350_000);
    s.reconcile_inactive_alarms(s.committed_time);
    assert_eq!(s.blocked.timed_waiters.next_deadline(), Some(at(400_000)));
    assert_eq!(
        s.real_timers
            .snapshot(leader, at(350_000))
            .unwrap()
            .remaining,
        at(50_000)
    );
    assert!(b.publications.lock().unwrap().is_empty());
    assert!(!s.backend_failed());
}

/// A thread of process 100 in one of the states that the choice of the
/// thread to take a published SIGALRM distinguishes
/// (`Scheduler::alarm_receiver`). Every thread's process has a SIGALRM
/// handler.
#[derive(Clone, Copy, Debug)]
enum AlarmThread {
    /// Queued to run with an ordinary request; SIGALRM unblocked.
    Runnable,
    /// Parked in `pause` at a controlled site; SIGALRM unblocked.
    Pause,
    /// Queued to run; its mask blocks SIGALRM.
    Blocks,
    /// Queued to run; SIGALRM unblocked, but SIGUSR1, which it does not
    /// block either, is pending for it alone (`task_sigpending`).
    Busy,
    /// As `Busy`, but its process ignores SIGUSR1: a copy Linux would have
    /// dropped as it was sent, which does not make it busy.
    IgnoredPending,
    /// Parked in `pause` at a controlled site, SIGALRM unblocked, with
    /// SIGUSR1, which it does not block either, pending for it alone: a state
    /// Linux never holds, since the SIGUSR1 woke it; it is observed before
    /// the receiver walk runs.
    BusyPause,
    /// Queued to run in a masked `pselect6` whose temporary mask blocks
    /// SIGALRM, which its own mask does not.
    MaskedWait,
    /// Queued to run in a masked `pselect6` whose temporary mask admits
    /// SIGALRM, which its own mask blocks.
    MaskedWaitAdmits,
    /// Polling in `rt_sigtimedwait` for SIGALRM, which its own mask blocks.
    Sigtimedwait,
    /// Queued to run with its exit filed; SIGALRM unblocked.
    Exiting,
    /// Parked in a futex wait, which the observation cannot interrupt;
    /// SIGALRM unblocked.
    FutexWait,
    /// A vfork parent waiting for its child; SIGALRM unblocked.
    VforkParent,
    /// A vfork parent waiting for its child outside the schedule
    /// (`external_io_blockers`), SIGALRM unblocked, with SIGUSR1, which it
    /// does not block either, pending for it alone.
    BusyVforkParent,
    /// Polling `poll`, whose probe never takes a pending signal, with the
    /// EINTR marker its request carries for the idle-poller diagnosis
    /// (`Detcore::handle_internal_poll_until`); SIGALRM unblocked.
    PollPoller,
    /// Polling an unmasked `ppoll`, marked as `poll` is; SIGALRM unblocked.
    PpollPoller,
    /// Polling a receive, with the ERESTARTSYS marker; SIGALRM unblocked.
    RecvPoller,
    /// Polling `select`, whose probe takes a pending signal itself (the EINTR
    /// marker); SIGALRM unblocked.
    SelectPoller,
    /// Readmitted with its rejoin request, which says its temporary mask
    /// admits SIGALRM, while its readmission recorded one that blocks it
    /// (`continuation_masks`); its own mask does not block it either.
    RejoinedUnderBlockingMask,
    /// Running a call outside the schedule (`external_io_blockers`), whose
    /// rejoin, filed when host timing finished the call, says its temporary
    /// mask is unknown; SIGALRM unblocked.
    OutsideWithUnknownRejoin,
    /// Parked in an `rt_sigsuspend` the backend serves (SIGSHARE S2b), held in
    /// the timed waiters, whose temporary mask admits SIGALRM, which its own
    /// mask blocks.
    BackendSuspend,
    /// As `BackendSuspend`, with a temporary mask that blocks SIGALRM, which
    /// its own mask does not.
    BackendSuspendBlocks,
    /// Queued to run in a masked `ppoll` the backend serves (SIGSHARE S2b),
    /// whose temporary mask blocks SIGALRM, which its own mask does not.
    MaskedPpoll,
    /// Queued to run in a masked `ppoll` whose temporary mask admits SIGALRM,
    /// which its own mask blocks.
    MaskedPpollAdmits,
}

/// Each `Pause` thread of `choose_alarm_receiver` and its response.
type ParkedResponses = Vec<(DetTid, Ivar<SchedResponse>)>;

/// One case of the receiver choice: its name, the threads, `curr_target`, the
/// thread step 3 resumes first, and the thread observed.
type ReceiverCase = (
    &'static str,
    &'static [AlarmThread],
    Option<i32>,
    Option<DetTid>,
    Option<DetTid>,
);

/// Process 100 with one thread per entry of `threads` (TIDs 100, 101, ...;
/// the first is the leader), each in its state, and a SIGALRM pending in its
/// shared queue, for which the backend reports each thread whose own mask
/// leaves it unblocked as a recipient. `curr_target` is the process's
/// `signal->curr_target`, the leader when `None`. Returns the scheduler
/// after one `select_parked_alarm`, the backend, and each `Pause` thread's
/// response.
fn choose_alarm_receiver(
    threads: &[AlarmThread],
    curr_target: Option<i32>,
) -> (Scheduler, Arc<Backend>, ParkedResponses) {
    let (mut s, b) = fixture();
    let mut parked = Vec::new();
    for (i, state) in threads.iter().enumerate() {
        let raw = 100 + i32::try_from(i).unwrap();
        let (tid, mm, site) = add(&mut s, 100, raw);
        let own_mask = match state {
            AlarmThread::Blocks
            | AlarmThread::MaskedWaitAdmits
            | AlarmThread::Sigtimedwait
            | AlarmThread::BackendSuspend
            | AlarmThread::MaskedPpollAdmits => SIGALRM_BIT,
            _ => 0,
        };
        let usr1 = 1 << (libc::SIGUSR1 - 1);
        let (pending, ignored) = match state {
            AlarmThread::Busy | AlarmThread::BusyPause | AlarmThread::BusyVforkParent => (usr1, 0),
            AlarmThread::IgnoredPending => (usr1, usr1),
            _ => (0, 0),
        };
        report_signal_sets(
            &b,
            100,
            raw,
            reverie::SignalSets::new(pending, SIGALRM_BIT, own_mask, ignored, SIGALRM_BIT),
        );
        if own_mask == 0 {
            b.recipients.lock().unwrap().push(SignalRecipient {
                task: task(100, raw),
            });
        }
        match state {
            AlarmThread::Runnable
            | AlarmThread::Blocks
            | AlarmThread::Busy
            | AlarmThread::IgnoredPending => {
                file_ordinary(&mut s, tid);
                s.runqueue_push_back(tid);
            }
            AlarmThread::Pause | AlarmThread::BusyPause => {
                parked.push((tid, parked_pause(&mut s, tid, mm, site)))
            }
            AlarmThread::MaskedWait => {
                file_masked_pselect6(&mut s, tid, SIGALRM_BIT);
                s.runqueue_push_back(tid);
            }
            AlarmThread::MaskedWaitAdmits => {
                file_masked_pselect6(&mut s, tid, 0);
                s.runqueue_push_back(tid);
            }
            AlarmThread::Sigtimedwait => {
                let mut r = Resources::new(tid);
                r.insert(ResourceID::InternalIOPolling, Permission::W);
                r.fyi("rt_sigtimedwait");
                r.poll_attempt = 1;
                s.next_turns[&tid].req.put(Ok(r));
                assert!(s.set_sigtimedwait_set(tid, Some(SIGALRM_BIT)));
                s.run_queue.push_poller(tid, DEFAULT_PRIORITY, 1);
            }
            AlarmThread::Exiting => {
                file_exit(&mut s, tid, mm);
                s.runqueue_push_back(tid);
            }
            AlarmThread::FutexWait => {
                let mut r = Resources::new(tid);
                r.insert(ResourceID::FutexWait, Permission::W);
                s.next_turns[&tid].req.put(Ok(r));
            }
            AlarmThread::VforkParent => {
                file_ordinary(&mut s, tid);
                s.vfork_barriers.insert(tid, None);
            }
            AlarmThread::BusyVforkParent => {
                s.blocked
                    .external_io_blockers
                    .insert(tid, ExternalOpId::new(tid, 1));
                s.vfork_barriers.insert(tid, None);
            }
            AlarmThread::PollPoller => file_poller(&mut s, tid, "poll", Some(Errno::EINTR)),
            AlarmThread::PpollPoller => file_poller(&mut s, tid, "ppoll", Some(Errno::EINTR)),
            AlarmThread::RecvPoller => {
                file_poller(&mut s, tid, "recvfrom", Some(Errno::ERESTARTSYS))
            }
            AlarmThread::SelectPoller => file_poller(&mut s, tid, "select", Some(Errno::EINTR)),
            AlarmThread::RejoinedUnderBlockingMask => {
                let mut r = Resources::new(tid);
                r.insert(
                    ResourceID::BlockedExternalContinue(ExternalOpId::new(tid, 1)),
                    Permission::RW,
                );
                r.blocked_signal_mask = Some(0);
                s.next_turns[&tid].req.put(Ok(r));
                s.blocked.continuation_masks.insert(tid, Some(SIGALRM_BIT));
                s.runqueue_push_back(tid);
            }
            AlarmThread::OutsideWithUnknownRejoin => {
                s.blocked
                    .external_io_blockers
                    .insert(tid, ExternalOpId::new(tid, 1));
                let mut r = Resources::new(tid);
                r.insert(
                    ResourceID::BlockedExternalContinue(ExternalOpId::new(tid, 1)),
                    Permission::RW,
                );
                r.blocked_signal_mask_unknown = true;
                s.next_turns[&tid].req.put(Ok(r));
            }
            AlarmThread::BackendSuspend => file_backend_suspend(&mut s, tid, 0),
            AlarmThread::BackendSuspendBlocks => file_backend_suspend(&mut s, tid, SIGALRM_BIT),
            AlarmThread::MaskedPpoll => {
                file_masked_ppoll(&mut s, tid, SIGALRM_BIT);
                s.runqueue_push_back(tid);
            }
            AlarmThread::MaskedPpollAdmits => {
                file_masked_ppoll(&mut s, tid, 0);
                s.runqueue_push_back(tid);
            }
        }
    }
    if let Some(target) = curr_target {
        s.signal_curr_targets
            .insert(DetPid::from_raw(100), DetTid::from_raw(target));
    }
    s.committed_time = at(10);
    s.select_parked_alarm().unwrap();
    (s, b, parked)
}

/// The thread step 3 resumes first for the pending SIGALRM, and the parked
/// thread observed for it.
fn alarm_action(
    s: &Scheduler,
    parked: &[(DetTid, Ivar<SchedResponse>)],
) -> (Option<DetTid>, Option<DetTid>) {
    let observed = parked
        .iter()
        .find(|(_, response)| response.try_read().is_some())
        .map(|(tid, response)| {
            selected(response);
            *tid
        });
    (s.first_alarm_taker(), observed)
}

/// https://github.com/rrnewton/hermit/issues/4929 (G): one choice of the
/// thread that takes a published SIGALRM, runnable or parked, by Linux's
/// `complete_signal` (v6.17 kernel/signal.c:963-997): the leader, which the
/// expiry names, if it wants the signal, and otherwise the first thread that
/// does, walking from `curr_target`. A thread wants it (`wants_signal`,
/// lines 946-961) when its mask leaves it unblocked (a wait's temporary
/// mask where it has one; the set an `rt_sigtimedwait` waits for is
/// unblocked, lines 3776-3777), its exit is not filed, and no other signal
/// it leaves unblocked is pending for it alone. Only that thread is acted
/// on: queued to run, step 3 resumes it first; parked in a wait the
/// observation can interrupt, it is observed; polling a call whose probe
/// never takes a pending signal (`poll`, an unmasked `ppoll`, a receive,
/// `rt_sigtimedwait`), it is woken and resumed first, as Linux's
/// `signal_wake_up` ends its wait, though a `poll`'s and a `ppoll`'s requests
/// carry the EINTR marker as a `select`'s does; polling `select` or
/// `pselect6`, whose probe takes the signal under the call's own mask,
/// nothing is done, and that probe takes the copy. No other
/// thread is acted on in its place, and a parked thread Linux does not wake
/// is not observed; where no thread wants the copy, a parked thread the walk
/// passed over as busy, which Linux woke for its own signal, is. A thread
/// waiting where Linux would interrupt the wait
/// but this backend cannot (a futex wait, a call outside the schedule) is
/// passed over, so the copy reaches a thread that can take it (the named KVM
/// gap D1); a vfork parent, which Linux cannot wake either, is not, and the
/// copy waits for it. A mask comes from committed state or from a request
/// the thread filed in its own turn: a rejoin's own mask fields, and
/// anything filed by a thread outside the schedule, are never read.
#[test]
fn one_receiver_choice_takes_a_published_alarm_as_complete_signal_chooses() {
    use AlarmThread::*;
    let t = DetTid::from_raw;
    let cases: [ReceiverCase; 33] = [
        (
            "a runnable leader that wants it, beside a parked sibling",
            &[Runnable, Pause],
            None,
            Some(t(100)),
            None,
        ),
        (
            "a parked leader that wants it, beside a runnable sibling",
            &[Pause, Runnable],
            None,
            None,
            Some(t(100)),
        ),
        (
            "the walk from the leader",
            &[Blocks, Runnable, Pause],
            None,
            Some(t(101)),
            None,
        ),
        (
            "the walk from curr_target",
            &[Blocks, Runnable, Pause],
            Some(102),
            None,
            Some(t(102)),
        ),
        (
            "the only parked thread that wants it",
            &[Blocks, Pause],
            None,
            None,
            Some(t(101)),
        ),
        (
            "a busy thread is passed over",
            &[Blocks, Busy, Runnable],
            None,
            Some(t(102)),
            None,
        ),
        (
            "a parked leader with its own signal pending, which Linux woke for \
             that signal, is observed",
            &[BusyPause],
            None,
            None,
            Some(t(100)),
        ),
        (
            "a parked sibling with its own signal pending is observed",
            &[Blocks, BusyPause],
            None,
            None,
            Some(t(101)),
        ),
        (
            "a parked leader with its own signal pending is observed, not passed \
             over for a parked sibling that wants the copy",
            &[BusyPause, Pause],
            None,
            None,
            Some(t(100)),
        ),
        (
            "a parked leader with its own signal pending is observed, and no \
             runnable sibling takes the copy in its place",
            &[BusyPause, Runnable],
            None,
            None,
            Some(t(100)),
        ),
        (
            "a parked sibling with its own signal pending is observed first, \
             beside a parked leader that wants the copy",
            &[Pause, BusyPause],
            None,
            None,
            Some(t(101)),
        ),
        (
            "an ignored pending signal does not make it busy",
            &[Blocks, IgnoredPending, Runnable],
            None,
            Some(t(101)),
            None,
        ),
        (
            "a wait's temporary mask blocks it",
            &[Blocks, MaskedWait, Runnable],
            None,
            Some(t(102)),
            None,
        ),
        (
            "a wait's temporary mask admits it",
            &[Blocks, MaskedWaitAdmits, Runnable],
            None,
            None,
            None,
        ),
        (
            "rt_sigtimedwait waits for it, and is woken to take it",
            &[Blocks, Sigtimedwait, Runnable],
            None,
            Some(t(101)),
            None,
        ),
        (
            "an exiting leader is passed over",
            &[Exiting, Runnable],
            None,
            Some(t(101)),
            None,
        ),
        (
            "the only thread that wants it is busy",
            &[Blocks, Busy],
            None,
            None,
            None,
        ),
        (
            "every thread blocks it",
            &[Blocks, Blocks],
            None,
            None,
            None,
        ),
        ("one thread, which blocks it", &[Blocks], None, None, None),
        (
            "a leader in a futex wait is passed over",
            &[FutexWait, Pause],
            None,
            None,
            Some(t(101)),
        ),
        (
            "a leader alone in a futex wait",
            &[FutexWait],
            None,
            None,
            None,
        ),
        (
            "a vfork parent leader is chosen, and the copy waits for it",
            &[VforkParent, Runnable],
            None,
            None,
            None,
        ),
        // SIGSHARE S3b-1 fix 9 (`alarm_wants`): a vfork parent's pending set
        // is read although it waits outside the schedule, since no host
        // timing ends its call or posts it a signal, so with a signal pending
        // it is busy and the walk passes it over (`wants_signal`,
        // kernel/signal.c:958-961 at v6.17). Mutation "a vfork parent outside
        // the schedule is never busy": the leader is chosen, and the copy
        // waits for it.
        (
            "a busy vfork parent leader outside the schedule is passed over",
            &[BusyVforkParent, Runnable],
            None,
            Some(t(101)),
            None,
        ),
        (
            "a leader polling poll is woken, not a parked sibling",
            &[PollPoller, Pause],
            None,
            Some(t(100)),
            None,
        ),
        (
            "a leader polling an unmasked ppoll is woken, not a parked sibling",
            &[PpollPoller, Pause],
            None,
            Some(t(100)),
            None,
        ),
        (
            "a leader polling a receive is woken, not a parked sibling",
            &[RecvPoller, Pause],
            None,
            Some(t(100)),
            None,
        ),
        (
            "a leader polling select takes it at its own probe",
            &[SelectPoller, Pause],
            None,
            None,
            None,
        ),
        (
            "a rejoin is read by the mask its readmission recorded",
            &[RejoinedUnderBlockingMask, Runnable],
            None,
            Some(t(101)),
            None,
        ),
        (
            "a thread outside the schedule is passed over, its rejoin unread",
            &[OutsideWithUnknownRejoin, Pause],
            None,
            None,
            Some(t(101)),
        ),
        (
            "a leader in a backend-served sigsuspend whose mask admits it is \
             chosen, and the drain's wake takes it: not passed over for a \
             parked sibling",
            &[BackendSuspend, Pause],
            None,
            None,
            None,
        ),
        (
            "a backend-served sigsuspend whose mask blocks it is passed over",
            &[BackendSuspendBlocks, Pause],
            None,
            None,
            Some(t(101)),
        ),
        (
            "a masked ppoll's temporary mask blocks it",
            &[Blocks, MaskedPpoll, Runnable],
            None,
            Some(t(102)),
            None,
        ),
        (
            "a masked ppoll's temporary mask admits it: its probe takes it",
            &[Blocks, MaskedPpollAdmits, Runnable],
            None,
            None,
            None,
        ),
    ];
    for (case, threads, curr_target, taker, observed) in cases {
        let (s, b, parked) = choose_alarm_receiver(threads, curr_target);
        assert_eq!(alarm_action(&s, &parked), (taker, observed), "{case}");
        assert_eq!(
            b.permits.lock().unwrap().len(),
            usize::from(observed.is_some()),
            "{case}"
        );
        assert!(!s.backend_failed(), "{case}");
    }
}

/// <https://github.com/rrnewton/hermit/issues/4929> (the Claude review of the
/// slice's rebase onto C2, P2 and P3): a thread parked in `pause` whose own
/// SIGUSR1 is pending is observed whether or not a SIGALRM is pending, since
/// Linux's thread-directed send woke it (kernel/signal.c:946-961 and 1033 at
/// v6.17), and `pause` returns at once with a signal pending (lines
/// 4822-4829). A SIGUSR1 it blocks, or one its process ignores, which Linux
/// drops as it is sent, leaves it asleep.
#[test]
fn a_parked_thread_with_its_own_signal_pending_is_observed_without_a_sigalrm() {
    let usr1 = 1 << (libc::SIGUSR1 - 1);
    for (case, blocked, ignored, observed) in [
        ("unblocked and handled", 0, 0, true),
        ("blocked", usr1, 0, false),
        ("ignored", 0, usr1, false),
    ] {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        report_signal_sets(
            &b,
            100,
            100,
            reverie::SignalSets::new(usr1, 0, blocked, ignored, usr1 & !ignored),
        );
        let response = parked_pause(&mut s, tid, mm, site);
        s.committed_time = at(10);
        s.select_parked_alarm().unwrap();
        assert_eq!(response.try_read().is_some(), observed, "{case}");
        assert_eq!(
            b.permits.lock().unwrap().len(),
            usize::from(observed),
            "{case}"
        );
        assert!(!s.backend_failed(), "{case}");
    }
}

/// https://github.com/rrnewton/hermit/issues/4929 (G): Linux's
/// `complete_signal` runs once per generated copy and moves `curr_target` to
/// the thread its walk chose (v6.17 kernel/signal.c:996): here the expiry's
/// publication, with the leader blocking SIGALRM, moves it to the worker. The
/// leader's own choice moves nothing (line 974), and nor does a copy the
/// shared queue already held, which `legacy_queue` keeps from reaching
/// `complete_signal` (lines 1063-1064).
#[test]
fn a_published_expiry_moves_curr_target_to_the_walks_choice() {
    for (leader_blocks, coalesced, moved) in [
        (true, false, Some(101)),
        (false, false, None),
        (true, true, None),
    ] {
        let (mut s, b) = fixture();
        let (leader, _, _) = add(&mut s, 100, 100);
        let (worker, _, _) = add(&mut s, 100, 101);
        for (raw, tid) in [(100, leader), (101, worker)] {
            let blocked = if raw == 100 && leader_blocks {
                SIGALRM_BIT
            } else {
                0
            };
            report_signal_sets(
                &b,
                100,
                raw,
                reverie::SignalSets::new(0, 0, blocked, 0, SIGALRM_BIT),
            );
            file_ordinary(&mut s, tid);
            s.runqueue_push_back(tid);
            if blocked == 0 {
                b.recipients_on_alarm.lock().unwrap().push(SignalRecipient {
                    task: task(100, raw),
                });
            }
        }
        b.coalesce_alarm
            .store(coalesced, std::sync::atomic::Ordering::Relaxed);
        s.replace_real_timer(
            leader,
            leader,
            at(0),
            at(100_000),
            at(100_000),
            Signal::SIGALRM,
        )
        .unwrap();
        s.committed_time = at(100_000);
        assert!(s.step2b_process_timed(&committed_clock(&s)));
        assert_eq!(b.publications.lock().unwrap().len(), 1);
        assert_eq!(
            s.signal_curr_targets.get(&DetPid::from_raw(100)).copied(),
            moved.map(DetTid::from_raw),
            "leader blocks {leader_blocks}, coalesced {coalesced}"
        );
        assert!(!s.backend_failed());
    }
}

/// https://github.com/rrnewton/hermit/issues/4929 (the Claude review of slice
/// 2, round 3, finding 1): a POSIX timer's SIGALRM goes through Linux's
/// `complete_signal` as an `ITIMER_REAL` copy does (`posixtimer_queue_sigqueue`,
/// kernel/signal.c:1943-1953 at v6.17): its publication moves `curr_target` to
/// the walk's choice, here the worker, as the leader blocks SIGALRM. A copy
/// the queue already held chooses nothing (the timer counts an overrun, lines
/// 2061-2064), and nor does a timer with another signal, which no SIGALRM
/// receiver choice serves. And (the Claude review of slice 2, round 4,
/// finding B) a copy the leader leaves unblocked and ignores is dropped when
/// it is sent (`send_sigqueue` -> `prepare_signal` -> `sig_ignored`,
/// kernel/signal.c:1966-2030 and 104-123 at v6.12): it is not published and
/// `curr_target` does not move, for SIGALRM as for any other signal; an
/// ignored copy the leader blocks is queued, and chooses as a caught one does.
#[test]
fn a_published_posix_timer_sigalrm_moves_curr_target_to_the_walks_choice() {
    const IGNORED: bool = true;
    const BLOCKED: bool = true;
    for (signal, coalesced, leader_ignores, leader_blocks, published, moved) in [
        (Signal::SIGALRM, false, !IGNORED, BLOCKED, 1, Some(101)),
        (Signal::SIGALRM, true, !IGNORED, BLOCKED, 1, None),
        (Signal::SIGUSR1, false, !IGNORED, BLOCKED, 1, None),
        (Signal::SIGALRM, false, IGNORED, !BLOCKED, 0, None),
        (Signal::SIGUSR1, false, IGNORED, !BLOCKED, 0, None),
        (Signal::SIGALRM, false, IGNORED, BLOCKED, 1, Some(101)),
    ] {
        let bit = 1u64 << (signal as i32 - 1);
        let (mut s, b) = fixture();
        let (leader, _, _) = add(&mut s, 100, 100);
        let (worker, _, _) = add(&mut s, 100, 101);
        for (raw, tid) in [(100, leader), (101, worker)] {
            let blocked = if raw == 100 && leader_blocks { bit } else { 0 };
            let (ignored, caught) = if leader_ignores { (bit, 0) } else { (0, bit) };
            report_signal_sets(
                &b,
                100,
                raw,
                reverie::SignalSets::new(0, 0, blocked, ignored, caught),
            );
            file_ordinary(&mut s, tid);
            s.runqueue_push_back(tid);
            if blocked == 0 {
                b.recipients_on_alarm.lock().unwrap().push(SignalRecipient {
                    task: task(100, raw),
                });
            }
        }
        b.coalesce_alarm
            .store(coalesced, std::sync::atomic::Ordering::Relaxed);
        s.dispatch_timed_signal(
            at(100_000),
            timed_waiters::SignalTimerId::Posix(DetPid::from_raw(100), 3),
            timed_waiters::SignalTarget::Process(leader),
            timed_waiters::TimerSignal::Send(signal),
            true,
            at(100_000),
        );
        let row = format!(
            "{signal:?}, coalesced {coalesced}, leader ignores {leader_ignores}, blocks {leader_blocks}"
        );
        assert_eq!(
            b.timer_publications.lock().unwrap().len(),
            published,
            "{row}"
        );
        assert!(b.publications.lock().unwrap().is_empty(), "{row}");
        assert_eq!(
            s.signal_curr_targets.get(&DetPid::from_raw(100)).copied(),
            moved.map(DetTid::from_raw),
            "{row}"
        );
        assert!(!s.backend_failed(), "{row}");
    }
}

/// https://github.com/rrnewton/hermit/issues/4929 (the Claude review of slice
/// 2, round 3, finding 2): a thread running a call outside the schedule is
/// never busy for the receiver walk. The backend can post a signal to it alone
/// when host timing completes that call (SIGPIPE for a write that fails with
/// EPIPE), so its pending set is not a function of the schedule. Linux's rule
/// gives it the copy whether or not such a signal is pending, which decides
/// the D1 record and the fatal refusal, and the backend's rule passes it over
/// (D1) either way.
#[test]
fn a_thread_outside_the_schedule_is_never_busy_for_the_receiver_walk() {
    let pid = DetPid::from_raw(100);
    let sigpipe = 1u64 << (libc::SIGPIPE - 1);
    for pending in [0, sigpipe] {
        let (s, b, _) = choose_alarm_receiver(
            &[AlarmThread::OutsideWithUnknownRejoin, AlarmThread::Blocks],
            None,
        );
        replace_signal_sets(
            &b,
            100,
            100,
            reverie::SignalSets::new(pending, SIGALRM_BIT, 0, 0, SIGALRM_BIT),
        );
        let recipients = b
            .alarm_recipients(task(100, 100).process)
            .expect("recipients");
        assert_eq!(
            s.alarm_receiver_and_walk(pid, &recipients, AlarmReachRule::Linux)
                .0,
            Some(DetTid::from_raw(100)),
            "pending {pending:#x}"
        );
        assert_eq!(
            s.alarm_receiver(pid, &recipients),
            None,
            "pending {pending:#x}"
        );
    }
}

/// https://github.com/rrnewton/hermit/issues/4929 (G): step 3 resumes the
/// thread chosen to take a pending SIGALRM before the run queue's own order,
/// and only while the copy waits: once the copy is taken, the next selection
/// pass chooses nothing and the queue's order decides again. Without a
/// process signal control (ptrace) no thread is ever chosen.
#[test]
fn step3_resumes_the_alarm_receiver_first_only_while_the_copy_waits() {
    let (mut s, b, _) = choose_alarm_receiver(&[AlarmThread::Blocks, AlarmThread::Runnable], None);
    assert_eq!(s.step3_peek().unwrap().0, DetTid::from_raw(101));
    s.run_queue.undo_tentative_pop();
    b.recipients.lock().unwrap().clear();
    s.select_parked_alarm().unwrap();
    assert_eq!(s.first_alarm_taker(), None);
    assert_eq!(s.step3_peek().unwrap().0, DetTid::from_raw(100));
    s.run_queue.undo_tentative_pop();

    let (mut s, b) = fixture_with_control(false);
    for raw in [100, 101] {
        let (tid, _, _) = add(&mut s, 100, raw);
        file_ordinary(&mut s, tid);
        s.runqueue_push_back(tid);
    }
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 101),
    });
    s.select_parked_alarm().unwrap();
    assert_eq!(s.first_alarm_taker(), None);
    assert_eq!(s.step3_peek().unwrap().0, DetTid::from_raw(100));
}

/// https://github.com/rrnewton/hermit/issues/4929 (review round 1, F3): with
/// a process signal control installed, the polling record leaves a probed
/// call's temporary mask out of the request it prints, as ptrace's request
/// does not hold it, while the request keeps it for the reader. A kernel
/// wait's mask, and every request where no control is installed, print as
/// before. Mutation: printing the request as it is (the probed text names
/// `blocked_signal_mask`).
#[test]
fn the_polling_record_prints_a_probed_mask_as_ptrace_does() {
    for installed in [true, false] {
        let (mut s, _) = fixture_with_control(installed);
        let (tid, _, _) = add(&mut s, 100, 100);
        file_masked_pselect6(&mut s, tid, SIGALRM_BIT);
        let probed = s.next_turns[&tid]
            .req
            .try_read()
            .expect("filed")
            .expect("a request");
        assert_eq!(probed.blocked_signal_mask, Some(SIGALRM_BIT));
        let printed = format!("{:?}", s.logged_polling_request(&probed));
        assert_eq!(
            printed.contains("blocked_signal_mask"),
            !installed,
            "control installed {installed}: {printed}"
        );
        let mut kernel_wait = Resources::new(tid);
        kernel_wait.insert(
            ResourceID::BlockingExternalIO(ExternalOpId::new(tid, 1)),
            Permission::RW,
        );
        kernel_wait.blocked_signal_mask = Some(SIGALRM_BIT);
        assert!(
            format!("{:?}", s.logged_polling_request(&kernel_wait)).contains("blocked_signal_mask"),
            "control installed {installed}"
        );
    }
}

/// https://github.com/rrnewton/hermit/issues/4929 (review round 1, F5): the
/// mask of a thread running a call outside the schedule is read from
/// committed state, never from its request slot, which is empty while the
/// call runs and then holds the rejoin host timing files. The scheduler
/// records a background call's temporary mask, or an unknown one, as it
/// releases the call (`controlled_background_masks`), and the reader takes
/// that, or else the thread's own mask from the control, whatever rejoin is
/// filed later; readmitted with that rejoin, the thread is read by the mask
/// its readmission recorded (`continuation_masks`), not by the rejoin's own
/// fields. Mutations: reading the request slot of a thread outside the
/// schedule (the answer moves when the rejoin is filed); not recording the
/// call's mask (the temporary and unknown cases read the own mask).
#[test]
fn a_thread_outside_the_schedule_is_read_from_committed_state() {
    let own = 1 << (libc::SIGUSR2 - 1);
    for (case, blocked, unknown, want) in [
        ("own mask", None, false, Ok(None)),
        (
            "temporary mask",
            Some(SIGALRM_BIT),
            false,
            Ok(Some(SIGALRM_BIT)),
        ),
        ("unknown mask", None, true, Err(TemporaryMaskUnknown)),
    ] {
        let (mut s, b) = fixture();
        let (tid, _, _) = add(&mut s, 100, 100);
        report_signal_sets(
            &b,
            100,
            100,
            reverie::SignalSets::new(0, 0, own, 0, SIGALRM_BIT),
        );
        let op = ExternalOpId::new(tid, 1);
        let mut call = Resources::new(tid);
        call.insert(ResourceID::BlockingExternalIO(op), Permission::RW);
        call.blocked_signal_mask = blocked;
        call.blocked_signal_mask_unknown = unknown;
        s.next_turns[&tid].req.put(Ok(call));
        s.runqueue_push_back(tid);
        assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
        assert!(
            s.block_for_one_resource(
                tid,
                &ResourceID::BlockingExternalIO(op),
                &Permission::RW,
                None,
                blocked,
                &Ivar::new()
            )
            .is_err(),
            "{case}"
        );
        assert!(s.blocked.external_io_blockers.contains_key(&tid), "{case}");
        let read = |s: &Scheduler| {
            (
                s.controlled_temporary_mask(tid),
                s.read_guest_signal_mask(tid),
            )
        };
        let expected = (
            want,
            match want {
                Ok(Some(mask)) => Some(mask),
                Ok(None) => Some(own),
                Err(_) => None,
            },
        );
        assert_eq!(read(&s), expected, "{case}: running");
        let mut rejoin = Resources::new(tid);
        rejoin.insert(ResourceID::BlockedExternalContinue(op), Permission::RW);
        rejoin.blocked_signal_mask = Some(0);
        rejoin.blocked_signal_mask_unknown = !unknown;
        s.next_turns.get_mut(&tid).unwrap().req = Ivar::full(Ok(rejoin));
        assert_eq!(read(&s), expected, "{case}: its rejoin filed");
        s.blocked.external_io_blockers.remove(&tid);
        s.blocked.controlled_background_masks.remove(&tid);
        s.blocked.continuation_masks.insert(tid, Some(SIGALRM_BIT));
        s.runqueue_push_back(tid);
        assert_eq!(
            read(&s),
            (Ok(Some(SIGALRM_BIT)), Some(SIGALRM_BIT)),
            "{case}: readmitted"
        );
    }
}

/// https://github.com/rrnewton/hermit/issues/4929 (review round 1, F1): a
/// receiver polling a call whose probe never takes a pending signal (`poll`,
/// `epoll_wait`, a receive) is woken as the scheduler wakes a polling thread
/// for a signal it sends (`wake_signaled_guest`): its filed retry is replaced
/// by the signal, which its turn answers `Signaled`, so its call returns for
/// it, and it is queued at the front as a thread that runs its turn, and
/// resumed first. Before, nothing was done for it and no other thread was
/// chosen: the copy stayed pending while the poller probed for ever. Not
/// while the alarm priority serves another process: woken then, it would
/// wait behind the run queue's own choice, so it keeps polling until then.
#[test]
fn a_polling_receiver_is_woken_for_the_copy_and_resumed_first() {
    let (s, _, _) = choose_alarm_receiver(&[AlarmThread::PollPoller, AlarmThread::Pause], None);
    let leader = DetTid::from_raw(100);
    let request = s.next_turns[&leader]
        .req
        .try_read()
        .expect("the woken poller's request")
        .expect("a filed request");
    assert_eq!(request.resources.len(), 1, "{request:?}");
    assert!(
        request
            .resources
            .contains_key(&ResourceID::InboundSignal(SigWrapper(libc::SIGALRM))),
        "{request:?}"
    );
    assert!(s.run_queue.contains_non_polling_tid(leader));
    assert_eq!(s.first_alarm_taker(), Some(leader));
    assert!(!s.backend_failed());

    let (mut s, b) = fixture();
    let (leader, _, _) = add(&mut s, 100, 100);
    report_signal_sets(
        &b,
        100,
        100,
        reverie::SignalSets::new(0, SIGALRM_BIT, 0, 0, SIGALRM_BIT),
    );
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    file_poller(&mut s, leader, "epoll_wait", None);
    s.parked.alarm_priority_served = Some(DetPid::from_raw(200));
    s.select_parked_alarm().unwrap();
    let request = s.next_turns[&leader]
        .req
        .try_read()
        .expect("the poller's request")
        .expect("a filed request");
    assert!(
        request
            .resources
            .contains_key(&ResourceID::InternalIOPolling),
        "woken while the alarm priority served another process: {request:?}"
    );
    assert!(!s.run_queue.contains_non_polling_tid(leader));
    assert_eq!(s.first_alarm_taker(), None);
}

/// https://github.com/rrnewton/hermit/issues/4929 (review round 1, Codex
/// P1): step 3 never makes two grants by alarm priority for different
/// processes without a choice of the run queue's own between them. Here two
/// processes each have a queued taker, and every grant takes its own
/// process's copy and makes the other process's copy pending, as two
/// recurring timers that fall due again within each other's grants do; a
/// third process has a runnable worker and no timer. Alarm priority alone
/// alternated the two takers at every selection and the worker never ran;
/// with the bound the worker runs within every four selections, and each
/// taker still runs at every fourth, so neither the signals' owners nor
/// their expiries change. The bound holds for one process too (the Codex
/// review of slice 2, round 2, P1): its taker whose copy falls due again at
/// once waits for one choice of the run queue's own, and is then resumed by
/// alarm priority.
#[test]
fn alarm_priority_leaves_room_for_the_run_queues_own_choice() {
    let (mut s, b) = fixture();
    let mut threads = Vec::new();
    for pid in [100, 200, 300] {
        let (tid, _, _) = add(&mut s, pid, pid);
        report_signal_sets(
            &b,
            pid,
            pid,
            reverie::SignalSets::new(0, SIGALRM_BIT, 0, 0, SIGALRM_BIT),
        );
        file_ordinary(&mut s, tid);
        s.runqueue_push_back(tid);
        threads.push(tid);
    }
    let worker = threads[2];
    let pending = |b: &Backend, pids: &[i32]| {
        *b.recipients.lock().unwrap() = pids
            .iter()
            .map(|pid| SignalRecipient {
                task: task(*pid, *pid),
            })
            .collect();
    };
    pending(&b, &[100, 200]);
    let mut selections = Vec::new();
    for _ in 0..16 {
        s.select_parked_alarm().unwrap();
        let served = s.parked.alarm_priority_served;
        let (tid, _, _) = s.step3_peek().unwrap();
        let by_alarm = s.parked.alarm_priority_served.is_some() && tid != worker;
        assert!(
            !(by_alarm && served.is_some_and(|served| served.as_raw() != tid.as_raw())),
            "two processes served in a row: {selections:?}"
        );
        s.run_queue.commit_tentative_pop();
        s.runqueue_push_back(tid);
        // The grant takes its process's copy and makes the others' pending.
        match tid.as_raw() {
            100 => pending(&b, &[200]),
            200 => pending(&b, &[100]),
            _ => {}
        }
        selections.push((tid, by_alarm));
    }
    for window in selections.windows(4) {
        assert!(
            window.iter().any(|(tid, _)| *tid == worker),
            "the worker waited four selections: {selections:?}"
        );
    }
    for taker in [threads[0], threads[1]] {
        assert!(
            selections.iter().filter(|(tid, _)| *tid == taker).count() >= 4,
            "{selections:?}"
        );
    }

    // The same process again: its next copy, pending before any other
    // process's, waits for one choice of the run queue's own.
    let (mut s, b) = fixture();
    for raw in [100, 101] {
        let (tid, _, _) = add(&mut s, 100, raw);
        report_signal_sets(
            &b,
            100,
            raw,
            reverie::SignalSets::new(
                0,
                SIGALRM_BIT,
                if raw == 100 { SIGALRM_BIT } else { 0 },
                0,
                SIGALRM_BIT,
            ),
        );
        file_ordinary(&mut s, tid);
        s.runqueue_push_back(tid);
    }
    pending(&b, &[]);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 101),
    });
    s.parked.alarm_priority_served = Some(DetPid::from_raw(100));
    s.select_parked_alarm().unwrap();
    assert_eq!(
        s.parked.alarm_takers.get(&DetPid::from_raw(100)),
        Some(&DetTid::from_raw(101))
    );
    assert_eq!(s.first_alarm_taker(), None);
    assert_eq!(s.step3_peek().unwrap().0, DetTid::from_raw(100));
    assert_eq!(s.parked.alarm_priority_served, None);
    s.run_queue.commit_tentative_pop();
    s.runqueue_push_back(DetTid::from_raw(100));
    s.select_parked_alarm().unwrap();
    assert_eq!(s.step3_peek().unwrap().0, DetTid::from_raw(101));
    assert_eq!(s.parked.alarm_priority_served, Some(DetPid::from_raw(100)));
    assert!(!s.backend_failed());
}

/// <https://github.com/rrnewton/hermit/issues/4929> (the Codex review of
/// slice 2, round 2, P1): a periodic POSIX timer rearms whether or not its
/// copy was taken, so its process's SIGALRM is pending again after every
/// grant, here for ever. The taker (the leader) wants it and a worker of the
/// same process blocks it. The alarm priority used to resume the taker again
/// at every selection, and the worker never ran (tests/c/alarm_receiver_reach.c's
/// `posixspin`: 6 iterations on KVM, 390 at the base, 88,283 natively); now
/// the run queue's own choice comes between any two grants, so the worker
/// runs at every other selection, and the taker at the others.
#[test]
fn a_rearming_timer_leaves_its_own_process_worker_room() {
    let (mut s, b) = fixture();
    for raw in [100, 101] {
        let (tid, _, _) = add(&mut s, 100, raw);
        report_signal_sets(
            &b,
            100,
            raw,
            reverie::SignalSets::new(
                0,
                SIGALRM_BIT,
                if raw == 101 { SIGALRM_BIT } else { 0 },
                0,
                SIGALRM_BIT,
            ),
        );
        file_ordinary(&mut s, tid);
        s.runqueue_push_back(tid);
    }
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    let mut selections = Vec::new();
    for _ in 0..12 {
        s.select_parked_alarm().unwrap();
        let (tid, _, _) = s.step3_peek().unwrap();
        s.run_queue.commit_tentative_pop();
        s.runqueue_push_back(tid);
        selections.push(tid.as_raw());
    }
    assert_eq!(
        selections,
        [100, 101, 100, 101, 100, 101, 100, 101, 100, 101, 100, 101],
        "{selections:?}"
    );
    assert!(!s.backend_failed());
}

/// <https://github.com/rrnewton/hermit/issues/4929> (the Codex review of
/// slice 2, round 2, P1): the alarm priority serves neither a signal
/// delivery that does not continue its thread's own `rt_sigreturn`, which
/// waits its place behind the other runnable threads by design (after
/// `MAX_SIGRETURN_CONTINUATIONS` deliveries in a row, or a stop that ended the
/// slice), nor a `sched_yield`, by which the thread gave its turn away.
/// Serving either put a handler that its timer kept busy first again at
/// every selection. A poller woken for the copy is served
/// (`a_polling_receiver_is_woken_for_the_copy_and_resumed_first`), and so is
/// the request that ended a slice whose time ran out: a thread that computes
/// between calls ends its turns only so.
#[test]
fn alarm_priority_serves_no_unmarked_delivery_and_no_yield() {
    let mut delivery = Resources::new(DetTid::from_raw(101));
    delivery.insert(
        ResourceID::InboundSignal(SigWrapper(libc::SIGALRM)),
        Permission::W,
    );
    let mut sched_yield = Resources::new(DetTid::from_raw(101));
    sched_yield.insert(ResourceID::SchedYield, Permission::W);
    let mut continuation = delivery.clone();
    continuation.continues_sigreturn = true;
    let mut slice_end = Resources::new(DetTid::from_raw(101));
    slice_end.insert(
        ResourceID::SleepUntil(LogicalTime::from_nanos(0)),
        Permission::W,
    );
    slice_end.slice_expired = true;
    for (label, request, served) in [
        ("an unmarked delivery", delivery, false),
        ("a sched_yield", sched_yield, false),
        ("an expired slice", slice_end, true),
        ("a sigreturn continuation", continuation, true),
        (
            "an ordinary request",
            Resources::new(DetTid::from_raw(101)),
            true,
        ),
    ] {
        let (mut s, b) = fixture();
        for raw in [100, 101] {
            let (tid, _, _) = add(&mut s, 100, raw);
            report_signal_sets(
                &b,
                100,
                raw,
                reverie::SignalSets::new(
                    0,
                    SIGALRM_BIT,
                    if raw == 100 { SIGALRM_BIT } else { 0 },
                    0,
                    SIGALRM_BIT,
                ),
            );
            s.runqueue_push_back(tid);
        }
        file_ordinary(&mut s, DetTid::from_raw(100));
        s.next_turns[&DetTid::from_raw(101)]
            .req
            .put(Ok(request.clone()));
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 101),
        });
        s.select_parked_alarm().unwrap();
        assert_eq!(
            s.parked.alarm_takers.get(&DetPid::from_raw(100)),
            Some(&DetTid::from_raw(101)),
            "{label}"
        );
        let expected = if served { 101 } else { 100 };
        assert_eq!(
            s.first_alarm_taker(),
            served.then(|| DetTid::from_raw(101)),
            "{label}"
        );
        assert_eq!(s.step3_peek().unwrap().0.as_raw(), expected, "{label}");
        assert_eq!(
            s.parked.alarm_priority_served,
            served.then(|| DetPid::from_raw(100)),
            "{label}"
        );
        assert!(!s.backend_failed(), "{label}");
    }
}

/// https://github.com/rrnewton/hermit/issues/4929 (review round 1): a
/// delivery that continues its thread's own `rt_sigreturn` goes before an
/// alarm taker, as Linux makes that delivery on the same return to user mode.
/// Run between the two, another process's grant advanced time far enough
/// that the returning thread's own timer fell due again before every return,
/// and neither process ran its own code again (the cross-process form of
/// https://github.com/rrnewton/hermit/issues/4505's livelock, measured with
/// tests/c/alarm_receiver_reach.c's `twotimers` mode). The taker keeps its
/// priority and goes first once no continuation waits.
#[test]
fn a_sigreturn_continuation_goes_before_an_alarm_taker() {
    let (mut s, b) = fixture();
    let (taker, _, _) = add(&mut s, 100, 100);
    report_signal_sets(
        &b,
        100,
        100,
        reverie::SignalSets::new(0, SIGALRM_BIT, 0, 0, SIGALRM_BIT),
    );
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    file_ordinary(&mut s, taker);
    s.runqueue_push_back(taker);
    let (returning, _, _) = add(&mut s, 200, 200);
    report_signal_sets(
        &b,
        200,
        200,
        reverie::SignalSets::new(0, 0, 0, 0, SIGALRM_BIT),
    );
    let mut delivery = Resources::new(returning);
    delivery.insert(
        ResourceID::InboundSignal(SigWrapper(libc::SIGALRM)),
        Permission::W,
    );
    delivery.continues_sigreturn = true;
    s.next_turns[&returning].req.put(Ok(delivery));
    s.sigreturn_continuations.insert(returning);
    s.runqueue_push_back(returning);
    s.select_parked_alarm().unwrap();
    assert_eq!(s.first_alarm_taker(), Some(taker));
    assert_eq!(s.step3_peek().unwrap().0, returning);
    assert_eq!(s.parked.alarm_priority_served, None);
    s.run_queue.commit_tentative_pop();
    s.sigreturn_continuations.remove(&returning);
    s.select_parked_alarm().unwrap();
    assert_eq!(s.step3_peek().unwrap().0, taker);
    assert_eq!(s.parked.alarm_priority_served, Some(DetPid::from_raw(100)));
    assert!(!s.backend_failed());
}

/// https://github.com/rrnewton/hermit/issues/4929 (review round 2, finding
/// B): D1 follows the backend's capability, not the backend. Once the backend
/// can interrupt a blocked wait for a signal (N8a's
/// `supports_blocked_wait_signal_interruption`), the choice counts the waiter
/// as Linux does, so turning the capability on retires the pass-over.
#[test]
fn the_d1_pass_over_follows_the_blocked_wait_capability() {
    use AlarmThread::*;
    let t = DetTid::from_raw;
    for (threads, without, with) in [
        (&[FutexWait, Pause][..], Some(t(101)), Some(t(100))),
        (&[FutexWait][..], None, Some(t(100))),
        (
            &[OutsideWithUnknownRejoin, Runnable][..],
            Some(t(101)),
            Some(t(100)),
        ),
    ] {
        let (mut s, b, _) = choose_alarm_receiver(threads, None);
        let recipients = b
            .alarm_recipients(task(100, 100).process)
            .expect("recipients");
        let pid = DetPid::from_raw(100);
        assert!(s.passes_over_waits_it_cannot_interrupt(), "{threads:?}");
        assert_eq!(s.alarm_receiver(pid, &recipients), without, "{threads:?}");
        s.backend.supports_blocked_wait_signal_interruption = true;
        assert!(!s.passes_over_waits_it_cannot_interrupt(), "{threads:?}");
        assert_eq!(s.alarm_receiver(pid, &recipients), with, "{threads:?}");
        assert_eq!(
            s.alarm_receiver_and_walk(pid, &recipients, AlarmReachRule::Linux)
                .0,
            with,
            "{threads:?}"
        );
    }
}

/// Replaces the signal sets the mock control reports for `pid`'s `tid`.
fn replace_signal_sets(b: &Backend, pid: i32, tid: i32, sets: reverie::SignalSets) {
    let mut reported = b.task_signal_sets.lock().unwrap();
    reported.retain(|(identity, _)| *identity != task(pid, tid));
    reported.push((task(pid, tid), sets));
}

/// https://github.com/rrnewton/hermit/issues/4929 (review round 2, finding
/// B): a new SIGALRM copy whose action is the default, which ends the
/// process, and which D1 leaves with no receiver (the only thread that wants
/// it waits in a futex wait), is refused by name: Linux ends the process at
/// once, whatever that thread waits in, and the copy would otherwise wait for
/// the wait to end. A copy a handler catches, or one the process ignores, is
/// not refused.
#[test]
fn a_fatal_alarm_that_d1_leaves_without_a_receiver_is_refused_by_name() {
    let t = DetTid::from_raw;
    let (s, b, _) = choose_alarm_receiver(&[AlarmThread::FutexWait], None);
    let pid = DetPid::from_raw(100);
    let recipients = b
        .alarm_recipients(task(100, 100).process)
        .expect("recipients");
    assert_eq!(s.alarm_receiver(pid, &recipients), None);
    assert_eq!(
        s.alarm_receiver_and_walk(pid, &recipients, AlarmReachRule::Linux)
            .0,
        Some(t(100))
    );
    // The fixture's handler.
    assert_eq!(s.fatal_alarm_refusal(pid, t(100)), None);
    replace_signal_sets(
        &b,
        100,
        100,
        reverie::SignalSets::new(0, SIGALRM_BIT, 0, SIGALRM_BIT, 0),
    );
    assert_eq!(s.fatal_alarm_refusal(pid, t(100)), None);
    replace_signal_sets(
        &b,
        100,
        100,
        reverie::SignalSets::new(0, SIGALRM_BIT, 0, 0, 0),
    );
    let refusal = s
        .fatal_alarm_refusal(pid, t(100))
        .expect("a fatal default copy is refused");
    assert!(
        refusal.contains("refusing the run rather than delaying it")
            && refusal.contains("KVM gap D1 until N8a"),
        "{refusal}"
    );
}

/// The coord7 review of https://github.com/rrnewton/hermit/pull/5221, finding
/// 5, and the kvm coordinator's addition (10:36 PM ET, 11-06): a masked
/// `ppoll` files its temporary mask on its request, and that mask must not
/// make it a wait the backend cannot interrupt. Its probe applies the mask
/// inside the call and takes a pending signal the mask lets in, as
/// `pselect6`'s does, so it takes the copy at its probe
/// (`AlarmReach::TakesAtItsProbe`), and the backend's choice is Linux's.
/// Classified only by its mask (the rule for a polled call with a temporary
/// mask and no probe that takes signals), it was passed over (D1): alone,
/// no receiver was found and a fatal SIGALRM would have been refused by name
/// where Linux ends the process; beside a runnable thread, that thread took
/// the copy where Linux gives it to the `ppoll`. Here the SIGALRM is at its
/// default action, so the fatal refusal is what a pass-over would reach.
#[test]
fn a_masked_ppoll_that_files_its_mask_takes_the_alarm_at_its_probe() {
    use AlarmThread::*;
    let t = DetTid::from_raw;
    let pid = DetPid::from_raw(100);
    for threads in [&[MaskedPpollAdmits][..], &[MaskedPpollAdmits, Runnable][..]] {
        let (s, b, _) = choose_alarm_receiver(threads, None);
        // SIGALRM at its default action, blocked by the leader's own mask.
        replace_signal_sets(
            &b,
            100,
            100,
            reverie::SignalSets::new(0, SIGALRM_BIT, SIGALRM_BIT, 0, 0),
        );
        let recipients = b
            .alarm_recipients(task(100, 100).process)
            .expect("recipients");
        assert_eq!(
            s.alarm_receiver_and_walk(pid, &recipients, AlarmReachRule::Linux)
                .0,
            Some(t(100)),
            "{threads:?}"
        );
        assert_eq!(
            s.alarm_receiver(pid, &recipients),
            Some(t(100)),
            "{threads:?}: the backend's choice is Linux's, so no fatal refusal"
        );
        assert_eq!(s.first_alarm_taker(), None, "{threads:?}");
    }
}

/// https://github.com/rrnewton/hermit/issues/4929 (D1, accepted as a named
/// KVM gap until N8a): Linux's choice counts a thread waiting where it
/// interrupts the wait but this backend cannot (a futex wait), and the
/// backend's choice passes over it, so that a thread that can take the copy
/// does. The two choices are computed apart, the second only to name the
/// difference at DEBUG (`note_alarm_generation`).
#[test]
fn the_linux_choice_differs_from_the_backends_only_by_the_named_gap() {
    use AlarmThread::*;
    let t = DetTid::from_raw;
    for (threads, linux, backend) in [
        (&[FutexWait, Pause][..], Some(t(100)), Some(t(101))),
        (&[FutexWait][..], Some(t(100)), None),
        (
            &[OutsideWithUnknownRejoin, Runnable][..],
            Some(t(100)),
            Some(t(101)),
        ),
        (&[Runnable, Pause][..], Some(t(100)), Some(t(100))),
        (
            &[Blocks, FutexWait, Runnable][..],
            Some(t(101)),
            Some(t(102)),
        ),
        // A backend-served sigsuspend is no gap: the drain ends its wait.
        (&[BackendSuspend][..], Some(t(100)), Some(t(100))),
        (&[BackendSuspend, Pause][..], Some(t(100)), Some(t(100))),
    ] {
        let (s, b, _) = choose_alarm_receiver(threads, None);
        let recipients = b
            .alarm_recipients(task(100, 100).process)
            .expect("recipients");
        let pid = DetPid::from_raw(100);
        assert_eq!(
            s.alarm_receiver_and_walk(pid, &recipients, AlarmReachRule::Linux)
                .0,
            linux,
            "{threads:?}"
        );
        assert_eq!(s.alarm_receiver(pid, &recipients), backend, "{threads:?}");
    }
}

/// The defensive step-4 settle of a no-progress probe: a successor that
/// reaches step 4 as the selected head unclassified (in production the
/// step-2 check classifies it first, as
/// `a_no_progress_probe_settles_at_step2_with_its_thread_requeued` covers).
/// The settle must close step 3's open selection before it substitutes (the
/// run queue refuses a removal inside an open selection: 0cde5810 panicked
/// here), and step 4 must end the turn rather than defer or grant the stale
/// retry: the stored request is the wake, the retry's response stays
/// unanswered, and the thread is queued exactly once, for the wake turn.
#[test]
fn a_no_progress_probe_settled_at_its_step4_head_skips_the_stale_retry() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    file_pipe_read_attempt(&mut s, tid, 1);
    s.runqueue_push_back(tid);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    assert!(sigalrm_reserved_for(&s, tid));
    // The probe is granted and its thread requeued (step 6).
    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    s.clear_nextturn(tid).unwrap();
    s.runqueue_push_back(tid);
    // Step 3 reselects it; its successor retry arrives (no bytes moved).
    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
    file_probe_successor(&mut s, tid, 1, false);
    let successor = s.next_turns[&tid].req.try_read().unwrap().unwrap();
    let resp = s.next_turns[&tid].resp.clone();

    assert!(s.step4_resource_block(tid, &successor, &resp).is_err());

    assert!(woken_by_sigalrm(&s, tid), "the wake replaced the retry");
    assert_eq!(s.sigalrm_reserved_poll_target(), None);
    assert!(resp.try_read().is_none(), "the stale retry is not answered");
    assert!(s.sigalrm_pending(pid), "taken in the wake turn");
    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid), "queued");
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    assert!(!s.run_queue.contains_tid(tid), "queued exactly once");
}

/// A completed probe settled at its step-4 head leaves the turn to
/// step 4: the head request (the completion's follow-up) proceeds as
/// it would without a reservation, which the settle consumed.
#[test]
fn a_completed_probe_settled_at_its_step4_head_lets_the_turn_proceed() {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    file_pipe_read_attempt(&mut s, tid, 1);
    s.runqueue_push_back(tid);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    s.clear_nextturn(tid).unwrap();
    s.runqueue_push_back(tid);
    assert_eq!(s.run_queue.tentative_pop_tid(tid), Some(tid));
    file_post_result(&mut s, tid);
    let follow_up = s.next_turns[&tid].req.try_read().unwrap().unwrap();
    let resp = s.next_turns[&tid].resp.clone();

    assert!(s.step4_resource_block(tid, &follow_up, &resp).is_ok());

    assert!(!woken_by_sigalrm(&s, tid), "no wake substituted a result");
    assert_eq!(s.sigalrm_reserved_poll_target(), None);
    assert!(s.sigalrm_pending(pid), "delivery remains post-result");
}

/// A thread with an outstanding reservation in the probe substate.
fn reserved_reader() -> (Scheduler, DetTid, DetPid) {
    let mut s = liteinst_scheduler();
    let (tid, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    s.set_sigalrm_handled(pid, true);
    file_pipe_read_attempt(&mut s, tid, 1);
    s.runqueue_push_back(tid);
    s.fire_alarm(pid, tid, Signal::SIGALRM);
    assert!(sigalrm_reserved_for(&s, tid));
    (s, tid, pid)
}

/// Gate G1: slice 1 never releases a reservation back to `Pending`; a
/// mask block published by the reserved target, which would release
/// it, stops the run instead of restoring the pre-reservation request.
#[test]
#[should_panic(expected = "G1")]
fn g1_a_mask_block_by_the_reserved_target_is_unreachable() {
    let (mut s, tid, _) = reserved_reader();
    s.set_sigalrm_blocked(tid, true);
}

/// Gate G1: likewise a disposition leaving the handler.
#[test]
#[should_panic(expected = "G1")]
fn g1_a_disposition_change_by_the_reserved_owner_is_unreachable() {
    let (mut s, _, pid) = reserved_reader();
    s.set_sigalrm_handled(pid, false);
}

/// Gate G1: an entry is taken only after its reservation is classified.
#[test]
#[should_panic(expected = "G1")]
fn g1_a_take_before_classification_is_unreachable() {
    let (mut s, tid, pid) = reserved_reader();
    let _ = s.take_sigalrm(pid, tid);
}

/// Gate G1's assertions bind only the reserved thread and process: an
/// unblocked publication, and any publication or take by another
/// process, proceed as before.
#[test]
fn g1_publications_elsewhere_proceed_while_a_probe_is_reserved() {
    let (mut s, tid, pid) = reserved_reader();
    let (other, _, _) = add(&mut s, 300, 300);
    let other_pid = DetPid::from_raw(300);
    s.set_sigalrm_blocked(tid, false);
    s.set_sigalrm_handled(other_pid, true);
    s.set_sigalrm_blocked(other, true);
    s.set_sigalrm_handled(other_pid, false);
    assert!(!s.take_sigalrm(other_pid, other));
    assert!(sigalrm_reserved_for(&s, tid));
    assert!(s.sigalrm_pending(pid));
}

/// Step 3 selects (`step3_peek`), the selected thread's turn completes, and
/// step 6 requeues it at the back with an empty request slot: the real
/// order around a grant.
fn grant_selected(s: &mut Scheduler) -> DetTid {
    let (tid, _, _) = s.step3_peek().expect("a selection");
    assert_eq!(s.run_queue.commit_tentative_pop_completed_turn(), tid);
    s.clear_nextturn(tid).unwrap();
    s.runqueue_push_back(tid);
    tid
}

/// A reader polling a pipe (DetPid 100, queued last) and two ordinary
/// threads (300, 500) queued ahead of it, with the reader's entry due and
/// reserved.
fn reserved_reader_behind(others: &[i32]) -> (Scheduler, DetTid, DetPid, Vec<DetTid>) {
    let mut s = liteinst_scheduler();
    let (reader, _, _) = add(&mut s, 100, 100);
    let pid = DetPid::from_raw(100);
    let mut queued = Vec::new();
    for &other in others {
        let (t, _, _) = add(&mut s, other, other);
        file_ordinary(&mut s, t);
        s.runqueue_push_back(t);
        queued.push(t);
    }
    s.set_sigalrm_handled(pid, true);
    file_pipe_read_attempt(&mut s, reader, 1);
    s.runqueue_push_back(reader);
    s.fire_alarm(pid, reader, Signal::SIGALRM);
    assert!(sigalrm_reserved_for(&s, reader));
    (s, reader, pid, queued)
}

/// The production order of a no-progress settle, at step 2 (step 1 waits
/// for every queued thread to file): step 3 selects the unrun probe ahead
/// of a writer queued before it (placed precedence,
/// `reserved_probe_to_resume`), step 6 requeues it, its successor retry is
/// filed, and the next pass's step 2 replaces that retry with the wake. The
/// thread is queued exactly once afterwards (the settle dequeues the
/// requeued retry before the wake's requeue). The wake goes to the front of
/// its band only; here the writer shares that band, so the wake runs first.
/// A thread in a better band, or a chaos draw, could still run before it,
/// and the wake grant's own attempt would then decide (R1).
#[test]
fn a_no_progress_probe_settles_at_step2_with_its_thread_requeued() {
    let (mut s, reader, pid, queued) = reserved_reader_behind(&[300]);
    let writer = queued[0];
    assert_eq!(grant_selected(&mut s), reader, "placed precedence");
    assert_eq!(s.parked.alarm_priority_served, Some(pid));
    file_probe_successor(&mut s, reader, 2, false);

    s.step2_sigalrm_ledger();

    assert!(woken_by_sigalrm(&s, reader));
    assert_eq!(s.sigalrm_reserved_poll_target(), None);
    assert_eq!(
        s.run_queue.tids().filter(|t| **t == reader).count(),
        1,
        "queued exactly once"
    );
    assert_eq!(
        grant_selected(&mut s),
        reader,
        "front of the band the writer shares"
    );
    assert_eq!(grant_selected(&mut s), writer);
}

/// Review item 1 (7cdd90dc): the probe's precedence ends with its settle.
/// A placement that outlived the reservation kept the reader first at every
/// other selection for the rest of the run. Precedence is now only the unrun
/// probe's selection at step 3 (`reserved_probe_to_resume`), which registers
/// no alarm taker; after the settle and the wake, selection is the run
/// queue's own round robin.
#[test]
fn the_reserved_probes_precedence_ends_with_its_settle() {
    let (mut s, reader, pid, queued) = reserved_reader_behind(&[300, 500]);
    assert_eq!(s.reserved_probe_to_resume(), Some((pid, reader)));
    assert!(
        s.parked.alarm_takers.is_empty(),
        "no alarm taker registered"
    );
    assert_eq!(grant_selected(&mut s), reader, "placed precedence");
    assert_eq!(
        s.reserved_probe_to_resume(),
        None,
        "granted: no longer unrun"
    );
    file_probe_successor(&mut s, reader, 2, false);
    s.step2_sigalrm_ledger();
    assert!(woken_by_sigalrm(&s, reader));
    assert_eq!(s.reserved_probe_to_resume(), None, "precedence ended");

    let mut selections = Vec::new();
    for _ in 0..9 {
        let t = grant_selected(&mut s);
        selections.push(t.as_raw());
        file_ordinary(&mut s, t);
    }
    let (b, c) = (queued[0].as_raw(), queued[1].as_raw());
    let r = reader.as_raw();
    assert_eq!(selections, [r, b, c, r, b, c, r, b, c]);
}

/// Gate item 1, the other consumers: a completed probe, a G1 loss and the
/// target's retirement each end the reservation too, so slice 1's single
/// flight is free again: another process's due polling read is reserved
/// afterwards. (A reservation left outstanding would refuse it.)
#[test]
fn every_reservation_consumer_frees_the_single_flight() {
    for consumer in ["completed", "g1-loss", "retired"] {
        let (mut s, reader, pid, _) = reserved_reader_behind(&[300]);
        assert_eq!(grant_selected(&mut s), reader);
        match consumer {
            "completed" => {
                file_post_result(&mut s, reader);
                s.step2_sigalrm_ledger();
            }
            "g1-loss" => {
                file_probe_successor(&mut s, reader, 2, true);
                s.step2_sigalrm_ledger();
                assert!(s.sigalrm_loss_recorded(pid));
            }
            _ => s.retire_sigalrm_thread(reader, pid),
        }
        assert_eq!(s.sigalrm_reserved_poll_target(), None, "{consumer}");

        let (next, _, _) = add(&mut s, 500, 500);
        let next_pid = DetPid::from_raw(500);
        s.set_sigalrm_handled(next_pid, true);
        file_pipe_read_attempt(&mut s, next, 1);
        s.runqueue_push_back(next);
        s.fire_alarm(next_pid, next, Signal::SIGALRM);
        assert!(sigalrm_reserved_for(&s, next), "{consumer}");
        assert!(!s.sigalrm_loss_recorded(next_pid), "{consumer}");
    }
}

/// Gate item 2: a poller step 4 already backed off gets placed precedence
/// when reserved (design 5.1): its priority is restored, its poll deadline
/// dropped, and step 3 resumes it ahead of a writer that became runnable
/// after the deferral.
#[test]
fn a_backed_off_poller_reserved_for_its_probe_runs_before_a_writer() {
    // Attempt 1 backs off a band or two; attempt 20 reaches the last band,
    // whose restore keeps the entry counted as a poller until it runs
    // (`deadline_restored`), which the reserved probe's placement allows.
    for attempt in [1, 20] {
        let mut s = liteinst_scheduler();
        let (reader, _, _) = add(&mut s, 100, 100);
        let (writer, _, _) = add(&mut s, 300, 300);
        let pid = DetPid::from_raw(100);
        s.set_sigalrm_handled(pid, true);
        let mut retry = pipe_read_request(reader, attempt);
        retry.poll_deadline = Some(at(5_000_000_000));
        file_request(&mut s, retry, false);
        s.runqueue_push_back(reader);
        assert_eq!(s.run_queue.tentative_pop_tid(reader), Some(reader));
        let stored = s.next_turns[&reader].req.try_read().unwrap().unwrap();
        assert!(
            s.step4_resource_block(reader, &stored, &Ivar::new())
                .is_err()
        );
        assert!(!s.run_queue.contains_non_polling_tid(reader), "backed off");
        assert!(s.poll_deadline_of.contains_key(&reader), "deadline noted");
        file_ordinary(&mut s, writer);
        s.runqueue_push_back(writer);

        s.fire_alarm(pid, reader, Signal::SIGALRM);

        assert!(sigalrm_reserved_for(&s, reader), "attempt {attempt}");
        assert_eq!(
            s.reserved_probe_to_resume(),
            Some((pid, reader)),
            "attempt {attempt}"
        );
        assert!(
            !s.poll_deadline_of.contains_key(&reader),
            "attempt {attempt}: deadline dropped"
        );
        assert_eq!(
            s.run_queue.restore_poller_priority(reader),
            PollerRestore::NotBackedOff,
            "attempt {attempt}: its normal priority was restored"
        );
        assert_eq!(
            grant_selected(&mut s),
            reader,
            "attempt {attempt}: probe before the writer"
        );
    }
}

/// The reservation's remaining refusals, where phase 1's loss stands and
/// nothing is placed: a selection already open (the poller restore is a
/// queue operation), the thread not queued to run (step 3 could not select
/// it), record and replay (excluded until the design's D6 test exists), and
/// an entry whose loss is already recorded (never reserved again).
#[test]
fn a_reservation_needs_its_thread_queued_outside_a_selection() {
    for case in [
        "open-selection",
        "not-queued",
        "record-replay",
        "already-lost",
    ] {
        let mut s = liteinst_scheduler();
        let (reader, _, _) = add(&mut s, 100, 100);
        let (other, _, _) = add(&mut s, 300, 300);
        let pid = DetPid::from_raw(100);
        s.set_sigalrm_handled(pid, true);
        file_ordinary(&mut s, other);
        s.runqueue_push_back(other);
        match case {
            "open-selection" => {
                file_pipe_read_attempt(&mut s, reader, 1);
                s.runqueue_push_back(reader);
                assert_eq!(s.run_queue.tentative_pop_tid(other), Some(other));
            }
            "not-queued" => file_pipe_read_attempt(&mut s, reader, 1),
            "record-replay" => {
                s.recordreplay_modes = true;
                file_pipe_read_attempt(&mut s, reader, 1);
                s.runqueue_push_back(reader);
            }
            _ => {
                // First due in a poll slice 1 does not reserve: the loss.
                let mut r = pipe_read_request(reader, 1);
                r.polls_guest_state_only = false;
                file_request(&mut s, r, false);
                s.runqueue_push_back(reader);
                s.fire_alarm(pid, reader, Signal::SIGALRM);
                assert!(s.sigalrm_loss_recorded(pid));
                // Then the same entry, still pending, in a reservable one.
                s.clear_nextturn(reader).unwrap();
                file_pipe_read_attempt(&mut s, reader, 2);
                s.step2_sigalrm_ledger();
            }
        }
        if case != "already-lost" {
            s.fire_alarm(pid, reader, Signal::SIGALRM);
        }

        assert!(!sigalrm_reserved_for(&s, reader), "{case}");
        assert!(s.sigalrm_loss_recorded(pid), "{case}: phase 1's loss");
        assert_eq!(s.reserved_probe_to_resume(), None, "{case}");
    }
}

/// Placed precedence enforced where step 3 selects (design rev.2 section 5,
/// review of e1ff4299 item (i)): an unrun reserved probe resumes right after
/// a group-exit decider and ahead of every other choice, whatever arrives
/// after the reservation: a spent alarm priority, an alarm taker, a
/// `rt_sigreturn` continuation of another thread with a vfork child queued
/// (which would otherwise make the alarm taker yield to the vfork child),
/// and a queued group-exit decider, which alone goes first. The probe's
/// grant spends the alarm priority (`alarm_priority_served`), but the probe
/// is selected whether or not that priority is available: like a SIGCHLD
/// taker it is exempt from the bound of
/// https://github.com/rrnewton/hermit/issues/4929, one probe grant per due
/// entry.
#[test]
fn the_unrun_probe_resumes_ahead_of_everything_but_a_group_exit_decider() {
    for case in [
        "served",
        "alarm-taker",
        "sigreturn-continuation-and-vfork-child",
        "group-exit-decider",
    ] {
        let (mut s, reader, pid, queued) = reserved_reader_behind(&[300, 500]);
        let (other, child) = (queued[0], queued[1]);
        // All of these arrive after the reservation.
        match case {
            "served" => s.parked.alarm_priority_served = Some(DetPid::from_raw(300)),
            "alarm-taker" => {
                s.parked.alarm_takers.insert(DetPid::from_raw(300), other);
            }
            "sigreturn-continuation-and-vfork-child" => {
                let mut delivery = Resources::new(other);
                delivery.insert(
                    ResourceID::InboundSignal(SigWrapper::from(Signal::SIGUSR1)),
                    Permission::W,
                );
                delivery.continues_sigreturn = true;
                s.clear_nextturn(other).unwrap();
                s.next_turns[&other].req.put(Ok(delivery));
                s.sigreturn_continuations.insert(other);
                s.vfork_barriers.insert(DetTid::from_raw(900), Some(child));
            }
            _ => s.group_exit_decider = Some(other),
        }
        assert!(sigalrm_reserved_for(&s, reader), "{case}");

        if case == "group-exit-decider" {
            assert_eq!(grant_selected(&mut s), other, "{case}: the decider first");
            // Its turn was its group exit: no longer the decider.
            s.group_exit_decider = None;
        }
        assert_eq!(grant_selected(&mut s), reader, "{case}: the probe");
        assert_eq!(s.parked.alarm_priority_served, Some(pid), "{case}");
        assert!(!s.sigalrm_loss_recorded(pid), "{case}");
    }
}

/// A scheduler signal wake that replaces the reader's own unrun probe (its
/// child's exit SIGCHLD, sent after the reservation): the probe is no
/// longer unrun, so step 3 resumes the thread as the signal taker; the
/// replaced request is classified completed before that turn is granted (no
/// loss, no G1 stop), and the entry stays pending for delivery at the
/// interrupted call's completion, where the take succeeds.
#[test]
fn a_signal_wake_replacing_the_unrun_probe_classifies_it_completed() {
    let (mut s, reader, pid, _) = reserved_reader_behind(&[300]);
    s.test_kernel_signal_states
        .insert(reader, KernelSignalState::default());
    s.wake_signaled_guest(reader, Signal::SIGCHLD);
    assert_eq!(s.reserved_probe_to_resume(), None, "replaced: not unrun");
    assert!(sigalrm_reserved_for(&s, reader), "until classified");

    let (tid, req, resp) = s.step3_peek().expect("a selection");
    assert_eq!(tid, reader, "the signal taker");
    let request = req.try_read().unwrap().unwrap();
    assert!(s.step4_resource_block(tid, &request, &resp).is_ok());

    assert_eq!(
        s.sigalrm_reserved_poll_target(),
        None,
        "classified completed"
    );
    assert!(!s.sigalrm_loss_recorded(pid));
    assert!(s.sigalrm_pending(pid), "delivered at the completion");
    assert!(s.take_sigalrm(pid, reader), "taken after classification");
}

/// A SIGCHLD taker created after the reservation, later in the same pass
/// (step 2b's child-exit pop), does not overtake the unrun probe: it
/// resumes after it.
#[test]
fn a_sigchld_taker_created_after_the_reservation_runs_after_the_probe() {
    let (mut s, reader, _pid, queued) = reserved_reader_behind(&[300]);
    let other = queued[0];
    // `other` polls a pipe read of its own, which the SIGCHLD wakes.
    s.clear_nextturn(other).unwrap();
    let mut r = pipe_read_request(other, 1);
    r.read_fd = Some((detcore_model::fd::FilesId::initial(other), 7));
    s.next_turns[&other].req.put(Ok(r));
    s.test_kernel_signal_states
        .insert(other, KernelSignalState::default());
    s.wake_signaled_guest(other, Signal::SIGCHLD);
    assert!(s.blocked.sigchld_takers.contains(&other));

    assert_eq!(grant_selected(&mut s), reader, "the probe first");
    file_probe_successor(&mut s, reader, 2, false);
    s.step2_sigalrm_ledger();
    assert!(woken_by_sigalrm(&s, reader));
    assert_eq!(grant_selected(&mut s), other, "then the taker");
}

/// Gate item 4: a poll whose readiness the host can change is not reserved,
/// since whether the probe finds data or the wake substitutes would depend
/// on host timing: a poll that is not on guest state only, one whose
/// guest-held marks were revoked, and a named FIFO.
#[test]
fn a_reserved_probe_refuses_a_poll_the_host_can_make_ready() {
    for case in ["host-readiness", "marks-revoked", "named-fifo"] {
        let mut s = liteinst_scheduler();
        let (tid, _, _) = add(&mut s, 100, 100);
        let pid = DetPid::from_raw(100);
        s.set_sigalrm_handled(pid, true);
        let mut r = pipe_read_request(tid, 1);
        match case {
            "host-readiness" => r.polls_guest_state_only = false,
            "marks-revoked" => s.guest_held_marks_revoked = true,
            _ => r.polls_a_named_fifo = true,
        }
        file_request(&mut s, r, false);
        s.runqueue_push_back(tid);

        s.fire_alarm(pid, tid, Signal::SIGALRM);

        assert!(!sigalrm_reserved_for(&s, tid), "{case}");
        assert!(s.sigalrm_loss_recorded(pid), "{case}");
    }
}

/// R1 with the successor production files: the probe completed the read,
/// so `complete_syscall` delivers the due entry and files its
/// `InboundSignal(SIGALRM)` turn. Step 2 classifies that as completed (no
/// wake substituted: the stored request is the delivery's own), and the
/// take in its turn then succeeds, after the classification (G1).
#[test]
fn a_completed_probe_is_classified_before_its_delivery_takes_the_entry() {
    let (mut s, reader, pid, _) = reserved_reader_behind(&[300]);
    assert_eq!(grant_selected(&mut s), reader);
    let mut delivery = Resources::new(reader);
    delivery.insert(
        ResourceID::InboundSignal(SigWrapper::from(Signal::SIGALRM)),
        Permission::W,
    );
    delivery.fyi("deliver-sigalrm");
    file_request(&mut s, delivery, false);

    s.step2_sigalrm_ledger();

    assert_eq!(s.sigalrm_reserved_poll_target(), None);
    let stored = s.next_turns[&reader].req.try_read().unwrap().unwrap();
    assert_eq!(stored.fyi, "deliver-sigalrm", "no wake substituted");
    assert!(s.sigalrm_pending(pid), "delivery remains post-result");
    assert!(s.take_sigalrm(pid, reader), "taken in its turn");
    assert!(!s.sigalrm_pending(pid));
}

/// A first attempt of another lineage on the same descriptor means a new
/// read began after the bound read ended without its due entry's delivery,
/// which the completion path never does with an entry due (design 5.1):
/// classified `NewRead`, charged as a loss, not woken, never mistaken for
/// the still-unrun probe.
#[test]
fn a_new_read_of_the_same_descriptor_charges_the_probe_a_loss() {
    let (mut s, reader, pid, _) = reserved_reader_behind(&[300]);
    assert_eq!(grant_selected(&mut s), reader);
    file_request(&mut s, pipe_read_request(reader, 0), false);

    s.step2_sigalrm_ledger();

    assert_eq!(s.sigalrm_reserved_poll_target(), None, "classified");
    assert!(
        s.sigalrm_loss_recorded(pid),
        "a protocol violation: the loss"
    );
    assert!(!woken_by_sigalrm(&s, reader), "no wake for a new read");
    assert!(s.sigalrm_pending(pid));
}

/// Precedence belongs to the unrun probe only. Once the probe ran and its
/// successor retry is filed but not yet classified (the defensive step-4
/// path: no step 2 in between), step 3 makes the run queue's own choice:
/// the thread queued ahead runs first, and the reader's successor is settled
/// at its own step-4 head when it is reached.
#[test]
fn a_ran_probe_has_no_precedence_before_its_classification() {
    let (mut s, reader, _pid, queued) = reserved_reader_behind(&[300]);
    let other = queued[0];
    assert_eq!(grant_selected(&mut s), reader, "the unrun probe first");
    file_probe_successor(&mut s, reader, 2, false);
    assert!(sigalrm_reserved_for(&s, reader), "not yet classified");
    assert_eq!(s.reserved_probe_to_resume(), None, "the probe ran");

    assert_eq!(grant_selected(&mut s), other, "the run queue's own choice");
}

/// A happens-before program holding the reader (thread 100) at its syscall
/// count 43 until thread 300 reaches its count 42.
fn hb_hold_reader_spec() -> HbRuntime {
    let program = detcore_model::happens_before::HappensBeforeSpec::from_json(
        r#"{"version": 1,
        "events": {"src": {"thread": "300", "syscalls": 42},
                   "dst": {"thread": "100", "syscalls": 43}},
        "edges": [{"before": "src", "after": "dst", "strength": "hard"}]}"#,
    )
    .unwrap()
    .normalize()
    .unwrap();
    HbRuntime::new(program)
}

/// Review of 8f75151f, the happens-before edge: the reader reaches a
/// happens-before hold only through its own checkpoint request, which is a
/// successor of the probe. Driven through step 3 and step 4 with no step 2
/// in between (the defensive path), the step-4 head classifies the
/// reservation completed before the checkpoint parks the thread, so no
/// reservation outlives into a hold.
#[test]
fn a_reserved_probe_never_outlives_into_a_happens_before_hold() {
    let (mut s, reader, pid, queued) = reserved_reader_behind(&[300]);
    let other = queued[0];
    s.happens_before = Some(hb_hold_reader_spec());
    assert_eq!(grant_selected(&mut s), reader, "the unrun probe first");
    // The probe's read completed; its posthook checkpoint (count 43) is the
    // successor, gated on 300's count 42, which has not fired.
    let mut r = Resources::new(reader);
    r.insert(ResourceID::HappensBeforeCheckpoint(43), Permission::W);
    file_request(&mut s, r, false);
    assert!(sigalrm_reserved_for(&s, reader), "not yet classified");
    assert_eq!(s.reserved_probe_to_resume(), None, "the probe ran");
    // Move the reader to the front so step 3 picks it now.
    assert!(s.run_queue.remove_tid(other));
    s.runqueue_push_back(other);
    let (tid, req, resp) = s.step3_peek().expect("a selection");
    assert_eq!(tid, reader);
    let request = req.try_read().unwrap().unwrap();
    assert!(
        s.step4_resource_block(tid, &request, &resp).is_err(),
        "held"
    );
    assert!(s.happens_before.as_ref().unwrap().parked.contains(&reader));
    assert!(!s.run_queue.contains_tid(reader));
    assert_eq!(s.sigalrm_reserved_poll_target(), None, "classified first");
    assert!(!s.sigalrm_loss_recorded(pid));
    assert!(s.sigalrm_pending(pid));
}

/// Review of 8f75151f, the happens-before edge's fail-closed release,
/// constructed directly (no production path is known to leave a reserved
/// target out of the run queue with its probe unrun): when step 3 begins,
/// the reservation is released through the loss path
/// (`release_unqueued_sigalrm_reservation`) instead of keeping a precedence
/// that every turn committed meanwhile would outlive. The entry stays
/// pending for delivery at a completion, and once the gate opens the
/// readmitted reader has no precedence left.
#[test]
fn an_unrun_probe_out_of_the_run_queue_is_released_with_a_loss_at_step3() {
    let (mut s, reader, pid, queued) = reserved_reader_behind(&[300, 500]);
    let (other, third) = (queued[0], queued[1]);
    let mut hb = hb_hold_reader_spec();
    hb.parked.insert(reader);
    s.happens_before = Some(hb);
    assert!(s.run_queue.remove_tid(reader));
    assert_eq!(s.reserved_probe_to_resume(), None, "held: not selectable");
    assert!(!s.sigalrm_loss_recorded(pid));

    assert_eq!(grant_selected(&mut s), other, "another turn commits");
    assert_eq!(s.sigalrm_reserved_poll_target(), None, "released at step 3");
    assert!(s.sigalrm_loss_recorded(pid), "charged as a loss");
    assert!(s.sigalrm_pending(pid), "the entry stays pending");

    s.happens_before.as_mut().unwrap().wake_pending = true;
    assert_eq!(grant_selected(&mut s), third, "no precedence after release");
    assert_eq!(grant_selected(&mut s), other);
    assert_eq!(grant_selected(&mut s), reader);
}
/// A call a KVM thread waits in under a temporary mask, as its request
/// stands where the kernel-sent receiver choice reads it.
#[derive(Clone, Copy, Debug)]
enum TemporaryMaskWait {
    /// A masked `ppoll`'s wait turn (`masked_ppoll_wait_request`).
    Ppoll,
    /// A masked `pselect6`'s probe turn (`handle_internal_pselect6`).
    Pselect6,
    /// A backend-served `rt_sigsuspend`, parked (`backend_rt_sigsuspend`).
    ParkedSuspend,
    /// The same, woken by the step-2 drain for another signal and not yet
    /// granted (`backend_suspend_woken`).
    WokenSuspend,
}

/// File `tid`'s `wait` under the temporary mask `temporary` (`None`: the
/// request says the mask is unknown), queued.
fn file_temporary_mask_wait(
    s: &mut Scheduler,
    tid: DetTid,
    wait: TemporaryMaskWait,
    temporary: Option<u64>,
) {
    let mut request = Resources::new(tid);
    match wait {
        TemporaryMaskWait::Ppoll | TemporaryMaskWait::Pselect6 => {
            request.insert(ResourceID::InternalIOPolling, Permission::W);
            request.fyi(if matches!(wait, TemporaryMaskWait::Ppoll) {
                "ppoll"
            } else {
                "pselect6"
            });
            request.set_signal_interrupt_errno(reverie::syscalls::Errno::EINTR);
        }
        TemporaryMaskWait::ParkedSuspend | TemporaryMaskWait::WokenSuspend => {
            request.insert(
                ResourceID::SleepUntil(LogicalTime::INDEFINITE),
                Permission::W,
            );
            request.fyi(crate::resources::RT_SIGSUSPEND_FYI);
        }
    }
    request.blocked_signal_mask = temporary;
    request.blocked_signal_mask_unknown = temporary.is_none();
    s.next_turns.get_mut(&tid).unwrap().req = Ivar::full(Ok(request));
    if matches!(wait, TemporaryMaskWait::WokenSuspend) {
        // The drain's wake replaces the request and keeps the suspend's mask.
        s.note_backend_suspend_replaced(tid);
        let mut woken = Resources::new(tid);
        woken.insert(
            ResourceID::WaitidSignals(vec![SigWrapper(libc::SIGUSR1)]),
            Permission::W,
        );
        s.next_turns.get_mut(&tid).unwrap().req = Ivar::full(Ok(woken));
    }
    s.runqueue_push_back(tid);
}

/// The Codex review of https://github.com/rrnewton/hermit/pull/5259, P1: on
/// KVM the receiver of a child's exit `SIGCHLD` is chosen by the mask Linux's
/// `blocked` holds, which for a thread waiting in `ppoll`, `pselect6` or
/// `rt_sigsuspend` is the call's temporary mask (`set_user_sigmask`,
/// kernel/signal.c:3272 at v6.17; `sigsuspend`, line 4833; `wants_signal`,
/// line 948), not its own. The creator M (100) waits in each call, under
/// both orientations, beside an eligible sibling W (101) in a pipe read:
/// - its own mask blocks SIGCHLD and the call's lets it in: Linux names M,
///   which wants it, and the queued copy interrupts M's wait;
/// - its own mask lets SIGCHLD in and the call's blocks it: M does not want
///   it, the walk reaches W, and on M the copy would stay pending.
///
/// A temporary mask the request says is unknown makes M no target: W.
/// Mutation "the own mask read" (`thread_signal_mask_for` in
/// `kernel_signal_choice`, the backend's sets in `queued_sigchld_fate`):
/// every row fails, choosing W for M and M for W.
#[test]
fn a_kvm_child_exit_receiver_is_chosen_by_the_temporary_mask_its_call_installs() {
    use super::controlled_child_exit::QueuedSigchld;
    let sigchld = kernel_signal_bit(libc::SIGCHLD);
    for wait in [
        TemporaryMaskWait::Ppoll,
        TemporaryMaskWait::Pselect6,
        TemporaryMaskWait::ParkedSuspend,
        TemporaryMaskWait::WokenSuspend,
    ] {
        for (own_blocks, temporary, expected_m) in [(true, 0, true), (false, sigchld, false)] {
            let case = format!("{wait:?}, own mask blocks SIGCHLD: {own_blocks}");
            let (mut s, backend) = kvm_fixture();
            let (creator, _, _) = add(&mut s, 100, 100);
            let (sibling, _, _) = add(&mut s, 100, 101);
            backend
                .signal_sets
                .lock()
                .unwrap()
                .insert(100, sigchld_sets(own_blocks, false, true));
            backend
                .signal_sets
                .lock()
                .unwrap()
                .insert(101, sigchld_sets(false, false, true));
            file_temporary_mask_wait(&mut s, creator, wait, Some(temporary));
            file_restartable_read(&mut s, sibling);
            assert_eq!(
                s.controlled_temporary_mask(creator),
                Ok(Some(temporary)),
                "{case}"
            );
            let expected = if expected_m { creator } else { sibling };
            assert_eq!(
                s.kernel_signal_choice(DetPid::from_raw(100), creator, libc::SIGCHLD)
                    .0,
                Some(expected),
                "{case}"
            );
            assert_eq!(
                s.queued_sigchld_fate(creator),
                if expected_m {
                    QueuedSigchld::Interrupts
                } else {
                    QueuedSigchld::StaysPending
                },
                "{case}"
            );
        }
        // Unknown: never the own mask, which lets SIGCHLD in here.
        let (mut s, backend) = kvm_fixture();
        let (creator, _, _) = add(&mut s, 100, 100);
        let (sibling, _, _) = add(&mut s, 100, 101);
        backend
            .signal_sets
            .lock()
            .unwrap()
            .insert(100, sigchld_sets(false, false, true));
        backend
            .signal_sets
            .lock()
            .unwrap()
            .insert(101, sigchld_sets(false, false, true));
        file_temporary_mask_wait(&mut s, creator, wait, None);
        file_restartable_read(&mut s, sibling);
        if !matches!(wait, TemporaryMaskWait::WokenSuspend) {
            assert_eq!(
                s.controlled_temporary_mask(creator),
                Err(TemporaryMaskUnknown),
                "{wait:?}"
            );
            assert_eq!(
                s.kernel_signal_choice(DetPid::from_raw(100), creator, libc::SIGCHLD)
                    .0,
                Some(sibling),
                "{wait:?}: unknown temporary mask"
            );
        }
    }
}

/// The Codex review of https://github.com/rrnewton/hermit/pull/5259, P1: an
/// unblocked SIGURG sent to a KVM thread while it is ignored by default is
/// one Linux discards before it is queued (`prepare_signal`,
/// `__send_signal_locked`, kernel/signal.c:935 and 1053 at v6.17), so a
/// handler installed afterwards never sees it, though the backend keeps the
/// copy for the Tool. The parked observation must not count it as the
/// thread's own signal to take (`has_own_signal_to_take`), and a delivery
/// stop that dequeues it delivers nothing
/// (`delivers_a_copy_linux_discarded`). A copy that was blocked when sent was
/// queued (`sig_ignored`, lines 113-114) and is taken under the late handler
/// as before; one still ignored is delivered as before (the kernel ignores
/// it). Mutation "the current disposition alone": the first row counts the
/// discarded copy and delivers it.
#[test]
fn a_kvm_copy_linux_discarded_at_send_is_never_taken_by_a_later_handler() {
    let urg = kernel_signal_bit(libc::SIGURG);
    for (name, blocked_at_send, caught_now, takes, suppressed) in [
        ("unblocked at send, caught now", false, true, false, true),
        ("blocked at send, caught now", true, true, true, false),
        (
            "unblocked at send, still ignored",
            false,
            false,
            false,
            false,
        ),
    ] {
        let (mut s, backend) = kvm_fixture();
        let (_, _, _) = add(&mut s, 100, 100);
        let (waiter, _, _) = add(&mut s, 100, 101);
        file_restartable_read(&mut s, waiter);
        let at_send = if blocked_at_send { urg } else { 0 };
        backend
            .signal_sets
            .lock()
            .unwrap()
            .insert(101, reverie::SignalSets::new(0, 0, at_send, 0, 0));
        // The sibling's send, as the guest hooks record it.
        let token = s.discard_pre_send(
            waiter,
            discarded_at_send::QueueId::Thread(waiter),
            libc::SIGURG,
            None,
        );
        assert_eq!(token.discarded, !blocked_at_send, "{name}");
        s.discard_note_send(token, None, None);
        // The copy is pending for the waiter, unblocked now, and a handler
        // may have been installed since.
        backend.signal_sets.lock().unwrap().insert(
            101,
            reverie::SignalSets::new(urg, 0, 0, 0, if caught_now { urg } else { 0 }),
        );
        assert_eq!(s.has_own_signal_to_take(waiter), takes, "{name}");
        // A delivery stop dequeued it: nothing is pending after.
        backend.signal_sets.lock().unwrap().insert(
            101,
            reverie::SignalSets::new(0, 0, 0, 0, if caught_now { urg } else { 0 }),
        );
        assert_eq!(
            s.delivers_a_copy_linux_discarded(waiter, libc::SIGURG),
            suppressed,
            "{name}"
        );
    }
}

