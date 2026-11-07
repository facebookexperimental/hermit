/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! SIGSHARE S2: the scheduler's reader of a guest thread's signal state on a
//! backend whose guest threads are not host tasks
//! (`Scheduler::backend_signal_state`).
//!
//! The scheduler is configured with KVM's capabilities and sequentialized
//! threads, and a scripted run-scoped signal control is installed, so every
//! answer below is the control's. Mutations each assertion catches:
//! - "ptrace asks the backend": the ptrace scheduler must answer `None`, so
//!   its callers keep their `/proc` read;
//! - "wrong task": each thread's answer is scripted per thread ID, and the
//!   control records which task lifetime it was asked about;
//! - "EAGAIN not retried" / "EAGAIN retried forever": one overlap is asked
//!   again, a second is returned;
//! - "an unknown state read as empty": `ESRCH`, `ENOSYS` and an unbound
//!   thread are errors, never an empty state.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use reverie::BackendSignalControl;
use reverie::BackendSignalControlMode;
use reverie::ChildExitCompletion;
use reverie::ChildExitPublicationResult;
use reverie::FactError;
use reverie::ProcessSignalControl;
use reverie::ProcessSignalPublicationResult;
use reverie::SignalDeliveryPermit;
use reverie::SignalProcessId;
use reverie::SignalRecipient;
use reverie::SignalSets;
use reverie::SignalTaskIdentity;

use super::*;

/// A run-scoped signal control that answers `signal_sets` from a script per
/// thread ID: each read takes the next answer, and the last one repeats.
#[derive(Default)]
pub(super) struct ScriptedControl {
    script: Mutex<BTreeMap<i32, VecDeque<Result<SignalSets, FactError>>>>,
    asked: Mutex<Vec<SignalTaskIdentity>>,
}

impl std::fmt::Debug for ScriptedControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedControl").finish_non_exhaustive()
    }
}

impl ScriptedControl {
    /// Answer thread `tid`'s reads with `answers`, in order.
    pub(super) fn script(&self, tid: DetTid, answers: Vec<Result<SignalSets, FactError>>) {
        self.script
            .lock()
            .unwrap()
            .insert(tid.as_raw(), answers.into());
    }

    /// The task lifetimes asked about, in order.
    pub(super) fn asked(&self) -> Vec<SignalTaskIdentity> {
        self.asked.lock().unwrap().clone()
    }
}

impl ProcessSignalControl for ScriptedControl {
    fn publish_alarm(
        &self,
        _process: SignalProcessId,
        _event: reverie::SignalEvent,
    ) -> ProcessSignalPublicationResult {
        ProcessSignalPublicationResult::RejectedBeforeCommit(reverie::Errno::ENOSYS)
    }

    fn publish_child_exit(&self, _completion: ChildExitCompletion) -> ChildExitPublicationResult {
        ChildExitPublicationResult::RejectedBeforeCommit(reverie::Errno::ENOSYS)
    }

    fn signal_recipients(
        &self,
        _process: SignalProcessId,
        _signal: i32,
    ) -> Result<Vec<SignalRecipient>, reverie::Errno> {
        Ok(Vec::new())
    }

    fn enable_orphan_placement(&self) -> Result<(), reverie::Errno> {
        Ok(())
    }

    fn signal_sets(&self, task: SignalTaskIdentity) -> Result<SignalSets, FactError> {
        self.asked.lock().unwrap().push(task);
        let mut script = self.script.lock().unwrap();
        let Some(answers) = script.get_mut(&task.tid.as_raw()) else {
            return Err(FactError::Errno(reverie::Errno::ESRCH));
        };
        if answers.len() > 1 {
            answers.pop_front().unwrap()
        } else {
            answers
                .front()
                .copied()
                .unwrap_or(Err(FactError::NotProvided))
        }
    }

    fn reserve_delivery(&self, _permit: SignalDeliveryPermit) -> Result<(), reverie::Errno> {
        Err(reverie::Errno::ENOSYS)
    }

