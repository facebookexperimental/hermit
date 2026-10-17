/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Keeping each thread's [`SignalShadow`] in step with the guest.
//!
//! Off unless [`SIGNAL_STATE_CHECK_ENV`] is `1` (docs/SIGNAL_STATE_CHECK.md).
//!
//! Every function here runs after the guest operation it records has
//! committed, uses only values Detcore already holds for that operation, and
//! issues no injection and no scheduler request: recording changes nothing the
//! guest or the schedule can observe, and adds no log line at INFO or above
//! while the copy matches (one TRACE line per check, for counting).
//!
//! Where the backend's kernel signal state is the guest's own
//! ([`kernel_state_is_the_guests`]), the
//! copy is checked against the kernel after each recorded call: the old action
//! or old mask the kernel wrote back, and `SigBlk`, `SigIgn` and `SigCgt`. A
//! mismatch is logged once, by name, and the thread's copy is dropped: it is
//! never repaired from the kernel, which would hide the defect the check is
//! for.

use reverie::Errno;
use reverie::Guest;
use reverie::Pid;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use tracing::debug;
use tracing::trace;
use tracing::warn;

use super::threads::KernelSigaction;
use super::threads::KernelSigset;
use super::threads::host_signal_state_reads_refused;
use super::threads::read_thread_signal_state;
use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;
use crate::signal_state::Action;
use crate::signal_state::Dispositions;
use crate::signal_state::SignalShadow;
use crate::signal_state::ThreadSignals;
use crate::signal_state::sigbit;

/// The environment variable that turns the copy on: `1` seeds, records and
/// checks it; unset (the default) leaves every run as it was. Step 1 of A5 is
/// a diagnostic until step 2 relies on the copy: without exact backend reports
/// of handler returns and deliveries (ptrace traps neither), a checked copy
/// can still report a divergence in rare valid cases, so it is opt-in.
pub const SIGNAL_STATE_CHECK_ENV: &str = "HERMIT_SIGNAL_STATE_CHECK";

/// Whether [`SIGNAL_STATE_CHECK_ENV`] turns the copy on, read once. Tests
/// always have it on.
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(|| std::env::var_os(SIGNAL_STATE_CHECK_ENV).is_some_and(|v| v == "1"))
    }
}

/// The prefix of the one WARN line a divergence produces, for log searches.
pub(crate) const DIVERGED: &str = "SIGNAL_STATE_COPY_DIVERGED";

/// The signal whose kernel state differs from the guest's by design, left out
/// of every comparison: Reverie's preemption signal is Hermit's, and Detcore
/// keeps the guest from installing or blocking it (`handle_rt_sigaction`,
/// `rt_sigprocmask_inner`).
fn reserved() -> KernelSigset {
    sigbit(reverie::PERF_EVENT_SIGNAL as i32)
}

/// SIGTRAP, whose kernel state the ptrace backend itself changes: when it
/// single-steps the guest, the kernel force-delivers the step trap, which
/// unblocks SIGTRAP and resets an ignored, or caught but blocked, SIGTRAP to
/// its default (`force_sig_info_to_task`; Reverie characterizes this in
/// `sigtrap_profile_follows_each_handler_shape`). The copy cannot see a step,
/// so a difference in SIGTRAP alone makes the copy unknown (it is given up
/// quietly) rather than reported, and never passes as a match.
fn trap() -> KernelSigset {
    sigbit(libc::SIGTRAP)
}

/// What comparing one of the copy's sets with the kernel's found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Compared {
    Same,
    /// Only SIGTRAP differs ([`trap`]).
    TrapOnly,
    Different,
}

fn compare(kernel: KernelSigset, copy: KernelSigset) -> Compared {
    match (kernel ^ copy) & !reserved() {
        0 => Compared::Same,
        difference if difference == trap() => Compared::TrapOnly,
        _ => Compared::Different,
    }
}

/// Gives the copy up quietly: the kernel's SIGTRAP state differs from it in a
/// way only the backend's single-step explains.
fn trap_changed<G, T>(guest: &mut G, at: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    debug!(
        "[dtid {}] signal-state copy dropped at {at}: SIGTRAP's kernel state differs, as a \
         ptrace single-step leaves it",
        guest.thread_state().dettid
    );
    drop_copy(guest);
}

impl From<KernelSigaction> for Action {
    fn from(action: KernelSigaction) -> Self {
        Action {
            handler: action.handler,
            flags: action.flags,
            restorer: action.restorer,
            mask: action.mask,
        }
    }
}

/// Whether this backend's kernel signal state for a guest thread is the
/// guest's own: the guest thread is a host task, and no runtime in the
/// guest's process installs handlers or masks of its own. This is the mechanism
/// fact `supports_blocked_wait_signal_interruption` describes (true for
/// ptrace, and for e9patch, which runs under ptrace). Threads must also be
/// serialized: otherwise two threads sharing actions could change them in the
/// kernel in one order and in the copy in another. Elsewhere the copy is
/// neither seeded from the kernel nor checked against it.
fn kernel_state_is_the_guests<G, T>(guest: &G) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let config = guest.config();
    enabled()
        && config.sequentialize_threads
        && config.backend.supports_blocked_wait_signal_interruption
        && !host_signal_state_reads_refused()
}

/// Applies `change` to this thread's copy: the actions its group shares and
/// its own masks. Returns `None`, and drops the thread's copy without a new
/// report, when the shared actions are unknown because another thread
/// sharing them lost its copy (that loss was reported).
fn with_copy<G, T, R>(
    guest: &mut G,
    change: impl FnOnce(&mut Dispositions, &mut ThreadSignals) -> R,
) -> Option<R>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let shadow = guest.thread_state_mut().signal_shadow.as_mut()?;
    let actions = std::sync::Arc::clone(&shadow.dispositions);
    let mut actions = actions.lock().unwrap();
    match actions.as_mut() {
        Some(dispositions) => Some(change(dispositions, &mut shadow.thread)),
        None => {
            drop(actions);
            debug!(
                "[dtid {}] signal-state copy dropped: a thread sharing its actions lost its copy",
                guest.thread_state().dettid
            );
            drop_copy(guest);
            None
        }
    }
}

/// Gives this thread's copy up quietly, for a call whose effect cannot be
/// known (`reason` says why).
pub(crate) fn give_up<G, T>(guest: &mut G, reason: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.thread_state().signal_shadow.is_some() {
        debug!(
            "[dtid {}] signal-state copy dropped: {reason}",
            guest.thread_state().dettid
        );
        drop_copy(guest);
    }
}

/// Drops this thread's copy: threads it creates start without one, and the
/// actions it shares become unknown, because this thread's later changes to
/// them would go unrecorded. A copy given up quietly is seeded again at the
/// thread's next exec, whose ignored set and mask describe the whole state
/// (nothing of the old image is repaired); [`diverged`] marks a copy that
/// diverged as never to be seeded again.
fn drop_copy<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Some(shadow) = guest.thread_state().signal_shadow.as_ref() {
        shadow.forget_dispositions();
    }
    guest.thread_state_mut().signal_shadow = None;
}

