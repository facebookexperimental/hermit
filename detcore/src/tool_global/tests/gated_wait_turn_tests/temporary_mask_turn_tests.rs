/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1d: the temporary-mask forms and the ignored-signal rules under
//! the in-guest delivery gate (`Config::in_guest_signal_delivery`; design
//! D37, D39, D50, IR-7, IR-9, IR-22), on the waiting thread's own side, with
//! the parent module's harness: the gate forced on in the Detcore `Config`,
//! `/proc` reads scripted, the guest's own mask a chosen value, scheduler
//! RPCs answered and logged, and the injections a kernel would answer (the
//! mask and action probes, `rt_sigtimedwait`, the poll probes) answered by
//! the harness and recorded.

use reverie::syscalls::RtSigaction;
use reverie::syscalls::RtSigsuspend;

use super::*;

/// A scripted kernel answer to a probe (`None`: no probe expected).
type Probe = Option<Result<i64, Errno>>;
/// A call's expected result.
type Answer = Result<i64, Errno>;
/// A `ppoll` row: name, timeout (ns), pollfd buffer, pending, probe,
/// expected, death, removed.
type PpollRow = (&'static str, u64, usize, u64, Probe, Answer, bool, u64);
/// An `epoll_pwait` row: name, timeout (ms), `/proc` states by probe count,
/// probe, expected, death, removed.
type EpollRow = (
    &'static str,
    i32,
    Vec<KernelSignalState>,
    Probe,
    Answer,
    bool,
    Vec<u64>,
);

/// Answer this thread's `/proc` reads of `THREAD` with `states` in turn,
/// the last one for every read after.
fn script_seq(states: Vec<KernelSignalState>) -> signal_state_read_seam::Installed {
    let mut states = VecDeque::from(states);
    signal_state_read_seam::install(move |pid, tid| {
        if pid.as_raw() != THREAD.as_raw() || tid.as_raw() != THREAD.as_raw() {
            return None;
        }
        let state = if states.len() > 1 {
            states.pop_front()
        } else {
            states.front().copied()
        };
        state.map(Ok)
    })
}

/// A leaked eight-byte cell holding `mask`, where a call names its mask.
fn mask_cell(mask: u64) -> usize {
    Box::leak(Box::new(mask)) as *mut u64 as usize
}

fn suspend(mask: u64) -> RtSigsuspend {
    RtSigsuspend::new()
        .with_mask(Addr::from_raw(mask_cell(mask)))
        .with_sigsetsize(8)
}

/// The requests the parked `rt_sigsuspend` filed.
fn suspend_requests(guest: &TurnGuest) -> Vec<Resources> {
    guest
        .requests()
        .into_iter()
        .filter(|request| request.fyi == crate::resources::RT_SIGSUSPEND_FYI)
        .collect()
}

/// Design D39 and the 3c.1d gate "rt_sigsuspend's eligibility uses the
/// temporary mask": under the gate `rt_sigsuspend` is parked, never run by
/// the kernel, and ends exactly for a default-fatal signal its temporary mask
/// lets in.
///
/// - (a) SIGTERM pending, blocked by the guest's own mask and let in by the
///   temporary one: the wait ends at once, `-ERESTARTNOHAND` with the D40
///   flag `Fatal(SIGTERM)`, nothing filed; the restart checkpoint turns it
///   into `-EINTR` with the interrupted code.
/// - (b) SIGTERM let in by the own mask at entry: D41's check under the own
///   mask still runs first, and the call is not run (`ERESTARTNOINTR`).
/// - (c) SIGTERM blocked by both: the wait parks with a request tagged
///   `RT_SIGSUSPEND_FYI` that publishes the temporary mask, and after a wake
///   ends for SIGQUIT, which the temporary mask lets in.
///
/// Mutation "the own mask decides eligibility": (a) parks and (c) ends at
/// once for nothing it can see. Mutation "the request publishes nothing":
/// (c)'s publication assertion fails.
#[tokio::test]
async fn under_the_gate_rt_sigsuspend_parks_and_decides_by_its_temporary_mask() {
    let term = bit(libc::SIGTERM);
    let quit = bit(libc::SIGQUIT);
    let call = Syscall::from(suspend(0));

    // (a)
    let _seam = script(proc_state(term, 0, 0));
    let mut guest = turn_guest(true, Some(term));
    let tool = super::tool(&guest);
    let result = tool.handle_rt_sigsuspend(&mut guest, suspend(0)).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND))),
        "(a): {result:?}"
    );
    assert_eq!(
        guest.thread.wait_signal_end,
        Some(WaitSignalEnd::Fatal(libc::SIGTERM))
    );
    assert!(suspend_requests(&guest).is_empty(), "(a): nothing filed");
    let (res, code) = tool.wait_end_checkpoint(&mut guest, &call, result).await;
    assert!(
        matches!(res, Err(Error::Errno(Errno::EINTR))),
        "(a): {res:?}"
    );
    assert_eq!(code, Some(Errno::ERESTARTNOHAND));
    drop(_seam);

    // (b)
    let _seam = script(proc_state(term, 0, 0));
    let mut guest = turn_guest(true, Some(0));
    let tool = super::tool(&guest);
    let result = tool.handle_rt_sigsuspend(&mut guest, suspend(term)).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ERESTARTNOINTR))),
        "(b): {result:?}"
    );
    assert!(guest.injected.is_empty(), "(b): not even the mask is read");
    drop(_seam);

    // (c): reads are D41's, the first check's, and the woken check's.
    let _seam = script_seq(vec![
        proc_state(term, 0, 0),
        proc_state(term, 0, 0),
        proc_state(term | quit, 0, 0),
    ]);
    let mut guest = turn_guest(true, Some(term));
    guest
        .answers
        .lock()
        .unwrap()
        .push_back(ResumeStatus::Signaled(None));
    let tool = super::tool(&guest);
    let result = tool.handle_rt_sigsuspend(&mut guest, suspend(term)).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND))),
        "(c): {result:?}"
    );
    assert_eq!(
        guest.thread.wait_signal_end,
        Some(WaitSignalEnd::Fatal(libc::SIGQUIT))
    );
    let filed = suspend_requests(&guest);
    assert_eq!(filed.len(), 1, "(c): parked once");
    assert_eq!(filed[0].blocked_signal_mask, Some(term));
    assert_eq!(
        filed[0].guest_signal_mask,
        Some(term),
        "(c): the request publishes the temporary mask"
    );
    assert!(matches!(
        filed[0].resources.keys().next(),
        Some(ResourceID::SleepUntil(deadline)) if deadline.is_indefinite()
    ));
}

/// Round-1 review (lane ruling 1): the parked `rt_sigsuspend` decides by,
/// and publishes, the mask Linux installs, which never holds SIGKILL or
/// SIGSTOP (`set_current_blocked`, kernel/signal.c:3089-3092 at v6.12, from
/// `sigsuspend`, :4700). The common `sigfillset(); sigdelset(SIGTERM);
/// sigsuspend()` pattern publishes the full mask less SIGTERM, SIGKILL and
/// SIGSTOP, so a cross-process `/proc` status, which shows the published mask
/// as `SigBlk` under the gate, has bits 9 and 19 clear as on Linux. Mutation
/// "the guest's mask published as written": the published mask has them.
#[tokio::test]
async fn a_parked_rt_sigsuspend_publishes_the_mask_linux_installs() {
    let term = bit(libc::SIGTERM);
    let unblockable = bit(libc::SIGKILL) | bit(libc::SIGSTOP);
    let _seam = script_seq(vec![
        proc_state(0, 0, 0),
        proc_state(0, 0, 0),
        proc_state(term, 0, 0),
    ]);
    let mut guest = turn_guest(true, Some(term));
    guest
        .answers
        .lock()
        .unwrap()
        .push_back(ResumeStatus::Signaled(None));
    let tool = super::tool(&guest);
    let result = tool.handle_rt_sigsuspend(&mut guest, suspend(!term)).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND))),
        "{result:?}"
    );
    let filed = suspend_requests(&guest);
    assert_eq!(filed.len(), 1);
    let installed = !term & !unblockable;
    assert_eq!(filed[0].guest_signal_mask, Some(installed));
    assert_eq!(filed[0].blocked_signal_mask, Some(installed));
}

/// HERMIT_TEMPORARY_MASK_CAUGHT_END (round-1 review): a caught signal the
/// temporary mask lets in would run its handler under that mask (step 3c.w,
/// unreachable in 3c.1): the check files the named stopping refusal, the
/// call returns `EINTR`, nothing is recorded for the restart checkpoint,
/// nothing is removed (a default-ignored SIGURG the masks would otherwise
/// remove stays) and nothing parks. SIGUSR1 is caught (`SigCgt`), blocked by
/// the own mask, let in by the temporary one. Mutations "a caught end
/// answered `Waits`" (the SIGURG is removed and the call parks) and
/// "answered `Fatal`" (`-ERESTARTNOHAND` and a D40 flag).
#[tokio::test]
async fn under_the_gate_a_caught_end_of_a_temporary_mask_wait_is_a_named_refusal() {
    let usr1 = bit(libc::SIGUSR1);
    let urg = bit(libc::SIGURG);
    let _seam = script(proc_state(usr1 | urg, usr1, 0));
    let mut guest = turn_guest(true, Some(usr1 | urg));
    let tool = super::tool(&guest);
    let result = tool.handle_rt_sigsuspend(&mut guest, suspend(0)).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::EINTR))),
        "{result:?}"
    );
    let refusals = guest
        .rpcs()
        .iter()
        .filter(|rpc| {
            matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                if reason.contains("HERMIT_TEMPORARY_MASK_CAUGHT_END"))
        })
        .count();
    assert_eq!(refusals, 1);
    assert_eq!(guest.thread.wait_signal_end, None);
    assert!(guest.sigtimedwait_sets.is_empty(), "nothing removed");
    assert!(suspend_requests(&guest).is_empty(), "nothing parked");
}

/// With the gate off `rt_sigsuspend` keeps its kernel-run path: no `/proc`
/// read, no parked request; it goes on to the `rt_sigpending` query of that
/// path (which this harness fails with ENOSYS). Mutation "the parked path
/// keyed on anything but the gate": it files a request or reads `/proc`.
#[tokio::test]
async fn with_the_gate_off_rt_sigsuspend_keeps_its_kernel_run_path() {
    let _seam = signal_state_read_seam::install(|_, _| panic!("a /proc read with the gate off"));
    let mut guest = turn_guest(false, Some(0));
    let tool = super::tool(&guest);
    let result = tool
        .handle_rt_sigsuspend(&mut guest, suspend(bit(libc::SIGTERM)))
        .await;
    assert!(result.is_err(), "{result:?}");
    assert!(guest.requests().is_empty());
    assert_eq!(guest.injected.first(), Some(&Sysno::rt_sigprocmask));
}