    fn release_delivery(&self, _permit: SignalDeliveryPermit) -> Result<(), reverie::Errno> {
        Err(reverie::Errno::ENOSYS)
    }

    fn finish_publication_failure(&self, _process: SignalProcessId) -> Result<(), reverie::Errno> {
        Ok(())
    }
}

/// The identity a thread `tid` of process `pid` is bound to: its process's
/// first generation and a task generation equal to its TID, as reverie-kvm
/// numbers neither.
pub(super) fn identity(pid: DetPid, tid: DetTid) -> SignalTaskIdentity {
    SignalTaskIdentity {
        process: SignalProcessId {
            tgid: reverie::Pid::from_raw(pid.as_raw()),
            generation: 1,
        },
        tid: reverie::Pid::from_raw(tid.as_raw()),
        task_generation: 1000 + tid.as_raw() as u64,
    }
}

/// A scheduler with KVM's capabilities and sequentialized threads, with
/// `control` installed when `installed`.
pub(super) fn kvm_scheduler(control: &Arc<ScriptedControl>, installed: bool) -> Scheduler {
    let mut scheduler = Scheduler::new(&Config {
        sequentialize_threads: true,
        max_timeslice: None,
        ..Config::default().with_backend(|backend| *backend = reverie::BackendCapabilities::KVM)
    });
    assert!(scheduler.backend.provides_process_signal_control);
    // SIGSHARE S3b: KVM's kernel signal targets are predictable.
    assert!(scheduler.models_signal_targets);
    if installed {
        assert_eq!(
            scheduler
                .install_signal_control(
                    Some(BackendSignalControl {
                        process: control.clone(),
                    }),
                    true,
                )
                .unwrap(),
            BackendSignalControlMode::ToolControlled
        );
    }
    scheduler
}

/// Register thread `tid` of process `pid` with the scheduler, bound to its
/// task lifetime ([`identity`]) as StartNewThread binds it.
pub(super) fn add_bound_thread(scheduler: &mut Scheduler, pid: DetPid, tid: DetTid) {
    scheduler.thread_tree.add_child(pid, tid, pid == tid);
    scheduler.register_thread_for_test(tid);
    scheduler
        .real_timers
        .bind(pid, tid, MmId::initial(pid), identity(pid, tid))
        .unwrap();
}

/// Signal sets with `blocked` as the mask, and `ignored` and `caught` as the
/// dispositions; nothing pending.
pub(super) fn sets(blocked: u64, ignored: u64, caught: u64) -> SignalSets {
    SignalSets::new(0, 0, blocked, ignored, caught)
}

fn bit(signal: i32) -> u64 {
    kernel_signal_bit(signal)
}

const PROCESS: DetPid = DetPid::from_raw(10);
const LEADER: DetTid = DetTid::from_raw(10);
const SIBLING: DetTid = DetTid::from_raw(11);

#[test]
fn a_backend_whose_threads_are_host_tasks_keeps_its_proc_read() {
    for backend in [
        reverie::BackendCapabilities::PTRACE,
        reverie::BackendCapabilities::LITEINST_IN_GUEST,
    ] {
        let mut scheduler = Scheduler::new(&Config {
            sequentialize_threads: true,
            ..Config::default().with_backend(|capabilities| *capabilities = backend)
        });
        scheduler.thread_tree.add_child(LEADER, LEADER, true);
        scheduler.register_thread_for_test(LEADER);
        assert_eq!(scheduler.backend_signal_state(LEADER), None);
    }
}

