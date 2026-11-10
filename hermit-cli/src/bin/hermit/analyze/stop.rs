/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Stopping `hermit analyze` on SIGTERM, SIGINT or SIGHUP.
//!
//! At their default action these signals kill analyze where it stands. The
//! trial running dies with it (its container init has a parent-death
//! signal), but analyze's workspace and the trial's own temporary
//! directories stay behind. While [`StopSignals`] is held, a stop signal
//! instead ends the trial running ([`signal_children`]), a trial whose
//! container init is forked after it exits at once
//! (`container::exit_if_stop_requested`), and every later trial refuses to
//! start ([`check`]), so analyze unwinds, removes what it created, and then
//! dies of the same signal (`AnalyzeOpts::main`).
//!
//! The kernel runs the handler on any thread of analyze that does not block
//! the signal (`complete_signal` and `wants_signal`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v6.9#n978>),
//! and a child can be reaped on another thread while it runs. So the handler
//! never signals a child by its number, which the kernel can give to another
//! process once the child is reaped: it signals through a pidfd it checks
//! ([`signal_children`]), which reaches that child or no one.
//!
//! The threads that leave the stop signals unblocked are the one that drives
//! the trials and every thread a trial starts in analyze's process while it
//! runs, which takes its creator's unblocked mask (`hermit run`'s group-kill
//! policing thread, `container::police_group_kills`, or a runtime's worker
//! or blocking-pool thread); the handler is safe on each. Only the memory watchdog's
//! thread, which analyze starts itself, blocks the stop signals from its
//! first instruction ([`spawn_blocking_stop_signals`]). Beyond the pidfd, the
//! handler stores the signal in an atomic ([`check`]) and reads `/proc`,
//! which reads the same from every thread.
//!
//! A signal the handler cannot pass on to a child it found (no descriptor
//! left for the scan or the pidfd, a pidfd call that fails for any reason
//! but the child having gone) is never swallowed: analyze says so on stderr
//! and dies of the signal by its default action, as it would without the
//! handler, and the trial's container init dies with it by its parent-death
//! signal. Such a death unwinds nothing, so the workspace stays behind, and so
//! does the private temporary directory of the trial that was running
//! ([`signal_children`], [`die_by_default_action`]).

use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

use anyhow::Context as _;

use crate::app::container;

/// The signals a supervisor, a shell or a terminal sends to stop a process.
const STOP_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// The process that installed [`on_stop_signal`]. A process forked from it
/// inherits the handler until it installs its own.
static HOLDER: AtomicI32 = AtomicI32::new(0);

/// How many children analyze may already have when it installs the handler
/// and still pass signals on.
const INHERITED_CAPACITY: usize = 64;

/// The children analyze already had when it installed the handler, 0 in the
/// unused slots. A process keeps its children across `exec`, so these are
/// processes whatever ran analyze started (`job & exec hermit analyze ...`,
/// or the `tee` behind `exec > >(tee log)`), not analyze's to stop. Nothing
/// reaps them, so their PIDs cannot be reused while analyze runs.
static INHERITED: [AtomicI32; INHERITED_CAPACITY] =
    [const { AtomicI32::new(0) }; INHERITED_CAPACITY];

/// The `/proc` directory the handler scans, opened when it is installed and
/// kept for the life of the process, deliberately: a scan then needs no
/// descriptor for it, and no handler still scanning on another thread can
/// find it closed; -1 until then.
static PROC_DIR: AtomicI32 = AtomicI32::new(-1);

/// Set while a handler scans `/proc` through [`PROC_DIR`], whose offset
/// every scan moves.
static SCANNING: AtomicBool = AtomicBool::new(false);

/// A stop signal arrived: analyze starts no further trial and reads no
/// result of the trial it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopped(pub reverie::Signal);

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "hermit analyze received {}", self.0)
    }
}

impl std::error::Error for Stopped {}

/// `Err` once a stop signal has arrived.
pub fn check() -> Result<(), Stopped> {
    stopped(container::stop_requested())
}

fn stopped(recorded: Option<libc::c_int>) -> Result<(), Stopped> {
    match recorded {
        None => Ok(()),
        // Only the three stop signals are recorded.
        Some(raw) => Err(Stopped(
            reverie::Signal::try_from(raw).unwrap_or(reverie::Signal::SIGTERM),
        )),
    }
}

/// Analyze's handler for every stop signal.
///
/// In analyze, it records the first signal (`container::request_stop`) and
/// passes every one on to the children analyze started: the container init
/// of the trial running, whose own handler exits 128 + signo and so takes the
/// trial's namespace down. A later signal reaches a trial that started after
/// an earlier one.
///
/// In a process analyze forked that has not installed its own handler yet,
/// which is a trial's container init during Reverie's container setup, it
/// exits 128 + signo, as that init's handler will
/// (`ContainerInitStopSignalBridge` does the same while analyze holds no
/// handler).
extern "C" fn on_stop_signal(signal: libc::c_int) {
    // SAFETY: getpid(2) and _exit(2) are async-signal-safe.
    let pid = unsafe { libc::getpid() };
    if pid != HOLDER.load(Ordering::Relaxed) {
        unsafe { libc::_exit(128 + signal) }
    }
    // SAFETY: errno is this thread's; restoring it keeps the code this
    // handler interrupted from reading a value the handler left.
    let errno = unsafe { *libc::__errno_location() };
    container::request_stop(signal);
    if let Err(undelivered) = signal_children(pid, signal, inherited) {
        die_by_default_action(signal, undelivered);
    }
    unsafe { *libc::__errno_location() = errno };
}

/// How long the diagnostic of [`die_by_default_action`] may take before the
/// signal ends analyze anyway.
const DIAGNOSTIC_BOUND: libc::timespec = libc::timespec {
    tv_sec: 1,
    tv_nsec: 0,
};

