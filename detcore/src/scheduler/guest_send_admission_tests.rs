/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The admission of a guest's signal to another process where the backend
//! keeps the guest's signal queues itself (`guest_send_admission`), on a
//! KVM scheduler whose backend state a scripted control answers.

use std::sync::Arc;

use reverie::SignalSets;

use super::guest_send_admission::ClaimReason;
use super::guest_send_admission::ClaimRefusal;
use super::guest_send_admission::GuestSendClaim;
use super::parked::ControlCapability;
use super::parked::ParkedWaitPolicy;
use super::parked::ResourceOrigin;
use super::parked::RpcOrigin;
use super::process_signal_receiver::ProcessSignalReceiver;
use super::signal_state_reader_tests::ScriptedControl;
use super::signal_state_reader_tests::add_bound_thread;
use super::signal_state_reader_tests::identity;
use super::signal_state_reader_tests::kvm_scheduler;
use super::*;
use crate::scheduler::guest_send_admission::CatchReach;
use crate::scheduler::guest_send_admission::SendAdmission;
use crate::scheduler::guest_send_admission::SendRefusal;
use crate::scheduler::guest_send_admission::SendScope;

/// The sending process, with one thread.
const SENDER_PID: DetPid = DetPid::from_raw(10);
const SENDER: DetTid = DetTid::from_raw(10);
/// The target process: its leader and one more thread.
const TARGET: DetPid = DetPid::from_raw(20);
const LEADER: DetTid = DetTid::from_raw(20);
const SIBLING: DetTid = DetTid::from_raw(21);

fn bit(signal: i32) -> u64 {
    kernel_signal_bit(signal)
}

/// A thread's backend state: `blocked` as its mask, `caught` and `ignored`
/// as its process's dispositions, `pending` in its own queue and `shared` in
/// its process's.
fn state(pending: u64, shared: u64, blocked: u64, ignored: u64, caught: u64) -> SignalSets {
    SignalSets::new(pending, shared, blocked, ignored, caught)
}

/// SIGUSR1 caught and nothing blocked or pending.
fn catching() -> SignalSets {
    state(0, 0, 0, 0, bit(libc::SIGUSR1))
}

/// A KVM scheduler with the scripted control installed, the sending process
/// and the two-thread target process, each thread scripted with `sets`.
fn two_processes(sets: SignalSets) -> (Scheduler, Arc<ScriptedControl>) {
    let control = Arc::new(ScriptedControl::default());
    let mut s = kvm_scheduler(&control, true);
    for (pid, tid) in [(SENDER_PID, SENDER), (TARGET, LEADER), (TARGET, SIBLING)] {
        add_bound_thread(&mut s, pid, tid);
        control.script(tid, vec![Ok(sets)]);
    }
    (s, control)
}

/// `tid` filed an ordinary request and waits in the run queue.
fn runnable(s: &mut Scheduler, tid: DetTid) {
    s.queue_request_for_test(tid, Resources::new(tid));
}

/// `tid` polls in `fyi` (a lone `InternalIOPolling` request, with the
/// interruption marker `marker` and the temporary mask `mask`, if any) from
/// the run queue, as the retry helper defers a poll that is not ready.
fn polling(s: &mut Scheduler, tid: DetTid, fyi: &str, marker: Option<Errno>, mask: Option<u64>) {
    let mut r = Resources::new(tid);
    r.insert(ResourceID::InternalIOPolling, Permission::W);
    r.fyi(fyi);
    r.poll_attempt = 1;
    if let Some(errno) = marker {
        r.set_signal_interrupt_errno(errno);
    }
    r.blocked_signal_mask = mask;
    s.next_turns[&tid].req.put(Ok(r));
    s.run_queue.push_poller(tid, DEFAULT_PRIORITY, 1);
}

/// `tid` of `pid` is parked in an emulated `pause` at a controlled site, as
/// `handle_pause` files it and its admission registers it.
fn parked_in_pause(s: &mut Scheduler, pid: DetPid, tid: DetTid) {
    let mm = MmId::initial(pid);
    let task = identity(pid, tid);
    let site = reverie::CallbackSignalSite {
        process: task.process,
        tid: task.tid,
        task_generation: task.task_generation,
        callback_nonce: 1,
        boundary_nonce: 1,
    };
    s.install_resource_origin(
        tid,
        ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm,
            control: ControlCapability::ParkedWait {
                policy: ParkedWaitPolicy::PauseNoHandlerRestart,
                site,
            },
        },
    )
    .unwrap();
    let mut r = Resources::new(tid);
    r.insert(
        ResourceID::SleepUntil(LogicalTime::INDEFINITE),
        Permission::W,
    );
    r.fyi(crate::resources::PAUSE_FYI);
    s.next_turns[&tid].req.put(Ok(r));
    s.blocked.timed_waiters.insert(LogicalTime::INDEFINITE, tid);
}

/// `tid` runs a host call outside the schedule.
fn in_host_call(s: &mut Scheduler, tid: DetTid) {
    s.blocked
        .external_io_blockers
        .insert(tid, crate::resources::ExternalOpId::new(tid, 1));
}

/// A process-directed send of `signal` from the other process, naming the
/// target's leader, as `kill(20, signal)` makes it.
fn kill(s: &Scheduler, signal: i32) -> SendAdmission {
    s.guest_send_admission(SENDER, Some(TARGET), LEADER, SendScope::Process, signal)
}

/// A thread-directed send of `signal` from the other process to `tid`, as
/// `tgkill(20, tid, signal)` makes it.
fn tgkill(s: &Scheduler, tid: DetTid, signal: i32) -> SendAdmission {
    s.guest_send_admission(SENDER, Some(TARGET), tid, SendScope::Thread, signal)
}