#[test]
fn the_reader_answers_each_thread_from_its_own_task_lifetime() {
    let control = Arc::new(ScriptedControl::default());
    let mut scheduler = kvm_scheduler(&control, true);
    add_bound_thread(&mut scheduler, PROCESS, LEADER);
    add_bound_thread(&mut scheduler, PROCESS, SIBLING);
    let leader = SignalSets::new(
        bit(libc::SIGUSR2),
        bit(libc::SIGTERM),
        bit(libc::SIGUSR1),
        bit(libc::SIGWINCH),
        bit(libc::SIGUSR1),
    );
    control.script(LEADER, vec![Ok(leader)]);
    control.script(
        SIBLING,
        vec![Ok(sets(0, bit(libc::SIGWINCH), bit(libc::SIGUSR1)))],
    );

    assert_eq!(
        scheduler.backend_signal_state(LEADER),
        Some(Ok(KernelSignalState {
            pending: bit(libc::SIGUSR2) | bit(libc::SIGTERM),
            thread_pending: bit(libc::SIGUSR2),
            shared_pending: bit(libc::SIGTERM),
            blocked: bit(libc::SIGUSR1),
            ignored: bit(libc::SIGWINCH),
            caught: bit(libc::SIGUSR1),
        }))
    );
    assert_eq!(
        scheduler
            .backend_signal_state(SIBLING)
            .unwrap()
            .unwrap()
            .blocked,
        0
    );
    assert_eq!(
        control.asked(),
        vec![identity(PROCESS, LEADER), identity(PROCESS, SIBLING)]
    );
}

#[test]
fn an_exec_overlapping_the_read_is_asked_once_more() {
    let control = Arc::new(ScriptedControl::default());
    let mut scheduler = kvm_scheduler(&control, true);
    add_bound_thread(&mut scheduler, PROCESS, LEADER);
    let answer = sets(bit(libc::SIGUSR1), 0, 0);
    control.script(
        LEADER,
        vec![Err(FactError::Errno(reverie::Errno::EAGAIN)), Ok(answer)],
    );
    assert_eq!(
        scheduler.backend_signal_state(LEADER),
        Some(Ok(KernelSignalState::from_backend_sets(&answer)))
    );
    assert_eq!(control.asked().len(), 2);

    // Two overlaps in a row are returned, not asked about again.
    let control = Arc::new(ScriptedControl::default());
    let mut scheduler = kvm_scheduler(&control, true);
    add_bound_thread(&mut scheduler, PROCESS, LEADER);
    control.script(
        LEADER,
        vec![
            Err(FactError::Errno(reverie::Errno::EAGAIN)),
            Err(FactError::Errno(reverie::Errno::EAGAIN)),
            Ok(answer),
        ],
    );
    assert_eq!(
        scheduler.backend_signal_state(LEADER),
        Some(Err(reverie::Errno::EAGAIN))
    );
    assert_eq!(control.asked().len(), 2);
}

#[test]
fn an_unknown_state_is_an_error_never_an_empty_state() {
    // The backend names no live task for the lifetime.
    let control = Arc::new(ScriptedControl::default());
    let mut scheduler = kvm_scheduler(&control, true);
    add_bound_thread(&mut scheduler, PROCESS, LEADER);
    control.script(LEADER, vec![Err(FactError::Errno(reverie::Errno::ESRCH))]);
    assert_eq!(
        scheduler.backend_signal_state(LEADER),
        Some(Err(reverie::Errno::ESRCH))
    );

    // The backend does not report the fact.
    control.script(LEADER, vec![Err(FactError::NotProvided)]);
    assert_eq!(
        scheduler.backend_signal_state(LEADER),
        Some(Err(reverie::Errno::ENOSYS))
    );

    // A thread with no bound lifetime: the backend is never asked.
    let asked = control.asked().len();
    scheduler.thread_tree.add_child(PROCESS, SIBLING, false);
    scheduler.register_thread_for_test(SIBLING);
    assert_eq!(
        scheduler.backend_signal_state(SIBLING),
        Some(Err(reverie::Errno::ESRCH))
    );
    assert_eq!(control.asked().len(), asked);

    // No control installed (a run without sequentialized threads).
    let control = Arc::new(ScriptedControl::default());
    let mut scheduler = kvm_scheduler(&control, false);
    scheduler.thread_tree.add_child(LEADER, LEADER, true);
    scheduler.register_thread_for_test(LEADER);
    control.script(LEADER, vec![Ok(sets(0, 0, 0))]);
    assert_eq!(
        scheduler.backend_signal_state(LEADER),
        Some(Err(reverie::Errno::ENOSYS))
    );
    assert!(control.asked().is_empty());
}

