/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore's own copy of each guest's signal dispositions and masks.
//!
//! Audit step A5 (ai_docs/2026-10-16-a5-signal-delivery-design.md in the
//! dev-hermit parent; <https://github.com/rrnewton/hermit/issues/4485>) moves
//! Linux signal policy into Detcore, so that no Detcore decision depends on
//! reading the host's `/proc/<pid>/task/<tid>/status` for a guest thread.
//! That read is the guest's real state only on the ptrace backend.
//!
//! This module is the state and its transitions, as Linux performs them. It
//! holds no guest handle and makes no syscall: callers apply a transition
//! once the guest operation it describes has committed. The state is kept,
//! seeded and compared with the kernel's only where the kernel's signal state
//! is the guest's own (ptrace, serialized threads), unless
//! `Config::signal_state_copy` is off (`--no-signal-state-copy`;
//! docs/SIGNAL_STATE_CHECK.md).
//! Step 1 kept it in shadow mode, read by no decision; since step 2 some
//! decisions read it in place of the kernel's `/proc` view, as that document
//! lists, and since step 2d it is on by default. Other backends need to
//! report a first image's inherited state first (design step 3).
//!
//! What is kept, and where Linux keeps the same thing:
//! - [`Dispositions`]: the 64 actions of one `struct sighand_struct`. Every
//!   task cloned with `CLONE_SIGHAND` shares one table, so a thread state
//!   holds it behind a shared reference, as it holds the descriptor table
//!   that `CLONE_FILES` shares. `CLONE_SIGHAND` requires `CLONE_VM`, so the
//!   tasks sharing a table also share the memory that reference lives in, on
//!   every backend.
//! - [`ThreadSignals`]: a task's `blocked` mask, the mask an interrupted
//!   temporary-mask wait puts back, and the masks its open handler frames
//!   will restore.
//!
//! Pending signals are not here: Detcore's existing queue owners keep them.

use std::sync::Arc;
use std::sync::Mutex;

use reverie::syscalls::Errno;
use serde::Deserialize;
use serde::Serialize;

/// A kernel signal set: bit `n - 1` is signal `n`, as `/proc` and
/// `rt_sigprocmask` use it.
pub(crate) type Sigset = u64;

/// The highest signal number on x86-64 Linux (`_NSIG`).
pub(crate) const MAX_SIGNAL: i32 = 64;

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

/// `sa_flags` bits the kernel keeps and reports back on x86-64: Linux's
/// `UAPI_SA_FLAGS` (`include/linux/signal_types.h`). `do_sigaction` clears
/// every other bit of a new action before storing it, so userspace can detect
/// unsupported flags, for example `SA_UNSUPPORTED`.
const UAPI_SA_FLAGS: u64 = sa_flag(libc::SA_NOCLDSTOP)
    | sa_flag(libc::SA_NOCLDWAIT)
    | sa_flag(libc::SA_SIGINFO)
    | sa_flag(libc::SA_ONSTACK)
    | sa_flag(libc::SA_RESTART)
    | sa_flag(libc::SA_NODEFER)
    | sa_flag(libc::SA_RESETHAND)
    | sa_flag(SA_EXPOSE_TAGBITS)
    | sa_flag(SA_RESTORER);

/// An `sa_flags` bit as the kernel's `unsigned long` holds it. The libc crate
/// declares the bits as `c_int`, so `SA_RESETHAND` (0x80000000) is negative
/// and a plain `as u64` would set the upper 32 bits as well.
const fn sa_flag(flag: libc::c_int) -> u64 {
    flag as u32 as u64
}

/// Not exported by the libc crate for x86-64.
const SA_EXPOSE_TAGBITS: libc::c_int = 0x0000_0800;

/// Not exported by the libc crate for x86-64 (`asm/signal.h`).
const SA_RESTORER: libc::c_int = 0x0400_0000;

/// The bit of `signal` in a [`Sigset`], or 0 outside `1..=64`.
pub(crate) fn sigbit(signal: i32) -> Sigset {
    if (1..=MAX_SIGNAL).contains(&signal) {
        1 << (signal - 1)
    } else {
        0
    }
}

/// `clone3`'s flag to reset caught signals in the child (Linux 5.5); not
/// exported by the libc crate.
const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;

/// The most open handler frames followed per thread. A handler left by
/// `siglongjmp`, which never returns, leaves its frame open, and a handler
/// whose return changes no mask can never be recognised as returned; beyond
/// this many, the copy is given up as unknown rather than guessing which frame
/// went.
pub(crate) const MAX_OPEN_FRAMES: usize = 32;

/// SIGKILL and SIGSTOP: never blocked, never caught, never ignored.
const UNBLOCKABLE: Sigset = (1 << (libc::SIGKILL - 1)) | (1 << (libc::SIGSTOP - 1));

/// One signal's action, as `struct kernel_sigaction` lays it out.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Action {
    /// `SIG_DFL` (0), `SIG_IGN` (1), or the handler's address.
    pub(crate) handler: u64,
    /// `sa_flags`, limited to [`UAPI_SA_FLAGS`].
    pub(crate) flags: u64,
    /// `sa_restorer`.
    pub(crate) restorer: u64,
    /// `sa_mask`, without SIGKILL and SIGSTOP.
    pub(crate) mask: Sigset,
}

