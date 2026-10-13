/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! timerfd expirations on Detcore's virtual clock (timerfd_create(2)).
//!
//! Detcore keeps each timerfd's setting (`crate::fd::VirtualTimerfd`) and
//! leaves the host timerfd disarmed, so the host never makes it readable on
//! its own. Before a probe that can observe a timerfd's readiness or count --
//! a `read`/`readv` of one, and every `poll`, `ppoll`, `select`, `pselect6`,
//! `epoll_wait`, `epoll_pwait` and `epoll_pwait2` probe -- [`publish_expired`]
//! counts the expirations at or before the guest clock and, for a timerfd that
//! gained one, makes the host descriptor readable and waits until the host
//! reports it so. Only then does the model record the expirations. A read
//! returns the model's count; `timerfd_gettime` answers from it.
//!
//! A call that would wait in the host, outside these probes, on descriptors
//! that may include a virtual timerfd would wait forever: the host is never
//! armed. Such a call refuses the run by name ([`refuse_host_wait`]), as does
//! any failure to make an expired timerfd readable ([`refuse`]), instead of
//! returning an errno Linux would not.
//!
//! Determinism: the host descriptor becomes readable only inside the probing
//! thread's turn, and that turn does not continue until the host reports it
//! readable, so the host state never lags the model at any probe. Whether a
//! probe finds the timerfd ready, and what a read returns, are therefore
//! functions of the model and of the guest clock read at that probe. The guest
//! clock read is `global_time_lower_bound` under the guest clock's floor, the
//! same reading `clock_gettime` returns; it includes the time charged for
//! internal polling retries, whose number is itself host-perturbed where a
//! probe waits on an external, host-timed source, as for every other timeout
//! Detcore serves. Host time decides only how long the readiness wait lasts,
//! up to `READABLE_VALVE`. Expirations are counted exactly from the deadline
//! and interval, never rounded.
//!
//! Scope: only where Detcore's model of an open file description is the one
//! copy every process sees -- Detcore outside the guest (ptrace, KVM), where a
//! fork shares the description's model and exec keeps it, as Linux shares the
//! timer. A tool inside the guest (SaBRe, DBT, in-guest LiteInst) copies its
//! state into a forked child, and SaBRe and DBT rediscover descriptors after
//! exec, so timerfds stay with the host there, unchanged. Also not under record/replay,
//! whose recordings keep the host's answers, or without sequentialized
//! threads. `TFD_TIMER_CANCEL_ON_SET` is accepted and never fires, because
//! Hermit's virtual clock is never set; an absolute deadline is read on the
//! guest clock for every clock id, as POSIX timers are.

use std::time::Duration;
use std::time::Instant;

use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use crate::detlog;
use crate::fd::FdType;
use crate::fd::VirtualTimerfd;
use crate::record_or_replay::RecordOrReplay;
use crate::syscalls::helpers::poll_deadline_for;
use crate::tool_global::thread_observe_time;
use crate::tool_local::Detcore;
use crate::types::LogicalTime;
use crate::types::RawFd;

/// How long, in host time, Detcore waits for an expired host timerfd to
/// become readable before it refuses the run, like the scheduler's other
/// valves. An expired high-resolution timer fires within microseconds.
const READABLE_VALVE: Duration = Duration::from_secs(30);

/// Test-only fault hook, like `HERMIT_TEST_ACCEPT_BRACKET_FAULT`, acting at
/// the first publication only. `never-readable`: Detcore does not arm the
/// host timerfd and waits one second for it, so the run ends on the named
/// refusal. `signal-before-arm`: Detcore sends the guest thread a SIGCONT
/// (no effect on a running guest) just before arming, so the injection meets
/// a pending signal; it logs a warning when that interrupted the first
/// attempt, which then retries. Unset, nothing changes.
const FAULT_ENV: &str = "HERMIT_TEST_TIMERFD_FAULT";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    NeverReadable,
    SignalBeforeArm,
}

/// `TFD_IOC_SET_TICKS`: `_IOW('T', 0, u64)`.
pub(crate) const TFD_IOC_SET_TICKS: u64 = 0x4008_5400;

