/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1e A: the scheduler's side of IR-18's tri-state, fork-child
//! seeding, IR-23(a)'s temporary-mask publication and the committed-send
//! window record (`guest_mask_publication`), with the gate forced on in the
//! Detcore `Config` and the in-guest backend's capabilities. Each test names
//! the mutation it catches.

use super::guest_mask_publication::CommittedSendWindow;
use super::guest_mask_publication::PublishedGuestMask;
use super::guest_mask_publication::TemporaryMaskControl;
use super::*;

const PARENT: DetTid = DetTid::from_raw(100);
const THREAD: DetTid = DetTid::from_raw(101);
const CHILD: DetTid = DetTid::from_raw(102);

fn bit(signal: i32) -> u64 {
    kernel_signal_bit(signal)
}

/// An in-guest scheduler with the delivery gate `gate`: `PARENT` and its
/// thread `THREAD`, the kernel's `SigBlk` of `THREAD` scripted as `kernel`.
fn gated(gate: bool, kernel: u64) -> Scheduler {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: gate,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    let mut scheduler = Scheduler::new(&config);
    scheduler.thread_tree.add_child(PARENT, PARENT, true);
    scheduler.thread_tree.add_child(PARENT, THREAD, false);
    for thread in [PARENT, THREAD] {
        scheduler.register_thread_for_test(thread);
    }
    scheduler.test_kernel_signal_states.insert(
        THREAD,
        KernelSignalState {
            blocked: kernel,
            ..Default::default()
        },
    );
    scheduler
}

/// A request of `thread` that publishes `mask`.
fn publishing(thread: DetTid, mask: Option<u64>) -> Resources {
    let mut request = Resources::new(thread);
    request.guest_signal_mask = mask;
    request
}

