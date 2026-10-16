/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Which pending standard signals Linux discarded when they were sent
//! (<https://github.com/rrnewton/hermit/issues/4293>).
//!
//! Linux discards a standard signal at generation when the task the sender
//! names leaves it unblocked and ignores it (`SIG_IGN`, or the default action
//! of `SIGCHLD`, `SIGCONT`, `SIGURG` or `SIGWINCH`), or when it is an uncaught
//! `SIG_DFL` signal to a PID namespace's init (`sig_ignored`, kernel/signal.c).
//! A handler installed afterwards never sees it. Under ptrace the kernel
//! queues the copy for the tracer anyway, so `/proc` shows it pending until
//! the target next returns to user mode, and a check that decides from
//! `/proc` would count it once a handler exists.
//!
//! This record keeps, for each queue (a thread's private queue, a process's
//! shared queue), one bit per standard signal: set when the queue's pending
//! copy of that signal, if any, is one Linux discarded. Every send the
//! schedule knows of updates it in its own turn (`note_send`), and every
//! consumption the schedule sees clears it (`consume`, `take`, `flush`). A
//! check subtracts the bits from the pending sets it reads (`discarded`).
//!
//! The same record retains the first kept ITIMER_REAL origin for reported
//! handler delivery. A claim retires the exact copy before callback accounting;
//! a later expiry cannot supply the earlier delivery's metadata. The shared
//! canonical payload also serves deferred and parked in-guest delivery.
//!
//! Residual `ITIMER_REAL_UNOBSERVED_SIGINFO`: synchronous signal reads,
//! unreported delivery, record/replay and processes outside `alarm_observable`
//! retain their prior metadata behavior. Once an unreported SIGALRM can requeue
//! privately after its hook, this record stops attributing handler metadata for
//! that process. Unknown provenance is never promoted to timer ownership.
//! These restrictions affect this metadata correction, not timer recurrence.
//!
//! Real-time signals are not tracked
//! (<https://github.com/rrnewton/hermit/issues/4319>). Every bit above
//! signal 31 is ignored.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use super::Scheduler;
use super::signal_lists::KernelSiginfo;
use super::signal_lists::SignalLists;
use super::signal_lists::siginfo_code;
use crate::resources::ResourceID;
use crate::scheduler::kernel_signal_bit;
use crate::syscalls::KernelSignalState;
use crate::syscalls::read_kernel_signal_state;
use crate::types::DetPid;
use crate::types::DetTid;

/// Whether a run with `cfg` keeps this record: where the scheduler models
/// signal targets (`Scheduler::models_signal_targets`, ptrace), whose guest
/// thread IDs are host tasks. Every guest-side hook and `/proc` read of the
/// record is gated on it, so another backend (KVM) behaves as before.
pub(crate) fn records(cfg: &crate::config::Config) -> bool {
    cfg.sequentialize_threads && cfg.backend.supports_blocked_wait_signal_interruption
}

/// The standard signals this record tracks: 1 to 31.
pub(crate) const STANDARD_SIGNALS: u64 = (1 << 31) - 1;

/// A signal queue: a thread's private queue, or a process's shared one.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
pub enum QueueId {
    /// The private queue of this thread (`SigPnd`).
    Thread(DetTid),
    /// The shared queue of this process (`ShdPnd`).
    Process(DetPid),
}

/// The periodic POSIX timer whose expiry, sent while its signal was
/// explicitly ignored, set a bit, with the timer's arming generation at that
/// expiry. Linux parks such a timer and re-queues its signal when the signal
/// stops being ignored (`posixtimer_sig_unignore`), but only for an expiry
/// still current: every `timer_settime` bumps the timer's sequence
/// (`it_signal_seq`), and a stale expiry is dropped at dequeue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerProducer {
    /// The process that owns the timer.
    pub process: DetPid,
    /// The timer's kernel id in that process.
    pub timer: i32,
    /// The timer's arming generation at the expiry.
    pub arm_gen: u64,
}

/// The causal source of a scheduler publication. Only ITIMER_REAL may select
/// Linux's SI_KERNEL timer payload; POSIX timer metadata is a separate source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalProducer {
    ItimerReal,
    Posix(TimerProducer),
}

/// Linux's cleared siginfo for it_real_fn's SEND_SIG_PRIV SIGALRM.
pub(crate) fn itimer_real_siginfo() -> [u8; reverie::SIGNAL_INFO_SIZE] {
    let mut info = [0; reverie::SIGNAL_INFO_SIZE];
    info[..4].copy_from_slice(&libc::SIGALRM.to_ne_bytes());
    info[8..12].copy_from_slice(&libc::SI_KERNEL.to_ne_bytes());
    info
}

/// How the record classifies a scheduler send (`Scheduler::send_guest_signal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendFate {
    /// As Linux decides for a new send (`discard_pre_send`); the producer is
    /// the actual ITIMER_REAL expiry or the periodic POSIX timer sending it,
    /// if any.
    Classify(Option<SignalProducer>),
    /// Kept whatever the target's state: Linux queued this entry at its own
    /// expiry or park, so this send is no new Linux send (a promotion, an
    /// unignore resend; 4262 design §4.5).
    Kept,
}

/// What a sender read of a queue before its send (`DiscardedAtSend::pre_send`):
/// whether the queue held a copy of the signal, and the queue's clear
/// generation for that signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreSend {
    pub(crate) present: bool,
    pub(crate) generation: u64,
}

/// How a consumption hook saw a copy leave, or stay in, its queue
/// (`DiscardedAtSend::consume`). Only a kept resume can put the copy back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Consumption {
    /// A delivery stop or an unreported pass kept the copy and resumed the
    /// thread with it. When the thread's mask at the resume blocks the
    /// signal, the kernel requeues it into the queue it came from
    /// (`ptrace_signal`), so nothing changes.
    KeptResume { blocked: bool },
    /// The backend discarded the copy: a filtered unreported pass resumed with
    /// no signal. Linux cancels it before any requeue, whatever the mask.
    Dropped,
}

/// The first kept SIGALRM copy in one existing pending queue.
/// Unknown includes a physical first copy that no recorded send identified.
/// This is payload provenance, not another signal queue or timer policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlarmOrigin {
    ItimerReal,
    Other,
    Unknown,
}

/// The record (see the module documentation).
#[derive(Debug, Default)]
pub(crate) struct DiscardedAtSend {
    /// For each queue, the bits of its pending copies Linux discarded.
    bits: BTreeMap<QueueId, u64>,
    /// Origin of the first kept SIGALRM, in this same pending-copy record.
    /// Absence is Unknown, never proof that the physical queue is empty.
    alarm_first: BTreeMap<QueueId, AlarmOrigin>,
    /// An unreported resume can queue a private copy after the consumption
    /// hook. Its origin is not an observed send, so this process no longer
    /// attributes handler metadata. Separate from timer recurrence policy.
    alarm_unreported: BTreeSet<DetPid>,
    /// For each queue, the bits of its pending copies a recorded send kept
    /// (`note_send`). A take decides from these and `bits` alone which queue
    /// its copy came from (`take_from`), so record and replay, where a
    /// replayed signalfd read dequeues nothing, decide alike.
    kept: BTreeMap<QueueId, u64>,
    /// For each queue and signal, how many times a hook saw that signal leave
    /// that queue. A sender that found a copy pending before its send, and
    /// sees this unchanged after it, knows its discarded copy coalesced into
    /// that one (`legacy_queue`).
    clear_generation: BTreeMap<(QueueId, i32), u64>,
    /// The periodic POSIX timers whose last expiry Linux discarded, by
    /// process and kernel timer id: Linux parks such a timer and queues its
    /// signal again when the signal stops being explicitly ignored, if the
    /// expiry is still current (`posixtimer_sig_ignore`,
    /// `posixtimer_sig_unignore`).
    parked: BTreeMap<(DetPid, i32), ParkedExpiry>,
    /// The logical entries of each pending standard signal of an observable
    /// pair (<https://github.com/rrnewton/hermit/issues/4262>,
    /// `signal_lists`).
    pub(crate) lists: SignalLists,
    /// The (process, signal) pairs that have, or have had, a POSIX timer
    /// notifying that signal. A kept ordinary send standing on a discarded
    /// copy is a `Proxy` only in such a pair (4262 design rev 8, E'). The
    /// history survives exec, and a child of `fork` starts without it, as it
    /// starts without timers.
    timer_history: BTreeSet<(DetPid, i32)>,
    /// The (queue, signal) whose discarded copy, the one its bit stands for,
    /// carries `SI_TIMER` (a guest's forged `rt_sigqueueinfo`, or a timer's
    /// own discarded expiry): exec flushes such a record.
    discarded_si_timer: BTreeSet<(QueueId, i32)>,
    /// The last `ParkedExpiry::seq` given.
    park_seq: u64,
}

/// What a recorded send carries for the lists (`DiscardedAtSend::note_send_listed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendRecord {
    /// The sender's siginfo, as the receiver would see it.
    pub siginfo: KernelSiginfo,
    /// Whether the receiving pair is observable (`Scheduler::signal_observable`),
    /// so a kept send joins its list.
    pub listed: bool,
}

/// A periodic POSIX timer's discarded expiry, parked (`DiscardedAtSend::parked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParkedExpiry {
    /// The queue the expiry's copy went to.
    queue: QueueId,
    /// The timer's signal.
    signal: i32,
    /// The timer's arming generation at the expiry.
    arm_gen: u64,
    /// When it was first parked (`DiscardedAtSend::park_seq`): Linux adds a
    /// parked timer at the head of the ignored list (`hlist_add_head`), so an
    /// unignore re-queues the most recently parked first.
    seq: u64,
}

/// What an unignore of a listed pair needs sent or woken
/// (`DiscardedAtSend::unignore_lists`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnignoreAction {
    /// The re-queued entry is the front of an empty list: send its record.
    Send(QueueId, super::signal_lists::TimerEntry),
    /// The discarded copy still pending stands for the re-queued entry:
    /// nothing is sent, so its queue's waiter is woken.
    Wake(QueueId),
}

impl DiscardedAtSend {
    /// The discarded bits of `queue`, standard signals only.
    pub(crate) fn discarded(&self, queue: QueueId) -> u64 {
        self.bits.get(&queue).copied().unwrap_or(0) & STANDARD_SIGNALS
    }

    /// Whether `queue`'s pending copy of `signal` is one Linux discarded.
    pub(crate) fn is_discarded(&self, queue: QueueId, signal: i32) -> bool {
        self.discarded(queue) & kernel_signal_bit(signal) != 0
    }

    /// What a sender reads before sending `signal` into `queue`, which held a
    /// copy of it (`present`) as the sender read the kernel's queue.
    pub(crate) fn pre_send(&self, queue: QueueId, signal: i32, present: bool) -> PreSend {
        PreSend {
            present,
            generation: self.generation(queue, signal),
        }
    }

    /// Record a send of `signal` into `queue`, `discarded` or kept, with what
    /// the sender read before it (`pre`), and the periodic timer that sent it
    /// while the signal was explicitly ignored, if one did.
    ///
    /// A kept send always clears the bit. A discarded send sets it only if the
    /// queue held no copy before the send, or if a hook saw the signal leave
    /// the queue since that read: otherwise the discarded copy coalesced into
    /// the pending one, which keeps its classification. A discarded expiry of
    /// a periodic timer parks the timer with its arming generation, and a
    /// later discarded expiry of it refreshes that generation, as Linux
    /// refreshes the parked expiry's sequence (`it_sigqueue_seq`).
    pub(crate) fn note_send(
        &mut self,
        queue: QueueId,
        signal: i32,
        discarded: bool,
        pre: PreSend,
        producer: Option<TimerProducer>,
    ) {
        self.note_send_with_alarm_origin(
            queue,
            signal,
            discarded,
            pre,
            producer,
            AlarmOrigin::Other,
        );
    }