/// Whether Detcore's tool state runs outside the guest's processes, so one
/// copy of an open file description's model serves every process that shares
/// it: a fork shares the model as Linux shares the timer, and exec keeps it.
///
/// Reverie names no capability for where the tool runs, so this derives it
/// from the two that follow from running inside the guest:
/// `tool_shares_guest_descriptor_table` (the tool runs in the guest process
/// and shares its descriptor table: SaBRe, DBT) and
/// `process_exits_complete_asynchronously` (the tool runs in the guest
/// process, so a process's exit completes only after its tool is gone, outside
/// any turn: in-guest LiteInst). A backend whose tool runs inside the guest
/// copies the tool's state into a forked child, so a timer's count would
/// split. ptrace and KVM set neither; `detcore_runs_outside_the_guest_per_backend`
/// pins the answer for every backend Reverie defines.
pub(crate) fn detcore_runs_outside_the_guest(backend: &reverie::BackendCapabilities) -> bool {
    !backend.tool_shares_guest_descriptor_table && !backend.process_exits_complete_asynchronously
}

/// Whether Detcore keeps timerfds on virtual time (see the module comment
/// for the scope): virtual time under sequentialized threads, outside
/// record/replay, where Detcore runs outside the guest.
pub(crate) fn enabled<T, G>(guest: &G) -> bool
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let config = guest.config();
    config.virtualize_time
        && config.sequentialize_threads
        && !config.recordreplay_modes
        && detcore_runs_outside_the_guest(&config.backend)
}

/// Ends the run on a refusal that names `what`.
pub(crate) fn refuse(what: std::fmt::Arguments<'_>) -> ! {
    crate::scheduler::exit_on_scheduler_refusal(&format_args!(
        "hermit: refusing the run: {what}; a timerfd on Detcore's virtual clock cannot be served here (https://github.com/rrnewton/hermit/issues/1923)"
    ))
}

/// Whether `fd` names a timerfd in this process's descriptor table.
pub(crate) fn is_timerfd<T, G>(guest: &G, fd: RawFd) -> bool
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    guest
        .thread_state()
        .with_detfd(fd, |detfd| detfd.ty() == FdType::Timerfd)
        .unwrap_or(false)
}

/// Whether this process holds a timerfd on virtual time.
pub(crate) fn has_virtual_timerfds<T, G>(guest: &G) -> bool
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    enabled(guest) && !guest.thread_state().virtual_timerfds().is_empty()
}

/// Before `call` waits in the host, outside Detcore's probes: refuse the run
/// by name if the process holds a timerfd on virtual time, which the call's
/// descriptors may include and which the host would never make readable.
pub(crate) fn refuse_host_wait<T, G>(guest: &G, call: &str)
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if has_virtual_timerfds(guest) {
        refuse(format_args!(
            "{call} would wait in the host on descriptors that may include a timerfd that only Detcore's probes make readable"
        ));
    }
}

/// Most descriptors a `poll`/`ppoll` list or `select`/`pselect6` set Detcore
/// reads to decide whether it names a timerfd; a larger call counts as naming
/// one.
const MAX_SCANNED_DESCRIPTORS: usize = 1 << 16;