fn catch(receiver: DetTid, reach: CatchReach) -> SendAdmission {
    SendAdmission::Catch { receiver, reach }
}

fn refused(refusal: SendRefusal) -> SendAdmission {
    SendAdmission::Refused(refusal)
}

/// Without a control, as on ptrace, the admission decides nothing: the host
/// kernel performs the send.
#[test]
fn without_a_control_nothing_is_decided() {
    let control = Arc::new(ScriptedControl::default());
    let mut s = kvm_scheduler(&control, false);
    for (pid, tid) in [(SENDER_PID, SENDER), (TARGET, LEADER), (TARGET, SIBLING)] {
        add_bound_thread(&mut s, pid, tid);
        control.script(tid, vec![Ok(catching())]);
    }
    runnable(&mut s, LEADER);
    assert_eq!(kill(&s, libc::SIGUSR1), SendAdmission::NotDecided);
    assert_eq!(
        tgkill(&s, SIBLING, libc::SIGUSR1),
        SendAdmission::NotDecided
    );
}

/// Within the sender's own process the backend still performs a
/// thread-directed send, and a process-directed one while the sender is
/// its process's only thread; a process-directed send to its own process
/// with another thread is decided.
#[test]
fn a_send_within_the_senders_process_is_decided_only_with_another_thread() {
    let (mut s, _control) = two_processes(catching());
    runnable(&mut s, SIBLING);
    assert_eq!(
        s.guest_send_admission(LEADER, None, SIBLING, SendScope::Thread, libc::SIGUSR1),
        SendAdmission::NotDecided
    );
    assert_eq!(
        s.guest_send_admission(
            SENDER,
            Some(SENDER_PID),
            SENDER,
            SendScope::Process,
            libc::SIGUSR1
        ),
        SendAdmission::NotDecided
    );
    assert_ne!(
        s.guest_send_admission(
            SIBLING,
            Some(TARGET),
            LEADER,
            SendScope::Process,
            libc::SIGUSR1
        ),
        SendAdmission::NotDecided
    );
}

/// A copy Linux discards as it sends it (an ignored signal, explicitly or by
/// default), one it coalesces with a standard signal already queued, and one
/// every thread blocks need no thread to act: no transition.
#[test]
fn discarded_coalesced_and_blocked_copies_need_no_transition() {
    let usr2 = bit(libc::SIGUSR2);
    let hup = bit(libc::SIGHUP);
    let (mut s, control) = two_processes(state(0, 0, hup, usr2, hup | bit(libc::SIGUSR1)));
    runnable(&mut s, LEADER);
    runnable(&mut s, SIBLING);
    assert_eq!(kill(&s, libc::SIGUSR2), SendAdmission::NoTransition);
    assert_eq!(kill(&s, libc::SIGWINCH), SendAdmission::NoTransition);
    assert_eq!(kill(&s, libc::SIGHUP), SendAdmission::NoTransition);
    assert_eq!(
        tgkill(&s, SIBLING, libc::SIGHUP),
        SendAdmission::NoTransition
    );
    // A standard signal already in the queue the copy would join.
    let usr1 = bit(libc::SIGUSR1);
    for tid in [LEADER, SIBLING] {
        control.script(tid, vec![Ok(state(usr1, usr1, 0, 0, usr1))]);
    }
    assert_eq!(kill(&s, libc::SIGUSR1), SendAdmission::NoTransition);
    assert_eq!(
        tgkill(&s, SIBLING, libc::SIGUSR1),
        SendAdmission::NoTransition
    );
}

/// The transitions this stage does not carry out are refused by name: kill,
/// stop and die.
#[test]
fn kill_stop_and_die_are_refused_by_name() {
    let (mut s, _control) = two_processes(catching());
    runnable(&mut s, LEADER);
    runnable(&mut s, SIBLING);
    assert_eq!(kill(&s, libc::SIGKILL), refused(SendRefusal::Kill));
    assert_eq!(kill(&s, libc::SIGTSTP), refused(SendRefusal::Stop));
    assert_eq!(kill(&s, libc::SIGTERM), refused(SendRefusal::Die));
    assert_eq!(
        tgkill(&s, SIBLING, libc::SIGTERM),
        refused(SendRefusal::Die)
    );
}

/// A caught copy for a runnable thread is taken at its next boundary: the
/// named leader for a process-directed send, the named thread for a
/// thread-directed one.
#[test]
fn a_caught_copy_for_a_runnable_thread_is_taken_at_its_boundary() {
    let (mut s, _control) = two_processes(catching());
    runnable(&mut s, LEADER);
    runnable(&mut s, SIBLING);
    assert_eq!(kill(&s, libc::SIGUSR1), catch(LEADER, CatchReach::Boundary));
    assert_eq!(
        tgkill(&s, SIBLING, libc::SIGUSR1),
        catch(SIBLING, CatchReach::Boundary)
    );
}

/// The walk reads each thread under the mask Linux's `blocked` holds now: a
/// masked `pselect6` that blocks the signal passes the leader over though
/// its own mask does not, and one that admits it chooses the leader though
/// its own mask blocks it, which then takes the copy at its next probe.
#[test]
fn the_walk_reads_a_calls_temporary_mask() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, _control) = two_processes(catching());
    polling(&mut s, LEADER, "pselect6", Some(Errno::EINTR), Some(usr1));
    runnable(&mut s, SIBLING);
    assert_eq!(
        kill(&s, libc::SIGUSR1),
        catch(SIBLING, CatchReach::Boundary)
    );

    let (mut s, control) = two_processes(catching());
    control.script(LEADER, vec![Ok(state(0, 0, usr1, 0, usr1))]);
    polling(&mut s, LEADER, "pselect6", Some(Errno::EINTR), Some(0));
    runnable(&mut s, SIBLING);
    assert_eq!(kill(&s, libc::SIGUSR1), catch(LEADER, CatchReach::Probe));
}