    /// The actual ITIMER_REAL send succeeded. No guest/POSIX-timer sender
    /// may call this tag: those keep using note_send (Other).
    pub(crate) fn note_itimer_real_send(&mut self, queue: QueueId, discarded: bool, pre: PreSend) {
        self.note_send_with_alarm_origin(
            queue,
            libc::SIGALRM,
            discarded,
            pre,
            None,
            AlarmOrigin::ItimerReal,
        );
    }

    /// Query before any prehook can publish another signal. Unknown never
    /// permits synthesizing timer siginfo.
    pub(crate) fn alarm_origin(&self, queue: QueueId) -> AlarmOrigin {
        self.alarm_first
            .get(&queue)
            .copied()
            .unwrap_or(AlarmOrigin::Unknown)
    }

    /// Whether an existing recorded send left a kept or discarded copy.
    /// False is NOT proof that the physical queue is empty.
    pub(crate) fn has_recorded_copy(&self, queue: QueueId, signal: i32) -> bool {
        let bit = kernel_signal_bit(signal) & STANDARD_SIGNALS;
        self.kept.get(&queue).is_some_and(|kept| kept & bit != 0)
            || self.is_discarded(queue, signal)
    }

    /// The caller has proved the exact queue of this already-dequeued copy,
    /// and that it is not an already-canonical SI_KERNEL requeue. Claim the
    /// timer BEFORE pre_handler_hook or another send. Never guess a shared
    /// source while an unknown physical private copy may win. A false claim
    /// leaves Other/Unknown/discarded copies for existing consumers to handle.
    pub(crate) fn claim_itimer_real(&mut self, source: QueueId) -> bool {
        if self.alarm_origin(source) != AlarmOrigin::ItimerReal {
            return false;
        }
        self.retire_copy(source, libc::SIGALRM);
        true
    }

    fn note_send_with_alarm_origin(
        &mut self,
        queue: QueueId,
        signal: i32,
        discarded: bool,
        pre: PreSend,
        producer: Option<TimerProducer>,
        origin: AlarmOrigin,
    ) {
        let bit = kernel_signal_bit(signal) & STANDARD_SIGNALS;
        if bit == 0 {
            return;
        }
        if !discarded {
            if signal == libc::SIGALRM {
                let same_generation = self.generation(queue, signal) == pre.generation;
                let preserves_first =
                    pre.present && same_generation && !self.is_discarded(queue, signal);
                if !preserves_first {
                    self.alarm_first.insert(queue, origin);
                }
                // If a physical copy was present but no send identified it,
                // preserves_first leaves the absent entry Unknown.
            }
            self.clear_bit(queue, signal);
            *self.kept.entry(queue).or_default() |= bit;
            return;
        }
        let emptied = !pre.present || self.generation(queue, signal) != pre.generation;
        if emptied {
            *self.bits.entry(queue).or_default() |= bit;
            if signal == libc::SIGALRM {
                self.alarm_first.remove(&queue);
            }
        }
        if let Some(producer) = producer {
            self.park(
                producer.process,
                producer.timer,
                queue,
                signal,
                producer.arm_gen,
            );
        }
    }