/// IR-18: a rejoin readmitted at an ordered point (`keep_mask_for_continuation`)
/// that returned under the guest's own mask (`ContinuationMask::Restored`)
/// but published none no longer leaves the temporary mask its call published:
///
/// - on a backend that keeps no guest mask apart (the thread never published
///   its own mask: every backend before step 3c.1e B), nothing is published,
///   so every reader, the `/proc` status reader included, takes the kernel's
///   `SigBlk`, which is then the guest's own mask, and nothing refuses;
/// - on one that keeps it apart (the thread published its own mask at an
///   earlier request), the mask is `Unknown`: the status reader refuses
///   (`HERMIT_GUEST_MASK_UNKNOWN`), and the scheduler's own readers take
///   nothing published, the kernel's `SigBlk`.
///
/// A failed vfork's rejoin (`VforkFailed`) takes the same rule. The other
/// rejoins keep 3c.1b's answers: one that publishes its mask records it, a
/// `Temporary` one records the temporary mask, and an unknown one
/// (`rt_sigsuspend`) keeps the call's mask. With the gate off nothing is
/// recorded. Mutations: "entry kept" (the restored rows show the temporary
/// mask), "unknown for every rejoin that publishes nothing" (the
/// `rt_sigsuspend` row turns unknown), "unknown regardless of the backend"
/// (the rows before B turn unknown, and a status read would refuse).
#[test]
fn ir18_a_restored_rejoin_that_publishes_nothing_leaves_the_mask_unknown() {
    let own = bit(libc::SIGUSR2);
    let temporary = bit(libc::SIGTERM);
    let kernel = bit(libc::SIGUSR1) | bit(libc::SIGCHLD);
    for gate in [true, false] {
        for keeper in [false, true] {
            for kind in [
                "restored, none",
                "failed vfork",
                "restored, own",
                "temporary",
                "unknown",
            ] {
                let case = format!("{kind}, keeper {keeper}, gate {gate}");
                let mut scheduler = gated(gate, kernel);
                if keeper {
                    // An earlier request that published the thread's own mask.
                    scheduler.record_request_guest_mask(THREAD, &publishing(THREAD, Some(own)));
                }
                let mut call = publishing(THREAD, gate.then_some(temporary));
                call.blocked_signal_mask = Some(temporary);
                assert_eq!(scheduler.record_request_guest_mask(THREAD, &call), gate);
                let op = ExternalOpId::new(THREAD, 1);
                let mut rejoin = Resources::new(THREAD);
                let resource = if kind == "failed vfork" {
                    ResourceID::VforkFailed(op)
                } else {
                    ResourceID::BlockedExternalContinue(op)
                };
                rejoin.insert(resource, Permission::RW);
                match kind {
                    "restored, none" | "failed vfork" => {}
                    "restored, own" => rejoin.guest_signal_mask = Some(own),
                    "temporary" => {
                        rejoin.blocked_signal_mask = Some(temporary);
                        rejoin.guest_signal_mask = Some(temporary);
                    }
                    _ => rejoin.blocked_signal_mask_unknown = true,
                }
                if !gate {
                    rejoin.guest_signal_mask = None;
                }
                assert!(rejoin.is_rejoin(), "{case}");
                scheduler.next_turns.get_mut(&THREAD).unwrap().req = Ivar::full(Ok(rejoin));
                scheduler.keep_mask_for_continuation(THREAD);
                let want = match (gate, kind) {
                    (false, _) => None,
                    (true, "restored, none" | "failed vfork") => {
                        keeper.then_some(PublishedGuestMask::Unknown)
                    }
                    (true, "restored, own") => Some(PublishedGuestMask::Known(own)),
                    (true, _) => Some(PublishedGuestMask::Known(temporary)),
                };
                assert_eq!(scheduler.published_guest_mask_state(THREAD), want, "{case}");
                assert_eq!(scheduler.status_guest_mask(THREAD).mask, want, "{case}");
                let read = match want {
                    Some(PublishedGuestMask::Known(mask)) => mask,
                    _ => kernel,
                };
                assert_eq!(
                    scheduler.read_thread_blocked_mask_result(THREAD),
                    Ok(read),
                    "{case}: the scheduler's readers"
                );
                // The next request's publication ends an unknown mask.
                scheduler.record_request_guest_mask(THREAD, &publishing(THREAD, Some(own)));
                assert_eq!(
                    scheduler.published_guest_mask_state(THREAD),
                    gate.then_some(PublishedGuestMask::Known(own)),
                    "{case}: after the next request"
                );
            }
        }
    }
}

/// Fork-child seeding: a new task starts with what its creator published,
/// `Known` or `Unknown`, and nothing when its creator published nothing (so
/// its readers take its own `SigBlk`, which Linux copied from its
/// creator's: `arch_dup_task_struct`, kernel/fork.c:1086-1091 at v6.12). A
/// stale publication of a reused tid goes. With the gate off, nothing.
/// Mutation "no seed": every gated row with a parent publication fails.
#[test]
fn a_new_task_starts_with_the_mask_its_creator_published() {
    let own = bit(libc::SIGUSR2);
    for gate in [true, false] {
        for parent in [
            Some(PublishedGuestMask::Known(own)),
            Some(PublishedGuestMask::Unknown),
            None,
        ] {
            let case = format!("{parent:?}, gate {gate}");
            let mut scheduler = gated(gate, 0);
            scheduler.thread_tree.add_child(PARENT, CHILD, true);
            scheduler.register_thread_for_test(CHILD);
            scheduler.put_guest_mask_state(PARENT, parent);
            // A stale entry of a reused tid.
            scheduler.put_guest_mask_state(CHILD, Some(PublishedGuestMask::Known(!0)));
            scheduler.seed_child_guest_sigmask(PARENT, CHILD);
            let want = if gate {
                parent
            } else {
                Some(PublishedGuestMask::Known(!0))
            };
            assert_eq!(scheduler.published_guest_mask_state(CHILD), want, "{case}");
        }
    }
}