/// Whether `call`, about to wait in the host, may name a timerfd: one of its
/// `poll`/`ppoll` entries or `select`/`pselect6` set bits is a timerfd, or it
/// reads one. An epoll descriptor in the list or set (its members, which
/// Detcore cannot see, may include a timerfd), or a list Detcore cannot read,
/// counts as naming one.
///
/// The list is read before the call is released to wait in the host, so a
/// sibling thread that rewrites it in between could add a timerfd unseen: the
/// same window the masked-`pselect6` staging describes. It needs a guest
/// program that races its own poll arguments, and only ends in a wait where a
/// refusal was due, never in a wrong result.
fn call_may_name_timerfd<T, G>(guest: &G, call: Syscall) -> bool
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let names = |fds: &mut dyn Iterator<Item = RawFd>| {
        for fd in fds {
            let ty = guest.thread_state().with_detfd(fd, |detfd| detfd.ty());
            if matches!(ty, Ok(FdType::Timerfd) | Ok(FdType::Epoll)) {
                return true;
            }
        }
        false
    };
    let pollfds = |address: Option<usize>, nfds: u64| -> bool {
        let count = nfds as u32 as usize;
        if count == 0 {
            return false;
        }
        let Some(address) = address.and_then(syscalls::Addr::<u8>::from_raw) else {
            return true;
        };
        if count > MAX_SCANNED_DESCRIPTORS {
            return true;
        }
        let mut bytes = vec![0u8; count * std::mem::size_of::<libc::pollfd>()];
        if guest.memory().read_exact(address, &mut bytes).is_err() {
            return true;
        }
        names(
            &mut bytes
                .as_chunks::<{ std::mem::size_of::<libc::pollfd>() }>()
                .0
                .iter()
                .map(|entry| RawFd::from_ne_bytes(entry[..4].try_into().unwrap())),
        )
    };
    let fd_sets = |nfds: i32, sets: [Option<usize>; 3]| -> bool {
        let Ok(count) = usize::try_from(nfds) else {
            return false;
        };
        if count > MAX_SCANNED_DESCRIPTORS {
            return true;
        }
        let len = count.div_ceil(64) * 8;
        sets.into_iter().flatten().any(|set| {
            let Some(address) = syscalls::Addr::<u8>::from_raw(set) else {
                return false;
            };
            let mut bits = vec![0u8; len];
            if guest.memory().read_exact(address, &mut bits).is_err() {
                return true;
            }
            names(
                &mut (0..count)
                    .filter(|fd| bits[fd / 8] & (1 << (fd % 8)) != 0)
                    .map(|fd| fd as RawFd),
            )
        })
    };
    match call {
        Syscall::Poll(poll) => pollfds(poll.fds().map(|fds| fds.as_raw()), poll.nfds()),
        Syscall::Ppoll(ppoll) => pollfds(ppoll.fds().map(|fds| fds.as_raw()), ppoll.nfds()),
        Syscall::Select(select) => fd_sets(
            select.nfds(),
            [select.readfds(), select.writefds(), select.exceptfds()]
                .map(|set| set.map(|set| set.as_raw())),
        ),
        Syscall::Pselect6(pselect6) => fd_sets(
            pselect6.nfds(),
            [
                pselect6.readfds(),
                pselect6.writefds(),
                pselect6.exceptfds(),
            ]
            .map(|set| set.map(|set| set.as_raw())),
        ),
        Syscall::Read(read) => is_timerfd(guest, read.fd()),
        Syscall::Readv(readv) => is_timerfd(guest, readv.fd()),
        _ => true,
    }
}

/// Before `call` waits in the host, outside Detcore's probes: refuse the run
/// by name if it can observe a timerfd, the process holds one on virtual
/// time, and the call may name it (`call_may_name_timerfd`).
pub(crate) fn refuse_host_wait_for<T, G>(guest: &G, call: Syscall)
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if has_virtual_timerfds(guest)
        && observes_timerfds(guest, call)
        && call_may_name_timerfd(guest, call)
    {
        refuse(format_args!(
            "{} would wait in the host on descriptors that may include a timerfd that only Detcore's probes make readable",
            call.number().name()
        ));
    }
}

/// Whether a probe of `call` can observe a timerfd's readiness or count, so it
/// must publish first and its poll deadline includes the next expiration: a
/// readiness wait, or a read of a timerfd.
pub(crate) fn observes_timerfds<T, G>(guest: &G, call: Syscall) -> bool
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    match call {
        Syscall::Poll(_)
        | Syscall::Ppoll(_)
        | Syscall::Select(_)
        | Syscall::Pselect6(_)
        | Syscall::EpollWait(_)
        | Syscall::EpollPwait(_) => true,
        Syscall::Other(Sysno::epoll_pwait2, _) => true,
        Syscall::Read(read) => is_timerfd(guest, read.fd()),
        Syscall::Readv(readv) => is_timerfd(guest, readv.fd()),
        _ => false,
    }
}

fn valid(ts: &libc::timespec) -> bool {
    ts.tv_sec >= 0 && (0..1_000_000_000).contains(&ts.tv_nsec)
}

fn timespec_ns(ts: &libc::timespec) -> u64 {
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

fn itimerspec(remaining_ns: u64, interval_ns: u64) -> libc::itimerspec {
    let ts = |ns: u64| libc::timespec {
        tv_sec: (ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (ns % 1_000_000_000) as libc::c_long,
    };
    libc::itimerspec {
        it_interval: ts(interval_ns),
        it_value: ts(remaining_ns),
    }
}

/// The guest clock at this probe, without raising its floor: what
/// `clock_gettime` would return now. Timerfd deadlines live on it.
async fn timerfd_now<T, G>(guest: &mut G) -> LogicalTime
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let raw = thread_observe_time(guest).await;
    guest.thread_state().peek_guest_clock(raw)
}

/// Counts every expiration of this process's virtual timerfds at or before the
/// guest clock, makes each timerfd that gained one readable on the host, and
/// only then records them. Returns the earliest expiration still ahead, for a
/// poller's deadline ([`poll_deadline_with`]). A failure refuses the run.
///
/// Call it in the thread's turn, before a probe that `observes_timerfds`.
pub(crate) async fn publish_expired<T, G>(guest: &mut G) -> Option<LogicalTime>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    publish_expired_in(guest, None).await
}