    /// [`Self::note_send`] for a send into a queue of `process` whose siginfo
    /// is known (`record`). A
    /// discarded copy that its bit now stands for records whether it carries
    /// `SI_TIMER`. A kept send of a listed pair joins its list (A1): only as
    /// the front of an empty list, since Linux drops an ordinary send of a
    /// pending standard signal (`legacy_queue`), and standing on the
    /// discarded copy if one is pending.
    #[expect(
        clippy::too_many_arguments,
        reason = "note_send's facts, plus the receiving process and the record"
    )]
    pub(crate) fn note_send_listed(
        &mut self,
        process: DetPid,
        queue: QueueId,
        signal: i32,
        discarded: bool,
        pre: PreSend,
        producer: Option<TimerProducer>,
        record: SendRecord,
    ) {
        if kernel_signal_bit(signal) & STANDARD_SIGNALS == 0 {
            return;
        }
        let standing = self
            .is_discarded(queue, signal)
            .then(|| self.discarded_si_timer.contains(&(queue, signal)));
        self.note_send(queue, signal, discarded, pre, producer);
        if discarded {
            if standing.is_none() && self.is_discarded(queue, signal) {
                self.note_discarded_si_timer(queue, signal, siginfo_code(&record.siginfo));
            }
            return;
        }
        if record.listed {
            let timer_pair = self.has_timer_history(process, signal);
            self.lists
                .append_plain(queue, signal, record.siginfo, standing, timer_pair);
            debug_assert!(self.lists.invariants_hold());
        }
    }

    /// Record the `si_code` of the discarded copy `queue`'s bit for `signal`
    /// stands for.
    pub(crate) fn note_discarded_si_timer(&mut self, queue: QueueId, signal: i32, code: i32) {
        if code == super::signal_lists::SI_TIMER {
            self.discarded_si_timer.insert((queue, signal));
        } else {
            self.discarded_si_timer.remove(&(queue, signal));
        }
    }

    /// Whether `queue`'s discarded copy of `signal` carries `SI_TIMER`.
    pub(crate) fn discarded_is_si_timer(&self, queue: QueueId, signal: i32) -> bool {
        self.is_discarded(queue, signal) && self.discarded_si_timer.contains(&(queue, signal))
    }

    /// `process` has a POSIX timer notifying `signal`.
    pub(crate) fn note_timer_history(&mut self, process: DetPid, signal: i32) {
        self.timer_history.insert((process, signal));
    }

    /// Whether `process` has, or has had, a POSIX timer notifying `signal`.
    pub(crate) fn has_timer_history(&self, process: DetPid, signal: i32) -> bool {
        self.timer_history.contains(&(process, signal))
    }

    /// A consumption hook saw `signal` taken from, or left in, the queues of
    /// `thread` and its process, then read `SigPnd` (`thread_holds`) and
    /// `ShdPnd` (`process_holds`). Unless the copy was resumed blocked, which
    /// requeues it, clear each queue's bit if the queue no longer holds the
    /// signal: another queue's copy, or a newer copy in the same queue, keeps
    /// its bit.
    pub(crate) fn consume(
        &mut self,
        thread: DetTid,
        process: DetPid,
        signal: i32,
        how: Consumption,
        thread_holds: bool,
        process_holds: bool,
    ) {
        if how == (Consumption::KeptResume { blocked: true }) {
            return;
        }
        if !thread_holds {
            self.retire_copy(QueueId::Thread(thread), signal);
        }
        if !process_holds {
            self.retire_copy(QueueId::Process(process), signal);
        }
    }

    /// A take (`rt_sigtimedwait`, a signalfd record) dequeued `signal` from
    /// `source`, whatever the thread's normal mask: answer whether the copy
    /// was one Linux discarded, as recorded before the take, then retire it.
    pub(crate) fn take(&mut self, source: QueueId, signal: i32) -> bool {
        let discarded = self.is_discarded(source, signal);
        self.retire_copy(source, signal);
        discarded
    }

    /// [`Self::take`] for a take by `thread` of `process`, deciding from the
    /// record alone which queue its copy came from. Linux takes the private
    /// queue's copy first (`dequeue_signal`), and a set bit means its queue
    /// holds such a copy (a stale bit over an empty queue is the named
    /// residual), so the copy came from:
    /// - the private queue, if a recorded send left a kept copy there;
    /// - else the private queue, if its copy is a discarded one;
    /// - else the shared queue, if its copy is a discarded one;
    /// - else whichever: a kept copy, maybe one no send hook saw (a SIGPIPE the
    ///   kernel raised).
    ///
    /// Not from what the queues hold after the take: a replayed signalfd read
    /// dequeues nothing, so that would decide differently in replay.
    pub(crate) fn take_from(&mut self, thread: DetTid, process: DetPid, signal: i32) -> bool {
        let (private, shared) = (QueueId::Thread(thread), QueueId::Process(process));
        let bit = kernel_signal_bit(signal);
        let source = if self.kept.get(&private).is_some_and(|kept| kept & bit != 0)
            || self.is_discarded(private, signal)
            || !self.is_discarded(shared, signal)
        {
            private
        } else {
            shared
        };
        if signal == libc::SIGALRM
            && source == private
            && !self.kept.get(&private).is_some_and(|kept| kept & bit != 0)
            && !self.is_discarded(private, signal)
        {
            // The old bool answer cannot distinguish an unrecorded private
            // copy from a kept shared one. Preserve that answer; never leave
            // positive shared provenance after a possibly shared take.
            self.alarm_first.remove(&shared);
        }
        self.take(source, signal)
    }

    /// A take for the signals `set` by `thread` of `process` failed copying
    /// out the copy it dequeued, which the kernel does not name; afterwards
    /// the private queue held `thread_pending` and the shared queue
    /// `shared_pending`. A queue whose bit for a signal of the set is set (it
    /// held a discarded copy) and which no longer holds that signal is the
    /// one the take emptied: nothing else consumed it within the taker's
    /// turn. Linux takes the lowest pending signal of the set, so a kept copy
    /// of a lower signal would have been taken instead, and the discarded one
    /// would still be pending. Answers whether such a queue exists, and
    /// retires the copy it held.
    pub(crate) fn take_faulted(
        &mut self,
        thread: DetTid,
        process: DetPid,
        set: u64,
        thread_pending: u64,
        shared_pending: u64,
    ) -> bool {
        let (private, shared) = (QueueId::Thread(thread), QueueId::Process(process));
        let alarm = kernel_signal_bit(libc::SIGALRM);
        if set & alarm != 0 {
            for (queue, pending) in [(private, thread_pending), (shared, shared_pending)] {
                if pending & alarm == 0 && self.alarm_first.contains_key(&queue) {
                    self.retire_copy(queue, libc::SIGALRM);
                }
            }
        }
        let emptied = |queue, pending: u64| self.discarded(queue) & set & !pending;
        let candidates = [
            (private, emptied(private, thread_pending)),
            (shared, emptied(shared, shared_pending)),
        ];
        let Some((queue, bits)) = candidates
            .into_iter()
            .filter(|(_, bits)| *bits != 0)
            .min_by_key(|(_, bits)| bits.trailing_zeros())
        else {
            return false;
        };
        let signal = bits.trailing_zeros() as i32 + 1;
        self.take(queue, signal)
    }

    /// Linux flushed `signal` from each of `queues`: a disposition change to
    /// one that ignores it, or a job-control send (`prepare_signal`).
    pub(crate) fn flush(&mut self, queues: impl IntoIterator<Item = QueueId>, signal: i32) {
        for queue in queues {
            self.retire_copy(queue, signal);
        }
    }

    /// `signal` stopped being explicitly ignored in `process`: each periodic
    /// timer of it parked for `signal` leaves the park, and when `current` says
    /// its expiry is still current (the timer is live, periodic, and not set
    /// again since), Linux queues it again. If its discarded copy is still
    /// pending, that copy now stands for the expiry, so its bit is cleared,
    /// and the copy stays, so no generation moves. Otherwise (a delivery stop
    /// took the copy while the signal was ignored) the expiry must be sent
    /// again: returns the queues to send it to.
    pub(crate) fn unignore(
        &mut self,
        process: DetPid,
        signal: i32,
        current: impl Fn(&TimerProducer) -> bool,
    ) -> Vec<(QueueId, TimerProducer)> {
        let mut resend: Vec<(QueueId, TimerProducer)> = Vec::new();
        for producer in self.parked_timers(process, signal) {
            let Some(parked) = self.parked.remove(&(process, producer.timer)) else {
                continue;
            };
            if !current(&producer) {
                continue;
            }
            if self.is_discarded(parked.queue, signal) {
                // The copy now stands for a kept expiry.
                self.clear_bit(parked.queue, signal);
                *self.kept.entry(parked.queue).or_default() |= kernel_signal_bit(signal);
                if signal == libc::SIGALRM {
                    // This existing parked producer is a POSIX timer, not
                    // ITIMER_REAL. Its now-kept copy must never gain that tag.
                    self.alarm_first.insert(parked.queue, AlarmOrigin::Other);
                }
            } else if !resend.iter().any(|(queue, _)| *queue == parked.queue) {
                resend.push((parked.queue, producer));
            }
        }
        resend
    }

    /// Park `process`'s timer `timer`, whose expiry went to `queue` as
    /// `signal`, with generation `arm_gen`; a timer already parked keeps its
    /// place in the ignored list and takes the new generation.
    fn park(&mut self, process: DetPid, timer: i32, queue: QueueId, signal: i32, arm_gen: u64) {
        let seq = match self.parked.get(&(process, timer)) {
            Some(parked) => parked.seq,
            None => {
                self.park_seq += 1;
                self.park_seq
            }
        };
        self.parked.insert(
            (process, timer),
            ParkedExpiry {
                queue,
                signal,
                arm_gen,
                seq,
            },
        );
    }

    /// Linux flushed `signal` from each of `queues` of a listed pair (4262
    /// design §4.8): every timer entry sent periodic whose timer is `live`,
    /// stale or not, is parked (`posixtimer_sig_ignore` tests the
    /// `it_sig_periodic` latched at the send), walking each list from its
    /// front; every other entry is freed. Then 4351's flush.
    pub(crate) fn flush_lists(
        &mut self,
        queues: &[QueueId],
        signal: i32,
        live: impl Fn(DetPid, i32) -> bool,
    ) {
        for &queue in queues {
            for entry in self.lists.take_all(queue, signal) {
                if let super::signal_lists::Entry::Timer(timer) = entry
                    && timer.periodic_at_send
                    && live(timer.process, timer.timer)
                {
                    self.park(timer.process, timer.timer, queue, signal, timer.gen_at_send);
                }
            }
        }
        self.flush(queues.iter().copied(), signal);
    }

    /// `signal` stopped being ignored in `process`, a listed pair (4262
    /// design §4.7): every parked timer of it that is `live`, current or not,
    /// is re-queued, the most recently parked first (`posixtimer_sig_unignore`
    /// walks the ignored list from its head). Its entry joins its queue's
    /// list; a discarded copy still pending there stands for the first one,
    /// as a `Proxy`, and its bit clears. Returns what must be sent or woken.
    pub(crate) fn unignore_lists(
        &mut self,
        process: DetPid,
        signal: i32,
        live: impl Fn(DetPid, i32) -> bool,
    ) -> Vec<UnignoreAction> {
        use super::signal_lists::TimerAppend;
        let mut parks: Vec<(i32, ParkedExpiry)> = self
            .parked
            .iter()
            .filter(|((owner, _), parked)| *owner == process && parked.signal == signal)
            .map(|(&(_, timer), parked)| (timer, *parked))
            .collect();
        for (timer, _) in &parks {
            self.parked.remove(&(process, *timer));
        }
        parks.retain(|(timer, _)| live(process, *timer));
        parks.sort_by_key(|(_, parked)| std::cmp::Reverse(parked.seq));
        let mut actions = Vec::new();
        for (timer, parked) in parks {
            let entry = super::signal_lists::TimerEntry {
                process,
                timer,
                gen_at_send: parked.arm_gen,
                periodic_at_send: true,
            };
            let standing = self
                .is_discarded(parked.queue, signal)
                .then(|| self.discarded_si_timer.contains(&(parked.queue, signal)));
            match self
                .lists
                .append_timer(parked.queue, signal, entry, standing)
            {
                TimerAppend::SendFront => actions.push(UnignoreAction::Send(parked.queue, entry)),
                TimerAppend::ProxyFront => {
                    self.clear_bit(parked.queue, signal);
                    *self.kept.entry(parked.queue).or_default() |= kernel_signal_bit(signal);
                    actions.push(UnignoreAction::Wake(parked.queue));
                }
                TimerAppend::Behind | TimerAppend::AlreadyPending => {}
            }
        }
        actions
    }

    /// The processes with a parked timer.
    pub(crate) fn parked_processes(&self) -> Vec<DetPid> {
        self.parked.keys().map(|(process, _)| *process).collect()
    }

    /// `timer_delete` of `process`'s timer `timer` removes its park.
    pub(crate) fn forget_park(&mut self, process: DetPid, timer: i32) {
        self.parked.remove(&(process, timer));
    }

    /// Exec deleted every timer of `process`, and their parks.
    pub(crate) fn forget_parks_of(&mut self, process: DetPid) {
        self.parked.retain(|&(owner, _), _| owner != process);
    }

    /// The periodic timers of `process` parked for `signal`, with the arming
    /// generation of their parked expiry.
    pub(crate) fn parked_timers(&self, process: DetPid, signal: i32) -> Vec<TimerProducer> {
        self.parked
            .iter()
            .filter(|((owner, _), parked)| *owner == process && parked.signal == signal)
            .map(|(&(_, timer), parked)| TimerProducer {
                process,
                timer,
                arm_gen: parked.arm_gen,
            })
            .collect()
    }

    /// `de_thread` gave the non-leader `from` the leader's id `to` at exec:
    /// its private queue moves with it, and the old leader's goes away.
    pub(crate) fn rekey_thread(&mut self, from: DetTid, to: DetTid) {
        if from == to {
            return;
        }
        let (old, new) = (QueueId::Thread(from), QueueId::Thread(to));
        self.retire(new);
        if let Some(origin) = self.alarm_first.remove(&old) {
            self.alarm_first.insert(new, origin);
        }
        if let Some(bits) = self.bits.remove(&old) {
            self.bits.insert(new, bits);
        }
        if let Some(kept) = self.kept.remove(&old) {
            self.kept.insert(new, kept);
        }
        let generations: Vec<_> = self
            .clear_generation
            .range((old, i32::MIN)..=(old, i32::MAX))
            .map(|(&(_, signal), &generation)| (signal, generation))
            .collect();
        for (signal, generation) in generations {
            self.clear_generation.remove(&(old, signal));
            self.clear_generation.insert((new, signal), generation);
        }
        for parked in self.parked.values_mut() {
            if parked.queue == old {
                parked.queue = new;
            }
        }
        let si_timer: Vec<i32> = self
            .discarded_si_timer
            .iter()
            .filter(|(queue, _)| *queue == old)
            .map(|(_, signal)| *signal)
            .collect();
        for signal in si_timer {
            self.discarded_si_timer.remove(&(old, signal));
            self.discarded_si_timer.insert((new, signal));
        }
        self.lists.rekey_thread(old, new);
    }

    /// Forget `queue`: its thread or process is gone. A process takes its
    /// timer history with it. Returns the entries its lists held.
    pub(crate) fn retire(&mut self, queue: QueueId) -> Vec<super::signal_lists::Entry> {
        let entries = self.lists.retire(queue);
        self.discarded_si_timer.retain(|&(owner, _)| owner != queue);
        if let QueueId::Process(process) = queue {
            self.timer_history.retain(|&(owner, _)| owner != process);
            self.alarm_unreported.remove(&process);
        }
        self.alarm_first.remove(&queue);
        self.bits.remove(&queue);
        self.kept.remove(&queue);
        self.clear_generation
            .retain(|&(entry, _), _| entry != queue);
        self.parked.retain(|&(owner, _), parked| {
            parked.queue != queue && QueueId::Process(owner) != queue
        });
        entries
    }

    fn generation(&self, queue: QueueId, signal: i32) -> u64 {
        self.clear_generation
            .get(&(queue, signal))
            .copied()
            .unwrap_or(0)
    }

    /// `signal` left `queue`: clear its bit and move its generation.
    fn retire_copy(&mut self, queue: QueueId, signal: i32) {
        if signal == libc::SIGALRM {
            self.alarm_first.remove(&queue);
        }
        self.clear_bit(queue, signal);
        if let Some(kept) = self.kept.get_mut(&queue) {
            *kept &= !kernel_signal_bit(signal);
        }
        *self.clear_generation.entry((queue, signal)).or_default() += 1;
    }

    fn clear_bit(&mut self, queue: QueueId, signal: i32) {
        if let Some(bits) = self.bits.get_mut(&queue) {
            *bits &= !kernel_signal_bit(signal);
        }
        self.discarded_si_timer.remove(&(queue, signal));
        self.prune(queue);
    }

    fn prune(&mut self, queue: QueueId) {
        if self.bits.get(&queue) == Some(&0) {
            self.bits.remove(&queue);
        }
    }
}