/// IR-23(a): a temporary-mask publication makes the published mask the
/// temporary one with D37's held masks, which the status reader gets, and
/// opens the committed-send window. A request the thread files supersedes it
/// (the held masks and the window go, nothing is put back); a withdrawal puts
/// back what was published before it (`Known`, `Unknown` or nothing) and
/// answers the window, once. Another thread cannot publish or withdraw for
/// it. With the gate off nothing is taken. Mutations: "withdrawal puts
/// nothing back" (the prior rows), "held masks dropped" (the status rows),
/// "a request does not supersede" (the held masks survive the request).
#[test]
fn a_temporary_mask_publication_holds_until_a_request_or_its_withdrawal() {
    let own = bit(libc::SIGUSR2);
    let temporary = bit(libc::SIGTERM);
    let urg = bit(libc::SIGURG);
    let winch = bit(libc::SIGWINCH);
    for gate in [true, false] {
        for prior in [
            Some(PublishedGuestMask::Known(own)),
            Some(PublishedGuestMask::Unknown),
            None,
        ] {
            let case = format!("{prior:?}, gate {gate}");
            let mut scheduler = gated(gate, 0);
            scheduler.put_guest_mask_state(THREAD, prior);
            let publish = TemporaryMaskControl::Publish {
                thread: THREAD,
                mask: temporary,
                held_private: urg,
                held_shared: winch,
            };
            // Not for another thread.
            assert_eq!(scheduler.temporary_mask_control(PARENT, publish), None);
            assert_eq!(
                scheduler.published_guest_mask_state(THREAD),
                prior,
                "{case}: refused for another sender"
            );
            assert_eq!(scheduler.temporary_mask_control(THREAD, publish), None);
            let status = scheduler.status_guest_mask(THREAD);
            if gate {
                assert_eq!(
                    status.mask,
                    Some(PublishedGuestMask::Known(temporary)),
                    "{case}"
                );
                assert_eq!(
                    (status.held_private, status.held_shared),
                    (urg, winch),
                    "{case}"
                );
            } else {
                assert_eq!(status.mask, prior, "{case}");
                assert_eq!((status.held_private, status.held_shared), (0, 0), "{case}");
            }
            scheduler.note_committed_send_in_window(THREAD, libc::SIGRTMIN() + 2, true);
            scheduler.note_committed_send_in_window(THREAD, libc::SIGUSR1, false);
            let withdraw = TemporaryMaskControl::Withdraw {
                thread: THREAD,
                keep_published: false,
            };
            let window = scheduler.temporary_mask_control(THREAD, withdraw);
            if gate {
                assert_eq!(
                    window,
                    Some(CommittedSendWindow {
                        sent_private: bit(libc::SIGRTMIN() + 2),
                        sent_shared: bit(libc::SIGUSR1),
                    }),
                    "{case}"
                );
            } else {
                assert_eq!(window, None, "{case}");
            }
            assert_eq!(
                scheduler.published_guest_mask_state(THREAD),
                prior,
                "{case}: put back"
            );
            assert_eq!(
                scheduler.status_guest_mask(THREAD).held_private,
                0,
                "{case}"
            );
            // Once only.
            assert_eq!(
                scheduler.temporary_mask_control(THREAD, withdraw),
                None,
                "{case}"
            );

            // A request supersedes a publication: nothing is put back, and
            // the held masks and the window go.
            scheduler.temporary_mask_control(THREAD, publish);
            scheduler.record_request_guest_mask(THREAD, &publishing(THREAD, Some(own)));
            let status = scheduler.status_guest_mask(THREAD);
            assert_eq!((status.held_private, status.held_shared), (0, 0), "{case}");
            scheduler.note_committed_send_in_window(THREAD, libc::SIGUSR1, false);
            assert_eq!(
                scheduler.temporary_mask_control(THREAD, withdraw),
                None,
                "{case}"
            );
            assert_eq!(
                scheduler.published_guest_mask_state(THREAD),
                if gate {
                    Some(PublishedGuestMask::Known(own))
                } else {
                    prior
                },
                "{case}: the request's own mask"
            );
        }
    }
}

