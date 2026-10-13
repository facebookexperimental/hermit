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
    fail_recipients: Mutex<Option<SignalProcessId>>,
    fail_reservation: std::sync::atomic::AtomicBool,
    failure_processes: Mutex<Vec<SignalProcessId>>,
    failure_probe: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    recipients: Mutex<Vec<SignalRecipient>>,
    publications: Mutex<Vec<(SignalProcessId, reverie::SignalEvent)>>,
    timer_publications: Mutex<Vec<(SignalProcessId, reverie::SignalEvent)>>,
    reject_child_publication: std::sync::atomic::AtomicBool,
    permits: Mutex<Vec<SignalDeliveryPermit>>,
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
        let receipt = ProcessSignalPublication {
            process,
            pending_generation: 0,
            coalesced: false,
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
        self.timer_publications
            .lock()
            .unwrap()
            .push((process, event));
        ProcessSignalPublicationResult::Committed(ProcessSignalPublication {
            process,
            pending_generation: 0,
            coalesced: false,
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
        _: i32,
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
        s.consume_signal_boundary(receipt).unwrap();
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
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: libc::SIGKILL,
        },
    })
    .unwrap();

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
    reserved
        .consume_signal_boundary(SignalBoundaryReceipt {
            permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group: true,
                wait_status: libc::SIGKILL,
            },
        })
        .unwrap();
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

#[test]
fn controlled_exit_mode_never_enqueues_legacy_timed_sigchld() {
    let (mut s, _) = fixture();
    let _ = add(&mut s, 100, 100);
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