/// At a syscall's entry, the thread has run guest code since its last event:
/// a temporary-mask restore no handler took has happened, a signal may have
/// reached a handler unreported, and an open handler frame may have returned
/// unseen. Accounts for all three before the call can depend on the mask (a
/// clone copies it; an exec keeps it). No guest memory is read here: a wait's
/// temporary mask is recorded by its handler, in the call's own turn
/// ([`note_wait_mask`]).
pub(crate) fn at_syscall_entry<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    take_unreported(guest);
    let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() else {
        return;
    };
    shadow.thread.settle();
    shadow.thread.wait_mask = None;
    reconcile_returns(guest, "syscall entry");
}

/// Signals the backend passed on to a thread without a report (the ptrace
/// backend does so for many signals it held during a Tool injection), as
/// (host pid, host tid, signal). They are recorded where the copy lives, in
/// this process (`Tool::filter_unreported_signal`), and consumed at that
/// thread's next operation, whether or not it has a copy. A realtime signal,
/// which that hook cannot name, is not recorded (see the issue draft named in
/// the A5 design note, section 11).
#[cfg(not(test))]
static UNREPORTED: std::sync::Mutex<Vec<(i32, i32, i32)>> = std::sync::Mutex::new(Vec::new());

/// Whether [`UNREPORTED`] may hold an entry, so that threads on a backend
/// that never notes one (or with none noted) never take its lock.
#[cfg(not(test))]
static ANY_UNREPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// Tests keep their notes per test thread, so concurrent tests cannot see each
// other's.
#[cfg(test)]
thread_local! {
    static UNREPORTED_NOTES: std::cell::RefCell<Vec<(i32, i32, i32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `f` on the notes.
fn with_notes<R>(f: impl FnOnce(&mut Vec<(i32, i32, i32)>) -> R) -> R {
    #[cfg(not(test))]
    {
        let mut notes = UNREPORTED.lock().unwrap();
        let result = f(&mut notes);
        ANY_UNREPORTED.store(!notes.is_empty(), std::sync::atomic::Ordering::SeqCst);
        result
    }
    #[cfg(test)]
    {
        UNREPORTED_NOTES.with(|notes| f(&mut notes.borrow_mut()))
    }
}

/// Whether any note may exist, without taking a lock.
fn any_notes() -> bool {
    #[cfg(not(test))]
    {
        ANY_UNREPORTED.load(std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(test)]
    {
        UNREPORTED_NOTES.with(|notes| !notes.borrow().is_empty())
    }
}

/// Records that `signal` reached host thread `tid` of process `pid` without a
/// report.
pub(crate) fn note_unreported(pid: i32, tid: i32, signal: i32) {
    trace!("[tid {tid}] signal-state copy: signal {signal} was passed on unreported");
    with_notes(|notes| notes.push((pid, tid, signal)));
}

/// Whether another live thread has an unreported signal noted: if it shares
/// this thread's actions, its handler may have changed them (`SA_RESETHAND`)
/// unseen, until it gives its own copy up. Entries of threads that no longer
/// exist are removed here.
fn unreported_elsewhere(tid: i32) -> bool {
    if !any_notes() {
        return false;
    }
    with_notes(|notes| {
        notes.retain(|(_, t, _)| {
            !super::threads::thread_is_gone(Pid::from_raw(*t), Pid::from_raw(*t))
        });
        // Any other thread: a CLONE_SIGHAND group need not be one process.
        notes.iter().any(|(_, t, _)| *t != tid)
    })
}

/// Whether any unreported signal is noted and not yet consumed: a clone now
/// could copy actions a handler has already changed unseen.
pub(crate) fn unreported_pending() -> bool {
    any_notes()
        && with_notes(|notes| {
            notes.retain(|(_, t, _)| {
                !super::threads::thread_is_gone(Pid::from_raw(*t), Pid::from_raw(*t))
            });
            !notes.is_empty()
        })
}

/// A signal that reached this thread unreported may have entered a handler
/// the copy never saw, with the action as it stood at delivery, which the
/// copy cannot know any more: the copy is given up. This thread's notes are
/// consumed whether or not it has a copy.
fn take_unreported<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if !any_notes() {
        return;
    }
    let (pid, tid) = (guest.pid().as_raw(), guest.tid().as_raw());
    let signals: Vec<i32> = with_notes(|notes| {
        let (mine, others): (Vec<_>, Vec<_>) = notes
            .drain(..)
            .partition(|(p, t, _)| *p == pid && *t == tid);
        *notes = others;
        mine.into_iter().map(|(_, _, signal)| signal).collect()
    });
    if let Some(signal) = signals.first() {
        give_up(guest, &format!("signal {signal} was passed on unreported"));
    }
}

/// Accounts for handler returns the kernel ran without a report (the ptrace
/// backend never traps `rt_sigreturn`), from the kernel's current mask, before
/// a transition that depends on the mask. Does nothing while no handler frame
/// is open, or where the kernel's state is not the guest's.
fn reconcile_returns<G, T>(guest: &mut G, before: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let open_frames = guest
        .thread_state()
        .signal_shadow
        .as_ref()
        .is_some_and(|shadow| !shadow.thread.activations.is_empty());
    if !open_frames || !kernel_state_is_the_guests(guest) {
        return;
    }
    let Some(kernel) = read_or_give_up(guest, before) else {
        return;
    };
    let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() else {
        return;
    };
    let copy = shadow.thread.blocked;
    let open = shadow.thread.activations.len();
    let innermost_observed = shadow.thread.innermost_observed();
    if shadow
        .thread
        .reconcile_returns(kernel.blocked, reserved() | trap())
    {
        let closed = open - shadow.thread.activations.len();
        if closed > 1 {
            // An outer frame's mask: several returns, or an inner handler
            // that rewrote its uc_sigmask. Mask equality cannot tell which.
            debug!(
                "[dtid {}] signal-state copy dropped at {before}: the kernel's mask matches an \
                 outer handler frame ({closed} frames), which is ambiguous",
                guest.thread_state().dettid
            );
            drop_copy(guest);
        } else {
            if closed == 1 {
                debug!(
                    "[dtid {}] signal-state copy: a handler frame taken as returned at {before}",
                    guest.thread_state().dettid
                );
                if !innermost_observed {
                    // A return, or a handler entry the copy predicted wrongly
                    // (or a frame the kernel failed to write): mask equality
                    // cannot tell which.
                    #[cfg(test)]
                    UNVERIFIED_ENTRIES.with(|count| count.set(count.get() + 1));
                    return give_up(
                        guest,
                        "a handler frame was taken as returned before any check saw its \
                         handler's mask",
                    );
                }
            } else if let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() {
                // The kernel holds the mask the copy predicts for the open
                // handler.
                shadow.thread.observe_handler_mask();
            }
            let blocked = guest
                .thread_state()
                .signal_shadow
                .as_ref()
                .map(|shadow| shadow.thread.blocked);
            if let Some(blocked) = blocked
                && compare(kernel.blocked, blocked) == Compared::TrapOnly
            {
                trap_changed(guest, before);
            }
        }
    } else if open > 0 {
        // A handler may install any mask by rewriting its frame's uc_sigmask
        // before an unseen return.
        give_up(
            guest,
            &format!(
                "at {before} the kernel's mask matches no open handler frame, which a \
                 handler's rewritten uc_sigmask could explain"
            ),
        );
    } else {
        diverged(
            guest,
            before,
            &format!(
                "SigBlk kernel {:#018x} is neither the copy's {:#018x} nor an open handler frame's",
                kernel.blocked & !reserved(),
                copy & !reserved()
            ),
        );
    }
}

/// Seeds the first image's copy, or applies `execve`'s reset to a later one.
/// Called from the new image's first callback. The first image's ignored set
/// and mask are what it inherited from the launcher, read from the kernel
/// before it runs a guest instruction; where that read is unavailable, the
/// copy stays unknown. Seeding there needs the backend to report the first
/// image's inherited state, which the A5 design puts in the `Guest` contract
/// (its step 3).
pub(crate) fn at_exec<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() {
        if !shadow.exec() {
            drop_copy(guest);
            return;
        }
        trace!(
            "[dtid {}] signal-state copy: reset for execve",
            guest.thread_state().dettid
        );
        check_against_kernel(guest, "execve");
        return;
    }
    // A thread with no copy is seeded at its exec, where the ignored set and
    // the mask describe the whole state. One whose copy diverged, or whose
    // creator's did, is never seeded again.
    if guest.thread_state().signal_shadow_lost || !kernel_state_is_the_guests(guest) {
        return;
    }
    match read_thread_signal_state(guest.pid(), guest.tid()) {
        Ok(state) => {
            guest.thread_state_mut().signal_shadow =
                Some(SignalShadow::seeded(state.ignored, state.blocked));
            trace!(
                "[dtid {}] signal-state copy: seeded from the first image",
                guest.thread_state().dettid
            );
        }
        Err(errno) => debug!(
            "[dtid {}] signal-state copy not seeded: the kernel state is unreadable ({errno})",
            guest.thread_state().dettid
        ),
    }
}

/// Records a committed `rt_sigaction`. `new` is the action the kernel
/// installed (Detcore's sanitized copy of an action the kernel validated),
/// `None` for a query. `result` is the call's result: an `EFAULT` after the
/// new action was read can only come from writing `old_action`, after the new
/// action is installed.
pub(super) fn record_rt_sigaction<G, T>(
    guest: &mut G,
    signum: i32,
    new: Option<KernelSigaction>,
    old_action: Option<AddrMut<'_, libc::sigaction>>,
    result: &Result<i64, Errno>,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let committed = result.is_ok() || (new.is_some() && *result == Err(Errno::EFAULT));
    if !committed {
        return;
    }
    let Some(predicted) = with_copy(guest, |dispositions, _| {
        dispositions.sigaction(signum, new.map(Action::from))
    }) else {
        return;
    };
    let old = match predicted {
        Ok(old) => old,
        Err(errno) => {
            return diverged(
                guest,
                "rt_sigaction",
                &format!(
                    "the kernel accepted signal {signum}, which the copy refuses with {errno}"
                ),
            );
        }
    };
    let written = match (result.is_ok(), old_action) {
        (true, Some(address)) => match guest
            .memory()
            .read_value::<_, KernelSigaction>(address.cast::<KernelSigaction>())
        {
            Ok(written) => Some(written),
            Err(_) => {
                return give_up(guest, "the old action the kernel wrote cannot be read back");
            }
        },
        _ => None,
    };
    // A sibling's unreported handler may have reset this action unseen
    // (SA_RESETHAND): the old action is not compared while one is noted.
    if let Some(written) = written
        && Action::from(written) != old
        && !unreported_elsewhere(guest.tid().as_raw())
    {
        if signum == libc::SIGTRAP {
            return trap_changed(guest, "rt_sigaction");
        }
        return diverged(
            guest,
            "rt_sigaction",
            &format!(
                "old action of signal {signum}: the kernel wrote {:?}, the copy holds {old:?}",
                Action::from(written)
            ),
        );
    }
    // A caught realtime signal can reach its handler without any report:
    // Reverie never shows one passed on unreported to the Tool (A5 design
    // section 11). Until it does, the copy cannot follow such a thread.
    if let Some(new) = new
        && (32..=crate::signal_state::MAX_SIGNAL).contains(&signum)
        && new.handler > 1
    {
        return give_up(
            guest,
            &format!(
                "signal {signum} is a caught realtime signal, which can reach its handler \
                 unreported"
            ),
        );
    }
    check_against_kernel(guest, "rt_sigaction");
}

/// What Detcore knows of an `rt_sigprocmask` call's set.
#[derive(Clone, Copy, Debug)]
pub(super) enum SetInput {
    /// The call passed no set: a query.
    None,
    /// Detcore never injected the call: its own check of the set failed
    /// first, so the kernel did nothing.
    NotRun,
    /// The kernel read this set and accepted it (the `SIG_BLOCK` and
    /// `SIG_SETMASK` paths validate it before injecting). It is the set
    /// without Hermit's reserved signal.
    Validated(KernelSigset),
    /// The call ran with a set the kernel validated only by running; Detcore
    /// reads it after a successful call, from memory the kernel just read
    /// (`None` when the old mask overwrote it, or the read failed).
    Unvalidated(Option<KernelSigset>),
}

/// Records a committed `rt_sigprocmask`. An `EFAULT` is a committed change
/// only when it can come from writing `oldset` alone.
pub(super) fn record_rt_sigprocmask<G, T>(
    guest: &mut G,
    how: i32,
    set: SetInput,
    oldset: Option<AddrMut<'_, libc::sigset_t>>,
    result: &Result<i64, Errno>,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let valid_how = matches!(how, libc::SIG_BLOCK | libc::SIG_UNBLOCK | libc::SIG_SETMASK);
    let set = match (set, result) {
        (SetInput::NotRun, _) => return,
        (SetInput::None, Ok(_)) => None,
        (SetInput::Validated(set), Ok(_) | Err(Errno::EFAULT)) => Some(set),
        (SetInput::Unvalidated(Some(set)), Ok(_)) => Some(set),
        (SetInput::Unvalidated(None), Ok(_)) => {
            debug!(
                "[dtid {}] signal-state copy dropped: rt_sigprocmask's set is not known after \
                 the call (it aliases the old mask, or could not be read)",
                guest.thread_state().dettid
            );
            drop_copy(guest);
            return;
        }
        // The kernel reads the set before checking `how`, and before writing
        // the old mask: without an old mask, or with an invalid `how`, the
        // fault was the set's and nothing changed.
        (SetInput::Unvalidated(_), Err(Errno::EFAULT)) if oldset.is_none() || !valid_how => {
            return;
        }
        (SetInput::Unvalidated(_), Err(Errno::EFAULT)) => {
            // The set or the old mask: Linux leaves no way to tell which.
            debug!(
                "[dtid {}] signal-state copy dropped: rt_sigprocmask's EFAULT may come from \
                 its set or its old mask",
                guest.thread_state().dettid
            );
            drop_copy(guest);
            return;
        }
        _ => return,
    };
    let Some(predicted) = with_copy(guest, |_, thread| thread.sigprocmask(how, set)) else {
        return;
    };
    let old = match predicted {
        Ok(old) => old,
        Err(errno) => {
            return diverged(
                guest,
                "rt_sigprocmask",
                &format!("the kernel accepted how={how}, which the copy refuses with {errno}"),
            );
        }
    };
    let written = match (result.is_ok(), oldset) {
        (true, Some(address)) => match guest
            .memory()
            .read_value::<_, KernelSigset>(address.cast::<KernelSigset>())
        {
            Ok(written) => Some(written),
            Err(_) => return give_up(guest, "the old mask the kernel wrote cannot be read back"),
        },
        _ => None,
    };
    if let Some(written) = written {
        match compare(written, old) {
            Compared::Same => {}
            Compared::TrapOnly => return trap_changed(guest, "rt_sigprocmask"),
            Compared::Different => {
                return diverged(
                    guest,
                    "rt_sigprocmask",
                    &format!(
                        "old mask: the kernel wrote {:#x}, the copy holds {:#x}",
                        written & !reserved(),
                        old & !reserved()
                    ),
                );
            }
        }
    }
    check_against_kernel(guest, "rt_sigprocmask");
}

/// Records that the Tool approved `signal` for delivery to this thread, so the
/// backend delivers it now: a caught signal enters its handler with
/// `sa_mask` added, and `SA_RESETHAND` puts the default back. The handler
/// mask is checked at the thread's next recorded call, when the frame has been
/// written. A delivery whose frame cannot be written becomes a SIGSEGV and
/// leaves the old mask, which the check can take for the frame's return: that
/// case is not told apart.
pub(crate) fn record_delivery<G, T>(guest: &mut G, signal: i32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // At a delivery stop the kernel has not installed the handler mask yet.
    take_unreported(guest);
    take_wait_outcome(guest);
    reconcile_returns(guest, "signal delivery");
    let Some(delivered) = with_copy(guest, |dispositions, thread| {
        crate::signal_state::deliver(dispositions, thread, signal)
    }) else {
        return;
    };
    match delivered {
        Ok(Some(action)) => {
            trace!(
                "[dtid {}] signal-state copy: recorded the delivery of signal {signal}",
                guest.thread_state().dettid
            );
            // A handler whose mask equals an open frame's saved mask cannot
            // be told from that frame's return: the next check verifies
            // nothing about this entry.
            if let Some(shadow) = guest.thread_state().signal_shadow.as_ref()
                && action.handler > 1
                && shadow.thread.activations.last() == Some(&shadow.thread.blocked)
            {
                debug!(
                    "[dtid {}] signal-state copy: the handler mask of signal {signal} equals the \
                     mask its frame saves, so its entry cannot be told from its return",
                    guest.thread_state().dettid
                );
            }
        }
        Ok(None) => {
            debug!(
                "[dtid {}] signal-state copy dropped: more than {} handler frames open",
                guest.thread_state().dettid,
                crate::signal_state::MAX_OPEN_FRAMES
            );
            drop_copy(guest);
        }
        Err(errno) => diverged(
            guest,
            "signal delivery",
            &format!("the backend delivers signal {signal}, which the copy refuses with {errno}"),
        ),
    }
}

/// At a delivery stop inside a temporary-mask wait, Linux holds one of two
/// masks: the wait's, when the kernel ran the call (the handler's frame then
/// saves the thread's own mask), or the thread's own, when the signal arrived
/// before the call started, which depends on host timing (see
/// `record_or_replay_rt_sigsuspend`). The kernel's current mask tells which;
/// any other mask is left for the checks that follow to report.
fn take_wait_outcome<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some((wait_mask, saved)) = guest
        .thread_state()
        .signal_shadow
        .as_ref()
        .map(|shadow| (shadow.thread.wait_mask, shadow.thread.saved))
    else {
        return;
    };
    if (wait_mask.is_none() && saved.is_none()) || !kernel_state_is_the_guests(guest) {
        return;
    }
    let Some(kernel) = read_or_give_up(guest, "signal delivery") else {
        return;
    };
    let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() else {
        return;
    };
    let compared = !(reserved() | trap());
    let same = |a: KernelSigset, b: KernelSigset| a & compared == b & compared;
    // A temporary-mask restore no handler took has happened if the kernel
    // already shows the restored mask (the wait returned to the guest).
    if let Some(saved) = saved
        && same(kernel.blocked, saved)
    {
        shadow.thread.settle();
    }
    if let Some(wait_mask) = wait_mask
        && same(kernel.blocked, wait_mask)
        && !same(shadow.thread.blocked, wait_mask)
    {
        shadow.thread.interrupted_under_temporary_mask(wait_mask);
    }
}

