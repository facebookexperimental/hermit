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
//! while the copy matches (one TRACE line per check, for counting). The
//! scheduler learns the copy's dispositions separately, from
//! [`publish_dispositions`], a message that carries no logical time.
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
use serde::Deserialize;
use serde::Serialize;
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
/// checks it, and lets the decisions A5 step 2 converted read it; unset (the
/// default) leaves every run as it was. The ptrace backend reports
/// handler returns to it (`Tool::handle_sigreturn`), but not every delivery,
/// and asking for the reports adds a stop at each handler return, so it is
/// opt-in.
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

/// The signals Linux force-delivers (`force_sig_info_to_task`): forcing one
/// that is blocked or ignored, or caught but blocked, unblocks it and resets
/// its action to the default, for the whole group sharing the actions,
/// before any ptrace stop. The copy sees none of these:
/// - SIGTRAP from the ptrace backend's single-step (Reverie characterizes it
///   in `sigtrap_profile_follows_each_handler_shape`);
/// - SIGSEGV from the backend's own trapped `rdtsc` or `cpuid`, which it
///   emulates and suppresses;
/// - a guest's own fault (SIGSEGV, SIGBUS, SIGILL, SIGFPE), and SIGSYS from a
///   guest's seccomp `RET_TRAP` filter.
///
/// So a difference confined to these signals makes the copy unknown (it is
/// given up quietly) rather than reported, and never passes as a match.
fn forced() -> KernelSigset {
    [
        libc::SIGTRAP,
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGILL,
        libc::SIGFPE,
        libc::SIGSYS,
    ]
    .into_iter()
    .fold(0, |set, signal| set | sigbit(signal))
}

/// What comparing one of the copy's sets with the kernel's found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Compared {
    Same,
    /// Only signals Linux force-delivers differ ([`forced`]).
    ForcedOnly,
    Different,
}

/// A forced delivery only ever clears a signal's bit (it unblocks the
/// signal and resets its action to the default), so a difference is
/// [`Compared::ForcedOnly`] only when every differing bit is a signal Linux
/// force-delivers and the kernel holds none the copy lacks. Anything else is
/// a real difference.
fn compare(kernel: KernelSigset, copy: KernelSigset) -> Compared {
    match (kernel ^ copy) & !reserved() {
        0 => Compared::Same,
        difference if difference & !forced() == 0 && kernel & !copy & !reserved() == 0 => {
            Compared::ForcedOnly
        }
        _ => Compared::Different,
    }
}

/// Gives the copy up quietly: the kernel's state of a signal Linux
/// force-delivers differs from it in a way only a forced delivery explains
/// ([`forced`]).
fn forced_changed<G, T>(guest: &mut G, at: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    debug!(
        "[dtid {}] signal-state copy dropped at {at}: a force-delivered signal's kernel state \
         differs (SIGTRAP from a ptrace single-step, SIGSEGV from a trapped rdtsc or cpuid, \
         or a fault)",
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
    drop_thread_copy(guest.thread_state_mut());
}

/// [`drop_copy`] given only the thread's state.
fn drop_thread_copy<T>(thread: &mut crate::tool_local::ThreadState<T>) {
    if let Some(shadow) = thread.signal_shadow.as_ref() {
        shadow.forget_dispositions();
    }
    thread.signal_shadow = None;
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
/// thread's next operation, whether or not it has a copy. Realtime signals
/// are shown and recorded like any other
/// (<https://github.com/rrnewton/reverie/issues/207>).
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

/// Before a transition that depends on the mask: where the backend reports
/// `rt_sigreturn`, checks that the kernel holds the mask the copy predicts
/// (with a handler frame open, or after a report); elsewhere, accounts for
/// handler returns the kernel ran without a report, from the kernel's
/// current mask. Does nothing otherwise, or where the kernel's state is not
/// the guest's.
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
    let verify = guest
        .thread_state()
        .signal_shadow
        .as_ref()
        .is_some_and(|shadow| shadow.thread.verify_at_entry);
    if !(open_frames || verify) || !kernel_state_is_the_guests(guest) {
        return;
    }
    let Some(kernel) = read_or_give_up(guest, before) else {
        return;
    };
    // Where the backend reports rt_sigreturn, frames close only on its
    // reports, so nothing is inferred. The kernel must still hold the mask
    // the copy predicts, for an open handler or after a report; otherwise a
    // report was missed (the capability names the calls it does not
    // report), the kernel did not follow one, or the handler's frame was
    // never written, and the copy fails closed.
    if observes_sigreturns(guest.config()) {
        let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() else {
            return;
        };
        shadow.thread.verify_at_entry = false;
        match compare(kernel.blocked, shadow.thread.blocked) {
            Compared::Same => shadow.thread.observe_handler_mask(),
            Compared::ForcedOnly => forced_changed(guest, before),
            Compared::Different => give_up(
                guest,
                "the kernel's mask is not the one the copy predicts after a reported return or \
                 for an open handler frame",
            ),
        }
        return;
    }
    let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() else {
        return;
    };
    let copy = shadow.thread.blocked;
    let open = shadow.thread.activations.len();
    let innermost_observed = shadow.thread.innermost_observed();
    if shadow
        .thread
        .reconcile_returns(kernel.blocked, reserved() | forced())
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
            // The frames were matched with the forced signals left out; the
            // whole mask must still be the copy's, or differ only as a forced
            // delivery leaves it.
            match blocked.map(|blocked| compare(kernel.blocked, blocked)) {
                Some(Compared::ForcedOnly) => forced_changed(guest, before),
                Some(Compared::Different) => give_up(
                    guest,
                    "the kernel's mask holds a force-delivered signal the copy's mask does not",
                ),
                Some(Compared::Same) | None => {}
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
    // A nonleader exec takes the leader's tid, whose entry the scheduler
    // drops with the old leader, and the copy may not survive the exec: what
    // the scheduler holds is unknown, so the mask, or its withdrawal, is
    // published again.
    guest.thread_state_mut().published_mask = None;
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
        // A forced delivery changes only the handler, to the default. Whether
        // the signal was blocked when it was forced is not knowable here (see
        // `check_against_kernel`).
        if forced() & sigbit(signum) != 0
            && Action::from(written)
                == (Action {
                    handler: libc::SIG_DFL as u64,
                    ..old
                })
            && old.handler != libc::SIG_DFL as u64
        {
            return forced_changed(guest, "rt_sigaction");
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
            Compared::ForcedOnly => return forced_changed(guest, "rt_sigprocmask"),
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
    let compared = !(reserved() | forced());
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
    // The outcome was chosen with the forced signals left out. A forced
    // delivery's clear gives the copy up now; any other difference is left to
    // the next check, which reports it.
    if compare(kernel.blocked, shadow.thread.blocked) == Compared::ForcedOnly {
        forced_changed(guest, "signal delivery");
    }
}

/// The guest's dispositions as Detcore's copy holds them, in the kernel's bit
/// layout: the signals it ignores and the signals it catches.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CopiedDispositions {
    /// Signals whose action is `SIG_IGN`.
    pub(crate) ignored: KernelSigset,
    /// Signals with a handler.
    pub(crate) caught: KernelSigset,
}

/// A process's record of its publications to the scheduler
/// ([`publish_dispositions`]), shared by its threads. Each publication takes
/// the next generation under this record's lock, with the copy read under the
/// same lock, so a later generation always carries a later reading of the
/// copy. The scheduler keeps the latest generation it received, in whatever
/// order publications arrive, and stores this same type per process.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublishedDispositions {
    /// The latest generation a thread of the process used, or the scheduler
    /// was found to hold; 0 before the first.
    pub(crate) generation: u64,
    /// The latest generation the scheduler held when it answered one of this
    /// process's publications: the one `dispositions` came from. Unused in
    /// the scheduler's own entry.
    #[serde(default)]
    pub(crate) accepted: u64,
    /// The dispositions the scheduler holds; `None` when it holds none.
    pub(crate) dispositions: Option<CopiedDispositions>,
}

/// The scheduler's answer to a publication
/// (`Scheduler::publish_signal_dispositions`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublicationAnswer {
    /// The generation the scheduler holds for the process afterwards.
    pub(crate) held: u64,
    /// Whether this publication is what it holds now: its generation was
    /// later than any the scheduler held.
    pub(crate) taken: bool,
}