/// Each state of the chosen receiver (section 2's rows) is admitted with how
/// it takes the copy, or refused by name.
#[test]
fn each_receiver_state_is_admitted_or_refused_by_name() {
    type Setup = fn(&mut Scheduler);
    let rows: [(&str, Setup, SendAdmission); 13] = [
        (
            "runnable",
            |s| runnable(s, LEADER),
            catch(LEADER, CatchReach::Boundary),
        ),
        (
            "raw select, which no probe ends",
            |s| polling(s, LEADER, "select", Some(Errno::EINTR), None),
            refused(SendRefusal::RawSelect(LEADER)),
        ),
        (
            "probing masked ppoll",
            |s| polling(s, LEADER, "ppoll", Some(Errno::EINTR), Some(0)),
            catch(LEADER, CatchReach::Probe),
        ),
        (
            "probing rt_sigtimedwait",
            |s| polling(s, LEADER, "rt_sigtimedwait", None, None),
            catch(LEADER, CatchReach::Probe),
        ),
        (
            "internal IO poll with the restart marker",
            |s| polling(s, LEADER, "read", Some(Errno::ERESTARTSYS), None),
            catch(LEADER, CatchReach::RestartablePoll),
        ),
        (
            "poll whose probe takes no signal",
            |s| polling(s, LEADER, "poll", Some(Errno::EINTR), None),
            refused(SendRefusal::Poll(LEADER)),
        ),
        (
            "unmasked ppoll",
            |s| polling(s, LEADER, "ppoll", Some(Errno::EINTR), None),
            refused(SendRefusal::Poll(LEADER)),
        ),
        (
            "poller outside the run queue",
            |s| {
                polling(s, LEADER, "select", Some(Errno::EINTR), None);
                assert!(s.run_queue.remove_tid(LEADER));
            },
            refused(SendRefusal::PollerOutsideRunQueue(LEADER)),
        ),
        (
            "parked in pause",
            |s| parked_in_pause(s, TARGET, LEADER),
            refused(SendRefusal::Parked(LEADER)),
        ),
        (
            "futex wait",
            |s| {
                s.park_futex_waiter(
                    &LEADER,
                    FutexID::private(MmId::initial(TARGET), 0x1000),
                    None,
                    !0,
                    None,
                    false,
                );
            },
            refused(SendRefusal::Futex(LEADER)),
        ),
        (
            "host call",
            |s| in_host_call(s, LEADER),
            refused(SendRefusal::ExternalIo(LEADER)),
        ),
        (
            "vfork parent",
            |s| {
                runnable(s, LEADER);
                s.vfork_barriers.insert(LEADER, None);
            },
            refused(SendRefusal::VforkParent(LEADER)),
        ),
        (
            "no request and not queued",
            |_| {},
            refused(SendRefusal::UnknownState(LEADER)),
        ),
    ];
    for (name, setup, expected) in rows {
        let (mut s, _control) = two_processes(catching());
        setup(&mut s);
        assert_eq!(kill(&s, libc::SIGUSR1), expected, "{name}");
    }
}

/// A receiver that cannot take the copy is refused, not passed over: Linux's
/// `complete_signal` tries the task a process-directed send names first, so a
/// futex-waiting leader is the receiver even while a runnable sibling leaves
/// the signal unblocked; and a thread-directed send has no other receiver
/// than the thread in a host call it names.
#[test]
fn a_receiver_that_cannot_take_the_copy_is_refused_not_passed_over() {
    let (mut s, _control) = two_processes(catching());
    runnable(&mut s, SIBLING);
    s.park_futex_waiter(
        &LEADER,
        FutexID::private(MmId::initial(TARGET), 0x1000),
        None,
        !0,
        None,
        false,
    );
    assert_eq!(kill(&s, libc::SIGUSR1), refused(SendRefusal::Futex(LEADER)));

    let (mut s, _control) = two_processes(catching());
    runnable(&mut s, LEADER);
    in_host_call(&mut s, SIBLING);
    assert_eq!(
        tgkill(&s, SIBLING, libc::SIGUSR1),
        refused(SendRefusal::ExternalIo(SIBLING))
    );
}

/// A backend-served `rt_sigsuspend` is woken by the drain, and a thread held
/// at a happens-before gate takes the copy once released.
#[test]
fn a_backend_suspend_and_a_gate_take_the_copy() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, _control) = two_processes(catching());
    let mut r = Resources::new(LEADER);
    r.insert(
        ResourceID::SleepUntil(LogicalTime::INDEFINITE),
        Permission::W,
    );
    r.fyi(crate::resources::RT_SIGSUSPEND_FYI);
    r.blocked_signal_mask = Some(!usr1);
    s.next_turns[&LEADER].req.put(Ok(r));
    s.blocked
        .timed_waiters
        .insert(LogicalTime::INDEFINITE, LEADER);
    assert_eq!(s.backend_suspend_mask(LEADER), Some(!usr1));
    assert_eq!(
        kill(&s, libc::SIGUSR1),
        catch(LEADER, CatchReach::BackendSuspend)
    );

    let (mut s, _control) = two_processes(catching());
    let program = detcore_model::happens_before::HappensBeforeSpec::from_json(
        r#"{"version": 1,
        "events": {"src": {"thread": "21", "syscalls": 42},
                   "dst": {"thread": "20", "syscalls": 43}},
        "edges": [{"before": "src", "after": "dst", "strength": "hard"}]}"#,
    )
    .unwrap()
    .normalize()
    .unwrap();
    let mut hb = HbRuntime::new(program);
    hb.parked.insert(LEADER);
    s.happens_before = Some(hb);
    s.next_turns[&LEADER].req.put(Ok(Resources::new(LEADER)));
    assert_eq!(kill(&s, libc::SIGUSR1), catch(LEADER, CatchReach::Gate));
}