/// Unused until a backend reports `rt_sigreturn` (A5 step 3): the ptrace
/// backend, the only one whose copy is seeded today, never traps it.
///
/// Records an `rt_sigreturn` at its entry, before it runs: Linux installs the
/// frame's `uc_sigmask` as soon as it can read it, before restoring the
/// registers can fail (`arch/x86/kernel/signal_64.c`). A frame whose mask
/// cannot be read changes no mask; the kernel then sends SIGSEGV.
pub(crate) async fn record_rt_sigreturn<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.thread_state().signal_shadow.is_none() {
        return;
    }
    let sp = guest.regs().await.rsp as usize;
    let restored = Addr::<u64>::from_raw(sp.wrapping_add(super::signal::UC_SIGMASK_OFFSET))
        .and_then(|address| guest.memory().read_value(address).ok());
    if let Some(mask) = restored {
        with_copy(guest, |_, thread| thread.sigreturn(mask & !reserved()));
    }
}

/// Compares the copy with the kernel's `/proc` view of this thread, where that
/// view is the guest's own.
fn check_against_kernel<G, T>(guest: &mut G, after: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    take_unreported(guest);
    if !kernel_state_is_the_guests(guest) || guest.thread_state().signal_shadow.is_none() {
        return;
    }
    let Some(kernel) = read_or_give_up(guest, after) else {
        return;
    };
    let Some(copy) = with_copy(guest, |dispositions, thread| {
        (
            thread.blocked,
            dispositions.ignored(),
            dispositions.caught(),
        )
    }) else {
        return;
    };
    let (blocked, ignored, caught) = copy;
    // A sibling's unreported handler may have changed the shared actions
    // unseen; that sibling gives the copy up at its own next operation.
    let shared_unsure = unreported_elsewhere(guest.tid().as_raw());
    let fields = [
        ("SigBlk", kernel.blocked, blocked, true),
        ("SigIgn", kernel.ignored, ignored, !shared_unsure),
        ("SigCgt", kernel.caught, caught, !shared_unsure),
    ];
    let mut trap_only = false;
    let mut mismatches = Vec::new();
    for (field, kernel, copy, checked) in fields {
        if !checked {
            continue;
        }
        match compare(kernel, copy) {
            Compared::Same => {}
            Compared::TrapOnly => trap_only = true,
            Compared::Different => mismatches.push(format!(
                "{field} kernel {:#018x} copy {:#018x}",
                kernel & !reserved(),
                copy & !reserved()
            )),
        }
    }
    if !mismatches.is_empty() {
        diverged(guest, after, &mismatches.join(", "));
    } else if trap_only {
        trap_changed(guest, after);
    } else if shared_unsure {
        trace!(
            "[dtid {}] signal-state copy matches the kernel's mask at {after} (actions not \
             compared: a sibling's signal was passed on unreported)",
            guest.thread_state().dettid
        );
    } else {
        if let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() {
            shadow.thread.observe_handler_mask();
        }
        #[cfg(test)]
        CHECKS_MATCHED.with(|count| count.set(count.get() + 1));
        trace!(
            "[dtid {}] signal-state copy matches the kernel at {after}",
            guest.thread_state().dettid
        );
    }
}