impl Action {
    /// The action as `do_sigaction` stores it.
    pub(crate) fn normalized(self) -> Self {
        Action {
            flags: self.flags & UAPI_SA_FLAGS,
            mask: self.mask & !UNBLOCKABLE,
            ..self
        }
    }

    pub(crate) fn is_caught(&self) -> bool {
        self.handler != SIG_DFL && self.handler != SIG_IGN
    }

    pub(crate) fn is_ignored(&self) -> bool {
        self.handler == SIG_IGN
    }
}

/// The actions of one disposition domain, indexed by signal number - 1.
/// Always 64 entries (a `Vec` only because serde does not derive arrays
/// that long).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Action>", into = "Vec<Action>")]
pub(crate) struct Dispositions {
    actions: Vec<Action>,
}

impl From<Dispositions> for Vec<Action> {
    fn from(dispositions: Dispositions) -> Self {
        dispositions.actions
    }
}

impl TryFrom<Vec<Action>> for Dispositions {
    type Error = String;

    fn try_from(actions: Vec<Action>) -> Result<Self, Self::Error> {
        if actions.len() == MAX_SIGNAL as usize {
            Ok(Dispositions { actions })
        } else {
            Err(format!(
                "a signal action table has {} entries, not {MAX_SIGNAL}",
                actions.len()
            ))
        }
    }
}

impl Dispositions {
    /// A freshly executed image's actions: after `execve` a signal is either
    /// ignored or at its default, so the ignored set alone describes them.
    pub(crate) fn after_exec(ignored: Sigset) -> Self {
        let mut actions = vec![Action::default(); MAX_SIGNAL as usize];
        for (index, action) in actions.iter_mut().enumerate() {
            if ignored & (1 << index) & !UNBLOCKABLE != 0 {
                action.handler = SIG_IGN;
            }
        }
        Dispositions { actions }
    }

    fn index(signal: i32) -> Result<usize, Errno> {
        if (1..=MAX_SIGNAL).contains(&signal) {
            Ok((signal - 1) as usize)
        } else {
            Err(Errno::EINVAL)
        }
    }

    /// `signal`'s current action.
    pub(crate) fn action(&self, signal: i32) -> Result<Action, Errno> {
        Ok(self.actions[Self::index(signal)?])
    }

    /// `rt_sigaction(signal, new, &old)` after the size check, which the
    /// caller makes first, as the kernel does. Returns the old action, which
    /// is what the kernel copies out. SIGKILL and SIGSTOP can be queried but
    /// not changed.
    pub(crate) fn sigaction(&mut self, signal: i32, new: Option<Action>) -> Result<Action, Errno> {
        let index = Self::index(signal)?;
        if new.is_some() && sigbit(signal) & UNBLOCKABLE != 0 {
            return Err(Errno::EINVAL);
        }
        let old = self.actions[index];
        if let Some(new) = new {
            self.actions[index] = new.normalized();
        }
        Ok(old)
    }

    /// The action used to deliver `signal`, chosen when the signal is
    /// dequeued for delivery. `SA_RESETHAND` puts the handler back to
    /// `SIG_DFL` at this point, before the frame is built, so a frame that
    /// cannot be written still leaves the default installed (Linux
    /// `get_signal`).
    pub(crate) fn select_for_delivery(&mut self, signal: i32) -> Result<Action, Errno> {
        let index = Self::index(signal)?;
        let action = self.actions[index];
        if action.is_caught() && action.flags & sa_flag(libc::SA_RESETHAND) != 0 {
            self.actions[index].handler = SIG_DFL;
        }
        Ok(action)
    }

    /// `execve`'s reset (Linux `flush_signal_handlers` with
    /// `force_default` false): a caught signal goes back to its default, an
    /// ignored one stays ignored, and every action loses its flags, restorer
    /// and mask.
    pub(crate) fn reset_for_exec(&mut self) {
        for action in &mut self.actions {
            let handler = if action.handler == SIG_IGN {
                SIG_IGN
            } else {
                SIG_DFL
            };
            *action = Action {
                handler,
                ..Action::default()
            };
        }
    }

    /// The signals whose action is `SIG_IGN`, as `/proc`'s `SigIgn`.
    pub(crate) fn ignored(&self) -> Sigset {
        self.select(|action| action.handler == SIG_IGN)
    }

    /// The signals with a handler installed, as `/proc`'s `SigCgt`.
    pub(crate) fn caught(&self) -> Sigset {
        self.select(Action::is_caught)
    }

    fn select(&self, keep: impl Fn(&Action) -> bool) -> Sigset {
        self.actions
            .iter()
            .enumerate()
            .filter(|(_, action)| keep(action))
            .fold(0, |set, (index, _)| set | (1 << index))
    }
}

