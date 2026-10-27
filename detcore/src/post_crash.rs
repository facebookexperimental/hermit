/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The post-crash timeslice switch (`Config::post_crash_timeslice`).
//!
//! A race that needs fine slices (`--max-timeslice` of 1000 to 10000) often
//! reaches its crash in seconds and then spends minutes in the crash path:
//! Android's debuggerd handler starts `crash_dump`, which ptrace-attaches every
//! thread of the crashed process, reads its registers and memory and unwinds
//! it, and every branch of that is sliced. Once a guest process has crashed,
//! the switch raises the slice bound of that process and of every process it
//! goes on to create to the configured post-crash bound, for the rest of the
//! run. Other processes (a harness that runs trials as children, a shell)
//! keep their bound.
//!
//! A crash is a fatal signal on its way to ending a process
//! ([`is_crash_signal`]). Every step below happens at a point the schedule
//! orders, so it is taken at the same point of every run and replay that
//! reaches it:
//! - a crash signal's delivery that takes the default action switches at
//!   once, for the crashed process;
//! - one the guest catches arms the switch for that thread. The processes
//!   that thread's process creates while it is armed (debuggerd's
//!   pseudothread, and `crash_dump` before its exec) belong to its crash;
//! - a successful `execve` by a thread of such a process takes the switch for
//!   an armed crash of its own lineage whose handler is still running:
//!   Android's crash handler starts `crash_dump` that way, from inside the
//!   handler. Crashes in unrelated processes keep separate lineages, and each
//!   crash in a process not yet switched takes a switch of its own. A switch
//!   applies to its whole process: every crash armed in it switches with it,
//!   and those crashes' lineages take the bound too;
//! - an armed thread's handler has returned once the kernel no longer blocks
//!   its crash signal for it ([`signal_blocked`]). That is read only where the
//!   armed thread itself is stopped at a point of its own: each syscall entry,
//!   before the call runs; each syscall exit or trap, after it completes and
//!   before the thread can give up its turn there; each end of one of its
//!   slices; each later signal delivery to it, before the kernel installs that
//!   handler's mask. A thread gives up its turn only at such a point, and with
//!   threads serialized no other thread runs in between, so the state known
//!   at another thread's `execve` is the armed thread's own. A handler that resumes the faulting
//!   code (ART's and JVMs' implicit null checks) is disarmed at its thread's
//!   next stop, and never switches.
//!
//! The scheduler keeps the [`PostCrashState`]; a thread of a crashed process
//! learns the bound from the answer to its own step and from every later reply
//! that resumes it from a grant (`GlobalResponse::WithPostCrashBound`), so it
//! uses the bound from its first slice after the switch.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::num::NonZeroU64;

use detcore_model::pid::DetPid;
use detcore_model::pid::DetTid;
use nix::sys::signal::Signal;
use reverie::Guest;
use serde::Deserialize;
use serde::Serialize;

use crate::Detcore;
use crate::RecordOrReplay;
use crate::tool_global;

/// One step of the switch, sent by a guest thread
/// (`GlobalRequest::PostCrash`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PostCrashEvent {
    /// A crash signal is being delivered to the sender.
    Delivered {
        /// The signal's number.
        signal: i32,
        /// Whether the delivery takes the default action (no handler).
        default_action: bool,
    },
    /// The sender's crash handler has returned: at a stop of its own, the
    /// kernel no longer blocks its armed crash signal.
    HandlerReturned,
    /// The sender's `execve` succeeded.
    Exec,
}

/// Why the switch was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PostCrashCause {
    /// The crash signal's delivery took the default action.
    DefaultAction,
    /// This thread's `execve` succeeded while the crash signal's handler ran.
    Exec(DetTid),
}

/// Where the switch was taken: recorded in a preemption record and reported
/// in the run summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PostCrashSwitch {
    /// The thread the crash signal was delivered to.
    pub dtid: DetTid,
    /// The crash signal's number.
    pub signal: i32,
    /// The thread whose `execve` took the switch, or `None` when the
    /// delivery took the default action.
    pub exec_by: Option<DetTid>,
    /// The last committed scheduler turn when the switch was taken.
    pub turn: u64,
    /// The bound from then on, in virtual nanoseconds.
    pub bound: u64,
}