/// Within the target's own process, a sender that does not block the signal
/// would take the copy at its own return, before the thread Linux chooses:
/// refused. One that blocks it leaves the copy to that thread, and a sender
/// Linux chooses itself takes it at its own boundary.
#[test]
fn the_sender_of_its_own_process_signal_takes_it_first_unless_it_blocks_it() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, control) = two_processes(catching());
    runnable(&mut s, LEADER);
    let own = |s: &Scheduler| {
        s.guest_send_admission(
            SIBLING,
            Some(TARGET),
            LEADER,
            SendScope::Process,
            libc::SIGUSR1,
        )
    };
    assert_eq!(own(&s), refused(SendRefusal::SenderTakesIt(LEADER)));
    control.script(SIBLING, vec![Ok(state(0, 0, usr1, 0, usr1))]);
    assert_eq!(own(&s), catch(LEADER, CatchReach::Boundary));
    // The sender is the named leader and does not block it.
    let (mut s, _control) = two_processes(catching());
    runnable(&mut s, SIBLING);
    assert_eq!(
        s.guest_send_admission(
            LEADER,
            Some(TARGET),
            LEADER,
            SendScope::Process,
            libc::SIGUSR1
        ),
        catch(LEADER, CatchReach::Boundary)
    );
}

/// A thread that waits for a default-fatal signal in `rt_sigtimedwait`, and
/// blocks it outside the wait, takes it by that wait: no die. Where its own
/// mask leaves the signal unblocked, Linux's `complete_signal` ends the
/// group: die.
#[test]
fn a_waiter_in_rt_sigtimedwait_takes_a_default_fatal_signal_it_blocks() {
    let term = bit(libc::SIGTERM);
    let (mut s, _control) = two_processes(state(0, 0, term, 0, 0));
    polling(&mut s, LEADER, "rt_sigtimedwait", None, None);
    assert!(s.set_sigtimedwait_set(LEADER, Some(term)));
    runnable(&mut s, SIBLING);
    assert_eq!(kill(&s, libc::SIGTERM), catch(LEADER, CatchReach::Probe));
    assert_eq!(
        tgkill(&s, LEADER, libc::SIGTERM),
        catch(LEADER, CatchReach::Probe)
    );

    let (mut s, control) = two_processes(state(0, 0, term, 0, 0));
    control.script(LEADER, vec![Ok(state(0, 0, 0, 0, 0))]);
    polling(&mut s, LEADER, "rt_sigtimedwait", None, None);
    assert!(s.set_sigtimedwait_set(LEADER, Some(term)));
    runnable(&mut s, SIBLING);
    assert_eq!(kill(&s, libc::SIGTERM), refused(SendRefusal::Die));
}

/// Where a signalfd may cover the signal, a send is refused while a thread
/// that can read it is in a host call, and a caught copy is refused outright;
/// a blocked copy with no such thread needs no transition, as before.
#[test]
fn a_signalfd_read_outside_the_schedule_is_refused() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, _control) = two_processes(state(0, 0, usr1, 0, usr1));
    s.register_signalfd(TARGET, usr1);
    runnable(&mut s, LEADER);
    runnable(&mut s, SIBLING);
    assert_eq!(kill(&s, libc::SIGUSR1), SendAdmission::NoTransition);
    in_host_call(&mut s, SIBLING);
    assert_eq!(
        kill(&s, libc::SIGUSR1),
        refused(SendRefusal::SignalfdReadOutsideTurn(SIBLING))
    );
    // A thread-directed copy joins the named thread's own queue, which only
    // its own reads reach.
    assert_eq!(
        tgkill(&s, LEADER, libc::SIGUSR1),
        SendAdmission::NoTransition
    );
    assert_eq!(
        tgkill(&s, SIBLING, libc::SIGUSR1),
        refused(SendRefusal::SignalfdReadOutsideTurn(SIBLING))
    );

    let (mut s, _control) = two_processes(catching());
    s.register_signalfd(TARGET, usr1);
    runnable(&mut s, LEADER);
    assert_eq!(
        kill(&s, libc::SIGUSR1),
        refused(SendRefusal::SignalfdCovered(LEADER))
    );
    // A signalfd for another signal changes nothing.
    let (mut s, _control) = two_processes(catching());
    s.register_signalfd(TARGET, bit(libc::SIGUSR2));
    runnable(&mut s, LEADER);
    assert_eq!(kill(&s, libc::SIGUSR1), catch(LEADER, CatchReach::Boundary));
}

