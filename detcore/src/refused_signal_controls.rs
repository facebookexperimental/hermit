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

/// The run-wide stopping refusal (A5 design D49) for a syscall that, under
/// the in-guest delivery gate, finished with one of the kernel's internal
/// restart codes other than `ERESTARTSYS` although no wait recorded a signal
/// ending it (design D40): the in-guest runtime would hand the raw code to
/// the guest, since only `ERESTARTSYS` is re-run there. `ERESTARTSYS` itself
/// passes through as that re-run.
pub(crate) fn restart_code_without_signal_refusal(call: &str, code: &str, caller: i32) -> String {
    format!(
        "hermit refused to continue the run (HERMIT_RESTART_CODE_WITHOUT_SIGNAL): {call} in \
         thread {caller} finished with {code} under the in-guest delivery gate, but no wait \
         recorded a signal that ended it, so the in-guest runtime would return the kernel's \
         internal restart code to the guest, which Linux never does \
         (A5 design D40, https://github.com/rrnewton/hermit/issues/4485)."
    )
}

/// The run-wide stopping refusal (A5 design D49) for a wait whose end the
/// restart checkpoint cannot resolve (`wait_signal_end::DequeueWalk::resolve`,
/// design D40): a caught signal is delivered first and a later delivery
/// (`behind`) could end the process, depending on the caught action, and the
/// query of that action failed. Fail closed: the run ends rather than guess.
pub(crate) fn caught_action_unreadable_refusal(
    call: &str,
    caught: i32,
    behind: i32,
    caller: i32,
) -> String {
    format!(
        "hermit refused to continue the run (HERMIT_CAUGHT_ACTION_UNREADABLE): {call} in \
         thread {caller} ended for caught signal {caught} ({}) with signal {behind} ({}) \
         deliverable behind it, and the caught signal's action could not be read, so whether \
         the process dies at the return cannot be decided (A5 design D40, \
         https://github.com/rrnewton/hermit/issues/4485).",
        signal_name(caught),
        signal_name(behind),
    )
}

/// The run-wide stopping refusal (A5 design D49, with D48) for a delivery
/// loop that dequeues a second instance of caught signal `signal` after
/// `SA_RESETHAND` reset its action to its default, a stop
/// (`wait_signal_end::WalkRefusal::ResetToStop`): the in-guest backend cannot
/// stop a guest process on the schedule.
pub(crate) fn signal_reset_to_stop_refusal(call: &str, signal: i32, caller: i32) -> String {
    format!(
        "hermit refused to continue the run (HERMIT_SIGNAL_RESET_TO_STOP): {call} in thread \
         {caller} ended for caught signal {signal} ({}), whose SA_RESETHAND resets it to its \
         default action, a stop, and a second instance of it is pending unblocked, which Linux \
         would then deliver by stopping the process; the in-guest backend cannot stop a guest \
         process on the schedule (A5 design D40, D48, \
         https://github.com/rrnewton/hermit/issues/4485).",
        signal_name(signal),
    )
}

/// The run-wide stopping refusal (A5 design D49) for a caught real-time
/// signal whose action has `SA_RESETHAND` and leaves the signal unblocked
/// after its delivery (`wait_signal_end::WalkRefusal::RealtimeInstances`):
/// another queued instance of it would be dequeued at its default action,
/// termination, and `/proc` shows only that one instance or more is pending,
/// so whether the process dies at the return cannot be decided.
pub(crate) fn realtime_instances_unknown_refusal(call: &str, signal: i32, caller: i32) -> String {
    format!(
        "hermit refused to continue the run (HERMIT_REALTIME_INSTANCES_UNKNOWN): {call} in \
         thread {caller} ended for caught real-time signal {signal} ({}), whose SA_RESETHAND \
         resets it to its default action, termination, while it stays unblocked after its \
         delivery; a second queued instance would then end the process, and how many \
         instances are queued cannot be read (A5 design D40, \
         https://github.com/rrnewton/hermit/issues/4485).",
        signal_name(signal),
    )
}

