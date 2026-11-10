/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The other threads of a process retired at the delivery of the signal that
//! ends it (`GlobalRequest::CommitFatalDelivery`,
//! `Scheduler::commit_fatal_delivery`) are committed SIGKILL victims until
//! their deaths are accounted.

use super::*;

/// A sibling the receiver's delivery retired can still file a request before
/// the kernel ends it: a background operation's rejoin, or a plain request.
/// Neither is answered, so its callback stays pending until the backend
/// consumes the real death, and none panics for a thread that has left the
/// run set. Its consuming cleanup is answered, and its deregistration
/// releases the fence and retires nothing more: the receiver stays
/// registered until its own deregistration ends the process.
#[tokio::test]
async fn a_retired_siblings_late_request_stays_unanswered() {
    use std::sync::atomic::Ordering;

    use reverie::Tool;

    let config = Config {
        sequentialize_threads: true,
        ..Config::default()
    }
    .with_backend(|backend| *backend = reverie::BackendCapabilities::PTRACE);
    let [parent, receiver, sibling] = [1, 3, 5].map(DetTid::from_raw);
    for late in ["rejoin", "plain"] {
        let state = GlobalState::initialize(&config, false);
        {
            let mut sched = state.sched.lock().unwrap();
            sched.thread_tree.add_child(parent, parent, true);
            sched.thread_tree.add_child(parent, receiver, true);
            sched.thread_tree.add_child(receiver, sibling, false);
        }
        let tool: Detcore = Detcore::new(Tid::from_raw(receiver.as_raw()), &config);
        let guest_of = |tid: DetTid| {
            let mut thread = tool.init_thread_state(Tid::from_raw(tid.as_raw()), None);
            thread.detpid = Some(receiver);
            RetirementGuest {
                global: &state,
                config: &config,
                thread,
                requests: Mutex::new(Vec::new()),
                retired: std::sync::atomic::AtomicBool::new(false),
                hold_private_memory_publication: false,
                local_memory: false,
            }
        };
        let mut receiving = guest_of(receiver);
        let mut guest = guest_of(sibling);
        for (tid, mm) in [
            (parent, MmId::initial(parent)),
            (receiver, receiving.thread.mm_id),
            (sibling, guest.thread.mm_id),
        ] {
            install_test_registration(&state, tid, Ivar::new());
            state.sched.lock().unwrap().record_thread_mm(tid, mm);
        }
        state.sched.lock().unwrap().set_running_for_test(receiver);

        assert!(super::super::commit_fatal_delivery(&mut receiving, true).await);
        {
            let sched = state.sched.lock().unwrap();
            assert!(sched.is_unaccounted_sigkill_victim(sibling), "{late}");
            assert!(!sched.next_turns.contains_key(&sibling), "{late}");
            assert!(sched.next_turns.contains_key(&receiver), "{late}");
            assert!(sched.sigkill_fence_holds(), "{late}");
        }

        // Polled, then polled again after the runtime has run everything else
        // that was ready: the request is still unanswered.
        async fn stays_pending<F: std::future::Future>(call: F) -> bool {
            let mut call = std::pin::pin!(call);
            if !futures::poll!(call.as_mut()).is_pending() {
                return false;
            }
            tokio::task::yield_now().await;
            futures::poll!(call.as_mut()).is_pending()
        }
        let pending = if late == "plain" {
            stays_pending(super::super::send_and_update_time(
                &mut guest,
                GlobalRequest::GlobalTimeLowerBound,
            ))
            .await
        } else {
            let mut rejoin = Resources::new(sibling);
            rejoin.insert(
                ResourceID::BlockedExternalContinue(ExternalOpId::new(sibling, 7)),
                Permission::RW,
            );
            stays_pending(super::super::resource_request(&mut guest, rejoin)).await
        };
        assert!(pending, "{late}: the callback resumed");
        assert!(
            !guest.retired.load(Ordering::SeqCst),
            "{late}: retire_current_thread tail-injects exit"
        );
        assert_eq!(guest.requests.lock().unwrap().len(), 1, "{late}");

        // The consuming cleanup is admitted and answered.
        let (_, ack) = state
            .receive_rpc(
                Tid::from_raw(sibling.as_raw()),
                (
                    guest.thread.thread_logical_time.clone(),
                    guest.thread.mm_id,
                    GlobalRequest::RobustListWakes(Vec::new()),
                ),
            )
            .await;
        assert_eq!(ack, GlobalResponse::RobustListWakes(Vec::new()), "{late}");
        let (_, ack) = state
            .receive_rpc(
                Tid::from_raw(sibling.as_raw()),
                (
                    guest.thread.thread_logical_time.clone(),
                    guest.thread.mm_id,
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        dettid: sibling,
                        detpid: receiver,
                        mm: guest.thread.mm_id,
                        thread_start_entered: true,
                        vfork_origin: None,
                        timeslice_stats: TimesliceStats::default(),
                        syscall_count: 0,
                        syscall_histogram: Default::default(),
                        chaos_epochs: Vec::new(),
                        passed_descriptors_out: false,
                        wait_status: 0,
                        killed_by_group_signal: true,
                    }),
                ),
            )
            .await;
        assert_eq!(ack, GlobalResponse::DeregisterThread(()), "{late}");
        let sched = state.sched.lock().unwrap();
        assert!(!sched.is_unaccounted_sigkill_victim(sibling), "{late}");
        assert!(!sched.sigkill_fence_holds(), "{late}");
        assert!(
            sched.next_turns.contains_key(&receiver),
            "{late}: the victim's deregistration retired the receiver"
        );
    }
}