/// A committed copy is claimed for its receiver, which is then busy for a
/// later choice; the audit keeps a claim whose copy is still pending and
/// that the receiver can still take, and drops one whose copy is gone.
#[test]
fn a_claim_stands_until_its_copy_is_taken() {
    let usr1 = bit(libc::SIGUSR1);
    let usr2 = bit(libc::SIGUSR2);
    let (mut s, control) = two_processes(state(0, 0, 0, 0, usr1 | usr2));
    runnable(&mut s, LEADER);
    runnable(&mut s, SIBLING);
    s.guest_send_committed(LEADER, SendScope::Process, libc::SIGUSR1);
    assert_eq!(
        s.guest_send_claims.get(&LEADER),
        Some(&GuestSendClaim {
            shared: usr1,
            private: 0
        })
    );
    // Linux's `signal_wake_up` makes the chosen thread busy: the next copy
    // goes to its sibling.
    assert_eq!(
        kill(&s, libc::SIGUSR2),
        catch(SIBLING, CatchReach::Boundary)
    );
    for tid in [LEADER, SIBLING] {
        control.script(tid, vec![Ok(state(0, usr1, 0, 0, usr1))]);
    }
    assert_eq!(s.audit_guest_send_claims(), None);
    assert!(s.guest_send_claims.contains_key(&LEADER));
    // Taken: nothing pending any more.
    for tid in [LEADER, SIBLING] {
        control.script(tid, vec![Ok(catching())]);
    }
    assert_eq!(s.audit_guest_send_claims(), None);
    assert!(s.guest_send_claims.is_empty());
}

/// A claimed receiver that begins to wait where it cannot take its copy ends
/// the run with that refusal: a host call for either copy, and an emulated
/// `pause` for a shared copy. A copy in its own queue reaches a parked
/// thread through the observation of its own signals.
#[test]
fn a_claimed_receiver_that_waits_where_it_cannot_take_the_copy_is_refused() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, _control) = two_processes(state(usr1, 0, 0, 0, usr1));
    runnable(&mut s, SIBLING);
    s.guest_send_committed(LEADER, SendScope::Thread, libc::SIGUSR1);
    in_host_call(&mut s, LEADER);
    assert_eq!(
        s.audit_guest_send_claims(),
        Some(ClaimRefusal {
            receiver: LEADER,
            signal: libc::SIGUSR1,
            reason: ClaimReason::Waits(SendRefusal::ExternalIo(LEADER)),
        })
    );

    let (mut s, _control) = two_processes(state(usr1, 0, 0, 0, usr1));
    runnable(&mut s, SIBLING);
    parked_in_pause(&mut s, TARGET, LEADER);
    s.guest_send_committed(LEADER, SendScope::Thread, libc::SIGUSR1);
    assert_eq!(s.audit_guest_send_claims(), None);
    assert!(s.guest_send_claims.contains_key(&LEADER));

    let (mut s, _control) = two_processes(state(0, usr1, 0, 0, usr1));
    runnable(&mut s, SIBLING);
    parked_in_pause(&mut s, TARGET, LEADER);
    s.guest_send_committed(LEADER, SendScope::Process, libc::SIGUSR1);
    let refusal = s.audit_guest_send_claims();
    assert_eq!(
        refusal,
        Some(ClaimRefusal {
            receiver: LEADER,
            signal: libc::SIGUSR1,
            reason: ClaimReason::Waits(SendRefusal::Parked(LEADER)),
        })
    );
    let text = refusal.unwrap().to_string();
    assert!(
        text.starts_with("HERMIT_KVM_SIGNAL_CLAIM_REFUSED: signal 10 ")
            && text.contains("stage X1 slice D"),
        "{text}"
    );
}

/// A shared copy whose receiver exited, or blocked the signal, before taking
/// it, while another live thread leaves it unblocked: Linux wakes that
/// thread (`retarget_shared_pending`), which this stage refuses by name.
/// Where every other thread blocks it too, the copy waits and the claim
/// goes.
#[test]
fn a_shared_copy_left_behind_is_refused_as_a_retarget() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, control) = two_processes(state(0, usr1, 0, 0, usr1));
    runnable(&mut s, LEADER);
    runnable(&mut s, SIBLING);
    s.guest_send_committed(LEADER, SendScope::Process, libc::SIGUSR1);
    control.script(LEADER, vec![Ok(state(0, usr1, usr1, 0, usr1))]);
    assert_eq!(
        s.audit_guest_send_claims(),
        Some(ClaimRefusal {
            receiver: LEADER,
            signal: libc::SIGUSR1,
            reason: ClaimReason::Retarget,
        })
    );
    control.script(SIBLING, vec![Ok(state(0, usr1, usr1, 0, usr1))]);
    s.guest_send_committed(LEADER, SendScope::Process, libc::SIGUSR1);
    assert_eq!(s.audit_guest_send_claims(), None);
    assert!(s.guest_send_claims.is_empty());

    // The receiver exited.
    let (mut s, _control) = two_processes(state(0, usr1, 0, 0, usr1));
    runnable(&mut s, SIBLING);
    s.guest_send_committed(LEADER, SendScope::Process, libc::SIGUSR1);
    s.next_turns.remove(&LEADER);
    assert_eq!(
        s.audit_guest_send_claims(),
        Some(ClaimRefusal {
            receiver: LEADER,
            signal: libc::SIGUSR1,
            reason: ClaimReason::Retarget,
        })
    );
}

/// An observation of another thread that leaves a claimed shared copy
/// unblocked could take it first: refused until the opportunity rule. A
/// thread that blocks it contests nothing, nor does the receiver itself.
#[test]
fn an_observation_that_could_take_a_claimed_copy_is_refused() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, control) = two_processes(state(0, usr1, 0, 0, usr1));
    runnable(&mut s, LEADER);
    runnable(&mut s, SIBLING);
    s.guest_send_committed(SIBLING, SendScope::Process, libc::SIGUSR1);
    assert_eq!(
        s.contested_guest_send_claim(TARGET, LEADER),
        Some(ClaimRefusal {
            receiver: SIBLING,
            signal: libc::SIGUSR1,
            reason: ClaimReason::Contested { observed: LEADER },
        })
    );
    assert_eq!(s.contested_guest_send_claim(TARGET, SIBLING), None);
    control.script(LEADER, vec![Ok(state(0, usr1, usr1, 0, usr1))]);
    assert_eq!(s.contested_guest_send_claim(TARGET, LEADER), None);
}