/// Design D37 rule (ii) with IR-7's per-instance masks, at a parked
/// `rt_sigsuspend`'s checks. Own mask: SIGURG, SIGWINCH. Temporary mask:
/// SIGWINCH, SIGCHLD.
///
/// - At the first check (everything pending was pending at entry, queued
///   under the own mask): SIGURG (own only: the transparent restart discards
///   it) and SIGCHLD (temporary only: discarded at send) are removed;
///   SIGWINCH (both) is kept.
/// - After a wake (sent during the park, queued under the temporary mask):
///   a `SIG_IGN` SIGUSR2 outside the temporary mask is removed; a SIGCHLD,
///   inside it, is kept; SIGWINCH, kept from entry, stays.
/// - The third check ends the wait for SIGTERM and removes nothing, though
///   a `SIG_IGN` SIGUSR2 outside the temporary mask is pending again (the
///   process dies at the return).
///
/// Mutations: "(i) using only `M_next`" (the first check keeps the SIGCHLD
/// Linux discarded at send); "`M_req` = the own mask at every check" (the
/// woken check removes the SIGCHLD Linux holds); "removal before the fatal
/// check" (the third check removes).
#[tokio::test]
async fn a_parked_rt_sigsuspend_removes_the_ignored_signals_linux_would_not_hold() {
    let urg = bit(libc::SIGURG);
    let winch = bit(libc::SIGWINCH);
    let chld = bit(libc::SIGCHLD);
    let usr2 = bit(libc::SIGUSR2);
    let term = bit(libc::SIGTERM);
    let own = urg | winch;
    let temporary = winch | chld;
    let _seam = script_seq(vec![
        proc_state(urg | winch | chld, 0, usr2),
        proc_state(urg | winch | chld, 0, usr2),
        proc_state(winch | chld | usr2, 0, usr2),
        proc_state(winch | chld | term | usr2, 0, usr2),
    ]);
    let mut guest = turn_guest(true, Some(own));
    for _ in 0..2 {
        guest
            .answers
            .lock()
            .unwrap()
            .push_back(ResumeStatus::Signaled(None));
    }
    let tool = super::tool(&guest);
    let result = tool
        .handle_rt_sigsuspend(&mut guest, suspend(temporary))
        .await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND))),
        "{result:?}"
    );
    assert_eq!(guest.sigtimedwait_sets, [urg | chld, usr2]);
    assert_eq!(suspend_requests(&guest).len(), 2);
}

/// The removal dequeues until `EAGAIN`, one instance per call (a real-time
/// signal may have several), and an error other than `EAGAIN` is Hermit's
/// own failure (`Error::Tool`), never a guest errno. Mutations "a single
/// dequeue" (the second and third instances stay) and "the errno returned
/// as the guest's" (the EFAULT row).
#[tokio::test]
async fn the_removal_dequeues_every_instance_until_eagain() {
    let rt = 1_u64 << 33;
    let _seam = script(proc_state(rt, 0, rt));
    let mut guest = turn_guest(true, Some(0));
    guest.sigtimedwait_results = VecDeque::from([Ok(34), Ok(34), Ok(34), Err(Errno::EAGAIN)]);
    crate::syscalls::helpers::remove_ignored_signals(&mut guest, rt)
        .await
        .unwrap();
    assert_eq!(guest.sigtimedwait_sets, [rt; 4]);
    let mut guest = turn_guest(true, Some(0));
    guest.sigtimedwait_results = VecDeque::from([Err(Errno::EFAULT)]);
    let failed = crate::syscalls::helpers::remove_ignored_signals(&mut guest, rt).await;
    assert!(
        matches!(failed, Err(Error::Tool(_))),
        "Hermit's own failure, never a guest errno: {failed:?}"
    );
    let mut guest = turn_guest(true, Some(0));
    crate::syscalls::helpers::remove_ignored_signals(&mut guest, 0)
        .await
        .unwrap();
    assert!(
        guest.injected.is_empty(),
        "nothing to remove, nothing injected"
    );
}

/// The 3c.1d gate "SIG_BLOCK of SIGCHLD with a send committed before the
/// grant": the SIGCHLD was sent while the guest's mask let it in, so Linux
/// discarded it, though the runtime's mask held it; the thread then blocked
/// SIGCHLD. Rule (i) at that call's return (`M_req` the mask before, `M_next`
/// after) removes it, and a cross-process status, read against the mask the
/// thread published at that request, does not show it. Mutation "(i) using
/// only `M_next`": the removal is empty and `SigPnd` shows SIGCHLD.
#[test]
fn a_sigchld_sent_before_its_block_was_granted_is_neither_kept_nor_shown() {
    let chld = bit(libc::SIGCHLD);
    let state = proc_state(chld, 0, 0);
    assert_eq!(crate::ignored_signals::removable(&state, 0, chld, 0), chld);
    assert_eq!(
        crate::ignored_signals::hidden_from_status(chld, 0, 0, 0),
        chld
    );
    assert_eq!(
        crate::ignored_signals::hidden_from_status(chld, 0, 0, chld),
        0,
        "once the block is published, a SIGCHLD sent after it is shown"
    );
}

/// Design D37 rule (iii) and the 3c.1d gate "SIG_IGN -> SIG_DFL of SIGTERM
/// with a send during the wait: no death". SIGTERM is `SIG_IGN` and the
/// guest's mask lets it in, so Linux discarded the instance sent during the
/// wait, which the runtime's mask held: the `rt_sigaction` that makes it
/// `SIG_DFL` removes it before the kernel installs the new action, so it
/// cannot kill at the return. A blocked SIGTERM stays (Linux queued it too),
/// a call that keeps it ignored removes nothing, and with the gate off
/// nothing is read or removed.
///
/// Mutations: "removed after the kernel installs the action" (the order
/// assertion), "the removal ignores `M_req`" (the blocked row removes),
/// "not keyed on a move away from ignored" (the SIG_IGN row removes).
#[tokio::test]
async fn an_rt_sigaction_that_unignores_removes_first_the_instances_linux_discarded() {
    let term = bit(libc::SIGTERM);
    let action = |handler: usize| {
        let cell: &'static mut [u64; 4] = Box::leak(Box::new([handler as u64, 0, 0, 0]));
        RtSigaction::new()
            .with_signum(libc::SIGTERM)
            .with_action(Some(
                Addr::from_raw(cell as *mut [u64; 4] as usize).unwrap(),
            ))
            .with_old_action(None)
            .with_sigsetsize(8)
    };
    for (name, gate, own, handler, removed) in [
        ("unblocked, to SIG_DFL", true, 0, libc::SIG_DFL, vec![term]),
        ("blocked, to SIG_DFL", true, term, libc::SIG_DFL, vec![]),
        ("unblocked, SIG_IGN again", true, 0, libc::SIG_IGN, vec![]),
        ("gate off", false, 0, libc::SIG_DFL, vec![]),
    ] {
        let _seam = script(proc_state(term, 0, term));
        let mut guest = turn_guest(gate, Some(own));
        let tool = super::tool(&guest);
        let result = tool.handle_rt_sigaction(&mut guest, action(handler)).await;
        assert_eq!(result.unwrap(), 0, "{name}");
        assert_eq!(guest.sigtimedwait_sets, removed, "{name}");
        let expected: Vec<Sysno> = if removed.is_empty() {
            vec![Sysno::rt_sigaction, Sysno::rt_sigaction]
        } else {
            vec![
                Sysno::rt_sigaction,
                Sysno::rt_sigtimedwait,
                Sysno::rt_sigaction,
            ]
        };
        assert_eq!(guest.injected, expected, "{name}");
    }
}

/// The "Codex H" row (design 3c.1 plan, revision 2's round-1 table) and the
/// 3c.1d gate "phase 1 still refuses temporary-mask calls in a SIGALRM
/// process": in a process whose SIGALRM is a phase-1 guest handler, every
/// temporary-mask call (`rt_sigsuspend`, a masked `pselect6`, `ppoll`,
/// `epoll_pwait`, `epoll_pwait2`) is refused with EOPNOTSUPP before its
/// handler runs, through the whole pipeline (`handle_syscall_event`), under
/// the gate too: with a default-fatal signal its temporary mask lets in
/// pending, nothing is parked, no D40 flag is recorded, and the guest sees
/// EOPNOTSUPP. Mutation "the gated form placed before phase 1's gate": the
/// call ends with EINTR for the fatal signal instead.
#[tokio::test]
async fn phase_1_still_refuses_every_temporary_mask_call_in_a_process_that_handles_sigalrm() {
    let term = bit(libc::SIGTERM);
    let empty = mask_cell(0);
    let wrapper: &'static mut [usize; 2] = Box::leak(Box::new([empty, 8]));
    let wrapper = wrapper as *mut [usize; 2] as usize;
    let zero: &'static mut [i64; 2] = Box::leak(Box::new([0, 0]));
    let zero = zero as *mut [i64; 2] as usize;
    let calls: Vec<(&str, Syscall)> = vec![
        ("rt_sigsuspend", Syscall::from(suspend(0))),
        (
            "pselect6",
            Syscall::from(
                reverie::syscalls::Pselect6::new()
                    .with_nfds(0)
                    .with_timeout(AddrMut::from_raw(zero))
                    .with_sigmask(Addr::from_raw(wrapper)),
            ),
        ),
        (
            "ppoll",
            Syscall::from(
                reverie::syscalls::Ppoll::new()
                    .with_nfds(0)
                    .with_timeout(AddrMut::from_raw(zero))
                    .with_sigmask(Addr::from_raw(empty))
                    .with_sigsetsize(8),
            ),
        ),
        (
            "epoll_pwait",
            Syscall::from(
                reverie::syscalls::EpollPwait::new()
                    .with_epfd(3)
                    .with_maxevents(1)
                    .with_timeout(1000)
                    .with_sigmask(Addr::from_raw(empty))
                    .with_sigsetsize(8),
            ),
        ),
        (
            "epoll_pwait2",
            Syscall::from_raw(
                Sysno::epoll_pwait2,
                reverie::syscalls::SyscallArgs::new(3, 0, 1, zero, empty, 8),
            ),
        ),
    ];
    for (name, call) in calls {
        let _seam = script(proc_state(term, 0, 0));
        let mut guest = turn_guest(true, Some(term));
        guest.thread.sigalrm_handled = true;
        guest.thread.sigalrm_blocked_published = false;
        let tool = <Detcore as Tool>::new(Pid::from_raw(THREAD.as_raw()), &guest.config);
        let result = tool.handle_syscall_event(&mut guest, call).await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::EOPNOTSUPP))),
            "{name}: {result:?}"
        );
        assert_eq!(guest.thread.wait_signal_end, None, "{name}");
        assert!(suspend_requests(&guest).is_empty(), "{name}");
        assert!(guest.probe_masks.is_empty(), "{name}: no probe ran");
    }
}

/// Answer this thread's `/proc` reads (by its process, whichever thread id
/// it reports) with `states[k]`, `k` the number of poll probes injected so
/// far, the last state for every count after: the state at entry is
/// `states[0]`, and a check after the `n`th probe reads `states[n]`.
fn script_by_probe(
    guest: &TurnGuest,
    states: Vec<KernelSignalState>,
) -> signal_state_read_seam::Installed {
    let probes = guest.probe_count.clone();
    signal_state_read_seam::install(move |pid, _| {
        if pid.as_raw() != THREAD.as_raw() {
            return None;
        }
        let probes = probes.load(std::sync::atomic::Ordering::SeqCst);
        Some(Ok(states[probes.min(states.len() - 1)]))
    })
}

/// The WARN events whose message names `tag`, as the parent's `WarnLog`
/// collects its one tag.
#[derive(Clone)]
struct TagLog {
    tag: &'static str,
    seen: std::sync::Arc<Mutex<Vec<String>>>,
}