/// One thread's masks.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ThreadSignals {
    /// The signals blocked now (Linux `task->blocked`).
    pub(crate) blocked: Sigset,
    /// The mask to put back once a temporary-mask wait (`rt_sigsuspend`, or
    /// `ppoll`, `pselect6` or `epoll_pwait` given a mask) that a signal
    /// interrupted returns to the guest: Linux's `saved_sigmask` while
    /// `TIF_RESTORE_SIGMASK` is set. A handler delivered first saves it in
    /// its frame instead.
    #[serde(default)]
    pub(crate) saved: Option<Sigset>,
    /// The temporary mask of the wait this thread is in (`rt_sigsuspend`, or
    /// `ppoll`, `pselect6`, `epoll_pwait` or `epoll_pwait2` given a mask), as
    /// Linux copied it at the call's entry. A signal that ends the wait finds
    /// either this mask installed, if the kernel ran the call, or the thread's
    /// own mask, if the call had not started.
    #[serde(default)]
    pub(crate) wait_mask: Option<Sigset>,
    /// With `wait_mask`, whether the kernel ran that wait, from the call's
    /// own raw result before any normalization: a not-run report
    /// (`-ERESTARTSYS`, the signal came first) means its mask was never
    /// installed. `None` where nothing recorded it.
    #[serde(default)]
    pub(crate) wait_ran: Option<bool>,
    /// The `uc_sigmask` of each handler frame entered and not yet known to
    /// have returned, innermost last. A backend that reports `rt_sigreturn`
    /// pops one per return ([`ThreadSignals::sigreturn`]); where the kernel
    /// runs `rt_sigreturn` unseen, a return is recognised when the kernel's
    /// mask equals one of these ([`ThreadSignals::reconcile_returns`]).
    #[serde(default)]
    pub(crate) activations: Vec<Sigset>,
    /// For each open frame, whether a check saw its handler's mask installed.
    /// A frame taken as returned before that happened leaves its entry
    /// unverified: the kernel's mask may have equalled the saved mask because
    /// the handler had returned, or because the entry was not what the copy
    /// predicted.
    #[serde(default)]
    pub(crate) observed: Vec<bool>,
    /// A backend report changed this thread's mask, or found the frame
    /// faulting, since the last check: the next syscall entry compares the
    /// copy with the kernel even with no handler frame open, so a report
    /// the kernel did not follow fails closed there rather than at a later
    /// check.
    #[serde(default)]
    pub(crate) verify_at_entry: bool,
}

impl ThreadSignals {
    /// A thread whose mask is `blocked`.
    pub(crate) fn new(blocked: Sigset) -> Self {
        ThreadSignals {
            blocked: blocked & !UNBLOCKABLE,
            saved: None,
            wait_mask: None,
            wait_ran: None,
            activations: Vec::new(),
            observed: Vec::new(),
            verify_at_entry: false,
        }
    }

    /// `rt_sigprocmask(how, set, &old)` after the size check. Returns the old
    /// mask, read before any change, which is what the kernel copies out.
    /// `how` is checked only when `set` is given.
    pub(crate) fn sigprocmask(&mut self, how: i32, set: Option<Sigset>) -> Result<Sigset, Errno> {
        let old = self.blocked;
        if let Some(set) = set {
            let set = set & !UNBLOCKABLE;
            self.blocked = match how {
                libc::SIG_BLOCK => old | set,
                libc::SIG_UNBLOCK => old & !set,
                libc::SIG_SETMASK => set,
                _ => return Err(Errno::EINVAL),
            };
        }
        Ok(old)
    }

    /// A temporary-mask wait that a signal interrupted returned with `mask`
    /// still installed, and the mask from before it to be put back.
    pub(crate) fn interrupted_under_temporary_mask(&mut self, mask: Sigset) {
        self.settle();
        self.saved = Some(self.blocked);
        self.blocked = mask & !UNBLOCKABLE;
    }

    /// The thread went on without a handler entering first: a pending
    /// temporary-mask restore has happened.
    pub(crate) fn settle(&mut self) {
        if let Some(saved) = self.saved.take() {
            self.blocked = saved;
        }
    }

    /// A handler for `signal` with `action` was entered, its frame written
    /// (Linux `signal_delivered`). The handler runs with `sa_mask` added and,
    /// without `SA_NODEFER`, `signal` itself. Its frame saves the mask
    /// `rt_sigreturn` restores: a pending temporary-mask restore's mask, which
    /// the frame now carries, else the current one (Linux `sigmask_to_save`).
    ///
    /// Returns false when the thread already has [`MAX_OPEN_FRAMES`] open
    /// frames: the copy can then no longer follow returns, and the caller must
    /// treat it as unknown.
    #[must_use]
    pub(crate) fn enter_handler(&mut self, signal: i32, action: &Action) -> bool {
        if self.activations.len() == MAX_OPEN_FRAMES {
            return false;
        }
        let frame = self.saved.take().unwrap_or(self.blocked);
        let mut blocked = self.blocked | action.mask;
        if action.flags & sa_flag(libc::SA_NODEFER) == 0 {
            blocked |= sigbit(signal);
        }
        self.blocked = blocked & !UNBLOCKABLE;
        self.activations.push(frame);
        self.observed.push(false);
        true
    }