impl PostCrashSwitch {
    /// Why it was taken.
    pub fn cause(&self) -> PostCrashCause {
        match self.exec_by {
            Some(dtid) => PostCrashCause::Exec(dtid),
            None => PostCrashCause::DefaultAction,
        }
    }
}

/// A thread whose caught crash signal armed the switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Armed {
    /// The crash signal.
    signal: i32,
    /// The armed thread's process.
    process: DetPid,
}

/// The scheduler's state of the switch.
#[derive(Debug, Clone, Default)]
pub struct PostCrashState {
    /// The post-crash bound in effect for this run, or `None` when the switch
    /// is off (`Config::post_crash_timeslice_in_effect`).
    bound: Option<NonZeroU64>,
    /// Each thread a caught crash signal armed, until its handler returned,
    /// it exited or its crash took the switch. Ordered, so a switch names the
    /// same thread in every run.
    armed: BTreeMap<DetTid, Armed>,
    /// For each process, the armed crashes whose lineage it is in: the
    /// armed thread's own process and the processes created from the lineage
    /// while that thread is armed. An `execve` in a process takes the switch
    /// for one of its own armed crashes only.
    lineage: BTreeMap<DetPid, BTreeSet<DetTid>>,
    /// The switches taken, in order.
    switches: Vec<PostCrashSwitch>,
    /// The processes whose threads take the bound: each switched crash's
    /// lineage, and every process any of them creates after.
    crashed: BTreeSet<DetPid>,
    /// Under a replay, the switches its record says were taken, still to
    /// come: each switch this run takes must be the next of them.
    expected: Option<std::collections::VecDeque<PostCrashSwitch>>,
}

impl PostCrashState {
    /// The state for a run whose post-crash bound in effect is `bound`; a
    /// replay passes the switches its record holds as `expected`.
    pub fn new(bound: Option<NonZeroU64>, expected: Option<Vec<PostCrashSwitch>>) -> Self {
        Self {
            bound,
            expected: expected.map(Into::into),
            ..Self::default()
        }
    }

    /// Apply `event` from `dtid`, a thread of `process`, during committed
    /// turn `turn`. Returns the bound if `process` takes it (now or since an
    /// earlier switch), and the switch if this step took one; or, under a
    /// replay, why the switch this step took is not the one its record holds.
    pub fn step(
        &mut self,
        dtid: DetTid,
        process: DetPid,
        event: PostCrashEvent,
        turn: u64,
    ) -> Result<(Option<NonZeroU64>, Option<PostCrashSwitch>), String> {
        let Some(bound) = self.bound else {
            return Ok((None, None));
        };
        if self.crashed.contains(&process) {
            if event == PostCrashEvent::HandlerReturned {
                self.disarm(dtid);
            }
            return Ok((self.bound_for(process), None));
        }
        let switch = match event {
            PostCrashEvent::Delivered {
                signal,
                default_action: true,
            } => {
                self.switch_process(process);
                Some((dtid, signal, None))
            }
            PostCrashEvent::Delivered {
                signal,
                default_action: false,
            } => {
                // A crash inside an armed thread's handler keeps the first
                // arm: the outer handler, whose signal stays blocked until it
                // returns, is the one still running.
                self.armed.entry(dtid).or_insert(Armed { signal, process });
                self.lineage.entry(process).or_default().insert(dtid);
                None
            }
            PostCrashEvent::HandlerReturned => {
                self.disarm(dtid);
                None
            }
            PostCrashEvent::Exec => {
                let crash = self
                    .lineage
                    .get(&process)
                    .and_then(|crashes| crashes.iter().next().copied());
                crash.map(|crashed| {
                    let Armed { signal, process } = self.armed[&crashed];
                    self.switch_process(process);
                    (crashed, signal, Some(dtid))
                })
            }
        };
        let Some((crashed, signal, exec_by)) = switch else {
            return Ok((None, None));
        };
        let taken = PostCrashSwitch {
            dtid: crashed,
            signal,
            exec_by,
            turn,
            bound: bound.get(),
        };
        if let Some(expected) = &mut self.expected {
            match expected.pop_front() {
                Some(recorded) if recorded == taken => {}
                recorded => {
                    return Err(format!(
                        "the replay took the post-crash switch {taken:?}, but its record holds {}; \
                         the replay has left its record",
                        recorded.map_or("no further switch".to_owned(), |recorded| format!(
                            "{recorded:?}"
                        )),
                    ));
                }
            }
        }
        self.switches.push(taken);
        Ok((self.bound_for(process), Some(taken)))
    }