impl TagLog {
    fn new(tag: &'static str) -> Self {
        Self {
            tag,
            seen: Default::default(),
        }
    }
}

impl tracing::Subscriber for TagLog {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::WARN)
    }
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = MessageVisitor(None);
        event.record(&mut visitor);
        if let Some(message) = visitor.0
            && message.contains(self.tag)
        {
            self.seen.lock().unwrap().push(message);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// A handler's result with its errno, for comparing with a row's.
fn errno_result(result: &Result<i64, Error>) -> Result<i64, Errno> {
    match result {
        Ok(value) => Ok(*value),
        Err(Error::Errno(errno)) => Err(*errno),
        Err(other) => panic!("not an errno: {other:?}"),
    }
}

/// Guest memory for the forms: a read-write page (timeout, mask wrapper,
/// mask, a writable set or pollfd), a read-only page and an unreadable one.
struct FormMemory {
    pages: crate::test_pages::Pages,
}

impl FormMemory {
    const TIMEOUT: usize = 0;
    const WRAPPER: usize = 64;
    const MASK: usize = 128;
    const SET: usize = 256;
    const READ_ONLY: usize = crate::test_pages::PAGE;
    const UNREADABLE: usize = 2 * crate::test_pages::PAGE;

    /// `timeout` in nanoseconds (`None`: no timeout argument), the
    /// temporary `mask`, and the writable set or pollfd zeroed.
    fn new(timeout: Option<u64>, mask: u64) -> Self {
        let pages = crate::test_pages::Pages::new(&[
            libc::PROT_READ | libc::PROT_WRITE,
            libc::PROT_READ,
            libc::PROT_NONE,
        ]);
        let mut memory = LocalMemory::new();
        let at = |offset| pages.address(offset).as_raw();
        if let Some(nanos) = timeout {
            memory
                .write_value(
                    AddrMut::<[i64; 2]>::from_raw(at(Self::TIMEOUT)).unwrap(),
                    &[
                        (nanos / 1_000_000_000) as i64,
                        (nanos % 1_000_000_000) as i64,
                    ],
                )
                .unwrap();
        }
        memory
            .write_value(
                AddrMut::<[usize; 2]>::from_raw(at(Self::WRAPPER)).unwrap(),
                &[at(Self::MASK), 8],
            )
            .unwrap();
        memory
            .write_value(AddrMut::<u64>::from_raw(at(Self::MASK)).unwrap(), &mask)
            .unwrap();
        memory
            .write_value(
                AddrMut::<[u64; 2]>::from_raw(at(Self::SET)).unwrap(),
                &[1, 0],
            )
            .unwrap();
        Self { pages }
    }

    fn at(&self, offset: usize) -> usize {
        self.pages.address(offset).as_raw()
    }

    /// The first word at `offset` (readable only for a readable page).
    fn word(&self, offset: usize) -> u64 {
        LocalMemory::new()
            .read_value(Addr::<u64>::from_raw(self.at(offset)).unwrap())
            .unwrap()
    }
}

fn pselect6(
    memory: &FormMemory,
    nfds: i32,
    set: usize,
    timeout: bool,
    masked: bool,
) -> reverie::syscalls::Pselect6 {
    reverie::syscalls::Pselect6::new()
        .with_nfds(nfds)
        .with_readfds(AddrMut::from_raw(memory.at(set)))
        .with_timeout(if timeout {
            AddrMut::from_raw(memory.at(FormMemory::TIMEOUT))
        } else {
            None
        })
        .with_sigmask(if masked {
            Addr::from_raw(memory.at(FormMemory::WRAPPER))
        } else {
            None
        })
}

/// The 3c.1d `pselect6` gate (D39, a zero timeout), with 3c.1.0's base rows
/// that in-guest already matches Linux in, which the gate must keep (F17's
/// order). Own mask: SIGTERM; temporary mask: empty, so it lets in the
/// pending SIGTERM. Run as the fake test thread, whose smaps cannot be read,
/// so every row takes the probe on copies (the path for a set the kernel
/// could not write back in place); the in-place path's rows are in
/// `the_base_rows_where_in_guest_matched_linux_keep_their_answers_under_the_gate`
/// and the tight-stack test, which run as the real thread.
///
/// - (a) an invalid fd: `EBADF`, no death;
/// - (b) a read-only set, ready: `EFAULT` (the copy-out), no death;
/// - (c) a read-only set, nothing ready: death (`-ERESTARTNOHAND` with the
///   D40 flag, `-EINTR` through the restart checkpoint), no `EFAULT`, the
///   set untouched;
/// - (d) an unreadable set: `EFAULT`, before any probe;
/// - (e) ready: the count, no death;
/// - (f) nothing ready, the temporary mask blocking SIGTERM: 0, the set
///   emptied.
///
/// Every probe carries no mask (IR-9). Mutation "the copies path probes the
/// guest's own sets": (c) gets the kernel's `EFAULT` copying the empty
/// result out to the read-only set.
#[tokio::test]
async fn under_the_gate_a_zero_timeout_masked_pselect6_follows_linux_order() {
    let term = bit(libc::SIGTERM);
    let rows: [(&str, usize, u64, Probe, Answer, bool); 6] = [
        (
            "(a) EBADF",
            FormMemory::SET,
            0,
            Some(Err(Errno::EBADF)),
            Err(Errno::EBADF),
            false,
        ),
        (
            "(b) read-only, ready",
            FormMemory::READ_ONLY,
            0,
            Some(Ok(1)),
            Err(Errno::EFAULT),
            false,
        ),
        (
            "(c) read-only, nothing ready",
            FormMemory::READ_ONLY,
            0,
            Some(Ok(0)),
            Err(Errno::ERESTARTNOHAND),
            true,
        ),
        (
            "(d) unreadable",
            FormMemory::UNREADABLE,
            0,
            None,
            Err(Errno::EFAULT),
            false,
        ),
        ("(e) ready", FormMemory::SET, 0, Some(Ok(1)), Ok(1), false),
        (
            "(f) blocked",
            FormMemory::SET,
            term,
            Some(Ok(0)),
            Ok(0),
            false,
        ),
    ];
    for (name, set, mask, probe, expected, dies) in rows {
        let memory = FormMemory::new(Some(0), mask);
        let mut guest = turn_guest(true, Some(term));
        // The unreadable row is classified from the caller's own smaps.
        use_real_test_thread(&mut guest);
        let _seam = script_by_probe(&guest, vec![proc_state(term, 0, 0)]);
        guest.probe_results.extend(probe);
        let tool = super::tool(&guest);
        let call = pselect6(&memory, 1, set, true, true);
        let result = tool.handle_pselect6(&mut guest, call).await;
        assert_eq!(errno_result(&result), expected, "{name}");
        assert_eq!(
            guest.thread.wait_signal_end,
            dies.then_some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            "{name}"
        );
        assert_eq!(
            guest.probe_masks,
            vec![None; usize::from(probe.is_some())],
            "{name}: no probe carries the mask"
        );
        if dies {
            assert_eq!(
                memory.word(FormMemory::READ_ONLY),
                u64::from_ne_bytes([crate::test_pages::Pages::FILL; 8]),
                "{name}: no copy-out"
            );
            let (res, code) = tool
                .wait_end_checkpoint(&mut guest, &Syscall::from(call), result)
                .await;
            assert!(
                matches!(res, Err(Error::Errno(Errno::EINTR))),
                "{name}: {res:?}"
            );
            assert_eq!(code, Some(Errno::ERESTARTNOHAND), "{name}");
        }
        if name.starts_with("(f)") {
            assert_eq!(
                memory.word(FormMemory::SET),
                0,
                "{name}: the set is emptied"
            );
        }
        assert!(
            !guest.requests().iter().any(|request| request
                .resources
                .contains_key(&ResourceID::InternalIOPolling)),
            "{name}: a zero timeout never parks"
        );
    }
}

/// Runs the calling test's body alone in a child copy of this test binary
/// (`--exact`, one test thread), and requires the child to pass it; returns
/// whether this is the child, which runs the body. For a test that reads
/// process-shared `/proc` state of the real test thread (its descriptor
/// table's `FDSize`, its `smaps`), which other tests of the same process
/// change concurrently under a plain `cargo test` (IR-31 (a)): in the child
/// nothing else runs, so the test reads only its own state, with every
/// assertion unchanged.
fn alone_in_child_process(name: &str) -> bool {
    const CHILD: &str = "HERMIT_TEST_ALONE_CHILD";
    let test = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    if std::env::var(CHILD).as_deref() == Ok(test.as_str()) {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .env(CHILD, &test)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{test} in its own process: {:?}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

/// D39: a masked `pselect6` with more than 64 descriptors and a nonzero
/// timeout gets `ENOSYS` by name once its first poll finds nothing ready and
/// no signal ends it, and returns at once a count Linux returns without
/// waiting; it never parks. The descriptor table's size is read from the
/// real test thread's `/proc` status (`FDSize`), and the sets' writability
/// from its `smaps`, so the body runs alone in its own process.
#[tokio::test]
async fn under_the_gate_a_masked_pselect6_with_more_than_64_descriptors_that_would_wait_is_enosys()
{
    if !alone_in_child_process(
        "under_the_gate_a_masked_pselect6_with_more_than_64_descriptors_that_would_wait_is_enosys",
    ) {
        return;
    }
    let term = bit(libc::SIGTERM);
    for (probe, expected) in [(Ok(0), Err(Errno::ENOSYS)), (Ok(2), Ok(2))] {
        let memory = FormMemory::new(Some(1_000_000_000), term);
        let log = TagLog::new("HERMIT_MASKED_PSELECT6_NFDS_REFUSED");
        let _subscriber = tracing::subscriber::set_default(log.clone());
        let mut guest = turn_guest(true, Some(term));
        use_real_test_thread(&mut guest);
        let _seam = script_by_probe(&guest, vec![proc_state(term, 0, 0)]);
        guest.probe_results.push_back(probe);
        let tool = super::tool(&guest);
        let result = tool
            .handle_pselect6(
                &mut guest,
                pselect6(&memory, 65, FormMemory::SET, true, true),
            )
            .await;
        assert_eq!(errno_result(&result), expected, "{probe:?}");
        assert_eq!(guest.probe_masks, [None]);
        // In place on the writable set: a count leaves the kernel's writes
        // (the requested descriptors, here as they were); the refusal puts
        // back the set the probe zeroed, as the call is not run (review
        // round 4).
        assert_eq!(memory.word(FormMemory::SET), 1, "{probe:?}: the set");
        let refused = log.seen.lock().unwrap().len();
        assert_eq!(refused, usize::from(expected.is_err()), "{probe:?}");
        assert!(
            !guest.requests().iter().any(|request| request
                .resources
                .contains_key(&ResourceID::InternalIOPolling)),
            "never parks"
        );
    }
}

/// IR-9 and D50 readers (3) and (4): under the gate each select or
/// `pselect6` form that waits (a masked `pselect6` on the internal path, an
/// unmasked one, `select`) ends for a default-fatal signal its mask (the
/// temporary one, or the guest's own) lets in, at the first turn that finds
/// nothing ready, and stays parked for a blocked or an ignored one, until
/// its deadline; no probe carries a mask, and a masked call's request
/// publishes its temporary mask. An ignored SIGURG pending at entry, blocked
/// by the own mask and let in by the temporary one, is removed (D37, IR-3's
/// companion row: the transparent restart, the timeout waited out).
///
/// Every request is answered `Signaled`, a hint that decides nothing under
/// the gate. Mutations: "today's probe mask kept under the gate" (the probe
/// masks); "the scheduler's `Signaled` hint ends the call" (the rows that
/// stay parked end with `EINTR` at the first answer); "the own mask decides
/// a masked call" (the first row parks).
#[tokio::test]
async fn under_the_gate_each_select_form_ends_only_for_a_signal_its_mask_lets_in() {
    let term = bit(libc::SIGTERM);
    let urg = bit(libc::SIGURG);
    #[derive(Clone, Copy, Debug)]
    enum Form {
        MaskedPselect6,
        Pselect6,
        Select,
    }
    // (form, own mask, temporary mask, pending, ends, removed)
    let rows = [
        (Form::MaskedPselect6, term, 0, term, true, 0),
        (Form::MaskedPselect6, term, term, term, false, 0),
        (Form::MaskedPselect6, urg, 0, urg, false, urg),
        (Form::Pselect6, 0, 0, term, true, 0),
        (Form::Pselect6, term, 0, term, false, 0),
        (Form::Select, 0, 0, term, true, 0),
        (Form::Select, term, 0, term, false, 0),
        (Form::Select, 0, 0, urg, false, 0),
    ];
    for (form, own, temporary, pending, ends, removed) in rows {
        let name = format!("{form:?} own {own:#x} temporary {temporary:#x} pending {pending:#x}");
        let memory = FormMemory::new(Some(1_000_000_000), temporary);
        let mut guest = turn_guest(true, Some(own));
        guest
            .answers
            .lock()
            .unwrap()
            .extend(std::iter::repeat_n(ResumeStatus::Signaled(None), 8));
        // An ignored signal removed at the first check is gone after it.
        let after = if removed != 0 { 0 } else { pending };
        let _seam = script_by_probe(
            &guest,
            vec![
                proc_state(pending, 0, 0),
                proc_state(pending, 0, 0),
                proc_state(after, 0, 0),
            ],
        );
        let tool = super::tool(&guest);
        let result = match form {
            Form::MaskedPselect6 | Form::Pselect6 => {
                let call = pselect6(
                    &memory,
                    1,
                    FormMemory::SET,
                    true,
                    matches!(form, Form::MaskedPselect6),
                );
                tool.handle_pselect6(&mut guest, call).await
            }
            Form::Select => {
                // A one-second `timeval` at the timeout's place.
                LocalMemory::new()
                    .write_value(
                        AddrMut::<[i64; 2]>::from_raw(memory.at(FormMemory::TIMEOUT)).unwrap(),
                        &[1, 0],
                    )
                    .unwrap();
                let call = reverie::syscalls::Select::new()
                    .with_nfds(1)
                    .with_readfds(AddrMut::from_raw(memory.at(FormMemory::SET)))
                    .with_timeout(AddrMut::from_raw(memory.at(FormMemory::TIMEOUT)));
                tool.handle_select(&mut guest, call).await
            }
        };
        if ends {
            assert!(
                matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND))),
                "{name}: {result:?}"
            );
            assert_eq!(
                guest.thread.wait_signal_end,
                Some(WaitSignalEnd::Fatal(libc::SIGTERM))
            );
            assert_eq!(guest.probe_masks.len(), 1, "{name}: ends at the first turn");
        } else {
            assert_eq!(result.unwrap(), 0, "{name}: waited out its timeout");
            assert!(guest.probe_masks.len() > 1, "{name}: parked across turns");
            assert_eq!(guest.thread.wait_signal_end, None, "{name}");
        }
        assert!(
            guest.probe_masks.iter().all(Option::is_none),
            "{name}: IR-9"
        );
        let removals: Vec<u64> = std::iter::once(removed).filter(|set| *set != 0).collect();
        assert_eq!(guest.sigtimedwait_sets, removals, "{name}");
        let polls: Vec<_> = guest
            .requests()
            .into_iter()
            .filter(|request| {
                request
                    .resources
                    .contains_key(&ResourceID::InternalIOPolling)
            })
            .collect();
        // A masked pselect6's first probe and check run in the call's own
        // turn: one request fewer than probes, none for a call the first
        // check ends. An unmasked pselect6 and select, which have no
        // temporary mask and no entry rule, keep their request before each
        // probe (round-2 ruling 4).
        let in_turn = usize::from(matches!(form, Form::MaskedPselect6));
        assert_eq!(
            polls.len() + in_turn,
            guest.probe_masks.len(),
            "{name}: the first probe precedes any request"
        );
        if let Form::MaskedPselect6 = form {
            assert!(
                polls
                    .iter()
                    .all(|request| request.guest_signal_mask == Some(temporary)),
                "{name}: the request publishes the temporary mask"
            );
        }
        if removed != 0 {
            assert_eq!(
                guest.sigtimedwait_after_requests,
                [0],
                "{name}: the entry instance is removed in the call's own turn"
            );
        }
    }
}