    /// A check found this thread's current mask installed while handler
    /// frames are open: the innermost handler's mask is verified, unless it
    /// equals the mask that frame saves, in which case the check cannot tell
    /// the handler from its return and verifies nothing.
    pub(crate) fn observe_handler_mask(&mut self) {
        if self.activations.last() != Some(&self.blocked)
            && let Some(observed) = self.observed.last_mut()
        {
            *observed = true;
        }
    }

    /// Whether the innermost open frame's handler mask was ever verified.
    pub(crate) fn innermost_observed(&self) -> bool {
        self.observed.last().copied().unwrap_or(true)
    }

    /// `rt_sigreturn` read `mask` from the frame and installed it.
    pub(crate) fn sigreturn(&mut self, mask: Sigset) {
        self.activations.pop();
        self.observed.pop();
        self.saved = None;
        self.blocked = mask & !UNBLOCKABLE;
    }

    /// Accounts for handler returns the backend did not report, given the
    /// kernel's current mask `observed`. Returns false when `observed` is
    /// neither the copy's mask nor the saved mask of an open handler frame:
    /// the copy is then wrong, or a handler rewrote its frame's `uc_sigmask` to
    /// a mask no open frame saved, which Linux allows and the check reports as
    /// a mismatch. A match pops that frame and every frame inside it.
    ///
    /// Bits in `ignore` are left out of every comparison: signals whose
    /// kernel state the backend itself changes. A recognised return installs
    /// the frame's mask as the copy holds it, those bits included.
    ///
    /// The match is a mask equality, so an inner handler that rewrites its
    /// `uc_sigmask` to an outer frame's saved mask looks like that many
    /// returns; a caller that needs certainty treats a match deeper than the
    /// innermost frame as ambiguous. Only a backend report of each
    /// `rt_sigreturn` removes the ambiguity, and where the backend reports
    /// them the copy does not use this.
    pub(crate) fn reconcile_returns(&mut self, observed: Sigset, ignore: Sigset) -> bool {
        let same = |mask: Sigset| mask & !ignore == observed & !ignore;
        if same(self.blocked) {
            return true;
        }
        match self.activations.iter().rposition(|&mask| same(mask)) {
            Some(frame) => {
                self.blocked = self.activations[frame];
                self.activations.truncate(frame);
                self.observed.truncate(frame);
                true
            }
            None => false,
        }
    }
}

/// `signal` was selected for delivery and, for a caught signal, its handler
/// frame written: the shared `dispositions` and the receiving thread's
/// `thread` change as Linux changes them. Returns the action used. An
/// ignored or default action changes no mask; `SA_RESETHAND` has reset the
/// handler in either case.
///
/// `Ok(None)` when the handler frame is one more than the copy follows
/// ([`ThreadSignals::enter_handler`]).
pub(crate) fn deliver(
    dispositions: &mut Dispositions,
    thread: &mut ThreadSignals,
    signal: i32,
) -> Result<Option<Action>, Errno> {
    let action = dispositions.select_for_delivery(signal)?;
    if action.is_caught() {
        // A handler ends the wait; an ignored signal does not.
        thread.wait_mask = None;
        thread.wait_ran = None;
        if !thread.enter_handler(signal, &action) {
            return Ok(None);
        }
    }
    Ok(Some(action))
}

/// One thread's view of the signal state: its process's (more exactly, its
/// `CLONE_SIGHAND` group's) actions, shared with every thread of the group,
/// and its own masks.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SignalShadow {
    /// Shared by every task created with `CLONE_SIGHAND`. `None` once any
    /// task sharing it lost its copy: that task's later changes would no
    /// longer reach it, so it is unknown to every sharer.
    pub(crate) dispositions: Arc<Mutex<Option<Dispositions>>>,
    /// This thread's masks.
    pub(crate) thread: ThreadSignals,
}

impl SignalShadow {
    /// The first image's state, from the ignored set and mask it inherited.
    pub(crate) fn seeded(ignored: Sigset, blocked: Sigset) -> Self {
        SignalShadow {
            dispositions: Arc::new(Mutex::new(Some(Dispositions::after_exec(ignored)))),
            thread: ThreadSignals::new(blocked),
        }
    }

    /// The state of a task this thread creates with clone flags `flags`. The
    /// child starts with this thread's mask. With `CLONE_SIGHAND` it shares
    /// the actions; otherwise it gets a copy, as Linux's `copy_sighand` makes,
    /// with caught signals reset under `CLONE_CLEAR_SIGHAND`. `None` when the
    /// actions are unknown, and for any child created inside a handler, whose
    /// frames the copy cannot follow.
    pub(crate) fn for_clone(&self, flags: u64) -> Option<Self> {
        let dispositions = if flags & libc::CLONE_SIGHAND as u64 != 0 {
            self.dispositions.lock().unwrap().as_ref()?;
            Arc::clone(&self.dispositions)
        } else {
            let mut copy = self.dispositions.lock().unwrap().clone()?;
            if flags & CLONE_CLEAR_SIGHAND != 0 {
                copy.reset_for_exec();
            }
            Arc::new(Mutex::new(Some(copy)))
        };
        let mut thread = self.thread.clone();
        thread.settle();
        // A child created inside a handler may carry on in it and return
        // through its frame (fork, vfork, or a raw clone with no child stack),
        // or start on a stack of its own; the clone flags do not say which,
        // so it gets no copy.
        if !thread.activations.is_empty() {
            if flags & libc::CLONE_SIGHAND as u64 != 0 {
                // The untracked child shares these actions and could change
                // them unseen.
                self.forget_dispositions();
            }
            return None;
        }
        thread.activations.clear();
        thread.observed.clear();
        Some(SignalShadow {
            dispositions,
            thread,
        })
    }