/// End analyze by `signal`'s default action, after saying on stderr that
/// its handler could not pass the signal on (`undelivered`): the stop is
/// then what it would be without the handler. The trial's container init
/// dies with analyze by its parent-death signal. Nothing unwinds, so the
/// workspace and the running trial's private temporary directory stay.
/// Async-signal-safe.
///
/// Death never waits on the diagnostic. The default action is armed first,
/// and a timer sends the same signal to the process after
/// [`DIAGNOSTIC_BOUND`] (`SIGEV_SIGNAL` without `SIGEV_THREAD_ID` is
/// process-directed, `posix_timer_event`,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v6.9#n280>),
/// before the signal is unblocked on this thread and the diagnostic written:
/// a write that blocks, on a full pipe whose reader is alive, ends when the
/// timer's signal kills the process. Without a timer the diagnostic is left
/// out. Every system call is made directly, with no descriptor opened.
fn die_by_default_action(signal: libc::c_int, undelivered: Undelivered) -> ! {
    let mut name = [0u8; 10];
    let mut errno = [0u8; 10];
    // SAFETY: sigaction(2), sigemptyset(3), sigaddset(3), pthread_sigmask(3),
    // timer_create(2), timer_settime(2), write(2), raise(3) and _exit(2) are
    // async-signal-safe, and every pointer refers to a local that outlives
    // the call. An all-zero sigaction is SIG_DFL; the kernel writes its
    // timer id, an int, into `timer`.
    unsafe {
        let default: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signal, &default, std::ptr::null_mut());
        let mut event: libc::sigevent = std::mem::zeroed();
        event.sigev_notify = libc::SIGEV_SIGNAL;
        event.sigev_signo = signal;
        let mut timer: libc::c_int = 0;
        let deadline = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: DIAGNOSTIC_BOUND,
        };
        let bounded = libc::syscall(
            libc::SYS_timer_create,
            libc::CLOCK_MONOTONIC,
            &mut event as *mut libc::sigevent,
            &mut timer as *mut libc::c_int,
        ) == 0
            && libc::syscall(
                libc::SYS_timer_settime,
                timer,
                0,
                &deadline as *const libc::itimerspec,
                std::ptr::null_mut::<libc::itimerspec>(),
            ) == 0;
        let mut unblock: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut unblock);
        libc::sigaddset(&mut unblock, signal);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &unblock, std::ptr::null_mut());
        if bounded {
            for part in [
                &b"hermit analyze: could not pass signal "[..],
                pid_digits(signal, &mut name).unwrap_or(b"?"),
                b" on to the run it started (",
                undelivered.call.as_bytes(),
                b" failed with errno ",
                pid_digits(undelivered.errno, &mut errno).unwrap_or(b"?"),
                b"), so it dies of the signal as it would without its handler, and leaves its \
                  workspace and the trial's private temporary directory behind\n",
            ] {
                libc::write(2, part.as_ptr().cast(), part.len());
            }
        }
        libc::raise(signal);
        libc::_exit(128 + signal)
    }
}

/// Whether `pid` is a child analyze had when it installed the handler.
fn inherited(pid: libc::pid_t) -> bool {
    INHERITED
        .iter()
        .any(|slot| slot.load(Ordering::Relaxed) == pid)
}

/// Holds [`on_stop_signal`] installed; dropping it restores the default
/// action.
#[must_use = "the stop handlers are removed when this guard is dropped"]
pub struct StopSignals {
    /// Which entries of [`STOP_SIGNALS`] this guard installed.
    installed: [bool; STOP_SIGNALS.len()],
    /// The exact handler value installed, compared on drop.
    handler: libc::sighandler_t,
}

impl StopSignals {
    /// Install the handler for every stop signal still at its default
    /// action. One that is ignored (SIGINT in a shell's background job,
    /// SIGHUP under `nohup`) or handled stays as it is, and so does every
    /// signal of a process that is PID 1: the container init of a trial sees
    /// itself as PID 1 too, so the handler could not tell the two apart.
    /// Nothing is installed either when the handler could not find the run's
    /// container init ([`record_inherited_children`]), or could not signal it
    /// safely: where a pidfd cannot be opened, signalled through, or waited on
    /// ([`usable_pidfds`]). Such an analyze dies of the signal, or ignores
    /// it, as before.
    ///
    /// Call it while no other thread of analyze can take a stop signal: the
    /// query and the install are two steps. The memory watchdog may run
    /// already, as it does when `main` installs this, because it blocks the
    /// stop signals before it starts.
    pub fn install() -> anyhow::Result<Self> {
        let handler: extern "C" fn(libc::c_int) = on_stop_signal;
        let mut guard = Self {
            installed: [false; STOP_SIGNALS.len()],
            handler: handler as libc::sighandler_t,
        };
        // SAFETY: getpid(2) has no preconditions.
        let pid = unsafe { libc::getpid() };
        if pid == 1 {
            return Ok(guard);
        }
        // Before any disposition changes: a handler that recorded a stop but
        // could not end the run in progress would leave that run going.
        if !usable_pidfds(pid) || !record_inherited_children(pid) {
            return Ok(guard);
        }
        HOLDER.store(pid, Ordering::Relaxed);
        // SAFETY: an all-zero sigaction is a valid value, and sigemptyset(3)
        // and sigaddset(3) only write the mask of this local.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = guard.handler;
        // Interrupted system calls resume: the trial's wait ends when its
        // init exits, not with EINTR.
        action.sa_flags = libc::SA_RESTART;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            for signal in STOP_SIGNALS {
                libc::sigaddset(&mut action.sa_mask, signal);
            }
        }
        for (index, signal) in STOP_SIGNALS.into_iter().enumerate() {
            if current_action(signal)?.sa_sigaction != libc::SIG_DFL {
                continue;
            }
            // SAFETY: the handler performs only async-signal-safe calls, and
            // `action` outlives the call.
            if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } == -1 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("cannot install analyze's signal {signal} handler"));
            }
            guard.installed[index] = true;
        }
        Ok(guard)
    }
}

impl Drop for StopSignals {
    fn drop(&mut self) {
        for (index, signal) in STOP_SIGNALS.into_iter().enumerate() {
            // Undo only this guard's own handler: anything else was installed
            // after it on purpose.
            if !self.installed[index]
                || !current_action(signal).is_ok_and(|current| current.sa_sigaction == self.handler)
            {
                continue;
            }
            // SAFETY: an all-zero sigaction is SIG_DFL with no flags.
            let default: libc::sigaction = unsafe { std::mem::zeroed() };
            unsafe { libc::sigaction(signal, &default, std::ptr::null_mut()) };
        }
    }
}

/// Record analyze's present children in [`INHERITED`], and say whether the
/// handler can find the children analyze starts and pass signals on to
/// them: not when `/proc` belongs to another PID namespace (the
/// `/proc/self` link names the reader's PID as that `/proc` numbers it,
/// and `kill` takes PIDs of analyze's own), nor when there are more
/// children than [`INHERITED`] holds.
fn record_inherited_children(pid: libc::pid_t) -> bool {
    let own_proc = std::fs::read_link("/proc/self")
        .ok()
        .and_then(|link| link.to_str()?.parse::<libc::pid_t>().ok())
        == Some(pid);
    if !own_proc || proc_dir().is_none() {
        return false;
    }
    for slot in &INHERITED {
        slot.store(0, Ordering::Relaxed);
    }
    let mut recorded = 0;
    let mut fits = true;
    let scanned = for_each_child(pid, |child| match INHERITED.get(recorded) {
        Some(slot) => {
            slot.store(child, Ordering::Relaxed);
            recorded += 1;
        }
        None => fits = false,
    });
    scanned.is_ok() && fits
}

/// The `/proc` directory [`for_each_child`] scans ([`PROC_DIR`]), opened on
/// first use; `None` if it cannot be opened.
fn proc_dir() -> Option<libc::c_int> {
    let open = PROC_DIR.load(Ordering::Acquire);
    if open >= 0 {
        return Some(open);
    }
    // SAFETY: open(2) on a NUL-terminated literal.
    let opened = unsafe {
        libc::open(
            c"/proc".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if opened < 0 {
        return None;
    }
    match PROC_DIR.compare_exchange(-1, opened, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Some(opened),
        Err(other) => {
            // SAFETY: `opened` was opened above and is not shared.
            unsafe { libc::close(opened) };
            Some(other)
        }
    }
}

/// The action installed for `signal`, read without changing it.
fn current_action(signal: libc::c_int) -> anyhow::Result<libc::sigaction> {
    // SAFETY: a null new action makes sigaction(2) a query, and `current` is
    // read only on success.
    let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(signal, std::ptr::null(), &mut current) } == -1 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot read the signal {signal} disposition"));
    }
    Ok(current)
}

