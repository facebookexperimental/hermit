/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! SIGSHARE S2b: `rt_sigsuspend` waits that the backend's own call serves
//! (`backend_signal_waits`), on a scheduler configured with KVM's
//! capabilities and a scripted run-scoped signal control, so every state is
//! the control's answer. Each row says which mutation it catches.

use reverie::FactError;
use reverie::SignalDeliveryPermit;
use reverie::SignalSets;

use super::signal_state_reader_tests::ScriptedControl;
use super::signal_state_reader_tests::add_bound_thread;
use super::signal_state_reader_tests::identity;
use super::signal_state_reader_tests::kvm_scheduler;
use super::*;
use crate::resources::RT_SIGSUSPEND_FYI;

const PROCESS: DetPid = DetPid::from_raw(20);
const WAITER: DetTid = DetTid::from_raw(20);
const SIBLING: DetTid = DetTid::from_raw(21);
const CHILD: DetTid = DetTid::from_raw(30);

fn bit(signal: i32) -> u64 {
    kernel_signal_bit(signal)
}

/// Signal sets: `pending` on the thread's queue, `shared` on its process's,
/// the thread's own mask `blocked`, and the dispositions.
fn state(pending: u64, shared: u64, blocked: u64, ignored: u64, caught: u64) -> SignalSets {
    SignalSets::new(pending, shared, blocked, ignored, caught)
}

/// Where a filed wait is when the drain looks at it.
#[derive(Clone, Copy, Debug)]
enum Place {
    /// Admitted to the timed waiters (an indefinite park).
    Parked,
    /// Filed, still in the run queue.
    Queued,
}

fn suspend_request(thread: DetTid, mask: u64) -> Resources {
    let mut request = Resources::new(thread);
    request.insert(
        ResourceID::SleepUntil(LogicalTime::INDEFINITE),
        Permission::W,
    );
    request.fyi(RT_SIGSUSPEND_FYI);
    request.blocked_signal_mask = Some(mask);
    request
}

/// A masked ppoll's polling request, as `backend_masked_ppoll_wait` files it,
/// but carrying its temporary mask, so only the request's kind can keep it
/// out of the scan.
fn masked_poll_request(thread: DetTid, mask: u64) -> Resources {
    let mut request = Resources::new(thread);
    request.insert(ResourceID::InternalIOPolling, Permission::W);
    request.fyi("ppoll");
    request.set_signal_interrupt_errno(Errno::EINTR);
    request.blocked_signal_mask = Some(mask);
    request
}

fn file(scheduler: &mut Scheduler, thread: DetTid, request: Resources, place: Place) {
    scheduler.next_turns.get_mut(&thread).unwrap().req = Ivar::full(Ok(request));
    match place {
        Place::Parked => {
            scheduler
                .blocked
                .timed_waiters
                .insert(LogicalTime::INDEFINITE, thread);
        }
        Place::Queued => {
            scheduler.runqueue_push_back(thread);
        }
    }
}

/// A KVM scheduler with the scripted control, the waiter (its process's
/// leader) and a sibling thread bound, and the waiter's state scripted.
fn kvm(waiter_state: Result<SignalSets, FactError>) -> (Scheduler, Arc<ScriptedControl>) {
    let control = Arc::new(ScriptedControl::default());
    let mut scheduler = kvm_scheduler(&control, true);
    add_bound_thread(&mut scheduler, PROCESS, WAITER);
    add_bound_thread(&mut scheduler, PROCESS, SIBLING);
    control.script(WAITER, vec![waiter_state]);
    (scheduler, control)
}

/// A KVM scheduler whose waiter is parked in a suspend under `mask` with
/// backend state `sets`.
fn parked_suspend(mask: u64, sets: SignalSets) -> (Scheduler, Arc<ScriptedControl>) {
    let (mut scheduler, control) = kvm(Ok(sets));
    file(
        &mut scheduler,
        WAITER,
        suspend_request(WAITER, mask),
        Place::Parked,
    );
    (scheduler, control)
}

/// The signals `thread` was woken with: its request is the signal set at
/// the front of the run queue, out of the timed waiters; `None` if it is not.
fn woken(scheduler: &Scheduler, thread: DetTid) -> Option<Vec<i32>> {
    let request = scheduler.next_turns.get(&thread)?.req.try_read()?.ok()?;
    let Some((ResourceID::WaitidSignals(signals), _)) = request.resources.iter().next() else {
        return None;
    };
    assert!(scheduler.run_queue.contains_tid(thread));
    assert!(
        scheduler
            .blocked
            .timed_waiters
            .thread_deadline(thread)
            .is_none()
    );
    Some(signals.iter().map(|signal| signal.raw()).collect())
}

