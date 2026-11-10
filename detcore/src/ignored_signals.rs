/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1d, design D37 (post-facto-human-review trigger 3): the ignored
//! signals the in-guest runtime's mask holds pending, under the in-guest
//! delivery gate (`Config::in_guest_signal_delivery`; dev-hermit
//! ai_docs/2026-10-16-a5-signal-delivery-design.md, "3c.1 implementation plan,
//! revision 4", D37 and IR-7, <https://github.com/rrnewton/hermit/issues/4485>).
//!
//! Linux discards an ignored signal when it is sent unless it is blocked
//! (`sig_ignored`, kernel/signal.c:104-120 at v6.12: "Blocked signals are never
//! ignored"), and ignored means `SIG_IGN`, or `SIG_DFL` for a signal whose
//! default is to ignore it (`sig_handler_ignored`, :75-80;
//! `SIG_KERNEL_IGNORE_MASK`, include/linux/signal.h:434-436: SIGCONT,
//! SIGCHLD, SIGWINCH, SIGURG). One that is blocked is queued, and
//! `get_signal` discards it once a mask lets it be dequeued (:2815, :2830).
//! Once the in-guest runtime runs under a mask of its own (step 3c.1e), every
//! signal sent to a guest thread is blocked when it arrives, so ignored ones
//! that Linux discarded at send are queued too. This module names which of
//! them Linux would not hold at a given point, so Detcore can remove them
//! and keep them out of `/proc`:
//!
//! - `I` = the pending signals that are ignored, under the actions in force;
//! - `M_req` = the guest mask in force when an instance was queued;
//! - `M_next` = the mask that governs next;
//! - remove `I ∖ (M_req ∩ M_next)`: an instance outside `M_req` was discarded
//!   by Linux when it was sent, and one outside `M_next` is discarded by Linux
//!   when that mask lets it be dequeued (at the return, or at the transparent
//!   restart of `pselect6`, `ppoll` and `rt_sigsuspend`, which return
//!   `-ERESTARTNOHAND` for it, kernel/signal.c:157-166 counting it as pending
//!   whatever its action, F21).
//!
//! The one place where Linux shows the guest something for such a signal is
//! a masked `epoll_pwait` that would wait: `ep_poll` returns `-EINTR` for any
//! pending signal the temporary mask leaves unblocked (fs/eventpoll.c:
//! 2017-2018), and keeps that mask until the signal is dequeued, and discarded
//! (:2495). That is [`epoll_interrupting`].

use crate::syscalls::KernelSignalState;
use crate::syscalls::ignored_at_generation;

/// `I` of design D37: the signals pending for the thread (`SigPnd ∪ ShdPnd`)
/// whose action is ignored (`SigIgn`, or the default for SIGCHLD, SIGURG,
/// SIGWINCH or SIGCONT with no handler: `ignored_at_generation`), less
/// `reserved`, the in-guest runtime's reserved signals, whose action is the
/// runtime's and which a Detcore removal must never dequeue.
pub(crate) fn pending_ignored(state: &KernelSignalState, reserved: u64) -> u64 {
    state.pending & ignored_at_generation(state.ignored, state.caught) & !reserved
}

/// D37's removal at a point whose instances were queued under `m_req` and
/// which `m_next` governs next: `I ∖ (M_req ∩ M_next)`.
pub(crate) fn removable(state: &KernelSignalState, m_req: u64, m_next: u64, reserved: u64) -> u64 {
    pending_ignored(state, reserved) & !(m_req & m_next)
}

/// D37's exception at the park of a masked `epoll_pwait` or `epoll_pwait2`
/// with a nonzero timeout and nothing ready at its first probe (F21, IR-3):
/// the ignored signals pending at the call's entry that the guest's own mask
/// `m_req` blocked and its temporary mask `m_temp` lets in, `I ∩ M_req ∖
/// M_temp`. Linux returns `-EINTR` for them at once and discards them as the
/// call returns; Detcore removes them and returns `EINTR`. The ignored ones
/// outside `m_req` were discarded by Linux when they were sent, so they end
/// nothing (they are in [`removable`] all the same).
pub(crate) fn epoll_interrupting(
    state: &KernelSignalState,
    m_req: u64,
    m_temp: u64,
    reserved: u64,
) -> u64 {
    pending_ignored(state, reserved) & m_req & !m_temp
}