// The step-2 drain decision for a restartable internal IO poll on KVM
// (design section 12's matrix, rows 1-9; it replaces
// `the_drain_never_reads_kernel_signal_state_under_process_signal_control`,
// which pinned the keep-every-signal guard this reader retires). Each row
// scripts the target's backend state, sends one signal as a guest's
// cross-task send (`notify_signal_pending`), and runs the drain. Mutations
// each row catches:
// - "guard left in place" (every signal wakes): the must-not-wake rows;
// - "filter by pending instead of mask and disposition": the state has
//   nothing pending, so a pending-based filter wakes nothing;
// - "hold dropped": the SIGCHLD and host-timed rows;
// - "SigBlk used under a published mask": the published-mask row.

/// The poller: thread 100, its own process's leader.
const POLLER: DetTid = DetTid::from_raw(100);

/// A KVM scheduler, with the scripted control installed, whose thread 100
/// polls internally (a `read` of an empty pipe), with the backend state
/// `state` scripted for it. `decides_itself` marks a parked pipe `writev`,
/// and `names_task` stamps the request with the thread's own task, as
/// `guest_signal_state_task` stamps it wherever a reader answers.
fn kvm_poller(
    state: SignalSets,
    decides_itself: bool,
    names_task: bool,
) -> (Scheduler, Arc<ScriptedControl>) {
    let control = Arc::new(ScriptedControl::default());
    let mut scheduler = kvm_scheduler(&control, true);
    // These cases pin S2a's drain filter, which decides from the backend's
    // dispositions where no record of copies Linux discarded at send is
    // kept. Since SIGSHARE S3b KVM keeps that record (`models_signal_targets`),
    // and the sender's turn decides instead
    // (`with_the_record_a_kvm_poll_is_decided_in_the_senders_turn`).
    scheduler.models_signal_targets = false;
    add_bound_thread(&mut scheduler, POLLER, POLLER);
    control.script(POLLER, vec![Ok(state)]);
    let mut polling = Resources::new(POLLER);
    polling.insert(ResourceID::InternalIOPolling, Permission::W);
    polling.set_signal_interrupt_errno(Errno::ERESTARTSYS);
    polling.signal_state_task = names_task.then_some((POLLER.as_raw(), POLLER.as_raw()));
    polling.decides_signal_interruption_itself = decides_itself;
    polling.fyi(if decides_itself { "writev" } else { "read" });
    scheduler.next_turns.get_mut(&POLLER).unwrap().req = Ivar::full(Ok(polling));
    scheduler.runqueue_push_back(POLLER);
    (scheduler, control)
}

/// Send `signal` to the poller and drain; the signals its wake carries
/// (empty when it keeps polling).
fn send_and_drain(scheduler: &mut Scheduler, signal: i32) -> Vec<SigWrapper> {
    scheduler.notify_signal_pending(POLLER, SigWrapper(signal));
    scheduler.drain_pending_cross_task_signals();
    scheduler.inbound_signals(POLLER)
}

/// The poller still holds its polling request, unchanged, in the run queue.
fn keeps_polling(scheduler: &Scheduler) -> bool {
    let request = scheduler
        .next_turns
        .get(&POLLER)
        .and_then(|turn| turn.req.try_read())
        .and_then(Result::ok)
        .expect("the poller holds a request");
    scheduler.run_queue.contains_tid(POLLER)
        && request.resources.len() == 1
        && request
            .resources
            .contains_key(&ResourceID::InternalIOPolling)
}