/// The committed-send window record at the scheduler's own commit point
/// (`send_guest_signal`, here through its unit-test send, which stands for a
/// `kill` that succeeded): a thread-directed send joins the private queue's
/// record, a process-directed one the shared queue's, only while the target
/// holds a temporary-mask publication. Mutation "the scheduler's sends not
/// recorded": the window is empty.
#[test]
fn the_window_records_the_schedulers_sends_by_queue_while_a_publication_is_in_force() {
    let mut scheduler = gated(true, 0);
    scheduler.test_sent_signals = Some(Vec::new());
    // Not in force: nothing is recorded.
    scheduler.send_guest_signal(
        THREAD,
        Signal::SIGUSR1,
        None,
        None,
        THREAD,
        discarded_at_send::SendFate::Classify(None),
    );
    assert!(scheduler.publish_temporary_mask(THREAD, 0, 0, 0));
    scheduler.send_guest_signal(
        THREAD,
        Signal::SIGUSR2,
        Some(PARENT),
        None,
        THREAD,
        discarded_at_send::SendFate::Classify(None),
    );
    scheduler.send_guest_signal(
        THREAD,
        Signal::SIGALRM,
        None,
        None,
        THREAD,
        discarded_at_send::SendFate::Classify(None),
    );
    assert_eq!(
        scheduler.withdraw_temporary_mask(THREAD, false),
        Some(CommittedSendWindow {
            sent_private: bit(libc::SIGUSR2),
            sent_shared: bit(libc::SIGALRM),
        })
    );
}

/// IR-23(a) on an interrupted end: a withdrawal with `keep_published` (the
/// call returned `-ERESTARTNOHAND` or `-EINTR`) answers and closes the window
/// and drops the held masks, but leaves the temporary mask published, as
/// Linux does not restore the own mask at the return
/// (`restore_saved_sigmask_unless`, fs/select.c:301 at v6.12) and the
/// delivery keeps the temporary mask in `blocked` with the handler's
/// `sa_mask` and the signal until `rt_sigreturn` (`signal_delivered`,
/// kernel/signal.c:2951-2954); the thread's next request then publishes its
/// own. Mutation "always put back": the
/// temporary mask is gone.
#[test]
fn an_interrupted_end_keeps_the_temporary_mask_published() {
    let own = bit(libc::SIGUSR2);
    let temporary = bit(libc::SIGTERM);
    let mut scheduler = gated(true, 0);
    scheduler.put_guest_mask_state(THREAD, Some(PublishedGuestMask::Known(own)));
    assert!(scheduler.publish_temporary_mask(THREAD, temporary, bit(libc::SIGURG), 0));
    scheduler.note_committed_send_in_window(THREAD, libc::SIGUSR1, false);
    let withdraw = TemporaryMaskControl::Withdraw {
        thread: THREAD,
        keep_published: true,
    };
    assert_eq!(
        scheduler.temporary_mask_control(THREAD, withdraw),
        Some(CommittedSendWindow {
            sent_private: 0,
            sent_shared: bit(libc::SIGUSR1),
        })
    );
    let status = scheduler.status_guest_mask(THREAD);
    assert_eq!(status.mask, Some(PublishedGuestMask::Known(temporary)));
    assert_eq!((status.held_private, status.held_shared), (0, 0));
    scheduler.record_request_guest_mask(THREAD, &publishing(THREAD, Some(own)));
    assert_eq!(
        scheduler.published_guest_mask_state(THREAD),
        Some(PublishedGuestMask::Known(own))
    );
}