/// A message about the record from a guest thread, answered before any clock
/// or scheduler accounting (`Scheduler::discarded_signal_control`). Like the
/// child-exit ledger's, it carries no logical time and its answer carries
/// none back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscardedSignalControl {
    /// The calling thread is about to send `signal` into `queue`, naming the
    /// task `named`, in its own turn: classify the send and read the queue.
    PreSend {
        named: DetTid,
        queue: QueueId,
        signal: i32,
    },
    /// That send succeeded: record it, with the siginfo the receiver sees
    /// (`None`: one Detcore does not know, recorded as `SI_USER` from the
    /// caller).
    Sent(DiscardToken, Option<KernelSiginfo>),
    /// A stop of the calling thread consumed, or kept, a copy of `signal`,
    /// after which its private queue (`thread_holds`) and its process's shared
    /// queue (`process_holds`) were read.
    Consumed {
        signal: i32,
        how: Consumption,
        thread_holds: bool,
        process_holds: bool,
    },
    /// The calling thread took a copy of `signal` (`rt_sigtimedwait`, a
    /// signalfd record) at its logical time `at`, after which its private and
    /// its process's shared queues held `after` (when read). Answers whether
    /// to drop the copy (one Linux discarded, `DiscardedAtSend::take_from`, or
    /// a stale timer entry) and the siginfo to write in its place
    /// (`Scheduler::timer_signal_take`).
    Take {
        signal: i32,
        at: crate::types::LogicalTime,
        after: Option<(u64, u64)>,
    },
    /// The calling thread's `rt_sigtimedwait` for the signals `set` failed
    /// with EFAULT copying out a signal it had dequeued, after which its
    /// private queue held `thread_pending` and its process's shared queue
    /// `shared_pending`. Answers whether to take again: the copy it took was
    /// one Linux discarded (`DiscardedAtSend::take_faulted`), or a stale timer
    /// entry Linux would have skipped (`Scheduler::timer_signal_take_faulted`).
    TakeFaulted {
        set: u64,
        thread_pending: u64,
        shared_pending: u64,
        at: crate::types::LogicalTime,
    },
    /// The calling thread's process changed `signal`'s action to one that
    /// ignores it, and Linux flushed it from every queue of the process.
    Flushed { signal: i32 },
    /// The calling thread's process changed `signal`'s action from `SIG_IGN`
    /// to one that does not ignore it (`posixtimer_sig_unignore`).
    Unignored { signal: i32 },
    /// The discarded bits of the calling thread's private queue and of its
    /// process's shared queue.
    Bits,
    /// Before any logical-time accounting, claim the reported SIGALRM's
    /// positively identified ITIMER_REAL copy. Already canonical SI_KERNEL
    /// requeues must not claim a newer pending copy.
    ClaimItimerReal,
    /// This process used an unreported SIGALRM resume, whose later private
    /// requeue is outside the pending-origin observation contract.
    UnreportedAlarm,
    /// A reported delivery stop of the calling thread dequeued `signal` at
    /// the thread's logical time `at`: identify the timer entry it took, if
    /// any, and answer what to deliver (`Scheduler::timer_signal_dequeue`,
    /// <https://github.com/rrnewton/hermit/issues/4262>).
    Dequeue {
        signal: i32,
        at: crate::types::LogicalTime,
    },
    /// The delivery stop of `signal` that answered `DeliveryAnswer::TimerEntry`
    /// has its turn, at the calling thread's time `at`: answer the entry's
    /// siginfo (`Scheduler::timer_delivery_info`).
    DeliveryInfo {
        signal: i32,
        at: crate::types::LogicalTime,
    },
    /// The backend passes `signal`, which the calling thread dequeued, on
    /// unreported (`Tool::filter_unreported_signal`): as `Dequeue`, for a hook
    /// that cannot replace the siginfo.
    Unreported { signal: i32 },
    /// The calling thread's process installed an action for `signal`: a
    /// handler (`caught`) whose `sa_mask` is `mask`, with or without
    /// `SA_NODEFER`. The mask a delivery leaves the receiving thread with
    /// decides where a promotion goes (`Scheduler::handler_masks`).
    Handler {
        signal: i32,
        caught: bool,
        mask: u64,
        nodefer: bool,
    },
    /// The calling thread's process created a POSIX timer notifying `signal`:
    /// the pair has a timer history from now on, armed or not (N10-2; the
    /// Codex review of <https://github.com/rrnewton/hermit/pull/4491>, P2-4).
    TimerCreated { signal: i32 },
    /// The signals whose list in the calling thread's private queue, and in
    /// its process's shared queue, holds an entry behind its front or a stale
    /// timer entry (`signal_lists`), as `Bits`: a dequeue of one sends the
    /// next, or drops a record Linux would have skipped.
    Queued,
    /// `timer_gettime` or `timer_getoverrun` of the calling thread's
    /// process's timer `timer` at the caller's time `now`
    /// (`Scheduler::posix_timer_remaining`, `Scheduler::posix_timer_overrun`).
    TimerReadout {
        timer: i32,
        now: crate::types::LogicalTime,
    },
}

/// The answer to a [`DiscardedSignalControl`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscardedSignalAnswer {
    Recorded,
    Token(DiscardToken),
    Discarded(bool),
    Bits {
        thread: u64,
        process: u64,
    },
    ItimerReal(bool),
    Delivery(super::posix_timer_coalesce::DeliveryAnswer),
    /// The remaining time (`None`: the caller's own answer stands) and the
    /// overrun of a POSIX timer.
    TimerReadout {
        remaining: Option<crate::types::LogicalTime>,
        overrun: i32,
    },
    /// What a take does with the copy it took (`DiscardedSignalControl::Take`).
    Taken(super::posix_timer_coalesce::TakeOutcome),
}

/// A sender's classification of one send, with what it read before the
/// physical send (`Scheduler::discard_pre_send`), to be recorded once the send
/// succeeds (`Scheduler::discard_note_send`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscardToken {
    /// The queue the copy joins.
    pub queue: QueueId,
    /// The signal.
    pub signal: i32,
    /// Whether Linux discards it.
    pub discarded: bool,
    /// Whether the queue held a copy before the send.
    pub present: bool,
    /// The queue's clear generation for the signal before the send.
    pub generation: u64,
}

impl Scheduler {
    /// Whether Linux discards `signal` sent now to the task `named`, which the
    /// sender names (`kill`'s pid, `tgkill`'s tid), from `sender` (`None`: the
    /// scheduler, which sends from an ancestor PID namespace). Read in the
    /// sending turn. Discarded means unblocked in `named`'s mask
    /// (`discard_mask`) and either ignored (`SIG_IGN`, or default-ignored with
    /// no handler) or an uncaught `SIG_DFL` signal to a PID namespace's init,
    /// except SIGKILL or SIGSTOP from an ancestor namespace (`sig_task_ignored`
    /// with `force`). A state that cannot be read, or a task whose mask
    /// Detcore does not know (an exited leader), is not discarded, as before
    /// <https://github.com/rrnewton/hermit/issues/4293>.
    pub(crate) fn discarded_when_sent_to(
        &self,
        named: DetTid,
        signal: i32,
        sender: Option<DetTid>,
    ) -> bool {
        let bit = kernel_signal_bit(signal) & STANDARD_SIGNALS;
        if !self.models_signal_targets || bit == 0 {
            return false;
        }
        let Some(mask) = self.discard_mask(named) else {
            return false;
        };
        if mask & bit != 0 {
            return false;
        }
        let Some(state) = self.named_signal_state(named) else {
            return false;
        };
        if state.ignored_at_generation() & bit != 0 {
            return true;
        }
        let process = self.thread_tree.process_of(named).unwrap_or(named);
        if !self.thread_tree.is_pid_namespace_init(process) || state.caught & bit != 0 {
            return false;
        }
        if signal == libc::SIGKILL || signal == libc::SIGSTOP {
            // Forced only from an ancestor namespace.
            return sender.is_some_and(|sender| {
                self.thread_tree.pid_namespaces.namespace_of(sender) == Some(process)
            });
        }
        true
    }

    /// The mask that decides whether `named` discards a signal sent now
    /// (design: dev-hermit ignored/kernel-ctl/design-4293.md, section 2). The
    /// first source present wins:
    /// 1. the guest's own mask while Detcore holds the thread under a private
    ///    blocking mask (`saved_guest_sigmasks`), Linux's `real_blocked` for
    ///    an `rt_sigtimedwait`;
    /// 2. a parked futex waiter's watch;
    /// 3. the mask recorded for a thread asleep outside the schedule
    ///    (`out_of_scheduler_masks`), where an unknown one is `None`;
    /// 4. a filed `BlockingRtSigsuspend` request's temporary mask;
    /// 5. `SigBlk`, for a live thread stopped at a request.
    pub(crate) fn discard_mask(&self, named: DetTid) -> Option<u64> {
        if let Some(saved) = self.saved_guest_sigmasks.get(&named) {
            return Some(*saved);
        }
        if let Some(watch) = self
            .parked_futex_waiter(named)
            .and_then(|waiter| waiter.signal_watch)
        {
            return Some(!watch.unblocked);
        }
        if let Some(recorded) = self.blocked.out_of_scheduler_masks.get(&named) {
            return *recorded;
        }
        if let Some(mask) = self.filed_sigsuspend_mask(named) {
            return Some(mask);
        }
        if !self.next_turns.contains_key(&named) {
            return None;
        }
        self.read_thread_blocked_mask(named)
    }

    /// The temporary mask of `dettid`'s filed `BlockingRtSigsuspend` request.
    fn filed_sigsuspend_mask(&self, dettid: DetTid) -> Option<u64> {
        let Some(Some(Ok(request))) = self.next_turns.get(&dettid).map(|nt| nt.req.try_read())
        else {
            return None;
        };
        request
            .resources
            .keys()
            .any(|rid| matches!(rid, ResourceID::BlockingRtSigsuspend(_)))
            .then_some(request.blocked_signal_mask)
            .flatten()
    }

    /// `named`'s signal state (dispositions, queues), read from the kernel, or
    /// `None` for a task the scheduler does not know or cannot read.
    fn named_signal_state(&self, named: DetTid) -> Option<KernelSignalState> {
        if !self.next_turns.contains_key(&named) {
            return None;
        }
        #[cfg(test)]
        if let Some(state) = self.test_kernel_signal_states.get(&named) {
            return Some(*state);
        }
        let process = self.thread_tree.process_of(named).unwrap_or(named);
        read_kernel_signal_state(
            reverie::Pid::from_raw(process.as_raw()),
            reverie::Pid::from_raw(named.as_raw()),
        )
        .ok()
    }

    /// Whether `queue` holds a copy of `signal`, as the kernel shows it now
    /// (`SigPnd` of the thread, `ShdPnd` of the process read through a live
    /// thread of it). An unreadable queue counts as holding one, so a
    /// discarded send never marks a copy it could not see.
    fn queue_holds(&self, queue: QueueId, signal: i32) -> bool {
        let bit = kernel_signal_bit(signal);
        let state = match queue {
            QueueId::Thread(thread) => self.named_signal_state(thread).map(|s| s.thread_pending),
            QueueId::Process(process) => self
                .process_signal_targets(process)
                .into_iter()
                .find_map(|thread| self.named_signal_state(thread))
                .map(|s| s.shared_pending),
        };
        state.is_none_or(|pending| pending & bit != 0)
    }

    /// Classify a send of `signal` into `queue`, named `named` by `sender`, and
    /// read the queue, before the physical send (see `DiscardToken`).
    pub(crate) fn discard_pre_send(
        &self,
        named: DetTid,
        queue: QueueId,
        signal: i32,
        sender: Option<DetTid>,
    ) -> DiscardToken {
        let discarded = self.discarded_when_sent_to(named, signal, sender);
        let pre = self
            .discarded_at_send
            .pre_send(queue, signal, self.queue_holds(queue, signal));
        DiscardToken {
            queue,
            signal,
            discarded,
            present: pre.present,
            generation: pre.generation,
        }
    }

    /// The token of a send into `queue` that Linux keeps whatever the target's
    /// state (`SendFate::Kept`): no classification, and the queue read as
    /// for any send.
    pub(crate) fn discard_kept_token(&self, queue: QueueId, signal: i32) -> DiscardToken {
        let pre = self
            .discarded_at_send
            .pre_send(queue, signal, self.queue_holds(queue, signal));
        DiscardToken {
            queue,
            signal,
            discarded: false,
            present: pre.present,
            generation: pre.generation,
        }
    }

    /// Record a send `token` classified, now that the physical send
    /// succeeded, with `record` the siginfo the receiver sees, if the send is
    /// an ordinary one that can join a list (A1, 4262 design §4.2); `None`
    /// for a timer entry's own record.
    pub(crate) fn discard_note_send(
        &mut self,
        token: DiscardToken,
        producer: Option<TimerProducer>,
        record: Option<KernelSiginfo>,
    ) {
        let pre = PreSend {
            present: token.present,
            generation: token.generation,
        };
        match record {
            Some(siginfo) => {
                if !token.discarded {
                    self.pop_in_flight_front(token.queue, token.signal, token.present);
                }
                let process = match token.queue {
                    QueueId::Thread(thread) => {
                        self.thread_tree.process_of(thread).unwrap_or(thread)
                    }
                    QueueId::Process(process) => process,
                };
                let listed = !token.discarded && self.signal_observable(process, token.signal);
                self.discarded_at_send.note_send_listed(
                    process,
                    token.queue,
                    token.signal,
                    token.discarded,
                    pre,
                    producer,
                    SendRecord { siginfo, listed },
                );
            }
            None => self.discarded_at_send.note_send(
                token.queue,
                token.signal,
                token.discarded,
                pre,
                producer,
            ),
        }
        // A send of a stop signal flushes SIGCONT, and a send of SIGCONT the
        // stop signals, from every queue of the process, before Linux decides
        // whether to discard the send itself (`prepare_signal`).
        self.discard_job_control_flush(token.queue, token.signal);
    }