fn woke_with(scheduler: &Scheduler, signals: &[i32]) -> bool {
    let request = scheduler
        .next_turns
        .get(&POLLER)
        .and_then(|turn| turn.req.try_read())
        .and_then(Result::ok)
        .expect("the poller holds a request");
    scheduler.run_queue.contains_tid(POLLER)
        && request.signal_interrupt_errno() == Some(Errno::ERESTARTSYS.into_raw())
        && scheduler.inbound_signals(POLLER)
            == signals
                .iter()
                .map(|signal| SigWrapper(*signal))
                .collect::<Vec<_>>()
}

#[test]
fn a_blocked_signal_does_not_wake_a_kvm_poll() {
    // Caught, but blocked by the thread's mask.
    let (mut scheduler, control) =
        kvm_poller(sets(bit(libc::SIGUSR1), 0, bit(libc::SIGUSR1)), false, true);
    assert!(send_and_drain(&mut scheduler, libc::SIGUSR1).is_empty());
    assert!(keeps_polling(&scheduler));
    assert_eq!(control.asked(), vec![identity(POLLER, POLLER)]);
}

#[test]
fn an_ignored_signal_does_not_wake_a_kvm_poll() {
    let (mut scheduler, _) = kvm_poller(sets(0, bit(libc::SIGUSR1), 0), false, true);
    assert!(send_and_drain(&mut scheduler, libc::SIGUSR1).is_empty());
    assert!(keeps_polling(&scheduler));
}

#[test]
fn a_default_ignored_signal_does_not_wake_a_kvm_poll() {
    for signal in [libc::SIGCHLD, libc::SIGWINCH, libc::SIGURG, libc::SIGCONT] {
        let (mut scheduler, _) = kvm_poller(sets(0, 0, 0), false, true);
        assert!(
            send_and_drain(&mut scheduler, signal).is_empty(),
            "signal {signal}"
        );
        assert!(keeps_polling(&scheduler), "signal {signal}");
    }
}

#[test]
fn a_caught_signal_wakes_a_kvm_poll() {
    let (mut scheduler, _) = kvm_poller(sets(0, 0, bit(libc::SIGUSR1)), false, true);
    send_and_drain(&mut scheduler, libc::SIGUSR1);
    assert!(woke_with(&scheduler, &[libc::SIGUSR1]));
    // The wake keeps the request's task, so a later batch is filtered by the
    // same thread's state.
    let request = scheduler.next_turns[&POLLER]
        .req
        .try_read()
        .unwrap()
        .unwrap();
    assert_eq!(
        request.signal_state_task,
        Some((POLLER.as_raw(), POLLER.as_raw()))
    );
    assert!(!request.decides_signal_interruption_itself);
}

#[test]
fn a_default_fatal_signal_wakes_a_kvm_poll() {
    let (mut scheduler, _) = kvm_poller(sets(0, 0, 0), false, true);
    send_and_drain(&mut scheduler, libc::SIGTERM);
    assert!(woke_with(&scheduler, &[libc::SIGTERM]));
}

#[test]
fn held_signals_do_not_wake_a_kvm_poll() {
    // A caught SIGCHLD is held, as for a futex waiter (`futex_wait_held_signals`).
    let (mut scheduler, _) = kvm_poller(sets(0, 0, bit(libc::SIGCHLD)), false, true);
    assert!(send_and_drain(&mut scheduler, libc::SIGCHLD).is_empty());
    assert!(keeps_polling(&scheduler));
    // So is a caught signal a host-timed source armed by the process can post.
    let (mut scheduler, _) = kvm_poller(sets(0, 0, bit(libc::SIGUSR2)), false, true);
    scheduler.record_host_timed_signals(POLLER, HostTimedSignalScope::Caller, bit(libc::SIGUSR2));
    assert!(send_and_drain(&mut scheduler, libc::SIGUSR2).is_empty());
    assert!(keeps_polling(&scheduler));
}