/// Fork-child seeding copies only the creator's own mask, never a masked
/// call's temporary mask the creator still publishes. Linux takes it down
/// before a ready or timed-out call returns (`restore_saved_sigmask_unless`,
/// fs/select.c:301 at v6.12), so a child forked then copies the own mask
/// (kernel/fork.c:1086-1091); after an interrupted end the delivery keeps it
/// in `blocked` with the handler's `sa_mask` and the signal until
/// `rt_sigreturn` (`signal_delivered`, kernel/signal.c:2951-2954), a mask
/// the scheduler does not hold until the delivery's request. The creator's
/// last publication:
///
/// - a park's request that publishes the temporary mask, a temporary-mask
///   publication in force, or one kept by an interrupted withdrawal: the
///   child takes what a rejoin that publishes nothing takes, `Unknown` where
///   the backend keeps the guest's mask apart (the creator published its own
///   mask at an earlier request), and otherwise nothing;
/// - the own mask put back after the park (`set_saved_guest_sigmask`, as a
///   ready end sends it), or a withdrawal that put back the own mask: the
///   child copies it.
///
/// Mutation "seed copies a temporary entry": the first rows seed the child
/// with the temporary mask.
#[test]
fn a_new_task_never_starts_with_its_creators_temporary_mask() {
    let own = bit(libc::SIGUSR2);
    let temporary = bit(libc::SIGTERM);
    let mut park = Resources::new(PARENT);
    park.blocked_signal_mask = Some(temporary);
    park.guest_signal_mask = Some(temporary);
    let publish = TemporaryMaskControl::Publish {
        thread: PARENT,
        mask: temporary,
        held_private: 0,
        held_shared: 0,
    };
    let withdraw = |keep_published| TemporaryMaskControl::Withdraw {
        thread: PARENT,
        keep_published,
    };
    type Step = Box<dyn Fn(&mut Scheduler, Option<u64>)>;
    let rows: Vec<(&str, Step, bool)> = vec![
        (
            "park",
            Box::new(move |s, _| assert!(s.record_request_guest_mask(PARENT, &park))),
            true,
        ),
        (
            "park, then the own mask put back",
            Box::new(move |s, own| {
                let mut park = Resources::new(PARENT);
                park.blocked_signal_mask = Some(temporary);
                park.guest_signal_mask = Some(temporary);
                assert!(s.record_request_guest_mask(PARENT, &park));
                assert!(s.set_saved_guest_sigmask(PARENT, own));
            }),
            false,
        ),
        (
            "publication in force",
            Box::new(move |s, _| {
                s.temporary_mask_control(PARENT, publish);
            }),
            true,
        ),
        (
            "publication kept by an interrupted end",
            Box::new(move |s, _| {
                s.temporary_mask_control(PARENT, publish);
                assert!(s.temporary_mask_control(PARENT, withdraw(true)).is_some());
            }),
            true,
        ),
        (
            "publication withdrawn",
            Box::new(move |s, _| {
                s.temporary_mask_control(PARENT, publish);
                assert!(s.temporary_mask_control(PARENT, withdraw(false)).is_some());
            }),
            false,
        ),
    ];
    for keeps_mask_apart in [true, false] {
        for (name, step, temporary_last) in &rows {
            let case = format!("{name}, keeps the mask apart {keeps_mask_apart}");
            let mut scheduler = gated(true, 0);
            scheduler.thread_tree.add_child(PARENT, CHILD, true);
            scheduler.register_thread_for_test(CHILD);
            let own_published = keeps_mask_apart.then_some(own);
            assert!(
                scheduler.record_request_guest_mask(PARENT, &publishing(PARENT, own_published))
            );
            step(&mut scheduler, own_published);
            scheduler.seed_child_guest_sigmask(PARENT, CHILD);
            let want = match (temporary_last, keeps_mask_apart) {
                (true, true) => Some(PublishedGuestMask::Unknown),
                (true, false) => None,
                (false, _) => own_published.map(PublishedGuestMask::Known),
            };
            assert_eq!(scheduler.published_guest_mask_state(CHILD), want, "{case}");
        }
    }
}

