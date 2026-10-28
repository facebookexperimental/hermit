/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Scheduler-level tests of POSIX timer coalescing
//! (<https://github.com/rrnewton/hermit/issues/4262>, `posix_timer_coalesce`).
//! Sends are captured (`Scheduler::test_sent_signals`), and the kernel's
//! queues are scripted (`test_kernel_signal_states`).

use nix::sys::signal::Signal;

use super::Scheduler;
use super::discarded_at_send::Consumption;
use super::discarded_at_send::DiscardToken;
use super::discarded_at_send::DiscardedSignalControl;
use super::discarded_at_send::QueueId;
use super::kernel_signal_bit;
use super::posix_timer_coalesce::DeliveryAnswer;
use super::signal_lists::Entry;
use super::signal_lists::FrontClass;
use super::signal_lists::KernelSiginfo;
use super::signal_lists::SI_TIMER;
use super::signal_lists::ordinary_siginfo;
use super::signal_lists::siginfo_code;
use super::test::queued_poller_for;
use super::test::register_known_thread_for;
use super::timed_waiters::SignalTarget;
use super::timed_waiters::TimedEvent;
use crate::config::Config;
use crate::resources::ResourceID;
use crate::syscalls::KernelSignalState;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::SigWrapper;

const USR1: i32 = libc::SIGUSR1;
const SI_QUEUE: i32 = -1;
const VALUE: u64 = 0x1122334455667788;

struct Family {
    sched: Scheduler,
    process: DetPid,
    leader: DetTid,
    sibling: DetTid,
}

/// Process 100 with threads 100 and 101, catching USR1, on the gated
/// scheduler, its sends captured.
fn family() -> Family {
    let mut config = Config {
        sequentialize_threads: true,
        ..Config::default()
    };
    detcore_model::config::set_kernel_signal_facts(&mut config.backend, true);
    config.backend.delivers_preemption_timer_events = false;
    let mut sched = Scheduler::new(&config);
    assert!(sched.coalesces_alarms());
    let leader = DetTid::from_raw(100);
    let sibling = DetTid::from_raw(101);
    sched.thread_tree.add_child(leader, leader, true);
    sched.thread_tree.add_child(leader, sibling, false);
    register_known_thread_for(&mut sched, leader);
    register_known_thread_for(&mut sched, sibling);
    sched.test_sent_signals = Some(Vec::new());
    let mut family = Family {
        sched,
        process: DetPid::from_raw(100),
        leader,
        sibling,
    };
    family.queues(0, 0);
    family
}

impl Family {
    /// Script the kernel's queues: the leader's private queue holds
    /// `private`, the shared queue `shared`.
    fn queues(&mut self, private: u64, shared: u64) {
        let caught = kernel_signal_bit(USR1);
        for (thread, thread_pending) in [(self.leader, private), (self.sibling, 0)] {
            self.sched.test_kernel_signal_states.insert(
                thread,
                KernelSignalState {
                    caught,
                    thread_pending,
                    shared_pending: shared,
                    ..Default::default()
                },
            );
        }
    }

    fn sent(&mut self) -> Vec<(DetTid, i32, Option<KernelSiginfo>, Option<DetPid>)> {
        std::mem::take(self.sched.test_sent_signals.as_mut().unwrap())
    }

    /// A guest send into `queue` from the leader, as the guest hooks record
    /// it (`DiscardedSignalControl::PreSend`, then `Sent`).
    fn guest_send(&mut self, queue: QueueId, siginfo: KernelSiginfo) {
        let token = self
            .sched
            .discard_pre_send(self.leader, queue, USR1, Some(self.leader));
        self.sched.discard_note_send(token, None, Some(siginfo));
    }

    /// A send into `queue` Linux discarded, whose physical copy carries
    /// `siginfo`.
    fn discarded_send(&mut self, queue: QueueId, siginfo: KernelSiginfo) {
        let token = DiscardToken {
            queue,
            signal: USR1,
            discarded: true,
            present: false,
            generation: 0,
        };
        self.sched.discard_note_send(token, None, Some(siginfo));
    }

    /// Create and arm timer `timer` notifying USR1 with `VALUE`, first due
    /// at `at` with `interval`, aimed at `target`.
    fn arm(&mut self, timer: i32, target: SignalTarget, at: u64, interval: u64) {
        self.sched
            .note_posix_timer_notification(self.process, timer, USR1, VALUE, false);
        self.sched.register_posix_timer(
            self.process,
            target,
            timer,
            Some(LogicalTime::from_nanos(at)),
            LogicalTime::from_nanos(interval),
            Signal::SIGUSR1.into(),
        );
    }

    /// Pop and dispatch the next timed event due by `now`.
    fn pop(&mut self, now: u64) {
        let (at, event) = self
            .sched
            .blocked
            .timed_waiters
            .pop_if_before(LogicalTime::from_nanos(now))
            .expect("a timer is due");
        let TimedEvent::SignalEvt(id, target, signal) = event else {
            panic!("a timer event, not {event}");
        };
        self.sched.committed_time = LogicalTime::from_nanos(now);
        self.sched
            .dispatch_timed_signal(at, id, target, signal, true, at);
    }

    /// A reported delivery stop of the leader at `at`, whose hooks then read
    /// the queues as scripted.
    fn stop(&mut self, at: u64, private_after: bool, shared_after: bool) -> DeliveryAnswer {
        let mut answer =
            self.sched
                .timer_signal_dequeue(self.leader, USR1, Some(LogicalTime::from_nanos(at)));
        // The delivery's turn asks for a timer entry's siginfo.
        if answer == DeliveryAnswer::TimerEntry {
            let info = self
                .sched
                .timer_delivery_info(self.leader, USR1, LogicalTime::from_nanos(at))
                .expect("a timer entry's siginfo");
            answer = DeliveryAnswer::Rewrite(info);
        }
        self.sched.discarded_signal_control(
            self.leader,
            DiscardedSignalControl::Consumed {
                signal: USR1,
                how: Consumption::KeptResume { blocked: false },
                thread_holds: private_after,
                process_holds: shared_after,
            },
        );
        answer
    }

    /// Whether `thread`'s request now carries a delivery of USR1, as a wake
    /// of a polling thread substitutes it (`force_unblock_thread_at`).
    fn woken(&self, thread: DetTid) -> bool {
        self.sched.next_turns[&thread]
            .req
            .try_read()
            .is_some_and(|request| {
                matches!(request, Ok(resources)
                    if resources.resources.contains_key(&ResourceID::InboundSignal(SigWrapper(USR1))))
            })
    }

    /// The leader blocks USR1, so a process-directed wake goes to the sibling,
    /// which polls.
    fn sibling_polls(&mut self) {
        self.sched
            .test_kernel_signal_states
            .get_mut(&self.leader)
            .unwrap()
            .blocked = kernel_signal_bit(USR1);
        queued_poller_for(&mut self.sched, self.sibling);
    }

    fn entries(&self, queue: QueueId) -> Vec<Entry> {
        self.sched.discarded_at_send.lists.entries(queue, USR1)
    }
}

fn shared(family: &Family) -> QueueId {
    QueueId::Process(family.process)
}

fn private(family: &Family) -> QueueId {
    QueueId::Thread(family.leader)
}

/// `rt_sigqueueinfo`'s `SI_QUEUE` record with a 64-bit `sival_ptr`.
fn queued() -> KernelSiginfo {
    let mut info = ordinary_siginfo(USR1, SI_QUEUE, 100, 1000);
    info.0[24..32].copy_from_slice(&VALUE.to_ne_bytes());
    info
}