/// This thread's dispositions from Detcore's copy, for a decision that needs
/// only them (A5 step 2a). `None` where the copy cannot answer: none is kept,
/// the actions it shares are unknown, or an unreported signal is noted
/// anywhere (a handler may have changed the actions unseen). The caller then
/// reads the kernel's `/proc` view as before. Where the copy answers, it holds
/// what the kernel holds: it is checked against the kernel after each
/// recorded call and gives itself up on any mismatch.
///
/// The bits of the signals Linux force-delivers ([`forced`]) and of the
/// preemption signal are not modelled exactly (their kernel state changes
/// unseen), so a decision about one of those signals uses
/// [`copied_dispositions_for`].
pub(crate) fn copied_dispositions<G, T>(guest: &G) -> Option<CopiedDispositions>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if any_notes() {
        return None;
    }
    table_dispositions(guest.thread_state())
}

/// The dispositions `thread`'s copy holds for the actions it shares, whether
/// or not an unreported signal is noted: `None` without a copy, or with the
/// shared actions unknown.
fn table_dispositions<T>(thread: &crate::tool_local::ThreadState<T>) -> Option<CopiedDispositions> {
    let shadow = thread.signal_shadow.as_ref()?;
    let actions = shadow.dispositions.lock().unwrap();
    let dispositions = actions.as_ref()?;
    Some(CopiedDispositions {
        ignored: dispositions.ignored(),
        caught: dispositions.caught(),
    })
}

/// [`copied_dispositions`] for a decision about `signal` alone: also `None`
/// for a signal Linux force-delivers ([`forced`]) and for Reverie's
/// preemption signal, whose kernel state changes without the copy seeing it.
pub(crate) fn copied_dispositions_for<G, T>(guest: &G, signal: i32) -> Option<CopiedDispositions>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if !models(signal) {
        return None;
    }
    copied_dispositions(guest)
}

/// Whether the copy holds `signal`'s state exactly: it is neither a signal
/// Linux force-delivers ([`forced`]) nor Reverie's preemption signal
/// ([`reserved`]), whose kernel state changes without the copy seeing it.
fn models(signal: i32) -> bool {
    (forced() | reserved()) & sigbit(signal) == 0
}

/// The publication this thread has to make for its process
/// ([`publish_dispositions`]), with the next generation, taken under the
/// record's lock with the copy read under the same lock: the copy's
/// dispositions, or `None` to withdraw them, because it has no copy or the
/// shared actions became unknown. Nothing when the scheduler already holds
/// that for the process, and where no copy is kept.
fn next_publication<G, T>(guest: &G) -> Option<(u64, Option<CopiedDispositions>)>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if !kernel_state_is_the_guests(guest) {
        return None;
    }
    let mut record = guest.thread_state().published_dispositions.lock().unwrap();
    let current = table_dispositions(guest.thread_state());
    if current == record.dispositions {
        return None;
    }
    record.generation += 1;
    Some((record.generation, current))
}

/// Applies the scheduler's `answer` to publication `generation` of `current`,
/// and returns whether to publish again at once.
/// - Taken, and no later answer was applied: the record takes its
///   dispositions.
/// - Not taken: the scheduler holds a generation at least as late, from a
///   later publication of this process or from an entry this process never
///   made. The record carries on after it, keeping its dispositions, and the
///   thread publishes again at once, before its request can let another
///   thread read the entry.
/// - Refused (`None`, a thread the scheduler does not know): nothing changes,
///   and the thread publishes again at its next request.
fn settle_publication(
    record: &mut PublishedDispositions,
    generation: u64,
    current: Option<CopiedDispositions>,
    answer: Option<PublicationAnswer>,
) -> bool {
    let Some(answer) = answer else {
        return false;
    };
    if answer.taken {
        if generation > record.accepted {
            record.accepted = generation;
            record.dispositions = current;
        }
        return false;
    }
    if answer.held > record.generation {
        record.generation = answer.held;
    }
    true
}

/// Publishes this thread's copied dispositions to the scheduler as its
/// process's (A5 step 2b), where they differ from what the scheduler holds
/// for it (`ThreadState::published_dispositions`), so that a decision about another thread reads them in
/// place of the kernel's `/proc` view (`Scheduler::published_dispositions_for`).
///
/// Called before each of the thread's requests that can let another thread
/// run (`tool_global::send_and_update_time`). Every change to the actions is
/// recorded in the operations of the thread that makes it, which publishes it
/// before its next request, so the scheduler holds the new actions before any
/// other thread can read them. Threads that do not hold the turn publish too
/// (a thread rejoining the schedule after a host call, a new thread's start,
/// a control message at a signal stop), so publications can arrive out of
/// order: each carries a generation ([`PublishedDispositions`]) and the
/// scheduler keeps the latest. A thread the scheduler has not registered yet
/// is refused, and publishes again at its next request
/// ([`settle_publication`]).
///
/// Every thread sharing the actions holds them behind one reference, so a
/// thread publishes them as they are now, whoever changed them. (After an
/// exec, the old image's other threads keep the old actions until the kernel
/// has killed them and their teardown runs; they make no request in
/// between.) The scheduler keys them by process, which is right because the
/// actions of a process are never shared with another one: [`before_clone`]
/// makes them unknown first.
///
/// That the scheduler's entry is never older than the copy rests on two
/// facts: where a copy is kept (ptrace), a publication is answered inline,
/// within the sender's own call, so it cannot be delayed past its sender;
/// and backends whose messages could be delayed keep no copy. The
/// generations, the scheduler's refusal of retired incarnations, and the
/// immediate republication after an answer that did not take it, guard
/// against an entry that should not be there all the same.
pub(crate) async fn publish_dispositions<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // A publication that was not taken is made again once, with a
    // generation past the one the scheduler holds, so it is taken.
    for _ in 0..2 {
        let Some((generation, current)) = next_publication(guest) else {
            return;
        };
        let answer =
            crate::tool_global::publish_signal_dispositions(guest, generation, current).await;
        trace!(
            "[dtid {}] signal-state copy: published its process's dispositions {:?} (generation {}, answer: {:?})",
            guest.thread_state().dettid,
            current,
            generation,
            answer
        );
        let mut record = guest.thread_state().published_dispositions.lock().unwrap();
        if !settle_publication(&mut record, generation, current, answer) {
            return;
        }
    }
}

