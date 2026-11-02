/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1e, sub-slice A (Detcore groundwork; dev-hermit
//! ai_docs/2026-10-16-a5-signal-delivery-design.md, "3c.1e plan of record and
//! decisions D56-D68", and ai_docs/evidence/a5-3c1e-20261024/plan-3c1e-r4.md
//! section 1 A with the parts it carries from revisions 3 and 2;
//! <https://github.com/rrnewton/hermit/issues/4485>): what the scheduler
//! holds of a guest thread's mask under the in-guest delivery gate
//! (`Config::in_guest_signal_delivery`), beyond the mask each request
//! publishes (step 3c.1b, `Scheduler::record_request_guest_mask`):
//!
//! - IR-18's tri-state: a published mask is [`PublishedGuestMask::Known`],
//!   or [`PublishedGuestMask::Unknown`] after a rejoin that returned under
//!   the guest's own mask (`ContinuationMask::Restored`) but published none,
//!   on a backend that keeps the guest's mask apart for that thread, so that
//!   no reader shows the call's earlier, temporary mask as the one in force
//!   (`Scheduler::keep_mask_for_continuation`); on a backend that keeps none
//!   (every backend before step 3c.1e B) nothing is published then, and
//!   readers take `SigBlk`, which is the guest's own mask. The `/proc` status
//!   reader refuses by name on `Unknown` (`HERMIT_GUEST_MASK_UNKNOWN`); every
//!   scheduler-internal reader takes `Unknown` as nothing published (the
//!   kernel's `SigBlk`), as it takes a request that publishes nothing.
//! - Fork-child seeding: a new task starts with its creator's published own
//!   mask, never a masked call's temporary mask that the creator still
//!   publishes, as Linux's child starts with its creator's `blocked`
//!   (`arch_dup_task_struct`, kernel/fork.c:1086-1091 at v6.12, from
//!   `dup_task_struct`, :1112; `copy_process` empties only `pending`,
//!   :2268) ([`Scheduler::seed_child_guest_sigmask`]).
//! - IR-23(a): a masked wait that must make an RPC before its first probe (the
//!   publication of expired virtual timerfds, `timerfd::publish_expired`,
//!   while the process holds an armed virtual timerfd) first publishes its
//!   temporary mask, with the ignored signals it keeps pending from its entry
//!   in each queue (D37's held masks), through the non-yielding
//!   `GlobalRequest::PublishTemporaryMask`
//!   ([`Scheduler::publish_temporary_mask`]). Linux has installed that mask
//!   for the call's duration (`set_user_sigmask`, kernel/signal.c:3169 at
//!   v6.12), so a cross-process `/proc` reader during the RPC sees it. The
//!   next request the thread files supersedes it; a call that files none
//!   withdraws it as it returns ([`Scheduler::withdraw_temporary_mask`]),
//!   which puts back what was published before it.
//! - The committed-send window record for `HERMIT_IGNORED_REALTIME_INSTANCES`:
//!   while such a publication is in force, each send the scheduler commits to
//!   the thread, from its two current commit points (`NotifySignalPending`'s
//!   handler, which runs only after the guest's injected send succeeded, and
//!   `send_guest_signal` after its `kill` succeeded), is recorded with its
//!   queue: a thread-directed send in the private queue, a process-directed
//!   one in the shared queue ([`Scheduler::note_committed_send_in_window`]).
//!   The withdrawal answers it, so the call's end (D37's R2) can tell a
//!   real-time instance sent in the window from one kept from entry, which
//!   the pending bits cannot (plan revision 3, section 1 A).
//!
//! Every function here is a no-op with the gate off, and nothing sets the
//! gate before step 3c.1f.

use serde::Deserialize;
use serde::Serialize;

use super::Scheduler;
use crate::types::DetTid;

/// A guest mask the scheduler holds for a thread under the in-guest delivery
/// gate (IR-18's tri-state; absent means nothing published, and readers take
/// the kernel's `SigBlk`). It travels in a bincode RPC, so it has no
/// `skip_serializing_if`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishedGuestMask {
    /// The mask the thread published.
    Known(u64),
    /// The thread published none where its mask changed: a rejoin that
    /// returned under the guest's own mask, on a backend that otherwise keeps
    /// the guest's mask apart for that thread. A reader must not take the
    /// kernel's `SigBlk` for it, which after step 3c.1e B is the in-guest
    /// runtime's mask.
    Unknown,
}