/// `thread` still holds its wait, unchanged, where it was.
fn still_waits(scheduler: &Scheduler, thread: DetTid) -> bool {
    scheduler.backend_suspend_mask(thread).is_some()
        && woken(scheduler, thread).is_none()
        && scheduler.terminal_deadlock.is_none()
}

/// Mutations: "temporary mask ignored" (the own mask decides), "pending not
/// read" (an empty state wakes), "ignored ones left" (an ignored or
/// default-ignored signal the mask lets in, which Linux discarded when it was
/// sent, does not wake the waiter to remove it), "stops not counted". Each
/// row is checked parked in the timed waiters and still queued.
#[test]
fn a_parked_suspend_ends_exactly_for_a_signal_its_temporary_mask_lets_in() {
    let usr1 = bit(libc::SIGUSR1);
    let rows: [(&str, u64, SignalSets, Option<Vec<i32>>); 11] = [
        (
            "caught, blocked outside the call only",
            0,
            state(usr1, 0, usr1, 0, usr1),
            Some(vec![libc::SIGUSR1]),
        ),
        (
            "caught, process-directed",
            0,
            state(0, usr1, usr1, 0, usr1),
            Some(vec![libc::SIGUSR1]),
        ),
        (
            "blocked by the temporary mask",
            usr1,
            state(usr1, 0, 0, 0, usr1),
            None,
        ),
        ("nothing pending", 0, state(0, 0, usr1, 0, usr1), None),
        (
            "SIG_IGN (to remove)",
            0,
            state(usr1, 0, usr1, usr1, 0),
            Some(vec![libc::SIGUSR1]),
        ),
        (
            "default-ignored SIGWINCH (to remove)",
            0,
            state(bit(libc::SIGWINCH), 0, bit(libc::SIGWINCH), 0, 0),
            Some(vec![libc::SIGWINCH]),
        ),
        (
            "default-ignored SIGWINCH blocked by the temporary mask",
            bit(libc::SIGWINCH),
            state(bit(libc::SIGWINCH), 0, bit(libc::SIGWINCH), 0, 0),
            None,
        ),
        (
            "default-ignored SIGURG and SIGCONT (to remove)",
            0,
            state(bit(libc::SIGURG) | bit(libc::SIGCONT), 0, 0, 0, 0),
            Some(vec![libc::SIGCONT, libc::SIGURG]),
        ),
        (
            "default-fatal SIGTERM",
            0,
            state(bit(libc::SIGTERM), 0, bit(libc::SIGTERM), 0, 0),
            Some(vec![libc::SIGTERM]),
        ),
        (
            "default stop SIGTSTP (the woken call refuses it by name)",
            0,
            state(bit(libc::SIGTSTP), 0, bit(libc::SIGTSTP), 0, 0),
            Some(vec![libc::SIGTSTP]),
        ),
        (
            "a SIGCHLD a guest sent, no child, caught",
            0,
            state(bit(libc::SIGCHLD), 0, 0, 0, bit(libc::SIGCHLD)),
            Some(vec![libc::SIGCHLD]),
        ),
    ];
    for (name, mask, sets, expected) in rows {
        for place in [Place::Parked, Place::Queued] {
            let (mut scheduler, control) = kvm(Ok(sets));
            file(&mut scheduler, WAITER, suspend_request(WAITER, mask), place);
            scheduler.wake_backend_signal_waits();
            assert_eq!(woken(&scheduler, WAITER), expected, "{name} {place:?}");
            if expected.is_none() {
                assert!(still_waits(&scheduler, WAITER), "{name} {place:?}");
            }
            // The waiter's own task lifetime, once per scan.
            assert_eq!(control.asked(), vec![identity(PROCESS, WAITER)], "{name}");
        }
    }
}