/// Whether dispositions a process published ([`publish_dispositions`])
/// decide about `signal` now: the copy holds it exactly ([`models`]), and no
/// unreported signal is noted, whose handler may have changed the actions
/// unseen (`SA_RESETHAND`) until its thread consumes the note at its next
/// syscall and gives its copy up.
pub(crate) fn published_dispositions_decide(signal: i32) -> bool {
    models(signal) && !any_notes()
}

/// The mask this thread publishes to the scheduler (A5 step 2b,
/// [`publish_mask`]): the copy's `blocked`, where that is the mask the kernel
/// holds for the thread whenever it is stopped at a request. `None` (nothing
/// to publish) without a copy, and while it may not be:
/// - inside a temporary-mask wait (`wait_mask`), whose mask the kernel may
///   have installed, or with an interrupted wait's mask still to put back
///   (`saved`);
/// - before the next syscall entry verifies a mask a backend report set
///   (`verify_at_entry`);
/// - with a handler frame open where its return would go unseen
///   (`observes_returns` false), or whose entry no check has seen yet (the
///   kernel keeps the old mask when it cannot write the frame).
///
/// A mask Detcore installs privately is not this one either. While one is
/// installed the scheduler reads no published mask: discard at send, poll
/// admission and real-time timer fate take the guest's mask from
/// `Scheduler::saved_guest_sigmasks`, and the choice of a process-directed
/// signal's thread and the first SIGCHLD taker read the kernel's.
fn mask_to_publish<T>(
    thread: &crate::tool_local::ThreadState<T>,
    observes_returns: bool,
) -> Option<KernelSigset> {
    let masks = &thread.signal_shadow.as_ref()?.thread;
    let uncertain = masks.wait_mask.is_some()
        || masks.saved.is_some()
        || masks.verify_at_entry
        || (!masks.activations.is_empty() && !(observes_returns && masks.innermost_observed()));
    (!uncertain).then_some(masks.blocked)
}

/// Publishes this thread's mask from Detcore's copy to the scheduler
/// ([`mask_to_publish`]), or withdraws it, where that differs from what it
/// last had taken or what the scheduler holds is unknown
/// (`ThreadState::published_mask`), so that a decision about
/// this thread reads it in place of the kernel's `SigBlk`
/// (`Scheduler::published_mask_for`). Called with [`publish_dispositions`],
/// before each request that can let another thread run. A mask is the
/// thread's own and only the thread changes or publishes it, so its
/// publications cannot overtake each other. A thread the scheduler has not
/// registered yet is refused, and publishes again at its next request.
pub(crate) async fn publish_mask<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if !kernel_state_is_the_guests(guest) {
        return;
    }
    let current = mask_to_publish(guest.thread_state(), observes_sigreturns(guest.config()));
    if guest.thread_state().published_mask == Some(current) {
        return;
    }
    let taken = crate::tool_global::publish_signal_mask(guest, current).await;
    trace!(
        "[dtid {}] signal-state copy: published its mask {:?} (taken: {})",
        guest.thread_state().dettid,
        current.map(|mask| format!("{mask:#x}")),
        taken
    );
    if taken {
        guest.thread_state_mut().published_mask = Some(current);
    }
}

/// Before this thread runs a clone with `flags`: a child that shares the
/// actions from another process (`CLONE_SIGHAND` without `CLONE_THREAD`)
/// could change them where the scheduler, which keeps the published
/// dispositions per process, would not see it for this one. The actions
/// become unknown to every thread sharing them, and are withdrawn before the
/// child exists, also for a clone that then fails. Such a child gets no copy
/// (`SignalShadow::for_clone`).
pub(crate) async fn before_clone<G, T>(guest: &mut G, flags: u64)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let shares_actions = flags & libc::CLONE_SIGHAND as u64 != 0;
    let new_process = flags & libc::CLONE_THREAD as u64 == 0;
    if shares_actions
        && new_process
        && let Some(shadow) = guest.thread_state().signal_shadow.as_ref()
    {
        debug!(
            "[dtid {}] signal-state copy: the actions become unknown, a clone shares them with a new process",
            guest.thread_state().dettid
        );
        shadow.forget_dispositions();
    }
    publish_dispositions(guest).await;
}

/// Whether this tool asks the backend to report every guest `rt_sigreturn`
/// (`Tool::observes_sigreturns`): where the copy is kept (serialized
/// threads), the backend can report them, no gdbserver is attached (the
/// ptrace backend does not report then), and the guest is known to inherit
/// no seccomp filter. An inherited filter could let an `rt_sigreturn` run
/// unreported (`BackendCapabilities::reports_sigreturns`) where nothing would
/// check the copy before a later check reports a divergence. Handler frames then close only on those reports
/// ([`record_sigreturn_report`]), not by inference ([`reconcile_returns`]).
///
/// Not under `--imprecise-timers` either. A reported return within the keep
/// margin of a precise preemption event requests it again at the same target
/// (Reverie `Timer::disregard_sigreturn_stop`), but an imprecise one past its
/// period is cancelled, and nothing would preempt a thread that then runs
/// without a syscall (A5 step 2c). Without the reports, the copy infers
/// returns from the kernel's mask, as it does wherever they are off.
pub(crate) fn observes_sigreturns(config: &crate::Config) -> bool {
    enabled()
        && config.sequentialize_threads
        && !config.gdbserver
        && !config.imprecise_timers
        && config.backend.reports_sigreturns
        && config.inherited_seccomp == detcore_model::config::InheritedSeccomp::Disabled
}

/// `sizeof(struct rt_sigframe)` on x86-64: the return address (8), the
/// `ucontext` (304) and the `siginfo` (128).
const RT_SIGFRAME_SIZE: u64 = 8 + 304 + 128;

/// The end of the x86-64 user address space under 4-level paging
/// (`TASK_SIZE_MAX`). Under 5-level paging it is higher.
const USER_ADDRESS_LIMIT_4_LEVEL: u64 = 0x7fff_ffff_f000;