/// Guest memory for making an expired timerfd readable, reserved by a caller
/// that holds the scratch stack across its probes ([`reserve_scratch`]), so
/// [`publish_expired_in`] need not take the stack itself.
#[derive(Clone, Copy)]
pub(crate) struct Scratch {
    setting: AddrMut<'static, libc::itimerspec>,
    pollfd: AddrMut<'static, libc::pollfd>,
}

/// Reserve a [`Scratch`] on `stack`, before the caller commits it.
/// Whether this process holds an armed timerfd on Detcore's virtual clock:
/// exactly when [`publish_expired_in`] can need guest scratch. A caller that
/// stages its own cells on the stack reserves the scratch only then, so a call
/// on a process with no armed timerfd keeps every byte of the stack it had
/// before timerfds were virtualized (a large masked `pselect6` on a tight
/// stack, `stage_private_cells`). It cannot change during one system call:
/// the guest runs no code between staging and publication.
pub(crate) fn holds_armed_virtual_timerfd<T, G>(guest: &G) -> bool
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    enabled(guest)
        && guest
            .thread_state()
            .virtual_timerfds()
            .iter()
            .any(|(_, detfd)| detfd.virtual_timerfd().and_then(|t| t.deadline).is_some())
}

pub(crate) fn reserve_scratch<S: Stack>(stack: &mut S) -> Scratch {
    Scratch {
        setting: stack.reserve(),
        pollfd: stack.reserve(),
    }
}

/// Whether a probe of `call` must publish first and may need guest scratch to
/// do so: it can observe a timerfd, and one is armed. A caller whose scratch
/// stack is committed releases it before `publish_expired` when this holds.
pub(crate) fn may_publish<T, G>(guest: &G, call: Syscall) -> bool
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    enabled(guest)
        && observes_timerfds(guest, call)
        && guest
            .thread_state()
            .virtual_timerfds()
            .iter()
            .any(|(_, detfd)| detfd.virtual_timerfd().and_then(|t| t.deadline).is_some())
}

/// [`publish_expired`] using `scratch` when the caller holds the scratch
/// stack, or the stack itself when `scratch` is `None`.
pub(crate) async fn publish_expired_in<T, G>(
    guest: &mut G,
    scratch: Option<Scratch>,
) -> Option<LogicalTime>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if !holds_armed_virtual_timerfd(guest) {
        return None;
    }
    let timerfds = guest.thread_state().virtual_timerfds();
    let now = timerfd_now(guest).await;
    let mut next: Option<LogicalTime> = None;
    for (fd, detfd) in timerfds {
        let Some(mut timer) = detfd.virtual_timerfd() else {
            continue;
        };
        let added = timer.expire_through(now);
        if added > 0 {
            make_readable(guest, fd, scratch).await;
            detfd.set_virtual_timerfd(Some(timer));
            detlog!(
                "[dtid {}] timerfd {} expired {} time(s) at virtual time {}, {} unread",
                guest.thread_state().dettid,
                fd,
                added,
                now,
                timer.unread,
            );
        }
        if let Some(deadline) = timer.deadline {
            next = Some(next.map_or(deadline, |earliest| earliest.min(deadline)));
        }
    }
    next
}

/// A polling request's deadline: the earlier of the call's own timeout and
/// the next virtual timerfd expiration, so a backed-off poller is restored
/// when a timerfd it may watch expires (`Resources::poll_deadline`).
pub(crate) fn poll_deadline_with<T, G>(
    guest: &G,
    timeout: Option<LogicalTime>,
    next_timerfd: Option<LogicalTime>,
) -> Option<LogicalTime>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let deadline = match (timeout, next_timerfd) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    poll_deadline_for(guest, deadline)
}