    /// Record a send `token` that succeeded, by its `fate`: an actual
    /// ITIMER_REAL expiry keeps its origin (`discard_note_itimer_real`), and
    /// any other send is recorded with its POSIX timer producer and its
    /// `record` (`discard_note_send`). The ITIMER_REAL pair is never listed:
    /// an ITIMER_REAL arm ended its observability.
    pub(crate) fn discard_note_fate(
        &mut self,
        token: DiscardToken,
        fate: SendFate,
        record: Option<KernelSiginfo>,
    ) {
        match fate {
            SendFate::Classify(Some(SignalProducer::ItimerReal)) => {
                self.discard_note_itimer_real(token)
            }
            SendFate::Classify(Some(SignalProducer::Posix(timer))) => {
                self.discard_note_send(token, Some(timer), record)
            }
            SendFate::Classify(None) | SendFate::Kept => {
                self.discard_note_send(token, None, record)
            }
        }
    }

    /// Record the successful physical publication of an actual ITIMER_REAL
    /// expiry in the existing pending-copy owner.
    pub(crate) fn discard_note_itimer_real(&mut self, token: DiscardToken) {
        debug_assert_eq!(token.signal, libc::SIGALRM);
        self.discarded_at_send.note_itimer_real_send(
            token.queue,
            token.discarded,
            PreSend {
                present: token.present,
                generation: token.generation,
            },
        );
    }

    /// A reported delivery in the observable timer domain. All consumers and
    /// sends are ordered there, and an unreported requeue permanently removes
    /// this proof. Linux selects the private queue before the shared queue.
    fn claim_reported_itimer_real(&mut self, thread: DetTid, process: DetPid) -> bool {
        if !self.alarm_observable_now(process)
            || self.discarded_at_send.alarm_unreported.contains(&process)
        {
            return false;
        }
        let private = QueueId::Thread(thread);
        let shared = QueueId::Process(process);
        let source = if self
            .discarded_at_send
            .has_recorded_copy(private, libc::SIGALRM)
        {
            private
        } else if self
            .discarded_at_send
            .has_recorded_copy(shared, libc::SIGALRM)
        {
            shared
        } else {
            return false;
        };
        self.discarded_at_send.claim_itimer_real(source)
    }

    /// The job-control flush of `prepare_signal` for a send of `signal` into
    /// `queue`'s process.
    fn discard_job_control_flush(&mut self, queue: QueueId, signal: i32) {
        const STOPS: [i32; 4] = [libc::SIGSTOP, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU];
        let flushed: &[i32] = if STOPS.contains(&signal) {
            &[libc::SIGCONT]
        } else if signal == libc::SIGCONT {
            &STOPS
        } else {
            return;
        };
        let process = match queue {
            QueueId::Thread(thread) => self.thread_tree.process_of(thread).unwrap_or(thread),
            QueueId::Process(process) => process,
        };
        let mut queues = self.process_queues(process);
        if !queues.contains(&queue) {
            queues.push(queue);
        }
        for &signal in flushed {
            self.discarded_at_send.flush(queues.iter().copied(), signal);
        }
    }

    /// Every queue of `process`: its shared queue and each live thread's (a
    /// thread that is gone took its queue with it, `retire`).
    pub(crate) fn process_queues(&self, process: DetPid) -> Vec<QueueId> {
        let mut queues = vec![QueueId::Process(process)];
        queues.extend(
            self.process_signal_targets(process)
                .into_iter()
                .map(QueueId::Thread),
        );
        queues
    }

    /// Answer `control` from the guest thread `dtid`.
    pub(crate) fn discarded_signal_control(
        &mut self,
        dtid: DetTid,
        control: DiscardedSignalControl,
    ) -> DiscardedSignalAnswer {
        self.signal_control_caller = Some(dtid);
        let answer = self.answer_discarded_signal_control(dtid, control);
        self.signal_control_caller = None;
        answer
    }

    fn answer_discarded_signal_control(
        &mut self,
        dtid: DetTid,
        control: DiscardedSignalControl,
    ) -> DiscardedSignalAnswer {
        let process = self.thread_tree.process_of(dtid).unwrap_or(dtid);
        match control {
            DiscardedSignalControl::ClaimItimerReal => {
                DiscardedSignalAnswer::ItimerReal(self.claim_reported_itimer_real(dtid, process))
            }
            DiscardedSignalControl::UnreportedAlarm => {
                self.discarded_at_send.alarm_unreported.insert(process);
                DiscardedSignalAnswer::Recorded
            }
            DiscardedSignalControl::PreSend {
                named,
                queue,
                signal,
            } => {
                // A sender names a process by any of its threads' ids.
                let queue = match queue {
                    QueueId::Process(task) => {
                        QueueId::Process(self.thread_tree.process_of(task).unwrap_or(task))
                    }
                    thread => thread,
                };
                DiscardedSignalAnswer::Token(self.discard_pre_send(
                    named,
                    queue,
                    signal,
                    Some(dtid),
                ))
            }
            DiscardedSignalControl::Sent(token, siginfo) => {
                let siginfo = siginfo.unwrap_or_else(|| {
                    super::signal_lists::ordinary_siginfo(token.signal, 0, process.as_raw(), 0)
                });
                self.discard_note_send(token, None, Some(siginfo));
                DiscardedSignalAnswer::Recorded
            }
            DiscardedSignalControl::Consumed {
                signal,
                how,
                thread_holds,
                process_holds,
            } => {
                // The read must agree with the timer entries before 4351's
                // record retires anything; the pop's promotion comes after.
                let pending = self.finish_timer_signal_dequeue(
                    dtid,
                    signal,
                    how,
                    thread_holds,
                    process_holds,
                );
                if how != (Consumption::KeptResume { blocked: true }) {
                    self.note_replayed_copy_taken(dtid, signal, thread_holds, process_holds);
                }
                self.discarded_at_send.consume(
                    dtid,
                    process,
                    signal,
                    how,
                    thread_holds,
                    process_holds,
                );
                if let Some(pending) = pending {
                    self.pop_and_promote(pending, dtid);
                }
                DiscardedSignalAnswer::Recorded
            }
            DiscardedSignalControl::Take { signal, at, after } => {
                DiscardedSignalAnswer::Taken(self.timer_signal_take(dtid, signal, at, after))
            }
            DiscardedSignalControl::TakeFaulted {
                set,
                thread_pending,
                shared_pending,
                at,
            } => DiscardedSignalAnswer::Discarded(self.timer_signal_take_faulted(
                dtid,
                set,
                thread_pending,
                shared_pending,
                at,
            )),
            DiscardedSignalControl::Flushed { signal } => {
                let queues = self.process_queues(process);
                if self.signal_observable(process, signal) {
                    let live: std::collections::BTreeSet<i32> = self
                        .posix_timer_values
                        .keys()
                        .filter(|(owner, _)| *owner == process)
                        .map(|(_, timer)| *timer)
                        .collect();
                    self.discarded_at_send
                        .flush_lists(&queues, signal, |_, timer| live.contains(&timer));
                } else {
                    self.discarded_at_send.flush(queues, signal);
                }
                DiscardedSignalAnswer::Recorded
            }
            DiscardedSignalControl::Unignored { signal }
                if self.signal_observable(process, signal) =>
            {
                let live: std::collections::BTreeSet<i32> = self
                    .posix_timer_values
                    .keys()
                    .filter(|(owner, _)| *owner == process)
                    .map(|(_, timer)| *timer)
                    .collect();
                let actions = self
                    .discarded_at_send
                    .unignore_lists(process, signal, |_, timer| live.contains(&timer));
                for action in actions {
                    match action {
                        UnignoreAction::Send(queue, entry) => {
                            let info = self.timer_signal_info(signal, &entry, 0);
                            let producer = TimerProducer {
                                process,
                                timer: entry.timer,
                                arm_gen: entry.gen_at_send,
                            };
                            self.send_unignored_expiry(
                                dtid,
                                process,
                                queue,
                                signal,
                                producer,
                                Some(info),
                            );
                        }
                        UnignoreAction::Wake(queue) => {
                            if let Ok(sig) = nix::sys::signal::Signal::try_from(signal) {
                                self.wake_for_entry(queue, sig, dtid);
                            }
                        }
                    }
                }
                DiscardedSignalAnswer::Recorded
            }
            DiscardedSignalControl::Unignored { signal } => {
                let current: Vec<_> = self
                    .discarded_at_send
                    .parked_timers(process, signal)
                    .into_iter()
                    .filter(|producer| self.posix_timer_expiry_current(producer))
                    .collect();
                let resend = self
                    .discarded_at_send
                    .unignore(process, signal, |producer| current.contains(producer));
                for (queue, producer) in resend {
                    // The timer's own `SI_TIMER` record, which exec's flush
                    // removes, as every physical timer record carries
                    // (https://github.com/rrnewton/hermit/issues/4262).
                    let entry = super::signal_lists::TimerEntry {
                        process,
                        timer: producer.timer,
                        gen_at_send: producer.arm_gen,
                        periodic_at_send: true,
                    };
                    let info = self
                        .models_signal_targets
                        .then(|| self.timer_signal_info(signal, &entry, 0));
                    self.send_unignored_expiry(dtid, process, queue, signal, producer, info);
                }
                DiscardedSignalAnswer::Recorded
            }
            DiscardedSignalControl::Bits => {
                let (thread, process) = self.discarded_bits(dtid);
                DiscardedSignalAnswer::Bits { thread, process }
            }
            DiscardedSignalControl::Dequeue { signal, at } => {
                DiscardedSignalAnswer::Delivery(self.timer_signal_dequeue(dtid, signal, Some(at)))
            }
            DiscardedSignalControl::DeliveryInfo { signal, at } => {
                match self.timer_delivery_info(dtid, signal, at) {
                    Some(info) => DiscardedSignalAnswer::Delivery(
                        super::posix_timer_coalesce::DeliveryAnswer::Rewrite(info),
                    ),
                    None => DiscardedSignalAnswer::Delivery(
                        super::posix_timer_coalesce::DeliveryAnswer::AsIs,
                    ),
                }
            }
            DiscardedSignalControl::Unreported { signal } => {
                DiscardedSignalAnswer::Delivery(self.timer_signal_dequeue(dtid, signal, None))
            }
            DiscardedSignalControl::Handler {
                signal,
                caught,
                mask,
                nodefer,
            } => {
                if caught {
                    self.handler_masks
                        .insert((process, signal), (mask, nodefer));
                } else {
                    self.handler_masks.remove(&(process, signal));
                }
                DiscardedSignalAnswer::Recorded
            }
            DiscardedSignalControl::TimerCreated { signal } => {
                if (1..32).contains(&signal) {
                    self.discarded_at_send.note_timer_history(process, signal);
                }
                DiscardedSignalAnswer::Recorded
            }
            DiscardedSignalControl::Queued => {
                let listed = |queue: QueueId| {
                    (1..32)
                        .filter(|&signal| {
                            let entries = self.discarded_at_send.lists.entries(queue, signal);
                            entries.len() > 1
                                || entries.iter().any(|entry| {
                                    matches!(entry, super::signal_lists::Entry::Timer(timer)
                                        if !self.timer_entry_valid(timer))
                                })
                        })
                        .fold(0, |bits, signal| bits | kernel_signal_bit(signal))
                };
                DiscardedSignalAnswer::Bits {
                    thread: listed(QueueId::Thread(dtid)),
                    process: listed(QueueId::Process(process)),
                }
            }
            DiscardedSignalControl::TimerReadout { timer, now } => {
                DiscardedSignalAnswer::TimerReadout {
                    remaining: self.posix_timer_remaining(process, timer, now),
                    overrun: self.posix_timer_overrun(process, timer),
                }
            }
        }
    }