/// Records a guest `rt_sigreturn` the backend reported before it runs
/// (`Tool::handle_sigreturn`). x86-64 `rt_sigreturn` installs the frame's
/// mask once `access_ok` accepts the whole frame at `sp - 8` and the frame's
/// `uc_sigmask` and `uc_flags` read without a fault, before restoring the
/// registers can fail (`arch/x86/kernel/signal_64.c`): the innermost handler
/// frame then closes and the mask is installed. A frame that fails either
/// (an overflowing range, or an unmapped word) changes no mask; the kernel
/// then sends SIGSEGV. The copy gives itself up quietly where it cannot tell
/// what Linux does: a frame that reaches past 4-level paging's user limit,
/// which the paging mode accepts or not, and a protected frame, whose
/// protection decides whether the kernel's read faults. For every report
/// that keeps the copy, the next syscall entry compares the copy with the
/// kernel ([`reconcile_returns`]), so a report the kernel did not follow (a
/// protection key, say) fails closed there.
pub(crate) fn record_sigreturn_report<T>(
    thread: &mut crate::tool_local::ThreadState<T>,
    report: reverie::SigreturnReport,
) {
    let dettid = thread.dettid;
    if thread.signal_shadow.is_none() {
        return;
    }
    let frame_end = report
        .stack_pointer()
        .checked_sub(8)
        .and_then(|frame| frame.checked_add(RT_SIGFRAME_SIZE));
    let installed = match (report.frame(), frame_end) {
        (reverie::SigreturnFrame::Readable { mask }, Some(end))
            if end <= USER_ADDRESS_LIMIT_4_LEVEL =>
        {
            Some(mask)
        }
        (reverie::SigreturnFrame::Readable { .. }, Some(_)) => {
            debug!(
                "[dtid {dettid}] signal-state copy dropped: rt_sigreturn's frame reaches past \
                 the 4-level user address limit"
            );
            return drop_thread_copy(thread);
        }
        (reverie::SigreturnFrame::Protected, Some(_)) => {
            debug!(
                "[dtid {dettid}] signal-state copy dropped: rt_sigreturn's frame is mapped but \
                 protected from the guest"
            );
            return drop_thread_copy(thread);
        }
        _ => None,
    };
    let Some(shadow) = thread.signal_shadow.as_mut() else {
        return;
    };
    shadow.thread.verify_at_entry = true;
    match installed {
        Some(mask) => {
            shadow.thread.sigreturn(mask & !reserved());
            #[cfg(test)]
            REPORTED_RETURNS.with(|count| count.set(count.get() + 1));
            trace!(
                "[dtid {dettid}] signal-state copy: rt_sigreturn reported, installing {:#x}",
                shadow.thread.blocked
            );
        }
        None => {
            trace!("[dtid {dettid}] signal-state copy: rt_sigreturn's frame faults, no mask change")
        }
    }
}

