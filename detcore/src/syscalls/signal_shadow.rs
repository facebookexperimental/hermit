/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Keeping each thread's [`SignalShadow`] in step with the guest.
//!
//! On unless `Config::signal_state_copy` is off (`--no-signal-state-copy`;
//! docs/SIGNAL_STATE_CHECK.md).
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
/// guest's process installs handlers or masks of its own
/// (`BackendCapabilities::kernel_signal_state_is_guests`: ptrace, and
/// e9patch, which runs under ptrace). Threads must also be
/// serialized: otherwise two threads sharing actions could change them in the
/// kernel in one order and in the copy in another. Elsewhere the copy is
/// neither seeded from the kernel nor checked against it.
fn kernel_state_is_the_guests<G, T>(guest: &G) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let config = guest.config();
    config.signal_state_copy
        && config.sequentialize_threads
        && config.backend.kernel_signal_state_is_guests
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
    shadow.thread.wait_ran = None;
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
///
/// Before the copy changes, a caught signal's delivery is checked against the
/// kernel through the backend (`check_delivery`, A5 step 3.2). Checking adds
/// only reads and a refusal: the copy changes exactly as it would unchecked,
/// at the same point, unless a check fails, which drops the copy, or the
/// backend reports that the kernel will not deliver the signal at this
/// resume, when nothing is recorded until a later delivery. This must
/// be the last thing the signal callback does before it returns the signal:
/// a later injection would void the backend's approval.
pub(crate) async fn check_and_record_delivery<G, T>(guest: &mut G, signal: i32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if check_delivery_to_record(guest, signal).await {
        record_checked_delivery(guest, signal);
    }
}

/// The check half of [`check_and_record_delivery`]: whether `signal`'s
/// delivery is to be recorded, after the copy has caught up and
/// `Guest::deliver_signal` has checked it. A two-phase commit
/// (`Tool::commit_signal_event`) runs it, checks that it was granted no turn
/// (its `deliver_signal` must not wait for one), and only then records.
pub(crate) async fn check_delivery_to_record<G, T>(guest: &mut G, signal: i32) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let had_copy = guest.thread_state().signal_shadow.is_some();
    // At a delivery stop the kernel has not installed the handler mask yet.
    take_unreported(guest);
    take_wait_outcome(guest).await;
    reconcile_returns(guest, "signal delivery");
    // A signal the kernel does not deliver at this resume is delivered, and
    // recorded, at a later stop, if at all.
    let record = check_delivery(guest, signal, had_copy).await;
    // A two-phase commit's test sees the check made, and can grant a turn in
    // it, as a `deliver_signal` that waited for one would
    // (`signal_announcement_tests`).
    #[cfg(test)]
    {
        crate::signal_announcement_tests::note_phase("checked");
        crate::signal_announcement_tests::grant_during_check_if_asked(guest.thread_state_mut());
    }
    record
}

/// The record half of [`check_and_record_delivery`], for a delivery its
/// check half approved: the copy enters the handler.
pub(crate) fn record_checked_delivery<G, T>(guest: &mut G, signal: i32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    #[cfg(test)]
    crate::signal_announcement_tests::note_phase("recorded");
    commit_delivery(guest, signal);
}

/// [`check_and_record_delivery`] without the check, for a delivery the
/// backend makes with no check (and for tests).
#[cfg(test)]
fn record_delivery<G, T>(guest: &mut G, signal: i32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    take_unreported(guest);
    futures::executor::block_on(take_wait_outcome(guest));
    reconcile_returns(guest, "signal delivery");
    commit_delivery(guest, signal);
}

/// The copy's half of [`check_and_record_delivery`], after reconciliation:
/// `signal` is delivered as the copy holds it.
fn commit_delivery<G, T>(guest: &mut G, signal: i32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
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

/// One thread's counts of the deliveries [`check_delivery`] saw (A5 step 3.2):
/// approved (and of those, approved at a held signal's report), contradicted,
/// checked with the interruption taken from the registers
/// (`delivery_interruption`), and not checked, by reason; and the signals the
/// backend passed on without reporting them, which no check saw, by the
/// backend's reason ([`reverie::UnreportedSignalReason`]). Logged at TRACE
/// when the thread exits, below INFO so the logs that runs compare are
/// unchanged.
#[derive(Clone, Debug, Default)]
pub(crate) struct DeliveryCounters {
    /// The thread the counts were taken under. A non-leader exec gives the
    /// thread its leader's identity before it exits, so the counts keep the
    /// identity they were taken under.
    owner: Option<crate::types::DetTid>,
    approved: u64,
    /// Approved deliveries of a signal the backend held during an injection
    /// and reported after the callback that held it
    /// ([`reverie::SignalReportPath::Held`]).
    held: u64,
    /// Approved deliveries whose interruption was a syscall result Detcore
    /// finished, so the backend compared `rax` with it.
    result_checked: u64,
    mismatched: u64,
    interruption_from_registers: u64,
    skipped: std::collections::BTreeMap<&'static str, u64>,
    /// Signals the backend passed on without a report, by the backend's
    /// reason: those that could reach a guest handler, as the copy notes them
    /// (`note_unreported`). Each pass is counted: at that resume the kernel
    /// delivers the signal, puts it back in the queue when the thread's mask
    /// blocks it (a later stop can take the same instance again), queues a
    /// new instance with a kernel siginfo (a syscall stop), or discards it
    /// (an ignored signal, or a ptrace event stop or group stop). Signals the
    /// filter dropped are not counted.
    unreported: std::collections::BTreeMap<&'static str, u64>,
}

impl DeliveryCounters {
    fn skip(&mut self, reason: &'static str) {
        *self.skipped.entry(reason).or_default() += 1;
    }

    /// Counts a signal the backend passed on unreported for `reason`, taken
    /// by thread `dettid`.
    pub(crate) fn count_unreported(
        &mut self,
        dettid: crate::types::DetTid,
        reason: reverie::UnreportedSignalReason,
    ) {
        self.owner.get_or_insert(dettid);
        *self.unreported.entry(reason.name()).or_default() += 1;
    }

    /// Logs the counts, if any delivery was seen, under the identity they
    /// were taken under (`dettid` if none was recorded).
    pub(crate) fn log(&self, dettid: crate::types::DetTid) {
        if self.approved + self.mismatched == 0
            && self.skipped.is_empty()
            && self.unreported.is_empty()
        {
            return;
        }
        let dettid = self.owner.unwrap_or(dettid);
        trace!(
            target: "detcore::signal_delivery",
            "[dtid {dettid}] signal deliveries: approved={} held={} result_checked={} mismatched={} interruption_from_registers={} skipped={:?} unreported={:?}",
            self.approved,
            self.held,
            self.result_checked,
            self.mismatched,
            self.interruption_from_registers,
            self.skipped,
            self.unreported
        );
    }
}

/// The name of a reason the backend could not check a delivery, for
/// [`DeliveryCounters`].
fn unavailable_reason(reason: reverie::SignalDeliveryUnavailable) -> &'static str {
    use reverie::SignalDeliveryUnavailable as U;
    match reason {
        U::NotInSignalCallback => "not in signal callback",
        U::NotTheReportedSignal { .. } => "not the reported signal",
        U::NoSignalRecord => "no signal record",
        U::NotADeliveryStop => "not a delivery stop",
        U::FrameModeStop => "frame-mode stop",
        U::TracerFiltered => "tracer filtered",
        U::AlreadyApproved => "already approved",
        U::AfterMismatch => "after mismatch",
        U::UncheckedSignal => "unchecked signal",
        U::BlockedAtResume => "blocked at resume",
        U::Gone => "gone",
        U::Unreadable(_) => "unreadable",
        U::UnsupportedArchitecture => "unsupported architecture",
    }
}

/// Whether Detcore sees syscall number `nr` through a Tool callback: a number
/// in Reverie's syscall table. Another number (beyond the table, or negative)
/// reaches the kernel with no callback.
#[cfg(target_arch = "x86_64")]
fn sees_syscall(nr: i64) -> bool {
    usize::try_from(nr).is_ok_and(|nr| reverie::syscalls::Sysno::new(nr).is_some())
}

/// `sizeof(struct ucontext)` on x86-64 Linux (`include/uapi/asm-generic/ucontext.h`):
/// `uc_flags` and `uc_link` (8 bytes each), `stack_t uc_stack` (24),
/// `struct sigcontext uc_mcontext` (256) and the kernel's 8-byte
/// `uc_sigmask`. Not libc's `ucontext_t`, which is larger.
#[cfg(target_arch = "x86_64")]
const KERNEL_UCONTEXT_SIZE: u64 = 304;

