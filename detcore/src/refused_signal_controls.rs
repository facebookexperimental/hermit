/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Guest signal controls an in-guest runtime refused before Detcore saw the
//! call, reported once per run.
//!
//! On the in-guest backends the runtime answers a guest's handler-installing
//! `rt_sigaction`, and an alternate signal stack, with `EPERM` itself
//! (reverie-inguest `protect_runtime_control`): a guest handler would run
//! outside the Tool's view. Until A5 step 3c makes guest handlers supported,
//! each refusal is announced once per run, by name, so that a run whose guest
//! carries on after the `EPERM` (many do, Rust's standard library among them)
//! is not silently different from Linux.
//!
//! The runtime records each refusal; the runtime crate that hosts Detcore
//! installs the function that takes them ([`install`]); Detcore takes them at
//! each syscall entry (`report`) and tells the coordinator, which prints one
//! WARN per refused signal and one for the alternate stack, per run. No other
//! backend installs a source, so nothing is reported there.
//!
//! A refusal is reported at the process's next syscall Detcore sees, which is
//! its exit call at the latest when it exits normally. A process that dies
//! before then reports nothing: one killed from outside, and one that faults
//! in the signal it asked to handle (SIGSEGV's default action is delivered by
//! the runtime while it traps instructions and by the kernel otherwise; the
//! kernel ends a process on SIGBUS, SIGFPE or SIGILL), because neither path
//! reaches Detcore; a thread's exit hook does not run on those deaths either,
//! so it is not used.
//!
//! The texts of Detcore's own refusals of the in-guest runtime's reserved
//! signals (`Config::in_guest_reserved_signals`: a send to another process,
//! or an arming) live here too. Detcore decides those itself, at the call,
//! and prints each through its guest WARN path (`crate::guest_log`).

use std::sync::OnceLock;

use reverie::Guest;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;

/// What a runtime refused since it was last asked: the signals whose new
/// action it refused, bit `n - 1` for signal `n`, and whether it refused an
/// alternate signal stack.
pub type RefusedSignalControls = (u64, bool);

static SOURCE: OnceLock<fn() -> RefusedSignalControls> = OnceLock::new();

/// Installs `source`, which takes the signal controls the runtime refused
/// since it was last called. The runtime crate calls this once per process
/// image; a forked child inherits it. Returns false if one was installed
/// already.
pub fn install(source: fn() -> RefusedSignalControls) -> bool {
    SOURCE.set(source).is_ok()
}

/// Takes what the runtime refused since the last call: nothing where no
/// source is installed.
fn take() -> RefusedSignalControls {
    SOURCE.get().map_or((0, false), |source| source())
}

/// At a syscall's entry, tells the coordinator what the runtime refused since
/// the last report (`GlobalRequest::AnnounceRefusedSignalControls`). The
/// message carries no logical time and is answered before any accounting, so
/// whether a run reports changes neither its schedule nor its records.
pub(crate) async fn report<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (actions, alternate_stack) = take();
    if actions == 0 && !alternate_stack {
        return;
    }
    crate::tool_global::announce_refused_signal_controls(guest, actions, alternate_stack).await;
}

/// The WARN for a signal whose new action the runtime refused: a handler, or
/// any action for a signal the runtime keeps for itself (SIGSYS, and SIGSEGV
/// while it traps instructions).
pub(crate) fn handler_refused_warning(signum: i32) -> String {
    let name = signal_name(signum);
    format!(
        "HERMIT_SIGNAL_HANDLER_REFUSED signum={signum} ({name}): the guest asked for a new \
         action for it (a handler, or any action for a signal the in-guest runtime keeps for \
         itself) and received EPERM, so the handler was not installed: in-guest signal \
         handlers are unsupported until A5 step 3c (https://github.com/rrnewton/hermit/issues/4485). \
         A {name} the guest receives takes the action it had before."
    )
}