/// The temporary-mask mark (`Scheduler::temporary_guest_sigmasks`) at a
/// rejoin and at a withdrawal, seen through what a child is seeded with:
///
/// - a `Temporary` rejoin (readmitted under the call's temporary mask, after
///   the thread had published its own mask) marks the mask, so the child
///   takes `Unknown` where the backend keeps the guest's mask apart, and
///   otherwise nothing;
/// - a `Restored` rejoin after a masked call's request ends the mark: the
///   child copies the own mask the rejoin published (nothing on a backend
///   that keeps none);
/// - a withdrawal that puts back a prior that was itself a temporary mask (a
///   masked call's request, then a temporary-mask publication) keeps the
///   mark.
///
/// Mutations: "a Temporary rejoin not marked" and "a withdrawal clears the
/// mark whatever the prior" (those rows seed the temporary mask), "a
/// Restored rejoin with a published mask keeps the mark" (that row seeds
/// `Unknown` in place of the own mask).
#[test]
fn rejoins_and_withdrawals_decide_the_mark_a_child_is_seeded_from() {
    let own = bit(libc::SIGUSR2);
    let temporary = bit(libc::SIGTERM);
    for keeps_mask_apart in [true, false] {
        for row in [
            "temporary rejoin",
            "restored rejoin",
            "withdrawn to a temporary prior",
        ] {
            let case = format!("{row}, keeps the mask apart {keeps_mask_apart}");
            let own_published = keeps_mask_apart.then_some(own);
            let mut scheduler = gated(true, 0);
            scheduler.thread_tree.add_child(PARENT, CHILD, true);
            scheduler.register_thread_for_test(CHILD);
            assert!(
                scheduler.record_request_guest_mask(THREAD, &publishing(THREAD, own_published)),
                "{case}"
            );
            let mut call = publishing(THREAD, Some(temporary));
            call.blocked_signal_mask = Some(temporary);
            let rejoin = |temporary_rejoin: bool| {
                let mut rejoin = Resources::new(THREAD);
                rejoin.insert(
                    ResourceID::BlockedExternalContinue(ExternalOpId::new(THREAD, 1)),
                    Permission::RW,
                );
                if temporary_rejoin {
                    rejoin.blocked_signal_mask = Some(temporary);
                    rejoin.guest_signal_mask = Some(temporary);
                } else {
                    rejoin.guest_signal_mask = own_published;
                }
                assert!(rejoin.is_rejoin());
                rejoin
            };
            let temporary_last = match row {
                "temporary rejoin" => {
                    scheduler.next_turns.get_mut(&THREAD).unwrap().req =
                        Ivar::full(Ok(rejoin(true)));
                    scheduler.keep_mask_for_continuation(THREAD);
                    true
                }
                "restored rejoin" => {
                    assert!(scheduler.record_request_guest_mask(THREAD, &call), "{case}");
                    scheduler.next_turns.get_mut(&THREAD).unwrap().req =
                        Ivar::full(Ok(rejoin(false)));
                    scheduler.keep_mask_for_continuation(THREAD);
                    false
                }
                _ => {
                    assert!(scheduler.record_request_guest_mask(THREAD, &call), "{case}");
                    let publish = TemporaryMaskControl::Publish {
                        thread: THREAD,
                        mask: temporary,
                        held_private: 0,
                        held_shared: 0,
                    };
                    scheduler.temporary_mask_control(THREAD, publish);
                    let withdraw = TemporaryMaskControl::Withdraw {
                        thread: THREAD,
                        keep_published: false,
                    };
                    assert!(scheduler.temporary_mask_control(THREAD, withdraw).is_some());
                    true
                }
            };
            scheduler.seed_child_guest_sigmask(THREAD, CHILD);
            let want = match (temporary_last, keeps_mask_apart) {
                (true, true) => Some(PublishedGuestMask::Unknown),
                (true, false) => None,
                (false, _) => own_published.map(PublishedGuestMask::Known),
            };
            assert_eq!(scheduler.published_guest_mask_state(CHILD), want, "{case}");
        }
    }
}