#[cfg(test)]
thread_local! {
    /// This test thread's kernel checks that found the copy matching.
    static CHECKS_MATCHED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// This test thread's reported divergences.
    static DIVERGENCES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// This test thread's handler entries taken as returned before any
    /// check saw their mask.
    static UNVERIFIED_ENTRIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// This thread's kernel signal state, where a check or a choice needs it.
/// A thread whose state cannot be read (it may be exiting) gives up its copy
/// without a divergence report, rather than go on unchecked.
fn read_or_give_up<G, T>(guest: &mut G, at: &str) -> Option<super::threads::KernelSignalState>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match read_thread_signal_state(guest.pid(), guest.tid()) {
        Ok(state) => Some(state),
        Err(errno) => {
            debug!(
                "[dtid {}] signal-state copy dropped at {at}: the kernel state is unreadable ({errno})",
                guest.thread_state().dettid
            );
            drop_copy(guest);
            None
        }
    }
}

/// The temporary mask a wait's handler copied in the wait's own turn, the
/// mask the kernel installs; it replaces the one read at the call's entry
/// ([`ThreadSignals::wait_mask`]), which a sibling could change before the
/// turn.
pub(crate) fn note_wait_mask<G, T>(guest: &mut G, mask: KernelSigset)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() {
        shadow.thread.wait_mask = Some(super::signal::kernel_installed_signal_mask(mask));
    }
}