/// Mutation "SIGCHLD counted with a child" or "fallback missing": in a
/// process with a child whose exit sends SIGCHLD, a pending SIGCHLD never
/// ends a wait (a child's exit is S4's), and a suspend whose temporary mask
/// lets in a caught SIGCHLD is woken with SIGCHLD whatever is pending, so its
/// turn takes the decline it had before S2b.
#[test]
fn with_a_sigchld_child_a_suspend_that_lets_in_a_caught_sigchld_takes_the_child_exit_path() {
    let chld = bit(libc::SIGCHLD);
    for (name, mask, sets, expected) in [
        (
            "caught, let in, nothing pending",
            0,
            state(0, 0, chld, 0, chld),
            Some(vec![libc::SIGCHLD]),
        ),
        (
            "caught, let in, pending",
            0,
            state(0, chld, chld, 0, chld),
            Some(vec![libc::SIGCHLD]),
        ),
        (
            "caught, blocked by the temporary mask, pending",
            chld,
            state(0, chld, 0, 0, chld),
            None,
        ),
        (
            "default disposition, pending",
            0,
            state(0, chld, 0, 0, 0),
            None,
        ),
    ] {
        let (mut scheduler, _) = kvm(Ok(sets));
        scheduler.thread_tree.add_child(WAITER, CHILD, true);
        assert!(scheduler.has_sigchld_child(PROCESS), "{name}");
        file(
            &mut scheduler,
            WAITER,
            suspend_request(WAITER, mask),
            Place::Parked,
        );
        scheduler.wake_backend_signal_waits();
        assert_eq!(woken(&scheduler, WAITER), expected, "{name}");
    }
}

/// Mutation "host-timed signals counted": a signal a host-timed source armed
/// by a guest can post (`record_host_timed_signals`) never ends the wait.
#[test]
fn a_host_timed_signal_does_not_end_a_backend_wait() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut scheduler, _) = parked_suspend(0, state(usr1, 0, usr1, 0, usr1));
    scheduler.record_host_timed_signals(WAITER, HostTimedSignalScope::Caller, usr1);
    scheduler.wake_backend_signal_waits();
    assert!(still_waits(&scheduler, WAITER));
}

/// Mutation "a thread gone is woken" / "an unreadable state is guessed":
/// ESRCH leaves the wait to the thread's retirement, quietly; any other
/// failure ends the run with a named diagnostic and wakes nothing.
#[test]
fn a_gone_waiter_is_left_and_an_unreadable_one_ends_the_run() {
    let (mut scheduler, _) = kvm(Err(FactError::Errno(reverie::Errno::ESRCH)));
    file(
        &mut scheduler,
        WAITER,
        suspend_request(WAITER, 0),
        Place::Parked,
    );
    scheduler.wake_backend_signal_waits();
    assert!(still_waits(&scheduler, WAITER));

    let (mut scheduler, _) = kvm(Err(FactError::NotProvided));
    file(
        &mut scheduler,
        WAITER,
        suspend_request(WAITER, 0),
        Place::Parked,
    );
    scheduler.wake_backend_signal_waits();
    assert!(woken(&scheduler, WAITER).is_none());
    let reason = scheduler.terminal_deadlock.clone().expect("a diagnostic");
    assert!(reason.contains("HERMIT_DEADLOCK"), "{reason}");
    assert!(reason.contains(&format!("dettid {WAITER}")), "{reason}");
}

/// Mutation "permits ignored": while a thread of the waiter's process holds
/// an observed parked wait's delivery permit, whose host thread may take a
/// signal of the process without a turn, the process's waits are not
/// decided, and its state is not read; once the permit settles, the next
/// drain decides.
#[test]
fn an_observation_permit_in_the_process_defers_the_decision() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut scheduler, control) = parked_suspend(0, state(0, usr1, usr1, 0, usr1));
    let task = identity(PROCESS, SIBLING);
    scheduler.parked.permits.insert(
        SIBLING,
        SignalDeliveryPermit {
            task,
            sequence: 1,
            site: Some(reverie::CallbackSignalSite {
                process: task.process,
                tid: task.tid,
                task_generation: task.task_generation,
                callback_nonce: 1,
                boundary_nonce: 1,
            }),
        },
    );
    scheduler.wake_backend_signal_waits();
    assert!(still_waits(&scheduler, WAITER));
    assert!(control.asked().is_empty());
    scheduler.parked.permits.remove(&SIBLING);
    scheduler.wake_backend_signal_waits();
    assert_eq!(woken(&scheduler, WAITER), Some(vec![libc::SIGUSR1]));
}