/// The WARN for a guest handler that Detcore refused with EPERM because the
/// run's threads are not sequentialized (A5 step 3c slice 0g, design 16.2):
/// `caller` is the calling thread. Printed for every refused install, through
/// Detcore's guest WARN path (`crate::guest_log`), like the reserved-signal
/// refusals: Detcore decides it itself, at the call.
pub(crate) fn unsequentialized_handler_refused_warning(signum: i32, caller: i32) -> String {
    let name = signal_name(signum);
    format!(
        "HERMIT_UNSEQUENTIALIZED_HANDLER_REFUSED signum={signum} ({name}) caller={caller}: the \
         guest asked for a handler and received EPERM, so the handler was not installed: threads \
         are not sequentialized (--no-sequentialize-threads, or --strace-only, which turns \
         sequentialization off; for a library caller, Config::sequentialize_threads is false), \
         and an in-guest guest handler needs Detcore's scheduler to run one guest thread at a \
         time. A named refusal, not Linux's behaviour (A5 step 3c, \
         https://github.com/rrnewton/hermit/issues/4485). A {name} the guest receives takes the \
         action it had before."
    )
}

/// The WARN for an alternate signal stack change the runtime refused.
pub(crate) fn alternate_stack_refused_warning() -> String {
    "HERMIT_SIGNAL_ALTSTACK_REFUSED: the guest asked to set or change its alternate signal \
     stack and received EPERM, so it was not installed: in-guest signal handlers are \
     unsupported until A5 step 3c (https://github.com/rrnewton/hermit/issues/4485)."
        .to_string()
}

/// The WARN for a send of a reserved in-guest signal
/// (`Config::in_guest_reserved_signals`) from the caller's process to
/// another, which Detcore refused with EPERM: `call` is the system call,
/// `sender` the calling thread and `target` the process it named. Printed for
/// every refused send, through Detcore's guest WARN path
/// (`crate::guest_log`), not once per run: each names its own target.
pub(crate) fn reserved_signal_send_refused_warning(
    call: &str,
    signum: i32,
    sender: i32,
    target: i32,
) -> String {
    let name = signal_name(signum);
    format!(
        "HERMIT_RESERVED_SIGNAL_SEND_REFUSED signum={signum} ({name}) call={call} sender={sender} \
         target=process {target}: received EPERM and nothing was sent. The in-guest runtime keeps \
         {name} for itself, and a {name} from another process would run the runtime's handler in \
         process {target} outside the schedule. A named refusal, not Linux's behaviour \
         (A5 step 3c, https://github.com/rrnewton/hermit/issues/4485)."
    )
}

/// A call that arms a notification signal, as the reserved-signal refusal
/// names it (`Detcore::refuse_reserved_signal_arming`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReservedArming {
    /// `fcntl(F_SETSIG)`: the descriptor's I/O signal.
    FcntlSetsig,
    /// `timer_create` with a signal notification.
    TimerCreate,
    /// `prctl(PR_SET_PDEATHSIG)`: the parent-death signal.
    PrctlPdeathsig,
}

impl ReservedArming {
    /// The call and operation, as the WARN names them.
    pub(crate) fn call(self) -> &'static str {
        match self {
            Self::FcntlSetsig => "fcntl(F_SETSIG)",
            Self::TimerCreate => "timer_create",
            Self::PrctlPdeathsig => "prctl(PR_SET_PDEATHSIG)",
        }
    }

    /// Who would send the signal, and when.
    fn sender(self) -> &'static str {
        match self {
            Self::FcntlSetsig => {
                "the kernel would send it for the descriptor's I/O events, at moments host timing \
                 decides"
            }
            Self::TimerCreate => "Detcore's virtual timer would deliver it at each expiry",
            Self::PrctlPdeathsig => {
                "the kernel would send it when the parent thread exits, at a moment host timing \
                 decides"
            }
        }
    }
}

/// The WARN for an arming of a reserved in-guest signal as a notification
/// signal by `arming`, which Detcore refused with EPERM.
pub(crate) fn reserved_signal_arming_refused_warning(
    arming: ReservedArming,
    signum: i32,
    caller: i32,
) -> String {
    let name = signal_name(signum);
    let call = arming.call();
    let sender = arming.sender();
    format!(
        "HERMIT_RESERVED_SIGNAL_ARMING_REFUSED signum={signum} ({name}) call={call} \
         caller={caller}: received EPERM and nothing was armed. The in-guest runtime keeps {name} \
         for itself: {sender}, and the runtime's handler, not the guest's action, would take it. \
         A named refusal, not Linux's behaviour \
         (A5 step 3c, https://github.com/rrnewton/hermit/issues/4485)."
    )
}

