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
    config.backend.supports_blocked_wait_signal_interruption = true;
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