/// The answer to a `/proc` status reader's `GlobalRequest::PublishedGuestSigmask`
/// under the in-guest delivery gate (an appended response variant,
/// `GlobalResponse::GuestSigmaskForStatus`): the published mask, tri-state,
/// and the ignored signals a temporary-mask publication keeps pending from
/// the call's entry in each queue (D37's held masks; zero without one).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusGuestMask {
    /// What the thread published, if anything.
    pub mask: Option<PublishedGuestMask>,
    /// `SigPnd_entry ∩ I ∩ own`: ignored signals kept pending in the private
    /// queue from the masked call's entry.
    pub held_private: u64,
    /// `ShdPnd_entry ∩ I ∩ own`: the same in the shared queue.
    pub held_shared: u64,
}

/// The payload of `GlobalRequest::PublishTemporaryMask` (IR-23(a)): a
/// publication, filed once per masked call before the first expired-timerfd
/// publication, or its withdrawal as a call that filed no later request
/// returns. It travels in a bincode RPC, so it has no
/// `skip_serializing_if`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemporaryMaskControl {
    /// `thread` waits under `mask` (as Linux installs it, without SIGKILL
    /// and SIGSTOP), keeping the ignored signals `held_private` and
    /// `held_shared` pending from its entry.
    Publish {
        /// The thread, the sender itself.
        thread: DetTid,
        /// The temporary mask.
        mask: u64,
        /// `SigPnd_entry ∩ I ∩ own`.
        held_private: u64,
        /// `ShdPnd_entry ∩ I ∩ own`.
        held_shared: u64,
    },
    /// `thread`'s call returned with its publication still in force: answer
    /// and close the committed-send window, and put back what was published
    /// before it, unless `keep_published`.
    Withdraw {
        /// The thread, the sender itself.
        thread: DetTid,
        /// The call ended interrupted (`-ERESTARTNOHAND` or `-EINTR`): Linux
        /// does not restore the own mask at the return
        /// (`restore_saved_sigmask_unless`, fs/select.c:301,
        /// fs/eventpoll.c:2495 and :2547 at v6.12); the delivery then sets
        /// `blocked` to the temporary mask with the handler's `sa_mask` and
        /// the signal (`signal_delivered`, kernel/signal.c:2951-2954), and
        /// the temporary mask leaves `blocked` only at `rt_sigreturn`, which
        /// restores the own mask saved in the frame. So the temporary mask
        /// stays published until the thread's next request; step 3c.1e B/C's
        /// in-guest delivery must file one before the handler's code runs,
        /// so that the published mask is the handler's.
        keep_published: bool,
    },
}

/// The sends the scheduler committed to a thread while its temporary-mask
/// publication was in force, by queue, as signal bits: the answer to a
/// withdrawal. It travels in a bincode RPC, so it has no
/// `skip_serializing_if`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedSendWindow {
    /// Thread-directed sends (the private queue).
    pub sent_private: u64,
    /// Process-directed sends (the shared queue).
    pub sent_shared: u64,
}

/// One temporary-mask publication in force (`Scheduler::temporary_mask_publications`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TemporaryMaskPublication {
    /// What the thread had published before it, put back by a withdrawal.
    prior: Option<PublishedGuestMask>,
    /// Whether that was a temporary mask (`Scheduler::temporary_guest_sigmasks`).
    prior_temporary: bool,
    /// D37's held masks.
    held_private: u64,
    held_shared: u64,
    /// The committed-send window record.
    window: CommittedSendWindow,
}

impl Scheduler {
    /// What the scheduler holds as `dettid`'s published guest mask, tri-state
    /// (IR-18): `Known`, `Unknown`, or `None` for nothing published.
    pub(crate) fn published_guest_mask_state(&self, dettid: DetTid) -> Option<PublishedGuestMask> {
        match self.saved_guest_sigmasks.get(&dettid) {
            Some(mask) => Some(PublishedGuestMask::Known(*mask)),
            None if self.unknown_guest_sigmasks.contains(&dettid) => {
                Some(PublishedGuestMask::Unknown)
            }
            None => None,
        }
    }

    /// The answer to a `/proc` status reader under the gate
    /// ([`StatusGuestMask`]).
    pub(crate) fn status_guest_mask(&self, dettid: DetTid) -> StatusGuestMask {
        let held = self.temporary_mask_publications.get(&dettid);
        StatusGuestMask {
            mask: self.published_guest_mask_state(dettid),
            held_private: held.map_or(0, |held| held.held_private),
            held_shared: held.map_or(0, |held| held.held_shared),
        }
    }

    /// Make `dettid`'s published mask `mask` (`None`: nothing), whatever it
    /// was, `Unknown` included.
    pub(super) fn put_guest_mask_state(
        &mut self,
        dettid: DetTid,
        mask: Option<PublishedGuestMask>,
    ) {
        match mask {
            Some(PublishedGuestMask::Known(mask)) => {
                self.saved_guest_sigmasks.insert(dettid, mask);
                self.unknown_guest_sigmasks.remove(&dettid);
            }
            Some(PublishedGuestMask::Unknown) => {
                self.saved_guest_sigmasks.remove(&dettid);
                self.unknown_guest_sigmasks.insert(dettid);
            }
            None => {
                self.saved_guest_sigmasks.remove(&dettid);
                self.unknown_guest_sigmasks.remove(&dettid);
            }
        }
    }