    /// `child`, a new process, was created by a thread of `creator`: it
    /// belongs to its creator's crashes, armed or switched.
    pub fn process_created(&mut self, creator: DetPid, child: DetPid) {
        if self.crashed.contains(&creator) {
            self.crashed.insert(child);
        }
        if let Some(crashes) = self.lineage.get(&creator).cloned() {
            self.lineage.entry(child).or_default().extend(crashes);
        }
    }

    /// Makes `process` a crashed one, and with it every crash armed in it:
    /// each such crash's lineage takes the bound too, and the crash retires.
    /// So no armed crash outlives its process taking the bound, where its
    /// handler's return could no longer be followed (the process's threads
    /// stop reading their masks once they hold the bound).
    fn switch_process(&mut self, process: DetPid) {
        self.crashed.insert(process);
        let in_process: Vec<DetTid> = self
            .armed
            .iter()
            .filter(|(_, armed)| armed.process == process)
            .map(|(&dtid, _)| dtid)
            .collect();
        for crash in in_process {
            let members: Vec<DetPid> = self
                .lineage
                .iter()
                .filter(|(_, crashes)| crashes.contains(&crash))
                .map(|(&member, _)| member)
                .collect();
            self.crashed.extend(members);
            self.disarm(crash);
        }
    }

    /// `dtid` exited: it cannot be in a handler any more.
    pub fn forget(&mut self, dtid: DetTid) {
        self.disarm(dtid);
    }

    /// Drops `dtid`'s armed crash and its lineage, leaving every other
    /// crash's.
    fn disarm(&mut self, dtid: DetTid) {
        if self.armed.remove(&dtid).is_some() {
            self.lineage.retain(|_, crashes| {
                crashes.remove(&dtid);
                !crashes.is_empty()
            });
        }
    }

    /// The bound `process`'s threads take, once a switch has made it a
    /// crashed process.
    pub fn bound_for(&self, process: DetPid) -> Option<NonZeroU64> {
        self.bound.filter(|_| self.crashed.contains(&process))
    }

    /// The switches taken so far, in order.
    pub fn switches(&self) -> &[PostCrashSwitch] {
        &self.switches
    }

    /// The run summary's account of the switch, or `None` when it is off.
    pub fn summary(&self) -> Option<detcore_model::summary::PostCrashSummary> {
        let bound = self.bound?;
        Some(detcore_model::summary::PostCrashSummary {
            bound: bound.get(),
            switches: self
                .switches
                .iter()
                .map(|switch| detcore_model::summary::PostCrashSwitchSummary {
                    dtid: switch.dtid.as_raw(),
                    signal: switch.signal,
                    exec_by: switch.exec_by.map(|exec| exec.as_raw()),
                    turn: switch.turn,
                })
                .collect(),
            recorded_not_taken: self.expected.as_ref().map_or(0, |expected| expected.len()),
        })
    }
}

/// Whether `signal`, delivered with `si_code` (`None` when it could not be
/// read), is a crash: SIGABRT however it was sent (`abort` sends it with
/// `tgkill`), or SIGSEGV, SIGBUS, SIGILL, SIGFPE or SIGSYS raised by the
/// kernel (`si_code > 0`; a `kill`, `tgkill` or `sigqueue` sends them with
/// `si_code <= 0`, and a fault is the only kernel source of these).
pub fn is_crash_signal(signal: i32, si_code: Option<i32>) -> bool {
    match Signal::try_from(signal) {
        Ok(Signal::SIGABRT) => true,
        Ok(Signal::SIGSEGV | Signal::SIGBUS | Signal::SIGILL | Signal::SIGFPE | Signal::SIGSYS) => {
            si_code.is_some_and(|code| code > 0)
        }
        _ => false,
    }
}

/// `si_code` of the signal a thread is stopped for: the third `int` of its
/// `siginfo_t` on every architecture Hermit runs on.
fn si_code<G, T>(guest: &G) -> Option<i32>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let info = guest.signal_info()?;
    Some(i32::from_ne_bytes(info.get(8..12)?.try_into().ok()?))
}