/// For a backend that shows `rt_sigreturn` as a syscall, which none does
/// today; the ptrace backend reports it through [`record_sigreturn_report`].
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
    // Linux resets a caught action only for a signal that was blocked when it
    // was forced. That mask is not this check's: the guest may have blocked or
    // unblocked the signal since, unseen by the reset, and a sibling sharing
    // the actions may have taken it. So a caught bit the kernel cleared counts
    // as forced on its direction alone (`compare`).
    let fields = [
        ("SigBlk", kernel.blocked, blocked, true),
        ("SigIgn", kernel.ignored, ignored, !shared_unsure),
        ("SigCgt", kernel.caught, caught, !shared_unsure),
    ];
    let mut forced_only = false;
    let mut mismatches = Vec::new();
    for (field, kernel, copy, checked) in fields {
        if !checked {
            continue;
        }
        match compare(kernel, copy) {
            Compared::Same => {}
            Compared::ForcedOnly => forced_only = true,
            Compared::Different => mismatches.push(format!(
                "{field} kernel {:#018x} copy {:#018x}",
                kernel & !reserved(),
                copy & !reserved()
            )),
        }
    }
    if !mismatches.is_empty() {
        diverged(guest, after, &mismatches.join(", "));
    } else if forced_only {
        forced_changed(guest, after);
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
    /// This test thread's `rt_sigreturn` reports applied to a copy.
    static REPORTED_RETURNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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
/// the copy up quietly first (an unreported delivery, a pending note at a
/// clone), so this line is not host-timed.
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

    /// A guest on the ptrace backend, whose first image inherited `blocked`
    /// and nothing ignored, seeded through `at_exec` from the scripted
    /// kernel. Its kernel signal state is the guest's own, and it reports
    /// each `rt_sigreturn`.
    fn seeded(kernel: &Kernel, blocked: KernelSigset) -> ShadowGuest {
        seeded_on(kernel, blocked, reverie::BackendCapabilities::PTRACE)
    }

    /// As [`seeded`], with the capabilities of a backend that runs
    /// `rt_sigreturn` without reporting it, so the copy infers handler
    /// returns: Reverie's `E9PATCH` value, ptrace's facts without sigreturn
    /// reports. No Hermit run has these capabilities (Hermit runs e9patch
    /// preprocessing as a ptrace run, which reports), so this exercises the
    /// inference that a backend without reports would need.
    fn seeded_inferring(kernel: &Kernel, blocked: KernelSigset) -> ShadowGuest {
        seeded_on(kernel, blocked, reverie::BackendCapabilities::E9PATCH)
    }

    /// As [`seeded`], on a backend with capabilities `backend`.
    fn seeded_on(
        kernel: &Kernel,
        blocked: KernelSigset,
        backend: reverie::BackendCapabilities,
    ) -> ShadowGuest {
        let config = Config {
            sequentialize_threads: true,
            backend,
            inherited_seccomp: detcore_model::config::InheritedSeccomp::Disabled,
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
        let mut guest = seeded_inferring(&kernel, 0);
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
        let mut guest = seeded_inferring(&kernel, 0);
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
    fn a_forced_sigsegv_reset_gives_the_copy_up_quietly() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let segv = 1 << (libc::SIGSEGV - 1);
        let handler = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        let mut guest = seeded(&kernel, segv);
        kernel.set(segv, 0, segv);
        record_rt_sigaction(&mut guest, libc::SIGSEGV, Some(handler), None, &Ok(0));
        assert_eq!(
            copied_dispositions(&guest).map(|copied| copied.caught),
            Some(segv)
        );
        let reported = DIVERGENCES.with(std::cell::Cell::get);
        // A forced SIGSEGV (the backend's trapped rdtsc, say) found it caught
        // but blocked: the kernel unblocked it and reset it to the default.
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

    /// A query of the mask, which checks the copy against the kernel. The old
    /// mask the kernel writes back is `kernel_blocked`.
    fn query_mask_written(guest: &mut ShadowGuest, kernel_blocked: KernelSigset) {
        let mut old: KernelSigset = kernel_blocked;
        let oldset = AddrMut::from_raw(&mut old as *mut KernelSigset as usize);
        record_rt_sigprocmask(guest, libc::SIG_BLOCK, SetInput::None, oldset, &Ok(0));
    }

    /// [`query_mask_written`] where the kernel's mask is empty.
    fn query_mask(guest: &mut ShadowGuest) {
        let mut old: KernelSigset = 0;
        let oldset = AddrMut::from_raw(&mut old as *mut KernelSigset as usize);
        record_rt_sigprocmask(guest, libc::SIG_BLOCK, SetInput::None, oldset, &Ok(0));
    }

    #[test]
    fn a_forced_reset_of_an_ignored_sigsegv_is_quiet() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let segv = 1 << (libc::SIGSEGV - 1);
        let ignore = KernelSigaction {
            handler: libc::SIG_IGN as u64,
            flags: 0x0400_0000,
            restorer: 0x7000,
            mask: 0,
        };
        let reported = DIVERGENCES.with(std::cell::Cell::get);
        // Only SigIgn changes: an ignored, unblocked SIGSEGV reset to the
        // default.
        let mut guest = seeded(&kernel, 0);
        kernel.set(0, segv, 0);
        record_rt_sigaction(&mut guest, libc::SIGSEGV, Some(ignore), None, &Ok(0));
        kernel.set(0, 0, 0);
        query_mask(&mut guest);
        assert!(guest.thread.signal_shadow.is_none(), "given up");
        // The old action the kernel writes back is the copy's with the
        // handler reset to the default.
        let mut guest = seeded(&kernel, 0);
        kernel.set(0, segv, 0);
        record_rt_sigaction(&mut guest, libc::SIGSEGV, Some(ignore), None, &Ok(0));
        kernel.set(0, 0, 0);
        let mut written = KernelSigaction {
            handler: libc::SIG_DFL as u64,
            ..ignore
        };
        let old_action = AddrMut::from_raw(&mut written as *mut KernelSigaction as usize);
        record_rt_sigaction(&mut guest, libc::SIGSEGV, None, old_action, &Ok(0));
        assert!(guest.thread.signal_shadow.is_none(), "given up");
        assert_eq!(
            DIVERGENCES.with(std::cell::Cell::get),
            reported,
            "nothing reported"
        );
    }

    #[test]
    fn forced_signal_differences_a_reset_cannot_make_are_divergences() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let segv = 1 << (libc::SIGSEGV - 1);
        let reported = || DIVERGENCES.with(std::cell::Cell::get);
        // The kernel gained SIGSEGV in SigIgn: a reset only clears.
        let mut guest = seeded(&kernel, 0);
        let before = reported();
        kernel.set(0, segv, 0);
        query_mask(&mut guest);
        assert!(reported() > before, "the kernel ignores more than the copy");
        // SIGSEGV and SIGUSR1 both differ.
        let mut guest = seeded(&kernel, segv | USR1);
        let before = reported();
        kernel.set(0, 0, 0);
        query_mask(&mut guest);
        assert!(
            reported() > before,
            "a difference beyond the forced signals"
        );
        // The kernel wrote a different handler for SIGSEGV.
        let mut guest = seeded(&kernel, 0);
        let before = reported();
        let mut written = KernelSigaction {
            handler: 0x5000,
            flags: 0x0400_0000,
            restorer: 0x7000,
            mask: 0,
        };
        let old_action = AddrMut::from_raw(&mut written as *mut KernelSigaction as usize);
        record_rt_sigaction(&mut guest, libc::SIGSEGV, None, old_action, &Ok(0));
        assert!(reported() > before, "a handler a reset cannot install");
        // The kernel wrote the default, but with other flags than the
        // copy's handler had.
        let mut guest = seeded(&kernel, 0);
        let handler = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        kernel.set(0, 0, segv);
        record_rt_sigaction(&mut guest, libc::SIGSEGV, Some(handler), None, &Ok(0));
        let before = reported();
        kernel.set(0, 0, 0);
        let mut written = KernelSigaction {
            handler: libc::SIG_DFL as u64,
            flags: 0x0400_0000,
            ..handler
        };
        let old_action = AddrMut::from_raw(&mut written as *mut KernelSigaction as usize);
        record_rt_sigaction(&mut guest, libc::SIGSEGV, None, old_action, &Ok(0));
        assert!(reported() > before, "flags a reset does not change");
    }

    #[test]
    fn a_caught_signal_reset_counts_as_forced_whatever_the_mask_is_now() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let trap = 1 << (libc::SIGTRAP - 1);
        let handler = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        let reported = || DIVERGENCES.with(std::cell::Cell::get);
        // As on the ptrace e2e corpus: SIGTRAP caught and blocked, a
        // single-step resets and unblocks it unseen, and the guest blocks
        // every signal again before its next recorded call. The kernel and
        // the copy then both have SIGTRAP blocked, and only SigCgt differs.
        let mut guest = seeded(&kernel, trap);
        kernel.set(trap, 0, trap);
        record_rt_sigaction(&mut guest, libc::SIGTRAP, Some(handler), None, &Ok(0));
        let before = reported();
        kernel.set(trap, 0, 0);
        query_mask_written(&mut guest, trap);
        assert!(guest.thread.signal_shadow.is_none(), "given up");
        assert_eq!(reported(), before, "quietly");
        // The kernel catching a signal the copy does not is still reported.
        let mut guest = seeded(&kernel, 0);
        let before = reported();
        kernel.set(0, 0, 1 << (libc::SIGSEGV - 1));
        query_mask(&mut guest);
        assert!(reported() > before, "the kernel catches more than the copy");
    }

    #[test]
    fn reconciliation_follows_the_forced_rule() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let segv = 1 << (libc::SIGSEGV - 1);
        let reported = || DIVERGENCES.with(std::cell::Cell::get);
        let before = reported();
        // Where returns are reported: the kernel gained a forced signal while a
        // frame is open, which no forced delivery explains.
        let mut guest = in_handler(&kernel, reverie::BackendCapabilities::PTRACE);
        kernel.set(USR1 | USR2 | segv, 0, USR1);
        at_syscall_entry(&mut guest);
        assert_eq!(blocked(&guest), None, "given up");
        // A forced clear while the frame is open.
        let mut guest = seeded(&kernel, segv);
        let handler = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        kernel.set(segv, 0, USR1);
        record_rt_sigaction(&mut guest, libc::SIGUSR1, Some(handler), None, &Ok(0));
        record_delivery(&mut guest, libc::SIGUSR1);
        kernel.set(USR1, 0, USR1);
        at_syscall_entry(&mut guest);
        assert_eq!(blocked(&guest), None, "given up");
        // Where returns are inferred: the frame matched with the forced signals
        // left out, but the kernel holds one the copy does not.
        let mut guest = in_handler(&kernel, reverie::BackendCapabilities::E9PATCH);
        kernel.set(USR1 | USR2, 0, USR1);
        at_syscall_entry(&mut guest);
        kernel.set(segv, 0, USR1);
        at_syscall_entry(&mut guest);
        assert_eq!(blocked(&guest), None, "given up");
        assert_eq!(reported(), before, "none of these is a divergence");
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
        let mut guest = seeded_inferring(&kernel, 0);
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

    /// A handler for SIGUSR1 that also blocks SIGUSR2, installed and entered
    /// on `backend`, with one frame open.
    fn in_handler(kernel: &Kernel, backend: reverie::BackendCapabilities) -> ShadowGuest {
        let mut guest = seeded_on(kernel, 0, backend);
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
        guest
    }

    fn open_frames(guest: &ShadowGuest) -> usize {
        guest
            .thread
            .signal_shadow
            .as_ref()
            .map_or(0, |shadow| shadow.thread.activations.len())
    }

    #[test]
    fn copied_dispositions_answer_only_where_the_copy_can() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let handler = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        let ignore = KernelSigaction {
            handler: libc::SIG_IGN as u64,
            flags: 0x0400_0000,
            restorer: 0x7000,
            mask: 0,
        };
        kernel.set(0, 0, USR1);
        record_rt_sigaction(&mut guest, libc::SIGUSR1, Some(handler), None, &Ok(0));
        kernel.set(0, USR2, USR1);
        record_rt_sigaction(&mut guest, libc::SIGUSR2, Some(ignore), None, &Ok(0));
        let expected = Some(CopiedDispositions {
            ignored: USR2,
            caught: USR1,
        });
        assert_eq!(
            copied_dispositions(&guest),
            expected,
            "what the kernel holds"
        );
        assert_eq!(copied_dispositions_for(&guest, libc::SIGUSR1), expected);
        // Linux can force-deliver these, resetting their kernel state
        // unseen.
        for signal in [
            libc::SIGTRAP,
            libc::SIGSEGV,
            libc::SIGBUS,
            libc::SIGILL,
            libc::SIGFPE,
            libc::SIGSYS,
        ] {
            assert_eq!(copied_dispositions_for(&guest, signal), None, "{signal}");
        }
        assert_eq!(
            copied_dispositions_for(&guest, reverie::PERF_EVENT_SIGNAL as i32),
            None
        );
        // An unreported signal noted anywhere: a handler may have changed the
        // actions unseen.
        note_unreported(1, i32::MAX - 1, libc::SIGUSR1);
        assert_eq!(copied_dispositions(&guest), None);
        with_notes(|notes| notes.clear());
        assert_eq!(copied_dispositions(&guest), expected);
        // Shared actions another thread lost are unknown.
        guest
            .thread
            .signal_shadow
            .as_ref()
            .unwrap()
            .forget_dispositions();
        assert_eq!(copied_dispositions(&guest), None);
        // No copy at all.
        guest.thread.signal_shadow = None;
        assert_eq!(copied_dispositions(&guest), None);
    }

    /// The publication `guest` has to make (`next_publication`), without its
    /// generation. Each call that finds one uses a generation.
    fn due(guest: &ShadowGuest) -> Option<Option<CopiedDispositions>> {
        next_publication(guest).map(|(_, dispositions)| dispositions)
    }

    /// A5 step 2b: the scheduler's answers settle the record in whatever
    /// order they arrive. Only the latest answered publication sets the
    /// dispositions; a refusal leaves them, so the next request publishes
    /// again; a generation the process never used moves its counter past.
    #[test]
    fn answers_settle_the_record_in_any_order() {
        let one = Some(CopiedDispositions {
            ignored: USR1,
            caught: 0,
        });
        let two = Some(CopiedDispositions {
            ignored: USR1 | USR2,
            caught: 0,
        });
        let answer = |held, taken| Some(PublicationAnswer { held, taken });
        // Generations 1 and 2 were used; 2's answer arrives first.
        let mut record = PublishedDispositions {
            generation: 2,
            accepted: 0,
            dispositions: None,
        };
        assert!(!settle_publication(&mut record, 2, two, answer(2, true)));
        assert!(
            settle_publication(&mut record, 1, one, answer(2, false)),
            "superseded: publish again"
        );
        assert_eq!((record.accepted, record.dispositions), (2, two));
        // In order, each answer applies.
        let mut record = PublishedDispositions {
            generation: 2,
            accepted: 0,
            dispositions: None,
        };
        assert!(!settle_publication(&mut record, 1, one, answer(1, true)));
        assert_eq!((record.accepted, record.dispositions), (1, one));
        assert!(!settle_publication(&mut record, 2, two, answer(2, true)));
        assert_eq!((record.accepted, record.dispositions), (2, two));
        // A refusal changes nothing and waits for the next request; the
        // dispositions still differ from the copy's, so that request publishes.
        let before = record;
        record.generation = 3;
        assert!(!settle_publication(&mut record, 3, None, None));
        assert_eq!(record.dispositions, before.dispositions);
        // A refused publication does not block a later one's answer.
        record.generation = 4;
        assert!(!settle_publication(&mut record, 4, None, answer(4, true)));
        assert_eq!((record.accepted, record.dispositions), (4, None));
        // The scheduler holds generation 50, which this process never used:
        // the counter carries on after it, the dispositions stay, and the
        // thread publishes again at once.
        record.generation = 5;
        assert!(settle_publication(&mut record, 5, one, answer(50, false)));
        assert_eq!(record.generation, 50);
        assert_eq!((record.accepted, record.dispositions), (4, None));
        // The same with an entry of this very generation, the likeliest
        // stale shape (every process starts at 1): not taken, so not
        // mistaken for this publication.
        record.generation = 51;
        assert!(settle_publication(&mut record, 51, one, answer(51, false)));
        assert_eq!(record.generation, 51);
        assert_eq!((record.accepted, record.dispositions), (4, None));
    }

    /// A5 step 2b: a thread has its process's dispositions to publish
    /// whenever the scheduler holds anything else for the process, and
    /// withdraws them once the copy is given up. Recording sends nothing
    /// (this guest fails any RPC): publication waits for the thread's next
    /// request.
    #[test]
    fn a_publication_is_due_whenever_the_scheduler_holds_other_dispositions() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let published = |guest: &ShadowGuest, dispositions| {
            guest
                .thread
                .published_dispositions
                .lock()
                .unwrap()
                .dispositions = dispositions;
        };
        let nothing = CopiedDispositions {
            ignored: 0,
            caught: 0,
        };
        assert_eq!(due(&guest), Some(Some(nothing)), "seeded");
        published(&guest, Some(nothing));
        assert_eq!(due(&guest), None, "the scheduler holds them");
        let ignore = KernelSigaction {
            handler: libc::SIG_IGN as u64,
            flags: 0x0400_0000,
            restorer: 0x7000,
            mask: 0,
        };
        kernel.set(0, USR1, 0);
        record_rt_sigaction(&mut guest, libc::SIGUSR1, Some(ignore), None, &Ok(0));
        let ignoring = CopiedDispositions {
            ignored: USR1,
            caught: 0,
        };
        assert_eq!(due(&guest), Some(Some(ignoring)));
        published(&guest, Some(ignoring));
        // A noted unreported signal does not stop the publication: the
        // scheduler consults the notes itself.
        note_unreported(1, i32::MAX - 1, libc::SIGUSR1);
        assert_eq!(due(&guest), None);
        with_notes(|notes| notes.clear());
        give_up(&mut guest, "a test");
        assert_eq!(due(&guest), Some(None), "withdrawn");
        published(&guest, None);
        assert_eq!(due(&guest), None);
    }

    /// A5 step 2b: a thread publishes its copy's mask only where it is what
    /// the kernel holds whenever the thread is stopped at a request.
    #[test]
    fn the_mask_is_published_only_where_the_kernel_holds_it() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, USR1);
        let publish = |guest: &ShadowGuest, observes| mask_to_publish(&guest.thread, observes);
        assert_eq!(publish(&guest, true), Some(USR1));
        fn masks(guest: &mut ShadowGuest) -> &mut ThreadSignals {
            &mut guest.thread.signal_shadow.as_mut().unwrap().thread
        }
        masks(&mut guest).wait_mask = Some(0);
        assert_eq!(publish(&guest, true), None, "inside a temporary-mask wait");
        masks(&mut guest).wait_mask = None;
        masks(&mut guest).saved = Some(USR1);
        assert_eq!(
            publish(&guest, true),
            None,
            "an interrupted wait's mask to put back"
        );
        masks(&mut guest).saved = None;
        masks(&mut guest).verify_at_entry = true;
        assert_eq!(
            publish(&guest, true),
            None,
            "a reported mask not yet verified"
        );
        masks(&mut guest).verify_at_entry = false;
        masks(&mut guest).activations.push(0);
        masks(&mut guest).observed.push(false);
        assert_eq!(
            publish(&guest, true),
            None,
            "a handler entry no check has seen"
        );
        *masks(&mut guest).observed.last_mut().unwrap() = true;
        assert_eq!(
            publish(&guest, true),
            Some(USR1),
            "a checked frame whose return is reported"
        );
        assert_eq!(
            publish(&guest, false),
            None,
            "a frame whose return would go unseen"
        );
        guest.thread.signal_shadow = None;
        assert_eq!(publish(&guest, true), None, "no copy");
    }

    /// Where no copy is kept nothing is published, even with the switch on.
    #[test]
    fn nothing_is_published_where_no_copy_is_kept() {
        let config = Config {
            sequentialize_threads: true,
            backend: reverie::BackendCapabilities::KVM,
            ..Config::default()
        };
        let thread = ThreadState::new(DetPid::from_raw(1), &config, ());
        let guest = ShadowGuest { config, thread };
        guest
            .thread
            .published_dispositions
            .lock()
            .unwrap()
            .dispositions = Some(CopiedDispositions {
            ignored: USR1,
            caught: 0,
        });
        assert_eq!(due(&guest), None);
    }

    /// Published dispositions decide only about a signal the copy holds
    /// exactly, and only while no unreported signal is noted.
    #[test]
    fn published_dispositions_decide_where_the_copy_does() {
        assert!(published_dispositions_decide(libc::SIGUSR1));
        assert!(published_dispositions_decide(libc::SIGCHLD));
        for signal in [
            libc::SIGTRAP,
            libc::SIGSEGV,
            libc::SIGBUS,
            libc::SIGILL,
            libc::SIGFPE,
            libc::SIGSYS,
            reverie::PERF_EVENT_SIGNAL as i32,
        ] {
            assert!(!published_dispositions_decide(signal), "{signal}");
        }
        note_unreported(1, i32::MAX - 1, libc::SIGUSR1);
        assert!(!published_dispositions_decide(libc::SIGUSR1));
        with_notes(|notes| notes.clear());
    }

    /// A clone that shares the actions with a new process makes them unknown
    /// before it runs, so its child gets no copy and the scheduler, which
    /// keys the published dispositions by process, holds none for them. A
    /// thread shares them within its process and changes nothing.
    #[test]
    fn a_clone_sharing_the_actions_with_a_new_process_makes_them_unknown() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let sighand = (libc::CLONE_VM | libc::CLONE_SIGHAND) as u64;
        let nothing = CopiedDispositions {
            ignored: 0,
            caught: 0,
        };

        let mut guest = seeded(&kernel, 0);
        guest
            .thread
            .published_dispositions
            .lock()
            .unwrap()
            .dispositions = Some(nothing);
        let thread = sighand | libc::CLONE_THREAD as u64;
        futures::executor::block_on(before_clone(&mut guest, thread));
        assert_eq!(copied_dispositions(&guest), Some(nothing), "a thread");

        // Withdrawing what the scheduler holds is an RPC, which this guest
        // fails, so it starts with nothing published.
        let mut guest = seeded(&kernel, 0);
        futures::executor::block_on(before_clone(&mut guest, sighand));
        assert_eq!(copied_dispositions(&guest), None);
        let shadow = guest.thread.signal_shadow.as_ref().unwrap();
        assert!(
            shadow.for_clone(sighand).is_none(),
            "the child gets no copy"
        );
    }

    /// The threads of a process share the record of what the scheduler holds
    /// for it; a new process starts with nothing published. A thread created
    /// inside a handler gets no copy and makes the shared actions unknown, so
    /// it has their withdrawal due at its first request, before it runs any
    /// guest code.
    #[test]
    fn a_thread_shares_its_process_record_and_one_denied_a_copy_withdraws() {
        use reverie::Tool;

        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut parent = seeded(&kernel, 0);
        let nothing = CopiedDispositions {
            ignored: 0,
            caught: 0,
        };
        parent
            .thread
            .published_dispositions
            .lock()
            .unwrap()
            .dispositions = Some(nothing);
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &parent.config);
        let child = |parent: &mut ShadowGuest, flags: u64| {
            parent.thread.clone_flags = Some(crate::CloneFlags::from_bits_truncate(flags));
            let thread = <Detcore as Tool>::init_thread_state(
                &tool,
                Pid::from_raw(2),
                Some((Pid::from_raw(1), &parent.thread)),
            );
            parent.thread.clone_flags = None;
            ShadowGuest {
                config: parent.config.clone(),
                thread,
            }
        };
        let thread_flags = (libc::CLONE_VM | libc::CLONE_SIGHAND | libc::CLONE_THREAD) as u64;

        let fork = child(&mut parent, libc::SIGCHLD as u64);
        assert!(!std::sync::Arc::ptr_eq(
            &fork.thread.published_dispositions,
            &parent.thread.published_dispositions
        ));
        assert_eq!(
            due(&fork),
            Some(Some(nothing)),
            "a new process publishes its own"
        );

        let sibling = child(&mut parent, thread_flags);
        assert!(std::sync::Arc::ptr_eq(
            &sibling.thread.published_dispositions,
            &parent.thread.published_dispositions
        ));
        assert_eq!(due(&sibling), None, "already published");

        // Inside a handler: the child gets no copy and the actions become
        // unknown to every sharer.
        let handler = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        kernel.set(0, 0, USR1);
        record_rt_sigaction(&mut parent, libc::SIGUSR1, Some(handler), None, &Ok(0));
        let caught = CopiedDispositions {
            ignored: 0,
            caught: USR1,
        };
        parent
            .thread
            .published_dispositions
            .lock()
            .unwrap()
            .dispositions = Some(caught);
        kernel.set(USR1, 0, USR1);
        record_delivery(&mut parent, libc::SIGUSR1);
        assert_eq!(open_frames(&parent), 1);
        let denied = child(&mut parent, thread_flags);
        assert!(denied.thread.signal_shadow.is_none());
        assert_eq!(due(&denied), Some(None), "withdrawn first");
        assert_eq!(due(&parent), Some(None));
    }

    #[test]
    fn sigreturns_are_observed_only_where_the_backend_reports_them() {
        let config = |backend| Config {
            backend,
            sequentialize_threads: true,
            inherited_seccomp: detcore_model::config::InheritedSeccomp::Disabled,
            ..Config::default()
        };
        assert!(observes_sigreturns(&config(
            reverie::BackendCapabilities::PTRACE
        )));
        // Unless the guest is known to inherit no seccomp filter.
        for inherited in [
            detcore_model::config::InheritedSeccomp::Unknown,
            detcore_model::config::InheritedSeccomp::Strict,
            detcore_model::config::InheritedSeccomp::Filter(Some(1)),
        ] {
            let config = Config {
                inherited_seccomp: inherited,
                ..config(reverie::BackendCapabilities::PTRACE)
            };
            assert!(!observes_sigreturns(&config), "{inherited:?}");
        }
        // Nor with a gdbserver attached, whose runs the backend does not
        // report, nor without serialized threads, where no copy is kept.
        let attached = Config {
            gdbserver: true,
            ..config(reverie::BackendCapabilities::PTRACE)
        };
        assert!(!observes_sigreturns(&attached));
        let parallel = Config {
            sequentialize_threads: false,
            ..config(reverie::BackendCapabilities::PTRACE)
        };
        assert!(!observes_sigreturns(&parallel));
        // Nor under --imprecise-timers, whose preemption events a reported
        // return would lose.
        let imprecise = Config {
            imprecise_timers: true,
            ..config(reverie::BackendCapabilities::PTRACE)
        };
        assert!(!observes_sigreturns(&imprecise));
        for backend in [
            reverie::BackendCapabilities::E9PATCH,
            reverie::BackendCapabilities::KVM,
            reverie::BackendCapabilities::DBT,
            reverie::BackendCapabilities::SABRE,
            reverie::BackendCapabilities::LITEINST_IN_GUEST,
        ] {
            assert!(!observes_sigreturns(&config(backend)));
        }
    }

    #[test]
    fn a_reported_return_installs_the_frame_mask_the_handler_left() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = in_handler(&kernel, reverie::BackendCapabilities::PTRACE);
        let (reported, matched, returns) = (
            DIVERGENCES.with(std::cell::Cell::get),
            CHECKS_MATCHED.with(std::cell::Cell::get),
            REPORTED_RETURNS.with(std::cell::Cell::get),
        );
        // The handler rewrote its frame's uc_sigmask to SIGUSR2, which no
        // frame saved; inference would have had to give the copy up.
        let tid = guest.tid();
        record_sigreturn_report(
            &mut guest.thread,
            reverie::SigreturnReport::new(
                tid,
                0x7fff_0000_0000,
                reverie::SigreturnFrame::Readable { mask: USR2 },
            ),
        );
        assert_eq!(REPORTED_RETURNS.with(std::cell::Cell::get), returns + 1);
        assert_eq!(blocked(&guest), Some(USR2));
        assert_eq!(open_frames(&guest), 0);
        kernel.set(USR2, 0, USR1);
        at_syscall_entry(&mut guest);
        check_against_kernel(&mut guest, "a test check");
        assert_eq!(blocked(&guest), Some(USR2), "the copy is kept");
        assert_eq!(CHECKS_MATCHED.with(std::cell::Cell::get), matched + 1);
        assert_eq!(DIVERGENCES.with(std::cell::Cell::get), reported);
    }

    #[test]
    fn a_reported_return_whose_frame_mask_cannot_be_read_changes_nothing() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = in_handler(&kernel, reverie::BackendCapabilities::PTRACE);
        let tid = guest.tid();
        record_sigreturn_report(
            &mut guest.thread,
            reverie::SigreturnReport::new(tid, 0x7fff_0000_0000, reverie::SigreturnFrame::Unmapped),
        );
        assert_eq!(blocked(&guest), Some(USR1 | USR2));
        assert_eq!(open_frames(&guest), 1);
    }

    #[test]
    fn the_frame_range_rule_follows_linux() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let readable = reverie::SigreturnFrame::Readable { mask: USR2 };
        // sp - 8 + 440 ends exactly at the limit: accepted.
        let at_limit = USER_ADDRESS_LIMIT_4_LEVEL - RT_SIGFRAME_SIZE + 8;
        let cases = [
            (at_limit, readable, Some(Some(USR2))),
            // One byte past the limit: the paging mode would decide.
            (at_limit + 1, readable, None),
            // sp - 8 underflows: access_ok refuses, no mask change.
            (4, readable, Some(Some(USR1 | USR2))),
            // A protected frame: its protection would decide.
            (0x7fff_0000_0000, reverie::SigreturnFrame::Protected, None),
        ];
        for (sp, frame, expected) in cases {
            let mut guest = in_handler(&kernel, reverie::BackendCapabilities::PTRACE);
            let tid = guest.tid();
            let reported = DIVERGENCES.with(std::cell::Cell::get);
            record_sigreturn_report(
                &mut guest.thread,
                reverie::SigreturnReport::new(tid, sp, frame),
            );
            assert_eq!(
                blocked(&guest).map(Some),
                expected,
                "sp {sp:#x}, {frame:?}: None means given up"
            );
            assert_eq!(DIVERGENCES.with(std::cell::Cell::get), reported);
        }
    }

    #[test]
    fn a_report_the_kernel_did_not_follow_fails_closed_at_the_next_entry() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = in_handler(&kernel, reverie::BackendCapabilities::PTRACE);
        let tid = guest.tid();
        // The report closes the last frame with SIGUSR2's mask, but the
        // kernel kept the handler's (as a protection key could make it).
        record_sigreturn_report(
            &mut guest.thread,
            reverie::SigreturnReport::new(
                tid,
                0x7fff_0000_0000,
                reverie::SigreturnFrame::Readable { mask: USR2 },
            ),
        );
        assert_eq!(open_frames(&guest), 0);
        let reported = DIVERGENCES.with(std::cell::Cell::get);
        kernel.set(USR1 | USR2, 0, USR1);
        at_syscall_entry(&mut guest);
        assert_eq!(blocked(&guest), None, "given up at the next entry, quietly");
        assert_eq!(DIVERGENCES.with(std::cell::Cell::get), reported);
        // Where the kernel followed the report, the check passes once and is
        // not repeated.
        let mut guest = in_handler(&kernel, reverie::BackendCapabilities::PTRACE);
        record_sigreturn_report(
            &mut guest.thread,
            reverie::SigreturnReport::new(
                tid,
                0x7fff_0000_0000,
                reverie::SigreturnFrame::Readable { mask: USR2 },
            ),
        );
        kernel.set(USR2, 0, USR1);
        at_syscall_entry(&mut guest);
        assert_eq!(blocked(&guest), Some(USR2));
        let verify_pending = |guest: &ShadowGuest| {
            guest
                .thread
                .signal_shadow
                .as_ref()
                .is_some_and(|shadow| shadow.thread.verify_at_entry)
        };
        assert!(!verify_pending(&guest), "checked once");
    }

    #[test]
    fn where_returns_are_reported_none_is_inferred_and_a_mismatch_fails_closed() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        for (backend, inferred) in [
            (reverie::BackendCapabilities::PTRACE, false),
            (reverie::BackendCapabilities::E9PATCH, true),
        ] {
            let mut guest = in_handler(&kernel, backend);
            // A syscall inside the handler: the kernel holds its mask.
            kernel.set(USR1 | USR2, 0, USR1);
            at_syscall_entry(&mut guest);
            assert_eq!(open_frames(&guest), 1, "{backend:?}");
            // The kernel holds the mask the frame saved, as after a return.
            let reported = DIVERGENCES.with(std::cell::Cell::get);
            kernel.set(0, 0, USR1);
            at_syscall_entry(&mut guest);
            if inferred {
                assert_eq!(blocked(&guest), Some(0), "the return is inferred");
                assert_eq!(open_frames(&guest), 0);
            } else {
                // With reports, a return no report announced is a missed
                // report or a frame never written: the copy fails closed,
                // quietly.
                assert_eq!(blocked(&guest), None, "the copy is given up");
            }
            assert_eq!(
                DIVERGENCES.with(std::cell::Cell::get),
                reported,
                "{backend:?}"
            );
        }
    }

    #[test]
    fn a_caught_realtime_handler_is_followed_until_one_is_passed_on_unreported() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let (pid, tid) = (guest.pid().as_raw(), guest.tid().as_raw());
        let action = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        let reported = DIVERGENCES.with(std::cell::Cell::get);
        kernel.set(0, 0, 1 << (40 - 1));
        record_rt_sigaction(&mut guest, 40, Some(action), None, &Ok(0));
        assert!(
            guest.thread.signal_shadow.is_some(),
            "installing a caught realtime handler keeps the copy"
        );
        // Reverie shows a realtime signal passed on unreported
        // (https://github.com/rrnewton/reverie/issues/207); its note gives
        // the copy up at the thread's next operation.
        note_unreported(pid, tid, 40);
        at_syscall_entry(&mut guest);
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