fn ppoll(memory: &FormMemory, pollfd: usize) -> reverie::syscalls::Ppoll {
    reverie::syscalls::Ppoll::new()
        .with_fds(AddrMut::from_raw(memory.at(pollfd)))
        .with_nfds(1)
        .with_timeout(AddrMut::from_raw(memory.at(FormMemory::TIMEOUT)))
        .with_sigmask(Addr::from_raw(memory.at(FormMemory::MASK)))
        .with_sigsetsize(8)
}

/// The 3c.1d `ppoll` gate (D50 reader (5); F17's `ppoll` order), with the
/// base rows in-guest already matches Linux in. Own mask: SIGTERM (and
/// SIGURG); temporary mask: empty.
///
/// - a zero timeout, nothing ready, SIGTERM pending, `revents` writable:
///   death (`-ERESTARTNOHAND`, the D40 flag);
/// - the same with a read-only pollfd array: `EFAULT` (the `revents`
///   copy-out comes first), no death;
/// - ready with the signal pending: the count;
/// - a nonzero timeout ended by SIGTERM: death at the first turn;
/// - a nonzero timeout with an ignored SIGURG the temporary mask lets in:
///   removed, and the timeout waited out (the transparent restart);
/// - a nonzero timeout with nothing that ends it: 0 at the deadline.
///
/// No probe carries the mask (IR-9). Mutations: "`ENOSYS` kept" (every
/// nonzero row returns `ENOSYS`); "the signal check before the probe", so
/// that `revents` are not copied out before the death (the read-only row
/// dies, and the ready row too).
#[tokio::test]
async fn under_the_gate_a_masked_ppoll_follows_linux_order_and_waits_under_its_temporary_mask() {
    let term = bit(libc::SIGTERM);
    let urg = bit(libc::SIGURG);
    // (name, timeout, pollfd, pending, probe, expected, dies, removed)
    let rows: [PpollRow; 6] = [
        (
            "zero, writable",
            0,
            FormMemory::SET,
            term,
            Some(Ok(0)),
            Err(Errno::ERESTARTNOHAND),
            true,
            0,
        ),
        (
            "zero, read-only",
            0,
            FormMemory::READ_ONLY,
            term,
            None,
            Err(Errno::EFAULT),
            false,
            0,
        ),
        (
            "zero, ready",
            0,
            FormMemory::SET,
            term,
            Some(Ok(1)),
            Ok(1),
            false,
            0,
        ),
        (
            "nonzero, fatal",
            1_000_000_000,
            FormMemory::SET,
            term,
            None,
            Err(Errno::ERESTARTNOHAND),
            true,
            0,
        ),
        (
            "nonzero, ignored",
            1_000_000_000,
            FormMemory::SET,
            urg,
            None,
            Ok(0),
            false,
            urg,
        ),
        (
            "nonzero, nothing",
            1_000_000_000,
            FormMemory::SET,
            0,
            None,
            Ok(0),
            false,
            0,
        ),
    ];
    for (name, timeout, pollfd, pending, probe, expected, dies, removed) in rows {
        let memory = FormMemory::new(Some(timeout), 0);
        let mut guest = turn_guest(true, Some(term | urg));
        // An ignored signal removed at the first check is gone after it.
        let after = if removed != 0 { 0 } else { pending };
        let _seam = script_by_probe(
            &guest,
            vec![
                proc_state(pending, 0, 0),
                proc_state(pending, 0, 0),
                proc_state(after, 0, 0),
            ],
        );
        guest.probe_results.extend(probe);
        let tool = super::tool(&guest);
        let result = tool.handle_ppoll(&mut guest, ppoll(&memory, pollfd)).await;
        assert_eq!(errno_result(&result), expected, "{name}");
        assert_eq!(
            guest.thread.wait_signal_end,
            dies.then_some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            "{name}"
        );
        assert!(!guest.probe_masks.is_empty(), "{name}");
        assert!(
            guest.probe_masks.iter().all(Option::is_none),
            "{name}: IR-9"
        );
        let removals: Vec<u64> = std::iter::once(removed).filter(|set| *set != 0).collect();
        assert_eq!(guest.sigtimedwait_sets, removals, "{name}");
        if timeout != 0 && expected == Ok(0) {
            assert!(guest.probe_masks.len() > 1, "{name}: parked across turns");
            assert!(
                guest
                    .requests()
                    .iter()
                    .filter(|request| request
                        .resources
                        .contains_key(&ResourceID::InternalIOPolling))
                    .all(|request| request.guest_signal_mask == Some(0)),
                "{name}: the request publishes the temporary mask"
            );
        }
    }
}

fn epoll_pwait(memory: &FormMemory, timeout: i32) -> reverie::syscalls::EpollPwait {
    reverie::syscalls::EpollPwait::new()
        .with_epfd(5)
        .with_events(AddrMut::from_raw(memory.at(FormMemory::SET)))
        .with_maxevents(1)
        .with_timeout(timeout)
        .with_sigmask(Addr::from_raw(memory.at(FormMemory::MASK)))
        .with_sigsetsize(8)
}