    /// This thread completed an `execve`. Its actions become a private copy
    /// first (Linux `unshare_sighand`), so a task that shared them keeps its
    /// own, and are then reset. The mask is kept. Returns false when the
    /// actions were unknown.
    pub(crate) fn exec(&mut self) -> bool {
        let Some(mut own) = self.dispositions.lock().unwrap().clone() else {
            return false;
        };
        own.reset_for_exec();
        self.dispositions = Arc::new(Mutex::new(Some(own)));
        self.thread.settle();
        self.thread.activations.clear();
        self.thread.observed.clear();
        true
    }

    /// Marks the shared actions unknown, for this thread's copy being
    /// dropped.
    pub(crate) fn forget_dispositions(&self) {
        *self.dispositions.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(address: u64, flags: libc::c_int, mask: Sigset) -> Action {
        Action {
            handler: address,
            flags: sa_flag(flags | SA_RESTORER),
            restorer: 0x7000,
            mask,
        }
    }

    #[test]
    fn exec_image_starts_ignored_or_default_and_never_ignores_sigkill() {
        let ignored = sigbit(libc::SIGPIPE) | sigbit(libc::SIGKILL);
        let table = Dispositions::after_exec(ignored);
        assert_eq!(table.ignored(), sigbit(libc::SIGPIPE));
        assert_eq!(table.caught(), 0);
    }

    #[test]
    fn sigaction_returns_old_and_stores_normalized() {
        let mut table = Dispositions::after_exec(0);
        let new = Action {
            handler: 0x4000,
            flags: sa_flag(libc::SA_SIGINFO | SA_RESTORER) | 0x400 | (1 << 40),
            restorer: 0x7000,
            mask: sigbit(libc::SIGUSR2) | UNBLOCKABLE,
        };
        assert_eq!(
            table.sigaction(libc::SIGUSR1, Some(new)),
            Ok(Action::default())
        );
        let stored = table.action(libc::SIGUSR1).unwrap();
        assert_eq!(
            stored.flags,
            sa_flag(libc::SA_SIGINFO | SA_RESTORER),
            "SA_UNSUPPORTED (0x400) and bits above 31 are not kept"
        );
        assert_eq!(stored.mask, sigbit(libc::SIGUSR2));
        assert_eq!(table.sigaction(libc::SIGUSR1, None), Ok(stored));
        assert_eq!(table.caught(), sigbit(libc::SIGUSR1));
    }

    #[test]
    fn sigaction_refuses_sigkill_changes_and_out_of_range_signals() {
        let mut table = Dispositions::after_exec(0);
        let new = handler(0x4000, 0, 0);
        assert_eq!(
            table.sigaction(libc::SIGKILL, Some(new)),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            table.sigaction(libc::SIGSTOP, Some(new)),
            Err(Errno::EINVAL)
        );
        assert_eq!(table.sigaction(libc::SIGKILL, None), Ok(Action::default()));
        assert_eq!(table.sigaction(0, None), Err(Errno::EINVAL));
        assert_eq!(table.sigaction(65, None), Err(Errno::EINVAL));
        assert_eq!(table.sigaction(64, None), Ok(Action::default()));
    }

    #[test]
    fn exec_reset_keeps_ignored_and_clears_everything_else() {
        let mut table = Dispositions::after_exec(0);
        table
            .sigaction(libc::SIGUSR1, Some(handler(0x4000, libc::SA_RESTART, 3)))
            .unwrap();
        table
            .sigaction(
                libc::SIGPIPE,
                Some(Action {
                    handler: SIG_IGN,
                    flags: sa_flag(libc::SA_RESTART),
                    restorer: 0x7000,
                    mask: 3,
                }),
            )
            .unwrap();
        table.reset_for_exec();
        assert_eq!(table.action(libc::SIGUSR1), Ok(Action::default()));
        assert_eq!(
            table.action(libc::SIGPIPE),
            Ok(Action {
                handler: SIG_IGN,
                ..Action::default()
            })
        );
    }

    #[test]
    fn resethand_resets_at_selection_and_keeps_flags() {
        let mut table = Dispositions::after_exec(0);
        let action = handler(0x4000, libc::SA_RESETHAND, 0);
        table.sigaction(libc::SIGUSR1, Some(action)).unwrap();
        assert_eq!(
            table.select_for_delivery(libc::SIGUSR1),
            Ok(action.normalized())
        );
        let after = table.action(libc::SIGUSR1).unwrap();
        assert_eq!(after.handler, SIG_DFL);
        assert_eq!(after.flags, action.normalized().flags);
    }

    #[test]
    fn sign_extended_flags_from_glibc_keep_only_the_kernel_bits() {
        // glibc widens its int sa_flags into the kernel's unsigned long, so a
        // guest's SA_RESETHAND arrives as 0xffffffff80000000.
        let widened = libc::SA_RESETHAND as i64 as u64;
        let mut table = Dispositions::after_exec(0);
        let new = Action {
            flags: widened | sa_flag(SA_RESTORER),
            ..handler(0x4000, 0, 0)
        };
        table.sigaction(libc::SIGUSR1, Some(new)).unwrap();
        assert_eq!(
            table.action(libc::SIGUSR1).unwrap().flags,
            0x8000_0000 | sa_flag(SA_RESTORER)
        );
    }

    #[test]
    fn sigprocmask_returns_the_old_mask_and_never_blocks_sigkill() {
        let mut thread = ThreadSignals::new(sigbit(libc::SIGUSR1));
        assert_eq!(
            thread.sigprocmask(libc::SIG_BLOCK, Some(sigbit(libc::SIGKILL) | 4)),
            Ok(sigbit(libc::SIGUSR1))
        );
        assert_eq!(thread.blocked, sigbit(libc::SIGUSR1) | 4);
        assert_eq!(
            thread.sigprocmask(libc::SIG_UNBLOCK, Some(4)),
            Ok(sigbit(libc::SIGUSR1) | 4)
        );
        assert_eq!(
            thread.sigprocmask(libc::SIG_SETMASK, Some(8)),
            Ok(sigbit(libc::SIGUSR1))
        );
        assert_eq!(thread.blocked, 8);
        assert_eq!(thread.sigprocmask(99, Some(1)), Err(Errno::EINVAL));
        assert_eq!(thread.blocked, 8, "a refused call changes nothing");
        assert_eq!(
            thread.sigprocmask(99, None),
            Ok(8),
            "how is ignored without a set"
        );
    }

    #[test]
    fn handler_entry_adds_sa_mask_and_the_signal_unless_nodefer() {
        let mut thread = ThreadSignals::new(1);
        assert!(thread.enter_handler(libc::SIGUSR1, &handler(0x4000, 0, sigbit(libc::SIGUSR2))));
        assert_eq!(
            thread.blocked,
            1 | sigbit(libc::SIGUSR1) | sigbit(libc::SIGUSR2)
        );
        let mut thread = ThreadSignals::new(1);
        assert!(thread.enter_handler(libc::SIGUSR1, &handler(0x4000, libc::SA_NODEFER, 0)));
        assert_eq!(thread.blocked, 1);
    }

    #[test]
    fn sigreturn_cannot_block_sigkill() {
        let mut thread = ThreadSignals::new(0);
        thread.sigreturn(u64::MAX);
        assert_eq!(thread.blocked, !UNBLOCKABLE);
    }

    #[test]
    fn delivery_to_a_handler_applies_resethand_and_the_handler_mask() {
        let mut dispositions = Dispositions::after_exec(sigbit(libc::SIGPIPE));
        let mut thread = ThreadSignals::new(0);
        dispositions
            .sigaction(
                libc::SIGUSR1,
                Some(handler(0x4000, libc::SA_RESETHAND, sigbit(libc::SIGUSR2))),
            )
            .unwrap();
        let used = deliver(&mut dispositions, &mut thread, libc::SIGUSR1)
            .unwrap()
            .unwrap();
        assert_eq!(used.handler, 0x4000);
        assert_eq!(
            thread.blocked,
            sigbit(libc::SIGUSR1) | sigbit(libc::SIGUSR2)
        );
        assert_eq!(dispositions.caught(), 0);
        let ignored = deliver(&mut dispositions, &mut thread, libc::SIGPIPE)
            .unwrap()
            .unwrap();
        assert_eq!(ignored.handler, SIG_IGN);
        assert_eq!(
            thread.blocked,
            sigbit(libc::SIGUSR1) | sigbit(libc::SIGUSR2),
            "an ignored signal changes no mask"
        );
    }

    #[test]
    fn clone_sighand_shares_actions_and_fork_copies_them() {
        let parent = SignalShadow::seeded(0, 4);
        let thread = parent
            .for_clone(libc::CLONE_SIGHAND as u64 | libc::CLONE_VM as u64)
            .unwrap();
        let mut child = parent.for_clone(0).unwrap();
        thread
            .dispositions
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .sigaction(libc::SIGUSR1, Some(handler(0x4000, 0, 0)))
            .unwrap();
        assert_eq!(
            parent
                .dispositions
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .caught(),
            sigbit(libc::SIGUSR1)
        );
        assert_eq!(
            child
                .dispositions
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .caught(),
            0
        );
        assert_eq!(child.thread.blocked, 4, "the mask is inherited");
        child
            .thread
            .sigprocmask(libc::SIG_SETMASK, Some(0))
            .unwrap();
        assert_eq!(parent.thread.blocked, 4, "masks are per thread");
    }

    #[test]
    fn exec_unshares_shared_actions_before_resetting_them() {
        let parent = SignalShadow::seeded(0, 0);
        let mut sibling = parent
            .for_clone(libc::CLONE_SIGHAND as u64 | libc::CLONE_VM as u64)
            .unwrap();
        parent
            .dispositions
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .sigaction(libc::SIGUSR1, Some(handler(0x4000, 0, 0)))
            .unwrap();
        assert!(
            sibling
                .thread
                .enter_handler(libc::SIGUSR2, &handler(0x4000, 0, 0))
        );
        assert!(sibling.exec());
        assert_eq!(
            sibling
                .dispositions
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .caught(),
            0
        );
        assert!(
            sibling.thread.activations.is_empty(),
            "exec leaves no frame open"
        );
        assert_eq!(
            parent
                .dispositions
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .caught(),
            sigbit(libc::SIGUSR1),
            "the peer keeps its handler"
        );
    }

    #[test]
    fn shadow_round_trips_through_serde_with_all_64_actions() {
        let shadow = SignalShadow::seeded(sigbit(64), 2);
        let bytes = serde_json::to_vec(&shadow).unwrap();
        let back: SignalShadow = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            *back.dispositions.lock().unwrap(),
            *shadow.dispositions.lock().unwrap()
        );
        assert_eq!(back.thread, shadow.thread);
        assert_eq!(
            back.dispositions
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .ignored(),
            sigbit(64)
        );
        let short = serde_json::to_vec(&vec![Action::default(); 63]).unwrap();
        assert!(serde_json::from_slice::<Dispositions>(&short).is_err());
    }