fn forged_si_timer() -> KernelSiginfo {
    let mut info = ordinary_siginfo(USR1, SI_TIMER, 0, 0);
    info.0[24..32].copy_from_slice(&7u64.to_ne_bytes());
    info
}

fn timer_fields(info: &KernelSiginfo) -> (i32, i32, i32, u64) {
    let field = |at: usize| i32::from_ne_bytes(info.0[at..at + 4].try_into().unwrap());
    (
        siginfo_code(info),
        field(16),
        field(20),
        u64::from_ne_bytes(info.0[24..32].try_into().unwrap()),
    )
}

/// The headline case: a kill and a timer's expiry of one signal, both
/// pending. The expiry is queued behind the kill's entry without a send
/// (the kernel would merge it), and the kill's dequeue promotes it: two
/// deliveries, the second with the timer's own siginfo.
#[test]
fn a_timer_behind_a_pending_kill_is_promoted_at_its_dequeue() {
    let mut f = family();
    let sh = shared(&f);
    f.guest_send(sh, ordinary_siginfo(USR1, 0, 100, 1000));
    f.queues(0, kernel_signal_bit(USR1));
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    assert!(f.sent().is_empty(), "the expiry merges, so nothing is sent");
    assert!(matches!(
        f.entries(sh).as_slice(),
        [Entry::Plain { .. }, Entry::Timer(t)] if t.timer == 0
    ));

    f.queues(0, 0);
    assert_eq!(f.stop(20, false, false), DeliveryAnswer::AsIs);
    let sent = f.sent();
    assert_eq!(sent.len(), 1, "the promotion");
    let info = sent[0].2.expect("the promotion carries SI_TIMER");
    assert_eq!(timer_fields(&info), (SI_TIMER, 0, 0, VALUE));
    assert_eq!(sent[0].3, None, "process-directed");

    f.queues(0, kernel_signal_bit(USR1));
    let DeliveryAnswer::Rewrite(info) = f.stop(30, false, false) else {
        panic!("the timer's entry is delivered with its own siginfo");
    };
    assert_eq!(timer_fields(&info), (SI_TIMER, 0, 0, VALUE));
    assert!(f.entries(sh).is_empty());
    assert!(f.sent().is_empty());
}

/// A periodic timer waits for its entry's dequeue, which reports the missed
/// periods and re-arms it on its grid after the consuming thread's time.
#[test]
fn a_periodic_timer_waits_for_its_dequeue_and_reports_the_overrun() {
    let mut f = family();
    f.arm(0, SignalTarget::Process(f.leader), 10, 10);
    f.pop(10);
    assert_eq!(f.sent().len(), 1, "the first expiry is sent");
    let id = super::timed_waiters::SignalTimerId::Posix(f.process, 0);
    assert!(
        f.sched.blocked.timed_waiters.next_deadline().is_none(),
        "no recurrence until the dequeue"
    );
    f.queues(0, kernel_signal_bit(USR1));
    let DeliveryAnswer::Rewrite(info) = f.stop(55, false, false) else {
        panic!("rewritten");
    };
    assert_eq!(timer_fields(&info).2, 4, "periods at 20, 30, 40, 50 missed");
    assert_eq!(f.sched.posix_timer_overrun(f.process, 0), 4);
    assert_eq!(
        f.sched
            .blocked
            .timed_waiters
            .signal_timer_state(id)
            .map(|s| s.0),
        Some(LogicalTime::from_nanos(60))
    );
}

/// A delivery's siginfo is the timer's as the stop dequeued it: a
/// `timer_delete` another thread commits before the delivery's turn changes
/// neither its `si_value` nor its overrun, which Linux fills at the dequeue.
#[test]
fn a_delivery_reports_the_timer_as_its_stop_found_it() {
    let mut f = family();
    f.arm(0, SignalTarget::Process(f.leader), 10, 10);
    f.pop(10);
    f.queues(0, kernel_signal_bit(USR1));
    assert_eq!(
        f.sched
            .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(55))),
        DeliveryAnswer::TimerEntry
    );
    // Deleted between the stop and the delivery's turn.
    f.sched
        .note_posix_timer_notification(f.process, 0, USR1, VALUE, true);
    f.sched.register_posix_timer(
        f.process,
        SignalTarget::Process(f.leader),
        0,
        None,
        LogicalTime::from_nanos(0),
        Signal::SIGUSR1.into(),
    );
    let info = f
        .sched
        .timer_delivery_info(f.leader, USR1, LogicalTime::from_nanos(55))
        .expect("a timer entry's siginfo");
    assert_eq!(
        timer_fields(&info),
        (SI_TIMER, 0, 4, VALUE),
        "the stop's si_value, and periods 20 to 50 missed on the stop's grid"
    );
}

/// An entry whose timer was set again since it was queued is skipped at the
/// stop, and its pop sends what is behind it.
#[test]
fn a_stale_front_is_suppressed_and_promotes_what_is_behind_it() {
    let mut f = family();
    let sh = shared(&f);
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    f.queues(0, kernel_signal_bit(USR1));
    f.arm(1, SignalTarget::Process(f.leader), 11, 0);
    f.pop(11);
    assert_eq!(f.entries(sh).len(), 2);
    f.sent();
    // Timer 0 is set again: its entry is stale.
    f.arm(0, SignalTarget::Process(f.leader), 1_000, 0);
    f.queues(0, 0);
    assert_eq!(f.stop(20, false, false), DeliveryAnswer::Suppress);
    let sent = f.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(timer_fields(&sent[0].2.unwrap()).1, 1, "timer 1's entry");
}

/// Codex's intermediate state: at the stop's read the identified queue's
/// bit is clear even though an entry remains, since the promotion has not
/// been sent yet; I1 holds again after it.
#[test]
fn the_stop_read_before_the_promotion_sees_the_identified_queue_clear() {
    let mut f = family();
    let sh = shared(&f);
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    f.queues(0, kernel_signal_bit(USR1));
    f.arm(1, SignalTarget::Process(f.leader), 11, 0);
    f.pop(11);
    f.sent();
    f.queues(0, 0);
    assert!(matches!(
        f.stop(20, false, false),
        DeliveryAnswer::Rewrite(_)
    ));
    assert_eq!(f.sent().len(), 1, "promoted after the read");
    assert_eq!(f.entries(sh).len(), 1, "the promoted entry is the front");
}

#[test]
#[should_panic(expected = "still held")]
fn the_identified_bit_set_at_the_stop_read_is_refused() {
    let mut f = family();
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    f.queues(0, kernel_signal_bit(USR1));
    f.stop(20, false, true);
}

#[test]
#[should_panic(expected = "held it")]
fn the_other_queue_set_with_no_list_and_no_discarded_bit_is_refused() {
    let mut f = family();
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    f.queues(0, kernel_signal_bit(USR1));
    f.stop(20, true, false);
}

#[test]
#[should_panic(expected = "did not hold")]
fn the_other_queue_clear_with_a_list_is_refused() {
    let mut f = family();
    let pr = private(&f);
    f.guest_send(pr, ordinary_siginfo(USR1, libc::SI_TKILL, 100, 1000));
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    f.sched
        .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(20)));
    // The private list was dequeued; the shared one still has an entry.
    f.sched.finish_timer_signal_dequeue(
        f.leader,
        USR1,
        Consumption::KeptResume { blocked: false },
        false,
        false,
    );
}