/// The run-wide stopping refusal (A5 design D49) for a caught end whose
/// restart table restarts the call with a code other than `ERESTARTSYS`
/// (D40): the restart needs the register rewind and post-handler delivery of
/// step 3c.2, and the in-guest Tool host would hand the raw code to the
/// guest. Fail closed.
pub(crate) fn restart_after_handler_refusal(
    call: &str,
    signal: i32,
    code: &str,
    caller: i32,
) -> String {
    format!(
        "hermit refused to continue the run (HERMIT_RESTART_AFTER_HANDLER): {call} in thread \
         {caller} finished with {code} and caught signal {signal} ({}) ends it, so Linux would \
         run the handler and then restart the call, which needs a register rewind and a \
         delivery after the handler that the in-guest delivery gate does not provide before A5 \
         step 3c.2; the guest would otherwise see the kernel's internal restart code (A5 design \
         D40, https://github.com/rrnewton/hermit/issues/4485).",
        signal_name(signal),
    )
}

/// The run-wide stopping refusal (A5 design D49) for a wait under a
/// temporary signal mask (`rt_sigsuspend`, a masked `pselect6`, `ppoll`,
/// `epoll_pwait` or `epoll_pwait2`) that caught signal `signal` would end
/// under the in-guest delivery gate (step 3c.1d): Linux would run its handler
/// with the temporary mask in force and restore the thread's own mask only at
/// the handler's `rt_sigreturn`, which step 3c.w provides. 3c.1 never reaches
/// it: phase 1 refuses every temporary-mask call in a process that handles
/// SIGALRM (design F8), and no other guest handler is admitted. Fail closed.
pub(crate) fn temporary_mask_caught_end_refusal(caller: i32, signal: i32) -> String {
    format!(
        "hermit refused to continue the run (HERMIT_TEMPORARY_MASK_CAUGHT_END): a wait under a \
         temporary signal mask in thread {caller} would end for caught signal {signal} ({}), \
         whose handler Linux runs under that mask; the in-guest delivery gate does not run \
         guest handlers under a temporary mask before A5 step 3c.w (A5 design D39, \
         https://github.com/rrnewton/hermit/issues/4485).",
        signal_name(signal),
    )
}

/// The run-wide stopping refusal (A5 design D49) for an argument of `call`
/// under the in-guest delivery gate (step 3c.1d, round-1 ruling 4) that
/// Detcore's own read could not reach although Linux's copy would not fault
/// (a write-only mapping, a byte below a grow-down mapping), or whose
/// caller's mappings could not be read to tell: Linux's answer is not known,
/// so the run ends rather than answer EFAULT where Linux would not.
pub(crate) fn unreadable_syscall_argument_refusal(
    call: &str,
    what: &str,
    address: usize,
    caller: i32,
) -> String {
    format!(
        "hermit refused to continue the run (HERMIT_UNREADABLE_SYSCALL_ARGUMENT): {call} in \
         thread {caller} names {what} at {address:#x}, which Hermit cannot read although \
         Linux's copy would not fault there (or the caller's mappings could not be read to \
         tell), so Linux's answer is not known (A5 design D39, \
         https://github.com/rrnewton/hermit/issues/4485)."
    )
}

/// The run-wide stopping refusal (A5 design D49) of a `/proc` status read
/// under the in-guest delivery gate whose thread's guest mask the scheduler
/// holds as unknown (A5 step 3c.1e A, IR-18): it was readmitted on a rejoin
/// that returned under its own mask but published none, so neither the mask
/// its call published (possibly a temporary one Linux has taken down) nor
/// the kernel's `SigBlk` (the in-guest runtime's mask once it has one) is the
/// guest's. Linux's answer is not known, so the run ends rather than show
/// either. Defect-only once step 3c.1e B guarantees a published mask on every
/// gated backend.
pub(crate) fn guest_mask_unknown_refusal(reader: i32, thread: i32) -> String {
    format!(
        "hermit refused to continue the run (HERMIT_GUEST_MASK_UNKNOWN): thread {reader} reads \
         the /proc status of thread {thread}, whose signal mask Hermit does not know (it \
         returned from a call outside the schedule under its own mask without publishing it), \
         so SigBlk cannot be shown as Linux shows it (A5 step 3c.1e, IR-18, \
         https://github.com/rrnewton/hermit/issues/4485)."
    )
}