/// Whether the kernel's signal masks of guest threads are the guests' own and
/// readable at a thread's own stops: the backend's guest threads are host
/// tasks whose masks no runtime of its own changes
/// (`kernel_signal_state_is_guests`: ptrace), threads are serialized (no other
/// thread runs between an armed thread's stops and another thread's
/// `execve`), and `/proc` reads are not refused. Elsewhere a caught crash does
/// not arm the switch.
fn reads_handler_masks(config: &crate::Config) -> bool {
    config.sequentialize_threads
        && config.backend.kernel_signal_state_is_guests
        && !crate::syscalls::host_signal_state_reads_refused()
}

/// Whether the kernel blocks `signal` for the stopped thread `guest`. A crash
/// handler runs with its signal blocked: Linux adds a caught signal to the
/// mask while its handler runs (unless the handler was installed with
/// `SA_NODEFER`, which a crash handler is not: such a handler counts as
/// returned), and the handler's `rt_sigreturn`, or a `siglongjmp` that
/// restores the mask, takes it out again; a blocked fault signal is never
/// delivered to a handler. A thread that cannot be read counts as returned.
fn signal_blocked<G, T>(guest: &G, signal: i32) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let bit = crate::syscalls::kernel_sigset_bit(signal);
    crate::syscalls::read_thread_signal_state(guest.pid(), guest.tid())
        .is_ok_and(|state| state.blocked & bit != 0)
}

/// Records what the scheduler answered a step: the bound, once the thread's
/// process takes it.
fn learn(state: &mut crate::tool_local::ThreadState<impl Sized>, bound: Option<NonZeroU64>) {
    state.post_crash_bound = state.post_crash_bound.or(bound);
    if state.post_crash_bound.is_some() {
        state.crash_armed = None;
    }
}

/// At a stop of the thread's own (a syscall entry before the call runs, a
/// syscall exit or trap after it completes, the end of one of its slices, a
/// signal delivery before the kernel installs the handler's mask): if the
/// thread is armed and its crash handler has returned, the step that disarms
/// it.
pub(crate) async fn note_stop<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some(signal) = guest.thread_state().crash_armed else {
        return;
    };
    if guest.thread_state().post_crash_bound.is_some() || signal_blocked(guest, signal) {
        return;
    }
    guest.thread_state_mut().crash_armed = None;
    let bound = tool_global::post_crash_step(guest, PostCrashEvent::HandlerReturned).await;
    learn(guest.thread_state_mut(), bound);
}

/// When `signal` is about to be delivered to the thread, before Detcore's copy
/// of the dispositions or the kernel applies the delivery (so a one-shot
/// `SA_RESETHAND` handler still counts as caught): a stop for an armed thread
/// ([`note_stop`]), then a crash signal's step (`PostCrashEvent::Delivered`).
pub(crate) async fn note_delivery<G, T>(guest: &mut G, signal: i32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.config().post_crash_timeslice_in_effect().is_none()
        || guest.thread_state().post_crash_bound.is_some()
    {
        return;
    }
    note_stop(guest).await;
    if !guest.thread_state().guest_past_first_execve() || !is_crash_signal(signal, si_code(guest)) {
        return;
    }
    let default_action = crate::delivery_takes_default_action(guest, signal);
    // A crash caught inside an armed thread's handler keeps the first arm;
    // one that takes the default action switches whatever the thread was
    // doing.
    if !default_action
        && (guest.thread_state().crash_armed.is_some() || !reads_handler_masks(guest.config()))
    {
        return;
    }
    let bound = tool_global::post_crash_step(
        guest,
        PostCrashEvent::Delivered {
            signal,
            default_action,
        },
    )
    .await;
    let state = guest.thread_state_mut();
    if !default_action && bound.is_none() {
        state.crash_armed = Some(signal);
    }
    learn(state, bound);
}