#[test]
#[should_panic(expected = "still held")]
fn the_identified_bit_still_set_in_the_take_after_set_is_refused() {
    let mut f = family();
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    let bit = kernel_signal_bit(USR1);
    f.sched
        .timer_signal_take(f.leader, USR1, LogicalTime::from_nanos(20), Some((0, bit)));
}

/// A take returns the timer's entry with its siginfo and promotes the next.
#[test]
fn a_take_rewrites_a_timer_entry_and_promotes() {
    let mut f = family();
    let sh = shared(&f);
    f.guest_send(sh, ordinary_siginfo(USR1, 0, 100, 1000));
    f.queues(0, kernel_signal_bit(USR1));
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    let first =
        f.sched
            .timer_signal_take(f.leader, USR1, LogicalTime::from_nanos(20), Some((0, 0)));
    assert!(
        !first.skip && first.rewrite.is_none(),
        "the kill's own record"
    );
    assert_eq!(f.sent().len(), 1, "promoted before the next take");
    let second =
        f.sched
            .timer_signal_take(f.leader, USR1, LogicalTime::from_nanos(20), Some((0, 0)));
    assert_eq!(timer_fields(&second.rewrite.unwrap()).0, SI_TIMER);
    assert!(f.entries(sh).is_empty());
}

#[test]
#[should_panic(expected = "cannot be told")]
fn a_faulted_take_that_emptied_two_listed_queues_is_refused() {
    let mut f = family();
    let pr = private(&f);
    let sh = shared(&f);
    f.guest_send(pr, ordinary_siginfo(USR1, libc::SI_TKILL, 100, 1000));
    f.guest_send(sh, ordinary_siginfo(USR1, 0, 100, 1000));
    let set = kernel_signal_bit(USR1);
    f.sched
        .timer_signal_take_faulted(f.leader, set, 0, 0, LogicalTime::from_nanos(20));
}

/// N8-3: a faulted take that emptied no listed queue is 4351's (`take_faulted`).
#[test]
fn a_faulted_take_of_an_unlisted_signal_takes_the_no_list_branch() {
    let mut f = family();
    let set = kernel_signal_bit(USR1) | kernel_signal_bit(libc::SIGUSR2);
    assert!(
        !f.sched
            .timer_signal_take_faulted(f.leader, set, 0, 0, LogicalTime::from_nanos(20))
    );
}

/// A promotion is Linux's entry queued at its own expiry: kept, whatever
/// the target's disposition now (`SendFate::Kept`).
#[test]
fn a_promotion_is_kept_even_when_the_signal_is_now_ignored() {
    let mut f = family();
    let sh = shared(&f);
    f.guest_send(sh, ordinary_siginfo(USR1, 0, 100, 1000));
    f.queues(0, kernel_signal_bit(USR1));
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    // USR1 becomes ignored (as if SIG_IGN without a flush).
    for thread in [f.leader, f.sibling] {
        let state = f.sched.test_kernel_signal_states.get_mut(&thread).unwrap();
        state.caught = 0;
        state.ignored = kernel_signal_bit(USR1);
        state.shared_pending = 0;
    }
    f.stop(20, false, false);
    assert_eq!(f.sent().len(), 1);
    assert!(!f.sched.discarded_at_send.is_discarded(sh, USR1));
}

/// N10-2: a `Plain` appended before the pair had a timer stays `Own`, so a
/// later stop delivers its record unchanged.
#[test]
fn a_plain_appended_before_the_first_timer_is_not_rewritten() {
    let mut f = family();
    let pr = private(&f);
    f.discarded_send(pr, forged_si_timer());
    f.guest_send(pr, queued());
    f.sched
        .note_posix_timer_notification(f.process, 0, USR1, VALUE, false);
    f.queues(kernel_signal_bit(USR1), 0);
    assert_eq!(f.stop(20, false, false), DeliveryAnswer::AsIs);
}

/// In a timer pair, a kept `SI_QUEUE` send standing on a discarded forged
/// `SI_TIMER` copy is a `Proxy`, rewritten at the stop from its stored
/// record (the shared-domain form no e2e construction reaches, 4034).
#[test]
fn a_proxy_plain_is_rewritten_from_its_stored_record() {
    let mut f = family();
    let sh = shared(&f);
    f.sched
        .discarded_at_send
        .note_timer_history(f.process, USR1);
    f.discarded_send(sh, forged_si_timer());
    f.guest_send(sh, queued());
    assert_eq!(
        f.sched.discarded_at_send.lists.front(sh, USR1).map(|f| f.1),
        Some(FrontClass::Proxy)
    );
    f.queues(0, kernel_signal_bit(USR1));
    assert_eq!(f.stop(20, false, false), DeliveryAnswer::Rewrite(queued()));
}

#[test]
#[should_panic(expected = "unreported")]
fn a_proxy_passed_unreported_is_refused() {
    let mut f = family();
    let sh = shared(&f);
    f.sched
        .discarded_at_send
        .note_timer_history(f.process, USR1);
    f.discarded_send(sh, forged_si_timer());
    f.guest_send(sh, queued());
    f.sched.timer_signal_dequeue(f.leader, USR1, None);
}

/// Exec, group 1: in a timer pair, a `Proxy` `Plain` on a private `SI_TIMER`
/// record the kernel flushed is sent again exactly once (`tgkill` to the
/// survivor), keeps its stored 48 bytes and its class, and the stop after
/// exec rewrites it to the stored record; the physical record is now the
/// re-send.
#[test]
fn exec_resends_a_proxy_plain_and_the_stop_after_it_rewrites() {
    let mut f = family();
    let pr = private(&f);
    f.sched
        .discarded_at_send
        .note_timer_history(f.process, USR1);
    f.discarded_send(pr, forged_si_timer());
    f.guest_send(pr, queued());
    f.sent();
    let flush = f.sched.take_exec_timer_flush(f.leader, f.process).unwrap();
    assert_eq!((flush.own, flush.shared), (0, 0));
    let sent = f.sent();
    assert_eq!(
        sent,
        vec![(f.leader, USR1, None, Some(f.process))],
        "one tgkill"
    );
    let (front, class, physical_si_timer) =
        f.sched.discarded_at_send.lists.front(pr, USR1).unwrap();
    assert!(matches!(front, Entry::Plain { siginfo, .. } if siginfo == queued()));
    assert_eq!(class, FrontClass::Proxy);
    assert!(!physical_si_timer);
    assert!(f.sched.discarded_at_send.has_timer_history(f.process, USR1));
    f.queues(kernel_signal_bit(USR1), 0);
    assert_eq!(f.stop(30, false, false), DeliveryAnswer::Rewrite(queued()));
}

/// Exec, groups 2 to 4: a timer-free `Plain` over a flushed record, a
/// forged `Plain` in either kind of pair, and a `Plain` appended before the
/// first timer are each popped, with nothing sent.
#[test]
fn exec_pops_every_plain_that_is_not_a_proxy_over_a_flushed_record() {
    for (history_before, history_after, forged) in [
        (false, false, false),
        (false, false, true),
        (true, true, true),
        (false, true, false),
    ] {
        let mut f = family();
        let pr = private(&f);
        if history_before {
            f.sched
                .discarded_at_send
                .note_timer_history(f.process, USR1);
        }
        f.discarded_send(pr, forged_si_timer());
        f.guest_send(pr, if forged { forged_si_timer() } else { queued() });
        if history_after {
            f.sched
                .discarded_at_send
                .note_timer_history(f.process, USR1);
        }
        f.sent();
        f.sched.take_exec_timer_flush(f.leader, f.process).unwrap();
        assert!(
            f.sent().is_empty(),
            "case {history_before} {history_after} {forged}"
        );
        assert!(f.entries(pr).is_empty());
    }
}