/// The run-wide stopping refusal (A5 design D49) of a masked wait's end under
/// the in-guest delivery gate (A5 step 3c.1e A, D37's R2) where one ignored
/// real-time signal `signal`, outside the call's temporary mask, has both an
/// instance kept pending from the call's entry and one sent to the same
/// `queue` while the temporary mask was published: Linux keeps the first and
/// discarded the second at send (`sig_ignored`, kernel/signal.c:104-120 at
/// v6.12), but the queue's pending bit cannot tell them apart, and a removal
/// would dequeue the older one first. Fail closed.
pub(crate) fn ignored_realtime_instances_refusal(caller: i32, signal: i32, queue: &str) -> String {
    format!(
        "hermit refused to continue the run (HERMIT_IGNORED_REALTIME_INSTANCES): a wait under a \
         temporary signal mask in thread {caller} ends with ignored real-time signal {signal} \
         ({}) pending in its {queue} queue both from before the call and from a send during \
         it, which Linux keeps and discards respectively; Hermit cannot remove only the second \
         (A5 step 3c.1e, D37, https://github.com/rrnewton/hermit/issues/4485).",
        signal_name(signal),
    )
}

/// The WARN of A5 step 3c.1e A, IR-15: a two-phase delivery's announcement
/// of a default-action signal, or a predicted death, for a thread of a
/// process that is the init of a PID namespace it created with
/// `CLONE_NEWPID`. Linux marks such an init `SIGNAL_UNKILLABLE`
/// (kernel/fork.c:2558-2560 at v6.12): a signal whose action is `SIG_DFL` is
/// ignored when it is sent unless blocked (`sig_task_ignored`,
/// kernel/signal.c:92-94, through `sig_ignored`, :104-120 and :111), and a
/// blocked one is discarded at dequeue (`get_signal`, :2843-2845). SIGKILL
/// and SIGSTOP are ignored the same way unless the send is forced
/// (`!(force && sig_kernel_only(sig))`, :93), which it is from an ancestor
/// PID namespace or the kernel (`send_signal_locked`, :1217-1224), a sender
/// the hook does not see. No default-action delivery or death can be
/// predicted for any of them; the hook is refused with `EPROTONOSUPPORT`.
pub(crate) fn pid_namespace_init_prediction_refused_warning(
    caller: i32,
    hook: &str,
    signal: i32,
) -> String {
    format!(
        "hermit refused an in-guest delivery (HERMIT_PID_NAMESPACE_INIT_PREDICTION): {hook} for \
         signal {signal} ({}), which takes its default action, in thread {caller}, whose \
         process is the init of a PID namespace it created, where Linux ignores or discards \
         a default-action signal (SIGNAL_UNKILLABLE), SIGKILL and SIGSTOP too unless sent \
         from an ancestor PID namespace or the kernel; the hook returns \
         EPROTONOSUPPORT (A5 step 3c.1e, IR-15, https://github.com/rrnewton/hermit/issues/4485).",
        signal_name(signal),
    )
}

/// The named refusal (A5 design D39) of a masked `pselect6` under the
/// in-guest delivery gate that Detcore cannot serve on staged copies: more
/// than 64 descriptors and a nonzero timeout with nothing ready (Detcore polls
/// only the first word of each set on its internal path, and the kernel would
/// sleep under the temporary mask, which the in-guest runtime cannot let it
/// install), or more than 1024 descriptors to copy. `why` says which. The
/// call fails with ENOSYS after Linux's own argument checks (and, for the
/// first, a first poll that found nothing ready and no signal that ends it).
pub(crate) fn masked_pselect6_nfds_refused_warning(caller: i32, nfds: i32, why: &str) -> String {
    format!(
        "hermit refused a call (HERMIT_MASKED_PSELECT6_NFDS_REFUSED): pselect6 in thread \
         {caller} with a temporary signal mask and {nfds} descriptors, with {why}, would need \
         the kernel to wait or copy under that mask, which the in-guest delivery gate cannot \
         run; the call returns ENOSYS (A5 design D39, \
         https://github.com/rrnewton/hermit/issues/4485)."
    )
}