/// The 3c.1d `epoll` gate (D39, D37's exception, IR-3, IR-7 rows (A) and
/// (B)), with the base rows in-guest already matches Linux in. Own mask:
/// SIGURG and SIGTERM; temporary mask: empty; a 1 s timeout unless named.
///
/// - ready with SIGURG pending at entry: the events, and SIGURG kept (row
///   (A): nothing removed);
/// - an invalid fd: `EBADF`;
/// - a zero timeout: one maskless probe (IR-9 literal; Linux never ends a
///   zero-timeout call for a signal), 0;
/// - IR-3: SIGURG pending at entry, nothing ready: `EINTR` at once, SIGURG
///   removed;
/// - row (B): SIGURG sent during the park (pending only from the second
///   turn): removed silently, the timeout waited out;
/// - SIGTERM pending: `EINTR` with the D40 flag (death at the return).
///
/// Mutations: IR-3's "removed and parked" (its row waits out the timeout)
/// and "kept pending" (its row removes nothing); "every removable instance
/// interrupts" (row (B) returns `EINTR`).
#[tokio::test]
async fn under_the_gate_a_masked_epoll_pwait_follows_ep_poll_with_d37_s_exception() {
    let term = bit(libc::SIGTERM);
    let urg = bit(libc::SIGURG);
    let none = proc_state(0, 0, 0);
    // (name, timeout, states by probe count, probe, expected, flag, removed, probes)
    let rows: Vec<EpollRow> = vec![
        (
            "(A) ready",
            1000,
            vec![proc_state(urg, 0, 0)],
            Some(Ok(1)),
            Ok(1),
            false,
            vec![],
        ),
        (
            "EBADF",
            1000,
            vec![proc_state(urg, 0, 0)],
            Some(Err(Errno::EBADF)),
            Err(Errno::EBADF),
            false,
            vec![],
        ),
        (
            "zero timeout",
            0,
            vec![proc_state(urg, 0, 0)],
            None,
            Ok(0),
            false,
            vec![],
        ),
        (
            "IR-3",
            1000,
            vec![proc_state(urg, 0, 0)],
            None,
            Err(Errno::EINTR),
            false,
            vec![urg],
        ),
        (
            "(B)",
            1000,
            vec![none, none, proc_state(urg, 0, 0), none],
            None,
            Ok(0),
            false,
            vec![urg],
        ),
        (
            "fatal",
            1000,
            vec![proc_state(term, 0, 0)],
            None,
            Err(Errno::EINTR),
            true,
            vec![],
        ),
    ];
    for (name, timeout, states, probe, expected, dies, removed) in rows {
        let memory = FormMemory::new(None, 0);
        let mut guest = turn_guest(true, Some(urg | term));
        let _seam = script_by_probe(&guest, states);
        guest.probe_results.extend(probe);
        let tool = super::tool(&guest);
        let result = tool
            .handle_epoll_pwait(&mut guest, epoll_pwait(&memory, timeout))
            .await;
        assert_eq!(errno_result(&result), expected, "{name}");
        assert_eq!(
            guest.thread.wait_signal_end,
            dies.then_some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            "{name}"
        );
        assert_eq!(guest.sigtimedwait_sets, removed, "{name}");
        if timeout == 0 {
            assert_eq!(
                guest.probe_masks,
                [None],
                "{name}: one maskless probe (IR-9)"
            );
        } else {
            assert!(
                guest.probe_masks.iter().all(Option::is_none),
                "{name}: IR-9"
            );
        }
        if name == "IR-3" {
            assert_eq!(guest.probe_masks.len(), 1, "{name}: at once");
            assert!(
                guest.requests().is_empty(),
                "{name}: decided in the call's own turn, before any request"
            );
        }
        if name == "(B)" {
            assert!(guest.probe_masks.len() > 2, "{name}: waited out");
        }
    }
}

/// `epoll_pwait2` takes the same gated path (IR-3's row through its own
/// handler, with its `timespec` timeout read first).
#[tokio::test]
async fn under_the_gate_a_masked_epoll_pwait2_takes_the_same_path() {
    let urg = bit(libc::SIGURG);
    let memory = FormMemory::new(Some(1_000_000_000), 0);
    let mut guest = turn_guest(true, Some(urg));
    let _seam = script_by_probe(&guest, vec![proc_state(urg, 0, 0)]);
    let tool = super::tool(&guest);
    let call = Syscall::from_raw(
        Sysno::epoll_pwait2,
        reverie::syscalls::SyscallArgs::new(
            5,
            memory.at(FormMemory::SET),
            1,
            memory.at(FormMemory::TIMEOUT),
            memory.at(FormMemory::MASK),
            8,
        ),
    );
    let result = tool.handle_epoll_pwait2(&mut guest, call).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::EINTR))),
        "{result:?}"
    );
    assert_eq!(guest.sigtimedwait_sets, [urg]);
    assert_eq!(guest.probe_masks, [None]);
}

/// With the gate off the masked forms keep today's paths: a masked `ppoll`
/// that would block still fails with `ENOSYS` on in-guest, a masked
/// `epoll_pwait` with a timeout keeps its raw in-turn call, and a masked
/// `pselect6`'s internal probes still carry its temporary mask; no `/proc`
/// read, no removal. Mutation "a gated path keyed on anything but the gate":
/// one of them changes.
#[tokio::test]
async fn with_the_gate_off_the_masked_forms_keep_today_s_paths() {
    let _seam = signal_state_read_seam::install(|_, _| panic!("a /proc read with the gate off"));
    let memory = FormMemory::new(Some(1_000_000_000), 0);
    let mut guest = turn_guest(false, Some(0));
    let tool = super::tool(&guest);
    let result = tool
        .handle_ppoll(&mut guest, ppoll(&memory, FormMemory::SET))
        .await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ENOSYS))),
        "{result:?}"
    );
    let mut guest = turn_guest(false, Some(0));
    let result = tool
        .handle_epoll_pwait(&mut guest, epoll_pwait(&memory, 1000))
        .await;
    assert_eq!(result.unwrap(), 0);
    assert_eq!(guest.probe_masks, [Some(memory.at(FormMemory::MASK))]);
    let mut guest = turn_guest(false, Some(0));
    guest.probe_results.push_back(Ok(1));
    let result = tool
        .handle_pselect6(
            &mut guest,
            pselect6(&memory, 1, FormMemory::SET, true, true),
        )
        .await;
    assert_eq!(result.unwrap(), 1);
    assert!(
        matches!(guest.probe_masks.as_slice(), [Some(_)]),
        "{:?}",
        guest.probe_masks
    );
    assert!(guest.sigtimedwait_sets.is_empty());
}

/// 3c.1.0's fourteen zero-timeout (and two nonzero) masked `pselect6` and
/// `ppoll` rows (ai_docs/evidence/a5-3c1-20261022/transition-table-base.md,
/// "D39/F17"), where in-guest already matches Linux at the base and the
/// 3c.1d gates must keep it: SIGTERM blocked by the own mask and pending, a
/// temporary mask that lets it in. Each row's native answer, by name: a
/// count or an errno with SIGTERM kept, or death (`-ERESTARTNOHAND` with the
/// D40 flag `Fatal(SIGTERM)`). The kernel's answer to each probe is scripted
/// (`EBADF` for a closed descriptor in a set; a closed descriptor in a
/// pollfd counts as ready, `POLLNVAL`), and the harness makes the kernel's
/// copy-out, which a read-only buffer fails.
#[tokio::test]
async fn the_base_rows_where_in_guest_matched_linux_keep_their_answers_under_the_gate() {
    let term = bit(libc::SIGTERM);
    let death = Err(Errno::ERESTARTNOHAND);
    // (row, ppoll, nfds, buffer, timeout, probe, native)
    let rows: [(&str, bool, i32, usize, u64, Probe, Answer); 14] = [
        (
            "zt-ps-badfd",
            false,
            1,
            FormMemory::SET,
            0,
            Some(Err(Errno::EBADF)),
            Err(Errno::EBADF),
        ),
        (
            "zt-ps-ro-ready",
            false,
            1,
            FormMemory::READ_ONLY,
            0,
            Some(Ok(1)),
            Err(Errno::EFAULT),
        ),
        (
            "zt-ps-ro-none",
            false,
            1,
            FormMemory::READ_ONLY,
            0,
            Some(Ok(0)),
            death,
        ),
        (
            "zt-ps-unreadable",
            false,
            1,
            FormMemory::UNREADABLE,
            0,
            None,
            Err(Errno::EFAULT),
        ),
        (
            "zt-ps-ready",
            false,
            1,
            FormMemory::SET,
            0,
            Some(Ok(1)),
            Ok(1),
        ),
        (
            "zt-ps-none",
            false,
            1,
            FormMemory::SET,
            0,
            Some(Ok(0)),
            death,
        ),
        (
            "zt-ps-gt64-nz",
            false,
            65,
            FormMemory::SET,
            1_000_000_000,
            Some(Ok(0)),
            death,
        ),
        (
            "zt-pp-none",
            true,
            1,
            FormMemory::SET,
            0,
            Some(Ok(0)),
            death,
        ),
        (
            "zt-pp-ro-none",
            true,
            1,
            FormMemory::READ_ONLY,
            0,
            Some(Ok(0)),
            Err(Errno::EFAULT),
        ),
        (
            "zt-pp-ro-ready",
            true,
            1,
            FormMemory::READ_ONLY,
            0,
            Some(Ok(1)),
            Err(Errno::EFAULT),
        ),
        (
            "zt-pp-ready",
            true,
            1,
            FormMemory::SET,
            0,
            Some(Ok(1)),
            Ok(1),
        ),
        (
            "zt-pp-badfd",
            true,
            1,
            FormMemory::SET,
            0,
            Some(Ok(1)),
            Ok(1),
        ),
        (
            "zt-pp-unreadable",
            true,
            1,
            FormMemory::UNREADABLE,
            0,
            Some(Ok(0)),
            Err(Errno::EFAULT),
        ),
        (
            "zt-pp-nz",
            true,
            1,
            FormMemory::SET,
            1_000_000_000,
            Some(Ok(0)),
            death,
        ),
    ];
    for (name, is_ppoll, nfds, buffer, timeout, probe, native) in rows {
        let memory = FormMemory::new(Some(timeout), 0);
        let mut guest = turn_guest(true, Some(term));
        use_real_test_thread(&mut guest);
        let _seam = script_by_probe(&guest, vec![proc_state(term, 0, 0)]);
        guest.probe_results.extend(probe);
        let tool = super::tool(&guest);
        let result = if is_ppoll {
            tool.handle_ppoll(&mut guest, ppoll(&memory, buffer)).await
        } else {
            tool.handle_pselect6(&mut guest, pselect6(&memory, nfds, buffer, true, true))
                .await
        };
        assert_eq!(errno_result(&result), native, "{name}");
        assert_eq!(
            guest.thread.wait_signal_end,
            (native == death).then_some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            "{name}"
        );
        assert!(
            guest.probe_masks.iter().all(Option::is_none),
            "{name}: IR-9"
        );
        if matches!(name, "zt-ps-none" | "zt-ps-gt64-nz") {
            // The probe ran in place (a writable set, the real thread's
            // smaps) and zeroed the set; Linux makes no copy-out with
            // -ERESTARTNOHAND (core_sys_select, fs/select.c:682-687 at
            // v6.12), so the set is put back (review round 4).
            assert_eq!(memory.word(buffer), 1, "{name}: the set put back");
        }
    }
}

/// IR-7 rows (A) and (B) for `pselect6` and `ppoll` (3c.1.0's
/// `ir7a-pselect`, `ir7a-ppoll`, `ir7b-pselect`, `ir7b-ppoll`; Linux is the
/// reference): (A) SIGURG pending at entry, blocked by the own mask and let
/// in by the temporary one, with a descriptor ready: the count, and SIGURG
/// kept (a ready return removes nothing; the mask goes back and blocks it);
/// (B) SIGURG sent during the park (pending from the second turn), outside
/// the temporary mask: Linux discarded it at send, so it is removed and the
/// timeout is waited out, with no `EINTR`. Mutations: "removal on a ready
/// return" ((A) removes), "a send during the park left pending" ((B)
/// removes nothing).
#[tokio::test]
async fn ir7_rows_a_and_b_hold_for_pselect6_and_ppoll() {
    let urg = bit(libc::SIGURG);
    let urg_state = proc_state(urg, 0, 0);
    let none = proc_state(0, 0, 0);
    for is_ppoll in [false, true] {
        for row_b in [false, true] {
            let name = format!("ppoll {is_ppoll}, row {}", if row_b { "B" } else { "A" });
            let memory = FormMemory::new(Some(1_000_000_000), 0);
            let mut guest = turn_guest(true, Some(urg));
            let states = if row_b {
                vec![none, none, urg_state, none]
            } else {
                vec![urg_state]
            };
            let _seam = script_by_probe(&guest, states);
            if !row_b {
                guest.probe_results.push_back(Ok(1));
            }
            let tool = super::tool(&guest);
            let result = if is_ppoll {
                tool.handle_ppoll(&mut guest, ppoll(&memory, FormMemory::SET))
                    .await
            } else {
                tool.handle_pselect6(
                    &mut guest,
                    pselect6(&memory, 1, FormMemory::SET, true, true),
                )
                .await
            };
            if row_b {
                assert_eq!(errno_result(&result), Ok(0), "{name}: waited out");
                assert_eq!(guest.sigtimedwait_sets, [urg], "{name}");
                assert!(guest.probe_masks.len() > 2, "{name}");
            } else {
                assert_eq!(errno_result(&result), Ok(1), "{name}");
                assert!(guest.sigtimedwait_sets.is_empty(), "{name}: kept");
            }
        }
    }
}