/// Whether `regs` are the registers Linux leaves when it has just built a
/// frame for `signal`'s handler at `handler`, at a stop whose `orig_ax` was
/// `orig_ax`, so that no guest instruction has run since: a chained delivery
/// at the same exit. Linux v6.12 `x64_setup_rt_frame`
/// (https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/arch/x86/kernel/signal_64.c?h=v6.12#n202)
/// sets `rdi` to the signal, `rsi` to the frame's siginfo, `rdx` to its
/// ucontext, `rip` to the handler and `rsp` to the frame, whose layout is
/// `struct rt_sigframe { pretcode; uc; info; }` (`arch/x86/include/asm/sigframe.h`).
/// `get_sigframe` puts the frame 8 bytes below a 16-byte boundary
/// (https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/arch/x86/kernel/signal.c?h=v6.12#n144).
/// Neither writes `orig_ax`, while every later entry to the kernel other
/// than by a syscall sets it to -1
/// (https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/arch/x86/entry/entry_64.S?h=v6.12#n344).
///
/// The handler's address alone was matched again by a handler whose code
/// returns to its own first instruction (Codex review of A5 step 3.2). Some
/// stops still match that are not chained: a signal that reaches the
/// handler's first instruction after the guest came back to it with these
/// registers unchanged, when the frame's delivery stop had `orig_ax` -1, or
/// a fault there (which `delivery_interruption` excludes by its siginfo,
/// `synchronous_with_a_kernel_code`).
/// `rax`, which Linux sets to 0 too, is not checked: a syscall the tool
/// injects at the stop leaves its result there (`reverie::Interruption`).
/// x32 and ia32 frames have another layout and never match.
#[cfg(target_arch = "x86_64")]
fn at_frame_entry(regs: &libc::user_regs_struct, handler: u64, signal: i32, orig_ax: i64) -> bool {
    regs.rip == handler
        && regs.rdi == signal as u64
        && regs.rdx == regs.rsp.wrapping_add(8)
        && regs.rsi == regs.rdx.wrapping_add(KERNEL_UCONTEXT_SIZE)
        && regs.rsp % 16 == 8
        && regs.orig_rax as i64 == orig_ax
}

/// Whether `signal` is one of Linux's synchronous signals
/// (https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v6.12#n196)
/// with a positive `si_code`. A fault has one, and needs the thread back in
/// user code, so it is never a chained delivery at the same exit; such a
/// signal is therefore never taken for one. That loses two genuine chained
/// deliveries, which then take the registers' answer, always safe and
/// counted: a SIGSEGV for a frame Linux failed to build after an earlier
/// one, and a synchronous signal the guest queued with a positive code,
/// which Linux lets a thread send only to itself (`do_rt_tgsigqueueinfo`,
/// https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v6.12#n4112)
/// or a thread-group leader to its own process (`do_rt_sigqueueinfo`,
/// https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v6.12#n4065).
#[cfg(target_arch = "x86_64")]
fn synchronous_with_a_kernel_code(
    signal: i32,
    info: Option<[u8; reverie::SIGNAL_INFO_SIZE]>,
) -> bool {
    let synchronous = [
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGILL,
        libc::SIGTRAP,
        libc::SIGFPE,
        libc::SIGSYS,
    ];
    synchronous.contains(&signal)
        && info.is_some_and(|info| i32::from_ne_bytes(info[8..12].try_into().unwrap()) > 0)
}

/// What the thread was doing at this delivery stop of `signal`, for the
/// backend's check of the syscall registers; whether it was taken from the
/// registers themselves rather than from Detcore's record; and the stop's
/// `orig_ax` (-1 where no register is read).
///
/// Detcore's record (`ThreadState::delivery_interruption`) is set at each
/// guest event: the syscall entered, the result Detcore finished it with, a
/// handler frame built at this exit, or no syscall (a preemption or a trapped
/// instruction). Between events the guest runs unseen, so the record cannot
/// know whether the thread is still where the record left it or has run on in
/// user code since. The registers answer that one question, and nothing else:
/// - `orig_ax` is -1 once the thread entered the kernel other than by a
///   syscall, so a syscall record with `orig_ax` -1 means the thread returned
///   to user code since: no syscall.
/// - A frame's chained delivery at the same exit stops before any guest
///   instruction, with the registers Linux set for the frame and the frame's
///   own stop's `orig_ax` (`at_frame_entry`), and is never a synchronous
///   signal with a positive `si_code`, as a fault has
///   (`synchronous_with_a_kernel_code`). Otherwise, either the
///   guest ran since, and with `orig_ax` -1 that is no syscall, or the frame
///   was not built (a frame fault, whose SIGSEGV comes at the same exit), and
///   the registers' answer is used.
/// - A record naming the syscall `orig_ax` names is sent as it stands, so
///   `rax` is compared with the result Detcore finished the call with.
/// - Where `orig_ax` names a syscall Detcore cannot see (a number outside
///   Reverie's table, or a negative one), no callback updated the record, and
///   the registers' answer is used.
/// - A record that disagrees with a known syscall in `orig_ax` is sent as it
///   stands, so the backend reports the contradiction.
/// - With no record (a new thread or image), or where Detcore does not see
///   every syscall (`--passthru-opt`), the registers' answer is used, and the
///   check of the syscall registers verifies nothing.
async fn delivery_interruption<G, T>(
    guest: &mut G,
    signal: i32,
) -> (reverie::Interruption, bool, i64)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    use reverie::Interruption;
    let record = guest.thread_state().delivery_interruption;
    // Only x86-64's registers are read here; elsewhere nothing is checked.
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (record, signal);
        (Interruption::AfterFrame, true, -1)
    }
    #[cfg(target_arch = "x86_64")]
    {
        let regs = guest.regs().await;
        let orig = regs.orig_rax as i64;
        // Only -1 means no syscall: another negative number is a syscall the
        // kernel refused with ENOSYS.
        let from_registers = if orig == -1 {
            Interruption::NotInSyscall
        } else {
            Interruption::KernelRun { syscall: orig }
        };
        if guest.config().passthru_opt {
            return (from_registers, true, orig);
        }
        let state = guest.thread_state();
        let (interruption, from_registers) = match record {
            Some(Interruption::AfterFrame)
                if !synchronous_with_a_kernel_code(signal, guest.signal_info())
                    && at_frame_entry(
                        &regs,
                        state.delivery_frame_handler,
                        state.delivery_frame_signal,
                        state.delivery_frame_orig_ax,
                    ) =>
            {
                (Interruption::AfterFrame, false)
            }
            Some(
                record @ (Interruption::Return { syscall, .. }
                | Interruption::KernelRun { syscall }),
            ) if syscall == orig => (record, false),
            Some(Interruption::NotInSyscall) if orig == -1 => (Interruption::NotInSyscall, false),
            // The record does not describe this stop, and the registers
            // decide:
            // - there is no record (a new thread or image);
            // - the guest ran on since the record (`orig_ax` -1 after a
            //   syscall or a frame);
            // - still at the syscall's exit without the frame's registers, the
            //   frame was not built (a frame fault raises SIGSEGV at the same
            //   exit);
            // - a synchronous signal with a positive `si_code`, as a fault has,
            //   which needs the thread back in user code
            //   (`synchronous_with_a_kernel_code`);
            // - the thread is in a syscall Detcore cannot see: a number
            //   outside Reverie's table, or a negative one the kernel refuses,
            //   reaches no Tool callback, so the record still names an earlier
            //   event.
            None | Some(Interruption::AfterFrame) => (from_registers, true),
            _ if orig == -1 || !sees_syscall(orig) => (from_registers, true),
            // A known syscall that the record does not name: Detcore missed an
            // event it subscribes to. Sent as it stands, so the backend reports
            // the contradiction.
            Some(record) => (record, false),
        };
        (interruption, from_registers, orig)
    }
}