/// With a control installed, the receiver choice that follows the admission
/// reads the temporary masks as the admission did, and no longer refuses a
/// thread whose call's mask admits the signal as taking it out of turn.
#[test]
fn the_controlled_receiver_choice_reads_temporary_masks() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, control) = two_processes(state(0, 0, usr1, 0, usr1));
    polling(&mut s, LEADER, "pselect6", Some(Errno::EINTR), Some(0));
    runnable(&mut s, SIBLING);
    control.script(SIBLING, vec![Ok(state(0, 0, usr1, 0, usr1))]);
    assert_eq!(kill(&s, libc::SIGUSR1), catch(LEADER, CatchReach::Probe));
    assert!(matches!(
        s.process_signal_receiver(TARGET, LEADER, SENDER, libc::SIGUSR1),
        ProcessSignalReceiver::Thread { thread, .. } if thread == LEADER
    ));
}

/// `tid` is parked in a backend-served `rt_sigsuspend` whose temporary mask
/// is `mask`, as the S2b wait files it.
fn parked_in_suspend(s: &mut Scheduler, tid: DetTid, mask: u64) {
    let mut r = Resources::new(tid);
    r.insert(
        ResourceID::SleepUntil(LogicalTime::INDEFINITE),
        Permission::W,
    );
    r.fyi(crate::resources::RT_SIGSUSPEND_FYI);
    r.blocked_signal_mask = Some(mask);
    s.next_turns[&tid].req.put(Ok(r));
    s.blocked.timed_waiters.insert(LogicalTime::INDEFINITE, tid);
}

/// The forwarded process-directed send left one copy of `signal` in the
/// target's shared queue; both threads still block it outside their calls.
fn sent_to_shared_queue(s: &mut Scheduler, control: &ScriptedControl, signal: i32) {
    let usr = bit(signal);
    let sent = state(0, usr, usr, 0, usr);
    for tid in [LEADER, SIBLING] {
        control.script(tid, vec![Ok(sent)]);
        s.test_kernel_signal_states
            .insert(tid, KernelSignalState::from_backend_sets(&sent));
    }
}

/// The Codex review of https://github.com/rrnewton/hermit/pull/5415 at
/// fcff78c8, P1, past the admission: the leader and its sibling both catch
/// SIGUSR1 and block it outside the call, and both wait in a backend-served
/// `rt_sigsuspend` whose temporary mask lets it in. `kill(leader, SIGUSR1)`
/// from another process: Linux's `complete_signal` chooses the named leader,
/// whose `blocked` is the suspend's mask (kernel/signal.c:4835-4846 and
/// 963-997 at v7.1.3), and `signal_wake_up` wakes it alone, so the leader
/// takes the copy and runs its handler, and the sibling sleeps on. The
/// admission chose the leader; the drain that follows the send, the wake,
/// and the choice of who runs first must keep it, so the leader's call runs
/// before the sibling's and takes the copy, and the claim settles once it is
/// taken.
#[test]
fn the_admitted_suspend_waiter_takes_the_copy_not_its_woken_sibling() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, control) = two_processes(state(0, 0, usr1, 0, usr1));
    for tid in [LEADER, SIBLING] {
        parked_in_suspend(&mut s, tid, !usr1);
    }
    assert_eq!(
        kill(&s, libc::SIGUSR1),
        catch(LEADER, CatchReach::BackendSuspend)
    );
    assert!(matches!(
        s.process_signal_receiver(TARGET, LEADER, SENDER, libc::SIGUSR1),
        ProcessSignalReceiver::Thread { thread, .. } if thread == LEADER
    ));
    s.process_signal_sent(SENDER, TARGET);
    s.guest_send_committed(LEADER, SendScope::Process, libc::SIGUSR1);
    sent_to_shared_queue(&mut s, &control, libc::SIGUSR1);

    s.step2_drain_prefix_for_test();
    assert_eq!(s.audit_guest_send_claims(), None);
    let (first, _, _) = s.step3_peek().expect("a woken waiter is runnable");
    s.run_queue.undo_tentative_pop();
    assert_eq!(
        first, LEADER,
        "the admitted receiver's call runs first and takes the shared copy"
    );

    // The leader's call takes the copy and runs its handler.
    let response = s.next_turns[&LEADER].resp.clone();
    assert!(s.unblock_guest(LEADER, &response).is_ok());
    let taken = state(0, 0, usr1, 0, usr1);
    for tid in [LEADER, SIBLING] {
        control.script(tid, vec![Ok(taken)]);
        s.test_kernel_signal_states
            .insert(tid, KernelSignalState::from_backend_sets(&taken));
    }
    assert_eq!(s.audit_guest_send_claims(), None);
    assert!(s.guest_send_claims.is_empty());
    assert_eq!(s.first_process_signal_taker(), None);
}

/// The same choice where both threads probe a masked `pselect6` from the
/// run queue: the admitted leader's probe runs first and takes the copy.
#[test]
fn the_admitted_polling_receiver_probes_before_its_sibling() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, control) = two_processes(state(0, 0, usr1, 0, usr1));
    // The sibling is queued ahead of the leader, so queue order alone
    // would probe it first.
    for tid in [SIBLING, LEADER] {
        polling(&mut s, tid, "pselect6", Some(Errno::EINTR), Some(0));
    }
    assert_eq!(kill(&s, libc::SIGUSR1), catch(LEADER, CatchReach::Probe));
    assert!(matches!(
        s.process_signal_receiver(TARGET, LEADER, SENDER, libc::SIGUSR1),
        ProcessSignalReceiver::Thread { thread, .. } if thread == LEADER
    ));
    s.process_signal_sent(SENDER, TARGET);
    s.guest_send_committed(LEADER, SendScope::Process, libc::SIGUSR1);
    sent_to_shared_queue(&mut s, &control, libc::SIGUSR1);

    s.step2_drain_prefix_for_test();
    assert_eq!(s.audit_guest_send_claims(), None);
    let (first, _, _) = s.step3_peek().expect("a poller is runnable");
    s.run_queue.undo_tentative_pop();
    assert_eq!(
        first, LEADER,
        "the admitted receiver probes first and takes the shared copy"
    );
}