/// `publish_expired` before a probe of `call`, if it can observe a timerfd;
/// returns the poll deadline the probe's request carries.
pub(crate) async fn publish_before_probe<T, G>(
    guest: &mut G,
    call: Syscall,
    timeout: Option<LogicalTime>,
) -> Option<LogicalTime>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if !observes_timerfds(guest, call) {
        return poll_deadline_for(guest, timeout);
    }
    let next = publish_expired(guest).await;
    poll_deadline_with(guest, timeout, next)
}

/// The fault the hook asks of this publication, at most once per process.
fn take_fault() -> Option<Fault> {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    static FIRED: AtomicBool = AtomicBool::new(false);
    let fault = match std::env::var(FAULT_ENV).as_deref() {
        Ok("never-readable") => Fault::NeverReadable,
        Ok("signal-before-arm") => Fault::SignalBeforeArm,
        _ => return None,
    };
    (!FIRED.swap(true, Ordering::SeqCst)).then_some(fault)
}

/// Inject `call`, retrying an interruption (a pending host signal stops an
/// injection with `ERESTARTSYS` or `EINTR`) until `start + valve`; any other
/// failure, or interruptions past the limit, refuses the run, naming `what`
/// and the timerfd `fd`.
async fn inject_bounded<T, G, S>(
    guest: &mut G,
    call: S,
    start: Instant,
    valve: Duration,
    what: &'static str,
    fd: RawFd,
) -> i64
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
    S: SyscallInfo + Copy,
{
    loop {
        match guest.inject(call).await {
            Ok(value) => return value,
            Err(Errno::ERESTARTSYS | Errno::EINTR) if start.elapsed() < valve => {}
            Err(errno) => refuse(format_args!("{what} {fd} failed with {errno}")),
        }
    }
}

/// Make the host timerfd `fd` readable and wait until the host reports it
/// readable: arm it to expire at absolute time 1 ns on its own clock, which
/// has passed, so the host fires it at once. Both injections retry an
/// interruption, so a host signal stop changes nothing. Any failure, or no
/// readiness (or only interruptions) within `READABLE_VALVE` of host time from
/// the start of this publication, refuses the run.
async fn make_readable<T, G>(guest: &mut G, fd: RawFd, scratch: Option<Scratch>)
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let (setting, pollfd, _guard) = match scratch {
        Some(scratch) => (scratch.setting, scratch.pollfd, None),
        None => {
            let mut stack = guest.stack().await;
            let scratch = reserve_scratch(&mut stack);
            let Ok(guard) = stack.commit() else {
                refuse(format_args!(
                    "no scratch memory to make expired timerfd {fd} readable"
                ));
            };
            (scratch.setting, scratch.pollfd, Some(guard))
        }
    };
    let fault = take_fault();
    let never_readable = fault == Some(Fault::NeverReadable);
    let valve = if never_readable {
        Duration::from_secs(1)
    } else {
        READABLE_VALVE
    };
    let start = Instant::now();
    if !never_readable {
        if let Err(error) = guest.memory().write_value(setting, &itimerspec(1, 0)) {
            refuse(format_args!(
                "could not stage the setting that makes expired timerfd {fd} readable: {error}"
            ));
        }
        let arm = syscalls::TimerfdSettime::new()
            .with_fd(fd)
            .with_flags(libc::TFD_TIMER_ABSTIME)
            .with_new_value(Some(setting.into()))
            .with_old_value(None);
        // The guest is a host process only where the backend does not
        // deliver its signals itself (ptrace); elsewhere its ids name no
        // host thread, so the hook does nothing.
        if fault == Some(Fault::SignalBeforeArm)
            && !guest.config().backend.provides_process_signal_control
        {
            // SAFETY: tgkill takes only integer arguments.
            unsafe {
                libc::syscall(
                    libc::SYS_tgkill,
                    guest.pid().as_raw(),
                    guest.tid().as_raw(),
                    libc::SIGCONT,
                );
            }
            if let Err(Errno::ERESTARTSYS | Errno::EINTR) = guest.inject(arm).await {
                tracing::warn!(
                    "[detcore, dtid {}] timerfd fault hook: a host signal interrupted arming timerfd {}; retrying",
                    guest.thread_state().dettid,
                    fd,
                );
            }
        }
        inject_bounded(
            guest,
            arm,
            start,
            valve,
            "arming, to make its expiration readable, the host timerfd",
            fd,
        )
        .await;
    }
    loop {
        if let Err(error) = guest.memory().write_value(
            pollfd,
            &libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ) {
            refuse(format_args!(
                "could not stage the readiness probe of timerfd {fd}: {error}"
            ));
        }
        // A 1 ms host wait where the backend honors it (ptrace); KVM probes
        // without waiting, so this loop then spins until the timer fires.
        let probe = syscalls::Poll::new()
            .with_fds(Some(pollfd.cast()))
            .with_nfds(1)
            .with_timeout(1);
        let ready = inject_bounded(
            guest,
            probe,
            start,
            valve,
            "probing for its expiration the host timerfd",
            fd,
        )
        .await;
        if ready > 0 {
            match guest.memory().read_value(pollfd) {
                Ok(pollfd) if pollfd.revents & libc::POLLIN != 0 => return,
                Ok(_) => {}
                Err(error) => refuse(format_args!(
                    "could not read the readiness probe of timerfd {fd}: {error}"
                )),
            }
        }
        if start.elapsed() >= valve {
            refuse(format_args!(
                "the host timerfd {fd} did not become readable within {valve:?} of being armed to expire at once"
            ));
        }
    }
}

