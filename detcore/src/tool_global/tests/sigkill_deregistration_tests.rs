/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A deregistration that reports a death by a signal that ends the whole
//! thread group, SIGKILL here, makes an empty run queue wait for the rest of
//! the group's deaths (`Scheduler::note_signal_killed_group`).

use super::*;

const PARENT: DetTid = DetTid::from_raw(1);
const LEADER: DetTid = DetTid::from_raw(3);
const WORKER: DetTid = DetTid::from_raw(5);

fn state() -> (GlobalState, DetTime) {
    let config = Config {
        sequentialize_threads: true,
        ..Config::default()
    }
    .with_backend(|backend| {
        backend.needs_killed_thread_rpc_cancellation = false;
    });
    let state = GlobalState::initialize(&config, false);
    {
        let mut sched = state.sched.lock().unwrap();
        sched.thread_tree.add_child(PARENT, PARENT, true);
        sched.thread_tree.add_child(PARENT, LEADER, true);
        sched.thread_tree.add_child(LEADER, WORKER, false);
    }
    let time = DetTime::new(&config);
    for tid in [LEADER, WORKER] {
        install_test_registration(&state, tid, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .install_test_exec_incarnation(tid, MmId::initial(LEADER));
        state.global_time.lock().unwrap().update_global_time(
            tid,
            time.as_nanos(),
            time.inherited_nanos(),
        );
    }
    (state, time)
}

fn deregistration(tid: DetTid, sigkilled: bool) -> ThreadDeregistration {
    ThreadDeregistration {
        dettid: tid,
        detpid: LEADER,
        mm: MmId::initial(LEADER),
        thread_start_entered: true,
        vfork_origin: None,
        timeslice_stats: TimesliceStats::default(),
        slice_ran_past_bound: false,
        syscall_count: 1,
        syscall_histogram: Default::default(),
        chaos_epochs: Vec::new(),
        killed_by_group_signal: sigkilled,
        passed_descriptors_out: false,
        wait_status: 0,
    }
}

/// A worker's death by SIGKILL makes the scheduler wait for the rest of its
/// group to die; any other death does not. Either way the leader is retired
/// only at its own deregistration.
#[tokio::test]
async fn a_sigkill_deregistration_waits_for_the_rest_of_its_group() {
    for sigkilled in [false, true] {
        let (state, time) = state();
        let response = state
            .receive_rpc(
                Tid::from_raw(WORKER.as_raw()),
                (
                    time,
                    MmId::initial(LEADER),
                    GlobalRequest::DeregisterThread(deregistration(WORKER, sigkilled)),
                ),
            )
            .await;
        assert_eq!(response, (None, GlobalResponse::DeregisterThread(())));
        let sched = state.sched.lock().unwrap();
        assert!(!sched.next_turns.contains_key(&WORKER));
        assert!(sched.next_turns.contains_key(&LEADER));
        assert_eq!(
            sched.signal_death_victim(DetPid::from_raw(LEADER.as_raw())),
            sigkilled,
            "sigkilled: {sigkilled}"
        );
    }
}

/// A death the scheduler already accounted is no signal death to wait for.
/// A leader retired at its own exit while its group lived on reports its
/// death by SIGKILL only at its deregistration, and a guest's
/// `kill(leader, SIGKILL)` retires the rest of the group at the kill turn:
/// the leader's deregistration landing first, at a host-chosen moment, must
/// neither log nor make the process a victim
/// (https://github.com/rrnewton/hermit/issues/4445). Nor does that of a
/// thread that reserved a SIGKILL of its own process: its send kills it, but
/// it is not among the victims the kill turn retires (the others are), so its
/// deregistration may land first. A SIGKILL the scheduler could not reserve,
/// only recorded after the send, retires nothing, so a death after it is
/// still awaited.
#[tokio::test]
async fn a_death_already_accounted_waits_for_nothing() {
    let process = DetPid::from_raw(LEADER.as_raw());
    for already in ["retired leader", "fallback sigkill", "reserved sigkill"] {
        let (state, time) = state();
        let (dying, sender_mm) = {
            let mut sched = state.sched.lock().unwrap();
            if already == "retired leader" {
                assert!(sched.retire_test_exited_leader(LEADER, MmId::initial(LEADER)));
                (LEADER, MmId::initial(LEADER))
            } else if already == "fallback sigkill" {
                sched.record_committed_sigkill(LEADER, Some(process));
                (WORKER, MmId::initial(LEADER))
            } else {
                for tid in [LEADER, WORKER] {
                    sched.record_thread_mm(tid, MmId::initial(LEADER));
                }
                sched
                    .reserve_sigkill(WORKER, process, 0)
                    .expect("the worker reserves a SIGKILL of its own process");
                (WORKER, MmId::initial(LEADER))
            }
        };
        let response = state
            .receive_rpc(
                Tid::from_raw(dying.as_raw()),
                (
                    time,
                    sender_mm,
                    GlobalRequest::DeregisterThread(deregistration(dying, true)),
                ),
            )
            .await;
        assert_eq!(response, (None, GlobalResponse::DeregisterThread(())));
        let sched = state.sched.lock().unwrap();
        assert_eq!(
            sched.signal_death_victim(process),
            already == "fallback sigkill",
            "{already}"
        );
        if already == "retired leader" {
            assert!(sched.next_turns.contains_key(&WORKER), "{already}");
        }
        if already == "reserved sigkill" {
            // The reserved victim waits for the kill turn.
            assert!(sched.next_turns.contains_key(&LEADER), "{already}");
        }
    }
}

/// A process whose only thread died of a signal that ends its group, as a
/// fault or an abort does, keeps that death's wait status as its zombie's
/// exit code (the signal, with `0x80` where it dumped core): recorded from
/// the deregistration before the thread is retired, so the `stat` that shows
/// the process exited also shows its code (revopus1's F2 on hermit 5139). A
/// death by anything else records nothing there: a raw `exit` or an
/// `exit_group` recorded its code in its own turn.
#[tokio::test]
async fn a_signal_death_keeps_its_wait_status_as_the_zombies_exit_code() {
    for (signaled, wait_status, shown) in [(true, 11 | 0x80, 11 | 0x80), (false, 3 << 8, 0)] {
        let (state, time) = state();
        state.sched.lock().unwrap().next_turns.remove(&WORKER);
        let mut leader = deregistration(LEADER, signaled);
        leader.wait_status = wait_status;
        let response = state
            .receive_rpc(
                Tid::from_raw(LEADER.as_raw()),
                (
                    time,
                    MmId::initial(LEADER),
                    GlobalRequest::DeregisterThread(leader),
                ),
            )
            .await;
        assert_eq!(response, (None, GlobalResponse::DeregisterThread(())));
        let sched = state.sched.lock().unwrap();
        assert!(!sched.next_turns.contains_key(&LEADER));
        assert_eq!(
            sched.task_stat_facts(LEADER, None).exit_code,
            shown,
            "signaled: {signaled}"
        );
    }
}