    /// [`Self::put_guest_mask_state`], for tests outside the scheduler.
    #[cfg(test)]
    pub(crate) fn put_guest_mask_state_for_test(
        &mut self,
        dettid: DetTid,
        mask: Option<PublishedGuestMask>,
    ) {
        self.put_guest_mask_state(dettid, mask);
    }

    /// IR-18: `dettid` is readmitted on a rejoin that returned under the
    /// guest's own mask (`ContinuationMask::Restored`: no temporary mask, and
    /// not unknown) but published none. The mask its call published, possibly
    /// a temporary one, no longer holds. Where the backend keeps the guest's
    /// mask apart from the kernel's for this thread (it has published its own
    /// mask at a request, `own_mask_publishers`), the scheduler does not know
    /// the mask Linux restored: `Unknown` (the `/proc` status reader refuses,
    /// defect-only once step 3c.1e B publishes at every request). Otherwise,
    /// as on every backend before B (none implements
    /// `Guest::resume_signal_mask`), the kernel's `SigBlk` is the guest's own
    /// mask: nothing is published, so readers take it, as for any request
    /// that publishes nothing. A failed vfork's rejoin (`VforkFailed`) takes
    /// the same rule. Under the gate only.
    pub(super) fn restored_rejoin_published_nothing(&mut self, dettid: DetTid) {
        if !self.in_guest_signal_delivery || !self.next_turns.contains_key(&dettid) {
            return;
        }
        self.temporary_guest_sigmasks.remove(&dettid);
        let mask = self.unknown_unless_kernel_shows_own(dettid);
        self.put_guest_mask_state(dettid, mask);
    }

    /// What to publish for `dettid` when the scheduler does not hold the
    /// guest's own mask for it: `Unknown` where its backend keeps the guest's
    /// mask apart from the kernel's (it has published its own mask at a
    /// request, `own_mask_publishers`), and otherwise nothing, so readers
    /// take the kernel's `SigBlk`, which is then the guest's own mask.
    fn unknown_unless_kernel_shows_own(&self, dettid: DetTid) -> Option<PublishedGuestMask> {
        self.own_mask_publishers
            .contains(&dettid)
            .then_some(PublishedGuestMask::Unknown)
    }

    /// Fork-child seeding, under the gate only: `child`, just registered by
    /// `parent`'s `clone`, `fork` or `vfork` in `parent`'s turn, starts with
    /// the guest's own mask as `parent` published it, `Known` or `Unknown`
    /// (nothing when `parent` published nothing, so `child`'s readers take
    /// its `SigBlk`, which the kernel copied from `parent`'s). Linux's child
    /// starts with its creator's `blocked` (`arch_dup_task_struct`,
    /// kernel/fork.c:1086-1091 at v6.12, from `dup_task_struct`, :1112) and
    /// an empty `pending` (:2268); its first request then publishes its own.
    /// A temporary mask `parent` still publishes
    /// (`Scheduler::temporary_guest_sigmasks`: a masked call's, which Linux
    /// takes down before the call returns, or, after an interrupted end,
    /// keeps through the signal's delivery together with the handler's
    /// `sa_mask` and the signal, `signal_delivered`, kernel/signal.c:2951-2954,
    /// until `rt_sigreturn`) is not the mask `parent` holds at the `fork`, and
    /// is not copied: `child` takes what a rejoin that publishes nothing takes
    /// ([`Self::unknown_unless_kernel_shows_own`]). An ordinary clone first
    /// publishes the creator's current mask (`tool_global::create_child_thread`),
    /// which clears the mark, so this applies only where that publishes
    /// nothing.
    pub(crate) fn seed_child_guest_sigmask(&mut self, parent: DetTid, child: DetTid) {
        if !self.in_guest_signal_delivery || parent == child {
            return;
        }
        let inherited = if self.temporary_guest_sigmasks.contains(&parent) {
            self.unknown_unless_kernel_shows_own(parent)
        } else {
            self.published_guest_mask_state(parent)
        };
        self.temporary_mask_publications.remove(&child);
        self.temporary_guest_sigmasks.remove(&child);
        self.put_guest_mask_state(child, inherited);
    }