/// Mutation "every permit defers": a sibling's ordinary boundary permit
/// (`site` None) was taken inside its own serial grant, before it filed the
/// request that let the drain run, so it does not hold the decision back.
/// Counting it held a suspended thread forever behind a sibling that signals
/// itself in a loop (`run_kvm_signal_permits_are_per_thread`,
/// `alarm-sigsuspend`).
#[test]
fn an_ordinary_boundary_permit_does_not_defer_the_decision() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut scheduler, _) = parked_suspend(0, state(0, usr1, usr1, 0, usr1));
    scheduler.parked.permits.insert(
        SIBLING,
        SignalDeliveryPermit {
            task: identity(PROCESS, SIBLING),
            sequence: 1,
            site: None,
        },
    );
    scheduler.wake_backend_signal_waits();
    assert_eq!(woken(&scheduler, WAITER), Some(vec![libc::SIGUSR1]));
}

/// Mutation "a dead thread is requeued": a logically killed waiter is not
/// decided, nor read.
#[test]
fn a_logically_killed_waiter_is_not_woken() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut scheduler, control) = parked_suspend(0, state(usr1, 0, usr1, 0, usr1));
    assert!(scheduler.backend.needs_killed_thread_rpc_cancellation);
    scheduler.logically_killed_threads.insert(WAITER);
    scheduler.wake_backend_signal_waits();
    assert!(woken(&scheduler, WAITER).is_none());
    assert!(control.asked().is_empty());
}

/// Mutation "a woken waiter is woken again": once woken, the request is the
/// signal set, not a wait, so the next drain neither reads nor touches it.
#[test]
fn a_woken_waiter_is_not_decided_again() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut scheduler, control) = parked_suspend(0, state(usr1, 0, usr1, 0, usr1));
    scheduler.wake_backend_signal_waits();
    assert_eq!(woken(&scheduler, WAITER), Some(vec![libc::SIGUSR1]));
    assert_eq!(scheduler.backend_suspend_mask(WAITER), None);
    scheduler.wake_backend_signal_waits();
    assert_eq!(woken(&scheduler, WAITER), Some(vec![libc::SIGUSR1]));
    assert_eq!(control.asked().len(), 1);
}

/// Mutation "polls decided here": a masked ppoll that waits is left to its
/// own zero-timeout probes, each run in its own turn with the temporary mask
/// applied inside the backend's call; the scan neither reads its state nor
/// wakes it, even when a signal its temporary mask lets in is pending.
#[test]
fn a_masked_poll_is_left_to_its_own_probes() {
    let usr1 = bit(libc::SIGUSR1);
    for place in [Place::Parked, Place::Queued] {
        let (mut scheduler, control) = kvm(Ok(state(usr1, 0, usr1, 0, usr1)));
        file(
            &mut scheduler,
            WAITER,
            masked_poll_request(WAITER, 0),
            place,
        );
        assert_eq!(scheduler.backend_suspend_mask(WAITER), None, "{place:?}");
        scheduler.wake_backend_signal_waits();
        assert!(woken(&scheduler, WAITER).is_none(), "{place:?}");
        assert!(control.asked().is_empty(), "{place:?}");
    }
}

/// Mutation "a claim rule": every waiter whose state shows a signal that ends
/// its wait is woken, in DetTid order; a shared signal wakes each of them,
/// and whichever runs the backend's call first takes it (the others park
/// again in their own turns).
#[test]
fn every_waiter_a_shared_signal_ends_is_woken() {
    let usr1 = bit(libc::SIGUSR1);
    let control = Arc::new(ScriptedControl::default());
    let mut scheduler = kvm_scheduler(&control, true);
    add_bound_thread(&mut scheduler, PROCESS, WAITER);
    add_bound_thread(&mut scheduler, PROCESS, SIBLING);
    for thread in [WAITER, SIBLING] {
        control.script(thread, vec![Ok(state(0, usr1, usr1, 0, usr1))]);
        file(
            &mut scheduler,
            thread,
            suspend_request(thread, 0),
            Place::Parked,
        );
    }
    scheduler.wake_backend_signal_waits();
    for thread in [WAITER, SIBLING] {
        assert_eq!(woken(&scheduler, thread), Some(vec![libc::SIGUSR1]));
    }
    assert_eq!(
        control.asked(),
        vec![identity(PROCESS, WAITER), identity(PROCESS, SIBLING)]
    );
}