/// The run-wide stopping refusal (A5 design D44(b) through D49) for a send
/// of `signum` to thread `target`, blocked in a kernel-run call (a vfork
/// parent's wait when `vfork`) whose continuation turn has not been granted:
/// the call runs in the kernel under the in-guest runtime's mask, so the
/// signal would stay pending and the call would never end for it, where Linux
/// interrupts it. `call` is the sending call (or "the scheduler" for its own
/// send), `sender` the sending thread. `unreadable`: the target's dispositions
/// could not be read, which refuses, fail closed.
pub(crate) fn kernel_run_signal_refusal(
    call: &str,
    signum: i32,
    sender: Option<i32>,
    target: i32,
    vfork: bool,
    unreadable: bool,
) -> String {
    let name = signal_name(signum);
    let sender = sender.map_or_else(String::new, |sender| format!(" by thread {sender}"));
    let blocked_in = if vfork {
        "a vfork parent's wait for its child"
    } else {
        "a call the kernel runs outside the schedule"
    };
    let why = if unreadable {
        "its signal dispositions could not be read, so the send is refused, fail closed"
    } else {
        "Linux would interrupt the call for it, but the in-guest runtime's mask holds it pending \
         for as long as the call blocks"
    };
    format!(
        "hermit refused to continue the run (HERMIT_KERNEL_RUN_SIGNAL_REFUSED): signal {signum} \
         ({name}) sent{sender} through {call} to thread {target}, which is blocked in {blocked_in} \
         and has not been granted its continuation turn: {why} \
         (A5 design D44, https://github.com/rrnewton/hermit/issues/4485)."
    )
}

/// Why a stop-signal send was refused (`stop_signal_send_refused_warning`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopSendRefusal {
    /// SIGSTOP, which always stops.
    Sigstop,
    /// SIGTSTP, SIGTTIN or SIGTTOU while the target's action is `SIG_DFL`, as
    /// its `/proc/<tid>/status` `SigIgn`/`SigCgt` say in the sender's turn.
    DefaultAction,
    /// The target's `/proc/<tid>/status` could not be read, so whether its
    /// action is `SIG_DFL` is unknown: refused, fail closed.
    StatusUnreadable,
}

/// The WARN for a send of a stop signal that Detcore refused with EPERM
/// under the in-guest delivery gate (A5 design D48, IR-4): `call` is the
/// system call, `sender` the calling thread, `target` the thread or process it
/// named. A program that ignores the EPERM and then waits for the target with
/// `waitpid(WUNTRACED)` waits until the run's wall timeout (IR-4(c)).
pub(crate) fn stop_signal_send_refused_warning(
    call: &str,
    signum: i32,
    sender: i32,
    target: i32,
    why: StopSendRefusal,
) -> String {
    let name = signal_name(signum);
    let why = match why {
        StopSendRefusal::Sigstop => "SIGSTOP always stops its target".to_string(),
        StopSendRefusal::DefaultAction => format!(
            "the target's action for {name} is SIG_DFL (its /proc status SigIgn and SigCgt, read in \
             the sender's turn), which stops it"
        ),
        StopSendRefusal::StatusUnreadable => format!(
            "the target's /proc status could not be read, so whether its action for {name} is \
             SIG_DFL is unknown, and the send is refused, fail closed"
        ),
    };
    format!(
        "HERMIT_STOP_SIGNAL_SEND_REFUSED signum={signum} ({name}) call={call} sender={sender} \
         target={target}: received EPERM and nothing was sent: {why}, and the in-guest backend \
         cannot stop a guest process on the schedule. A named refusal, not Linux's behaviour \
         (A5 design D48, https://github.com/rrnewton/hermit/issues/4485)."
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