    #[test]
    fn unreported_returns_are_recognised_by_their_saved_masks() {
        let mut thread = ThreadSignals::new(1);
        assert!(thread.enter_handler(libc::SIGUSR1, &handler(0x4000, 0, 0)));
        let outer = thread.blocked;
        assert!(thread.enter_handler(libc::SIGUSR2, &handler(0x4000, 0, 0)));
        assert_eq!(thread.activations, vec![1, outer]);
        assert!(thread.reconcile_returns(thread.blocked, 0), "still inside");
        assert!(
            thread.reconcile_returns(outer, 0),
            "the inner handler returned"
        );
        assert_eq!(thread.activations, vec![1]);
        assert!(thread.reconcile_returns(1, 0), "the outer one too");
        assert!(thread.activations.is_empty());
        assert_eq!(thread.blocked, 1);
        assert!(
            !thread.reconcile_returns(4, 0),
            "an unexplained mask is a mismatch"
        );
        assert!(
            thread.reconcile_returns(1 | 4, 4),
            "an ignored bit is left out"
        );
        assert_eq!(
            thread.blocked, 1,
            "the copy keeps its own value of an ignored bit"
        );
    }

    #[test]
    fn two_returns_at_once_pop_both_frames() {
        let mut thread = ThreadSignals::new(0);
        assert!(thread.enter_handler(libc::SIGUSR1, &handler(0x4000, 0, 0)));
        assert!(thread.enter_handler(libc::SIGUSR2, &handler(0x4000, 0, 0)));
        assert!(thread.reconcile_returns(0, 0));
        assert!(thread.activations.is_empty());
    }