#[test]
fn a_kvm_poll_is_filtered_by_the_mask_it_published() {
    // The poll blocks every blockable signal in place and published the
    // guest's own mask, which leaves SIGUSR1 unblocked.
    let (mut scheduler, _) = kvm_poller(sets(!0, 0, bit(libc::SIGUSR1)), false, true);
    assert!(scheduler.set_saved_guest_sigmask(POLLER, Some(0)));
    send_and_drain(&mut scheduler, libc::SIGUSR1);
    assert!(woke_with(&scheduler, &[libc::SIGUSR1]));
    // And the published mask blocks one the backend's mask lets through.
    let (mut scheduler, _) = kvm_poller(sets(0, 0, bit(libc::SIGUSR1)), false, true);
    assert!(scheduler.set_saved_guest_sigmask(POLLER, Some(bit(libc::SIGUSR1))));
    assert!(send_and_drain(&mut scheduler, libc::SIGUSR1).is_empty());
    assert!(keeps_polling(&scheduler));
}

#[test]
fn an_already_woken_kvm_poll_merges_only_signals_that_interrupt_it() {
    let (mut scheduler, _) = kvm_poller(
        sets(
            bit(libc::SIGUSR2),
            0,
            bit(libc::SIGUSR1) | bit(libc::SIGUSR2),
        ),
        false,
        true,
    );
    send_and_drain(&mut scheduler, libc::SIGUSR1);
    assert!(woke_with(&scheduler, &[libc::SIGUSR1]));
    // A blocked one sent to the woken poll is not added to its wake.
    send_and_drain(&mut scheduler, libc::SIGUSR2);
    assert!(woke_with(&scheduler, &[libc::SIGUSR1]));
    // A caught one is.
    send_and_drain(&mut scheduler, libc::SIGTERM);
    assert!(woke_with(&scheduler, &[libc::SIGUSR1, libc::SIGTERM]));
}

#[test]
fn a_kvm_writev_that_decides_for_itself_is_not_filtered() {
    // A parked pipe writev polls under a private mask and checks the
    // disposition itself after the wake, on every backend: the drain keeps
    // every signal for it and never asks the backend.
    let (mut scheduler, control) =
        kvm_poller(sets(bit(libc::SIGUSR1), 0, bit(libc::SIGUSR1)), true, true);
    send_and_drain(&mut scheduler, libc::SIGUSR1);
    assert!(woke_with(&scheduler, &[libc::SIGUSR1]));
    assert!(control.asked().is_empty());
}

#[test]
fn a_kvm_poll_whose_state_is_unknown_keeps_every_signal_it_does_not_hold() {
    // The backend no longer has the task (it exited): the wake is what it was
    // before this reader, and the woken call decides.
    let (mut scheduler, control) =
        kvm_poller(sets(bit(libc::SIGUSR1), 0, bit(libc::SIGUSR1)), false, true);
    control.script(POLLER, vec![Err(FactError::Errno(reverie::Errno::ESRCH))]);
    send_and_drain(&mut scheduler, libc::SIGUSR1);
    assert!(woke_with(&scheduler, &[libc::SIGUSR1]));
    // A held signal stays held.
    assert!(send_and_drain(&mut scheduler, libc::SIGCHLD) == vec![SigWrapper(libc::SIGUSR1)]);

    // A request that names no task: the backend is not asked.
    let (mut scheduler, control) = kvm_poller(
        sets(bit(libc::SIGUSR1), 0, bit(libc::SIGUSR1)),
        false,
        false,
    );
    send_and_drain(&mut scheduler, libc::SIGUSR1);
    assert!(woke_with(&scheduler, &[libc::SIGUSR1]));
    assert!(control.asked().is_empty());
}