/// The deterministic-scheduling review of
/// https://github.com/rrnewton/hermit/pull/5415 at 5055d688, required change
/// 1: in the state of
/// `the_admitted_suspend_waiter_takes_the_copy_not_its_woken_sibling`, Linux
/// wakes only the leader (`complete_signal` chooses it and `signal_wake_up`
/// wakes it alone, kernel/signal.c:963-997 at v7.1.3), so the sibling stays
/// asleep in its `rt_sigsuspend`. Here too: after the drain the sibling
/// still holds its park, with its own request, out of the run queue; and of
/// the turns step 3 then offers, each granted as the turn loop grants one
/// (a charge of virtual time, then the `COMMIT` record and the grant), only
/// the leader's is one: one turn, one charge, and none of either for the
/// sibling.
#[test]
fn the_unchosen_suspend_sibling_stays_asleep_and_has_no_turn() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, control) = two_processes(state(0, 0, usr1, 0, usr1));
    for tid in [LEADER, SIBLING] {
        parked_in_suspend(&mut s, tid, !usr1);
    }
    assert_eq!(
        kill(&s, libc::SIGUSR1),
        catch(LEADER, CatchReach::BackendSuspend)
    );
    assert!(matches!(
        s.process_signal_receiver(TARGET, LEADER, SENDER, libc::SIGUSR1),
        ProcessSignalReceiver::Thread { thread, .. } if thread == LEADER
    ));
    s.process_signal_sent(SENDER, TARGET);
    s.guest_send_committed(LEADER, SendScope::Process, libc::SIGUSR1);
    sent_to_shared_queue(&mut s, &control, libc::SIGUSR1);

    s.step2_drain_prefix_for_test();
    assert_eq!(s.audit_guest_send_claims(), None);
    // Every fact is gathered first and compared at once, so a failure shows
    // the wake, the turns and the charge together.
    let parked = |s: &Scheduler| {
        s.backend_suspend_mask(SIBLING) == Some(!usr1)
            && !s.run_queue.contains_tid(SIBLING)
            && s.blocked.timed_waiters.thread_deadline(SIBLING).is_some()
    };
    let parked_after_drain = parked(&s);

    let global = std::sync::Mutex::new(GlobalTime::new(&Config::default()));
    let turn = s.turn;
    let mut granted = Vec::new();
    let mut charged_after_leader = None;
    while let Some((tid, request, response)) = s.step3_peek() {
        let request = request
            .try_read()
            .expect("a filed request")
            .expect("a live thread");
        s.bump_global_time(&global, &Ok(request.clone()));
        assert!(s.step5_guest_unblock(tid, &request, &response).is_ok());
        assert_eq!(s.run_queue.commit_tentative_pop_completed_turn(), tid);
        granted.push(tid);
        if tid == LEADER {
            // The leader's call takes the copy and runs its handler.
            let taken = state(0, 0, usr1, 0, usr1);
            for tid in [LEADER, SIBLING] {
                control.script(tid, vec![Ok(taken)]);
                s.test_kernel_signal_states
                    .insert(tid, KernelSignalState::from_backend_sets(&taken));
            }
            charged_after_leader = Some(s.committed_time);
        }
        if granted.len() > 2 {
            break;
        }
    }
    assert_eq!(
        (
            parked_after_drain,
            granted,
            s.turn - turn,
            Some(s.committed_time) == charged_after_leader,
            s.last_commit_at.contains_key(&SIBLING),
            parked(&s),
        ),
        (true, vec![LEADER], 1, true, false, true),
        "(sibling parked after the drain, turns granted, COMMIT records, no \
         charge after the leader's turn, sibling committed, sibling parked at the end)"
    );
}

/// The deterministic-scheduling review of
/// https://github.com/rrnewton/hermit/pull/5415 at 5055d688, required change
/// 2: a thread chosen for a shared copy whose call outside the schedule
/// returned, readmitted with the temporary mask its readmission recorded
/// (`BlockedPool::continuation_masks`), and queued to run. Its taker mark is
/// judged under that mask, as the admission and the choice read it: kept
/// where the mask lets the signal in, dropped where it blocks it. Where the
/// readmission could not record the mask (`None`: an `rt_sigsuspend` rejoin,
/// a masked wait under record or replay), the mark stays: an unknown mask
/// is no evidence the thread blocks the signal. The claims audit does not
/// refuse it but defers an unreadable claim to its next pass; admission is
/// what refuses an unknown mask, at send time.
#[test]
fn a_readmitted_takers_mark_is_judged_under_its_continuation_mask() {
    let usr1 = bit(libc::SIGUSR1);
    for (case, own, continuation, kept) in [
        ("its temporary mask lets it in", usr1, Some(!usr1), true),
        ("its temporary mask blocks it", 0, Some(usr1), false),
        ("its temporary mask is unknown", usr1, None, true),
    ] {
        let (mut s, _control) = two_processes(state(0, usr1, own, 0, usr1));
        let mut rejoin = Resources::new(LEADER);
        rejoin.insert(
            ResourceID::BlockedExternalContinue(crate::resources::ExternalOpId::new(LEADER, 1)),
            Permission::RW,
        );
        s.next_turns.get_mut(&LEADER).unwrap().req = Ivar::full(Ok(rejoin));
        s.blocked.continuation_masks.insert(LEADER, continuation);
        s.runqueue_push_back(LEADER);
        s.process_signal_takers.insert(LEADER, usr1);
        assert_eq!(
            s.first_process_signal_taker(),
            kept.then_some(LEADER),
            "{case}"
        );
        assert_eq!(
            s.process_signal_takers.get(&LEADER).copied(),
            kept.then_some(usr1),
            "{case}"
        );
    }
}