    #[test]
    fn handler_after_an_interrupted_temporary_mask_wait_saves_the_original_mask() {
        let original = sigbit(libc::SIGUSR1) | sigbit(libc::SIGUSR2);
        let temporary = sigbit(libc::SIGUSR1);
        let mut thread = ThreadSignals::new(original);
        thread.interrupted_under_temporary_mask(temporary);
        assert_eq!(thread.blocked, temporary);
        assert!(thread.enter_handler(libc::SIGINT, &handler(0x4000, 0, 0)));
        assert_eq!(thread.blocked, temporary | sigbit(libc::SIGINT));
        assert_eq!(thread.saved, None, "the frame now carries the restore");
        assert_eq!(thread.activations, vec![original]);
        assert!(
            thread.reconcile_returns(original, 0),
            "its return restores it"
        );
        assert_eq!(thread.blocked, original);
        assert!(thread.activations.is_empty());
    }

    #[test]
    fn temporary_mask_without_a_handler_comes_back_when_the_thread_goes_on() {
        let mut thread = ThreadSignals::new(4);
        thread.interrupted_under_temporary_mask(8);
        thread.settle();
        assert_eq!(thread, ThreadSignals::new(4));
        thread.interrupted_under_temporary_mask(8);
        assert_eq!(
            thread.sigprocmask(libc::SIG_BLOCK, None),
            Ok(8),
            "not settled yet"
        );
    }