/// N9-5: a thread's private lists go with it, and the `SIGEV_THREAD_ID`
/// timer its entry named re-arms, so its next expiry takes the exited-thread
/// path instead of finding itself ALREADY_PENDING.
#[test]
fn a_dead_threads_entries_go_and_their_timer_rearms() {
    let mut f = family();
    let sibling = f.sibling;
    f.arm(0, SignalTarget::Thread(sibling), 10, 10);
    f.pop(10);
    assert_eq!(f.entries(QueueId::Thread(sibling)).len(), 1);
    assert!(f.sched.blocked.timed_waiters.next_deadline().is_none());
    let mm = crate::types::MmId::initial(f.process);
    let process = f.process;
    f.sched.logically_kill_thread(&sibling, &process, mm);
    assert!(f.entries(QueueId::Thread(sibling)).is_empty());
    assert!(
        f.sched
            .discarded_at_send
            .lists
            .timer_entry(f.process, 0)
            .is_none()
    );
    assert_eq!(
        f.sched.blocked.timed_waiters.next_deadline(),
        Some(LogicalTime::from_nanos(20))
    );
}

/// R5-2 (rev 7 F): a stale entry refreshed in place by its timer's next
/// expiry makes the signal deliverable with no physical send, so the single
/// target is woken in that turn.
#[test]
fn a_stale_entry_refreshed_in_place_wakes_the_target() {
    let mut f = family();
    f.sibling_polls();
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    assert_eq!(f.sent().len(), 1);
    // Set again while queued: the entry is stale and nothing is deliverable.
    f.arm(0, SignalTarget::Process(f.leader), 15, 0);
    assert!(!f.woken(f.sibling));
    f.pop(15);
    assert!(f.sent().is_empty(), "refreshed in place, nothing sent");
    assert!(f.woken(f.sibling), "the refresh wakes the sibling");
}

/// R5-2 (rev 7 F): a timer's entry appended behind a stale head is
/// deliverable with no physical send, so the single target is woken.
#[test]
fn an_entry_appended_behind_a_stale_head_wakes_the_target() {
    let mut f = family();
    f.sibling_polls();
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    f.arm(0, SignalTarget::Process(f.leader), 1_000, 0);
    f.arm(1, SignalTarget::Process(f.leader), 15, 0);
    f.sent();
    f.pop(15);
    assert!(f.sent().is_empty(), "behind the head, nothing sent");
    assert_eq!(f.entries(shared(&f)).len(), 2);
    assert!(f.woken(f.sibling), "the append wakes the sibling");
}

/// `timer_delete` removes the timer's park (PT:1006-1011).
#[test]
fn a_deleted_timers_park_goes() {
    let mut f = family();
    for thread in [f.leader, f.sibling] {
        let state = f.sched.test_kernel_signal_states.get_mut(&thread).unwrap();
        state.caught = 0;
        state.ignored = kernel_signal_bit(USR1);
    }
    f.arm(0, SignalTarget::Process(f.leader), 10, 10);
    f.pop(10);
    assert_eq!(
        f.sched
            .discarded_at_send
            .parked_timers(f.process, USR1)
            .len(),
        1
    );
    f.sched
        .note_posix_timer_notification(f.process, 0, USR1, 0, true);
    assert!(
        f.sched
            .discarded_at_send
            .parked_timers(f.process, USR1)
            .is_empty()
    );
}

/// R5-2 (rev 7 F): an unignore whose parked expiry's discarded copy is still
/// pending lets that copy stand for the re-queued entry, with no physical
/// send, so the single target is woken; the caller, running, is not.
#[test]
fn a_copy_standing_for_an_unignored_entry_wakes_the_target() {
    let mut f = family();
    for thread in [f.leader, f.sibling] {
        let state = f.sched.test_kernel_signal_states.get_mut(&thread).unwrap();
        state.caught = 0;
        state.ignored = kernel_signal_bit(USR1);
    }
    f.arm(0, SignalTarget::Process(f.leader), 10, 10);
    f.pop(10);
    let sh = shared(&f);
    assert!(
        f.sched.discarded_at_send.is_discarded(sh, USR1),
        "parked, its copy discarded"
    );
    f.sent();
    // Now the leader blocks USR1 (it would have been discarded otherwise),
    // and the sibling polls.
    f.sibling_polls();
    assert!(!f.woken(f.sibling));
    f.sched
        .discarded_signal_control(f.leader, DiscardedSignalControl::Unignored { signal: USR1 });
    assert!(
        f.sent().is_empty(),
        "the discarded copy stands; nothing is sent"
    );
    assert_eq!(
        f.sched
            .discarded_at_send
            .lists
            .front(sh, USR1)
            .map(|front| front.1),
        Some(FrontClass::Proxy)
    );
    assert!(!f.sched.discarded_at_send.is_discarded(sh, USR1));
    assert!(
        f.woken(f.sibling),
        "the copy-stands unignore wakes the sibling"
    );
}

/// An unignore whose copy stands for the entry, with the caller the only
/// thread the signal can go to: the caller runs, and is not woken (a wake
/// would assert it parked). Kills "the caller rule in `wake_for_entry`".
#[test]
fn a_copy_standing_for_an_unignored_entry_does_not_wake_its_caller() {
    let mut f = family();
    for thread in [f.leader, f.sibling] {
        let state = f.sched.test_kernel_signal_states.get_mut(&thread).unwrap();
        state.caught = 0;
        state.ignored = kernel_signal_bit(USR1);
    }
    f.arm(0, SignalTarget::Process(f.leader), 10, 10);
    f.pop(10);
    f.sent();
    // The sibling blocks USR1, so the leader, the caller, is the target.
    f.sched
        .test_kernel_signal_states
        .get_mut(&f.sibling)
        .unwrap()
        .blocked = kernel_signal_bit(USR1);
    queued_poller_for(&mut f.sched, f.leader);
    f.sched
        .discarded_signal_control(f.leader, DiscardedSignalControl::Unignored { signal: USR1 });
    assert!(!f.woken(f.leader), "the running caller is not woken");
}

/// An unreported pass of a stale timer entry drops it, as Linux skips it
/// inside the dequeue (§4.10).
#[test]
fn an_unreported_stale_entry_is_dropped() {
    let mut f = family();
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    f.arm(0, SignalTarget::Process(f.leader), 1_000, 0);
    assert_eq!(
        f.sched.timer_signal_dequeue(f.leader, USR1, None),
        DeliveryAnswer::Suppress
    );
}