/// A claim is a wake choice, not a reservation (module docs; the
/// deterministic-scheduling review of
/// https://github.com/rrnewton/hermit/pull/5415 at 5055d688, finding P2).
/// The leader probes a masked `pselect6` that lets SIGUSR1 in while its own
/// mask blocks it, and its runnable sibling blocks SIGUSR1 when `kill` is
/// sent, so the leader is the only thread Linux can choose: it is claimed,
/// with no taker mark (a sole receiver needs none). The sibling then
/// unblocks SIGUSR1 itself, and step 3 runs it first by queue order. Its
/// return to user mode dequeues the shared copy, as Linux's `dequeue_signal`
/// lets any thread that leaves it unblocked do (kernel/signal.c:618-640 at
/// v7.1.3). The next audit finds the copy gone, drops the claim, and does
/// not end the run. (A sibling that leaves SIGUSR1 unblocked when it is
/// sent gives the leader a taker mark, and step 3 then resumes the leader
/// first: `the_admitted_polling_receiver_probes_before_its_sibling`.)
#[test]
fn a_sibling_boundary_that_takes_a_claimed_copy_first_drops_the_claim() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, control) = two_processes(state(0, 0, usr1, 0, usr1));
    runnable(&mut s, SIBLING);
    polling(&mut s, LEADER, "pselect6", Some(Errno::EINTR), Some(0));
    assert_eq!(kill(&s, libc::SIGUSR1), catch(LEADER, CatchReach::Probe));
    assert!(matches!(
        s.process_signal_receiver(TARGET, LEADER, SENDER, libc::SIGUSR1),
        ProcessSignalReceiver::Thread { thread, .. } if thread == LEADER
    ));
    s.process_signal_sent(SENDER, TARGET);
    s.guest_send_committed(LEADER, SendScope::Process, libc::SIGUSR1);
    assert!(
        s.process_signal_takers.is_empty(),
        "a sole receiver has no mark"
    );
    // The copy is in the shared queue, and the sibling has since unblocked
    // SIGUSR1 (its own `rt_sigprocmask`).
    control.script(LEADER, vec![Ok(state(0, usr1, usr1, 0, usr1))]);
    control.script(SIBLING, vec![Ok(state(0, usr1, 0, 0, usr1))]);
    assert_eq!(s.audit_guest_send_claims(), None);

    let (first, _, _) = s.step3_peek().expect("a thread is runnable");
    s.run_queue.undo_tentative_pop();
    assert_eq!(
        first, SIBLING,
        "the runnable sibling reaches its boundary first"
    );
    // Its return to user mode takes the shared copy.
    control.script(LEADER, vec![Ok(state(0, 0, usr1, 0, usr1))]);
    control.script(SIBLING, vec![Ok(state(0, 0, 0, 0, usr1))]);
    assert_eq!(s.audit_guest_send_claims(), None, "no refusal ends the run");
    assert!(s.guest_send_claims.is_empty());
    assert_eq!(s.first_process_signal_taker(), None);
}

/// The receiver choice's re-check that the sender would take its own copy
/// first (`ProcessSignalReceiver::SenderTakesIt`) reads the sender's mask
/// in the controlled view, as the admission and the walk read every thread's
/// (the deterministic-scheduling review of
/// https://github.com/rrnewton/hermit/pull/5415 at 5055d688, finding P3).
/// A sibling that blocks SIGUSR1 sends it to its process naming the
/// runnable leader, which leaves it unblocked: the leader takes it, and the
/// sender is not refused as taking it first.
#[test]
fn the_sender_check_of_the_choice_reads_the_controlled_mask() {
    let usr1 = bit(libc::SIGUSR1);
    let (mut s, control) = two_processes(catching());
    control.script(SIBLING, vec![Ok(state(0, 0, usr1, 0, usr1))]);
    runnable(&mut s, LEADER);
    let choice = s.process_signal_receiver(TARGET, LEADER, SIBLING, libc::SIGUSR1);
    assert!(
        matches!(choice, ProcessSignalReceiver::Thread { thread, .. } if thread == LEADER),
        "{choice:?}"
    );
}

/// `Scheduler::own_process_signal_choice` reads only the stopped view
/// because the sends it follows (`Detcore::own_process_signal`) happen only
/// where the kernel's signal state is the guest's own, and no such backend
/// installs a process signal control.
#[test]
fn own_process_sends_never_meet_a_control() {
    use reverie::BackendCapabilities;
    for (name, backend) in [
        ("ptrace", BackendCapabilities::PTRACE),
        ("e9patch", BackendCapabilities::E9PATCH),
        ("kvm", BackendCapabilities::KVM),
        ("sabre", BackendCapabilities::SABRE),
        ("dbt", BackendCapabilities::DBT),
        ("liteinst in-guest", BackendCapabilities::LITEINST_IN_GUEST),
    ] {
        assert!(
            !(backend.kernel_signal_state_is_guests && backend.provides_process_signal_control),
            "{name}"
        );
    }
}