    /// Send again, into `queue` of `process`, a parked expiry of the timer
    /// `producer` that Linux queues when `caller`'s `rt_sigaction` stops
    /// ignoring `signal` (`DiscardedAtSend::unignore`), in the caller's turn.
    /// Linux re-queues the parked signal without the discard check
    /// (`posixtimer_sig_unignore`), so the copy is kept, whatever the target's
    /// state. It is recorded as a timer's copy, as `fire_process_timer` and
    /// `fire_thread_timer` record theirs (the exec flush,
    /// https://github.com/rrnewton/hermit/issues/4184), and a process-directed
    /// one as a scheduler signal left in the shared queue (`note_group_send`),
    /// and, when every thread blocks it, in `shared_queue_scheduler_signals`,
    /// as `fire_alarm_with` records a process timer's
    /// (https://github.com/rrnewton/hermit/issues/4234). A thread other than
    /// the caller is woken for it; the caller, running, needs no wake.
    /// `info`, a timer entry's `SI_TIMER` record, is sent with it where given
    /// (`rt_sigqueueinfo`, `rt_tgsigqueueinfo`).
    fn send_unignored_expiry(
        &mut self,
        caller: DetTid,
        process: DetPid,
        queue: QueueId,
        signal: i32,
        producer: TimerProducer,
        info: Option<[u8; 128]>,
    ) {
        let Ok(sig) = nix::sys::signal::Signal::try_from(signal) else {
            return;
        };
        let target = match queue {
            QueueId::Thread(thread) => Some(thread),
            QueueId::Process(_) => self
                .select_signal_target(process, Some(caller))
                .map(|preferred| self.timer_signal_target(process, preferred, sig)),
        };
        let Some(target) = target else {
            return;
        };
        let token = self.discard_kept_token(queue, signal);
        // Decided before the send, as `fire_alarm_with` decides it.
        let every_thread_blocks = matches!(queue, QueueId::Process(_))
            && self.models_signal_targets
            && self.kernel_signal_target(process, target, signal).is_none();
        // SAFETY: tgkill and kill take only integers; the siginfo forms take
        // a pointer to `info`, which outlives the call.
        let rc = match (queue, info.as_ref()) {
            (QueueId::Thread(thread), None) => unsafe {
                libc::syscall(libc::SYS_tgkill, process.as_raw(), thread.as_raw(), signal)
            },
            (QueueId::Thread(thread), Some(info)) => unsafe {
                libc::syscall(
                    libc::SYS_rt_tgsigqueueinfo,
                    process.as_raw(),
                    thread.as_raw(),
                    signal,
                    info.as_ptr(),
                )
            },
            (QueueId::Process(_), None) => unsafe {
                libc::kill(target.as_raw(), signal) as libc::c_long
            },
            (QueueId::Process(_), Some(info)) => unsafe {
                libc::syscall(
                    libc::SYS_rt_sigqueueinfo,
                    target.as_raw(),
                    signal,
                    info.as_ptr(),
                )
            },
        };
        if rc < 0 {
            tracing::info!(
                "[dtid {}] the parked expiry of signal {} was not queued again: the thread exited",
                target,
                sig
            );
            return;
        }
        tracing::info!(
            "[dtid {}] signal {} stopped being ignored: a parked timer expiry is queued again",
            target,
            sig
        );
        self.discard_note_send(token, None, None);
        let bit = kernel_signal_bit(signal);
        match queue {
            QueueId::Thread(thread) => {
                let id = super::timed_waiters::SignalTimerId::Posix(process, producer.timer);
                if self.thread_timer_copy_is_observable(thread, bit)
                    && !super::is_job_control_signal(signal)
                {
                    self.thread_timer_copies.insert((thread, signal), id);
                } else {
                    self.thread_timer_copies.remove(&(thread, signal));
                }
            }
            QueueId::Process(_) => {
                if every_thread_blocks {
                    self.note_signal_left_in_shared_queue(process, signal);
                }
                if !self.note_group_send(target, sig, false) {
                    return;
                }
            }
        }
        super::TIMER_COPIES_RECORDED.store(true, std::sync::atomic::Ordering::SeqCst);
        if target != caller {
            self.wake_signaled_guest_as(target, sig, false);
        }
    }

    /// The periodic POSIX timer `id`, with its arming generation, as the
    /// producer of an expiry it sends now; `None` for any other timer.
    pub(crate) fn posix_timer_producer(
        &self,
        id: super::timed_waiters::SignalTimerId,
    ) -> Option<TimerProducer> {
        let super::timed_waiters::SignalTimerId::Posix(process, timer) = id else {
            return None;
        };
        self.blocked.timed_waiters.signal_timer_period(id)?;
        Some(TimerProducer {
            process,
            timer,
            arm_gen: self.posix_timer_arm_generation(process, timer),
        })
    }

    /// Whether `producer`'s expiry is still current: its timer is armed,
    /// periodic, and not set again since (`posixtimer_sig_unignore` re-queues
    /// only such an expiry).
    pub(crate) fn posix_timer_expiry_current(&self, producer: &TimerProducer) -> bool {
        let id = super::timed_waiters::SignalTimerId::Posix(producer.process, producer.timer);
        self.blocked.timed_waiters.signal_timer_period(id).is_some()
            && self.posix_timer_arm_generation(producer.process, producer.timer) == producer.arm_gen
    }