#[test]
fn the_kvm_drain_never_reads_host_proc() {
    use std::sync::atomic::Ordering;
    // With the control installed the backend answers; without it (a run
    // without sequentialized threads) nothing does, and every signal the
    // wait does not hold wakes the poll, as before this reader. In neither
    // case is a host task's `/proc` state read.
    for installed in [true, false] {
        let control = Arc::new(ScriptedControl::default());
        let mut scheduler = kvm_scheduler(&control, installed);
        add_bound_thread(&mut scheduler, POLLER, POLLER);
        control.script(
            POLLER,
            vec![Ok(sets(bit(libc::SIGUSR1), 0, bit(libc::SIGUSR1)))],
        );
        let mut polling = Resources::new(POLLER);
        polling.insert(ResourceID::InternalIOPolling, Permission::W);
        polling.set_signal_interrupt_errno(Errno::ERESTARTSYS);
        polling.signal_state_task = Some((POLLER.as_raw(), POLLER.as_raw()));
        scheduler.next_turns.get_mut(&POLLER).unwrap().req = Ivar::full(Ok(polling));
        scheduler.runqueue_push_back(POLLER);
        let woke = send_and_drain(&mut scheduler, libc::SIGUSR1);
        assert_eq!(
            scheduler
                .test_internal_io_signal_state_reads
                .load(Ordering::Relaxed),
            0,
            "installed {installed}"
        );
        assert_eq!(woke.is_empty(), installed, "installed {installed}");
    }
}

// SIGSHARE S3a: every scheduler reader of a guest thread's signal state goes
// through one reader (`Scheduler::guest_signal_state`,
// `guest_thread_signal_state`, `guest_thread_is_gone`), which asks the backend
// where Detcore reads it there and host `/proc` elsewhere, exactly as before.
// Mutations each test catches:
// - "a converted reader keeps its `/proc` read on KVM": the host reads are
//   refused in these KVM tests (the latch), so a `/proc` read answers ENOSYS
//   and never the scripted sets;
// - "the reader keys the backend on the host task numbers": the host numbers
//   passed are ones no guest thread has, and the control records which
//   lifetime it was asked about;
// - "ptrace asks the backend": the ptrace scheduler answers from the `/proc`
//   seam, with the numbers the caller passed, and never from a control;
// - "an unreadable thread taken as gone" / "a gone thread taken as
//   unreadable": only ESRCH is gone.

/// Host numbers no guest thread has, which a `/proc` read would open.
const HOST_PID: i32 = i32::MAX - 1;
const HOST_TID: i32 = i32::MAX;

#[test]
fn every_scheduler_reader_asks_the_backend_on_kvm() {
    crate::syscalls::refuse_host_signal_state_reads(&reverie::BackendCapabilities::KVM);
    let control = Arc::new(ScriptedControl::default());
    let mut scheduler = kvm_scheduler(&control, true);
    add_bound_thread(&mut scheduler, PROCESS, LEADER);
    add_bound_thread(&mut scheduler, PROCESS, SIBLING);
    let leader = SignalSets::new(
        bit(libc::SIGUSR2),
        bit(libc::SIGTERM),
        bit(libc::SIGUSR1),
        bit(libc::SIGWINCH),
        bit(libc::SIGHUP),
    );
    control.script(LEADER, vec![Ok(leader)]);
    control.script(SIBLING, vec![Ok(sets(bit(libc::SIGINT), 0, 0))]);
    let expected = KernelSignalState::from_backend_sets(&leader);

    assert_eq!(
        scheduler.guest_signal_state(LEADER, HOST_PID, HOST_TID),
        Ok(expected)
    );
    assert_eq!(
        scheduler.guest_thread_signal_state(LEADER, HOST_PID, HOST_TID),
        Ok(expected)
    );
    assert!(!scheduler.guest_thread_is_gone(LEADER, HOST_PID, HOST_TID));
    // The wrappers the scheduler's decisions call.
    assert_eq!(
        scheduler.read_thread_blocked_mask_result(SIBLING),
        Ok(bit(libc::SIGINT))
    );
    assert_eq!(
        scheduler.read_shared_pending(LEADER),
        Some(bit(libc::SIGTERM))
    );
    // A process-wide `/proc/<pid>/status` field is the leader's.
    assert_eq!(
        scheduler.signal_mask_field(PROCESS, None, "SigIgn"),
        Some(bit(libc::SIGWINCH))
    );
    assert_eq!(
        scheduler.signal_mask_field(PROCESS, None, "SigCgt"),
        Some(bit(libc::SIGHUP))
    );
    assert_eq!(
        scheduler.signal_mask_field(PROCESS, Some(SIBLING), "SigBlk"),
        Some(bit(libc::SIGINT))
    );
    assert_eq!(scheduler.signal_mask_field(PROCESS, None, "SigQ"), None);
    assert!(
        control
            .asked()
            .iter()
            .all(|task| task.tid.as_raw() == LEADER.as_raw()
                || task.tid.as_raw() == SIBLING.as_raw()),
        "asked {:?}",
        control.asked()
    );
}