/// Mutations "the gate decides here" / "ptrace decides here": only a backend
/// that reports the guest's signal state, outside the in-guest delivery
/// gate, has waits this module decides; elsewhere the scan reads nothing.
#[test]
fn only_a_backend_that_reports_signal_state_has_backend_waits() {
    for (backend, gate) in [
        (reverie::BackendCapabilities::PTRACE, false),
        (reverie::BackendCapabilities::LITEINST_IN_GUEST, false),
        (reverie::BackendCapabilities::KVM, true),
    ] {
        let mut scheduler = Scheduler::new(&Config {
            sequentialize_threads: true,
            ..Config::default().with_backend(|capabilities| *capabilities = backend)
        });
        scheduler.in_guest_signal_delivery = gate;
        scheduler.register_thread_for_test(WAITER);
        file(
            &mut scheduler,
            WAITER,
            suspend_request(WAITER, 0),
            Place::Parked,
        );
        assert_eq!(scheduler.backend_suspend_mask(WAITER), None, "gate {gate}");
        scheduler.wake_backend_signal_waits();
        assert!(woken(&scheduler, WAITER).is_none(), "gate {gate}");
    }
}

/// Mutation "the drain does not scan": the end of the step-2 drain ends the
/// wait (the one call site).
#[test]
fn the_step2_drain_ends_a_backend_wait() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut scheduler, _) = parked_suspend(0, state(usr1, 0, usr1, 0, usr1));
    scheduler.step2_drain_prefix_for_test();
    assert_eq!(woken(&scheduler, WAITER), Some(vec![libc::SIGUSR1]));
}

/// The decision itself, without a scheduler: Linux's `signal_pending` after
/// `sigsuspend` installs the temporary mask. A caught signal ends the wait,
/// and an ignored one (SIGUSR2) settles it: Linux discarded it when it was
/// sent, and the woken waiter removes it.
#[test]
fn the_decision_is_pending_and_unblocked() {
    let usr1 = bit(libc::SIGUSR1);
    let usr2 = bit(libc::SIGUSR2);
    let sets =
        KernelSignalState::from_backend_sets(&state(usr1 | usr2, 0, usr1 | usr2, usr2, usr1));
    assert_eq!(
        Scheduler::backend_suspend_signals(&sets, 0, 0, false),
        usr1 | usr2
    );
    assert_eq!(
        Scheduler::backend_suspend_signals(&sets, usr1, 0, false),
        usr2
    );
    assert_eq!(
        Scheduler::backend_suspend_signals(&sets, usr1 | usr2, 0, false),
        0
    );
    assert_eq!(
        Scheduler::backend_suspend_signals(&sets, 0, usr1, false),
        usr2
    );
}

/// Mutations "the guard reads no waiter", "every pending signal counts" and
/// "another process's waiter counts": a call that stops ignoring a signal
/// asks whether a parked suspend of its process still holds it although
/// Linux discarded it at send; only one whose temporary mask lets it in, with
/// it pending and still ignored, does.
#[test]
fn a_parked_suspend_holds_a_discarded_signal_only_while_it_is_ignored_and_let_in() {
    let winch = bit(libc::SIGWINCH);
    for (name, mask, sets, process, expected) in [
        (
            "pending, default-ignored, let in",
            0,
            state(winch, 0, winch, 0, 0),
            PROCESS,
            true,
        ),
        (
            "process-directed",
            0,
            state(0, winch, winch, 0, 0),
            PROCESS,
            true,
        ),
        (
            "blocked by the temporary mask",
            winch,
            state(winch, 0, winch, 0, 0),
            PROCESS,
            false,
        ),
        (
            "caught now",
            0,
            state(winch, 0, winch, 0, winch),
            PROCESS,
            false,
        ),
        ("not pending", 0, state(0, 0, winch, 0, 0), PROCESS, false),
        (
            "another process",
            0,
            state(winch, 0, winch, 0, 0),
            DetPid::from_raw(99),
            false,
        ),
    ] {
        let (scheduler, _) = parked_suspend(mask, sets);
        assert_eq!(
            scheduler.suspend_holds_discarded(process, libc::SIGWINCH),
            expected,
            "{name}"
        );
    }
    let (mut scheduler, _) = kvm(Ok(state(winch, 0, winch, 0, 0)));
    file(
        &mut scheduler,
        WAITER,
        masked_poll_request(WAITER, 0),
        Place::Parked,
    );
    assert!(
        !scheduler.suspend_holds_discarded(PROCESS, libc::SIGWINCH),
        "a masked poll"
    );
}
