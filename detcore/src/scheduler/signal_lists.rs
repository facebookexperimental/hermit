/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The logical entries of each pending standard signal, per queue
//! (<https://github.com/rrnewton/hermit/issues/4262>).
//!
//! Linux keeps a POSIX timer's preallocated sigqueue as its own entry: it is
//! appended without the `legacy_queue` check that drops a second ordinary
//! send of a pending standard signal (`posixtimer_queue_sigqueue`,
//! kernel/signal.c), so a queue can hold an ordinary copy of a signal and,
//! behind it, one entry per timer that notifies with it. Each dequeue takes
//! one entry. Under ptrace a scheduler send merges into the one pending
//! physical copy, so this list keeps the entries Linux would hold, and the
//! scheduler re-sends the next one when the front is dequeued (promotion).
//!
//! Invariants, for every (queue, signal) that has a list:
//! - **I2:** only the front entry can be [`Entry::Plain`]: an ordinary send
//!   into a non-empty list adds nothing (`legacy_queue`).
//! - **I3:** a timer has at most one entry across all lists.
//! - The front's physical record is the front's own send ([`FrontClass::Own`])
//!   or another sender's record standing for it ([`FrontClass::Proxy`]), and
//!   whether that physical record carries `SI_TIMER` is kept separately
//!   (`physical_si_timer`), because a guest can forge `SI_TIMER`.
//!
//! I1 (the physical bit is set iff the list is non-empty or the discarded
//! bit is set) relates the list to the kernel's queue and 4351's record; the
//! scheduler checks it, not this module.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use serde::Deserialize;
use serde::Serialize;

use super::discarded_at_send::QueueId;
use crate::types::DetPid;

/// Linux's `kernel_siginfo_t` is 48 bytes; copy-out zeroes the rest.
pub(crate) const KERNEL_SIGINFO_SIZE: usize = 48;

/// `si_code` of a POSIX timer's signal.
pub(crate) const SI_TIMER: i32 = -2;

/// One logical entry of a pending signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Entry {
    /// An ordinary send (`kill`, `tgkill`, `rt_sig(tg)queueinfo`, a pidfd, a
    /// scheduler send other than a POSIX timer's), with the sender's siginfo.
    Plain {
        siginfo: KernelSiginfo,
        /// The sender's own `si_code` was `SI_TIMER` (a forged one): exec
        /// flushes it like a timer's (`flush_itimer_signals`).
        forged_si_timer: bool,
    },
    /// A POSIX timer's expiry.
    Timer(TimerEntry),
}

/// A POSIX timer's entry: which timer, and the state Linux latches at each
/// send of its sigqueue (`it_sigqueue_seq`, `it_sig_periodic`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TimerEntry {
    pub(crate) process: DetPid,
    pub(crate) timer: i32,
    /// The timer's arming generation at its latest send
    /// (`Scheduler::posix_timer_arm_generation`).
    pub(crate) gen_at_send: u64,
    /// Whether the timer was periodic at its latest send.
    pub(crate) periodic_at_send: bool,
}

/// Whose record the front's physical copy is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FrontClass {
    /// The front's own send.
    Own,
    /// Another sender's record, standing for the front (a timer's send that
    /// merged into a discarded copy, or an unignore that reused one).
    Proxy,
}

/// The entries of one (queue, signal), front first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SignalList {
    entries: VecDeque<Entry>,
    front_class: FrontClass,
    /// Whether the physical record standing for the front carries `SI_TIMER`.
    physical_si_timer: bool,
}

/// What appending a timer's expiry did (`SignalLists::append_timer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimerAppend {
    /// The list was empty: the scheduler sends the timer's record, which is
    /// the front's own.
    SendFront,
    /// The list was empty, but a discarded copy of the signal is physically
    /// pending, so the kernel drops the scheduler's send (`legacy_queue`):
    /// that copy stands for the front.
    ProxyFront,
    /// Appended behind other entries: sent at its promotion.
    Behind,
    /// The timer already had an entry here: refreshed in place
    /// (ALREADY_PENDING); nothing is sent.
    AlreadyPending,
}