#[test]
fn only_a_task_the_backend_does_not_know_is_gone_on_kvm() {
    crate::syscalls::refuse_host_signal_state_reads(&reverie::BackendCapabilities::KVM);
    let control = Arc::new(ScriptedControl::default());
    let mut scheduler = kvm_scheduler(&control, true);
    add_bound_thread(&mut scheduler, PROCESS, LEADER);
    control.script(LEADER, vec![Err(FactError::Errno(reverie::Errno::ESRCH))]);
    assert!(scheduler.guest_thread_is_gone(LEADER, HOST_PID, HOST_TID));
    assert_eq!(
        scheduler.guest_signal_state(LEADER, HOST_PID, HOST_TID),
        Err(reverie::Errno::ESRCH)
    );
    // Unreadable, but not gone.
    control.script(LEADER, vec![Err(FactError::NotProvided)]);
    assert!(!scheduler.guest_thread_is_gone(LEADER, HOST_PID, HOST_TID));
    control.script(LEADER, vec![Err(FactError::Errno(reverie::Errno::EIO))]);
    assert!(!scheduler.guest_thread_is_gone(LEADER, HOST_PID, HOST_TID));
    assert_eq!(
        scheduler.guest_signal_state(LEADER, HOST_PID, HOST_TID),
        Err(reverie::Errno::EIO)
    );
    // A thread with no bound lifetime names no task of the run.
    scheduler.thread_tree.add_child(PROCESS, SIBLING, false);
    scheduler.register_thread_for_test(SIBLING);
    assert!(scheduler.guest_thread_is_gone(SIBLING, HOST_PID, HOST_TID));
    // A process with no thread the backend can read reports no shared queue.
    assert_eq!(scheduler.read_process_shared_pending(PROCESS), None);
}

#[test]
fn ptrace_scheduler_readers_keep_their_proc_read() {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::syscalls::signal_state_read_seam;

    let mut scheduler = Scheduler::new(&Config {
        sequentialize_threads: true,
        ..Config::default()
    });
    scheduler.thread_tree.add_child(LEADER, LEADER, true);
    scheduler.register_thread_for_test(LEADER);
    let opened = Rc::new(RefCell::new(Vec::new()));
    let state = KernelSignalState {
        blocked: bit(libc::SIGUSR1),
        ..KernelSignalState::default()
    };
    let _seam = {
        let opened = opened.clone();
        signal_state_read_seam::install(move |pid, tid| {
            opened.borrow_mut().push((pid.as_raw(), tid.as_raw()));
            Some(Ok(state))
        })
    };
    assert_eq!(
        scheduler.guest_signal_state(LEADER, HOST_PID, HOST_TID),
        Ok(state)
    );
    assert_eq!(
        scheduler.guest_thread_signal_state(LEADER, HOST_PID, HOST_TID),
        Ok(state)
    );
    assert_eq!(
        *opened.borrow(),
        vec![(HOST_PID, HOST_TID), (HOST_PID, HOST_TID)],
        "the /proc read opens the host task the caller names"
    );
}