    #[test]
    fn a_handler_that_changes_its_own_mask_is_still_followed_back() {
        // SA_NODEFER with an empty sa_mask leaves the mask as it was, but the
        // handler blocks SIGUSR2 itself before it returns.
        let mut thread = ThreadSignals::new(1);
        assert!(thread.enter_handler(libc::SIGUSR1, &handler(0x4000, libc::SA_NODEFER, 0)));
        assert_eq!(thread.activations, vec![1]);
        thread
            .sigprocmask(libc::SIG_BLOCK, Some(sigbit(libc::SIGUSR2)))
            .unwrap();
        assert!(thread.reconcile_returns(1, 0), "its return restores 1");
        assert_eq!(thread.blocked, 1);
        assert!(thread.activations.is_empty());
    }

    #[test]
    fn frames_beyond_the_limit_give_the_copy_up() {
        let mut thread = ThreadSignals::new(0);
        for _ in 0..MAX_OPEN_FRAMES {
            assert!(thread.enter_handler(libc::SIGUSR1, &handler(0x4000, libc::SA_NODEFER, 0)));
        }
        assert!(!thread.enter_handler(libc::SIGUSR1, &handler(0x4000, libc::SA_NODEFER, 0)));
        assert_eq!(
            thread.activations.len(),
            MAX_OPEN_FRAMES,
            "nothing is dropped silently"
        );
    }

    #[test]
    fn a_child_made_inside_a_handler_gets_no_copy() {
        let mut parent = SignalShadow::seeded(0, 0);
        assert!(
            parent
                .thread
                .enter_handler(libc::SIGUSR1, &handler(0x4000, 0, 0))
        );
        assert!(parent.for_clone(0).is_none(), "fork");
        let vfork = libc::CLONE_VM as u64 | libc::CLONE_VFORK as u64;
        assert!(parent.for_clone(vfork).is_none(), "vfork");
        assert!(
            parent.dispositions.lock().unwrap().is_some(),
            "children with copied actions leave the parent's known"
        );
        // A thread, even with CLONE_THREAD, may have no stack of its own
        // (raw clone): no copy, and the actions it shares become unknown.
        let thread = libc::CLONE_VM as u64 | libc::CLONE_SIGHAND as u64 | libc::CLONE_THREAD as u64;
        assert!(parent.for_clone(thread).is_none());
        assert!(parent.dispositions.lock().unwrap().is_none());
    }

    #[test]
    fn an_ignored_signal_does_not_end_the_wait() {
        let mut dispositions = Dispositions::after_exec(sigbit(libc::SIGURG));
        let mut thread = ThreadSignals::new(0);
        thread.wait_mask = Some(sigbit(libc::SIGUSR1));
        deliver(&mut dispositions, &mut thread, libc::SIGURG).unwrap();
        assert_eq!(
            thread.wait_mask,
            Some(sigbit(libc::SIGUSR1)),
            "still waiting"
        );
        dispositions
            .sigaction(libc::SIGUSR2, Some(handler(0x4000, 0, 0)))
            .unwrap();
        deliver(&mut dispositions, &mut thread, libc::SIGUSR2).unwrap();
        assert_eq!(thread.wait_mask, None, "a handler ends it");
    }

    #[test]
    fn a_frame_is_observed_only_once_a_check_saw_its_handler_mask() {
        let mut thread = ThreadSignals::new(0);
        assert!(thread.innermost_observed(), "no open frame needs observing");
        assert!(thread.enter_handler(libc::SIGUSR1, &handler(0x4000, 0, 0)));
        assert!(!thread.innermost_observed());
        thread.observe_handler_mask();
        assert!(thread.innermost_observed());
        // A handler whose mask equals its frame's saved mask cannot be seen.
        let mut nodefer = ThreadSignals::new(sigbit(libc::SIGUSR1));
        assert!(nodefer.enter_handler(libc::SIGUSR1, &handler(0x4000, libc::SA_NODEFER, 0)));
        nodefer.observe_handler_mask();
        assert!(!nodefer.innermost_observed());
        assert!(thread.enter_handler(libc::SIGUSR2, &handler(0x4000, 0, 0)));
        assert!(
            !thread.innermost_observed(),
            "a nested frame starts unobserved"
        );
        thread.sigreturn(sigbit(libc::SIGUSR1));
        assert!(thread.innermost_observed(), "the outer frame was observed");
    }

    #[test]
    fn clone_clear_sighand_resets_caught_signals_and_keeps_ignored_ones() {
        let parent = SignalShadow::seeded(sigbit(libc::SIGPIPE), 0);
        parent
            .dispositions
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .sigaction(libc::SIGUSR1, Some(handler(0x4000, 0, 0)))
            .unwrap();
        let child = parent.for_clone(CLONE_CLEAR_SIGHAND).unwrap();
        let child = child.dispositions.lock().unwrap();
        assert_eq!(child.as_ref().unwrap().caught(), 0);
        assert_eq!(child.as_ref().unwrap().ignored(), sigbit(libc::SIGPIPE));
    }

    #[test]
    fn unknown_actions_give_no_clone_and_no_exec() {
        let mut shadow = SignalShadow::seeded(0, 0);
        shadow.forget_dispositions();
        assert!(shadow.for_clone(0).is_none());
        assert!(
            shadow
                .for_clone(libc::CLONE_SIGHAND as u64 | libc::CLONE_VM as u64)
                .is_none()
        );
        assert!(!shadow.exec());
    }
}