/// Exec takes back a kept physical record whose only entry it removed: a
/// timer entry standing, as a `Proxy`, on a discarded copy that is not
/// `SI_TIMER` (D1 + exec). Behind a copy of the same signal in the
/// survivor's own queue, that record cannot be taken alone: refused by name.
#[test]
fn exec_takes_back_a_kept_record_or_refuses_behind_a_private_copy() {
    let mut f = family();
    let sh = shared(&f);
    f.discarded_send(sh, ordinary_siginfo(USR1, 0, 100, 1000));
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.queues(0, kernel_signal_bit(USR1));
    f.pop(10);
    assert_eq!(
        f.sched
            .discarded_at_send
            .lists
            .front(sh, USR1)
            .map(|front| front.1),
        Some(FrontClass::Proxy)
    );
    let flush = f.sched.take_exec_timer_flush(f.leader, f.process).unwrap();
    assert_eq!(flush.shared, kernel_signal_bit(USR1), "taken back");
    assert_eq!(flush.own, 0);
    assert!(f.entries(sh).is_empty());

    let mut f = family();
    let sh = shared(&f);
    f.discarded_send(sh, ordinary_siginfo(USR1, 0, 100, 1000));
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.queues(kernel_signal_bit(USR1), kernel_signal_bit(USR1));
    f.pop(10);
    let refusal = f
        .sched
        .take_exec_timer_flush(f.leader, f.process)
        .unwrap_err();
    assert!(
        refusal.contains("behind another copy") && refusal.contains("own queue"),
        "{refusal}"
    );
}

/// Exec's kernel flush removes a discarded `SI_TIMER` copy, so its bit goes.
#[test]
fn exec_forgets_a_discarded_si_timer_copy_the_kernel_flushed() {
    let mut f = family();
    let pr = private(&f);
    f.discarded_send(pr, forged_si_timer());
    assert!(f.sched.discarded_at_send.discarded_is_si_timer(pr, USR1));
    f.sched.take_exec_timer_flush(f.leader, f.process).unwrap();
    assert!(!f.sched.discarded_at_send.is_discarded(pr, USR1));
}

/// Disclosure 11: the timer history survives exec on the path that deletes
/// the process's timers (`exit_itimers`) as on the flush.
#[test]
fn the_timer_history_survives_the_exec_that_deletes_the_timers() {
    let mut f = family();
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    assert!(f.sched.discarded_at_send.has_timer_history(f.process, USR1));
    f.sched.delete_exec_posix_timers(f.process);
    f.sched.take_exec_timer_flush(f.leader, f.process).unwrap();
    assert!(f.sched.discarded_at_send.has_timer_history(f.process, USR1));
}

/// A forked child starts without its parent's timer history (N10-2's
/// condition), copies its parent's handler masks (P3-3), and its exit
/// signal ends the parent's pair's observability at its creation (§3.4).
#[test]
fn a_forked_child_takes_handler_masks_but_no_timer_history() {
    let mut f = family();
    f.arm(0, SignalTarget::Process(f.leader), 1_000, 0);
    f.sched.discarded_signal_control(
        f.leader,
        DiscardedSignalControl::Handler {
            signal: USR1,
            caught: true,
            mask: kernel_signal_bit(libc::SIGUSR2),
            nodefer: false,
        },
    );
    let child = DetTid::from_raw(200);
    f.sched
        .thread_tree
        .add_child_with_wait_metadata(f.leader, child, true, false, USR1);
    register_known_thread_for(&mut f.sched, child);
    f.sched.child_ends_timer_observability(child);
    let child_process = DetPid::from_raw(200);
    assert!(f.sched.discarded_at_send.has_timer_history(f.process, USR1));
    assert!(
        !f.sched
            .discarded_at_send
            .has_timer_history(child_process, USR1)
    );
    assert_eq!(
        f.sched.handler_masks.get(&(child_process, USR1)),
        Some(&(kernel_signal_bit(libc::SIGUSR2), false))
    );
    assert!(
        f.sched.signal_unobservable.contains(&(f.process, USR1)),
        "the child's exit signal ends the parent's USR1 pair"
    );
}

/// Arming ITIMER_REAL ends the SIGALRM pair's observability at the arm.
#[test]
fn arming_itimer_real_ends_the_sigalrm_pair() {
    let mut f = family();
    let t = LogicalTime::from_nanos;
    f.sched
        .register_alarm(f.process, f.leader, t(0), t(1_000), t(0), Signal::SIGALRM);
    assert!(
        f.sched
            .signal_unobservable
            .contains(&(f.process, libc::SIGALRM))
    );
}

/// The R2b-1 seam: a list holding only stale timer entries is reported as
/// not deliverable (`discarded_bits`); a valid one is not.
#[test]
fn a_stale_only_list_is_not_deliverable_to_the_wake_rules() {
    let mut f = family();
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    assert_eq!(
        f.sched.discarded_bits(f.leader).1 & kernel_signal_bit(USR1),
        0
    );
    f.arm(0, SignalTarget::Process(f.leader), 1_000, 0);
    assert_ne!(
        f.sched.discarded_bits(f.leader).1 & kernel_signal_bit(USR1),
        0
    );
}

/// A transition with a single front standing as a `Proxy` is refused (D1 +
/// transition): the fallback would deliver another sender's record.
#[test]
#[should_panic(expected = "stopped being observable")]
fn a_transition_with_a_proxy_front_is_refused() {
    let mut f = family();
    let sh = shared(&f);
    f.discarded_send(sh, ordinary_siginfo(USR1, 0, 100, 1000));
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    f.sched.lose_timer_observability(f.process, USR1);
}

/// A transition while a current ignored expiry is parked is refused.
#[test]
#[should_panic(expected = "is parked")]
fn a_transition_with_a_current_park_is_refused() {
    let mut f = family();
    for thread in [f.leader, f.sibling] {
        let state = f.sched.test_kernel_signal_states.get_mut(&thread).unwrap();
        state.caught = 0;
        state.ignored = kernel_signal_bit(USR1);
    }
    f.arm(0, SignalTarget::Process(f.leader), 10, 10);
    f.pop(10);
    f.sched.lose_timer_observability(f.process, USR1);
}

/// P3-3: a promotion at a delivery stop is aimed with the mask the
/// receiver's handler frame leaves it with: its handler blocks USR1, so the
/// sibling takes the next entry.
#[test]
fn a_promotion_at_a_delivery_goes_where_the_handler_mask_allows() {
    let mut f = family();
    f.sched.discarded_signal_control(
        f.leader,
        DiscardedSignalControl::Handler {
            signal: USR1,
            caught: true,
            mask: 0,
            nodefer: false,
        },
    );
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    f.queues(0, kernel_signal_bit(USR1));
    f.arm(1, SignalTarget::Process(f.leader), 11, 0);
    f.pop(11);
    f.sent();
    f.queues(0, 0);
    f.stop(20, false, false);
    let sent = f.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, f.sibling, "the leader's handler blocks USR1");
}

/// The queue comes from the model, private first: a private discarded copy
/// with no list beside a shared timer list is what a stop takes, so the
/// shared entry stays (rev 7 G, "queue chosen from physical reads").
#[test]
fn a_private_discarded_copy_is_taken_before_a_shared_timer_entry() {
    let mut f = family();
    let (pr, sh) = (private(&f), shared(&f));
    f.discarded_send(pr, ordinary_siginfo(USR1, libc::SI_TKILL, 100, 1000));
    f.arm(0, SignalTarget::Process(f.leader), 10, 0);
    f.pop(10);
    f.queues(kernel_signal_bit(USR1), kernel_signal_bit(USR1));
    assert_eq!(f.stop(20, false, true), DeliveryAnswer::AsIs);
    assert_eq!(f.entries(sh).len(), 1, "the shared timer entry stays");
}