/// D41's check under the guest's own mask comes first in every masked form
/// under the gate: SIGTERM pending and let in by the own mask, blocked by
/// the temporary one, was deliverable before the call, so the call is not
/// run (`ERESTARTNOINTR` with the D40 flag, before any injection and before
/// any guest-memory read) where it would otherwise wait with SIGTERM held.
/// Mutations "no check at entry": each form waits out its timeout; "the
/// epoll_pwait2 observation and timeout read before D41" (round-2 ruling 3):
/// its row reads its timeout first.
#[tokio::test]
async fn under_the_gate_a_masked_form_with_a_fatal_signal_deliverable_at_entry_is_not_run() {
    let term = bit(libc::SIGTERM);
    for form in ["pselect6", "ppoll", "epoll_pwait", "epoll_pwait2"] {
        let memory = FormMemory::new(Some(1_000_000_000), term);
        let mut guest = turn_guest(true, Some(0));
        let _seam = script_by_probe(&guest, vec![proc_state(term, 0, 0)]);
        let tool = super::tool(&guest);
        let result = match form {
            "pselect6" => {
                tool.handle_pselect6(
                    &mut guest,
                    pselect6(&memory, 1, FormMemory::SET, true, true),
                )
                .await
            }
            "ppoll" => {
                tool.handle_ppoll(&mut guest, ppoll(&memory, FormMemory::SET))
                    .await
            }
            "epoll_pwait" => {
                tool.handle_epoll_pwait(&mut guest, epoll_pwait(&memory, 1000))
                    .await
            }
            _ => {
                tool.handle_epoll_pwait2(&mut guest, epoll_pwait2(&memory, 8))
                    .await
            }
        };
        assert_eq!(errno_result(&result), Err(Errno::ERESTARTNOINTR), "{form}");
        assert_eq!(
            guest.thread.wait_signal_end,
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            "{form}"
        );
        assert!(guest.injected.is_empty(), "{form}: not run");
        assert!(
            guest.memory_reads().is_empty(),
            "{form}: D41 comes first, before any argument is read: {:#x?}",
            guest.memory_reads()
        );
    }
}

fn epoll_pwait2(memory: &FormMemory, sigsetsize: usize) -> Syscall {
    Syscall::from_raw(
        Sysno::epoll_pwait2,
        reverie::syscalls::SyscallArgs::new(
            5,
            memory.at(FormMemory::SET),
            1,
            memory.at(FormMemory::TIMEOUT),
            memory.at(FormMemory::MASK),
            sigsetsize,
        ),
    )
}

/// Round-1 ruling 5 (D41 before the argument errors; the order kept): an
/// invalid argument with a fatal signal pending meets Linux's two cases.
/// SIGTERM let in only by the temporary mask: Linux never delivers it before
/// the call (the own mask blocks it), so the call runs and fails with the
/// kernel's errno, and nothing dies: a wrong `pselect6` mask size (EINVAL), a
/// `ppoll` array the kernel cannot read (EFAULT), a closed epoll descriptor
/// (EBADF), a wrong `epoll_pwait2` mask size (EINVAL). SIGTERM let in by the
/// own mask: Linux delivers it at the return to user mode before the syscall
/// instruction runs, so the call never reads its arguments: D41's not-run
/// (`ERESTARTNOINTR` with the D40 flag), no errno, no injection and no
/// guest-memory read. Mutations "D41 after the argument checks" (the second
/// half gets the errnos), "D41 under the temporary mask" (the first half
/// dies), and "the epoll_pwait2 observation and timeout read before D41"
/// (round-2 ruling 3: its own-mask row reads its timeout first).
#[tokio::test]
async fn under_the_gate_an_invalid_masked_call_meets_d41_only_for_a_signal_its_own_mask_lets_in() {
    let term = bit(libc::SIGTERM);
    for own in [term, 0] {
        for form in ["pselect6", "ppoll", "epoll_pwait", "epoll_pwait2"] {
            let name = format!("{form} own {own:#x}");
            let memory = FormMemory::new(Some(1_000_000_000), 0);
            let mut guest = turn_guest(true, Some(own));
            let _seam = script_by_probe(&guest, vec![proc_state(term, 0, 0)]);
            let tool = super::tool(&guest);
            let (result, errno) = match form {
                "pselect6" => {
                    // A wrapper naming a 9-byte mask.
                    LocalMemory::new()
                        .write_value(
                            AddrMut::<[usize; 2]>::from_raw(memory.at(FormMemory::WRAPPER))
                                .unwrap(),
                            &[memory.at(FormMemory::MASK), 9],
                        )
                        .unwrap();
                    let call = pselect6(&memory, 1, FormMemory::SET, true, true);
                    (tool.handle_pselect6(&mut guest, call).await, Errno::EINVAL)
                }
                "ppoll" => (
                    tool.handle_ppoll(&mut guest, ppoll(&memory, FormMemory::UNREADABLE))
                        .await,
                    Errno::EFAULT,
                ),
                "epoll_pwait" => {
                    guest.probe_results.push_back(Err(Errno::EBADF));
                    (
                        tool.handle_epoll_pwait(&mut guest, epoll_pwait(&memory, 1000))
                            .await,
                        Errno::EBADF,
                    )
                }
                _ => (
                    tool.handle_epoll_pwait2(&mut guest, epoll_pwait2(&memory, 9))
                        .await,
                    Errno::EINVAL,
                ),
            };
            if own == 0 {
                assert_eq!(errno_result(&result), Err(Errno::ERESTARTNOINTR), "{name}");
                assert_eq!(
                    guest.thread.wait_signal_end,
                    Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
                    "{name}"
                );
                assert!(guest.injected.is_empty(), "{name}: not run");
                assert!(
                    guest.memory_reads().is_empty(),
                    "{name}: D41 comes first, before any argument is read: {:#x?}",
                    guest.memory_reads()
                );
            } else {
                assert_eq!(errno_result(&result), Err(errno), "{name}");
                assert_eq!(guest.thread.wait_signal_end, None, "{name}: no death");
            }
        }
    }
}

/// Round-1 ruling 1: every gated masked wait decides by, and publishes, the
/// mask Linux installs, without SIGKILL and SIGSTOP (`set_current_blocked`,
/// kernel/signal.c:3089-3092 at v6.12, from `set_user_sigmask`, :3169): a
/// masked `pselect6` (the internal path), `ppoll` and `epoll_pwait` whose
/// temporary mask is everything but SIGTERM publish it less SIGKILL and
/// SIGSTOP at every request, so a cross-process `SigBlk` has bits 9 and 19
/// clear. Mutation "the guest's mask published as written".
#[tokio::test]
async fn under_the_gate_a_masked_wait_publishes_the_mask_linux_installs() {
    let term = bit(libc::SIGTERM);
    let installed = !term & !(bit(libc::SIGKILL) | bit(libc::SIGSTOP));
    for form in ["pselect6", "ppoll", "epoll_pwait"] {
        let memory = FormMemory::new(Some(1_000_000_000), !term);
        let mut guest = turn_guest(true, Some(term));
        let _seam = script_by_probe(&guest, vec![proc_state(0, 0, 0)]);
        let tool = super::tool(&guest);
        let result = match form {
            "pselect6" => {
                tool.handle_pselect6(
                    &mut guest,
                    pselect6(&memory, 1, FormMemory::SET, true, true),
                )
                .await
            }
            "ppoll" => {
                tool.handle_ppoll(&mut guest, ppoll(&memory, FormMemory::SET))
                    .await
            }
            _ => {
                tool.handle_epoll_pwait(&mut guest, epoll_pwait(&memory, 1000))
                    .await
            }
        };
        assert_eq!(errno_result(&result), Ok(0), "{form}: the timeout");
        let polls: Vec<_> = guest
            .requests()
            .into_iter()
            .filter(|request| {
                request
                    .resources
                    .contains_key(&ResourceID::InternalIOPolling)
            })
            .collect();
        assert!(!polls.is_empty(), "{form}");
        for request in polls {
            assert_eq!(request.guest_signal_mask, Some(installed), "{form}");
            assert_eq!(request.blocked_signal_mask, Some(installed), "{form}");
        }
    }
}

/// Round-1 ruling 4: an argument Detcore's own read cannot reach is EFAULT
/// only where Linux's copy faults too, as the caller's `/proc/<tid>/smaps`
/// shows (3c.1c's rule): a `pselect6` timeout or set on a write-only page,
/// which x86 Linux copies from, gets the named stopping refusal
/// HERMIT_UNREADABLE_SYSCALL_ARGUMENT (fail closed; `ENOSYS` were the run to
/// go on); on a `PROT_NONE` page, and at an address in no mapping (0x1000,
/// below `vm.mmap_min_addr`), EFAULT, with no refusal. Run as the real test
/// thread, whose smaps the classification reads. Mutation "an unreadable
/// argument taken as EFAULT": the write-only rows become EFAULT.
#[tokio::test]
async fn under_the_gate_an_unreadable_pselect6_argument_is_efault_only_where_linux_faults() {
    let term = bit(libc::SIGTERM);
    let write_only = crate::test_pages::Pages::new(&[libc::PROT_WRITE]);
    let no_access = crate::test_pages::Pages::new(&[libc::PROT_NONE]);
    for (name, place, timeout_arg, expected, refused) in [
        (
            "timeout, write-only",
            "timeout",
            write_only.address(0).as_raw(),
            Errno::ENOSYS,
            1,
        ),
        (
            "timeout, PROT_NONE",
            "timeout",
            no_access.address(0).as_raw(),
            Errno::EFAULT,
            0,
        ),
        ("timeout, unmapped", "timeout", 0x1000, Errno::EFAULT, 0),
        (
            "set, write-only",
            "set",
            write_only.address(0).as_raw(),
            Errno::ENOSYS,
            1,
        ),
        (
            "set, PROT_NONE",
            "set",
            no_access.address(0).as_raw(),
            Errno::EFAULT,
            0,
        ),
        ("set, unmapped", "set", 0x1000, Errno::EFAULT, 0),
    ] {
        let memory = FormMemory::new(Some(0), 0);
        let mut guest = turn_guest(true, Some(term));
        use_real_test_thread(&mut guest);
        let _seam = script_by_probe(&guest, vec![proc_state(0, 0, 0)]);
        let tool = super::tool(&guest);
        let call = reverie::syscalls::Pselect6::new()
            .with_nfds(1)
            .with_readfds(AddrMut::from_raw(if place == "set" {
                timeout_arg
            } else {
                memory.at(FormMemory::SET)
            }))
            .with_timeout(AddrMut::from_raw(if place == "timeout" {
                timeout_arg
            } else {
                memory.at(FormMemory::TIMEOUT)
            }))
            .with_sigmask(Addr::from_raw(memory.at(FormMemory::WRAPPER)));
        let result = tool.handle_pselect6(&mut guest, call).await;
        assert_eq!(errno_result(&result), Err(expected), "{name}");
        let refusals = guest
            .rpcs()
            .iter()
            .filter(|rpc| {
                matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                    if reason.contains("HERMIT_UNREADABLE_SYSCALL_ARGUMENT"))
            })
            .count();
        assert_eq!(refusals, refused, "{name}");
        assert!(guest.probe_masks.is_empty(), "{name}: no probe");
    }
}