/// Reports a divergence once, by name, drops this thread's copy and marks the
/// actions it shares unknown, so no sharing thread reports the consequence
/// again. Every source of a copy error that host timing could decide gives
/// the copy up quietly first (an unreported delivery, a caught realtime
/// signal, a pending note at a clone), so this line is not host-timed.
fn diverged<G, T>(guest: &mut G, after: &str, what: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    #[cfg(test)]
    DIVERGENCES.with(|count| count.set(count.get() + 1));
    guest.thread_state_mut().signal_shadow_lost = true;
    warn!(
        "[dtid {}] {DIVERGED} at {after}: {what}; this thread's copy is dropped",
        guest.thread_state().dettid
    );
    drop_copy(guest);
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Pid;
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::SyscallInfo;

    use super::*;
    use crate::Config;
    use crate::GlobalState;
    use crate::ThreadState;
    use crate::syscalls::threads::KernelSignalState;
    use crate::syscalls::threads::signal_state_read_seam;
    use crate::types::DetPid;

    /// A guest whose memory is this process's own and whose `/proc` signal
    /// state is scripted; anything else it is asked to do fails the test.
    struct ShadowGuest {
        config: Config,
        thread: ThreadState<()>,
    }

    struct NoStack;
    struct NoStackGuard;

    impl Drop for NoStackGuard {
        fn drop(&mut self) {}
    }

    impl reverie::Stack for NoStack {
        type StackGuard = NoStackGuard;
        fn size(&self) -> usize {
            panic!("recording uses no scratch stack")
        }
        fn capacity(&self) -> usize {
            panic!("recording uses no scratch stack")
        }
        fn push<'stack, T>(&mut self, _: T) -> Addr<'stack, T> {
            panic!("recording uses no scratch stack")
        }
        fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
            panic!("recording uses no scratch stack")
        }
        fn commit(self) -> Result<Self::StackGuard, Errno> {
            Ok(NoStackGuard)
        }
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for ShadowGuest {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            panic!("recording must not send an RPC: {:?}", message.2)
        }
        fn config(&self) -> &Config {
            &self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for ShadowGuest {
        type Memory = LocalMemory;
        type Stack = NoStack;

        fn tid(&self) -> Pid {
            // This test's own thread: the notes of unreported signals are
            // keyed by tid and must not reach another test.
            // SAFETY: gettid has no preconditions.
            Pid::from_raw(unsafe { libc::gettid() })
        }
        fn pid(&self) -> Pid {
            Pid::from_raw(std::process::id() as i32)
        }
        fn ppid(&self) -> Option<Pid> {
            None
        }
        fn memory(&self) -> Self::Memory {
            LocalMemory::new()
        }
        fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
            &mut self.thread
        }
        fn thread_state(&self) -> &ThreadState<()> {
            &self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("recording on a ptrace-like backend reads no registers")
        }
        async fn stack(&mut self) -> Self::Stack {
            NoStack
        }
        async fn daemonize(&mut self) {
            panic!("recording must not daemonize")
        }
        async fn inject<S: SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
            panic!("recording must not inject")
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
            panic!("recording must not retire the guest")
        }
        fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
            panic!("recording must not set a timer")
        }
        fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
            panic!("recording must not set a timer")
        }
        fn read_clock(&mut self) -> Result<u64, reverie::Error> {
            panic!("recording must not read a clock")
        }
    }

    /// The kernel's signal state as the scripted `/proc` reports it.
    #[derive(Clone, Default)]
    struct Kernel(Rc<Cell<(KernelSigset, KernelSigset, KernelSigset)>>);

    impl Kernel {
        fn set(&self, blocked: KernelSigset, ignored: KernelSigset, caught: KernelSigset) {
            self.0.set((blocked, ignored, caught));
        }
        fn install(&self) -> signal_state_read_seam::Installed {
            let state = self.0.clone();
            signal_state_read_seam::install(move |_, _| {
                let (blocked, ignored, caught) = state.get();
                Some(Ok(KernelSignalState {
                    blocked,
                    ignored,
                    caught,
                    ..KernelSignalState::default()
                }))
            })
        }
    }

    /// A ptrace-like guest whose first image inherited `blocked` and nothing
    /// ignored, seeded through `at_exec` from the scripted kernel.
    fn seeded(kernel: &Kernel, blocked: KernelSigset) -> ShadowGuest {
        // The ptrace backend's capabilities: its kernel signal state is the
        // guest's own.
        let config = Config {
            sequentialize_threads: true,
            backend: reverie::BackendCapabilities::PTRACE,
            ..Config::default()
        };
        let thread = ThreadState::new(DetPid::from_raw(1), &config, ());
        let mut guest = ShadowGuest { config, thread };
        kernel.set(blocked, 0, 0);
        at_exec(&mut guest);
        assert!(
            guest.thread.signal_shadow.is_some(),
            "the first image is seeded"
        );
        guest
    }

    fn blocked(guest: &ShadowGuest) -> Option<KernelSigset> {
        guest
            .thread
            .signal_shadow
            .as_ref()
            .map(|shadow| shadow.thread.blocked)
    }

    const USR1: KernelSigset = 1 << (libc::SIGUSR1 - 1);
    const USR2: KernelSigset = 1 << (libc::SIGUSR2 - 1);

    #[test]
    fn a_matching_mask_change_is_recorded_and_checked() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        kernel.set(USR1, 0, 0);
        let matched = CHECKS_MATCHED.with(std::cell::Cell::get);
        // The old mask the kernel wrote: none was blocked.
        let mut old: KernelSigset = 0;
        let oldset = AddrMut::from_raw(&mut old as *mut KernelSigset as usize);
        record_rt_sigprocmask(
            &mut guest,
            libc::SIG_BLOCK,
            SetInput::Validated(USR1),
            oldset,
            &Ok(0),
        );
        assert_eq!(blocked(&guest), Some(USR1));
        assert!(!guest.thread.signal_shadow_lost);
        assert!(
            CHECKS_MATCHED.with(std::cell::Cell::get) > matched,
            "the check ran and matched"
        );
    }

    #[test]
    fn a_wait_mask_lasts_until_the_next_syscall_and_never_holds_sigkill() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let wait_mask = |guest: &ShadowGuest| {
            guest
                .thread
                .signal_shadow
                .as_ref()
                .and_then(|shadow| shadow.thread.wait_mask)
        };
        note_wait_mask(&mut guest, USR1 | (1 << (libc::SIGKILL - 1)));
        assert_eq!(wait_mask(&guest), Some(USR1));
        at_syscall_entry(&mut guest);
        assert_eq!(wait_mask(&guest), None, "the next call is not in the wait");
    }

    #[test]
    fn any_signal_passed_on_unreported_gives_the_copy_up_and_its_note_is_consumed() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let (pid, tid) = (guest.pid().as_raw(), guest.tid().as_raw());
        // The action at delivery cannot be known any more, so even a signal
        // the copy holds as default gives it up.
        note_unreported(pid, tid, libc::SIGURG);
        at_syscall_entry(&mut guest);
        assert!(guest.thread.signal_shadow.is_none());
        assert!(
            !guest.thread.signal_shadow_lost,
            "a quiet drop is seeded again at exec"
        );
        at_exec(&mut guest);
        assert!(guest.thread.signal_shadow.is_some(), "seeded again");
        // A thread without a copy still consumes its notes.
        guest.thread.signal_shadow = None;
        note_unreported(pid, tid, libc::SIGUSR1);
        at_syscall_entry(&mut guest);
        assert!(!with_notes(|notes| notes.iter().any(|(_, t, _)| *t == tid)));
    }

    #[test]
    fn a_match_with_an_outer_frame_is_ambiguous_and_gives_the_copy_up() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let action = |mask| KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask,
        };
        kernel.set(0, 0, USR1 | USR2);
        record_rt_sigaction(&mut guest, libc::SIGUSR1, Some(action(0)), None, &Ok(0));
        record_rt_sigaction(&mut guest, libc::SIGUSR2, Some(action(0)), None, &Ok(0));
        record_delivery(&mut guest, libc::SIGUSR1);
        kernel.set(USR1, 0, USR1 | USR2);
        record_delivery(&mut guest, libc::SIGUSR2);
        // Back to the outermost saved mask in one step.
        kernel.set(0, 0, USR1 | USR2);
        at_syscall_entry(&mut guest);
        assert!(guest.thread.signal_shadow.is_none());
    }

    #[test]
    fn a_silent_drop_leaves_no_sharer_reporting_its_consequence() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let sharer = guest
            .thread
            .signal_shadow
            .as_ref()
            .unwrap()
            .for_clone(libc::CLONE_SIGHAND as u64 | libc::CLONE_VM as u64)
            .unwrap();
        let before = DIVERGENCES.with(std::cell::Cell::get);
        // An ambiguous EFAULT gives the copy up without a report.
        let mut old: KernelSigset = 0;
        let oldset = AddrMut::from_raw(&mut old as *mut KernelSigset as usize);
        record_rt_sigprocmask(
            &mut guest,
            libc::SIG_UNBLOCK,
            SetInput::Unvalidated(Some(USR1)),
            oldset,
            &Err(Errno::EFAULT),
        );
        assert!(guest.thread.signal_shadow.is_none());
        assert!(sharer.dispositions.lock().unwrap().is_none());
        assert_eq!(
            DIVERGENCES.with(std::cell::Cell::get),
            before,
            "nothing was reported"
        );
    }

    #[test]
    fn an_old_action_the_kernel_wrote_differently_is_a_divergence() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        // The kernel says SIGUSR1 was ignored; the copy holds the default.
        let mut written = KernelSigaction {
            handler: 1,
            flags: 0,
            restorer: 0,
            mask: 0,
        };
        let old_action = AddrMut::from_raw(&mut written as *mut KernelSigaction as usize);
        let reported = DIVERGENCES.with(std::cell::Cell::get);
        record_rt_sigaction(&mut guest, libc::SIGUSR1, None, old_action, &Ok(0));
        assert!(guest.thread.signal_shadow.is_none());
        assert!(
            DIVERGENCES.with(std::cell::Cell::get) > reported,
            "reported"
        );
    }

    #[test]
    fn a_mismatch_drops_the_copy_for_good_and_its_sharers_quietly() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let sharer = guest
            .thread
            .signal_shadow
            .as_ref()
            .unwrap()
            .for_clone(libc::CLONE_SIGHAND as u64 | libc::CLONE_VM as u64)
            .unwrap();
        // The kernel says USR2 is blocked; the copy, told USR1, disagrees.
        kernel.set(USR2, 0, 0);
        let reported = DIVERGENCES.with(std::cell::Cell::get);
        record_rt_sigprocmask(
            &mut guest,
            libc::SIG_BLOCK,
            SetInput::Validated(USR1),
            None,
            &Ok(0),
        );
        assert_eq!(blocked(&guest), None, "the copy is dropped");
        assert!(
            DIVERGENCES.with(std::cell::Cell::get) > reported,
            "the divergence was reported"
        );
        assert!(guest.thread.signal_shadow_lost);
        assert!(
            sharer.dispositions.lock().unwrap().is_none(),
            "the actions it shared are unknown now"
        );
        at_exec(&mut guest);
        assert!(
            guest.thread.signal_shadow.is_none(),
            "a lost copy is never reseeded"
        );
    }

    #[test]
    fn an_efault_counts_only_when_the_kernel_ran_the_call_and_the_fault_was_the_old_masks() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let mut old: KernelSigset = 0;
        let oldset = AddrMut::from_raw(&mut old as *mut KernelSigset as usize);
        // Detcore's own check of the set failed: nothing ran.
        record_rt_sigprocmask(
            &mut guest,
            libc::SIG_BLOCK,
            SetInput::NotRun,
            oldset,
            &Err(Errno::EFAULT),
        );
        assert_eq!(blocked(&guest), Some(0));
        // A validated set whose old-mask write failed: the change happened.
        kernel.set(USR1, 0, 0);
        record_rt_sigprocmask(
            &mut guest,
            libc::SIG_BLOCK,
            SetInput::Validated(USR1),
            oldset,
            &Err(Errno::EFAULT),
        );
        assert_eq!(blocked(&guest), Some(USR1));
        // An unvalidated set without an old mask: the set faulted.
        record_rt_sigprocmask(
            &mut guest,
            libc::SIG_UNBLOCK,
            SetInput::Unvalidated(Some(USR1)),
            None,
            &Err(Errno::EFAULT),
        );
        assert_eq!(blocked(&guest), Some(USR1));
        // With an old mask, either could have faulted: the copy is given up,
        // without a divergence report.
        record_rt_sigprocmask(
            &mut guest,
            libc::SIG_UNBLOCK,
            SetInput::Unvalidated(Some(USR1)),
            oldset,
            &Err(Errno::EFAULT),
        );
        assert_eq!(blocked(&guest), None);
        assert!(!guest.thread.signal_shadow_lost, "a quiet drop");
    }

    #[test]
    fn a_delivery_inside_a_wait_follows_the_mask_the_kernel_holds() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let action = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        let int = 1 << (libc::SIGINT - 1);
        for kernel_ran_the_wait in [true, false] {
            let mut guest = seeded(&kernel, USR1 | USR2);
            kernel.set(USR1 | USR2, 0, 1 << (libc::SIGINT - 1));
            record_rt_sigaction(&mut guest, libc::SIGINT, Some(action), None, &Ok(0));
            // rt_sigsuspend(&{USR1}).
            let temporary: KernelSigset = USR1;
            at_syscall_entry(&mut guest);
            // The wait's handler copies its mask in the call's turn.
            note_wait_mask(&mut guest, temporary);
            // The delivery stop: the kernel holds the wait's mask if it ran the
            // call, else the thread's own (the not-run case of
            // record_or_replay_rt_sigsuspend).
            kernel.set(
                if kernel_ran_the_wait {
                    USR1
                } else {
                    USR1 | USR2
                },
                0,
                int,
            );
            record_delivery(&mut guest, libc::SIGINT);
            let shadow = guest.thread.signal_shadow.as_ref().unwrap();
            let expected = if kernel_ran_the_wait {
                USR1
            } else {
                USR1 | USR2
            };
            assert_eq!(
                shadow.thread.blocked,
                expected | int,
                "{kernel_ran_the_wait}"
            );
            assert_eq!(
                shadow.thread.activations,
                vec![USR1 | USR2],
                "the frame saves the thread's own mask either way"
            );
        }
    }

    #[test]
    fn an_unreported_return_is_recognised_at_the_next_syscall() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let action = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: USR2,
        };
        kernel.set(0, 0, USR1);
        record_rt_sigaction(&mut guest, libc::SIGUSR1, Some(action), None, &Ok(0));
        record_delivery(&mut guest, libc::SIGUSR1);
        assert_eq!(blocked(&guest), Some(USR1 | USR2));
        // A syscall inside the handler: the kernel holds its mask.
        kernel.set(USR1 | USR2, 0, USR1);
        at_syscall_entry(&mut guest);
        assert_eq!(blocked(&guest), Some(USR1 | USR2));
        // The handler returned unseen; the next syscall finds the old mask.
        kernel.set(0, 0, USR1);
        at_syscall_entry(&mut guest);
        assert_eq!(blocked(&guest), Some(0));
        // With a frame open, a mask that is neither could be a rewritten
        // uc_sigmask: given up quietly, not reported.
        record_delivery(&mut guest, libc::SIGUSR1);
        kernel.set(USR2, 0, USR1);
        let reported = DIVERGENCES.with(std::cell::Cell::get);
        at_syscall_entry(&mut guest);
        assert_eq!(blocked(&guest), None);
        assert_eq!(DIVERGENCES.with(std::cell::Cell::get), reported);
    }

    #[test]
    fn a_sigtrap_change_gives_the_copy_up_quietly() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let trap = 1 << (libc::SIGTRAP - 1);
        let mut guest = seeded(&kernel, trap);
        let reported = DIVERGENCES.with(std::cell::Cell::get);
        // The kernel unblocked SIGTRAP behind the guest's back (a single-step).
        kernel.set(0, 0, 0);
        let mut old: KernelSigset = 0;
        let oldset = AddrMut::from_raw(&mut old as *mut KernelSigset as usize);
        record_rt_sigprocmask(&mut guest, libc::SIG_BLOCK, SetInput::None, oldset, &Ok(0));
        assert!(
            guest.thread.signal_shadow.is_none(),
            "the copy is unknown now"
        );
        assert_eq!(
            DIVERGENCES.with(std::cell::Cell::get),
            reported,
            "and nothing was reported"
        );
    }

    #[test]
    fn the_preemption_signal_is_left_out_of_old_values() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        // The kernel's mask holds Reverie's own signal; the guest never sees it.
        let perf = 1 << (reverie::PERF_EVENT_SIGNAL as i32 - 1);
        kernel.set(perf, 0, perf);
        let mut old: KernelSigset = perf;
        let oldset = AddrMut::from_raw(&mut old as *mut KernelSigset as usize);
        record_rt_sigprocmask(&mut guest, libc::SIG_BLOCK, SetInput::None, oldset, &Ok(0));
        assert!(guest.thread.signal_shadow.is_some());
    }

    #[test]
    fn a_wait_mask_from_sigfillset_matches_what_the_kernel_installs() {
        // sigfillset sets SIGKILL and SIGSTOP; Linux never installs them.
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let int = 1 << (libc::SIGINT - 1);
        let action = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        let filled: KernelSigset = !(int | (1 << 31) | (1 << 32));
        let installed = filled & !((1 << (libc::SIGKILL - 1)) | (1 << (libc::SIGSTOP - 1)));
        for kernel_ran_the_wait in [true, false] {
            let mut guest = seeded(&kernel, USR1);
            kernel.set(USR1, 0, int);
            record_rt_sigaction(&mut guest, libc::SIGINT, Some(action), None, &Ok(0));
            at_syscall_entry(&mut guest);
            // The wait's handler copies its mask in the call's turn.
            note_wait_mask(&mut guest, filled);
            let at_stop = if kernel_ran_the_wait { installed } else { USR1 };
            kernel.set(at_stop, 0, int);
            record_delivery(&mut guest, libc::SIGINT);
            let shadow = guest.thread.signal_shadow.as_ref().unwrap();
            assert_eq!(
                shadow.thread.blocked,
                at_stop | int,
                "{kernel_ran_the_wait}"
            );
            assert_eq!(shadow.thread.activations, vec![USR1]);
        }
    }

    #[test]
    fn an_old_mask_the_kernel_wrote_differently_is_a_divergence() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        // The kernel says the old mask was USR2; the copy holds 0.
        let mut old: KernelSigset = USR2;
        let oldset = AddrMut::from_raw(&mut old as *mut KernelSigset as usize);
        let reported = DIVERGENCES.with(std::cell::Cell::get);
        record_rt_sigprocmask(&mut guest, libc::SIG_BLOCK, SetInput::None, oldset, &Ok(0));
        assert_eq!(blocked(&guest), None);
        assert!(
            DIVERGENCES.with(std::cell::Cell::get) > reported,
            "reported"
        );
    }

    #[test]
    fn rt_sigaction_installs_even_when_the_old_action_cannot_be_written() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let action = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        kernel.set(0, 0, USR1);
        record_rt_sigaction(
            &mut guest,
            libc::SIGUSR1,
            Some(action),
            None,
            &Err(Errno::EFAULT),
        );
        let shadow = guest.thread.signal_shadow.as_ref().unwrap();
        let caught = shadow
            .dispositions
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .caught();
        assert_eq!(caught, USR1);
    }

    #[test]
    fn a_handler_entry_no_check_saw_gives_the_copy_up_when_taken_as_returned() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let action = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: USR2,
        };
        // Seen first, then returned: kept.
        let mut guest = seeded(&kernel, 0);
        kernel.set(0, 0, USR1);
        record_rt_sigaction(&mut guest, libc::SIGUSR1, Some(action), None, &Ok(0));
        record_delivery(&mut guest, libc::SIGUSR1);
        kernel.set(USR1 | USR2, 0, USR1);
        at_syscall_entry(&mut guest);
        kernel.set(0, 0, USR1);
        let before = UNVERIFIED_ENTRIES.with(std::cell::Cell::get);
        at_syscall_entry(&mut guest);
        assert!(guest.thread.signal_shadow.is_some());
        assert_eq!(UNVERIFIED_ENTRIES.with(std::cell::Cell::get), before);
        // The copy predicts USR1|USR2 for the handler, but the kernel shows
        // the saved mask at once: a return, or a wrong prediction. The copy
        // cannot tell, so it is given up, quietly.
        let reported = DIVERGENCES.with(std::cell::Cell::get);
        record_delivery(&mut guest, libc::SIGUSR1);
        at_syscall_entry(&mut guest);
        assert_eq!(UNVERIFIED_ENTRIES.with(std::cell::Cell::get), before + 1);
        assert!(guest.thread.signal_shadow.is_none());
        assert_eq!(DIVERGENCES.with(std::cell::Cell::get), reported);
    }

    #[test]
    fn installing_a_caught_realtime_handler_gives_the_copy_up() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let action = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        let reported = DIVERGENCES.with(std::cell::Cell::get);
        record_rt_sigaction(&mut guest, 40, Some(action), None, &Ok(0));
        assert!(guest.thread.signal_shadow.is_none());
        assert_eq!(DIVERGENCES.with(std::cell::Cell::get), reported);
    }

    #[test]
    fn a_siblings_note_makes_checks_compare_only_the_mask() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        // This test thread's id stands for a live sibling of the guest.
        let sibling = guest.tid().as_raw();
        guest.thread.signal_shadow.as_mut().unwrap().thread.blocked = 0;
        let action = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000 | libc::SA_RESETHAND) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        kernel.set(0, 0, USR1);
        record_rt_sigaction(&mut guest, libc::SIGUSR1, Some(action), None, &Ok(0));
        // The sibling's note: pretend it belongs to another live thread by
        // keying it to this one and asking as a different tid.
        note_unreported(guest.pid().as_raw(), sibling, libc::SIGUSR1);
        assert!(unreported_elsewhere(sibling + 1));
        assert!(
            !unreported_elsewhere(sibling),
            "a thread's own note is not a sibling's"
        );
    }

    #[test]
    fn notes_of_threads_that_are_gone_are_dropped() {
        // No live thread has this tid (pid_max is far below it).
        note_unreported(1, i32::MAX - 1, libc::SIGUSR1);
        assert!(!unreported_elsewhere(1));
        assert!(!unreported_pending());
    }

    #[test]
    fn nothing_is_seeded_without_serialized_threads() {
        // Outside tests the copy also needs this variable set to `1`.
        assert_eq!(SIGNAL_STATE_CHECK_ENV, "HERMIT_SIGNAL_STATE_CHECK");
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let config = Config {
            sequentialize_threads: false,
            backend: reverie::BackendCapabilities::PTRACE,
            ..Config::default()
        };
        let thread = ThreadState::new(DetPid::from_raw(1), &config, ());
        let mut guest = ShadowGuest { config, thread };
        at_exec(&mut guest);
        assert!(guest.thread.signal_shadow.is_none());
    }

    #[test]
    fn nothing_is_seeded_where_the_kernel_state_is_not_the_guests() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let config = Config {
            sequentialize_threads: true,
            backend: reverie::BackendCapabilities::KVM,
            ..Config::default()
        };
        let thread = ThreadState::new(DetPid::from_raw(1), &config, ());
        let mut guest = ShadowGuest { config, thread };
        at_exec(&mut guest);
        assert!(guest.thread.signal_shadow.is_none());
    }
}