/// After the thread's `execve` succeeded: the step that takes an armed
/// switch.
pub(crate) async fn note_exec<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.config().post_crash_timeslice_in_effect().is_none()
        || guest.thread_state().post_crash_bound.is_some()
    {
        return;
    }
    let bound = tool_global::post_crash_step(guest, PostCrashEvent::Exec).await;
    learn(guest.thread_state_mut(), bound);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(raw: i32) -> DetTid {
        DetTid::from_raw(raw)
    }

    fn pid(raw: i32) -> DetPid {
        DetPid::from_raw(raw)
    }

    const ABRT: i32 = libc::SIGABRT;
    const SEGV: i32 = libc::SIGSEGV;
    const BOUND: u64 = 200_000_000;

    fn delivered(signal: i32, default_action: bool) -> PostCrashEvent {
        PostCrashEvent::Delivered {
            signal,
            default_action,
        }
    }

    fn on() -> PostCrashState {
        PostCrashState::new(NonZeroU64::new(BOUND), None)
    }

    fn step(
        state: &mut PostCrashState,
        dtid: i32,
        process: i32,
        event: PostCrashEvent,
        turn: u64,
    ) -> (Option<NonZeroU64>, Option<PostCrashSwitch>) {
        state.step(tid(dtid), pid(process), event, turn).unwrap()
    }

    /// Only SIGABRT counts whoever sent it; the fault signals count only when
    /// the kernel raised them; nothing else counts.
    #[test]
    fn crash_signals_are_aborts_and_kernel_faults() {
        assert!(is_crash_signal(ABRT, Some(-6)));
        assert!(is_crash_signal(ABRT, None));
        for signal in [SEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE, libc::SIGSYS] {
            assert!(is_crash_signal(signal, Some(1)), "{signal}");
            assert!(is_crash_signal(signal, Some(0x80)), "{signal}");
            assert!(!is_crash_signal(signal, Some(0)), "{signal} from kill");
            assert!(!is_crash_signal(signal, Some(-6)), "{signal} from tgkill");
            assert!(!is_crash_signal(signal, None), "{signal} unread");
        }
        for signal in [libc::SIGQUIT, libc::SIGTRAP, libc::SIGTERM, libc::SIGKILL] {
            assert!(!is_crash_signal(signal, Some(1)), "{signal}");
        }
    }

    /// With the switch off, nothing is ever taken.
    #[test]
    fn an_off_switch_is_never_taken() {
        let mut state = PostCrashState::new(None, None);
        assert_eq!(
            step(&mut state, 3, 3, delivered(ABRT, true), 7),
            (None, None)
        );
        assert_eq!(state.bound_for(pid(3)), None);
        assert_eq!(state.summary(), None);
    }

    /// A default-action delivery switches at once for the crashed process
    /// only, naming its thread and turn; its later children take the bound,
    /// other processes do not.
    #[test]
    fn a_default_action_crash_switches_its_process_at_once() {
        let mut state = on();
        let (answer, taken) = step(&mut state, 4, 3, delivered(SEGV, true), 7);
        assert_eq!(answer, NonZeroU64::new(BOUND));
        let taken = taken.unwrap();
        assert_eq!(
            (taken.dtid, taken.signal, taken.cause(), taken.turn),
            (tid(4), SEGV, PostCrashCause::DefaultAction, 7)
        );
        assert_eq!(state.bound_for(pid(3)), NonZeroU64::new(BOUND));
        assert_eq!(state.bound_for(pid(1)), None, "the harness keeps its bound");
        assert_eq!(
            step(&mut state, 1, 1, PostCrashEvent::Exec, 9),
            (None, None)
        );
        state.process_created(pid(3), pid(20));
        state.process_created(pid(1), pid(21));
        assert_eq!(state.bound_for(pid(20)), NonZeroU64::new(BOUND));
        assert_eq!(state.bound_for(pid(21)), None);
    }

    /// A caught crash arms; an `execve` in a process the armed process
    /// created while armed takes the switch for that lineage, naming the
    /// crashed thread and the exec'ing one.
    #[test]
    fn a_caught_crash_switches_at_its_lineages_exec() {
        let mut state = on();
        assert_eq!(
            step(&mut state, 5, 3, delivered(ABRT, false), 10),
            (None, None)
        );
        // debuggerd's pseudothread, then crash_dump forked from it.
        state.process_created(pid(3), pid(30));
        state.process_created(pid(30), pid(31));
        let (answer, taken) = step(&mut state, 31, 31, PostCrashEvent::Exec, 12);
        assert_eq!(answer, NonZeroU64::new(BOUND));
        let taken = taken.unwrap();
        assert_eq!(
            (taken.dtid, taken.signal, taken.cause(), taken.turn),
            (tid(5), ABRT, PostCrashCause::Exec(tid(31)), 12)
        );
        for process in [3, 30, 31] {
            assert_eq!(state.bound_for(pid(process)), NonZeroU64::new(BOUND));
        }
    }

    /// An `execve` outside the armed lineage (a sibling trial, the harness)
    /// takes nothing.
    #[test]
    fn an_exec_outside_the_lineage_takes_nothing() {
        let mut state = on();
        step(&mut state, 5, 3, delivered(ABRT, false), 10);
        state.process_created(pid(1), pid(40));
        assert_eq!(
            step(&mut state, 40, 40, PostCrashEvent::Exec, 12),
            (None, None)
        );
        assert!(state.switches().is_empty());
    }

    /// A handler return seen at the thread's own stop disarms it, and its
    /// lineage with it, so a later `execve` takes nothing.
    #[test]
    fn a_handler_that_returns_disarms() {
        let mut state = on();
        step(&mut state, 5, 3, delivered(SEGV, false), 10);
        step(&mut state, 5, 3, PostCrashEvent::HandlerReturned, 11);
        state.process_created(pid(3), pid(30));
        assert_eq!(
            step(&mut state, 30, 30, PostCrashEvent::Exec, 12),
            (None, None)
        );
        assert_eq!(
            step(&mut state, 5, 3, PostCrashEvent::Exec, 13),
            (None, None)
        );
    }

    /// Two unrelated processes arm; one recovers. An `execve` in the
    /// recovered one's former lineage takes nothing, while the other crash
    /// still switches at its own lineage's `execve`.
    #[test]
    fn a_recovered_lineage_does_not_take_another_crashs_switch() {
        let mut state = on();
        step(&mut state, 5, 3, delivered(SEGV, false), 10);
        step(&mut state, 7, 6, delivered(ABRT, false), 11);
        state.process_created(pid(3), pid(30));
        step(&mut state, 5, 3, PostCrashEvent::HandlerReturned, 12);
        state.process_created(pid(3), pid(32));
        for (thread, process) in [(30, 30), (32, 32), (5, 3)] {
            assert_eq!(
                step(&mut state, thread, process, PostCrashEvent::Exec, 13),
                (None, None)
            );
        }
        state.process_created(pid(6), pid(60));
        let (_, taken) = step(&mut state, 60, 60, PostCrashEvent::Exec, 14);
        assert_eq!(taken.unwrap().dtid, tid(7));
        assert_eq!(state.bound_for(pid(6)), NonZeroU64::new(BOUND));
        for process in [3, 30, 32] {
            assert_eq!(state.bound_for(pid(process)), None, "{process}");
        }
    }

    /// Two threads of one process armed, with a child created while only
    /// the higher one was armed: when the lower one's crash switches at a
    /// later child's exec, the whole process switches, so the higher one's
    /// crash switches with it and its earlier child takes the bound too. Its
    /// handler's later return leaves nothing behind that could switch again.
    #[test]
    fn a_switch_takes_every_crash_armed_in_its_process() {
        let mut state = on();
        step(&mut state, 8, 3, delivered(SEGV, false), 10);
        state.process_created(pid(3), pid(30));
        step(&mut state, 5, 3, delivered(ABRT, false), 11);
        state.process_created(pid(3), pid(31));
        let (_, taken) = step(&mut state, 31, 31, PostCrashEvent::Exec, 12);
        assert_eq!(taken.unwrap().dtid, tid(5));
        for process in [3, 30, 31] {
            assert_eq!(
                state.bound_for(pid(process)),
                NonZeroU64::new(BOUND),
                "{process}"
            );
        }
        step(&mut state, 8, 3, PostCrashEvent::HandlerReturned, 13);
        assert_eq!(
            step(&mut state, 30, 30, PostCrashEvent::Exec, 14),
            (NonZeroU64::new(BOUND), None),
            "no second switch for the returned handler"
        );
        assert_eq!(state.switches().len(), 1);
    }

    /// Crashes in unrelated processes, one after another, each take a switch
    /// of their own; their parent keeps its bound.
    #[test]
    fn each_crashing_trial_takes_its_own_switch() {
        let mut state = on();
        state.process_created(pid(1), pid(10));
        let (_, first) = step(&mut state, 10, 10, delivered(ABRT, true), 5);
        state.process_created(pid(1), pid(11));
        assert_eq!(state.bound_for(pid(11)), None);
        let (_, second) = step(&mut state, 11, 11, delivered(SEGV, true), 9);
        assert_eq!(
            state.switches(),
            &[first.unwrap(), second.unwrap()],
            "two switches, in order"
        );
        assert_eq!(state.bound_for(pid(1)), None);
        assert_eq!(state.summary().unwrap().switches.len(), 2);
    }

    /// A crash inside an armed thread's handler that takes the default action
    /// (an `abort` from a SIGSEGV handler) switches at once, naming its own
    /// signal, and retires the outer arm.
    #[test]
    fn a_nested_default_action_crash_switches() {
        let mut state = on();
        step(&mut state, 5, 3, delivered(SEGV, false), 10);
        let (answer, taken) = step(&mut state, 5, 3, delivered(ABRT, true), 11);
        assert_eq!(answer, NonZeroU64::new(BOUND));
        let taken = taken.unwrap();
        assert_eq!(
            (taken.dtid, taken.signal, taken.cause()),
            (tid(5), ABRT, PostCrashCause::DefaultAction)
        );
        state.process_created(pid(3), pid(30));
        assert_eq!(
            step(&mut state, 30, 30, PostCrashEvent::Exec, 12).1,
            None,
            "the outer arm retired with its process"
        );
    }

    /// A crash caught inside an armed thread's handler keeps the first arm,
    /// so the switch names the outer handler's signal.
    #[test]
    fn a_nested_crash_keeps_the_first_arm() {
        let mut state = on();
        step(&mut state, 5, 3, delivered(SEGV, false), 10);
        step(&mut state, 5, 3, delivered(ABRT, false), 11);
        let (_, taken) = step(&mut state, 5, 3, PostCrashEvent::Exec, 12);
        assert_eq!(taken.unwrap().signal, SEGV);
    }

    /// An armed thread that exits is forgotten.
    #[test]
    fn an_exited_armed_thread_is_forgotten() {
        let mut state = on();
        step(&mut state, 5, 3, delivered(SEGV, false), 10);
        state.forget(tid(5));
        assert_eq!(
            step(&mut state, 6, 3, PostCrashEvent::Exec, 12),
            (None, None)
        );
    }

    /// With two threads of one process armed, the switch names the lower
    /// thread id, the same in every run whatever the order they were armed
    /// in.
    #[test]
    fn the_switch_names_the_same_armed_thread_every_run() {
        for order in [[7, 4], [4, 7]] {
            let mut state = on();
            for raw in order {
                step(&mut state, raw, 3, delivered(ABRT, false), 1);
            }
            let (_, taken) = step(&mut state, 9, 3, PostCrashEvent::Exec, 2);
            assert_eq!(taken.unwrap().dtid, tid(4));
        }
    }

    /// A replay takes exactly the switches its record holds, in order; a
    /// different one, or one more than the record holds, is refused.
    #[test]
    fn a_replay_takes_only_the_recorded_switches() {
        let mut record = on();
        let (_, recorded) = step(&mut record, 4, 3, delivered(SEGV, true), 7);
        let recorded = recorded.unwrap();
        let mut replay = PostCrashState::new(NonZeroU64::new(BOUND), Some(vec![recorded]));
        assert_eq!(
            replay
                .step(tid(4), pid(3), delivered(SEGV, true), 7)
                .unwrap()
                .1,
            Some(recorded)
        );
        let mut elsewhere = PostCrashState::new(NonZeroU64::new(BOUND), Some(vec![recorded]));
        let refusal = elsewhere
            .step(tid(4), pid(3), delivered(SEGV, true), 8)
            .unwrap_err();
        assert!(refusal.contains("left its record"), "{refusal}");
        let short = PostCrashState::new(NonZeroU64::new(BOUND), Some(vec![recorded]));
        assert_eq!(
            short.summary().unwrap().recorded_not_taken,
            1,
            "never reached"
        );
        let mut unrecorded = PostCrashState::new(NonZeroU64::new(BOUND), Some(Vec::new()));
        let refusal = unrecorded
            .step(tid(4), pid(3), delivered(SEGV, true), 7)
            .unwrap_err();
        assert!(refusal.contains("no further switch"), "{refusal}");
    }
}