/// `timerfd_settime` on a virtual timerfd, or `None` when Detcore leaves the
/// call to the host (virtual timerfds off, or `fd` not a timerfd, whose
/// error the host then reports).
pub(crate) async fn settime<T, G>(
    guest: &mut G,
    call: syscalls::TimerfdSettime,
) -> Result<Option<i64>, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let fd = call.fd();
    if !enabled(guest) || !is_timerfd(guest, fd) {
        return Ok(None);
    }
    let new_ptr = call.new_value().ok_or(Errno::EFAULT)?;
    let new: libc::itimerspec = guest.memory().read_value(new_ptr)?;
    if !valid(&new.it_value) || !valid(&new.it_interval) {
        return Err(Errno::EINVAL.into());
    }
    // The host timerfd stays disarmed: the call runs with the guest's flags,
    // less TFD_TIMER_CANCEL_ON_SET (which would let a host clock step make it
    // readable at a host-timed moment), and a zero setting, so Linux still
    // validates the flags and the descriptor, and discards any expiration the
    // host holds, as a setting change does.
    {
        let mut stack = guest.stack().await;
        let disarm: AddrMut<libc::itimerspec> = stack.reserve();
        let _guard = stack.commit()?;
        guest.memory().write_value(disarm, &itimerspec(0, 0))?;
        guest
            .inject_with_retry(
                call.with_flags(call.flags() & !libc::TFD_TIMER_CANCEL_ON_SET)
                    .with_new_value(Some(disarm.into()))
                    .with_old_value(None),
            )
            .await?;
    }
    let now = timerfd_now(guest).await;
    let detfd = guest.thread_state().with_detfd(fd, |detfd| detfd.clone())?;
    let mut old = detfd.virtual_timerfd().unwrap_or_default();
    old.expire_through(now);
    let (old_remaining_ns, old_interval_ns) = old.remaining(now);
    let value_ns = timespec_ns(&new.it_value);
    let interval_ns = timespec_ns(&new.it_interval);
    let deadline = if value_ns == 0 {
        None
    } else if call.flags() & libc::TFD_TIMER_ABSTIME != 0 {
        Some(LogicalTime::from_nanos(value_ns))
    } else {
        Some(LogicalTime::from_nanos(
            now.as_nanos().saturating_add(value_ns),
        ))
    };
    // Linux keeps the interval of a disarmed timer for timerfd_gettime.
    detfd.set_virtual_timerfd(Some(VirtualTimerfd::set(deadline, interval_ns)));
    detlog!(
        "[dtid {}] timerfd_settime(fd={}, flags={:#x}, interval_ns={}, value_ns={}) armed against virtual clock",
        guest.thread_state().dettid,
        fd,
        call.flags(),
        interval_ns,
        value_ns,
    );
    // As Linux, the old setting is stored after the new one takes effect.
    if let Some(old_ptr) = call.old_value() {
        guest
            .memory()
            .write_value(old_ptr, &itimerspec(old_remaining_ns, old_interval_ns))?;
    }
    Ok(Some(0))
}