/// A system call that failed while the handler passed a signal on, and its
/// errno: the signal may not have reached a child it should have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Undelivered {
    call: &'static str,
    errno: libc::c_int,
}

impl Undelivered {
    /// The `call` that just failed, with this thread's errno.
    fn now(call: &'static str) -> Self {
        // SAFETY: errno is this thread's.
        let errno = unsafe { *libc::__errno_location() };
        Self { call, errno }
    }
}

/// Send `signal` to every child of `parent` that `skip` does not name;
/// returns how many it reached, or the call that failed when it may not have
/// reached one. Async-signal-safe, as [`for_each_child`].
///
/// The handler can run on any thread that does not block the signal, and a
/// child found here can be reaped meanwhile on another thread, and its PID
/// given to a process analyze did not start. So each child is signalled
/// through a pidfd ([`signal_child`]), never by its number. A child that is
/// gone, or a PID that names another process by the time its pidfd is open,
/// is not an error; a scan that could not read `/proc`, or a pidfd call that
/// failed otherwise, is, and the handler then ends analyze by the signal's
/// default action ([`die_by_default_action`]) rather than leave the run
/// going.
///
/// One scan runs at a time: [`PROC_DIR`]'s offset is shared. A handler that
/// finds another scanning, on another thread, leaves the children to it; a
/// container init exits on the first stop signal it gets.
pub(super) fn signal_children(
    parent: libc::pid_t,
    signal: libc::c_int,
    skip: impl Fn(libc::pid_t) -> bool,
) -> Result<usize, Undelivered> {
    if SCANNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(0);
    }
    let mut reached = 0;
    let mut failed = None;
    let scanned = for_each_child(parent, |child| {
        if skip(child) || failed.is_some() {
            return;
        }
        #[cfg(test)]
        tests::in_the_window(child);
        match signal_child(parent, child, signal) {
            Ok(true) => reached += 1,
            Ok(false) => {}
            Err(undelivered) => failed = Some(undelivered),
        }
    });
    SCANNING.store(false, Ordering::Release);
    scanned?;
    match failed {
        Some(undelivered) => Err(undelivered),
        None => Ok(reached),
    }
}

/// Send `signal` to `pid` through a pidfd if `pid` is a child of `parent`:
/// `Ok(true)` if a child received it, `Ok(false)` if `pid` names no child of
/// `parent` any more, and the failed call otherwise. Async-signal-safe:
/// system calls on stack buffers.
///
/// A pidfd names the process that held `pid` when it was opened, and a signal
/// sent through it reaches that process or, once it is reaped, none: the
/// kernel looks the process up by the pidfd's `struct pid`, never by number
/// (`pidfd_open`,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/pid.c?h=v6.9#n629>;
/// `pidfd_send_signal` and `kill_pid_info_type`,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v6.9#n1483>).
/// Which process that is, is checked through the pidfd too, after it is
/// open: `waitid(P_PIDFD, ..., WNOHANG | WNOWAIT)` succeeds only for a child
/// of the caller's thread group, running or exited and not yet reaped, and
/// reaps nothing; for any other process it fails with ECHILD
/// (`kernel_waitid_prepare` and `eligible_child`,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/exit.c?h=v6.9#n1679>).
/// The check needs no descriptor beyond the pidfd. `parent` other than the
/// calling process's own PID is checked through `/proc` instead, for tests.
fn signal_child(
    parent: libc::pid_t,
    pid: libc::pid_t,
    signal: libc::c_int,
) -> Result<bool, Undelivered> {
    // SAFETY: pidfd_open(2) has no memory arguments.
    let opened = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    let Some(pidfd) = libc::c_int::try_from(opened).ok().filter(|&fd| fd >= 0) else {
        let undelivered = Undelivered::now("pidfd_open");
        return match undelivered.errno {
            libc::ESRCH => Ok(false),
            _ => Err(undelivered),
        };
    };
    let delivered = is_child(parent, pid, pidfd).and_then(|child| {
        if !child {
            return Ok(false);
        }
        // SAFETY: pidfd_send_signal(2) with no siginfo; `pidfd` is open.
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd,
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if sent == 0 {
            return Ok(true);
        }
        let undelivered = Undelivered::now("pidfd_send_signal");
        match undelivered.errno {
            // It exited after the check.
            libc::ESRCH => Ok(false),
            _ => Err(undelivered),
        }
    });
    // SAFETY: `pidfd` is the descriptor opened above.
    unsafe { libc::close(pidfd) };
    delivered
}

/// Whether `pidfd`, opened for `pid`, names a child of `parent`: through the
/// pidfd itself for analyze's own children ([`signal_child`]), else (in
/// tests) by `pid`'s parent in `/proc`.
fn is_child(
    parent: libc::pid_t,
    pid: libc::pid_t,
    pidfd: libc::c_int,
) -> Result<bool, Undelivered> {
    // SAFETY: getpid(2) has no preconditions.
    if parent != unsafe { libc::getpid() } {
        let mut digits = [0u8; 10];
        let Some(name) = pid_digits(pid, &mut digits) else {
            return Ok(false);
        };
        let proc_dir = proc_dir().ok_or_else(|| Undelivered::now("open /proc"))?;
        return Ok(parent_of(proc_dir, name)? == Some(parent));
    }
    if waitid_pidfd(pidfd) == 0 {
        return Ok(true);
    }
    let undelivered = Undelivered::now("waitid");
    match undelivered.errno {
        libc::ECHILD => Ok(false),
        _ => Err(undelivered),
    }
}

/// `waitid(P_PIDFD, pidfd, WEXITED | WNOHANG | WNOWAIT | __WALL)`: 0 if
/// `pidfd` names a child of the calling process, running or exited and not
/// yet reaped (reaping nothing), else -1 with errno set (ECHILD for any
/// other process). Made as the raw system call rather than through the C
/// library's wrapper, which POSIX does not list as async-signal-safe.
fn waitid_pidfd(pidfd: libc::c_int) -> libc::c_long {
    // SAFETY: an all-zero siginfo_t is a valid out-parameter; waitid(2)
    // writes only `info`, and a null rusage asks for none.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::syscall(
            libc::SYS_waitid,
            libc::P_PIDFD,
            pidfd,
            &mut info as *mut libc::siginfo_t,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT | libc::__WALL,
            std::ptr::null_mut::<libc::rusage>(),
        )
    }
}