/// The front entry a dequeue took, and what follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Popped {
    pub(crate) entry: Entry,
    pub(crate) class: FrontClass,
    pub(crate) physical_si_timer: bool,
    /// The new front, which the scheduler must re-send (promotion), if any.
    pub(crate) next: Option<Entry>,
}

/// What an exec does to the physical queues (`SignalLists::exec`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ExecOutcome {
    /// `Proxy` `Plain` fronts whose physical record the kernel flushed: each
    /// is sent again (`kill` for the shared queue, `tgkill` for a private
    /// one), with its stored siginfo for the rewrite.
    pub(crate) resend: Vec<(QueueId, i32, KernelSiginfo)>,
    /// Physical records the kernel kept whose logical entries exec removed.
    pub(crate) take_back: Vec<(QueueId, i32)>,
}

/// Every list (see the module documentation).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SignalLists {
    lists: BTreeMap<(QueueId, i32), SignalList>,
}

impl SignalLists {
    /// Whether (queue, signal) has a list.
    pub(crate) fn is_listed(&self, queue: QueueId, signal: i32) -> bool {
        self.lists.contains_key(&(queue, signal))
    }

    /// The entries of (queue, signal), front first.
    pub(crate) fn entries(&self, queue: QueueId, signal: i32) -> Vec<Entry> {
        self.lists
            .get(&(queue, signal))
            .map(|list| list.entries.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The front's class and whether its physical record carries `SI_TIMER`.
    pub(crate) fn front(&self, queue: QueueId, signal: i32) -> Option<(Entry, FrontClass, bool)> {
        let list = self.lists.get(&(queue, signal))?;
        Some((
            *list.entries.front()?,
            list.front_class,
            list.physical_si_timer,
        ))
    }

    /// A1: an ordinary send of `signal` into `queue`, kept by Linux, with the
    /// sender's siginfo. It becomes the front only if the list is empty;
    /// otherwise Linux drops it (`legacy_queue`). `discarded_physical` says a
    /// copy Linux discarded is physically pending there, and whether that
    /// record's `si_code` is `SI_TIMER`: it, not the send, is the physical
    /// record standing for the front. The front is `Proxy` only in a
    /// `timer_pair` (rev 8 E'); elsewhere it keeps 4351's behaviour, so a
    /// timer-free program meets no `Proxy` refusal. Returns whether it was
    /// appended.
    pub(crate) fn append_plain(
        &mut self,
        queue: QueueId,
        signal: i32,
        siginfo: KernelSiginfo,
        discarded_physical: Option<bool>,
        timer_pair: bool,
    ) -> bool {
        if self.is_listed(queue, signal) {
            return false;
        }
        let own_si_timer = siginfo_code(&siginfo) == SI_TIMER;
        let front_class = if discarded_physical.is_some() && timer_pair {
            FrontClass::Proxy
        } else {
            FrontClass::Own
        };
        self.lists.insert(
            (queue, signal),
            SignalList {
                entries: VecDeque::from([Entry::Plain {
                    siginfo,
                    forged_si_timer: own_si_timer,
                }]),
                front_class,
                physical_si_timer: discarded_physical.unwrap_or(own_si_timer),
            },
        );
        true
    }

    /// A timer's expiry into `queue` as `signal` (Linux appends its sigqueue
    /// unconditionally). `discarded_pending` says a copy Linux discarded is
    /// physically pending there, whose `si_code` is `physical_si_timer`.
    pub(crate) fn append_timer(
        &mut self,
        queue: QueueId,
        signal: i32,
        entry: TimerEntry,
        discarded_pending: Option<bool>,
    ) -> TimerAppend {
        if self.refresh_timer(entry) {
            return TimerAppend::AlreadyPending;
        }
        if let Some(list) = self.lists.get_mut(&(queue, signal)) {
            list.entries.push_back(Entry::Timer(entry));
            return TimerAppend::Behind;
        }
        let (front_class, physical_si_timer, append) = match discarded_pending {
            Some(physical_si_timer) => (
                FrontClass::Proxy,
                physical_si_timer,
                TimerAppend::ProxyFront,
            ),
            None => (FrontClass::Own, true, TimerAppend::SendFront),
        };
        self.lists.insert(
            (queue, signal),
            SignalList {
                entries: VecDeque::from([Entry::Timer(entry)]),
                front_class,
                physical_si_timer,
            },
        );
        append
    }

    /// The refresh every expiry of a timer makes first: its entry, wherever
    /// it is, takes the timer's current generation and periodicity (Linux
    /// sets `it_sigqueue_seq` and `it_sig_periodic` before the
    /// ALREADY_PENDING return). Returns whether the timer had an entry.
    pub(crate) fn refresh_timer(&mut self, current: TimerEntry) -> bool {
        for list in self.lists.values_mut() {
            for entry in list.entries.iter_mut() {
                if let Entry::Timer(timer) = entry
                    && timer.process == current.process
                    && timer.timer == current.timer
                {
                    *timer = current;
                    return true;
                }
            }
        }
        false
    }

    /// Whether the timer has an entry in any list.
    #[cfg(test)]
    pub(crate) fn timer_entry(&self, process: DetPid, timer: i32) -> Option<(QueueId, i32, TimerEntry)> {
        self.lists.iter().find_map(|(&(queue, signal), list)| {
            list.entries.iter().find_map(|entry| match entry {
                Entry::Timer(t) if t.process == process && t.timer == timer => {
                    Some((queue, signal, *t))
                }
                _ => None,
            })
        })
    }

    /// A dequeue of (queue, signal) took the front. The new front, if any, is
    /// the scheduler's to re-send; it is then the front's own record.
    pub(crate) fn pop(&mut self, queue: QueueId, signal: i32) -> Option<Popped> {
        let list = self.lists.get_mut(&(queue, signal))?;
        let entry = list.entries.pop_front()?;
        let popped = Popped {
            entry,
            class: list.front_class,
            physical_si_timer: list.physical_si_timer,
            next: list.entries.front().copied(),
        };
        match popped.next {
            None => {
                self.lists.remove(&(queue, signal));
            }
            Some(next) => {
                list.front_class = FrontClass::Own;
                list.physical_si_timer = matches!(next, Entry::Timer(_));
            }
        }
        Some(popped)
    }

    /// Remove (queue, signal)'s list entirely (a flush, a retired queue),
    /// returning its entries front first.
    pub(crate) fn take_all(&mut self, queue: QueueId, signal: i32) -> Vec<Entry> {
        self.lists
            .remove(&(queue, signal))
            .map(|list| list.entries.into_iter().collect())
            .unwrap_or_default()
    }

    /// Whether (queue, signal) holds an entry a dequeue would deliver: a
    /// `Plain` front, or any `Timer` whose generation is current. Linux skips
    /// every stale timer entry inside one dequeue (`goto again`).
    pub(crate) fn deliverable(
        &self,
        queue: QueueId,
        signal: i32,
        current_gen: impl Fn(DetPid, i32) -> u64,
    ) -> bool {
        self.lists.get(&(queue, signal)).is_some_and(|list| {
            list.entries
                .iter()
                .enumerate()
                .any(|(index, entry)| match entry {
                    Entry::Plain { .. } => index == 0,
                    Entry::Timer(t) => t.gen_at_send == current_gen(t.process, t.timer),
                })
        })
    }

    /// A successful exec flushed every physical record whose `si_code` is
    /// `SI_TIMER` from `queues`, the survivor's private queue and the shared
    /// one (`flush_itimer_signals`). Removes every logical entry Linux would
    /// have flushed (every `Timer`, every forged `Plain`), then compares each
    /// list's physical record with what survives (rev 6 §4.9, rev 10 K):
    /// - the kernel flushed the physical record and a `Plain` survives: Linux
    ///   kept that entry. A `Proxy` front is re-sent, and stays `Proxy` so the
    ///   stop and take rewrite its siginfo from the stored bytes; any other is
    ///   popped (N9-2, N10-2), as 4351 leaves it;
    /// - the kernel kept the physical record and no entry survives: Linux had
    ///   no such copy, so it is taken back.
    pub(crate) fn exec(&mut self, queues: &[QueueId]) -> ExecOutcome {
        let mut outcome = ExecOutcome::default();
        let keys: Vec<(QueueId, i32)> = self
            .lists
            .keys()
            .copied()
            .filter(|(queue, _)| queues.contains(queue))
            .collect();
        for key in keys {
            let list = self.lists.get_mut(&key).unwrap();
            let flushed = list.physical_si_timer;
            list.entries.retain(|entry| {
                matches!(
                    entry,
                    Entry::Plain {
                        forged_si_timer: false,
                        ..
                    }
                )
            });
            let survivor = list.entries.front().copied();
            match (flushed, survivor) {
                (true, Some(Entry::Plain { siginfo, .. }))
                    if list.front_class == FrontClass::Proxy =>
                {
                    list.physical_si_timer = false;
                    outcome.resend.push((key.0, key.1, siginfo));
                }
                (true, Some(_)) | (true, None) => {
                    self.lists.remove(&key);
                }
                (false, Some(_)) => {}
                (false, None) => {
                    self.lists.remove(&key);
                    outcome.take_back.push(key);
                }
            }
        }
        outcome
    }

    /// `de_thread` gave the non-leader `from` the leader's id `to`: its
    /// private lists move with it, and the old leader's are dropped.
    pub(crate) fn rekey_thread(&mut self, from: QueueId, to: QueueId) {
        if from == to {
            return;
        }
        self.retire(to);
        let moved: Vec<(i32, SignalList)> = self
            .lists
            .keys()
            .filter(|(queue, _)| *queue == from)
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|key| (key.1, self.lists.remove(&key).unwrap()))
            .collect();
        for (signal, list) in moved {
            self.lists.insert((to, signal), list);
        }
    }

    /// `queue` went away with its thread or process: its lists are dropped,
    /// and the timers their entries named stop being queued.
    pub(crate) fn retire(&mut self, queue: QueueId) -> Vec<Entry> {
        let keys: Vec<(QueueId, i32)> = self
            .lists
            .keys()
            .filter(|(owner, _)| *owner == queue)
            .copied()
            .collect();
        keys.into_iter()
            .flat_map(|key| self.lists.remove(&key).unwrap().entries)
            .collect()
    }

    /// Every (queue, signal) that has a list.
    pub(crate) fn listed(&self) -> Vec<(QueueId, i32)> {
        self.lists.keys().copied().collect()
    }

    /// I2 and I3 hold.
    pub(crate) fn invariants_hold(&self) -> bool {
        let mut timers = std::collections::BTreeSet::new();
        self.lists.values().all(|list| {
            !list.entries.is_empty()
                && list
                    .entries
                    .iter()
                    .skip(1)
                    .all(|entry| matches!(entry, Entry::Timer(_)))
                && list.entries.iter().all(|entry| match entry {
                    Entry::Timer(t) => timers.insert((t.process, t.timer)),
                    Entry::Plain { .. } => true,
                })
        })
    }
}

/// The `si_code` field of a kernel siginfo (bytes 8-11).
pub(crate) fn siginfo_code(siginfo: &KernelSiginfo) -> i32 {
    i32::from_ne_bytes(siginfo.0[8..12].try_into().unwrap())
}

/// The 128-byte `signalfd_siginfo` a signalfd read returns for `info`, as
/// `signalfd_copyinfo` lays it out (fs/signalfd.c): `SI_TIMER` fills the
/// timer id, overrun and value; any other negative code (`siginfo_layout`'s
/// `SIL_RT`, `rt_sigqueueinfo`'s and `tgkill`'s) the sender, uid and value;
/// a non-negative one the sender and uid. Every other byte is zero.
pub(crate) fn signalfd_record(info: &KernelSiginfo) -> [u8; 128] {
    let si = &info.0;
    let mut record = [0u8; 128];
    record[0..12].copy_from_slice(&si[0..12]);
    let code = siginfo_code(info);
    if code == SI_TIMER {
        record[24..28].copy_from_slice(&si[16..20]);
        record[32..36].copy_from_slice(&si[20..24]);
        record[44..48].copy_from_slice(&si[24..28]);
        record[48..56].copy_from_slice(&si[24..32]);
    } else {
        record[12..20].copy_from_slice(&si[16..24]);
        if code < 0 {
            record[44..48].copy_from_slice(&si[24..28]);
            record[48..56].copy_from_slice(&si[24..32]);
        }
    }
    record
}

/// The siginfo of an ordinary send of `signal` with `code` (`SI_USER` 0,
/// `SI_TKILL` -6, `SI_KERNEL` 0x80) from `pid` and `uid` (the `_kill`
/// member), every other byte zero.
pub(crate) fn ordinary_siginfo(signal: i32, code: i32, pid: i32, uid: u32) -> KernelSiginfo {
    let mut info = [0u8; KERNEL_SIGINFO_SIZE];
    info[0..4].copy_from_slice(&signal.to_ne_bytes());
    info[8..12].copy_from_slice(&code.to_ne_bytes());
    info[16..20].copy_from_slice(&pid.to_ne_bytes());
    info[20..24].copy_from_slice(&uid.to_ne_bytes());
    KernelSiginfo(info)
}

/// The `SI_TIMER` siginfo of a timer's entry, as Linux builds it at dequeue:
/// `si_signo`, `si_code = SI_TIMER`, `si_tid` (the timer id), `si_overrun`
/// and the 64-bit `si_value`, every other byte zero (`kernel_siginfo_t`; the
/// `_timer` member's `si_sys_private` stays 0).
pub(crate) fn timer_siginfo(signal: i32, timer: i32, overrun: i32, value: u64) -> KernelSiginfo {
    let mut info = [0u8; KERNEL_SIGINFO_SIZE];
    info[0..4].copy_from_slice(&signal.to_ne_bytes());
    info[8..12].copy_from_slice(&SI_TIMER.to_ne_bytes());
    info[16..20].copy_from_slice(&timer.to_ne_bytes());
    info[20..24].copy_from_slice(&overrun.to_ne_bytes());
    info[24..32].copy_from_slice(&value.to_ne_bytes());
    KernelSiginfo(info)
}

/// A sender's siginfo as Linux keeps it (`kernel_siginfo_t`, 48 bytes).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct KernelSiginfo(pub [u8; KERNEL_SIGINFO_SIZE]);

impl KernelSiginfo {
    /// The 128-byte `siginfo_t` a sender passes, zero past the first 48
    /// bytes, as copy-out leaves it.
    pub(crate) fn to_user(self) -> [u8; 128] {
        let mut user = [0u8; 128];
        user[..KERNEL_SIGINFO_SIZE].copy_from_slice(&self.0);
        user
    }