/// Checks a caught signal's delivery against the kernel through the backend
/// (`Guest::deliver_signal`), from the reconciled copy, before the copy
/// records it (A5 step 3.2). Reads only:
/// - the backend's approval binds the guest's resume to exactly this signal
///   and these masks;
/// - a contradiction, where the copy and the kernel are both certain, is a
///   Detcore defect: it is reported once, as a divergence, and the copy is
///   dropped, so the delivery goes on as the kernel makes it;
/// - a point the backend cannot check is counted by reason and changes
///   nothing.
///
/// Not checked, and counted: no copy (told apart from a copy this delivery's
/// reconciliation gave up, `had_copy`), the shared actions unknown, a signal
/// that is not caught (the kernel ignores it or takes its default action).
///
/// Returns false only when the backend says the kernel will not deliver the
/// signal at this resume, so the copy must not record a delivery:
/// [`reverie::SignalDeliveryUnavailable::BlockedAtResume`] (it requeues it)
/// and [`reverie::SignalDeliveryUnavailable::NotADeliveryStop`] (it queues
/// it anew or drops it).
async fn check_delivery<G, T>(guest: &mut G, signal: i32, had_copy: bool) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if !kernel_state_is_the_guests(guest) {
        return true;
    }
    let dettid = guest.thread_state().dettid;
    guest
        .thread_state_mut()
        .delivery_counters
        .owner
        .get_or_insert(dettid);
    let state = guest.thread_state().signal_shadow.as_ref().map(|shadow| {
        let actions = shadow.dispositions.lock().unwrap();
        (
            actions
                .as_ref()
                .map(|dispositions| dispositions.action(signal)),
            shadow.thread.saved.unwrap_or(shadow.thread.blocked),
            shadow.thread.blocked,
            shadow.thread.clone(),
        )
    });
    let skip = |guest: &mut G, reason| {
        guest.thread_state_mut().delivery_counters.skip(reason);
        true
    };
    let (action, frame_mask, current_mask, mut entered) = match state {
        None if had_copy => return skip(guest, "copy given up at this delivery"),
        None => return skip(guest, "no copy"),
        Some((None, ..)) => return skip(guest, "dispositions unknown"),
        Some((Some(Err(_)), ..)) => return skip(guest, "invalid signal"),
        Some((Some(Ok(action)), frame, current, thread)) => (action, frame, current, thread),
    };
    // A force-delivered signal the copy holds blocked or ignored was
    // unblocked, and its action reset to the default, by the kernel before
    // this stop (`force_sig_info_to_task`), which the copy did not see: it is
    // not the copy's to check, and the copy no longer knows the action.
    // - Blocked: the kernel never dequeues a signal the thread blocks, so a
    //   delivery stop for one the copy blocks shows the forced unblock,
    //   whichever instance was dequeued (a pending `raise` of it, say).
    // - Ignored: only a signal the kernel generated (`si_code` > 0: a fault
    //   or SI_KERNEL) can have been forced; a guest's `kill` of the same
    //   number changed nothing. Without a siginfo nothing shows it was forced
    //   (and the backend cannot check the delivery either: `NoSignalRecord`).
    if forced() & sigbit(signal) != 0 {
        if current_mask & sigbit(signal) != 0 {
            forced_changed(guest, "signal delivery");
            return skip(guest, "forced while blocked");
        }
        let kernel_generated = guest
            .signal_info()
            .is_some_and(|info| i32::from_ne_bytes(info[8..12].try_into().unwrap()) > 0);
        if kernel_generated && action.is_ignored() {
            forced_changed(guest, "signal delivery");
            return skip(guest, "forced while ignored");
        }
    }
    if !action.is_caught() {
        return skip(guest, "not caught");
    }
    // The handler's mask as the copy will record it (`enter_handler`), which
    // the backend compares with Linux's rule over the same masks and action.
    if !entered.enter_handler(signal, &action) {
        // One frame more than the copy follows: recording drops the copy.
        return skip(guest, "too many frames");
    }
    let handler_mask = entered.blocked;
    // Force-delivered bits are exempt in each of the copy's masks that holds
    // them, and only there: a forced delivery the copy did not see can only
    // have taken such a bit out of the kernel's masks. Inside a
    // temporary-mask wait the two masks differ, and a forced bit in only one
    // of them stays checked in the other.
    let forced_frame = forced() & frame_mask & !sigbit(signal);
    let forced_current = forced() & current_mask & !sigbit(signal);
    let (interruption, from_registers, orig_ax) = delivery_interruption(guest, signal).await;
    let action = reverie::SignalAction {
        handler: action.handler,
        flags: action.flags,
        restorer: action.restorer,
        mask: action.mask,
    };
    let delivery = reverie::SignalDelivery {
        signal,
        action,
        frame_mask,
        current_mask,
        handler_mask,
        // Reverie's preemption signal, whose kernel state differs from the
        // copy's by design; and in each mask, the force-delivered signals
        // the copy blocks there: a forced delivery the copy did not see can
        // only have unblocked one (`compare`), so one the copy holds
        // unblocked stays checked. The delivered signal is always checked.
        unchecked_mask: reserved() & !sigbit(signal),
        unchecked_frame_mask: forced_frame,
        unchecked_current_mask: forced_current,
        interruption,
    };
    let result_checked = matches!(interruption, reverie::Interruption::Return { .. });
    let held = guest.signal_report_path() == Some(reverie::SignalReportPath::Held);
    let outcome = guest.deliver_signal(delivery).await;
    let not_delivered = matches!(
        outcome,
        reverie::SignalDeliveryOutcome::Unavailable(
            reverie::SignalDeliveryUnavailable::BlockedAtResume
                | reverie::SignalDeliveryUnavailable::NotADeliveryStop
        )
    );
    if !not_delivered {
        // The kernel builds this handler's frame as the guest resumes: a
        // further delivery before a guest instruction runs is at the handler's
        // first instruction.
        let state = guest.thread_state_mut();
        state.delivery_interruption = Some(reverie::Interruption::AfterFrame);
        state.delivery_frame_handler = action.handler;
        state.delivery_frame_signal = signal;
        state.delivery_frame_orig_ax = orig_ax;
    }
    let counters = &mut guest.thread_state_mut().delivery_counters;
    counters.interruption_from_registers += u64::from(from_registers);
    match outcome {
        reverie::SignalDeliveryOutcome::Accepted => {
            counters.approved += 1;
            counters.held += u64::from(held);
            counters.result_checked += u64::from(result_checked);
            trace!(
                target: "detcore::signal_delivery",
                "[dtid {}] signal-state copy: the delivery of signal {signal} is approved, \
                 interrupting {interruption:?}",
                guest.thread_state().dettid
            );
        }
        reverie::SignalDeliveryOutcome::Mismatch(mismatch) => {
            counters.mismatched += 1;
            let what = format!(
                "the kernel contradicts the delivery of signal {signal} in {:?}: the copy holds \
                 {:#x}, the kernel {:#x}",
                mismatch.field, mismatch.expected, mismatch.actual
            );
            if guest.config().signal_delivery_check_fatal {
                // A Detcore defect where both sides are certain: a strict run
                // ends rather than continue on the kernel's word alone.
                let failure = format!(
                    "HERMIT_INTERNAL_FAILURE class=signal-delivery-check dtid={} {what}",
                    guest.thread_state().dettid
                );
                tracing::error!("{failure}");
                panic!("{failure}");
            }
            diverged(guest, "signal delivery", &what);
        }
        reverie::SignalDeliveryOutcome::Unavailable(reason) => {
            counters.skip(unavailable_reason(reason));
        }
        reverie::SignalDeliveryOutcome::Refused(_) => counters.skip("backend refuses"),
        // Only a frame-building backend answers these, and no backend that
        // keeps a copy builds frames.
        reverie::SignalDeliveryOutcome::Installed | reverie::SignalDeliveryOutcome::FrameFault => {
            counters.skip("frame-building outcome");
        }
    }
    !not_delivered
}

/// At a delivery stop inside a temporary-mask wait, Linux holds one of two
/// masks: the wait's, when the kernel ran the call (the handler's frame then
/// saves the thread's own mask), or the thread's own, when the signal arrived
/// before the call started, which depends on host timing (see
/// `record_or_replay_rt_sigsuspend`). The kernel's current mask tells which;
/// any other mask is left for the checks that follow to report.
///
/// The masks are compared with the signals Linux can force-deliver left out
/// ([`forced`]), since a forced delivery the copy did not see can clear one.
/// When the wait's mask and the copy's differ only in such signals, that
/// comparison cannot tell them apart, and an exact one alone can mislead: a
/// forced clear only takes bits out, and can turn the copy's mask into the
/// wait's. Then the copy keeps its own mask if the kernel holds it exactly,
/// and takes the wait's if the kernel holds that exactly and either it has a
/// bit the copy's lacks (no clear adds one) or the call's raw result says the
/// kernel ran the wait ([`kernel_ran_the_wait`]), installing its mask over
/// any earlier clear. Any other subset of either mask gives the copy up
/// quietly, as a forced delivery's clear; a kernel mask with a bit neither
/// mask has is left to the checks that follow to report.
async fn take_wait_outcome<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some((wait_mask, saved, blocked)) =
        guest.thread_state().signal_shadow.as_ref().map(|shadow| {
            (
                shadow.thread.wait_mask,
                shadow.thread.saved,
                shadow.thread.blocked,
            )
        })
    else {
        return;
    };
    if (wait_mask.is_none() && saved.is_none()) || !kernel_state_is_the_guests(guest) {
        return;
    }
    let Some(kernel) = read_or_give_up(guest, "signal delivery") else {
        return;
    };
    let compared = !(reserved() | forced());
    let same = |a: KernelSigset, b: KernelSigset| a & compared == b & compared;
    let exact = |a: KernelSigset, b: KernelSigset| a & !reserved() == b & !reserved();
    // A temporary-mask restore no handler took has happened if the kernel
    // already shows the restored mask (the wait returned to the guest).
    let settled = match saved {
        Some(saved) if same(kernel.blocked, saved) => Some(saved),
        _ => None,
    };
    let current = settled.unwrap_or(blocked);
    let forced_only = |wait: KernelSigset| same(wait, current) && !exact(wait, current);
    let take_wait = match wait_mask {
        Some(wait) if forced_only(wait) && same(kernel.blocked, wait) => {
            // A forced delivery only takes bits out of the kernel's mask. So
            // the copy's own mask, held exactly, stays; and the wait's,
            // held exactly, was installed if it has a bit the copy's lacks,
            // and otherwise only if the call's raw result says the kernel
            // ran it. Any other subset of either mask can be a forced clear,
            // which gives the copy up; a kernel mask with a bit neither has
            // no forced delivery explains, and is left to the checks that
            // follow to report, as before.
            let subset = |a: KernelSigset, b: KernelSigset| a & !b & !reserved() == 0;
            if exact(kernel.blocked, current) {
                None
            } else if exact(kernel.blocked, wait)
                && (wait & !current & !reserved() != 0 || kernel_ran_the_wait(guest).await)
            {
                Some(wait)
            } else if subset(kernel.blocked, current) || subset(kernel.blocked, wait) {
                forced_changed(guest, "signal delivery");
                return;
            } else {
                None
            }
        }
        Some(wait) if same(kernel.blocked, wait) && !same(current, wait) => Some(wait),
        _ => None,
    };
    let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() else {
        return;
    };
    if settled.is_some() {
        shadow.thread.settle();
    }
    if let Some(wait) = take_wait {
        shadow.thread.interrupted_under_temporary_mask(wait);
    }
    // The outcome was chosen with the forced signals left out. A forced
    // delivery's clear gives the copy up now; any other difference is left to
    // the next check, which reports it.
    if compare(kernel.blocked, shadow.thread.blocked) == Compared::ForcedOnly {
        forced_changed(guest, "signal delivery");
    }
}