/// `timerfd_gettime` on a virtual timerfd, or `None` for the host to answer.
pub(crate) async fn gettime<T, G>(
    guest: &mut G,
    call: syscalls::TimerfdGettime,
) -> Result<Option<i64>, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let fd = call.fd();
    if !enabled(guest) || !is_timerfd(guest, fd) {
        return Ok(None);
    }
    let detfd = guest.thread_state().with_detfd(fd, |detfd| detfd.clone())?;
    if detfd.virtual_timerfd().is_none() {
        return Ok(None);
    }
    let value_ptr = call.value().ok_or(Errno::EFAULT)?;
    publish_expired(guest).await;
    let now = timerfd_now(guest).await;
    let timer = detfd.virtual_timerfd().unwrap_or_default();
    let (remaining_ns, interval_ns) = timer.remaining(now);
    guest
        .memory()
        .write_value(value_ptr, &itimerspec(remaining_ns, interval_ns))?;
    Ok(Some(0))
}

/// The count a successful read of the virtual timerfd `fd` returns, which the
/// read consumes: the expirations counted on the virtual clock, instead of
/// the host's count of Detcore's own arming. `None` when there is nothing to
/// replace.
fn take_unread<T, G>(guest: &mut G, fd: RawFd) -> Option<u64>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let detfd = guest
        .thread_state()
        .with_detfd(fd, |detfd| detfd.clone())
        .ok()?;
    let mut timer = detfd.virtual_timerfd()?;
    if timer.unread == 0 {
        return None;
    }
    let unread = timer.unread;
    timer.unread = 0;
    detfd.set_virtual_timerfd(Some(timer));
    Some(unread)
}

/// After a `read` of the timerfd `fd` into `buf` returned `result`.
pub(crate) fn finish_read<T, G>(
    guest: &mut G,
    fd: RawFd,
    buf: Option<AddrMut<u8>>,
    result: Result<i64, Error>,
) -> Result<i64, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if !enabled(guest) || !matches!(result, Ok(8)) {
        return result;
    }
    let Some(buf) = buf else {
        return result;
    };
    if let Some(unread) = take_unread(guest, fd) {
        guest.memory().write_value(buf.cast::<u64>(), &unread)?;
    }
    result
}

/// After a `readv` of the timerfd `fd` with the `iovcnt` iovecs at `iov`
/// returned `result`. Linux copies the 8-byte count through the iovecs in
/// order (`read_iter`), so the virtual count is scattered the same way.
pub(crate) fn finish_readv<T, G>(
    guest: &mut G,
    fd: RawFd,
    iov: Option<syscalls::Addr<libc::iovec>>,
    iovcnt: usize,
    result: Result<i64, Error>,
) -> Result<i64, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if !enabled(guest) || !matches!(result, Ok(8)) {
        return result;
    }
    let Some(iov) = iov else {
        return result;
    };
    let Some(unread) = take_unread(guest, fd) else {
        return result;
    };
    let bytes = unread.to_ne_bytes();
    let mut written = 0;
    for index in 0..iovcnt {
        if written == bytes.len() {
            break;
        }
        let Some(slot) = syscalls::Addr::<libc::iovec>::from_raw(
            iov.as_raw() + index * std::mem::size_of::<libc::iovec>(),
        ) else {
            break;
        };
        let entry: libc::iovec = guest.memory().read_value(slot)?;
        let take = entry.iov_len.min(bytes.len() - written);
        if take == 0 {
            continue;
        }
        let Some(base) = AddrMut::<u8>::from_raw(entry.iov_base as usize) else {
            break;
        };
        guest
            .memory()
            .write_exact(base, &bytes[written..written + take])?;
        written += take;
    }
    result
}

#[cfg(test)]
mod tests {
    use reverie::BackendCapabilities;

    use super::detcore_runs_outside_the_guest;

    /// Virtual timerfds are served only where Detcore runs outside the guest:
    /// the ptrace family and KVM, not SaBRe, DBT or in-guest LiteInst.
    #[test]
    fn detcore_runs_outside_the_guest_per_backend() {
        for (name, backend, outside) in [
            ("ptrace", BackendCapabilities::PTRACE, true),
            ("e9patch", BackendCapabilities::E9PATCH, true),
            ("kvm", BackendCapabilities::KVM, true),
            ("sabre", BackendCapabilities::SABRE, false),
            ("dbt", BackendCapabilities::DBT, false),
            (
                "liteinst in-guest",
                BackendCapabilities::LITEINST_IN_GUEST,
                false,
            ),
        ] {
            assert_eq!(detcore_runs_outside_the_guest(&backend), outside, "{name}");
        }
    }
}