/// A wake with no physical send records the signal as left in the shared
/// queue when every thread blocks it, as a send would
/// (`note_signal_left_in_shared_queue`).
#[test]
fn a_wake_with_every_thread_blocking_keeps_the_shared_queue_record() {
    let mut f = family();
    f.arm(0, SignalTarget::Process(f.sibling), 10, 0);
    f.pop(10);
    f.arm(0, SignalTarget::Process(f.sibling), 1_000, 0);
    queued_poller_for(&mut f.sched, f.sibling);
    for thread in [f.leader, f.sibling] {
        f.sched
            .test_kernel_signal_states
            .get_mut(&thread)
            .unwrap()
            .blocked = kernel_signal_bit(USR1);
    }
    f.arm(1, SignalTarget::Process(f.sibling), 15, 0);
    f.pop(15);
    assert_ne!(
        f.sched
            .shared_queue_scheduler_signals
            .get(&f.process)
            .copied()
            .unwrap_or(0)
            & kernel_signal_bit(USR1),
        0
    );
}

/// Exec re-sends a shared `Proxy` `Plain` with `kill` (no thread), and the
/// survivor, running its exec, is not woken by its own re-send (P2-3).
#[test]
fn exec_resends_a_shared_proxy_plain_with_kill_and_wakes_no_one() {
    let mut f = family();
    let sh = shared(&f);
    f.sched
        .discarded_at_send
        .note_timer_history(f.process, USR1);
    f.discarded_send(sh, forged_si_timer());
    f.guest_send(sh, queued());
    f.sent();
    f.sched.test_send_wakes.clear();
    f.sched.take_exec_timer_flush(f.leader, f.process).unwrap();
    assert_eq!(f.sent(), vec![(f.leader, USR1, None, None)], "one kill");
    assert!(
        f.sched.test_send_wakes.is_empty(),
        "the exec caller is not woken"
    );
    let (_, class, physical_si_timer) = f.sched.discarded_at_send.lists.front(sh, USR1).unwrap();
    assert_eq!(class, FrontClass::Proxy);
    assert!(
        !physical_si_timer,
        "I1: the physical record is now the re-send"
    );
}

/// After a non-leader exec, the caller's private lists move to the leader's
/// tid (`rekey_thread`), and exec re-sends there.
#[test]
fn exec_resends_to_the_survivors_tid_after_a_non_leader_exec() {
    let mut f = family();
    let worker = QueueId::Thread(f.sibling);
    f.sched
        .discarded_at_send
        .note_timer_history(f.process, USR1);
    f.discarded_send(worker, forged_si_timer());
    f.guest_send(worker, queued());
    f.sent();
    f.sched.discarded_at_send.rekey_thread(f.sibling, f.leader);
    f.sched.take_exec_timer_flush(f.leader, f.process).unwrap();
    assert_eq!(f.sent(), vec![(f.leader, USR1, None, Some(f.process))]);
    assert!(f.entries(worker).is_empty());
    assert_eq!(f.entries(private(&f)).len(), 1);
}

/// Under record and replay, a non-leader exec moves the caller's replay
/// records to the leader's tid with its queue, and drops the records of the
/// queues the exec destroyed, the former leader's among them; the exec's
/// refusal then finds the caller's unproven copy (the Codex re-review of
/// https://github.com/rrnewton/hermit/pull/4491, P1).
#[test]
fn a_non_leader_exec_carries_its_replay_records_to_the_leaders_tid() {
    let mut f = family();
    f.sched.recordreplay_modes = true;
    let (worker, leader) = (f.sibling, f.leader);
    let usr2 = libc::SIGUSR2;
    for set in [
        &mut f.sched.replayed_timer_copies,
        &mut f.sched.replay_unproven_copies,
    ] {
        set.insert((QueueId::Thread(worker), USR1));
        set.insert((QueueId::Thread(leader), usr2));
    }
    f.sched.transfer_replay_copies(worker, leader, &[leader]);
    for set in [
        &f.sched.replayed_timer_copies,
        &f.sched.replay_unproven_copies,
    ] {
        assert_eq!(
            set.iter().copied().collect::<Vec<_>>(),
            vec![(QueueId::Thread(leader), USR1)],
            "the worker's record moved; the destroyed leader queue's went"
        );
    }
    f.queues(kernel_signal_bit(USR1), 0);
    let refusal = f
        .sched
        .replay_exec_refusal(leader, f.process)
        .expect("the moved unproven copy is refused");
    assert!(
        refusal.contains("signal 10 is pending in Thread"),
        "{refusal}"
    );
}

/// A send of a signal whose list's front the kernel already dequeued for a
/// delivery stop, before that stop's hooks ran (its turn was still to come),
/// is queued by Linux: it joins the list the early pop emptied, and the
/// stop's check expects the identified queue to hold it (cargo's jobserver
/// helper shutdown, the regression of
/// https://github.com/rrnewton/hermit/pull/4491).
#[test]
fn a_send_after_an_in_flight_dequeue_joins_the_emptied_list() {
    let mut f = family();
    let pr = private(&f);
    let bit = kernel_signal_bit(USR1);
    f.guest_send(pr, ordinary_siginfo(USR1, -6, 100, 1000));
    f.queues(bit, 0);
    assert_eq!(
        f.sched
            .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(10))),
        DeliveryAnswer::AsIs
    );
    // The kernel dequeued the front; another tgkill lands before the hooks.
    f.queues(0, 0);
    f.guest_send(pr, ordinary_siginfo(USR1, -6, 100, 1000));
    assert!(matches!(f.entries(pr).as_slice(), [Entry::Plain { .. }]));
    // The hooks read the new copy in the private queue: no refusal.
    f.queues(bit, 0);
    f.sched.discarded_signal_control(
        f.leader,
        DiscardedSignalControl::Consumed {
            signal: USR1,
            how: Consumption::KeptResume { blocked: false },
            thread_holds: true,
            process_holds: false,
        },
    );
    assert!(
        matches!(f.entries(pr).as_slice(), [Entry::Plain { .. }]),
        "the send is the list's front"
    );
    assert!(f.sent().is_empty(), "nothing is promoted or re-sent");
}

/// The same send while a timer entry waits behind the in-flight front: Linux,
/// whose pending bit stays set, drops it, but the kernel queued it, which no
/// list can hold, so the run is refused by name.
#[test]
#[should_panic(expected = "the front had an entry behind it")]
fn a_send_after_an_in_flight_dequeue_with_an_entry_behind_is_refused() {
    let mut f = family();
    let pr = private(&f);
    let bit = kernel_signal_bit(USR1);
    f.guest_send(pr, ordinary_siginfo(USR1, -6, 100, 1000));
    f.queues(bit, 0);
    f.arm(0, SignalTarget::Thread(f.leader), 10, 0);
    f.pop(10);
    assert!(matches!(
        f.entries(pr).as_slice(),
        [Entry::Plain { .. }, Entry::Timer(_)]
    ));
    f.sched
        .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(20)));
    f.queues(0, 0);
    f.guest_send(pr, ordinary_siginfo(USR1, -6, 100, 1000));
}

/// A periodic timer's entry popped early for a delivery (a send took its
/// place) whose thread then exits before the delivery finishes re-arms the
/// timer, as an entry that dies with its thread does: it is not left waiting
/// for a dequeue that will never be reported.
#[test]
fn an_early_popped_timer_entry_rearms_when_its_thread_exits() {
    let mut f = family();
    let pr = private(&f);
    let bit = kernel_signal_bit(USR1);
    f.arm(0, SignalTarget::Thread(f.leader), 10, 10);
    f.pop(10);
    f.queues(bit, 0);
    assert!(matches!(f.entries(pr).as_slice(), [Entry::Timer(_)]));
    assert_eq!(
        f.sched
            .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(15))),
        DeliveryAnswer::TimerEntry
    );
    f.queues(0, 0);
    f.guest_send(pr, ordinary_siginfo(USR1, -6, 100, 1000));
    assert!(matches!(f.entries(pr).as_slice(), [Entry::Plain { .. }]));
    assert!(
        f.sched.blocked.timed_waiters.next_deadline().is_none(),
        "the timer still waits for its entry's dequeue"
    );
    // The thread's retirement, as the scheduler runs it.
    let mm = crate::types::MmId::initial(f.process);
    let (leader, process) = (f.leader, f.process);
    f.sched.logically_kill_thread(&leader, &process, mm);
    assert!(
        f.sched.blocked.timed_waiters.next_deadline().is_some(),
        "the timer re-armed"
    );
}