/// `pid` in decimal digits, written into `buffer`.
fn pid_digits(pid: libc::pid_t, buffer: &mut [u8; 10]) -> Option<&[u8]> {
    let mut rest = u32::try_from(pid).ok()?;
    let mut start = buffer.len();
    loop {
        start -= 1;
        buffer[start] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    Some(&buffer[start..])
}

/// Whether [`signal_child`] can work here, tried on this process (`pid`): a
/// pidfd opens (Linux 5.3), a null signal goes through it (5.1), and
/// `waitid` takes it (`P_PIDFD`, Linux 5.4), answering ECHILD since a process
/// is not its own child. A seccomp filter can refuse any of the three.
fn usable_pidfds(pid: libc::pid_t) -> bool {
    // SAFETY: pidfd_open(2) has no memory arguments.
    let opened = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    let Some(pidfd) = libc::c_int::try_from(opened).ok().filter(|&fd| fd >= 0) else {
        return false;
    };
    // SAFETY: pidfd_send_signal(2) with signal 0 and no siginfo sends nothing;
    // waitid(2) writes only `info`; `pidfd` is open until the close.
    unsafe {
        let sends = libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            0,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        ) == 0;
        let waits = waitid_pidfd(pidfd) == -1 && *libc::__errno_location() == libc::ECHILD;
        libc::close(pidfd);
        sends && waits
    }
}

/// Spawn `builder`'s thread with SIGTERM, SIGINT and SIGHUP blocked from its
/// first instruction, so that analyze's stop handler never runs on it. A new
/// thread starts with its creator's signal mask (`dup_task_struct` copies
/// it,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/fork.c?h=v6.9#n1100>),
/// so they are blocked on the calling thread around the spawn and its own
/// mask is restored after.
pub fn spawn_blocking_stop_signals<F, T>(
    builder: std::thread::Builder,
    run: F,
) -> std::io::Result<std::thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    // SAFETY: sigemptyset(3) and sigaddset(3) write only this local, and
    // pthread_sigmask(3) reads and writes the two locals it is given.
    let mut stop: libc::sigset_t = unsafe { std::mem::zeroed() };
    let mut before: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut stop);
        for signal in STOP_SIGNALS {
            libc::sigaddset(&mut stop, signal);
        }
    }
    let blocked = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &stop, &mut before) };
    if blocked != 0 {
        return Err(std::io::Error::from_raw_os_error(blocked));
    }
    let spawned = builder.spawn(run);
    // SAFETY: `before` is the mask pthread_sigmask(3) returned above.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &before, std::ptr::null_mut()) };
    spawned
}

/// Call `visit` with the PID of every child of `parent`; the failed call if
/// `/proc` could not be read whole, when a child may have been missed. A
/// process that exits during the scan is not a failure.
///
/// Reverie gives the trial's container init's PID out only once the trial
/// has ended, so a signal handler finds the init here: the processes whose
/// `/proc/<pid>/stat` names `parent` as their parent (`do_task_stat`,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/proc/array.c?h=v6.9#n534>).
/// `/proc/<pid>/task/<tid>/children` would be shorter, but kernels built
/// without CONFIG_PROC_CHILDREN (which comes with checkpoint/restore) leave it
/// out. The directory is [`PROC_DIR`], read from its start; each `stat` file
/// takes one descriptor while it is read.
///
/// Async-signal-safe: system calls on stack buffers, no allocation, no lock,
/// no panic.
fn for_each_child(
    parent: libc::pid_t,
    mut visit: impl FnMut(libc::pid_t),
) -> Result<(), Undelivered> {
    let proc_dir = proc_dir().ok_or_else(|| Undelivered::now("open /proc"))?;
    // SAFETY: lseek(2) has no memory arguments.
    if unsafe { libc::lseek(proc_dir, 0, libc::SEEK_SET) } < 0 {
        return Err(Undelivered::now("lseek /proc"));
    }
    let reclen_at = std::mem::offset_of!(libc::dirent64, d_reclen);
    let name_at = std::mem::offset_of!(libc::dirent64, d_name);
    let mut entries = [0u8; 4096];
    loop {
        // SAFETY: getdents64(2) writes at most `entries.len()` bytes into it.
        let filled = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                proc_dir,
                entries.as_mut_ptr(),
                entries.len(),
            )
        };
        if filled < 0 {
            return Err(Undelivered::now("getdents64 /proc"));
        }
        let Some(filled) = usize::try_from(filled).ok().filter(|&filled| filled > 0) else {
            return Ok(());
        };
        let mut records = entries.get(..filled).unwrap_or_default();
        while let Some(length) = records
            .get(reclen_at..reclen_at + 2)
            .map(|bytes| usize::from(u16::from_ne_bytes([bytes[0], bytes[1]])))
            .filter(|&length| length > name_at)
        {
            let Some(record) = records.get(..length) else {
                break;
            };
            let name = &record[name_at..];
            let name = &name[..name
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(name.len())];
            if let Some(pid) = parse_pid(name)
                && parent_of(proc_dir, name)? == Some(parent)
            {
                visit(pid);
            }
            records = &records[length..];
        }
    }
}