    /// The first 48 bytes of a 128-byte `siginfo_t`.
    pub(crate) fn from_user(user: &[u8; 128]) -> Self {
        Self(user[..KERNEL_SIGINFO_SIZE].try_into().unwrap())
    }
}

impl std::fmt::Debug for KernelSiginfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KernelSiginfo(code {})", siginfo_code(self))
    }
}

// Serde derives arrays only up to 32 elements.
impl Serialize for KernelSiginfo {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for KernelSiginfo {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        bytes
            .try_into()
            .map(KernelSiginfo)
            .map_err(|_| serde::de::Error::custom("a siginfo must be 48 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DetTid;

    const P: DetPid = DetPid::from_raw(10);
    const USR1: i32 = libc::SIGUSR1;

    fn shared() -> QueueId {
        QueueId::Process(P)
    }

    fn timer(id: i32, generation: u64) -> TimerEntry {
        TimerEntry {
            process: P,
            timer: id,
            gen_at_send: generation,
            periodic_at_send: true,
        }
    }

    fn kill_info() -> KernelSiginfo {
        let mut info = [0u8; KERNEL_SIGINFO_SIZE];
        info[0..4].copy_from_slice(&USR1.to_ne_bytes());
        KernelSiginfo(info)
    }

    /// `rt_sigqueueinfo`'s `SI_QUEUE` record with a 64-bit `sival_ptr`.
    fn queue_info() -> KernelSiginfo {
        let mut info = kill_info();
        info.0[8..12].copy_from_slice(&(-1i32).to_ne_bytes());
        info.0[24..32].copy_from_slice(&0x1122334455667788u64.to_ne_bytes());
        info
    }

    fn private() -> QueueId {
        QueueId::Thread(DetTid::from_raw(10))
    }

    #[test]
    fn an_ordinary_send_into_a_non_empty_list_adds_nothing() {
        let mut lists = SignalLists::default();
        assert_eq!(
            lists.append_timer(shared(), USR1, timer(1, 1), None),
            TimerAppend::SendFront
        );
        assert!(!lists.append_plain(shared(), USR1, kill_info(), None, true));
        assert_eq!(
            lists.entries(shared(), USR1),
            vec![Entry::Timer(timer(1, 1))]
        );
        assert!(lists.invariants_hold());
    }

    #[test]
    fn a_timer_goes_behind_an_ordinary_front_and_is_promoted_at_its_dequeue() {
        let mut lists = SignalLists::default();
        assert!(lists.append_plain(shared(), USR1, kill_info(), None, true));
        assert_eq!(
            lists.append_timer(shared(), USR1, timer(1, 1), None),
            TimerAppend::Behind
        );
        let popped = lists.pop(shared(), USR1).unwrap();
        assert!(matches!(popped.entry, Entry::Plain { .. }));
        assert_eq!(popped.next, Some(Entry::Timer(timer(1, 1))));
        let (front, class, physical_si_timer) = lists.front(shared(), USR1).unwrap();
        assert_eq!(front, Entry::Timer(timer(1, 1)));
        assert_eq!(class, FrontClass::Own);
        assert!(physical_si_timer);
        assert!(lists.pop(shared(), USR1).unwrap().next.is_none());
        assert!(!lists.is_listed(shared(), USR1));
    }

    #[test]
    fn an_expiry_with_its_entry_queued_refreshes_it_in_place() {
        let mut lists = SignalLists::default();
        lists.append_timer(shared(), USR1, timer(1, 1), None);
        let refreshed = TimerEntry {
            gen_at_send: 2,
            periodic_at_send: false,
            ..timer(1, 1)
        };
        assert_eq!(
            lists.append_timer(shared(), USR1, refreshed, None),
            TimerAppend::AlreadyPending
        );
        assert_eq!(lists.entries(shared(), USR1), vec![Entry::Timer(refreshed)]);
        assert!(lists.invariants_hold());
    }

    #[test]
    fn a_timer_onto_a_discarded_copy_is_a_proxy_front() {
        let mut lists = SignalLists::default();
        assert_eq!(
            lists.append_timer(shared(), USR1, timer(1, 1), Some(false)),
            TimerAppend::ProxyFront
        );
        let (_, class, physical_si_timer) = lists.front(shared(), USR1).unwrap();
        assert_eq!(class, FrontClass::Proxy);
        assert!(!physical_si_timer);
    }

    #[test]
    fn a_forged_si_timer_send_is_marked_forged() {
        let mut lists = SignalLists::default();
        let mut info = kill_info();
        info.0[8..12].copy_from_slice(&SI_TIMER.to_ne_bytes());
        assert!(lists.append_plain(shared(), USR1, info, None, true));
        assert!(matches!(
            lists.entries(shared(), USR1)[0],
            Entry::Plain {
                forged_si_timer: true,
                ..
            }
        ));
        assert!(lists.front(shared(), USR1).unwrap().2);
    }

    #[test]
    fn deliverable_sees_a_valid_timer_behind_a_stale_head() {
        let mut lists = SignalLists::default();
        lists.append_timer(shared(), USR1, timer(1, 1), None);
        lists.append_timer(shared(), USR1, timer(2, 1), None);
        let gens = |_: DetPid, id: i32| if id == 1 { 2 } else { 1 };
        assert!(lists.deliverable(shared(), USR1, gens));
        let all_stale = |_: DetPid, _: i32| 9;
        assert!(!lists.deliverable(shared(), USR1, all_stale));
    }

    #[test]
    fn a_thread_queue_and_the_shared_queue_are_separate_lists() {
        let mut lists = SignalLists::default();
        let private = QueueId::Thread(DetTid::from_raw(11));
        lists.append_timer(private, USR1, timer(1, 1), None);
        lists.append_timer(shared(), USR1, timer(2, 1), None);
        assert_eq!(lists.listed().len(), 2);
        assert_eq!(lists.timer_entry(P, 1).map(|(q, ..)| q), Some(private));
        assert!(lists.invariants_hold());
    }

    #[test]
    fn a_signalfd_record_lays_out_each_kind_as_signalfd_copyinfo_does() {
        let timer = signalfd_record(&timer_siginfo(USR1, 3, 4, 0x1122334455667788));
        assert_eq!(i32::from_ne_bytes(timer[8..12].try_into().unwrap()), SI_TIMER);
        assert_eq!(u32::from_ne_bytes(timer[24..28].try_into().unwrap()), 3);
        assert_eq!(u32::from_ne_bytes(timer[32..36].try_into().unwrap()), 4);
        assert_eq!(i32::from_ne_bytes(timer[44..48].try_into().unwrap()), 0x55667788);
        assert_eq!(
            u64::from_ne_bytes(timer[48..56].try_into().unwrap()),
            0x1122334455667788
        );
        assert!(timer[12..24].iter().all(|&byte| byte == 0));
        let queued = signalfd_record(&queue_info());
        assert_eq!(
            u64::from_ne_bytes(queued[48..56].try_into().unwrap()),
            0x1122334455667788
        );
        let killed = signalfd_record(&ordinary_siginfo(USR1, 0, 77, 1000));
        assert_eq!(u32::from_ne_bytes(killed[12..16].try_into().unwrap()), 77);
        assert_eq!(u32::from_ne_bytes(killed[16..20].try_into().unwrap()), 1000);
        assert!(killed[20..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn the_timer_siginfo_matches_the_kernel_layout() {
        let info = timer_siginfo(USR1, 3, 4, 0x1122334455667788);
        assert_eq!(siginfo_code(&info), SI_TIMER);
        let info = info.0;
        assert_eq!(i32::from_ne_bytes(info[0..4].try_into().unwrap()), USR1);
        assert_eq!(i32::from_ne_bytes(info[16..20].try_into().unwrap()), 3);
        assert_eq!(i32::from_ne_bytes(info[20..24].try_into().unwrap()), 4);
        assert_eq!(
            u64::from_ne_bytes(info[24..32].try_into().unwrap()),
            0x1122334455667788
        );
        assert!(info[32..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn exec_resends_a_proxy_plain_whose_si_timer_record_the_kernel_flushed() {
        // A forged SI_TIMER copy was discarded; the kept SI_QUEUE send stands
        // on it, in a timer pair.
        let mut lists = SignalLists::default();
        assert!(lists.append_plain(private(), USR1, queue_info(), Some(true), true));
        let outcome = lists.exec(&[private(), shared()]);
        assert_eq!(outcome.resend, vec![(private(), USR1, queue_info())]);
        assert!(outcome.take_back.is_empty());
        let (front, class, physical_si_timer) = lists.front(private(), USR1).unwrap();
        // The stored 48 bytes, SI_QUEUE and the high half of sival_ptr.
        assert!(matches!(front, Entry::Plain { siginfo, .. } if siginfo == queue_info()));
        assert_eq!(
            class,
            FrontClass::Proxy,
            "the stop and take rewrite still apply"
        );
        assert!(
            !physical_si_timer,
            "the physical record is now Hermit's re-send"
        );
        assert!(lists.invariants_hold());
    }

    #[test]
    fn exec_pops_a_timer_free_plain_over_a_flushed_record() {
        let mut lists = SignalLists::default();
        assert!(lists.append_plain(private(), USR1, queue_info(), Some(true), false));
        assert_eq!(lists.front(private(), USR1).unwrap().1, FrontClass::Own);
        let outcome = lists.exec(&[private(), shared()]);
        assert_eq!(outcome, ExecOutcome::default());
        assert!(!lists.is_listed(private(), USR1));
    }

    #[test]
    fn exec_removes_a_forged_plain_in_either_kind_of_pair() {
        for timer_pair in [false, true] {
            let mut lists = SignalLists::default();
            let mut forged = kill_info();
            forged.0[8..12].copy_from_slice(&SI_TIMER.to_ne_bytes());
            assert!(lists.append_plain(shared(), USR1, forged, None, timer_pair));
            let outcome = lists.exec(&[private(), shared()]);
            assert_eq!(outcome, ExecOutcome::default());
            assert!(!lists.is_listed(shared(), USR1));
        }
    }

    #[test]
    fn exec_pops_a_plain_appended_before_the_first_timer() {
        // N10-2: the Plain was appended while the pair had no timer history,
        // so it is Own; a timer created later does not reclassify it.
        let mut lists = SignalLists::default();
        assert!(lists.append_plain(shared(), USR1, queue_info(), Some(true), false));
        let outcome = lists.exec(&[shared()]);
        assert!(outcome.resend.is_empty());
        assert!(!lists.is_listed(shared(), USR1));
    }

    #[test]
    fn exec_takes_back_a_kept_record_whose_timer_entry_it_removed() {
        // A timer's entry stood, as a Proxy, on a discarded SI_USER copy.
        let mut lists = SignalLists::default();
        lists.append_timer(shared(), USR1, timer(1, 1), Some(false));
        let outcome = lists.exec(&[shared()]);
        assert_eq!(outcome.take_back, vec![(shared(), USR1)]);
        assert!(!lists.is_listed(shared(), USR1));
    }

    #[test]
    fn exec_leaves_nothing_of_an_own_timer_entry() {
        let mut lists = SignalLists::default();
        lists.append_timer(shared(), USR1, timer(1, 1), None);
        lists.append_timer(shared(), USR1, timer(2, 1), None);
        assert_eq!(lists.exec(&[shared()]), ExecOutcome::default());
        assert!(lists.listed().is_empty());
    }

    #[test]
    fn exec_keeps_a_plain_front_and_drops_the_timers_behind_it() {
        let mut lists = SignalLists::default();
        assert!(lists.append_plain(shared(), USR1, kill_info(), None, true));
        lists.append_timer(shared(), USR1, timer(1, 1), None);
        assert_eq!(lists.exec(&[shared()]), ExecOutcome::default());
        assert_eq!(lists.entries(shared(), USR1).len(), 1);
        assert!(lists.invariants_hold());
    }

    #[test]
    fn exec_touches_only_the_named_queues() {
        let mut lists = SignalLists::default();
        let sibling = QueueId::Thread(DetTid::from_raw(12));
        lists.append_timer(sibling, USR1, timer(1, 1), None);
        assert_eq!(lists.exec(&[private(), shared()]), ExecOutcome::default());
        assert!(lists.is_listed(sibling, USR1));
    }

    #[test]
    fn rekey_moves_private_lists_and_drops_the_old_leaders() {
        let mut lists = SignalLists::default();
        let (from, to) = (QueueId::Thread(DetTid::from_raw(12)), private());
        lists.append_timer(to, USR1, timer(1, 1), None);
        assert!(lists.append_plain(from, USR1, queue_info(), Some(true), true));
        lists.rekey_thread(from, to);
        assert!(!lists.is_listed(from, USR1));
        assert_eq!(lists.front(to, USR1).unwrap().1, FrontClass::Proxy);
        assert!(lists.timer_entry(P, 1).is_none());
    }

    #[test]
    fn retire_drops_the_lists_and_frees_their_timers() {
        let mut lists = SignalLists::default();
        lists.append_timer(private(), USR1, timer(1, 1), None);
        let dropped = lists.retire(private());
        assert_eq!(dropped, vec![Entry::Timer(timer(1, 1))]);
        assert!(lists.timer_entry(P, 1).is_none());
        // Its next expiry is not ALREADY_PENDING.
        assert!(!lists.refresh_timer(timer(1, 2)));
    }
}