/// A delivery resumed blocked requeues its copy, which `legacy_queue` drops
/// because the joined send's copy is pending: the kernel holds exactly the
/// joined send, as the list does. The early-popped periodic timer entry is
/// retired, so its timer re-arms (the Claude re-check of the in-flight-dequeue
/// fix, R4).
#[test]
fn a_blocked_resume_after_an_early_pop_retires_the_entry() {
    let mut f = family();
    let pr = private(&f);
    let bit = kernel_signal_bit(USR1);
    f.arm(0, SignalTarget::Thread(f.leader), 10, 10);
    f.pop(10);
    f.queues(bit, 0);
    f.sched
        .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(15)));
    f.queues(0, 0);
    f.guest_send(pr, ordinary_siginfo(USR1, -6, 100, 1000));
    f.queues(bit, 0);
    f.sched.discarded_signal_control(
        f.leader,
        DiscardedSignalControl::Consumed {
            signal: USR1,
            how: Consumption::KeptResume { blocked: true },
            thread_holds: true,
            process_holds: false,
        },
    );
    assert!(matches!(f.entries(pr).as_slice(), [Entry::Plain { .. }]));
    assert!(
        f.sched.blocked.timed_waiters.next_deadline().is_some(),
        "the timer re-armed"
    );
}

/// An early-popped periodic timer entry whose delivery's finish names
/// another signal (the pending dequeue does not match) is retired, and its
/// timer re-arms (the Claude re-check of the in-flight-dequeue fix, R3).
#[test]
fn an_early_popped_timer_entry_rearms_when_its_finish_names_another_signal() {
    let mut f = family();
    let pr = private(&f);
    let bit = kernel_signal_bit(USR1);
    f.arm(0, SignalTarget::Thread(f.leader), 10, 10);
    f.pop(10);
    f.queues(bit, 0);
    f.sched
        .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(15)));
    f.queues(0, 0);
    f.guest_send(pr, ordinary_siginfo(USR1, -6, 100, 1000));
    assert!(f.sched.blocked.timed_waiters.next_deadline().is_none());
    f.sched.discarded_signal_control(
        f.leader,
        DiscardedSignalControl::Consumed {
            signal: libc::SIGUSR2,
            how: Consumption::KeptResume { blocked: false },
            thread_holds: false,
            process_holds: false,
        },
    );
    assert!(
        f.sched.blocked.timed_waiters.next_deadline().is_some(),
        "the timer re-armed"
    );
}

/// With no delivery stop in flight, a send that finds the kernel's queue
/// without the signal while its list has a front does not pop that front: a
/// queue emptied behind the model's back is a consumer the schedule did not
/// see, and the 4262 checks must still find it. The send is an A1 drop and
/// the old front, with its own sender, stays (the Claude review of the
/// in-flight-dequeue fix, F2).
#[test]
fn no_early_pop_without_an_in_flight_dequeue() {
    let mut f = family();
    let pr = private(&f);
    let first = ordinary_siginfo(USR1, -6, 100, 1000);
    f.guest_send(pr, first);
    // An unseen consumer empties the kernel's queue; no stop reported it.
    f.queues(0, 0);
    f.guest_send(pr, ordinary_siginfo(USR1, -6, 200, 1000));
    assert!(
        matches!(f.entries(pr).as_slice(), [Entry::Plain { siginfo, .. }] if *siginfo == first),
        "the old front stays: {:?}",
        f.entries(pr)
    );
}

/// After an early pop on the shared queue, another thread's delivery stop
/// takes the send's copy before the first delivery's turn: every step is a
/// valid Linux history (copy 1 to the leader, copy 2 to the sibling), so
/// neither delivery refuses, whichever turn comes first (the Claude review of
/// the in-flight-dequeue fix, F3).
#[test]
fn a_second_delivery_of_the_joined_copy_is_no_unseen_consumer() {
    for sibling_first in [false, true] {
        let mut f = family();
        let sh = shared(&f);
        let bit = kernel_signal_bit(USR1);
        f.guest_send(sh, ordinary_siginfo(USR1, 0, 100, 1000));
        f.queues(0, bit);
        assert_eq!(
            f.sched
                .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(10))),
            DeliveryAnswer::AsIs
        );
        f.queues(0, 0);
        f.guest_send(sh, ordinary_siginfo(USR1, 0, 100, 1000));
        f.queues(0, bit);
        assert_eq!(
            f.sched
                .timer_signal_dequeue(f.sibling, USR1, Some(LogicalTime::from_nanos(20))),
            DeliveryAnswer::AsIs
        );
        f.queues(0, 0);
        let order = if sibling_first {
            [f.sibling, f.leader]
        } else {
            [f.leader, f.sibling]
        };
        for thread in order {
            f.sched.discarded_signal_control(
                thread,
                DiscardedSignalControl::Consumed {
                    signal: USR1,
                    how: Consumption::KeptResume { blocked: false },
                    thread_holds: false,
                    process_holds: false,
                },
            );
        }
        assert!(f.entries(sh).is_empty(), "sibling_first={sibling_first}");
    }
    // Three sends: copy 2 early-pops copy 1 into the leader's dequeue, the
    // sibling's stop takes copy 2, and copy 3 early-pops copy 2 into the
    // sibling's dequeue and stays queued. The leader's dequeue, whose own
    // front was popped early, took no copy that is still listed, so copy 3
    // is expected in the queue at both turns (the Claude re-check, R2).
    for sibling_first in [false, true] {
        let mut f = family();
        let sh = shared(&f);
        let bit = kernel_signal_bit(USR1);
        f.guest_send(sh, ordinary_siginfo(USR1, 0, 100, 1000));
        f.queues(0, bit);
        f.sched
            .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(10)));
        f.queues(0, 0);
        f.guest_send(sh, ordinary_siginfo(USR1, 0, 101, 1000));
        f.queues(0, bit);
        f.sched
            .timer_signal_dequeue(f.sibling, USR1, Some(LogicalTime::from_nanos(20)));
        f.queues(0, 0);
        f.guest_send(sh, ordinary_siginfo(USR1, 0, 102, 1000));
        assert_eq!(f.entries(sh).len(), 1, "copy 3 joined");
        f.queues(0, bit);
        let order = if sibling_first {
            [f.sibling, f.leader]
        } else {
            [f.leader, f.sibling]
        };
        for thread in order {
            f.sched.discarded_signal_control(
                thread,
                DiscardedSignalControl::Consumed {
                    signal: USR1,
                    how: Consumption::KeptResume { blocked: false },
                    thread_holds: false,
                    process_holds: true,
                },
            );
        }
        assert_eq!(
            f.entries(sh).len(),
            1,
            "copy 3 stays listed, sibling_first={sibling_first}"
        );
    }
}