/// Round-1 ruling 6: a masked `pselect6` with more than 1024 descriptors to
/// copy reads the guest's sets first, so an unreadable set is EFAULT (the
/// kernel's `get_fd_set`, fs/select.c:670-673), and only readable sets reach
/// the named ENOSYS refusal, whose text names the EBADF a poll would find.
/// The test process's descriptor table is grown past 1500 entries (a dup to
/// descriptor 1500, closed after) so that FDSize admits 1501 descriptors.
/// Mutation "the refusal before the reads": the unreadable row gets ENOSYS.
#[tokio::test]
async fn under_the_gate_a_masked_pselect6_reads_its_sets_before_refusing_more_than_1024() {
    let term = bit(libc::SIGTERM);
    // SAFETY: plain descriptor calls on this process.
    assert_eq!(unsafe { libc::dup2(2, 1500) }, 1500);
    let sets: &'static mut [u64; 32] = Box::leak(Box::new([0; 32]));
    let sets = sets as *mut [u64; 32] as usize;
    for (name, readfds, expected, warned) in [
        ("unreadable", 0x1000, Errno::EFAULT, 0),
        ("readable", sets, Errno::ENOSYS, 1),
    ] {
        let memory = FormMemory::new(Some(0), term);
        let log = TagLog::new("HERMIT_MASKED_PSELECT6_NFDS_REFUSED");
        let _subscriber = tracing::subscriber::set_default(log.clone());
        let mut guest = turn_guest(true, Some(term));
        use_real_test_thread(&mut guest);
        let _seam = script_by_probe(&guest, vec![proc_state(0, 0, 0)]);
        let tool = super::tool(&guest);
        let call = pselect6(&memory, 1501, FormMemory::SET, true, true)
            .with_readfds(AddrMut::from_raw(readfds));
        let result = tool.handle_pselect6(&mut guest, call).await;
        assert_eq!(errno_result(&result), Err(expected), "{name}");
        let seen = log.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), warned, "{name}");
        assert!(
            seen.iter().all(|warning| warning.contains("EBADF")),
            "{name}: the EBADF residual is named: {seen:?}"
        );
        assert!(guest.probe_masks.is_empty(), "{name}: no probe");
    }
    // SAFETY: the descriptor dup'd above.
    unsafe { libc::close(1500) };
}

/// Round-2 ruling 5: under the gate no timeslice runs, so a thread yields
/// only when it makes a request; a masked wait with a nonzero timeout whose
/// first probe, in the call's own turn, finds a descriptor ready or an error
/// returns that result after one yielding empty request, which names no
/// resource and publishes the guest's own mask. The result is decided before
/// the request and is unchanged by it, the time left included: it is written
/// back before the request (round-3 ruling 6), so the time that other
/// threads' turns add during the yield (the harness advances its clock by
/// 100 ms at each request) does not shorten it: 1 s less the one 300 ms step
/// between the two clock observations, 0.7 s. A masked `pselect6` on the
/// internal path, one with 65 descriptors on the staged path (D54; round-3
/// ruling 2), `ppoll` and `epoll_pwait`, each with a ready count and with an
/// error. Mutations "no yield after an in-turn result" (no request at all),
/// "no yield on the staged path" (its rows), "ppoll's time left written after
/// the yield" (0.6 s).
#[tokio::test]
async fn under_the_gate_a_masked_wait_ready_at_its_first_probe_still_yields_once() {
    let term = bit(libc::SIGTERM);
    for form in ["pselect6", "pselect6 65", "ppoll", "epoll_pwait"] {
        for probe in [Ok(2), Err(Errno::EBADF)] {
            let name = format!("{form} {probe:?}");
            let memory = FormMemory::new(Some(1_000_000_000), 0);
            let mut guest = turn_guest(true, Some(term));
            if form == "pselect6 65" {
                // The staged path reads FDSize and smaps for the real thread.
                use_real_test_thread(&mut guest);
            }
            guest.clock_step_on_request = 100_000_000;
            guest.probe_results.push_back(probe);
            let _seam = script_by_probe(&guest, vec![proc_state(0, 0, 0)]);
            let tool = super::tool(&guest);
            let result = match form {
                "pselect6" => {
                    tool.handle_pselect6(
                        &mut guest,
                        pselect6(&memory, 1, FormMemory::SET, true, true),
                    )
                    .await
                }
                "pselect6 65" => {
                    tool.handle_pselect6(
                        &mut guest,
                        pselect6(&memory, 65, FormMemory::SET, true, true),
                    )
                    .await
                }
                "ppoll" => {
                    tool.handle_ppoll(&mut guest, ppoll(&memory, FormMemory::SET))
                        .await
                }
                _ => {
                    tool.handle_epoll_pwait(&mut guest, epoll_pwait(&memory, 1000))
                        .await
                }
            };
            assert_eq!(errno_result(&result), probe, "{name}");
            assert_eq!(guest.probe_masks, [None], "{name}: one in-turn probe");
            let requests = guest.requests();
            assert_eq!(requests.len(), 1, "{name}: one yield: {requests:?}");
            assert!(requests[0].resources.is_empty(), "{name}: an empty request");
            assert_eq!(
                requests[0].guest_signal_mask,
                Some(term),
                "{name}: the request publishes the guest's own mask"
            );
            if form != "epoll_pwait" {
                let left: [i64; 2] = LocalMemory::new()
                    .read_value(Addr::from_raw(memory.at(FormMemory::TIMEOUT)).unwrap())
                    .unwrap();
                assert_eq!(
                    left,
                    [0, 700_000_000],
                    "{name}: the time left, written before the yield"
                );
            }
        }
    }
}

/// Round-2 ruling 2: a masked `ppoll` under the gate on a stack with no
/// room for its private zero timeout (no writable scratch below the red
/// zone, and no free red-zone window) gets the kernel's answer first, from a
/// stage-free `poll` probe with an immediate zero timeout and no mask, before
/// any refusal: a ready count, `nfds > RLIMIT_NOFILE` (EINVAL), an array the
/// kernel cannot read or write back (EFAULT), as `do_sys_poll` gives them
/// (fs/select.c:987-1030 at v6.12). Only nothing ready, which would need the
/// private cell to wait, is refused (ENOSYS with
/// `--allow-unsupported-syscalls`), as before the gate. Round-3 ruling 5:
/// that probe cannot publish expired virtual timerfds first (publication
/// needs scratch memory too), so in a process with an armed virtual timerfd
/// the array may name, a count could miss a timerfd Linux reports ready (an
/// expired timerfd beside another ready descriptor): the count is refused
/// then too, and only an error, which no readiness changes, is returned.
/// Mutations "the scratch refusal before any probe" (every row is ENOSYS with
/// no probe) and "a count returned without the publication" (the timerfd
/// ready row returns 1).
#[tokio::test]
async fn under_the_gate_a_masked_ppoll_on_a_tight_stack_gets_the_kernel_s_answer_first() {
    for (name, pollfd, probe, expected) in [
        ("ready", FormMemory::SET, Some(Ok(1)), Ok(1)),
        (
            "timerfd ready",
            FormMemory::SET,
            Some(Ok(1)),
            Err(Errno::ENOSYS),
        ),
        (
            "timerfd nfds",
            FormMemory::SET,
            Some(Err(Errno::EINVAL)),
            Err(Errno::EINVAL),
        ),
        (
            "nfds",
            FormMemory::SET,
            Some(Err(Errno::EINVAL)),
            Err(Errno::EINVAL),
        ),
        (
            "unreadable",
            FormMemory::UNREADABLE,
            None,
            Err(Errno::EFAULT),
        ),
        ("nothing ready", FormMemory::SET, None, Err(Errno::ENOSYS)),
    ] {
        let memory = FormMemory::new(Some(1_000_000_000), 0);
        let mut guest = turn_guest(true, Some(0));
        guest.tight_stack = true;
        guest.config.panic_on_unsupported_syscalls = false;
        if name.starts_with("timerfd") {
            arm_virtual_timerfd(&mut guest);
        }
        if let Some(probe) = probe {
            guest.probe_results.push_back(probe);
        }
        let _seam = script_by_probe(&guest, vec![proc_state(0, 0, 0)]);
        let tool = super::tool(&guest);
        let result = tool.handle_ppoll(&mut guest, ppoll(&memory, pollfd)).await;
        assert_eq!(errno_result(&result), expected, "{name}");
        let probes: Vec<Sysno> = guest
            .injected
            .iter()
            .copied()
            .filter(|sysno| matches!(sysno, Sysno::poll | Sysno::ppoll))
            .collect();
        assert_eq!(
            probes,
            [Sysno::poll],
            "{name}: one stage-free probe, no ppoll"
        );
        assert_eq!(guest.probe_masks, [None], "{name}: no mask (IR-9)");
    }
}

/// Answer this thread's `/proc` reads with `before` until the harness has
/// answered a `GlobalTimeLowerBound` as a hand-off (`TurnGuest::handoffs`),
/// then with `after`: a signal committed while another thread held the turn.
fn script_by_handoff(
    guest: &TurnGuest,
    before: KernelSignalState,
    after: KernelSignalState,
) -> signal_state_read_seam::Installed {
    let handoffs = guest.handoffs.clone();
    signal_state_read_seam::install(move |pid, _| {
        if pid.as_raw() != THREAD.as_raw() {
            return None;
        }
        let handed_off = handoffs.load(std::sync::atomic::Ordering::SeqCst) != 0;
        Some(Ok(if handed_off { after } else { before }))
    })
}

/// Arm a virtual timerfd far in the future in `guest`'s process, so a probe
/// must publish expired timerfds first and that publication observes the
/// clock (`timerfd::publish_expired`, an RPC).
fn arm_virtual_timerfd(guest: &mut TurnGuest) {
    const FD: i32 = 900;
    guest.config.virtualize_time = true;
    guest
        .thread
        .add_fd(
            FD,
            nix::fcntl::OFlag::empty(),
            crate::fd::FdType::Timerfd,
            None,
        )
        .unwrap();
    guest
        .thread
        .with_detfd(FD, |detfd| {
            detfd.set_virtual_timerfd(Some(crate::fd::VirtualTimerfd::set(
                Some(crate::types::LogicalTime::from_nanos(u64::MAX / 2)),
                0,
            )))
        })
        .unwrap();
    assert!(crate::syscalls::timerfd::holds_armed_virtual_timerfd(guest));
}