/// The bits a `/proc` status under the gate leaves out of `SigPnd` and
/// `ShdPnd`: the pending ignored signals the thread's published guest mask
/// `m_req` does not block, `I ∖ M_req`, which Linux discarded when they were
/// sent (D37's visibility rule). `pending` is the field's own set; `sig_ign`
/// and `sig_cgt` are the status's `SigIgn` and `SigCgt`, the actions in force
/// (fs/proc/array.c:264-300 at v6.12 renders all five from one lock of the
/// thread's signal state). A reserved signal of the in-guest runtime is never
/// in it: its `SigCgt` bit is the runtime's handler.
pub(crate) fn hidden_from_status(pending: u64, sig_ign: u64, sig_cgt: u64, m_req: u64) -> u64 {
    pending & ignored_at_generation(sig_ign, sig_cgt) & !m_req
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bit(signal: i32) -> u64 {
        1_u64 << (signal - 1)
    }

    fn state(pending: u64, ignored: u64, caught: u64) -> KernelSignalState {
        KernelSignalState {
            pending,
            thread_pending: pending,
            shared_pending: 0,
            blocked: !0,
            ignored,
            caught,
        }
    }

    /// `I` is `SIG_IGN`, or the default of a default-ignored signal with no
    /// handler; a caught default-ignored signal, a default-fatal one and a
    /// reserved one are not in it. Mutation "default-ignored counted even
    /// when caught": the caught SIGCHLD row fails.
    #[test]
    fn pending_ignored_is_sig_ign_or_an_uncaught_default_ignored_signal() {
        let urg = bit(libc::SIGURG);
        let chld = bit(libc::SIGCHLD);
        let term = bit(libc::SIGTERM);
        let usr1 = bit(libc::SIGUSR1);
        let s = state(urg | chld | term | usr1, usr1, chld);
        assert_eq!(pending_ignored(&s, 0), urg | usr1);
        assert_eq!(pending_ignored(&s, usr1), urg);
    }

    /// `I ∖ (M_req ∩ M_next)`: kept only when both masks block it. Rows of
    /// design D37 and IR-7: an instance pending at entry under the own mask
    /// and a temporary mask that unblocks it (removed: the transparent
    /// restart discards it); one both block (kept); one only the temporary
    /// mask blocks, sent during the park (`M_req` is then the temporary
    /// mask, kept); one neither blocks (removed: discarded at send).
    /// Mutation "(i) using only `M_next`": the first row keeps it.
    #[test]
    fn removable_is_i_minus_the_intersection_of_the_two_masks() {
        let urg = bit(libc::SIGURG);
        let s = state(urg, 0, 0);
        assert_eq!(
            removable(&s, urg, 0, 0),
            urg,
            "own blocks, temporary lets in"
        );
        assert_eq!(removable(&s, urg, urg, 0), 0, "both block");
        assert_eq!(removable(&s, 0, urg, 0), urg, "the own mask lets it in");
        assert_eq!(removable(&s, 0, 0, 0), urg, "neither blocks");
    }

    /// The `epoll` exception: only an ignored instance the own mask blocks
    /// and the temporary mask lets in. Mutation "every removable instance
    /// interrupts": the own-mask-unblocked row returns EINTR where Linux,
    /// which discarded it at send, waits.
    #[test]
    fn the_epoll_exception_is_i_within_the_own_mask_outside_the_temporary_one() {
        let urg = bit(libc::SIGURG);
        let s = state(urg, 0, 0);
        assert_eq!(epoll_interrupting(&s, urg, 0, 0), urg);
        assert_eq!(epoll_interrupting(&s, 0, 0, 0), 0);
        assert_eq!(epoll_interrupting(&s, urg, urg, 0), 0);
    }

    /// The status subtraction leaves a blocked ignored signal and every
    /// signal that is not ignored. Mutation "the subtraction ignores
    /// `M_req`": the blocked SIGURG row disappears.
    #[test]
    fn the_status_hides_only_ignored_signals_the_published_mask_lets_in() {
        let urg = bit(libc::SIGURG);
        let term = bit(libc::SIGTERM);
        assert_eq!(hidden_from_status(urg | term, 0, 0, 0), urg);
        assert_eq!(hidden_from_status(urg | term, 0, 0, urg), 0);
        assert_eq!(hidden_from_status(term, term, 0, 0), term);
        assert_eq!(hidden_from_status(urg, 0, urg, 0), 0);
    }
}