/// An early-popped periodic timer entry whose delivery is replaced by a new
/// stop of the same thread (the old dequeue never finished) re-arms its
/// timer, as at the thread's exit (the Claude review of the in-flight-dequeue
/// fix, F5).
#[test]
fn an_early_popped_timer_entry_rearms_when_a_new_stop_replaces_its_dequeue() {
    let mut f = family();
    let pr = private(&f);
    let bit = kernel_signal_bit(USR1);
    f.arm(0, SignalTarget::Thread(f.leader), 10, 10);
    f.pop(10);
    f.queues(bit, 0);
    f.sched
        .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(15)));
    f.queues(0, 0);
    f.guest_send(pr, ordinary_siginfo(USR1, -6, 100, 1000));
    assert!(f.sched.blocked.timed_waiters.next_deadline().is_none());
    f.queues(bit, 0);
    f.sched
        .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(25)));
    assert!(
        f.sched.blocked.timed_waiters.next_deadline().is_some(),
        "the timer re-armed"
    );
}

/// Retiring an early-popped entry of an older timer generation does not
/// re-arm the newer one: after a `timer_settime`, the timer waits for its new
/// generation's entry, queued behind the joined send, and a new stop that
/// replaces the unfinished delivery leaves it waiting (the Codex re-check of
/// the in-flight-dequeue fix, P2-1).
#[test]
fn retiring_an_old_generations_early_entry_keeps_the_new_generation_waiting() {
    let mut f = family();
    let pr = private(&f);
    let bit = kernel_signal_bit(USR1);
    f.arm(0, SignalTarget::Thread(f.leader), 10, 10);
    f.pop(10);
    f.queues(bit, 0);
    f.sched
        .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(15)));
    f.queues(0, 0);
    f.guest_send(pr, ordinary_siginfo(USR1, -6, 100, 1000));
    // timer_settime: a new generation, first due at 40.
    f.arm(0, SignalTarget::Thread(f.leader), 40, 10);
    f.queues(bit, 0);
    f.pop(40);
    assert!(matches!(
        f.entries(pr).as_slice(),
        [Entry::Plain { .. }, Entry::Timer(_)]
    ));
    assert!(f.sched.blocked.timed_waiters.next_deadline().is_none());
    // The unfinished delivery is replaced by a new stop of the same thread.
    f.sched
        .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(45)));
    assert!(
        f.sched.blocked.timed_waiters.next_deadline().is_none(),
        "the new generation still waits for its own entry's dequeue"
    );
}

/// On a shared queue, a delivery resumed blocked after an early pop whose
/// joined copy another thread's stop already took: the kernel queues the
/// requeue (`legacy_queue` finds the signal absent), which no list can hold,
/// so the run is refused by name rather than the front retired (the Claude
/// re-check of the in-flight-dequeue fix, B1).
#[test]
#[should_panic(expected = "another thread had already taken that send's copy")]
fn a_blocked_resume_after_the_joined_copy_was_taken_is_refused() {
    let mut f = family();
    let sh = shared(&f);
    let bit = kernel_signal_bit(USR1);
    f.guest_send(sh, ordinary_siginfo(USR1, 0, 100, 1000));
    f.queues(0, bit);
    f.sched
        .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(10)));
    f.queues(0, 0);
    f.guest_send(sh, ordinary_siginfo(USR1, 0, 101, 1000));
    f.queues(0, bit);
    f.sched
        .timer_signal_dequeue(f.sibling, USR1, Some(LogicalTime::from_nanos(20)));
    f.queues(0, 0);
    let sibling = f.sibling;
    f.sched.discarded_signal_control(
        sibling,
        DiscardedSignalControl::Consumed {
            signal: USR1,
            how: Consumption::KeptResume { blocked: false },
            thread_holds: false,
            process_holds: false,
        },
    );
    // The leader's read finds the shared queue clear; it is resumed blocked.
    f.sched.discarded_signal_control(
        f.leader,
        DiscardedSignalControl::Consumed {
            signal: USR1,
            how: Consumption::KeptResume { blocked: true },
            thread_holds: false,
            process_holds: false,
        },
    );
}

/// No early pop: a delivery resumed blocked with its queue clear at the stop's
/// read requeues its copy into the empty queue, and the list still holds
/// that front, so there is no refusal and the front stays listed, on either
/// queue (the Claude re-check of the in-flight-dequeue fix at 6916a6a3, N1:
/// the early-pop condition of the blocked-resume refusal).
#[test]
fn a_blocked_resume_without_an_early_pop_keeps_its_front() {
    for shared_queue in [false, true] {
        let mut f = family();
        let q = if shared_queue {
            shared(&f)
        } else {
            private(&f)
        };
        let bit = kernel_signal_bit(USR1);
        let code = if shared_queue { 0 } else { -6 };
        f.guest_send(q, ordinary_siginfo(USR1, code, 100, 1000));
        if shared_queue {
            f.queues(0, bit)
        } else {
            f.queues(bit, 0)
        }
        f.sched
            .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(10)));
        f.queues(0, 0);
        f.sched.discarded_signal_control(
            f.leader,
            DiscardedSignalControl::Consumed {
                signal: USR1,
                how: Consumption::KeptResume { blocked: true },
                thread_holds: false,
                process_holds: false,
            },
        );
        assert!(
            matches!(f.entries(q).as_slice(), [Entry::Plain { .. }]),
            "shared_queue={shared_queue}: {:?}",
            f.entries(q)
        );
    }
}

/// After an early pop, a finish naming another signal and resumed blocked,
/// with both queues clear for that signal, retires the mismatched pending
/// dequeue (its timer re-arms) and is not refused (N1: the same-signal
/// condition of the blocked-resume refusal).
#[test]
fn a_mismatched_blocked_finish_after_an_early_pop_retires() {
    let mut f = family();
    let pr = private(&f);
    let bit = kernel_signal_bit(USR1);
    f.arm(0, SignalTarget::Thread(f.leader), 10, 10);
    f.pop(10);
    f.queues(bit, 0);
    f.sched
        .timer_signal_dequeue(f.leader, USR1, Some(LogicalTime::from_nanos(15)));
    f.queues(0, 0);
    f.guest_send(pr, ordinary_siginfo(USR1, -6, 100, 1000));
    f.sched.discarded_signal_control(
        f.leader,
        DiscardedSignalControl::Consumed {
            signal: libc::SIGUSR2,
            how: Consumption::KeptResume { blocked: true },
            thread_holds: false,
            process_holds: false,
        },
    );
    assert!(
        f.sched.blocked.timed_waiters.next_deadline().is_some(),
        "the timer re-armed"
    );
}

/// A timer's send is made at the time the timer was due, whenever the pass
/// that pops it runs: `dispatch_timed_signal` sets `signal_sent_at` for the
/// length of the dispatch, so a sleep the signal reaches is judged by when
/// the signal was due, and restores it after. A send outside a dispatch is
/// made at the commit point (https://github.com/rrnewton/hermit/pull/4682).
#[test]
fn a_timer_send_is_made_at_its_due_time() {
    let mut f = family();
    let leader = f.leader;
    f.arm(0, SignalTarget::Thread(leader), 1_000, 0);
    f.pop(5_000);
    assert_eq!(f.sent().len(), 1);
    assert_eq!(
        f.sched.test_send_times,
        vec![LogicalTime::from_nanos(1_000)]
    );
    assert_eq!(f.sched.signal_sent_at, None);
    assert_eq!(f.sched.signal_sent_at(), LogicalTime::from_nanos(5_000));
}