/// Round-3 ruling 1 (Codex high): no RPC that may hand the turn off runs
/// between a masked wait's entry and its first check, so a signal committed
/// while another thread held the turn is decided as one sent during the
/// call, under the temporary mask, never as one pending at entry under the
/// guest's own mask (IR-7). The harness answers the first clock observation
/// as a hand-off (`Granted(GlobalTimeLowerBound)`) during which an ignored
/// SIGURG is committed. Two directions:
///
/// - (a) the own mask blocks SIGURG, the temporary mask lets it in: Linux
///   discards it at send (the temporary mask is installed), so it is removed
///   at a later check and no epoll form returns EINTR (D37's exception
///   applies to instances pending at entry only), and the `/proc` view of the
///   parked call, whose request publishes the temporary mask, hides it;
/// - (b) the own mask lets it in, the temporary mask blocks it: Linux keeps
///   it queued, so it is never removed, and the `/proc` view shows it.
///
/// Every finite-timeout form: a masked `pselect6` on the internal path and on
/// the staged path (65 descriptors), `ppoll`, `epoll_pwait`, `epoll_pwait2`.
/// The clock is observed after the first check, so the hand-off comes after
/// it (the staged path observes it only for a write-back and makes no
/// hand-off before its refusal). With an armed virtual timerfd the first
/// observation is the deadline's at the entry, after the temporary mask's
/// publication and before the timerfd publication (A5 step 3c.1e A,
/// IR-23(c)): the at-entry set is read before both
/// (`TemporaryMaskCall::enter`), so the same answers hold. Mutations "the clock observed before the first probe"
/// (every form but the timerfd rows: (a) epoll returns EINTR, (b) removes)
/// and "the at-entry set taken as everything pending" (the timerfd rows).
#[tokio::test]
async fn under_the_gate_a_signal_sent_during_a_hand_off_before_the_first_check_is_not_an_entry_instance()
 {
    let urg = bit(libc::SIGURG);
    for timerfd in [false, true] {
        let forms: &[&str] = if timerfd {
            &["pselect6", "ppoll", "epoll_pwait"]
        } else {
            &[
                "pselect6",
                "pselect6 65",
                "ppoll",
                "epoll_pwait",
                "epoll_pwait2",
            ]
        };
        for &form in forms {
            for (direction, own, temporary) in [("(a)", urg, 0), ("(b)", 0, urg)] {
                let name = format!("{form} {direction} timerfd {timerfd}");
                let memory = FormMemory::new(Some(1_000_000_000), temporary);
                let mut guest = turn_guest(true, Some(own));
                if form == "pselect6 65" {
                    use_real_test_thread(&mut guest);
                }
                if timerfd {
                    arm_virtual_timerfd(&mut guest);
                }
                guest
                    .handoffs_left
                    .store(1, std::sync::atomic::Ordering::SeqCst);
                let _seam = script_by_handoff(&guest, proc_state(0, 0, 0), proc_state(urg, 0, 0));
                let tool = super::tool(&guest);
                let result = match form {
                    "pselect6" => {
                        tool.handle_pselect6(
                            &mut guest,
                            pselect6(&memory, 1, FormMemory::SET, true, true),
                        )
                        .await
                    }
                    "pselect6 65" => {
                        tool.handle_pselect6(
                            &mut guest,
                            pselect6(&memory, 65, FormMemory::SET, true, true),
                        )
                        .await
                    }
                    "ppoll" => {
                        tool.handle_ppoll(&mut guest, ppoll(&memory, FormMemory::SET))
                            .await
                    }
                    "epoll_pwait" => {
                        tool.handle_epoll_pwait(&mut guest, epoll_pwait(&memory, 1000))
                            .await
                    }
                    _ => {
                        tool.handle_epoll_pwait2(&mut guest, epoll_pwait2(&memory, 8))
                            .await
                    }
                };
                let staged = form == "pselect6 65";
                let expected = if staged { Err(Errno::ENOSYS) } else { Ok(0) };
                assert_eq!(errno_result(&result), expected, "{name}: no EINTR");
                assert_eq!(guest.thread.wait_signal_end, None, "{name}");
                let removed = !guest.sigtimedwait_sets.is_empty();
                match (direction, staged) {
                    // The staged path's refusal comes before any hand-off.
                    (_, true) => assert!(!removed, "{name}: decided before any hand-off"),
                    ("(a)", false) => {
                        assert!(
                            guest.sigtimedwait_sets.iter().all(|set| *set == urg) && removed,
                            "{name}: removed as Linux discards it at send"
                        )
                    }
                    _ => assert!(!removed, "{name}: kept, as Linux queues it"),
                }
                if !staged {
                    assert_eq!(
                        guest.handoffs.load(std::sync::atomic::Ordering::SeqCst),
                        1,
                        "{name}: the hand-off happened"
                    );
                    // The /proc view while parked: the request publishes the
                    // temporary mask, and the status subtraction (I less
                    // M_req) hides SIGURG exactly where Linux discarded it.
                    let polls: Vec<_> = guest
                        .requests()
                        .into_iter()
                        .filter(|request| {
                            request
                                .resources
                                .contains_key(&ResourceID::InternalIOPolling)
                        })
                        .collect();
                    assert!(!polls.is_empty(), "{name}: parked");
                    for request in polls {
                        let published = request.guest_signal_mask.unwrap();
                        assert_eq!(published, temporary, "{name}");
                        let hidden =
                            crate::ignored_signals::hidden_from_status(urg, 0, 0, published);
                        assert_eq!(hidden, urg & !temporary, "{name}: the /proc view");
                    }
                }
            }
        }
    }
}

/// Round-3 ruling 3: a masked `pselect6` with a zero timeout probes on the
/// guest's own sets with the guest's own zero timeout (the probe's timeout
/// pointer is the guest's), so a stack with no scratch changes nothing: the
/// kernel's ready count, EBADF, or 0 with the sets zeroed, as Linux returns
/// them. A set the kernel cannot write back (read-only) is probed on copies
/// instead, which need scratch: on such a stack it is refused by name (a
/// named residual), never an EFAULT of Hermit's own. Run as the real test
/// thread, whose smaps classify the sets. Mutations "a private zero timeout
/// staged for a zero timeout too" and "always on copies" (the writable rows
/// are refused).
#[tokio::test]
async fn under_the_gate_a_zero_timeout_masked_pselect6_on_a_tight_stack_gets_the_kernel_s_answer() {
    for (name, set, probe, expected, word) in [
        ("ready", FormMemory::SET, Some(Ok(1)), Ok(1), Some(1)),
        (
            "EBADF",
            FormMemory::SET,
            Some(Err(Errno::EBADF)),
            Err(Errno::EBADF),
            Some(1),
        ),
        ("nothing ready", FormMemory::SET, None, Ok(0), Some(0)),
        (
            "read-only",
            FormMemory::READ_ONLY,
            Some(Ok(1)),
            Err(Errno::ENOSYS),
            None,
        ),
    ] {
        let memory = FormMemory::new(Some(0), 0);
        let mut guest = turn_guest(true, Some(bit(libc::SIGTERM)));
        use_real_test_thread(&mut guest);
        guest.tight_stack = true;
        if let Some(probe) = probe {
            guest.probe_results.push_back(probe);
        }
        let _seam = script_by_probe(&guest, vec![proc_state(0, 0, 0)]);
        let tool = super::tool(&guest);
        let result = tool
            .handle_pselect6(&mut guest, pselect6(&memory, 1, set, true, true))
            .await;
        assert_eq!(errno_result(&result), expected, "{name}");
        if let Some(word) = word {
            assert_eq!(
                guest.probe_timeouts,
                [memory.at(FormMemory::TIMEOUT)],
                "{name}: one probe, on the guest's own timeout"
            );
            assert_eq!(
                memory.word(set),
                word,
                "{name}: the kernel's in-place writes"
            );
        } else {
            assert!(
                guest.probe_timeouts.is_empty(),
                "{name}: refused before a probe"
            );
        }
        assert!(
            guest.probe_masks.iter().all(Option::is_none),
            "{name}: IR-9"
        );
    }
}

/// Round-3 ruling 3: a masked `pselect6` with more than 64 descriptors and a
/// nonzero timeout stages only a private zero timeout, and where it takes a
/// red-zone window (no scratch below the red zone) the window is clear of the
/// sets, which may lie in the red zone themselves: the probe's timeout cell
/// never overlaps a set, the sets get the kernel's writes in place, and the
/// red zone's other bytes are restored afterwards. Mutation "the window chosen
/// with no live ranges" (the cell lands on the set).
#[tokio::test]
async fn under_the_gate_a_staged_pselect6_s_private_timeout_avoids_sets_in_the_red_zone() {
    const LEN: usize = 16; // FDS_BYTES(65)
    let memory = FormMemory::new(Some(1_000_000_000), 0);
    let red: &'static mut [u8; 4096] = Box::leak(Box::new([0xab; 4096]));
    let rsp = red.as_ptr() as usize + 2048;
    let set = rsp - LEN;
    LocalMemory::new()
        .write_exact(AddrMut::<u8>::from_raw(set).unwrap(), &[0xff; LEN])
        .unwrap();
    let mut guest = turn_guest(true, Some(0));
    use_real_test_thread(&mut guest);
    guest.tight_stack = true;
    guest.rsp = rsp as u64;
    guest.probe_results.push_back(Ok(2));
    let _seam = script_by_probe(&guest, vec![proc_state(0, 0, 0)]);
    let tool = super::tool(&guest);
    let call =
        pselect6(&memory, 65, FormMemory::SET, true, true).with_readfds(AddrMut::from_raw(set));
    let result = tool.handle_pselect6(&mut guest, call).await;
    assert_eq!(errno_result(&result), Ok(2));
    assert_eq!(guest.probe_timeouts.len(), 1, "one probe");
    let cell = guest.probe_timeouts[0];
    assert!(
        cell + 16 <= set || cell >= set + LEN,
        "the private timeout {cell:#x} is clear of the set {set:#x}"
    );
    assert!(
        (rsp - 128..rsp).contains(&cell),
        "the private timeout is in the red zone"
    );
    let bytes: [u8; 128] = LocalMemory::new()
        .read_value(Addr::from_raw(rsp - 128).unwrap())
        .unwrap();
    assert!(
        bytes[..128 - LEN].iter().all(|byte| *byte == 0xab),
        "the red zone's other bytes are restored"
    );
    assert_eq!(
        &bytes[128 - LEN..],
        &[0xff; LEN],
        "the set, written in place"
    );
}

/// Round-3 ruling 4: the removal of ignored signals stages its two cells on
/// the scratch stack; a stack with no room is Hermit's own failure
/// (`Error::Tool`), never an EFAULT the guest's `rt_sigaction`,
/// `rt_sigsuspend` or masked wait could return. Mutation "the staging error
/// returned as it is" (EFAULT).
#[tokio::test]
async fn a_removal_with_no_scratch_stack_is_hermit_s_own_failure() {
    let mut guest = turn_guest(true, Some(0));
    guest.tight_stack = true;
    let result =
        crate::syscalls::helpers::remove_ignored_signals(&mut guest, bit(libc::SIGURG)).await;
    assert!(
        matches!(result, Err(Error::Tool(_))),
        "Hermit's own failure: {result:?}"
    );
    assert!(guest.sigtimedwait_sets.is_empty(), "nothing injected");
}

mod publication_tests;