/// `signum`'s name: `SIGUSR1`, or, for a realtime signal, its kernel number
/// (C libraries reserve different numbers of them, so `SIGRTMIN+n` would
/// depend on the guest's).
fn signal_name(signum: i32) -> String {
    match nix::sys::signal::Signal::try_from(signum) {
        Ok(signal) => signal.as_str().to_string(),
        Err(_) => format!("realtime signal {signum}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warnings_name_the_signal_the_errno_and_why() {
        let usr1 = handler_refused_warning(libc::SIGUSR1);
        assert!(usr1.starts_with("HERMIT_SIGNAL_HANDLER_REFUSED signum=10 (SIGUSR1): "));
        let stack = alternate_stack_refused_warning();
        assert!(stack.starts_with("HERMIT_SIGNAL_ALTSTACK_REFUSED: "));
        for text in [&usr1, &stack] {
            assert!(text.contains("received EPERM"), "{text}");
            assert!(text.contains("was not installed"), "{text}");
            assert!(
                text.contains("in-guest signal handlers are unsupported until A5 step 3c"),
                "{text}"
            );
        }
        assert!(handler_refused_warning(36).contains("(realtime signal 36)"));

        let send = reserved_signal_send_refused_warning("tgkill", libc::SIGSYS, 4, 5);
        assert!(
            send.starts_with(
                "HERMIT_RESERVED_SIGNAL_SEND_REFUSED signum=31 (SIGSYS) call=tgkill sender=4 \
                 target=process 5: received EPERM"
            ),
            "{send}"
        );
        let arming =
            reserved_signal_arming_refused_warning(ReservedArming::FcntlSetsig, libc::SIGSEGV, 4);
        assert!(
            arming.starts_with(
                "HERMIT_RESERVED_SIGNAL_ARMING_REFUSED signum=11 (SIGSEGV) \
                 call=fcntl(F_SETSIG) caller=4: received EPERM"
            ),
            "{arming}"
        );
        // Each arming names who would send the signal: host timing for the
        // kernel's I/O and parent-death signals, never for Detcore's timers.
        assert!(
            arming.contains("at moments host timing decides"),
            "{arming}"
        );
        let timer =
            reserved_signal_arming_refused_warning(ReservedArming::TimerCreate, libc::SIGSYS, 4);
        assert!(timer.contains("call=timer_create caller=4"), "{timer}");
        assert!(
            timer.contains("Detcore's virtual timer would deliver it"),
            "{timer}"
        );
        assert!(!timer.contains("host timing"), "{timer}");
        let pdeathsig = reserved_signal_arming_refused_warning(
            ReservedArming::PrctlPdeathsig,
            libc::SIGTRAP,
            4,
        );
        assert!(
            pdeathsig.contains("call=prctl(PR_SET_PDEATHSIG) caller=4"),
            "{pdeathsig}"
        );
        assert!(
            pdeathsig.contains("when the parent thread exits"),
            "{pdeathsig}"
        );
        let unsequentialized = unsequentialized_handler_refused_warning(libc::SIGALRM, 4);
        assert!(
            unsequentialized.starts_with(
                "HERMIT_UNSEQUENTIALIZED_HANDLER_REFUSED signum=14 (SIGALRM) caller=4: the guest \
                 asked for a handler and received EPERM, so the handler was not installed: \
                 threads are not sequentialized"
            ),
            "{unsequentialized}"
        );
        // It names what turns sequentialization off, for the CLI and for a
        // library caller.
        for cause in [
            "--no-sequentialize-threads",
            "--strace-only",
            "Config::sequentialize_threads",
        ] {
            assert!(unsequentialized.contains(cause), "{unsequentialized}");
        }
        for text in [&send, &arming, &timer, &pdeathsig, &unsequentialized] {
            assert!(
                text.contains("A named refusal, not Linux's behaviour"),
                "{text}"
            );
            assert!(
                text.contains("https://github.com/rrnewton/hermit/issues/4485"),
                "{text}"
            );
            for backend in ["liteinst", "LiteInst", "in-guest-trap", "sabre", "SaBRe"] {
                assert!(!text.contains(backend), "{text}");
            }
        }
        // No backend is named: the text is the same wherever it is printed.
        for text in [usr1, stack] {
            for backend in ["liteinst", "LiteInst", "in-guest-trap", "sabre", "SaBRe"] {
                assert!(!text.contains(backend), "{text}");
            }
        }
    }
}