/// The parent of the process named `pid` (its decimal digits) in the open
/// `/proc` directory `proc_dir`; `None` once it has exited, and the failed
/// call when its `stat` cannot be read otherwise (no descriptor left).
fn parent_of(proc_dir: libc::c_int, pid: &[u8]) -> Result<Option<libc::pid_t>, Undelivered> {
    const STAT: &[u8] = b"/stat\0";
    let mut path = [0u8; 32];
    let Some(()) = path
        .get_mut(..pid.len())
        .map(|name| name.copy_from_slice(pid))
        .and_then(|()| path.get_mut(pid.len()..pid.len() + STAT.len()))
        .map(|tail| tail.copy_from_slice(STAT))
    else {
        return Ok(None);
    };
    // SAFETY: `path` is NUL-terminated within its length.
    let file = unsafe {
        libc::openat(
            proc_dir,
            path.as_ptr().cast(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if file < 0 {
        let undelivered = Undelivered::now("open /proc/<pid>/stat");
        return match undelivered.errno {
            libc::ENOENT | libc::ESRCH => Ok(None),
            _ => Err(undelivered),
        };
    }
    let mut stat = [0u8; 512];
    // SAFETY: read(2) writes at most `stat.len()` bytes into it, and `file`
    // is the descriptor opened above.
    let read = unsafe { libc::read(file, stat.as_mut_ptr().cast(), stat.len()) };
    let undelivered = (read < 0).then(|| Undelivered::now("read /proc/<pid>/stat"));
    unsafe { libc::close(file) };
    match undelivered {
        Some(undelivered) if undelivered.errno == libc::ESRCH => Ok(None),
        Some(undelivered) => Err(undelivered),
        None => Ok(stat
            .get(..usize::try_from(read).unwrap_or_default())
            .and_then(stat_parent)),
    }
}

/// The parent PID in a `/proc/<pid>/stat` line: the field after the state,
/// which follows the command name. The name is in parentheses and may itself
/// hold spaces and parentheses, so the fields are counted from its last
/// closing one.
fn stat_parent(stat: &[u8]) -> Option<libc::pid_t> {
    let name_end = stat.iter().rposition(|&byte| byte == b')')?;
    let mut fields = stat[name_end + 1..]
        .split(|&byte| byte == b' ')
        .filter(|field| !field.is_empty());
    fields.next()?;
    parse_pid(fields.next()?)
}

/// A PID written in decimal digits, and nothing else.
fn parse_pid(digits: &[u8]) -> Option<libc::pid_t> {
    if digits.is_empty() {
        return None;
    }
    let zero: libc::pid_t = 0;
    digits.iter().try_fold(zero, |pid, &digit| {
        if digit.is_ascii_digit() {
            pid.checked_mul(10)?
                .checked_add(libc::pid_t::from(digit - b'0'))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::Command;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use std::time::Instant;

    use super::*;

    /// The function [`in_the_window`] calls, an `fn(libc::pid_t)`, or 0.
    static WINDOW: AtomicUsize = AtomicUsize::new(0);

    /// Where [`signal_children`] pauses for a test: after it found `child`
    /// and before it signals it, the window in which another thread can reap
    /// the child.
    pub(super) fn in_the_window(child: libc::pid_t) {
        let hook = WINDOW.load(Ordering::SeqCst);
        if hook != 0 {
            // SAFETY: only `as_analyze` stores a value here, and it stores an
            // `fn(libc::pid_t)`.
            let hook = unsafe { std::mem::transmute::<usize, fn(libc::pid_t)>(hook) };
            hook(child);
        }
    }

    /// The parent field is read after the command name's last closing
    /// parenthesis, whatever the name holds.
    #[test]
    fn the_parent_is_the_field_after_the_state() {
        assert_eq!(stat_parent(b"12 (sh) S 1 12 12 0 -1"), Some(1));
        assert_eq!(stat_parent(b"345 (a) b (c)) R 77 345"), Some(77));
        assert_eq!(stat_parent(b"345 (a) b (c)) R"), None);
        assert_eq!(stat_parent(b"345 (no close S 77"), None);
        assert_eq!(parse_pid(b"self"), None);
        assert_eq!(parse_pid(b""), None);
        assert_eq!(parse_pid(b"99999999999"), None);
    }

    /// A stop reads as unset until one is recorded, and then as that signal.
    #[test]
    fn the_first_stop_signal_is_the_one_reported() {
        assert_eq!(stopped(None), Ok(()));
        assert_eq!(
            stopped(Some(libc::SIGHUP)),
            Err(Stopped(reverie::Signal::SIGHUP))
        );
        assert_eq!(
            Stopped(reverie::Signal::SIGINT).to_string(),
            "hermit analyze received SIGINT"
        );
    }

    /// The children of a process are found without
    /// `/proc/<pid>/task/<tid>/children`, and only they are signalled, unless
    /// skipped: here a shell's `sleep`, so the shell's `wait` returns
    /// 128 + SIGTERM. A process without children reaches none.
    #[test]
    fn a_stop_signal_reaches_the_children_and_only_them() {
        let mut childless = Command::new("sleep").arg("60").spawn().unwrap();
        assert_eq!(
            signal_children(childless.id() as libc::pid_t, libc::SIGTERM, |_| false),
            Ok(0)
        );
        childless.kill().unwrap();
        childless.wait().unwrap();

        let mut shell = Command::new("sh")
            .args(["-c", "sleep 60 & wait $!"])
            .spawn()
            .unwrap();
        let parent = shell.id() as libc::pid_t;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut children = Vec::new();
        while children.is_empty() {
            assert!(Instant::now() < deadline, "the shell never started sleep");
            std::thread::sleep(Duration::from_millis(10));
            for_each_child(parent, |child| children.push(child)).unwrap();
        }
        assert_eq!(
            signal_children(parent, libc::SIGTERM, |child| children.contains(&child)),
            Ok(0)
        );
        assert_eq!(signal_children(parent, libc::SIGTERM, |_| false), Ok(1));
        let status = shell.wait().unwrap();
        assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{status:?}");
    }

    /// The handler itself, in a child process of its own (this test binary,
    /// running this test alone), so that the dispositions it changes cannot
    /// reach the test harness, and no other test's thread holds a lock when
    /// it forks. With more inherited children than it can record, nothing is
    /// installed. Otherwise it takes SIGTERM and SIGHUP but leaves an ignored
    /// SIGINT ignored. The first signal is the one recorded, and each is
    /// passed on to a child started after the install but not to one the
    /// process already had (as a shell's job is after `exec hermit
    /// analyze`). A process forked afterwards exits 128 + SIGTERM when it
    /// arms the container-init guards, as a trial's container init does
    /// before it starts its guest. Dropped, it restores the default actions
    /// it replaced.
    #[test]
    fn the_handler_stops_what_analyze_started_and_nothing_else() {
        const CHILD: &str = "HERMIT_ANALYZE_STOP_HANDLER_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let name = format!(
                "{}::the_handler_stops_what_analyze_started_and_nothing_else",
                module_path!().split_once("::").unwrap().1
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "the child ran no test: {output:?}"
            );
            return;
        }
        let action = |signal| current_action(signal).unwrap().sa_sigaction;
        // SAFETY: SIG_IGN installs no handler.
        unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        let mut crowd: Vec<_> = (0..=INHERITED_CAPACITY)
            .map(|_| Command::new("sleep").arg("60").spawn().unwrap())
            .collect();
        let unarmed = StopSignals::install().unwrap();
        assert_eq!(unarmed.installed, [false; STOP_SIGNALS.len()]);
        assert_eq!(action(libc::SIGTERM), libc::SIG_DFL);
        assert_eq!(action(libc::SIGHUP), libc::SIG_DFL);
        drop(unarmed);
        for mut child in crowd.drain(..) {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        let mut before = Command::new("sleep").arg("60").spawn().unwrap();
        let guard = StopSignals::install().unwrap();
        assert_eq!(action(libc::SIGINT), libc::SIG_IGN);
        assert_eq!(action(libc::SIGTERM), guard.handler);
        assert_eq!(action(libc::SIGHUP), guard.handler);
        let mut after = Command::new("sleep").arg("60").spawn().unwrap();
        // SAFETY: raise(3) runs the handler on this thread before it returns.
        unsafe {
            libc::raise(libc::SIGTERM);
            libc::raise(libc::SIGHUP);
        }
        let status = after.wait().unwrap();
        let left_alone = before.try_wait().unwrap();
        let _ = before.kill();
        let _ = before.wait();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
        assert_eq!(left_alone, None, "the child the process had before");
        assert_eq!(check(), Err(Stopped(reverie::Signal::SIGTERM)));
        // SAFETY: the harness's other thread waits for this test and holds
        // no lock; the child calls only async-signal-safe functions.
        let forked = unsafe { libc::fork() };
        if forked == 0 {
            let _ = container::arm_container_init_guards();
            unsafe { libc::_exit(0) }
        }
        let mut raw = 0;
        // SAFETY: `raw` outlives the call; `forked` is this process's child.
        assert_eq!(unsafe { libc::waitpid(forked, &mut raw, 0) }, forked);
        assert!(
            libc::WIFEXITED(raw) && libc::WEXITSTATUS(raw) == 128 + libc::SIGTERM,
            "the process forked after the stop: status {raw:#x}"
        );
        drop(guard);
        assert_eq!(action(libc::SIGTERM), libc::SIG_DFL);
        assert_eq!(action(libc::SIGHUP), libc::SIG_DFL);
        assert_eq!(action(libc::SIGINT), libc::SIG_IGN);
    }

    /// How the processes of
    /// [`a_stop_signal_on_a_helper_thread_never_reaches_a_reused_pid`] end.
    mod ended {
        pub const AS_IT_SHOULD: libc::c_int = 0;
        pub const NO_USER_AND_PID_NAMESPACE: libc::c_int = 10;
        pub const NO_PROC_OF_ITS_OWN: libc::c_int = 11;
        pub const NO_FORK: libc::c_int = 12;
        pub const NO_PIPE: libc::c_int = 13;
        pub const NO_CHOSEN_PID: libc::c_int = 14;
        pub const NO_HANDLER: libc::c_int = 15;
        pub const NOT_IN_THE_WINDOW: libc::c_int = 16;
        pub const STOP_NOT_RECORDED: libc::c_int = 17;
        pub const THE_REUSED_PID_WAS_SIGNALLED: libc::c_int = 18;
        pub const THE_REUSED_PID_ENDED: libc::c_int = 19;
        pub const STOP_SWALLOWED: libc::c_int = 20;

        /// What an exit says; the process said why on stderr, with errno.
        pub fn describe(code: libc::c_int) -> &'static str {
            match code {
                NO_USER_AND_PID_NAMESPACE => {
                    "unshare(CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWPID) failed: this host \
                     refuses an unprivileged user, mount or PID namespace, which the container \
                     tests need too"
                }
                NO_PROC_OF_ITS_OWN => {
                    "could not mount a procfs of the new PID namespace in its user namespace, \
                     as container::tests::the_placeholder_directory_mounts_and_leaves_procfs_mountable \
                     does"
                }
                NO_FORK => "fork failed",
                NO_PIPE => "pipe failed",
                NO_CHOSEN_PID => {
                    "clone3 with set_tid failed: it needs Linux 5.5 or later and CAP_SYS_ADMIN \
                     over the new PID namespace's user namespace"
                }
                NO_HANDLER => "StopSignals::install did not install the SIGTERM handler",
                NOT_IN_THE_WINDOW => "the handler did not stop in the window at the child",
                STOP_NOT_RECORDED => "the handler did not record the stop",
                THE_REUSED_PID_WAS_SIGNALLED => {
                    "the process that took the reaped child's PID died of SIGTERM: the handler \
                     signalled a process analyze did not start"
                }
                THE_REUSED_PID_ENDED => "the process that took the reaped child's PID ended early",
                STOP_SWALLOWED => {
                    "the handler returned from a stop it could not pass on, and analyze went on"
                }
                _ => "unexpected exit",
            }
        }
    }

    /// The exit status `pid` ends with, or 100 plus the signal that ended it.
    fn exit_code(pid: libc::pid_t) -> libc::c_int {
        let mut raw = 0;
        // SAFETY: `raw` outlives the call.
        while unsafe { libc::waitpid(pid, &mut raw, 0) } != pid {
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return ended::NO_FORK;
            }
        }
        if libc::WIFEXITED(raw) {
            libc::WEXITSTATUS(raw)
        } else {
            100 + libc::WTERMSIG(raw)
        }
    }

    /// A pipe's read and write ends.
    fn pipe() -> Option<(libc::c_int, libc::c_int)> {
        let mut ends = [-1; 2];
        // SAFETY: pipe(2) writes two descriptors into `ends`.
        (unsafe { libc::pipe(ends.as_mut_ptr()) } == 0).then_some((ends[0], ends[1]))
    }

    /// Write `bytes` to `fd` whole.
    fn send(fd: libc::c_int, bytes: &[u8]) -> bool {
        // SAFETY: write(2) reads `bytes.len()` bytes of `bytes`.
        let done = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        done == isize::try_from(bytes.len()).unwrap_or(-1)
    }

    /// Read `bytes.len()` bytes from `fd`, in one read.
    fn receive(fd: libc::c_int, bytes: &mut [u8]) -> bool {
        // SAFETY: read(2) writes at most `bytes.len()` bytes into `bytes`.
        let done = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        done == isize::try_from(bytes.len()).unwrap_or(-1)
    }

    /// Where the handler, paused in the window, writes the child it found,
    /// and where it reads that it may go on.
    static FOUND: AtomicI32 = AtomicI32::new(-1);
    static GO_ON: AtomicI32 = AtomicI32::new(-1);
    static PAUSED: AtomicBool = AtomicBool::new(false);

    /// [`in_the_window`]'s hook in `as_analyze`: the first time, say which
    /// child the handler found and wait until the main thread lets it go on.
    /// Async-signal-safe: it runs in the handler.
    fn pause_in_the_window(child: libc::pid_t) {
        if PAUSED.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut go = [0u8];
        if send(FOUND.load(Ordering::SeqCst), &child.to_ne_bytes()) {
            receive(GO_ON.load(Ordering::SeqCst), &mut go);
        }
    }

    /// The process that holds the handler, PID 2 of its own namespace: it
    /// starts a child, has the handler run on a helper thread that blocks no
    /// signal, and while the handler is paused at that child reaps it, as
    /// the thread that drives the trials reaps a trial's container init,
    /// and has its parent give the PID to a process of its own.
    fn as_analyze(request: libc::c_int, granted: libc::c_int) -> libc::c_int {
        let Ok(guard) = StopSignals::install() else {
            return ended::NO_HANDLER;
        };
        if !guard.installed[0] {
            return ended::NO_HANDLER;
        }
        // SAFETY: this process has one thread; the child only pauses.
        let child = unsafe { libc::fork() };
        if child == 0 {
            loop {
                unsafe { libc::pause() };
            }
        }
        if child < 0 {
            return ended::NO_FORK;
        }
        let (Some((found, found_by)), Some((go_on_by, go_on))) = (pipe(), pipe()) else {
            return ended::NO_PIPE;
        };
        FOUND.store(found_by, Ordering::SeqCst);
        GO_ON.store(go_on_by, Ordering::SeqCst);
        let hook: fn(libc::pid_t) = pause_in_the_window;
        WINDOW.store(hook as usize, Ordering::SeqCst);
        let helper = std::thread::spawn(|| {
            // SAFETY: tgkill(2) to this very thread, which runs the handler
            // before the call returns.
            unsafe {
                libc::syscall(
                    libc::SYS_tgkill,
                    libc::getpid(),
                    libc::gettid(),
                    libc::SIGTERM,
                )
            }
        });
        let mut in_window = [0u8; 4];
        if !receive(found, &mut in_window) || libc::pid_t::from_ne_bytes(in_window) != child {
            return ended::NOT_IN_THE_WINDOW;
        }
        // SAFETY: kill(2) and waitpid(2) on this process's own child.
        unsafe { libc::kill(child, libc::SIGKILL) };
        if exit_code(child) != 100 + libc::SIGKILL {
            return ended::NO_FORK;
        }
        let mut given = [0u8];
        if !send(request, &child.to_ne_bytes()) || !receive(granted, &mut given) {
            return ended::NO_CHOSEN_PID;
        }
        send(go_on, &[1]);
        let _ = helper.join();
        WINDOW.store(0, Ordering::SeqCst);
        if check() != Err(Stopped(reverie::Signal::SIGTERM)) {
            return ended::STOP_NOT_RECORDED;
        }
        drop(guard);
        ended::AS_IT_SHOULD
    }

    /// A child of this process with the PID `pid` (clone3's `set_tid`) that
    /// takes stop signals at their default action, blocks none, says so on
    /// `ready` and then pauses.
    fn child_with_pid(pid: libc::pid_t, ready: libc::c_int) -> libc::pid_t {
        // struct clone_args through set_tid_size (CLONE_ARGS_SIZE_VER1).
        let set_tid: libc::pid_t = pid;
        let mut args = [0u64; 10];
        args[4] = libc::SIGCHLD as u64; // exit_signal
        args[8] = std::ptr::from_ref(&set_tid) as u64; // set_tid
        args[9] = 1; // set_tid_size
        // SAFETY: a fork-like clone3 without CLONE_VM from a process with one
        // thread; the child calls only async-signal-safe functions.
        let child = unsafe {
            libc::syscall(
                libc::SYS_clone3,
                args.as_ptr(),
                std::mem::size_of_val(&args),
            )
        };
        if child == 0 {
            // SAFETY: an all-zero sigset_t is valid for sigemptyset(3).
            let mut none: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sigemptyset(&mut none);
                libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
                for signal in STOP_SIGNALS {
                    libc::signal(signal, libc::SIG_DFL);
                }
            }
            send(ready, &[1]);
            loop {
                unsafe { libc::pause() };
            }
        }
        libc::pid_t::try_from(child).unwrap_or(-1)
    }

    /// PID 1 of the new namespace: a /proc of its own for the handler to
    /// read, the process that holds the handler, and on its request a
    /// process of its own with the PID it freed. Whether that process
    /// received SIGTERM decides the exit.
    fn as_namespace_init() -> libc::c_int {
        // SAFETY: mount(2) on NUL-terminated literals.
        let private = unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        };
        let proc = unsafe {
            libc::mount(
                c"proc".as_ptr(),
                c"/proc".as_ptr(),
                c"proc".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                std::ptr::null(),
            )
        };
        if private != 0 || proc != 0 {
            eprintln!("mount: {}", std::io::Error::last_os_error());
            return ended::NO_PROC_OF_ITS_OWN;
        }
        let (Some((request, request_by)), Some((granted_by, granted)), Some((ready, ready_by))) =
            (pipe(), pipe(), pipe())
        else {
            return ended::NO_PIPE;
        };
        // SAFETY: this process has one thread.
        let analyze = unsafe { libc::fork() };
        if analyze == 0 {
            // SAFETY: closes this copy's descriptors of the init's ends.
            unsafe {
                libc::close(request);
                libc::close(granted);
                libc::close(ready);
                libc::close(ready_by);
                libc::_exit(as_analyze(request_by, granted_by))
            }
        }
        // SAFETY: closes the init's copies of the ends it gave away, so that
        // a read sees the other end close.
        unsafe {
            libc::close(request_by);
            libc::close(granted_by);
        }
        if analyze < 0 {
            return ended::NO_FORK;
        }
        let mut freed = [0u8; 4];
        if !receive(request, &mut freed) {
            return exit_code(analyze);
        }
        let other = child_with_pid(libc::pid_t::from_ne_bytes(freed), ready_by);
        let mut up = [0u8];
        if other < 0 {
            eprintln!("clone3 set_tid: {}", std::io::Error::last_os_error());
        }
        if other < 0 || !receive(ready, &mut up) {
            // SAFETY: kill(2) on this process's own child.
            unsafe { libc::kill(analyze, libc::SIGKILL) };
            let _ = exit_code(analyze);
            return ended::NO_CHOSEN_PID;
        }
        send(granted, &[1]);
        let analyzed = exit_code(analyze);
        // A SIGTERM the handler sent at its default action already made the
        // process exit with it (`complete_signal`), and the kernel drops the
        // SIGKILL sent to a process that is exiting: so the status names the
        // signal that came first.
        // SAFETY: kill(2) on this process's own child.
        unsafe { libc::kill(other, libc::SIGKILL) };
        match exit_code(other) {
            code if code == 100 + libc::SIGTERM => ended::THE_REUSED_PID_WAS_SIGNALLED,
            code if code == 100 + libc::SIGKILL => analyzed,
            _ => ended::THE_REUSED_PID_ENDED,
        }
    }

    /// A stop signal the handler takes on a helper thread, while another
    /// thread reaps the child it found and the kernel gives the child's PID
    /// to a process analyze did not start, never reaches that process.
    ///
    /// Forced, not timed. In a user, mount and PID namespace of its own, so
    /// that a PID can be chosen (clone3's `set_tid`), the process holding the
    /// handler starts a child, and a helper thread that blocks no signal
    /// sends itself SIGTERM. The handler pauses after finding the child
    /// ([`in_the_window`]); the main thread reaps the child; the namespace's
    /// init, which is not analyze, gives the child's PID to a process of its
    /// own; then the handler goes on. Signalled by number, as before, that
    /// process died of SIGTERM.
    ///
    /// It needs what the container tests in both `test.hermit_unit` cells
    /// already need (unprivileged user, mount and PID namespaces, and a
    /// procfs mounted in them), and clone3's `set_tid` (Linux 5.5). A host
    /// without one fails it with the capability named, never a pass.
    #[test]
    fn a_stop_signal_on_a_helper_thread_never_reaches_a_reused_pid() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        // SAFETY: the copy's other thread waits for this test and holds no
        // lock.
        let outer = unsafe { libc::fork() };
        if outer == 0 {
            // SAFETY: a forked child of one thread, as a new user namespace
            // requires.
            let unshared = unsafe {
                libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWPID)
            };
            let code = if unshared != 0 {
                eprintln!("unshare: {}", std::io::Error::last_os_error());
                ended::NO_USER_AND_PID_NAMESPACE
            } else {
                match unsafe { libc::fork() } {
                    0 => unsafe { libc::_exit(as_namespace_init()) },
                    init if init > 0 => exit_code(init),
                    _ => ended::NO_FORK,
                }
            };
            unsafe { libc::_exit(code) }
        }
        assert!(outer > 0, "fork: {}", std::io::Error::last_os_error());
        let code = exit_code(outer);
        assert_eq!(code, ended::AS_IT_SHOULD, "{}", ended::describe(code));
    }
    /// The process holding the handler in [`stop_without_descriptors`]. It
    /// starts a child that dies with it (its parent-death signal, as a
    /// trial's container init has) and waits until the child has armed that
    /// signal while this process is still its parent, tells the child's PID
    /// on `tell`, with `full_stderr` fills its stderr pipe so that a write
    /// there blocks, uses up every descriptor it may open, and sends itself
    /// SIGTERM. Exiting is the regression: the stop was swallowed.
    fn as_analyze_without_descriptors(
        says: libc::c_int,
        tell: libc::c_int,
        full_stderr: bool,
    ) -> libc::c_int {
        // SAFETY: dup2(2) and close(2) on descriptors this process owns.
        unsafe {
            libc::dup2(says, 2);
            libc::close(says);
        }
        let Ok(guard) = StopSignals::install() else {
            return ended::NO_HANDLER;
        };
        if !guard.installed[0] {
            return ended::NO_HANDLER;
        }
        let Some((armed, arms)) = pipe() else {
            return ended::NO_FORK;
        };
        // SAFETY: getpid(2) has no preconditions; this process has one
        // thread, and the child only arms its signal, reports and pauses.
        let parent = unsafe { libc::getpid() };
        let child = unsafe { libc::fork() };
        if child == 0 {
            unsafe {
                libc::close(2);
                libc::close(tell);
                libc::close(armed);
                let ready = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == 0
                    && libc::getppid() == parent;
                if !ready || !send(arms, b"A") {
                    libc::_exit(1);
                }
                loop {
                    libc::pause();
                }
            }
        }
        // SAFETY: closes this process's copy of the child's end.
        unsafe { libc::close(arms) };
        let mut ready = [0u8; 1];
        if child < 0 || !receive(armed, &mut ready) || !send(tell, &child.to_ne_bytes()) {
            return ended::NO_FORK;
        }
        // SAFETY: fcntl(2) and write(2) on this process's stderr, a pipe
        // whose reader is alive; setrlimit(2) reads `limit`; open(2) on a
        // NUL-terminated literal; raise(3) runs the handler on this thread.
        unsafe {
            libc::close(armed);
            libc::close(tell);
            if full_stderr {
                let flags = libc::fcntl(2, libc::F_GETFL);
                libc::fcntl(2, libc::F_SETFL, flags | libc::O_NONBLOCK);
                let chunk = [b'x'; 4096];
                while libc::write(2, chunk.as_ptr().cast(), chunk.len()) > 0 {}
                libc::fcntl(2, libc::F_SETFL, flags);
            }
            let mut limit: libc::rlimit = std::mem::zeroed();
            libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit);
            limit.rlim_cur = limit.rlim_cur.min(64);
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
            while libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) >= 0 {}
            libc::raise(libc::SIGTERM);
        }
        std::mem::forget(guard);
        ended::STOP_SWALLOWED
    }

    /// `pid`'s wait status once it ends, if it does within `bound`; else it
    /// is killed through a pidfd (it is this process's child, not yet
    /// reaped, so the pidfd names it) and reaped, and `None` returned.
    fn wait_within(pid: libc::pid_t, bound: Duration) -> Option<libc::c_int> {
        let deadline = std::time::Instant::now() + bound;
        let mut raw = 0;
        loop {
            // SAFETY: `raw` outlives the call; `pid` is this process's child.
            let waited = unsafe { libc::waitpid(pid, &mut raw, libc::WNOHANG) };
            if waited == pid {
                return Some(raw);
            }
            if waited < 0 || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: pidfd_open(2) and pidfd_send_signal(2) without siginfo;
        // close(2) of the pidfd opened here; waitpid(2) reaps the child.
        unsafe {
            let pidfd = libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int;
            if pidfd >= 0 {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd,
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
                libc::close(pidfd);
            }
            libc::waitpid(pid, &mut raw, 0);
        }
        None
    }

    /// Run [`as_analyze_without_descriptors`] in a process of its own and
    /// say how it ended (its wait status, `None` if it had to be killed after
    /// the bound), what it wrote on stderr (read once it and its child have
    /// ended, so a full pipe stays full until then), and how its child ended.
    fn stop_without_descriptors(
        full_stderr: bool,
    ) -> (Option<libc::c_int>, String, Option<libc::c_int>) {
        // SAFETY: prctl(2) with integer arguments: the child, orphaned when
        // its parent dies, comes to this process to be reaped.
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        let (Some((said, says)), Some((told, tell))) = (pipe(), pipe()) else {
            panic!("pipe: {}", std::io::Error::last_os_error());
        };
        // SAFETY: the copy's other thread waits for this test and holds no
        // lock.
        let analyze = unsafe { libc::fork() };
        if analyze == 0 {
            unsafe {
                libc::close(said);
                libc::close(told);
                libc::_exit(as_analyze_without_descriptors(says, tell, full_stderr))
            }
        }
        assert!(analyze > 0, "fork: {}", std::io::Error::last_os_error());
        // SAFETY: closes this process's copies of the ends the child uses.
        unsafe {
            libc::close(says);
            libc::close(tell);
        }
        let ended = wait_within(analyze, Duration::from_secs(20));
        let mut child = [0u8; 4];
        let child = receive(told, &mut child)
            .then(|| libc::pid_t::from_ne_bytes(child))
            .and_then(|child| wait_within(child, Duration::from_secs(20)));
        let mut stderr = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            // SAFETY: read(2) writes at most `chunk.len()` bytes into it.
            let got = unsafe { libc::read(said, chunk.as_mut_ptr().cast(), chunk.len()) };
            let Some(got) = usize::try_from(got).ok().filter(|&got| got > 0) else {
                break;
            };
            stderr.extend_from_slice(&chunk[..got]);
        }
        // SAFETY: closes the ends this test opened.
        unsafe {
            libc::close(said);
            libc::close(told);
        }
        let stderr = String::from_utf8_lossy(&stderr);
        (ended, stderr.trim_start_matches('x').to_owned(), child)
    }

    /// Whether `ended` is a death by `signal`, and if not, what it was.
    fn died_of(ended: Option<libc::c_int>, signal: libc::c_int) -> Result<(), String> {
        match ended {
            Some(raw) if libc::WIFSIGNALED(raw) && libc::WTERMSIG(raw) == signal => Ok(()),
            Some(raw) if libc::WIFEXITED(raw) => Err(format!(
                "exited {} ({})",
                libc::WEXITSTATUS(raw),
                ended::describe(libc::WEXITSTATUS(raw))
            )),
            Some(raw) => Err(format!("status {raw:#x}")),
            None => Err("still alive after the bound, killed".to_owned()),
        }
    }

    /// A stop the handler cannot pass on is never swallowed. With no
    /// descriptor left, the scan cannot read a `stat` file and so cannot
    /// tell which process is the child it must signal. Before, the handler
    /// then found no child and returned: the run went on, and so did
    /// analyze. Now analyze says on stderr which call failed and dies of the
    /// signal by its default action, as without the handler, and the child
    /// dies with it by its parent-death signal.
    #[test]
    fn a_stop_the_handler_cannot_pass_on_ends_analyze_by_its_default_action() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        let (ended, stderr, child) = stop_without_descriptors(false);
        if let Err(how) = died_of(ended, libc::SIGTERM) {
            panic!("analyze should die of SIGTERM, but {how}; stderr: {stderr}");
        }
        assert!(
            stderr.contains(
                "hermit analyze: could not pass signal 15 on to the run it started (open \
                 /proc/<pid>/stat failed with errno 24)"
            ),
            "{stderr}"
        );
        if let Err(how) = died_of(child, libc::SIGKILL) {
            panic!("the child should die with analyze, but {how}");
        }
    }

    /// Death does not wait on the diagnostic. With stderr a full pipe whose
    /// reader is alive, the diagnostic's write blocks; before, the handler
    /// blocked in it with the stop signals masked, and analyze never died.
    /// Now the signal ends it within the bound, and the child with it.
    #[test]
    fn a_stop_ends_analyze_even_when_its_stderr_is_full() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        let (ended, stderr, child) = stop_without_descriptors(true);
        if let Err(how) = died_of(ended, libc::SIGTERM) {
            panic!("analyze should die of SIGTERM, but {how}; stderr: {stderr}");
        }
        if let Err(how) = died_of(child, libc::SIGKILL) {
            panic!("the child should die with analyze, but {how}");
        }
    }
}