    /// The bits of `thread`'s private queue and of its process's shared
    /// queue whose pending copy no dequeue would deliver: one Linux discarded
    /// (`DiscardedAtSend::discarded`), or one standing only for POSIX timer
    /// entries that are all stale, which Linux skips in a single dequeue
    /// (`SignalLists::deliverable`, <https://github.com/rrnewton/hermit/issues/4262>).
    pub(crate) fn discarded_bits(&self, thread: DetTid) -> (u64, u64) {
        let process = self.thread_tree.process_of(thread).unwrap_or(thread);
        let undeliverable = |queue: QueueId| {
            (1..32)
                .filter(|&signal| {
                    self.discarded_at_send.lists.is_listed(queue, signal)
                        && !self.list_deliverable(queue, signal)
                })
                .fold(0, |bits, signal| bits | kernel_signal_bit(signal))
        };
        let (private, shared) = (QueueId::Thread(thread), QueueId::Process(process));
        (
            self.discarded_at_send.discarded(private) | undeliverable(private),
            self.discarded_at_send.discarded(shared) | undeliverable(shared),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: DetTid = DetTid::from_raw(10);
    const U: DetTid = DetTid::from_raw(11);
    const P: DetPid = DetPid::from_raw(10);
    const USR1: i32 = libc::SIGUSR1;
    const USR2: i32 = libc::SIGUSR2;

    fn thread() -> QueueId {
        QueueId::Thread(T)
    }

    fn send(record: &mut DiscardedAtSend, queue: QueueId, discarded: bool, present: bool) {
        let pre = record.pre_send(queue, USR1, present);
        record.note_send(queue, USR1, discarded, pre, None);
    }

    #[test]
    fn a_discarded_send_to_an_empty_queue_sets_and_a_kept_send_clears() {
        let mut record = DiscardedAtSend::default();
        send(&mut record, thread(), true, false);
        assert!(record.is_discarded(thread(), USR1));
        assert!(!record.is_discarded(QueueId::Process(P), USR1));
        send(&mut record, thread(), false, true);
        assert!(!record.is_discarded(thread(), USR1));
    }

    #[test]
    fn a_discarded_send_coalescing_into_a_kept_copy_leaves_it_kept() {
        let mut record = DiscardedAtSend::default();
        send(&mut record, thread(), true, true);
        assert!(!record.is_discarded(thread(), USR1));
    }

    #[test]
    fn a_same_signal_clear_between_read_and_update_sets_the_bit() {
        // Codex rev6 P2-2 (a): the retained copy left, so the discarded copy
        // is the one now pending.
        let mut record = DiscardedAtSend::default();
        let pre = record.pre_send(thread(), USR1, true);
        record.consume(T, P, USR1, Consumption::Dropped, false, false);
        record.note_send(thread(), USR1, true, pre, None);
        assert!(record.is_discarded(thread(), USR1));
    }

    #[test]
    fn an_unrelated_signal_clear_between_read_and_update_leaves_the_bit() {
        // Codex rev6 P2-2 (b): the retained USR1 is still pending, and the
        // discarded USR1 coalesced into it.
        let mut record = DiscardedAtSend::default();
        let pre = record.pre_send(thread(), USR1, true);
        record.consume(T, P, USR2, Consumption::Dropped, false, false);
        record.note_send(thread(), USR1, true, pre, None);
        assert!(!record.is_discarded(thread(), USR1));
    }

    #[test]
    fn a_blocked_kept_resume_keeps_the_bit() {
        let mut record = DiscardedAtSend::default();
        send(&mut record, thread(), true, false);
        record.consume(
            T,
            P,
            USR1,
            Consumption::KeptResume { blocked: true },
            false,
            false,
        );
        assert!(record.is_discarded(thread(), USR1));
    }

    #[test]
    fn an_unblocked_kept_resume_clears_only_the_emptied_queue() {
        let mut record = DiscardedAtSend::default();
        send(&mut record, thread(), true, false);
        send(&mut record, QueueId::Process(P), true, false);
        record.consume(
            T,
            P,
            USR1,
            Consumption::KeptResume { blocked: false },
            false,
            true,
        );
        assert!(!record.is_discarded(thread(), USR1));
        assert!(record.is_discarded(QueueId::Process(P), USR1));
    }

    #[test]
    fn a_blocked_drop_clears_the_bit() {
        // Codex rev6 P2-1: a dropped copy is gone whatever the mask.
        let mut record = DiscardedAtSend::default();
        send(&mut record, thread(), true, false);
        send(&mut record, QueueId::Process(P), true, false);
        record.consume(T, P, USR1, Consumption::Dropped, false, true);
        assert!(!record.is_discarded(thread(), USR1));
        assert!(record.is_discarded(QueueId::Process(P), USR1));
    }

    #[test]
    fn a_take_answers_the_captured_bit_then_retires_it() {
        // Codex rev6 P2-1: a take is final even when the signal is normally
        // blocked, and it leaves the other queue's bit alone.
        let mut record = DiscardedAtSend::default();
        send(&mut record, thread(), true, false);
        send(&mut record, QueueId::Process(P), true, false);
        assert!(record.take(thread(), USR1));
        assert!(!record.is_discarded(thread(), USR1));
        assert!(record.is_discarded(QueueId::Process(P), USR1));
        assert!(!record.take(thread(), USR1));
    }

    #[test]
    fn a_take_finds_its_queue_from_the_record_alone() {
        let shared = QueueId::Process(P);
        // Private discarded, shared kept, both taken in turn by one read.
        let mut record = DiscardedAtSend::default();
        send(&mut record, thread(), true, false);
        send(&mut record, shared, false, false);
        assert!(record.take_from(T, P, USR1));
        assert!(!record.take_from(T, P, USR1));
        // Private kept by a recorded send, shared discarded: the private one
        // goes first, then the shared one.
        let mut record = DiscardedAtSend::default();
        send(&mut record, shared, true, false);
        send(&mut record, thread(), false, false);
        assert!(!record.take_from(T, P, USR1));
        assert!(record.is_discarded(shared, USR1));
        assert!(record.take_from(T, P, USR1));
        // A copy no send hook saw, after a discarded one was taken, is kept.
        let mut record = DiscardedAtSend::default();
        send(&mut record, thread(), true, false);
        assert!(record.take_from(T, P, USR1));
        assert!(!record.take_from(T, P, USR1));
    }

    #[test]
    fn a_faulted_take_of_a_discarded_copy_is_found_by_its_emptied_queue() {
        let usr1 = kernel_signal_bit(USR1);
        let mut record = DiscardedAtSend::default();
        send(&mut record, thread(), true, false);
        // The take left the discarded copy pending: it took something else.
        assert!(!record.take_faulted(T, P, usr1, usr1, 0));
        assert!(record.is_discarded(thread(), USR1));
        // The take emptied the queue that held the discarded copy.
        assert!(record.take_faulted(T, P, usr1, 0, 0));
        assert!(!record.is_discarded(thread(), USR1));
        // No discarded copy in the set: a kept one faulted.
        assert!(!record.take_faulted(T, P, usr1, 0, 0));
    }

    #[test]
    fn a_flush_clears_every_named_queue() {
        let mut record = DiscardedAtSend::default();
        send(&mut record, thread(), true, false);
        send(&mut record, QueueId::Thread(U), true, false);
        send(&mut record, QueueId::Process(P), true, false);
        record.flush([thread(), QueueId::Process(P)], USR1);
        assert!(!record.is_discarded(thread(), USR1));
        assert!(record.is_discarded(QueueId::Thread(U), USR1));
        assert!(!record.is_discarded(QueueId::Process(P), USR1));
    }

    #[test]
    fn exec_moves_the_private_queue_to_the_leader_id() {
        let mut record = DiscardedAtSend::default();
        send(&mut record, QueueId::Thread(U), true, false);
        send(&mut record, thread(), false, false);
        let leader_bits = record.discarded(thread());
        assert_eq!(leader_bits, 0);
        record.rekey_thread(U, T);
        assert!(record.is_discarded(thread(), USR1));
        assert_eq!(record.discarded(QueueId::Thread(U)), 0);
        // The generation moved with it: a sender's read before the exec
        // still compares against the same count.
        let pre = record.pre_send(thread(), USR1, true);
        record.note_send(thread(), USR1, true, pre, None);
        assert!(record.is_discarded(thread(), USR1));
    }

    #[test]
    fn retirement_forgets_the_queue() {
        let mut record = DiscardedAtSend::default();
        send(&mut record, thread(), true, false);
        record.retire(thread());
        assert_eq!(record.discarded(thread()), 0);
    }

    #[test]
    fn real_time_signals_are_not_tracked() {
        let mut record = DiscardedAtSend::default();
        let pre = record.pre_send(thread(), 34, false);
        record.note_send(thread(), 34, true, pre, None);
        assert_eq!(record.discarded(thread()), 0);
    }

    fn timer(arm_gen: u64) -> TimerProducer {
        TimerProducer {
            process: P,
            timer: 0,
            arm_gen,
        }
    }

    fn timer_send(record: &mut DiscardedAtSend, arm_gen: u64, present: bool) {
        let queue = QueueId::Process(P);
        let pre = record.pre_send(queue, USR1, present);
        record.note_send(queue, USR1, true, pre, Some(timer(arm_gen)));
    }

    #[test]
    fn unignore_clears_a_pending_expiry_of_an_unchanged_live_timer() {
        let mut record = DiscardedAtSend::default();
        timer_send(&mut record, 1, false);
        let resend = record.unignore(P, USR1, |p| p.arm_gen == 1);
        assert!(resend.is_empty());
        assert!(!record.is_discarded(QueueId::Process(P), USR1));
    }

    #[test]
    fn unignore_resends_a_current_expiry_whose_copy_was_taken() {
        // A delivery stop dropped the copy while SIGUSR1 was ignored.
        let mut record = DiscardedAtSend::default();
        timer_send(&mut record, 1, false);
        record.consume(T, P, USR1, Consumption::Dropped, false, false);
        assert_eq!(
            record.unignore(P, USR1, |p| p.arm_gen == 1),
            vec![(QueueId::Process(P), timer(1))]
        );
        // Once: the park is empty.
        assert!(record.unignore(P, USR1, |_| true).is_empty());
    }

    #[test]
    fn unignore_keeps_an_expiry_of_a_rearmed_disarmed_or_deleted_timer() {
        // Codex rev6 P2-3: `current` answers false for each.
        let mut record = DiscardedAtSend::default();
        timer_send(&mut record, 1, false);
        assert!(record.unignore(P, USR1, |p| p.arm_gen == 2).is_empty());
        assert!(record.is_discarded(QueueId::Process(P), USR1));
    }

    #[test]
    fn a_new_ignored_expiry_after_a_rearm_refreshes_the_generation() {
        // Codex rev7: the rearmed timer expired again while still ignored, so
        // the pending copy stands for a current expiry.
        let mut record = DiscardedAtSend::default();
        timer_send(&mut record, 1, false);
        timer_send(&mut record, 2, true);
        assert!(record.is_discarded(QueueId::Process(P), USR1));
        assert!(record.unignore(P, USR1, |p| p.arm_gen == 2).is_empty());
        assert!(!record.is_discarded(QueueId::Process(P), USR1));
    }

    #[test]
    fn unignore_leaves_a_bit_no_timer_set() {
        let mut record = DiscardedAtSend::default();
        send(&mut record, QueueId::Process(P), true, false);
        assert!(record.unignore(P, USR1, |_| true).is_empty());
        assert!(record.is_discarded(QueueId::Process(P), USR1));
    }

    fn siginfo(code: i32) -> KernelSiginfo {
        let mut info = [0u8; super::super::signal_lists::KERNEL_SIGINFO_SIZE];
        info[0..4].copy_from_slice(&USR1.to_ne_bytes());
        info[8..12].copy_from_slice(&code.to_ne_bytes());
        KernelSiginfo(info)
    }

    fn listed_send(
        record: &mut DiscardedAtSend,
        queue: QueueId,
        discarded: bool,
        code: i32,
        listed: bool,
    ) {
        let present = record.is_discarded(queue, USR1) || record.lists.is_listed(queue, USR1);
        let pre = record.pre_send(queue, USR1, present);
        let record_ = SendRecord {
            siginfo: siginfo(code),
            listed,
        };
        record.note_send_listed(P, queue, USR1, discarded, pre, None, record_);
    }

    const SI_QUEUE: i32 = -1;
    const SI_TIMER: i32 = super::super::signal_lists::SI_TIMER;

    #[test]
    fn a_kept_send_of_a_listed_pair_becomes_the_front_of_an_empty_list() {
        let mut record = DiscardedAtSend::default();
        listed_send(&mut record, thread(), false, 0, true);
        assert_eq!(record.lists.entries(thread(), USR1).len(), 1);
        // Linux drops a second ordinary send (`legacy_queue`).
        listed_send(&mut record, thread(), false, SI_QUEUE, true);
        assert_eq!(record.lists.entries(thread(), USR1).len(), 1);
        assert!(record.lists.invariants_hold());
    }

    #[test]
    fn an_unlisted_pair_keeps_no_list() {
        let mut record = DiscardedAtSend::default();
        listed_send(&mut record, thread(), false, 0, false);
        assert!(!record.lists.is_listed(thread(), USR1));
    }

    #[test]
    fn a_kept_send_on_a_discarded_si_timer_copy_is_a_proxy_only_in_a_timer_pair() {
        use super::super::signal_lists::FrontClass;
        for (history, class) in [(false, FrontClass::Own), (true, FrontClass::Proxy)] {
            let mut record = DiscardedAtSend::default();
            if history {
                record.note_timer_history(P, USR1);
            }
            listed_send(&mut record, thread(), true, SI_TIMER, true);
            assert!(record.discarded_is_si_timer(thread(), USR1));
            listed_send(&mut record, thread(), false, SI_QUEUE, true);
            let (_, front_class, physical_si_timer) = record.lists.front(thread(), USR1).unwrap();
            assert_eq!(front_class, class);
            // The physical record is the forged one either way.
            assert!(physical_si_timer);
            assert!(!record.is_discarded(thread(), USR1));
            assert!(!record.discarded_is_si_timer(thread(), USR1));
        }
    }

    #[test]
    fn a_discarded_send_coalescing_into_a_pending_copy_records_no_si_code() {
        let mut record = DiscardedAtSend::default();
        listed_send(&mut record, thread(), true, 0, true);
        listed_send(&mut record, thread(), true, SI_TIMER, true);
        assert!(record.is_discarded(thread(), USR1));
        assert!(!record.discarded_is_si_timer(thread(), USR1));
    }

    #[test]
    fn the_timer_history_survives_rekey_and_goes_with_the_process() {
        let mut record = DiscardedAtSend::default();
        record.note_timer_history(P, USR1);
        listed_send(&mut record, QueueId::Thread(U), false, 0, true);
        record.rekey_thread(U, T);
        assert!(record.lists.is_listed(thread(), USR1));
        assert!(!record.lists.is_listed(QueueId::Thread(U), USR1));
        assert!(record.has_timer_history(P, USR1));
        record.retire(thread());
        assert!(!record.lists.is_listed(thread(), USR1));
        assert!(record.has_timer_history(P, USR1));
        record.retire(QueueId::Process(P));
        assert!(!record.has_timer_history(P, USR1));
    }
}

#[cfg(test)]
mod alarm_origin_tests {
    use super::*;

    const T: DetTid = DetTid::from_raw(10);
    const U: DetTid = DetTid::from_raw(11);
    const P: DetPid = DetPid::from_raw(10);
    const ALRM: i32 = libc::SIGALRM;
    const TIMER: AlarmOrigin = AlarmOrigin::ItimerReal;
    const OTHER: AlarmOrigin = AlarmOrigin::Other;
    const UNKNOWN: AlarmOrigin = AlarmOrigin::Unknown;

    fn send(
        r: &mut DiscardedAtSend,
        q: QueueId,
        origin: AlarmOrigin,
        discarded: bool,
        present: bool,
    ) {
        let pre = r.pre_send(q, ALRM, present);
        match origin {
            AlarmOrigin::ItimerReal => r.note_itimer_real_send(q, discarded, pre),
            AlarmOrigin::Other => r.note_send(q, ALRM, discarded, pre, None),
            AlarmOrigin::Unknown => panic!("Unknown is not an authenticated send tag"),
        }
    }

    #[test]
    fn every_two_send_order_preserves_the_first_kept_origin() {
        for q in [QueueId::Thread(T), QueueId::Process(P)] {
            for first in [TIMER, OTHER] {
                for second in [TIMER, OTHER] {
                    let mut r = DiscardedAtSend::default();
                    send(&mut r, q, first, false, false);
                    send(&mut r, q, second, false, true);
                    assert_eq!(r.alarm_origin(q), first);
                    assert!(!r.is_discarded(q, ALRM));
                }
            }
        }
    }

    #[test]
    fn a_discarded_later_send_does_not_replace_either_kept_origin() {
        for first in [TIMER, OTHER] {
            for later in [TIMER, OTHER] {
                let q = QueueId::Process(P);
                let mut r = DiscardedAtSend::default();
                send(&mut r, q, first, false, false);
                send(&mut r, q, later, true, true);
                assert_eq!(r.alarm_origin(q), first);
                assert!(!r.is_discarded(q, ALRM));
            }
        }
    }

    #[test]
    fn either_kept_origin_replaces_a_known_discarded_artifact() {
        for first in [TIMER, OTHER] {
            for kept in [TIMER, OTHER] {
                let q = QueueId::Process(P);
                let mut r = DiscardedAtSend::default();
                send(&mut r, q, first, true, false);
                assert_eq!(r.alarm_origin(q), UNKNOWN);
                assert!(r.is_discarded(q, ALRM));
                send(&mut r, q, kept, false, true);
                assert_eq!(r.alarm_origin(q), kept);
                assert!(!r.is_discarded(q, ALRM));
            }
        }
    }

    #[test]
    fn an_unknown_physical_first_copy_stays_unknown_for_every_later_send() {
        for later in [TIMER, OTHER] {
            for discarded in [false, true] {
                let q = QueueId::Process(P);
                let mut r = DiscardedAtSend::default();
                send(&mut r, q, later, discarded, true);
                assert_eq!(r.alarm_origin(q), UNKNOWN);
                assert!(!r.is_discarded(q, ALRM));
                send(&mut r, q, TIMER, false, true);
                assert_eq!(r.alarm_origin(q), UNKNOWN);
            }
        }
    }

    #[test]
    fn explicit_claim_retires_before_a_prehook_can_publish_the_next_copy() {
        let q = QueueId::Process(P);
        let mut r = DiscardedAtSend::default();
        send(&mut r, q, TIMER, false, false);
        let before = r.pre_send(q, ALRM, true).generation;
        assert!(r.claim_itimer_real(q));
        assert_eq!(r.alarm_origin(q), UNKNOWN);
        assert_eq!(r.pre_send(q, ALRM, false).generation, before + 1);
        // Models a later prehook send, not the same already-dequeued copy.
        send(&mut r, q, OTHER, false, false);
        let generation = r.pre_send(q, ALRM, true).generation;
        assert!(!r.claim_itimer_real(q));
        assert_eq!(r.alarm_origin(q), OTHER);
        assert!(r.has_recorded_copy(q, ALRM));
        assert_eq!(r.pre_send(q, ALRM, true).generation, generation);
        r.take(q, ALRM);
        assert!(!r.claim_itimer_real(q));
    }

    #[test]
    fn explicit_private_first_claim_preserves_the_shared_timer() {
        let private = QueueId::Thread(T);
        let shared = QueueId::Process(P);
        let mut r = DiscardedAtSend::default();
        send(&mut r, private, OTHER, false, false);
        send(&mut r, shared, TIMER, false, false);
        let generation = r.pre_send(private, ALRM, true).generation;
        assert!(!r.claim_itimer_real(private));
        assert_eq!(r.alarm_origin(private), OTHER);
        assert_eq!(r.pre_send(private, ALRM, true).generation, generation);
        // Existing ordinary-copy path consumes the selected private copy.
        r.take(private, ALRM);
        assert_eq!(r.alarm_origin(shared), TIMER);
        assert!(r.claim_itimer_real(shared));
    }

    #[test]
    fn a_generic_ambiguous_take_cannot_leave_positive_shared_provenance() {
        let shared = QueueId::Process(P);
        let mut r = DiscardedAtSend::default();
        send(&mut r, shared, TIMER, false, false);
        // Old bool interface cannot exclude an unknown physical private copy.
        assert!(!r.take_from(T, P, ALRM));
        assert_eq!(r.alarm_origin(shared), UNKNOWN);
    }

    #[test]
    fn an_explicit_take_clears_only_its_selected_origin() {
        let private = QueueId::Thread(T);
        let shared = QueueId::Process(P);
        let mut r = DiscardedAtSend::default();
        send(&mut r, private, OTHER, false, false);
        send(&mut r, shared, TIMER, false, false);
        assert!(!r.take(private, ALRM));
        assert_eq!(r.alarm_origin(private), UNKNOWN);
        assert_eq!(r.alarm_origin(shared), TIMER);
        assert!(!r.take(shared, ALRM));
        assert_eq!(r.alarm_origin(shared), UNKNOWN);
    }

    #[test]
    fn faulted_takes_clear_emptied_alarm_origins_but_keep_pending_ones() {
        let private = QueueId::Thread(T);
        let shared = QueueId::Process(P);
        let alarm = kernel_signal_bit(ALRM);
        let mut r = DiscardedAtSend::default();
        send(&mut r, private, OTHER, false, false);
        send(&mut r, shared, TIMER, false, false);
        assert!(!r.take_faulted(T, P, alarm, 0, alarm));
        assert_eq!(r.alarm_origin(private), UNKNOWN);
        assert_eq!(r.alarm_origin(shared), TIMER);
        assert!(!r.take_faulted(T, P, alarm, 0, 0));
        assert_eq!(r.alarm_origin(shared), UNKNOWN);
    }

    #[test]
    fn blocked_resume_keeps_origins_and_an_emptied_consumption_clears_them() {
        let private = QueueId::Thread(T);
        let shared = QueueId::Process(P);
        let mut r = DiscardedAtSend::default();
        send(&mut r, private, OTHER, false, false);
        send(&mut r, shared, TIMER, false, false);
        r.consume(
            T,
            P,
            ALRM,
            Consumption::KeptResume { blocked: true },
            false,
            false,
        );
        assert_eq!(r.alarm_origin(private), OTHER);
        assert_eq!(r.alarm_origin(shared), TIMER);
        r.consume(T, P, ALRM, Consumption::Dropped, false, true);
        assert_eq!(r.alarm_origin(private), UNKNOWN);
        assert_eq!(r.alarm_origin(shared), TIMER);
        r.consume(
            T,
            P,
            ALRM,
            Consumption::KeptResume { blocked: false },
            false,
            false,
        );
        assert_eq!(r.alarm_origin(shared), UNKNOWN);
    }

    #[test]
    fn flush_retire_and_rekey_follow_existing_queue_lifetimes() {
        let private = QueueId::Thread(T);
        let moved = QueueId::Thread(U);
        let shared = QueueId::Process(P);
        let mut r = DiscardedAtSend::default();
        send(&mut r, private, OTHER, false, false);
        send(&mut r, moved, TIMER, false, false);
        send(&mut r, shared, TIMER, false, false);
        r.rekey_thread(U, T);
        assert_eq!(r.alarm_origin(private), TIMER);
        assert_eq!(r.alarm_origin(moved), UNKNOWN);
        assert_eq!(r.alarm_origin(shared), TIMER);
        r.rekey_thread(T, T);
        assert_eq!(r.alarm_origin(private), TIMER);
        r.flush([private], ALRM);
        assert_eq!(r.alarm_origin(private), UNKNOWN);
        assert_eq!(r.alarm_origin(shared), TIMER);
        r.retire(shared);
        assert_eq!(r.alarm_origin(shared), UNKNOWN);
    }

    #[test]
    fn unrelated_signals_do_not_clear_or_forge_alarm_origin() {
        let q = QueueId::Thread(T);
        let mut r = DiscardedAtSend::default();
        send(&mut r, q, TIMER, false, false);
        let pre = r.pre_send(q, libc::SIGUSR1, false);
        r.note_send(q, libc::SIGUSR1, false, pre, None);
        r.take(q, libc::SIGUSR1);
        r.flush([q], 34);
        assert_eq!(r.alarm_origin(q), TIMER);
    }

    #[test]
    fn a_known_empty_queue_or_generation_change_allows_a_new_first_origin() {
        let q = QueueId::Process(P);
        let mut r = DiscardedAtSend::default();
        send(&mut r, q, TIMER, false, false);
        // Physical pre-send emptiness supersedes an old stale origin.
        send(&mut r, q, OTHER, false, false);
        assert_eq!(r.alarm_origin(q), OTHER);
        let pre = r.pre_send(q, ALRM, true);
        r.take(q, ALRM);
        r.note_itimer_real_send(q, false, pre);
        assert_eq!(r.alarm_origin(q), TIMER);
    }

    #[test]
    fn false_claims_preserve_discarded_unknown_and_other_signal_records() {
        let q = QueueId::Process(P);
        let mut r = DiscardedAtSend::default();
        send(&mut r, q, OTHER, true, false);
        let generation = r.pre_send(q, ALRM, true).generation;
        assert!(!r.claim_itimer_real(q));
        assert!(r.is_discarded(q, ALRM));
        assert!(r.has_recorded_copy(q, ALRM));
        assert_eq!(r.pre_send(q, ALRM, true).generation, generation);
        r.take(q, ALRM);
        send(&mut r, q, TIMER, false, true); // Physical first is unknown.
        assert_eq!(r.alarm_origin(q), UNKNOWN);
        assert!(!r.claim_itimer_real(q));
        assert!(r.has_recorded_copy(q, ALRM));
        assert!(!r.has_recorded_copy(q, 34));
        assert!(!r.has_recorded_copy(QueueId::Thread(T), ALRM));
    }

    #[test]
    fn a_posix_timer_unignored_as_sigalrm_is_other_not_itimer_real() {
        let q = QueueId::Process(P);
        let mut r = DiscardedAtSend::default();
        let producer = TimerProducer {
            process: P,
            timer: 7,
            arm_gen: 2,
        };
        let pre = r.pre_send(q, ALRM, false);
        r.note_send(q, ALRM, true, pre, Some(producer));
        assert_eq!(r.alarm_origin(q), UNKNOWN);
        assert!(r.unignore(P, ALRM, |p| *p == producer).is_empty());
        assert_eq!(r.alarm_origin(q), OTHER);
        send(&mut r, q, TIMER, false, true);
        assert_eq!(r.alarm_origin(q), OTHER);
    }
}

#[cfg(test)]
mod reported_alarm_tests {
    use super::*;

    #[test]
    fn canonical_itimer_info_zeros_every_non_header_byte() {
        let info = itimer_real_siginfo();
        assert_eq!(
            i32::from_ne_bytes(info[..4].try_into().unwrap()),
            libc::SIGALRM
        );
        assert_eq!(
            i32::from_ne_bytes(info[8..12].try_into().unwrap()),
            libc::SI_KERNEL
        );
        assert!(info[4..8].iter().chain(&info[12..]).all(|byte| *byte == 0));
    }

    #[test]
    fn unreported_origin_loss_survives_thread_exec_rekey_until_process_retirement() {
        let process = DetPid::from_raw(100);
        let thread = DetTid::from_raw(101);
        let leader = DetTid::from_raw(100);
        let mut record = DiscardedAtSend::default();
        record.alarm_unreported.insert(process);
        record.rekey_thread(thread, leader);
        assert!(record.alarm_unreported.contains(&process));
        record.retire(QueueId::Thread(leader));
        assert!(record.alarm_unreported.contains(&process));
        record.retire(QueueId::Process(process));
        assert!(!record.alarm_unreported.contains(&process));
    }
}