    /// IR-23(a), the answer to `GlobalRequest::PublishTemporaryMask`, in the
    /// sender's own turn, with no scheduling decision. Under the gate only;
    /// `None` with it off or for a thread the scheduler does not know, and
    /// for a withdrawal with no publication in force (a later request
    /// superseded it).
    pub(crate) fn temporary_mask_control(
        &mut self,
        sender: DetTid,
        control: TemporaryMaskControl,
    ) -> Option<CommittedSendWindow> {
        match control {
            TemporaryMaskControl::Publish {
                thread,
                mask,
                held_private,
                held_shared,
            } if thread == sender => {
                self.publish_temporary_mask(thread, mask, held_private, held_shared);
                None
            }
            TemporaryMaskControl::Withdraw {
                thread,
                keep_published,
            } if thread == sender => self.withdraw_temporary_mask(thread, keep_published),
            _ => None,
        }
    }

    /// `thread` waits under the temporary mask `mask` from now, keeping the
    /// ignored signals `held_private` and `held_shared` pending from its
    /// entry: the published mask becomes `mask` (readers, the `/proc` status
    /// reader included, take it in place of what the thread's latest request
    /// published), the held masks are kept for the status reader, and the
    /// committed-send window opens. Under the gate only. Returns whether the
    /// publication was taken.
    pub(crate) fn publish_temporary_mask(
        &mut self,
        thread: DetTid,
        mask: u64,
        held_private: u64,
        held_shared: u64,
    ) -> bool {
        if !self.in_guest_signal_delivery || !self.next_turns.contains_key(&thread) {
            return false;
        }
        let (prior, prior_temporary) = match self.temporary_mask_publications.get(&thread) {
            // A second publication in the same call keeps the first one's
            // prior (the call files the pair once, so this does not happen).
            Some(publication) => (publication.prior, publication.prior_temporary),
            None => (
                self.published_guest_mask_state(thread),
                self.temporary_guest_sigmasks.contains(&thread),
            ),
        };
        self.put_guest_mask_state(thread, Some(PublishedGuestMask::Known(mask)));
        self.temporary_guest_sigmasks.insert(thread);
        self.temporary_mask_publications.insert(
            thread,
            TemporaryMaskPublication {
                prior,
                prior_temporary,
                held_private,
                held_shared,
                window: CommittedSendWindow::default(),
            },
        );
        true
    }

    /// `thread`'s masked call returned with its temporary-mask publication
    /// still in force (it filed no request since): the committed-send window
    /// is answered and closed, with the held masks, and what was published
    /// before it is put back, as Linux restores the thread's own mask as the
    /// call returns (`restore_saved_sigmask_unless`, fs/select.c:301,
    /// fs/eventpoll.c:2495 and :2547 at v6.12). On an interrupted end
    /// (`keep_published`) Linux does not restore it at the return, and the
    /// delivery keeps the temporary mask in `blocked` with the handler's
    /// `sa_mask` and the signal until `rt_sigreturn` (`signal_delivered`,
    /// kernel/signal.c:2951-2954), so it stays published until the thread's
    /// next request.
    /// `None` when no publication is in force.
    pub(crate) fn withdraw_temporary_mask(
        &mut self,
        thread: DetTid,
        keep_published: bool,
    ) -> Option<CommittedSendWindow> {
        let publication = self.temporary_mask_publications.remove(&thread)?;
        if !keep_published {
            self.put_guest_mask_state(thread, publication.prior);
            if !publication.prior_temporary {
                self.temporary_guest_sigmasks.remove(&thread);
            }
        }
        Some(publication.window)
    }

    /// A later publication by `thread` (a request it filed, or an explicit
    /// `SetSavedGuestSigmask`) supersedes its temporary-mask publication: the
    /// held masks and the window go with it, and nothing is put back.
    pub(super) fn forget_temporary_mask_publication(&mut self, thread: DetTid) {
        self.temporary_mask_publications.remove(&thread);
    }

    /// The committed-send window record: a send of `signal` the scheduler has
    /// just committed to `target`, thread-directed (`private`) or
    /// process-directed, from one of its two current commit points
    /// (`NotifySignalPending`'s handler, after the guest's injected send
    /// succeeded; `send_guest_signal`, after its `kill` succeeded). Recorded
    /// only while `target` holds a temporary-mask publication; nothing is
    /// kept, or even looked up, with the gate off.
    pub(crate) fn note_committed_send_in_window(
        &mut self,
        target: DetTid,
        signal: i32,
        private: bool,
    ) {
        if !self.in_guest_signal_delivery {
            return;
        }
        let Some(publication) = self.temporary_mask_publications.get_mut(&target) else {
            return;
        };
        let bit = super::kernel_signal_bit(signal);
        if private {
            publication.window.sent_private |= bit;
        } else {
            publication.window.sent_shared |= bit;
        }
    }
}
