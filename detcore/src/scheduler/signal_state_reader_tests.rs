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
    assert!(!scheduler.models_signal_targets);
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