/// Whether the kernel ran the temporary-mask wait this delivery stop ends,
/// and so installed its mask: the call's own raw result, before any
/// normalization, was not the not-run report ([`note_wait_ran`]; only
/// rt_sigsuspend records it today, so other waits never count as run here).
/// Two more conditions tie it to this stop: Detcore finished the call
/// interrupted (`-EINTR` or `-ERESTARTNOHAND`), and the stop's `orig_ax`
/// still names that call, so the thread has not run on.
async fn kernel_ran_the_wait<G, T>(guest: &mut G) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    const ERESTARTNOHAND: i64 = 514;
    let ran = guest
        .thread_state()
        .signal_shadow
        .as_ref()
        .and_then(|shadow| shadow.thread.wait_ran);
    if ran != Some(true) {
        return false;
    }
    let Some(reverie::Interruption::Return { syscall, result }) =
        guest.thread_state().delivery_interruption
    else {
        return false;
    };
    if result != -i64::from(libc::EINTR) && result != -ERESTARTNOHAND {
        return false;
    }
    #[cfg(target_arch = "x86_64")]
    {
        guest.regs().await.orig_rax as i64 == syscall
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = syscall;
        false
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
    config.signal_state_copy
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
    let sp = crate::arch::stack_pointer(&guest.regs().await) as usize;
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

/// Records whether the kernel ran the wait whose mask [`note_wait_mask`]
/// noted, from the call's raw result: `ran` is false for the not-run report
/// (`-ERESTARTSYS`), whose mask the kernel never installed. Read by
/// [`kernel_ran_the_wait`].
pub(crate) fn note_wait_ran<G, T>(guest: &mut G, ran: bool)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut()
        && shadow.thread.wait_mask.is_some()
    {
        shadow.thread.wait_ran = Some(ran);
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
        delivery: DeliveryScript,
    }

    /// What the scripted backend answers `Guest::deliver_signal`, and what it
    /// was asked, with the `orig_rax` its registers show.
    #[derive(Default)]
    struct DeliveryScript {
        answer: Option<reverie::SignalDeliveryOutcome>,
        /// The siginfo `Guest::signal_info` reports, if any.
        signal_info: Option<[u8; reverie::SIGNAL_INFO_SIZE]>,
        asked: Vec<reverie::SignalDelivery>,
        /// What `Guest::signal_report_path` answers.
        report_path: Option<reverie::SignalReportPath>,
        orig_rax: Option<i64>,
        rip: u64,
        /// `rdi`, `rsi`, `rdx` and `rsp`, as `regs` shows them.
        frame_regs: [u64; 4],
        registers_read: usize,
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
            let orig_rax = self
                .delivery
                .orig_rax
                .expect("recording on a ptrace-like backend reads no registers");
            self.delivery.registers_read += 1;
            // SAFETY: an all-zero `user_regs_struct` is a valid value.
            let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
            crate::arch::set_syscall_number(&mut regs, orig_rax as u64);
            crate::arch::set_instruction_pointer(&mut regs, self.delivery.rip);
            let [arg0, arg1, arg2, sp] = self.delivery.frame_regs;
            crate::arch::set_syscall_arguments(&mut regs, &[arg0, arg1, arg2]);
            crate::arch::set_stack_pointer(&mut regs, sp);
            regs
        }
        fn signal_report_path(&self) -> Option<reverie::SignalReportPath> {
            self.delivery.report_path
        }
        fn signal_info(&self) -> Option<[u8; reverie::SIGNAL_INFO_SIZE]> {
            self.delivery.signal_info
        }
        async fn deliver_signal(
            &mut self,
            delivery: reverie::SignalDelivery,
        ) -> reverie::SignalDeliveryOutcome {
            self.delivery.asked.push(delivery);
            self.delivery
                .answer
                .expect("this test does not expect a delivery check")
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
        let mut guest = ShadowGuest {
            config,
            thread,
            delivery: DeliveryScript::default(),
        };
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

    /// The same when the wait's mask and the thread's own differ only in
    /// signals Linux can force-deliver: masks alone cannot tell the wait's
    /// mask from a forced clear of the own one, so the copy follows the
    /// kernel only when the record agrees, and otherwise gives itself up
    /// quietly, as for a forced delivery, never reporting a divergence.
    // x86_64 only: Detcore reads a delivery's interruption from the
    // registers only there (elsewhere it takes AfterFrame).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_wait_mask_differing_only_in_forced_signals_is_taken_only_with_evidence() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let action = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        let int = 1 << (libc::SIGINT - 1);
        let ill = sigbit(libc::SIGILL);
        let fpe = sigbit(libc::SIGFPE);
        assert_eq!((ill | fpe) & forced(), ill | fpe);
        let interrupted = reverie::Interruption::Return {
            syscall: libc::SYS_rt_sigsuspend,
            result: -514,
        };
        let usr1 = sigbit(libc::SIGUSR1);
        let returned = |result: i64| reverie::Interruption::Return {
            syscall: libc::SYS_rt_sigsuspend,
            result,
        };
        let sigsuspend = libc::SYS_rt_sigsuspend;
        // (own mask, wait mask, kernel's mask at the stop, record at the
        // stop, the stop's orig_ax, the wait's raw outcome) -> the copy's mask
        // afterwards, or None for a quiet give-up; and how many register reads
        // the decision made.
        let cases = [
            (
                ill | fpe,
                0,
                0,
                Some(interrupted),
                sigsuspend,
                Some(true),
                Some(0),
                1,
                "the kernel ran the wait",
            ),
            (
                ill | fpe,
                0,
                0,
                Some(returned(-4)),
                sigsuspend,
                Some(true),
                Some(0),
                1,
                "the wait ended with EINTR",
            ),
            (
                ill | fpe,
                0,
                ill | fpe,
                None,
                sigsuspend,
                None,
                Some(ill | fpe),
                0,
                "the signal came first",
            ),
            (
                ill | fpe,
                0,
                ill | fpe,
                Some(interrupted),
                sigsuspend,
                Some(true),
                Some(ill | fpe),
                0,
                "the own mask held exactly stays, whatever the record says",
            ),
            (
                0,
                ill,
                ill,
                None,
                sigsuspend,
                None,
                Some(ill),
                0,
                "the wait's mask holds a bit the own lacks: installed",
            ),
            (
                ill | fpe,
                0,
                0,
                None,
                sigsuspend,
                None,
                None,
                0,
                "the wait's mask, but no sign it ran: a forced clear may have made it",
            ),
            (
                ill | fpe,
                0,
                0,
                Some(interrupted),
                sigsuspend,
                Some(false),
                None,
                0,
                "ERESTARTNOHAND normalized from a not-run report: the mask was never installed",
            ),
            (
                ill | fpe,
                0,
                0,
                Some(returned(-513)),
                sigsuspend,
                Some(true),
                None,
                0,
                "a restart, not an interrupted wait",
            ),
            (
                ill | fpe,
                0,
                0,
                Some(interrupted),
                -1,
                Some(true),
                None,
                1,
                "the thread ran on since the wait (orig_ax -1)",
            ),
            (
                ill | fpe,
                0,
                fpe,
                None,
                sigsuspend,
                None,
                None,
                0,
                "neither mask: a forced clear of SIGILL",
            ),
            (
                0,
                ill,
                0,
                Some(interrupted),
                sigsuspend,
                Some(true),
                Some(0),
                0,
                "the wait ran and lost SIGILL to a forced clear: the own mask, exactly",
            ),
            (
                ill,
                0,
                usr1,
                None,
                sigsuspend,
                None,
                Some(ill),
                0,
                "an ordinary signal differs: decided as before, left for the delivery check to report",
            ),
            (
                ill,
                0,
                fpe,
                None,
                sigsuspend,
                None,
                Some(ill),
                0,
                "a forced bit neither mask has, which no forced delivery adds: left for the delivery check",
            ),
            (
                ill,
                0,
                ill | fpe,
                Some(interrupted),
                sigsuspend,
                Some(true),
                Some(ill),
                0,
                "the own mask plus a forced bit: left for the delivery check",
            ),
        ];
        for (own, wait, at_stop, record, orig_ax, ran, expected, reads, why) in cases {
            let mut guest = seeded(&kernel, own);
            kernel.set(own, 0, int);
            record_rt_sigaction(&mut guest, libc::SIGINT, Some(action), None, &Ok(0));
            at_syscall_entry(&mut guest);
            note_wait_mask(&mut guest, wait);
            if let Some(ran) = ran {
                note_wait_ran(&mut guest, ran);
            }
            kernel.set(at_stop, 0, int);
            guest.thread.delivery_interruption = record;
            guest.delivery.orig_rax = Some(orig_ax);
            record_delivery(&mut guest, libc::SIGINT);
            assert_eq!(
                guest.delivery.registers_read, reads,
                "register reads: {why}"
            );
            match expected {
                Some(mask) => {
                    let shadow = guest
                        .thread
                        .signal_shadow
                        .as_ref()
                        .unwrap_or_else(|| panic!("copy given up: {why}"));
                    assert_eq!(shadow.thread.blocked, mask | int, "{why}");
                    assert_eq!(
                        shadow.thread.activations,
                        vec![own],
                        "the frame saves the thread's own mask: {why}"
                    );
                }
                None => assert!(guest.thread.signal_shadow.is_none(), "kept: {why}"),
            }
            assert!(!guest.thread.signal_shadow_lost, "a divergence: {why}");
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

    /// IR-13 of the A5 3c.1 plan (design D36): on the in-guest backend, a
    /// two-phase commit's check returns before `Guest::deliver_signal`, so no
    /// 3c.1 commit, default-fatal deliveries among them, asks the backend to
    /// check a delivery: the kernel's signal state there is the runtime's,
    /// and the check comes with 3c.2's caught handlers. The thread keeps a
    /// copy in which SIGUSR1 is caught, so a check that went on would ask the
    /// backend about SIGUSR1 (its script refuses to answer) and count the
    /// other signals as skipped. Mutation it catches: the early return
    /// removed.
    #[tokio::test]
    async fn an_in_guest_commit_returns_before_the_backend_check() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = seeded(&kernel, 0);
        let handler = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        kernel.set(0, 0, USR1);
        record_rt_sigaction(&mut guest, libc::SIGUSR1, Some(handler), None, &Ok(0));
        guest.config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
        guest.config.in_guest_signal_delivery = true;
        let counters = format!("{:?}", guest.thread.delivery_counters);
        for signal in [
            libc::SIGTERM,
            libc::SIGKILL,
            libc::SIGQUIT,
            libc::SIGUSR1,
            40,
        ] {
            assert!(
                check_delivery_to_record(&mut guest, signal).await,
                "{signal}"
            );
        }
        assert!(guest.delivery.asked.is_empty());
        assert_eq!(format!("{:?}", guest.thread.delivery_counters), counters);
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
        let guest = ShadowGuest {
            config,
            thread,
            delivery: DeliveryScript::default(),
        };
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
                delivery: DeliveryScript::default(),
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

    // x86_64 only: the ptrace backend reports sigreturns only there (the
    // report reads an x86-64 signal frame).
    #[cfg(target_arch = "x86_64")]
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
        // Nor with the copy off.
        let off = Config {
            signal_state_copy: false,
            ..config(reverie::BackendCapabilities::PTRACE)
        };
        assert!(!observes_sigreturns(&off));
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

    // x86_64 only: the ptrace backend reports sigreturns only there (the
    // report reads an x86-64 signal frame).
    #[cfg(target_arch = "x86_64")]
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
            // The ptrace backend reports sigreturns only on x86_64 (the report
            // reads an x86-64 signal frame); elsewhere its returns are inferred.
            #[cfg(target_arch = "x86_64")]
            (reverie::BackendCapabilities::PTRACE, false),
            #[cfg(not(target_arch = "x86_64"))]
            (reverie::BackendCapabilities::PTRACE, true),
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
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let ptrace = Config {
            sequentialize_threads: true,
            backend: reverie::BackendCapabilities::PTRACE,
            ..Config::default()
        };
        // Nor with the copy turned off (`--no-signal-state-copy`), which is
        // otherwise on.
        assert!(ptrace.signal_state_copy);
        for config in [
            Config {
                sequentialize_threads: false,
                ..ptrace.clone()
            },
            Config {
                signal_state_copy: false,
                ..ptrace.clone()
            },
        ] {
            let thread = ThreadState::new(DetPid::from_raw(1), &config, ());
            let mut guest = ShadowGuest {
                config,
                thread,
                delivery: DeliveryScript::default(),
            };
            at_exec(&mut guest);
            assert!(guest.thread.signal_shadow.is_none());
        }
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
        let mut guest = ShadowGuest {
            config,
            thread,
            delivery: DeliveryScript::default(),
        };
        at_exec(&mut guest);
        assert!(guest.thread.signal_shadow.is_none());
    }

    /// A guest whose copy catches SIGUSR1 with `sa_mask` SIGUSR2, blocks
    /// SIGINT, and whose scripted backend answers delivery checks `answer`.
    fn catching(kernel: &Kernel, answer: reverie::SignalDeliveryOutcome) -> ShadowGuest {
        let int = sigbit(libc::SIGINT);
        let mut guest = seeded(kernel, int);
        let action = KernelSigaction {
            handler: 0x4000,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: USR2,
        };
        kernel.set(int, 0, USR1);
        record_rt_sigaction(&mut guest, libc::SIGUSR1, Some(action), None, &Ok(0));
        assert!(guest.thread.signal_shadow.is_some());
        guest.delivery.answer = Some(answer);
        guest
    }

    /// The siginfo of a fault the kernel generated: `si_code` SEGV_MAPERR (1).
    fn fault_siginfo(signal: i32) -> [u8; reverie::SIGNAL_INFO_SIZE] {
        let mut info = [0u8; reverie::SIGNAL_INFO_SIZE];
        info[0..4].copy_from_slice(&signal.to_ne_bytes());
        info[8..12].copy_from_slice(&1i32.to_ne_bytes());
        info
    }

    fn check(guest: &mut ShadowGuest, signal: i32) {
        futures::executor::block_on(check_and_record_delivery(guest, signal));
    }

    /// The delivery a check builds is the copy's, before it records the
    /// delivery: the caught action, the frame and current masks, Linux's
    /// handler mask, every reserved and forced signal unchecked except the
    /// delivered one, and the interruption Detcore recorded.
    // x86_64 only: Detcore reads a delivery's interruption from the
    // registers only there (elsewhere it takes AfterFrame).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_checked_delivery_is_built_from_the_copy_and_recorded_as_unchecked() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let int = sigbit(libc::SIGINT);
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        let mut unchecked = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        let interruption = reverie::Interruption::Return {
            syscall: libc::SYS_read,
            result: -514,
        };
        guest.thread.delivery_interruption = Some(interruption);
        guest.delivery.orig_rax = Some(libc::SYS_read);
        check(&mut guest, libc::SIGUSR1);
        record_delivery(&mut unchecked, libc::SIGUSR1);
        let asked = guest.delivery.asked.clone();
        assert_eq!(asked.len(), 1);
        let d = asked[0];
        assert_eq!(d.signal, libc::SIGUSR1);
        assert_eq!(d.action.handler, 0x4000);
        assert_eq!(d.action.mask, USR2);
        assert_eq!((d.frame_mask, d.current_mask), (int, int));
        assert_eq!(d.handler_mask, int | USR2 | USR1);
        assert_eq!(d.unchecked_mask, reserved());
        assert_eq!((d.unchecked_frame_mask, d.unchecked_current_mask), (0, 0));
        assert_eq!(d.interruption, interruption);
        assert_eq!(guest.thread.delivery_counters.approved, 1);
        assert_eq!(
            guest.thread.delivery_counters.held, 0,
            "the backend does not say the signal was held"
        );
        // Checking changes the copy exactly as not checking does.
        assert_eq!(
            guest
                .thread
                .signal_shadow
                .as_ref()
                .map(|shadow| shadow.thread.clone()),
            unchecked
                .thread
                .signal_shadow
                .as_ref()
                .map(|shadow| shadow.thread.clone()),
        );
        assert_eq!(
            guest.thread.delivery_interruption,
            Some(reverie::Interruption::AfterFrame),
            "the next delivery at this exit follows this frame"
        );
    }

    /// Inside a temporary-mask wait whose restore is pending, the frame saves
    /// the mask from before the wait, and the current mask is the wait's.
    #[test]
    fn a_delivery_inside_a_temporary_mask_wait_saves_the_mask_from_before_it() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let int = sigbit(libc::SIGINT);
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        let shadow = guest.thread.signal_shadow.as_mut().unwrap();
        shadow.thread.blocked = int | USR1;
        shadow.thread.interrupted_under_temporary_mask(0);
        guest.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
        guest.delivery.orig_rax = Some(-1);
        kernel.set(0, 0, USR1);
        check(&mut guest, libc::SIGUSR1);
        let d = guest.delivery.asked[0];
        assert_eq!((d.frame_mask, d.current_mask), (int | USR1, 0));
        assert_eq!(d.handler_mask, USR2 | USR1);
    }

    /// A contradiction where both sides are certain is a divergence: it is
    /// counted, the copy is dropped, and nothing more is recorded.
    #[test]
    fn a_mismatch_is_a_divergence_that_drops_the_copy() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mismatch = reverie::SignalDeliveryMismatch {
            field: reverie::SignalDeliveryField::FrameMask,
            expected: 1,
            actual: 2,
        };
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Mismatch(mismatch));
        guest.delivery.orig_rax = Some(-1);
        guest.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
        let before = DIVERGENCES.with(Cell::get);
        check(&mut guest, libc::SIGUSR1);
        assert_eq!(DIVERGENCES.with(Cell::get), before + 1);
        assert!(guest.thread.signal_shadow.is_none());
        assert!(guest.thread.signal_shadow_lost);
        assert_eq!(guest.thread.delivery_counters.mismatched, 1);
    }

    /// Under `--strict` (`Config::signal_delivery_check_fatal`) the same
    /// contradiction ends the run as an internal failure.
    #[test]
    #[should_panic(expected = "HERMIT_INTERNAL_FAILURE class=signal-delivery-check")]
    fn a_mismatch_ends_a_strict_run() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mismatch = reverie::SignalDeliveryMismatch {
            field: reverie::SignalDeliveryField::InterruptedResult,
            expected: 1,
            actual: 2,
        };
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Mismatch(mismatch));
        guest.config.signal_delivery_check_fatal = true;
        guest.delivery.orig_rax = Some(-1);
        guest.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
        check(&mut guest, libc::SIGUSR1);
    }

    /// A point the backend cannot check is counted by reason, and the copy
    /// records the delivery as it does unchecked; but a signal the kernel will
    /// not deliver at this resume (`BlockedAtResume`, which requeues it, and
    /// `NotADeliveryStop`, which queues it anew or drops it) is not recorded,
    /// and no handler frame follows it.
    #[test]
    fn an_unavailable_check_is_counted_and_an_undelivered_signal_is_not_recorded() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let unavailable = reverie::SignalDeliveryOutcome::Unavailable;
        let mut guest = catching(
            &kernel,
            unavailable(reverie::SignalDeliveryUnavailable::TracerFiltered),
        );
        let mut unchecked = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        guest.delivery.orig_rax = Some(-1);
        guest.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
        check(&mut guest, libc::SIGUSR1);
        record_delivery(&mut unchecked, libc::SIGUSR1);
        assert_eq!(
            guest
                .thread
                .delivery_counters
                .skipped
                .get("tracer filtered"),
            Some(&1)
        );
        assert_eq!(
            guest
                .thread
                .signal_shadow
                .as_ref()
                .map(|shadow| shadow.thread.clone()),
            unchecked
                .thread
                .signal_shadow
                .as_ref()
                .map(|shadow| shadow.thread.clone()),
        );
        assert_eq!(
            guest.thread.delivery_interruption,
            Some(reverie::Interruption::AfterFrame)
        );

        for (reason, name) in [
            (
                reverie::SignalDeliveryUnavailable::BlockedAtResume,
                "blocked at resume",
            ),
            (
                reverie::SignalDeliveryUnavailable::NotADeliveryStop,
                "not a delivery stop",
            ),
        ] {
            let mut undelivered = catching(&kernel, unavailable(reason));
            undelivered.delivery.orig_rax = Some(-1);
            undelivered.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
            let before = undelivered
                .thread
                .signal_shadow
                .as_ref()
                .map(|shadow| shadow.thread.clone());
            check(&mut undelivered, libc::SIGUSR1);
            assert_eq!(
                undelivered.thread.delivery_counters.skipped.get(name),
                Some(&1)
            );
            assert_eq!(
                undelivered
                    .thread
                    .signal_shadow
                    .as_ref()
                    .map(|shadow| shadow.thread.clone()),
                before,
                "nothing is recorded for a signal the kernel does not deliver ({name})"
            );
            assert_eq!(
                undelivered.thread.delivery_interruption,
                Some(reverie::Interruption::NotInSyscall),
                "no frame follows an undelivered signal ({name})"
            );
        }
    }

    /// The registers Linux leaves for a SIGUSR1 frame at 0x7fff_0008, stated
    /// on their own rather than from `KERNEL_UCONTEXT_SIZE`: `rdi` the signal,
    /// `rsi` the siginfo 8 + 304 bytes above the frame, `rdx` the ucontext 8
    /// bytes above it, and `rsp` the frame. x86_64 only, as are its tests.
    #[cfg(target_arch = "x86_64")]
    const FRAME_REGS: [u64; 4] = [libc::SIGUSR1 as u64, 0x7fff_0140, 0x7fff_0010, 0x7fff_0008];

    /// A frame's chained delivery at the same exit stops at the handler's
    /// first instruction with the frame's registers and its stop's
    /// `orig_ax`, and is `AfterFrame`. Each register that differs means the
    /// guest ran on, and with `orig_ax` -1 that is no syscall. Anywhere else
    /// it is no syscall too; and still at the syscall's exit away from the
    /// handler, where a frame fault's SIGSEGV stops, the registers decide.
    // x86_64 only: Detcore reads a delivery's interruption from the
    // registers only there (elsewhere it takes AfterFrame).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn after_frame_holds_only_at_the_handlers_first_instruction() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        guest.delivery.orig_rax = Some(-1);
        guest.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
        check(&mut guest, libc::SIGUSR1);
        assert_eq!(guest.thread.delivery_frame_handler, 0x4000);
        assert_eq!(guest.thread.delivery_frame_signal, libc::SIGUSR1);
        assert_eq!(guest.thread.delivery_frame_orig_ax, -1);
        // The kernel now shows the handler's mask, as at a chained stop.
        let int = sigbit(libc::SIGINT);
        kernel.set(int | USR2 | USR1, 0, USR1);
        guest.delivery.rip = 0x4000;
        guest.delivery.frame_regs = FRAME_REGS;
        check(&mut guest, libc::SIGUSR1);
        assert_eq!(
            guest.delivery.asked[1].interruption,
            reverie::Interruption::AfterFrame
        );
        // At the handler's first instruction, but not as Linux leaves a
        // frame: the guest ran since. Each case differs from FRAME_REGS in
        // one way only.
        let [rdi, rsi, rdx, rsp] = FRAME_REGS;
        for (frame_regs, why) in [
            ([0, 0, 0, 0], "no frame registers at all"),
            (
                [libc::SIGUSR2 as u64, rsi, rdx, rsp],
                "another signal in rdi",
            ),
            (
                [rdi, rsi, rdx + 8, rsp],
                "the ucontext not right above the return address",
            ),
            (
                [rdi, rdx + 968, rdx, rsp],
                "the siginfo after glibc's 968-byte ucontext_t",
            ),
            (
                [rdi, rsi + 8, rdx + 8, rsp + 8],
                "a frame Linux never aligns so",
            ),
        ] {
            guest.delivery.frame_regs = frame_regs;
            check(&mut guest, libc::SIGUSR1);
            assert_eq!(
                guest.delivery.asked.last().unwrap().interruption,
                reverie::Interruption::NotInSyscall,
                "{why}"
            );
            // Each delivery went ahead, so the next one follows its frame.
            assert_eq!(
                guest.thread.delivery_interruption,
                Some(reverie::Interruption::AfterFrame)
            );
        }
        guest.delivery.frame_regs = FRAME_REGS;
        guest.delivery.rip = 0x5123;
        check(&mut guest, libc::SIGUSR1);
        assert_eq!(
            guest.delivery.asked[7].interruption,
            reverie::Interruption::NotInSyscall
        );
        guest.delivery.orig_rax = Some(libc::SYS_tgkill);
        check(&mut guest, libc::SIGUSR1);
        assert_eq!(
            guest.delivery.asked[8].interruption,
            reverie::Interruption::KernelRun {
                syscall: libc::SYS_tgkill
            }
        );
        assert_eq!(
            guest.thread.delivery_counters.interruption_from_registers,
            7
        );
    }

    /// A frame delivered at a syscall's stop keeps that syscall in `orig_ax`
    /// until the thread enters the kernel again. At the handler's first
    /// instruction with every frame register intact but `orig_ax` -1, the
    /// thread went back to user code and came in again: not `AfterFrame`.
    // x86_64 only: Detcore reads a delivery's interruption from the
    // registers only there (elsewhere it takes AfterFrame).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn after_frame_needs_the_frames_own_orig_ax() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        guest.delivery.orig_rax = Some(libc::SYS_tgkill);
        guest.thread.delivery_interruption = Some(reverie::Interruption::Return {
            syscall: libc::SYS_tgkill,
            result: 0,
        });
        check(&mut guest, libc::SIGUSR1);
        assert_eq!(guest.thread.delivery_frame_orig_ax, libc::SYS_tgkill);
        kernel.set(sigbit(libc::SIGINT) | USR2 | USR1, 0, USR1);
        guest.delivery.rip = 0x4000;
        guest.delivery.frame_regs = FRAME_REGS;
        check(&mut guest, libc::SIGUSR1);
        assert_eq!(
            guest.delivery.asked[1].interruption,
            reverie::Interruption::AfterFrame,
            "the chained stop keeps the syscall's orig_ax"
        );
        guest.delivery.orig_rax = Some(-1);
        check(&mut guest, libc::SIGUSR1);
        assert_eq!(
            guest.delivery.asked[2].interruption,
            reverie::Interruption::NotInSyscall
        );
        assert_eq!(
            guest.thread.delivery_counters.interruption_from_registers,
            1
        );
    }

    /// A fault needs the thread back in user code: a synchronous signal with
    /// a positive `si_code` at the handler's first instruction (a `ud2`
    /// there), with every frame register and `orig_ax` intact, is not
    /// `AfterFrame`. The same signal sent by `kill` or `tgkill` and pending
    /// at the same exit is. One the thread queued to itself with a positive
    /// code is chained too, but cannot be told from a fault, and takes the
    /// registers' answer as well.
    // x86_64 only: Detcore reads a delivery's interruption from the
    // registers only there (elsewhere it takes AfterFrame).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn after_frame_is_never_a_synchronous_signal_with_a_kernel_code() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let int = sigbit(libc::SIGINT);
        let ill = sigbit(libc::SIGILL);
        let interruption = |si_code: i32| {
            let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
            let action = KernelSigaction {
                handler: 0x4100,
                flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
                restorer: 0x7000,
                mask: 0,
            };
            kernel.set(int, 0, USR1 | ill);
            record_rt_sigaction(&mut guest, libc::SIGILL, Some(action), None, &Ok(0));
            guest.delivery.orig_rax = Some(-1);
            guest.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
            check(&mut guest, libc::SIGUSR1);
            kernel.set(int | USR2 | USR1, 0, USR1 | ill);
            guest.delivery.rip = 0x4000;
            guest.delivery.frame_regs = FRAME_REGS;
            let mut info = fault_siginfo(libc::SIGILL);
            info[8..12].copy_from_slice(&si_code.to_ne_bytes());
            guest.delivery.signal_info = Some(info);
            check(&mut guest, libc::SIGILL);
            assert_eq!(guest.delivery.asked.len(), 2, "both deliveries checked");
            (
                guest.delivery.asked[1].interruption,
                guest.thread.delivery_counters.interruption_from_registers,
            )
        };
        // SI_USER (0) and SI_TKILL (-6): a `kill` or `tgkill` pending at the
        // same exit.
        assert_eq!(interruption(0), (reverie::Interruption::AfterFrame, 0));
        assert_eq!(interruption(-6), (reverie::Interruption::AfterFrame, 0));
        // ILL_ILLOPN (2): the handler's first instruction faulted, or the
        // thread queued SIGILL to itself with that code (rt_tgsigqueueinfo),
        // pending at the same exit. Both take the registers' answer.
        assert_eq!(interruption(2), (reverie::Interruption::NotInSyscall, 1));
    }

    /// A caught force-delivered signal the copy holds blocked reaches its stop
    /// unblocked and reset to the default by the kernel: the copy is given up
    /// quietly, and nothing is checked.
    #[test]
    fn a_forced_signal_the_copy_blocks_gives_the_copy_up_unchecked() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        let segv = sigbit(libc::SIGSEGV);
        let action = KernelSigaction {
            handler: 0x4100,
            flags: (libc::SA_SIGINFO | 0x0400_0000) as u64,
            restorer: 0x7000,
            mask: 0,
        };
        kernel.set(sigbit(libc::SIGINT), 0, USR1 | segv);
        record_rt_sigaction(&mut guest, libc::SIGSEGV, Some(action), None, &Ok(0));
        guest.thread.signal_shadow.as_mut().unwrap().thread.blocked |= segv;
        let before = DIVERGENCES.with(Cell::get);
        // The instance dequeued can be a pending raise of the same signal
        // (SI_TKILL), the fault's own having been dropped as a duplicate: a
        // stop for a signal the copy blocks still shows the forced unblock.
        let mut raised = fault_siginfo(libc::SIGSEGV);
        raised[8..12].copy_from_slice(&libc::SI_TKILL.to_ne_bytes());
        guest.delivery.signal_info = Some(raised);
        check(&mut guest, libc::SIGSEGV);
        assert!(guest.delivery.asked.is_empty());
        assert!(guest.thread.signal_shadow.is_none());
        assert!(!guest.thread.signal_shadow_lost, "given up quietly");
        assert_eq!(DIVERGENCES.with(Cell::get), before);
        assert_eq!(
            guest
                .thread
                .delivery_counters
                .skipped
                .get("forced while blocked"),
            Some(&1)
        );
    }

    /// A force-delivered signal the copy holds ignored reaches its stop reset
    /// to the default by the kernel: the copy is given up quietly, and nothing
    /// is checked.
    #[test]
    fn a_forced_signal_the_copy_ignores_gives_the_copy_up_unchecked() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        let segv = sigbit(libc::SIGSEGV);
        let ignore = KernelSigaction {
            handler: 1,
            flags: 0x0400_0000,
            restorer: 0x7000,
            mask: 0,
        };
        kernel.set(sigbit(libc::SIGINT), segv, USR1);
        record_rt_sigaction(&mut guest, libc::SIGSEGV, Some(ignore), None, &Ok(0));
        assert!(guest.thread.signal_shadow.is_some());
        let before = DIVERGENCES.with(Cell::get);
        // A fault: the kernel generated it (SEGV_MAPERR).
        guest.delivery.signal_info = Some(fault_siginfo(libc::SIGSEGV));
        check(&mut guest, libc::SIGSEGV);
        assert!(guest.delivery.asked.is_empty());
        assert!(guest.thread.signal_shadow.is_none());
        assert!(!guest.thread.signal_shadow_lost, "given up quietly");
        assert_eq!(DIVERGENCES.with(Cell::get), before);
        assert_eq!(
            guest
                .thread
                .delivery_counters
                .skipped
                .get("forced while ignored"),
            Some(&1)
        );

        // The same number sent by a guest's kill (SI_USER) was not forced: the
        // copy is kept, and the ignored signal is not caught.
        let mut sent = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        kernel.set(sigbit(libc::SIGINT), segv, USR1);
        record_rt_sigaction(&mut sent, libc::SIGSEGV, Some(ignore), None, &Ok(0));
        let mut info = [0u8; reverie::SIGNAL_INFO_SIZE];
        info[0..4].copy_from_slice(&libc::SIGSEGV.to_ne_bytes());
        info[8..12].copy_from_slice(&libc::SI_USER.to_ne_bytes());
        sent.delivery.signal_info = Some(info);
        check(&mut sent, libc::SIGSEGV);
        assert!(sent.thread.signal_shadow.is_some(), "kept");
        assert_eq!(
            sent.thread.delivery_counters.skipped.get("not caught"),
            Some(&1)
        );
    }

    /// A force-delivered bit that the copy holds in only one of the frame and
    /// current masks (a temporary-mask wait) is exempt in that mask's
    /// comparison only, and the delivery is checked; the copy records it as
    /// it records an unchecked one.
    #[test]
    fn a_forced_bit_in_only_one_mask_is_exempt_in_that_mask_only() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        let mut unchecked = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        let trap = sigbit(libc::SIGTRAP);
        for g in [&mut guest, &mut unchecked] {
            let shadow = g.thread.signal_shadow.as_mut().unwrap();
            shadow.thread.blocked |= trap;
            shadow.thread.interrupted_under_temporary_mask(0);
            g.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
            g.delivery.orig_rax = Some(-1);
        }
        kernel.set(0, 0, USR1);
        check(&mut guest, libc::SIGUSR1);
        record_delivery(&mut unchecked, libc::SIGUSR1);
        let d = guest.delivery.asked[0];
        assert_eq!(
            d.frame_mask & trap,
            trap,
            "the frame's mask holds the forced bit"
        );
        assert_eq!(d.current_mask & trap, 0, "the wait's mask does not");
        assert_eq!(d.unchecked_mask, reserved());
        assert_eq!(
            (d.unchecked_frame_mask, d.unchecked_current_mask),
            (trap, 0),
            "exempt in the frame's comparison only"
        );
        assert_eq!(guest.thread.delivery_counters.approved, 1);
        assert!(guest.thread.delivery_counters.skipped.is_empty());
        assert_eq!(
            guest
                .thread
                .signal_shadow
                .as_ref()
                .map(|shadow| shadow.thread.clone()),
            unchecked
                .thread
                .signal_shadow
                .as_ref()
                .map(|shadow| shadow.thread.clone()),
        );
    }

    /// The unchecked signals are Reverie's preemption signal and the
    /// force-delivered signals the copy blocks; a forced signal the copy holds
    /// unblocked, and the delivered signal, stay checked.
    #[test]
    fn only_blocked_forced_signals_are_left_unchecked() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        let trap = sigbit(libc::SIGTRAP);
        guest.thread.signal_shadow.as_mut().unwrap().thread.blocked |= trap;
        guest.delivery.orig_rax = Some(-1);
        guest.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
        check(&mut guest, libc::SIGUSR1);
        let d = guest.delivery.asked[0];
        assert_eq!(d.unchecked_mask, reserved());
        assert_eq!(
            (d.unchecked_frame_mask, d.unchecked_current_mask),
            (trap, trap)
        );
    }

    /// An approval at a held signal's report counts as held as well.
    #[test]
    fn an_approval_at_a_held_report_is_counted_as_held() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        guest.delivery.report_path = Some(reverie::SignalReportPath::Held);
        guest.delivery.orig_rax = Some(-1);
        guest.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
        check(&mut guest, libc::SIGUSR1);
        let counters = &guest.thread.delivery_counters;
        assert_eq!((counters.approved, counters.held), (1, 1));
        let mut refused = catching(
            &kernel,
            reverie::SignalDeliveryOutcome::Unavailable(
                reverie::SignalDeliveryUnavailable::TracerFiltered,
            ),
        );
        refused.delivery.report_path = Some(reverie::SignalReportPath::Held);
        refused.delivery.orig_rax = Some(-1);
        refused.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
        check(&mut refused, libc::SIGUSR1);
        let counters = &refused.thread.delivery_counters;
        assert_eq!(
            (counters.approved, counters.held),
            (0, 0),
            "only an approval counts"
        );
    }

    /// A signal the backend passes on unreported is counted by its reason.
    #[test]
    fn unreported_signals_are_counted_by_reason() {
        let mut counters = DeliveryCounters::default();
        let dettid = crate::types::DetTid::from_raw(7);
        let count = |counters: &mut DeliveryCounters, reason| {
            counters.count_unreported(dettid, reason);
        };
        count(
            &mut counters,
            reverie::UnreportedSignalReason::HeldRestorePending,
        );
        count(
            &mut counters,
            reverie::UnreportedSignalReason::HeldRestorePending,
        );
        count(
            &mut counters,
            reverie::UnreportedSignalReason::DuringSyscallSkip,
        );
        assert_eq!(counters.owner, Some(dettid));
        assert_eq!(
            counters.unreported.into_iter().collect::<Vec<_>>(),
            vec![("during a syscall skip", 1), ("held, restore pending", 2)]
        );
    }

    /// A signal that is not caught, and a thread with no copy, are not
    /// checked; each is counted.
    #[test]
    fn uncaught_signals_and_threads_without_a_copy_are_not_checked() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        check(&mut guest, libc::SIGUSR2);
        assert!(guest.delivery.asked.is_empty());
        assert_eq!(
            guest.thread.delivery_counters.skipped.get("not caught"),
            Some(&1)
        );
        guest.thread.signal_shadow = None;
        check(&mut guest, libc::SIGUSR1);
        assert!(guest.delivery.asked.is_empty());
        assert_eq!(
            guest.thread.delivery_counters.skipped.get("no copy"),
            Some(&1)
        );
    }

    /// A copy that this delivery's reconciliation gives up is counted apart
    /// from a thread that had no copy at the stop.
    #[test]
    fn a_copy_given_up_by_the_deliverys_reconciliation_is_counted_apart() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
        guest.delivery.orig_rax = Some(-1);
        guest.thread.delivery_interruption = Some(reverie::Interruption::NotInSyscall);
        check(&mut guest, libc::SIGUSR1);
        assert_eq!(guest.thread.delivery_counters.approved, 1);
        // The handler's frame is open, and the kernel does not hold its mask.
        kernel.set(sigbit(libc::SIGINT) | sigbit(libc::SIGHUP), 0, USR1);
        check(&mut guest, libc::SIGUSR1);
        assert!(guest.thread.signal_shadow.is_none());
        assert_eq!(guest.delivery.asked.len(), 1);
        assert_eq!(
            guest
                .thread
                .delivery_counters
                .skipped
                .get("copy given up at this delivery"),
            Some(&1)
        );
        assert_eq!(guest.thread.delivery_counters.skipped.get("no copy"), None);
    }

    /// The interruption sent is Detcore's record where the registers say the
    /// thread is at that syscall (or in none, for a no-syscall record); the
    /// registers' own answer, counted, where there is no record, Detcore does
    /// not see every syscall, the thread has run on, or it is in a syscall
    /// Detcore cannot see; and the record as it stands where it disagrees with
    /// a known syscall, so the backend reports it. Only `orig_ax` -1 is no
    /// syscall.
    // x86_64 only: Detcore reads a delivery's interruption from the
    // registers only there (elsewhere it takes AfterFrame).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_interruption_is_the_record_where_the_registers_agree_on_the_syscall() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let read = reverie::Interruption::Return {
            syscall: libc::SYS_read,
            result: -512,
        };
        let cases = [
            (Some(read), libc::SYS_read, false, read, false),
            (
                Some(read),
                -1,
                false,
                reverie::Interruption::NotInSyscall,
                true,
            ),
            // A known syscall the record does not name: sent as it stands, so
            // the contradiction is reported.
            (Some(read), libc::SYS_write, false, read, false),
            // A syscall Detcore cannot see (a number outside Reverie's table):
            // the record names an earlier event, so the registers decide.
            (
                Some(read),
                1000,
                false,
                reverie::Interruption::KernelRun { syscall: 1000 },
                true,
            ),
            // Only -1 is no syscall; another negative number is a refused one.
            (
                Some(read),
                -5,
                false,
                reverie::Interruption::KernelRun { syscall: -5 },
                true,
            ),
            (
                None,
                libc::SYS_read,
                false,
                reverie::Interruption::KernelRun {
                    syscall: libc::SYS_read,
                },
                true,
            ),
            (None, -1, false, reverie::Interruption::NotInSyscall, true),
            (
                Some(read),
                libc::SYS_write,
                true,
                reverie::Interruption::KernelRun {
                    syscall: libc::SYS_write,
                },
                true,
            ),
            // A preemption record, but the thread is in a known syscall:
            // reported.
            (
                Some(reverie::Interruption::NotInSyscall),
                libc::SYS_read,
                false,
                reverie::Interruption::NotInSyscall,
                false,
            ),
            // In a syscall Detcore cannot see since the preemption.
            (
                Some(reverie::Interruption::NotInSyscall),
                1000,
                false,
                reverie::Interruption::KernelRun { syscall: 1000 },
                true,
            ),
            (
                Some(reverie::Interruption::NotInSyscall),
                -1,
                false,
                reverie::Interruption::NotInSyscall,
                false,
            ),
        ];
        for (record, orig, passthru, expected, from_registers) in cases {
            let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
            guest.config.passthru_opt = passthru;
            guest.thread.delivery_interruption = record;
            guest.delivery.orig_rax = Some(orig);
            check(&mut guest, libc::SIGUSR1);
            assert_eq!(
                guest.delivery.asked[0].interruption, expected,
                "record {record:?}, orig_rax {orig}, passthru {passthru}"
            );
            assert_eq!(
                guest.thread.delivery_counters.interruption_from_registers,
                u64::from(from_registers),
                "record {record:?}, orig_rax {orig}, passthru {passthru}"
            );
        }
    }

    /// The aarch64 side of `delivery_interruption`: it reads no registers
    /// there, so every delivery is sent as `AfterFrame`, which checks nothing,
    /// whatever the record, the syscall the thread is in and `--passthru-opt`,
    /// and counts as taken from the registers.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn without_register_reads_every_interruption_is_after_frame() {
        let kernel = Kernel::default();
        let _seam = kernel.install();
        let read = reverie::Interruption::Return {
            syscall: libc::SYS_read,
            result: -512,
        };
        let cases = [
            (Some(read), libc::SYS_read, false),
            (Some(read), -1, false),
            (None, libc::SYS_read, false),
            (Some(reverie::Interruption::NotInSyscall), -1, false),
            (Some(read), libc::SYS_write, true),
        ];
        for (record, orig, passthru) in cases {
            let mut guest = catching(&kernel, reverie::SignalDeliveryOutcome::Accepted);
            guest.config.passthru_opt = passthru;
            guest.thread.delivery_interruption = record;
            guest.delivery.orig_rax = Some(orig);
            check(&mut guest, libc::SIGUSR1);
            assert_eq!(
                guest.delivery.asked[0].interruption,
                reverie::Interruption::AfterFrame,
                "record {record:?}, orig {orig}, passthru {passthru}"
            );
            assert_eq!(
                guest.thread.delivery_counters.interruption_from_registers, 1,
                "record {record:?}, orig {orig}, passthru {passthru}"
            );
            assert_eq!(guest.delivery.registers_read, 0, "no register is read");
        }
    }
}
