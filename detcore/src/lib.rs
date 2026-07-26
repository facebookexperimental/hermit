/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore is a Reverie tool that determinizes the execution of a process.
//!
//! # Backend-abstraction commandment
//!
//! Detcore is a *tool* written against Reverie's **abstract** instrumentation
//! interface (the `reverie` crate). It depends only on those traits and types
//! and is deliberately ignorant of how a guest is actually traced.
//!
//! Detcore MUST NEVER depend on or import a concrete Reverie backend --
//! `reverie-ptrace`, `reverie-dbi`, or `reverie-kvm`. Choosing and
//! instantiating a backend, and running a detcore tool against it, is the sole
//! responsibility of the `hermit-cli` package. There are no backend-specific
//! hacks in detcore: any tracing-mechanism-specific behavior belongs behind the
//! Reverie abstraction, not here.
//!
//! Why: Hermit follows Reverie's abstract model. A backend dependency in
//! detcore would couple the determinism engine to one tracing mechanism and
//! break the clean abstraction boundary that lets the same tool run over any
//! backend.
//!
//! The one allowed exception is test-only: detcore's own integration tests
//! (under `detcore/tests/`, wired via the `reverie-ptrace` **dev-dependency**)
//! drive a real tracer to exercise the tool. That coupling never reaches the
//! shipped library. This invariant is enforced in CI by
//! `scripts/check-detcore-backend-abstraction.sh`.

#![deny(clippy::all)]
#![deny(missing_docs)]
#![allow(clippy::uninlined_format_args)]

mod config;
mod consts;
mod cpuid;
mod dirents;
mod fd;
#[allow(unused)]
mod ivar;
pub mod logdiff;
mod memory;
#[allow(unused)]
mod mvar;
mod procfs;
mod procmaps;
mod record_or_replay;
mod resources;
mod scheduler;
mod stat;
mod syscall_classification;
mod syscall_time;
mod syscalls;
mod tool_global;
mod tool_local;
pub mod util;

pub mod detlog;
pub mod preemptions;
pub mod types;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

pub use config::BlockingMode;
pub use config::Config;
pub use config::RunsPostFork;
pub use config::SchedHeuristic;
use rand::RngExt as _;
use raw_cpuid::CpuIdResult;
use raw_cpuid::cpuid;
pub use record_or_replay::RecordOrReplay;
use reverie::Error;
use reverie::ExitStatus;
use reverie::GlobalRPC;
use reverie::Guest;
use reverie::Pid;
use reverie::Rdtsc;
use reverie::RdtscResult;
use reverie::RegDisplay;
use reverie::Signal;
use reverie::Subscription;
use reverie::Tid;
use reverie::TimerSchedule;
use reverie::Tool;
pub use reverie::process::Namespace;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::Displayable;
use reverie::syscalls::EpollCreate1;
use reverie::syscalls::Errno;
use reverie::syscalls::InotifyInit1;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
pub use scheduler::Priority;
pub use scheduler::runqueue::DEFAULT_PRIORITY;
pub use scheduler::runqueue::FIRST_PRIORITY;
pub use scheduler::runqueue::LAST_PRIORITY;
pub use tool_global::GlobalState;
use tool_global::create_child_thread;
use tool_global::create_vfork_child_thread;
use tool_global::deregister_thread;
pub use tool_global::format_unsupported_syscall_warning;
use tool_global::report_unsupported_syscall;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the typed fail-closed backend signal.
/// Identifies an unsupported syscall that a backend must terminate without unwinding.
#[derive(Debug)]
pub struct UnsupportedSyscallError(pub Sysno);

impl std::fmt::Display for UnsupportedSyscallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "unsupported syscall: {:?}", self.0)
    }
}

impl std::error::Error for UnsupportedSyscallError {}
pub use tool_local::Detcore;
pub use tool_local::FileMetadata;
/// Returns whether the audited runtime policy classifies `sysno` as unsupported.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the copied-DBI-child classification surface.
pub fn is_unsupported_syscall(sysno: Sysno) -> bool {
    matches!(
        syscall_classification::classify_syscall(sysno),
        syscall_classification::SyscallClassification::Unsupported
    )
}

use tool_local::PosixTimers;
use tool_local::ProcessCpuTime;
pub use tool_local::ThreadState;
pub use tool_local::ThreadStats;
pub use tool_local::thread_rng_from_parent;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;
pub use types::DetTid;
use types::*;
pub use util::punch_out_print;

use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::syscall_classification::SyscallClassification;
use crate::syscall_classification::classify_syscall;
use crate::syscall_classification::is_credential_identity_noop_syscall;
use crate::syscall_classification::is_mount_ns_admin_refused_syscall;
use crate::syscall_classification::is_privileged_admin_refused_syscall;
use crate::syscall_classification::is_unimplemented_enosys_syscall;
use crate::syscall_classification::is_unsupported_async_ipc_syscall;
use crate::syscalls::helpers::with_guest_rip;
use crate::syscalls::helpers::with_guest_time;
use crate::tool_global::resource_request;
use crate::tool_global::trace_schedevent;
use crate::tool_global::unrecoverable_shutdown;
use crate::types::SigWrapper;

#[macro_use]
extern crate bitflags;

#[cold]
fn report_rcb_overshoot(
    panic_on_rcb_overshoot: bool,
    clock_value: u64,
    delta_rcbs: u64,
    last_timer: u64,
) {
    let message = format!(
        "prehook: PMU RCB overshoot! Clock_value: {}. Stepped forward {} RCBs, but should have trapped at {}",
        clock_value, delta_rcbs, last_timer
    );
    if panic_on_rcb_overshoot {
        panic!("{}", message);
    }
    error!("{}", message);
}

fn choose_rcb_timer(
    max_rcbs_remaining: u64,
    current_rcbs: u64,
    next_interrupt: Option<u64>,
) -> (u64, bool) {
    if let Some(next_interrupt) = next_interrupt {
        let interrupt_rcbs = next_interrupt - current_rcbs;
        if interrupt_rcbs < max_rcbs_remaining {
            return (interrupt_rcbs, false);
        }
    }
    (max_rcbs_remaining, true)
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Registers a child whose native backend executed the clone syscall.
    ///
    /// The caller must initialize the child's local thread state from the same
    /// parent state and clone flags before the child enters its start hook.
    // TODO-HUMAN-REVIEW(PR-743): Review the backend-neutral native child registration API.
    pub async fn register_external_child<G: Guest<Self>>(
        &self,
        guest: &mut G,
        child_tid: Tid,
        child_tid_addr: usize,
        flags: CloneFlags,
    ) {
        tool_global::create_child_thread(
            guest,
            DetTid::from_raw(child_tid.into()),
            child_tid_addr,
            Some(flags),
        )
        .await;
    }
    async fn passthrough<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-643): Review unsupported-syscall reporting and fail-fast behavior.
    /// Applies the legacy policy to an explicitly listed but unsupported syscall.
    async fn handle_unsupported_syscall<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        dettid: DetTid,
        panic_on_unsupported_syscalls: bool,
    ) -> Result<i64, Error> {
        if panic_on_unsupported_syscalls {
            error!(
                "[detcore, dtid {}] inbound syscall: {} = ?",
                dettid,
                call.display(&guest.memory()),
            );
            if guest.config().shutdown_on_unsupported_syscall {
                unrecoverable_shutdown(guest).await;
            }
            if guest.config().exit_on_unsupported_syscall {
                return Err(Error::Tool(anyhow::Error::new(UnsupportedSyscallError(
                    call.number(),
                ))));
            }
            panic!("unsupported syscall: {:?}", call);
        }
        report_unsupported_syscall(guest, call.number()).await;
        self.passthrough(guest, call).await
    }

    /// Defense-in-depth determinism for the registers the syscall instruction
    /// clobbers.
    ///
    /// On x86-64 the `syscall` instruction destroys `%rcx` (which the CPU loads
    /// with the return instruction pointer) and `%r11` (the saved `RFLAGS`).
    /// After a syscall these are architecturally "undefined", so hermit must not
    /// assume a well-behaved guest ignores them: a misbehaving guest that reads
    /// `%rcx`/`%r11` must still observe deterministic values. Reverie's
    /// injected-syscall path can otherwise leave its *private trampoline page's*
    /// RIP/RFLAGS in these registers, which is both nondeterministic and an
    /// information leak of tracer internals.
    ///
    /// This forces both registers to the guest's own (deterministic) RIP and
    /// RFLAGS, which is exactly what a faithful `SYSRET` would leave there. It is
    /// a no-op when they already hold the canonical values (the common path), so
    /// it only writes registers when something diverged. Register-preserved
    /// arguments (`%rdi`..`%r9`, callee-saved) are deliberately left untouched:
    /// the Linux ABI preserves them, so zeroing them would break faithful,
    /// well-behaved programs.
    #[cfg(target_arch = "x86_64")]
    async fn canonicalize_syscall_clobbers<G: Guest<Self>>(&self, guest: &mut G) {
        let mut regs = guest.regs().await;
        // A faithful SYSRET leaves the return RIP in %rcx and RFLAGS in %r11.
        if regs.rcx != regs.rip || regs.r11 != regs.eflags {
            regs.rcx = regs.rip;
            regs.r11 = regs.eflags;
            if let Err(err) = guest.set_regs(regs).await {
                // Best-effort: some backends cannot write registers. Do not fail
                // the syscall over a defense-in-depth hardening step.
                debug!(
                    "canonicalize_syscall_clobbers: set_regs unsupported/failed: {}",
                    err
                );
            }
        }
    }

    /// No-op on architectures without the x86-64 `%rcx`/`%r11` syscall clobber.
    #[cfg(not(target_arch = "x86_64"))]
    async fn canonicalize_syscall_clobbers<G: Guest<Self>>(&self, _guest: &mut G) {}

    /// Update logical thread time with any outstanding ticks of the Reverie clock.  Returns a list
    /// of corresponding Branch/OtherInstructions events if schedule recording is enabled.
    ///
    /// # Arguments
    ///
    /// * `precise_branch`: if true, there were no non-branch instructions since the last recorded branch instruction.
    async fn update_logical_time_rcbs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        precise_branch: bool,
    ) -> Option<Vec<SchedEvent>> {
        if self.cfg.max_timeslice.is_some() {
            let precise_timers = !guest.config().imprecise_timers;
            // TODO(T86591083): we might need to not always increment as a hack fix
            // for deterministic virtual time without sequentialize threads.
            let clock_value = guest.read_clock().expect("Couldn't read clock");
            // N.B. clock_value does not yet include any updates for the inbound
            // syscall/instruction because this function is the very first thing that
            // happens in each type of handler.
            let thread_state = guest.thread_state_mut();
            let dettid = thread_state.dettid;
            assert!(thread_state.committed_clock_value <= clock_value);
            let delta_rcbs: u64 = clock_value - thread_state.committed_clock_value;
            if self.cfg.use_rcb_time() {
                thread_state.thread_logical_time.add_rcbs(delta_rcbs);
            }
            thread_state.account_process_cpu_time();
            thread_state.committed_clock_value = clock_value;

            if thread_state.end_of_timeslice.is_some() {
                if let Some(last_timer) = thread_state.last_rcb_timer
                    && delta_rcbs >= last_timer
                    && precise_timers
                {
                    report_rcb_overshoot(
                        self.cfg.panic_on_rcb_overshoot,
                        clock_value,
                        delta_rcbs,
                        last_timer,
                    );
                    // Preserve timer state. `pre_handler_hook` will yield through the normal
                    // scheduler path if the slice expired; `post_handler_hook` will otherwise
                    // re-arm an overshot `interrupt_at` timer.
                }
                // Otherwise we're very early, at the prehook of handle_thread_start.
            } else {
                panic!(
                    "Invariant violation: end_of_timeslice is None during update_logical_time_rcbs..."
                )
            }

            trace!(
                "[dtid {}] updated rcb clock, new logical time: {:?}, i.e. {}, timeslice end: {}, local rcb clock_value {:?}",
                dettid,
                &thread_state.thread_logical_time,
                thread_state.thread_logical_time.as_nanos(),
                thread_state
                    .end_of_timeslice
                    .map_or_else(|| "".to_string(), |x| format!("{}", x)),
                clock_value,
            );
            if self.cfg.use_rcb_time() && self.cfg.should_trace_schedevent() {
                let mut vec = Vec::new();
                let ev = with_guest_time(
                    guest,
                    SchedEvent::branches(
                        dettid,
                        delta_rcbs
                            .try_into()
                            .expect("should not have more than 2^32 branches at once"),
                    ),
                );
                let ev = if precise_branch {
                    with_guest_rip(guest, ev).await
                } else {
                    ev
                };

                if delta_rcbs > 0 {
                    // We don't fill the end_rip here, because the current rip is NOT precisely the
                    // end of this block of branch events.  Other instructions may have occured
                    // since the last branch.
                    vec.push(ev)
                } else {
                    trace!(
                        "[detcore, dtid {}] Refusing to record zero-braches event: {:?}",
                        &ev.dettid, ev
                    );
                }
                if !precise_branch {
                    // This will ALWAYS record, even if the branches above are zero.
                    let ev2 = with_guest_time(
                        guest,
                        SchedEvent {
                            dettid,
                            op: Op::OtherInstructions,
                            count: 1,
                            start_rip: None,
                            end_rip: None,
                            end_time: None,
                        },
                    );
                    // Fill in end_rip because current rip represents the end of this event.
                    let ev2 = with_guest_rip(guest, ev2).await;
                    vec.push(ev2);
                }
                Some(vec)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// A common hook called at the start of *every* handler, just after we receive
    /// control from the guest.
    async fn pre_handler_hook<G: Guest<Self>>(&self, guest: &mut G, precise_branch: bool) {
        let dettid = guest.thread_state().dettid;
        let evs = self.update_logical_time_rcbs(guest, precise_branch).await;

        if guest.thread_state().guest_past_first_execve() {
            detlog_debug!(
                "(pre) registers [dtid {}][rcbs {}]. {}",
                dettid,
                guest.thread_state().thread_logical_time.rcbs(),
                guest.regs().await.display()
            );
        }
        trace!(
            "prehook [dtid {}] Updating rcbs and checking time remaining.",
            dettid
        );
        if let Some(vec) = evs {
            for ev in vec {
                trace_schedevent(guest, ev, false).await;
            }
        }

        self.end_timeslice_if_needed(guest).await;
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    /// Yield when accumulated logical time reaches the syscall-boundary target deadline.
    async fn end_timeslice_if_needed<G: Guest<Self>>(&self, guest: &mut G) {
        let thread_state = guest.thread_state();
        let Some(slice_end) = thread_state.end_of_timeslice else {
            return;
        };
        if !thread_state.timeslice_expired() {
            return;
        }

        trace!(
            "[dtid {}] logical time {} reached timeslice target {}",
            thread_state.dettid,
            thread_state.thread_logical_time.as_nanos(),
            slice_end
        );
        self.end_timeslice(guest).await;
    }

    /// A common hook called at the end of *every* handler, just before returning control
    /// to the guest. This enforces the logical target and resets the PMU maximum timer.
    ///
    /// However, note that the thread's timeslice (turn) may have expired DURING this handler.
    /// Therefore the timeslice can end in the posthook as well as in the prehook.
    async fn post_handler_hook<G: Guest<Self>>(&self, guest: &mut G) {
        self.end_timeslice_if_needed(guest).await;

        let dettid = guest.thread_state().dettid;
        let mut current_time = guest.thread_state().thread_logical_time.as_nanos();

        if let Some(mut max_timeslice_end) = guest.thread_state().max_timeslice_end {
            assert!(guest.config().max_timeslice.is_some());
            // TODO: get rid of fractional NANOS_PER_RCB so it's clear that this does not lose precision:
            let clock_multiplier = guest.config().clock_multiplier.unwrap_or(1.0);
            let epsilon = Duration::from_nanos((NANOS_PER_RCB * clock_multiplier).ceil() as u64);

            if current_time + epsilon > max_timeslice_end {
                trace!(
                    "posthook [dtid {}] less than one RCB remains before PMU maximum {}; ending slice",
                    dettid, max_timeslice_end
                );
                self.end_timeslice(guest).await;
                max_timeslice_end = guest
                    .thread_state()
                    .max_timeslice_end
                    .expect("ending a PMU-backed timeslice must install a new maximum");
                current_time = guest.thread_state().thread_logical_time.as_nanos();
            }
            if current_time + epsilon > max_timeslice_end {
                panic!(
                    "Ended time slice, but current time {} is still beyond PMU maximum {}",
                    current_time, max_timeslice_end
                );
            }

            let ns_remaining = max_timeslice_end - current_time;
            let max_rcbs_remaining = ns_remaining.into_rcbs_with_multiplier(clock_multiplier);
            let current_rcbs = guest.thread_state().thread_logical_time.rcbs();
            let next_interrupt = self
                .cfg
                .use_rcb_time()
                .then(|| {
                    guest
                        .thread_state()
                        .interrupt_at
                        .range((current_rcbs + 1)..)
                        .next()
                        .copied()
                })
                .flatten();
            let (rcbs_remaining, timer_is_max) =
                choose_rcb_timer(max_rcbs_remaining, current_rcbs, next_interrupt);
            if let Some(next_interrupt) = next_interrupt {
                debug!(
                    "[dtid: {}] current rcbs: {}, next interrupt_at: {}",
                    dettid, current_rcbs, next_interrupt
                )
            }

            trace!(
                "posthook [dtid {}] {} remaining before PMU maximum ({} rcbs).",
                dettid, ns_remaining, rcbs_remaining,
            );

            if ns_remaining.is_zero() {
                panic!(
                    "Timer invariant broken: we should not exit a handler with 0 timeslice remaining."
                );
            }
            assert!(rcbs_remaining > 0);
            trace!(
                "posthook [dtid {}] Resetting timer to {:?} RCBs in the future (current {})",
                dettid,
                rcbs_remaining,
                guest.thread_state().thread_logical_time.rcbs()
            );
            {
                let thread_state = guest.thread_state_mut();
                thread_state.last_rcb_timer = Some(rcbs_remaining);
                thread_state.last_rcb_timer_is_max = timer_is_max;
            }

            if guest.config().imprecise_timers {
                guest
                    .set_timer(TimerSchedule::Rcbs(rcbs_remaining))
                    .expect("Failed to set timer");
            } else {
                guest
                    .set_timer_precise(TimerSchedule::Rcbs(rcbs_remaining))
                    .expect("Failed to set timer");
            }
        } else {
            assert!(guest.config().max_timeslice.is_none());
            guest.thread_state_mut().last_rcb_timer = None;
            guest.thread_state_mut().last_rcb_timer_is_max = false;
        }

        if guest.thread_state().guest_past_first_execve() {
            detlog_debug!(
                "(post) registers [dtid {}][rcbs {}]. {}",
                dettid,
                guest.thread_state().thread_logical_time.rcbs(),
                guest.regs().await.display(),
            );
        }
    }

    /// End this logical timeslice and talk to the scheduler before continuing.
    ///
    /// Effects
    ///  - ends timeslice (mutating thread stats and both deadlines)
    ///  - priority change / yield RPC
    async fn end_timeslice<G: Guest<Self>>(&self, guest: &mut G) {
        self.end_timeslice_with_sched_yield(guest, false).await;
    }

    async fn end_timeslice_for_sched_yield<G: Guest<Self>>(&self, guest: &mut G) {
        self.end_timeslice_with_sched_yield(guest, true).await;
    }

    async fn end_timeslice_with_sched_yield<G: Guest<Self>>(
        &self,
        guest: &mut G,
        explicit_sched_yield: bool,
    ) {
        let thread_state = guest.thread_state();
        let dettid = thread_state.dettid;
        let chaos = guest.config().chaos;
        let end_time = thread_state.thread_logical_time.as_nanos();
        info!(
            "[detcore, dtid {}] ending timeslice T{}. {} syscalls and {} signals this timeslice.",
            dettid,
            thread_state.stats.timeslice_count,
            thread_state.stats.timeslice_syscall_count,
            thread_state.stats.timeslice_signal_count,
        );
        let maybe_prio = guest.thread_state_mut().next_timeslice(&self.cfg); // Reset end_of_timeslice

        // Depending on chaos mode, a received timer event is either a preemption or a changepoint
        let req = if let Some(prio) = maybe_prio {
            Self::priority_changepoint_request(guest, end_time, prio)
        } else if chaos {
            Self::random_priority_changepoint_request(guest, end_time)
        } else if explicit_sched_yield && self.cfg.replay_schedule_from.is_none() {
            Self::sched_yield_request(guest)
        } else {
            Self::yield_request(guest)
        };
        resource_request(guest, req).await;
    }

    fn detlog_memory_maps<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), reverie::Error> {
        if !(self.cfg.detlog_stack || self.cfg.detlog_heap) {
            // Don't incur the *significant* performance penalty for reading
            // /proc/maps unless one of these flags is enabled.
            return Ok(());
        }
        for mmap in procmaps::from_pid(guest.pid(), |map| match map.pathname {
            procmaps::MMapPath::Stack if self.cfg.detlog_stack => true,
            procmaps::MMapPath::Heap if self.cfg.detlog_heap => true,
            _ => false,
        })? {
            let dettid = guest.thread_state().dettid;
            detlog!(
                "[memory][dtid {}] {}->{}",
                dettid,
                procmaps::display(&mmap),
                procmaps::compute_hash(guest, &mmap)?
            )
        }
        Ok(())
    }

    fn display_syscall_finished<'a, M: MemoryAccess>(
        syscall: &'a Syscall,
        memory: &'a M,
    ) -> reverie::syscalls::Display<'a, M, Syscall> {
        match syscall {
            Syscall::Fstat(_) => syscall.display(memory), //FIXME: T136880615 - fstat structure isn't fully deterministic yet
            _ => syscall.display_with_outputs(memory),
        }
    }
}

#[reverie::tool]
impl<T: RecordOrReplay> Tool for Detcore<T> {
    type GlobalState = GlobalState;
    type ThreadState = ThreadState<T::ThreadState>;

    /// Constructor for Detcore process-local state.
    fn new(pid: Pid, cfg: &Config) -> Self {
        let detpid = DetPid::from_raw(pid.into()); // TODO(T78538674): virtualize pid.
        cfg.validate_invariants();
        Self {
            detpid,
            cfg: cfg.clone(),
            record_or_replay: T::new(pid, cfg),
        }
    }

    /// NOTE: these subscriptions are used ONLY for hermit run mode.  Hermit record has its own
    /// subscriptions specified in recorder/mod.rs.
    fn subscriptions(config: &Config) -> Subscription {
        let do_sched =
            config.sched_heuristic != SchedHeuristic::None || config.sequentialize_threads;

        if !config.passthru_opt {
            // Fail closed by default in every build profile. Besides allowing syscall-specific
            // handlers to run, interception is what charges generic syscall logical time.
            Subscription::all()
        } else {
            // Explicit performance opt-in: unlisted syscalls bypass Detcore entirely. Keep this
            // path separate so its allow-list can be tightened without weakening the default.
            let mut subscription = Subscription::none();
            subscription.syscalls([
                Sysno::write,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#547)
                Sysno::writev,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#683)
                Sysno::pwrite64,
                Sysno::openat,
                Sysno::open,
                Sysno::creat,
                Sysno::close,
                Sysno::read,
                Sysno::pread64,
                Sysno::lseek,
                Sysno::fadvise64,
                Sysno::mmap,
                Sysno::madvise,
                Sysno::munmap,
                Sysno::mremap,
                Sysno::fcntl,
                Sysno::arch_prctl,
                Sysno::ioctl,
                Sysno::futex,
                Sysno::clone,
                Sysno::clone3,
                Sysno::fork,
                Sysno::vfork,
                Sysno::wait4,
                Sysno::waitid,
                Sysno::setsid,
                Sysno::uname,
                Sysno::exit_group,
                Sysno::exit,
                Sysno::dup,
                Sysno::dup2,
                Sysno::dup3,
                Sysno::pipe,
                Sysno::pipe2,
                Sysno::getrandom,
                Sysno::utime,
                Sysno::utimes,
                Sysno::utimensat,
                Sysno::futimesat,
                Sysno::socket,
                Sysno::socketpair,
                Sysno::eventfd,
                Sysno::eventfd2,
                Sysno::sched_getaffinity,
                Sysno::sched_setaffinity,
                Sysno::signalfd,
                Sysno::signalfd4,
                Sysno::timerfd_create,
                Sysno::timerfd_settime,
                Sysno::timerfd_gettime,
                Sysno::inotify_init,
                Sysno::inotify_init1,
                Sysno::inotify_add_watch,
                Sysno::inotify_rm_watch,
                Sysno::memfd_create,
                Sysno::userfaultfd,
                Sysno::io_uring_setup,
                Sysno::io_uring_enter,
                Sysno::io_uring_register,
                Sysno::accept,
                Sysno::accept4,
                Sysno::nanosleep,
                Sysno::clock_nanosleep,
                Sysno::sched_yield,
                Sysno::poll,
                Sysno::ppoll,
                Sysno::prlimit64,
                Sysno::epoll_create,
                Sysno::epoll_create1,
                Sysno::epoll_ctl,
                Sysno::epoll_pwait,
                Sysno::epoll_wait,
                Sysno::epoll_wait_old,
                Sysno::epoll_ctl_old,
                Sysno::recvfrom,
                Sysno::rt_sigsuspend,
                Sysno::rt_sigtimedwait,
                Sysno::execve,
                Sysno::execveat,
                Sysno::rseq,
                Sysno::getpid,
                Sysno::gettid,
                Sysno::getcpu,
                Sysno::rt_sigprocmask,
                Sysno::rt_sigaction,
                Sysno::getrusage,
                Sysno::sysinfo,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#686): Review scratch fd sets and scheduler polling.
                Sysno::pselect6,
                // TODO(T137258824): add proper Select
                // Sysno::select,
            ]);

            if do_sched {
                subscription.syscalls([
                    // TODO: some of the above could probably move to this bucket.
                    Sysno::alarm,
                    Sysno::pause,
                ]);
            }

            if config.virtualize_metadata {
                subscription.syscalls([
                    Sysno::getdents,
                    Sysno::getdents64,
                    Sysno::stat,
                    Sysno::lstat,
                    Sysno::fstat,
                    Sysno::newfstatat,
                    Sysno::statx,
                ]);
            }

            if true
            // TODO: could introduce a flag for this:
            /* config.virtualize_keys */
            {
                subscription.syscalls([Sysno::add_key, Sysno::request_key, Sysno::keyctl]);
            }

            if do_sched {
                subscription.syscall(Sysno::connect);
            }
            if do_sched || config.warn_non_zero_binds {
                subscription.syscall(Sysno::bind);
            }

            if config.warn_non_zero_binds {
                subscription.syscall(Sysno::bind);
            }

            if config.virtualize_time {
                subscription.rdtsc();
                subscription.syscalls([
                    Sysno::gettimeofday,
                    Sysno::time,
                    Sysno::clock_gettime,
                    Sysno::clock_getres,
                ]);
            }

            if config.virtualize_cpuid {
                subscription.cpuid();
            }

            // Make sure we also intercept everything that the record-or-replay tool
            // wants.
            subscription | T::subscriptions(config)
        }
    }

    async fn handle_cpuid_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        eax: u32,
        ecx: u32,
    ) -> Result<CpuIdResult, Errno> {
        trace!("handle_cpuid_event: eax: {}, ecx: {}", eax, ecx);
        self.pre_handler_hook(guest, false).await;
        let res = if self.cfg.virtualize_cpuid {
            let dettid = guest.thread_state().dettid;
            let time = &mut guest.thread_state_mut().thread_logical_time;
            let intercepted = cpuid::InterceptedCpuid::new();
            time.add_cpuid();
            let nanos = time.as_nanos();
            trace!(
                "[dtid {}] inbound cpuid, new logical time: {:?}",
                dettid, time
            );
            if self.cfg.should_trace_schedevent() {
                trace_schedevent(
                    guest,
                    SchedEvent {
                        dettid,
                        op: Op::Cpuid,
                        count: 1,
                        start_rip: None,
                        end_rip: None,
                        end_time: Some(nanos),
                    },
                    true,
                )
                .await;
            }
            intercepted.cpuid(eax).unwrap_or_else(|| {
                warn!(
                    "[dtid {}] cpuid leaf 0x{:x} not in deterministic table; returning zero result",
                    dettid, eax
                );
                CpuIdResult {
                    eax: 0,
                    ebx: 0,
                    ecx: 0,
                    edx: 0,
                }
            })
        } else {
            cpuid!(eax, ecx)
        };
        self.post_handler_hook(guest).await;
        Ok(res)
    }

    async fn handle_rdtsc_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: Rdtsc,
    ) -> Result<RdtscResult, Errno> {
        trace!("handle_rdtsc_event: {:?}", request);
        self.pre_handler_hook(guest, false).await;
        let result = if guest.config().virtualize_time {
            let dettid = guest.thread_state().dettid;
            guest.thread_state_mut().thread_logical_time.add_rdtsc();
            info!(
                "[dtid {}] inbound rdtsc, new logical time: {:?}",
                dettid,
                guest.thread_state().thread_logical_time
            );
            if self.cfg.should_trace_schedevent() {
                let ev = with_guest_time(
                    guest,
                    SchedEvent {
                        dettid,
                        op: Op::Rdtsc,
                        count: 1,
                        start_rip: None,
                        end_rip: None,
                        end_time: None,
                    },
                );
                trace_schedevent(guest, ev, true).await;
            }
            // TODO: use global time for rdtsc:
            Ok(RdtscResult {
                tsc: guest
                    .thread_state()
                    .thread_logical_time
                    .as_nanos()
                    .as_nanos(), // We treat virtual cycles as equivalent to virtual nanoseconds.
                aux: None,
            })
        } else {
            self.record_or_replay
                .handle_rdtsc_event(&mut guest.into_guest(), request)
                .await
        };
        self.post_handler_hook(guest).await;
        result
    }

    // Note: we will not see SIGSTKFLT used for timers.
    async fn handle_signal_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        signal: Signal,
    ) -> Result<Option<Signal>, Errno> {
        if signal == Signal::SIGINT && self.cfg.sigint_instakill {
            warn!("Fatal: Exiting hermit container immediately upon SIGINT");
            unrecoverable_shutdown(guest).await
        } else {
            self.pre_handler_hook(guest, false).await;

            let dettid = guest.thread_state().dettid;
            let mycount = guest.thread_state().stats.signal_count;
            info!(
                "[dtid {}] handling inbound signal (#{}) {}",
                dettid, mycount, signal
            );
            guest.thread_state_mut().stats.count_signal();
            let time = &guest.thread_state().thread_logical_time;
            let nanos = time.as_nanos();

            if self.cfg.sequentialize_threads && self.cfg.should_trace_schedevent() {
                trace_schedevent(
                    guest,
                    SchedEvent {
                        dettid,
                        op: Op::SignalReceived(signal.into()),
                        count: 1,
                        start_rip: None,
                        end_rip: None,
                        end_time: Some(nanos),
                    },
                    true,
                )
                .await;
            }

            let request = guest.thread_state_mut().mk_request(
                ResourceID::InboundSignal(SigWrapper(signal)),
                Permission::RW,
            );
            resource_request(guest, request).await;
            info!(
                "[dtid {}] finish delivering signal (#{}) {}",
                dettid, mycount, signal
            );

            self.post_handler_hook(guest).await;
            Ok(Some(signal))
        }
    }

    fn init_thread_state(
        &self,
        tid: Tid,
        parent: Option<(Tid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        trace!("[tid {}] detcore init new thread state", tid);

        let record_or_replay = self
            .record_or_replay
            .init_thread_state(tid, parent.map(|(ptid, ts)| (ptid, ts.as_ref())));

        // TODO(T78538674): virtualize tid, extend tid<=>dettid mapping here.
        match parent {
            None => ThreadState::new(DetPid::from_raw(tid.into()), &self.cfg, record_or_replay),
            Some(pts) => {
                let clone_flags = pts
                    .1
                    .clone_flags
                    .expect("clone_flags must be set by parent");
                let dettid = DetPid::from_raw(tid.into());

                // If we had mutable access to the parent state, we could update it here, but
                // instead we leave that to the clone/fork handling.
                let (child_pedigree, _parent) = pts.1.pedigree.fork();
                let child_logical_time = pts.1.thread_logical_time.clone();
                let last_accounted_user_time = child_logical_time.user_cpu_time();
                let last_accounted_system_time = child_logical_time.system_cpu_time();
                if !clone_flags.contains(CloneFlags::CLONE_THREAD) {
                    pts.1.prepare_child_process_cpu_time(dettid);
                }

                ThreadState {
                    dettid,
                    detpid: None, // Initialized later.
                    mm_id: MmId::for_clone(
                        pts.1.mm_id,
                        dettid,
                        clone_flags.contains(CloneFlags::CLONE_VM),
                    ),
                    memory_metadata: if clone_flags.contains(CloneFlags::CLONE_VM) {
                        Arc::clone(&pts.1.memory_metadata)
                    } else {
                        Arc::new(Mutex::new(
                            pts.1
                                .memory_metadata
                                .lock()
                                .expect("memory metadata mutex poisoned")
                                .clone(),
                        ))
                    },
                    pedigree: child_pedigree,
                    stats: ThreadStats::new(),
                    file_metadata: {
                        debug!(
                            "[init_thread-state, parent dtid = {}] child thread {}, clone_flags = {:x?}",
                            pts.0, tid, clone_flags
                        );
                        if clone_flags.contains(CloneFlags::CLONE_FILES) {
                            pts.1.file_metadata.clone()
                        } else {
                            Arc::new(Mutex::new(
                                pts.1.file_metadata.lock().unwrap().fork_for(dettid),
                            ))
                        }
                    },
                    // POSIX timers are shared among threads of a process but are
                    // NOT inherited across fork(2). Share the table for a new
                    // thread (CLONE_THREAD); give a new process a fresh, empty
                    // one.
                    posix_timers: if clone_flags.contains(CloneFlags::CLONE_THREAD) {
                        Arc::clone(&pts.1.posix_timers)
                    } else {
                        Arc::new(Mutex::new(PosixTimers::default()))
                    },
                    // Resource limits are process state: threads share them,
                    // while a forked process inherits a snapshot.
                    resource_limits: if clone_flags.contains(CloneFlags::CLONE_THREAD) {
                        Arc::clone(&pts.1.resource_limits)
                    } else {
                        Arc::new(Mutex::new(
                            pts.1
                                .resource_limits
                                .lock()
                                .expect("resource limits mutex poisoned")
                                .clone(),
                        ))
                    },
                    process_cpu_time: if clone_flags.contains(CloneFlags::CLONE_THREAD) {
                        Arc::clone(&pts.1.process_cpu_time)
                    } else {
                        Arc::new(Mutex::new(ProcessCpuTime::default()))
                    },
                    parent_process_cpu_time: if clone_flags.contains(CloneFlags::CLONE_THREAD) {
                        pts.1.parent_process_cpu_time.clone()
                    } else {
                        Some(Arc::clone(&pts.1.process_cpu_time))
                    },
                    last_accounted_user_time,
                    last_accounted_system_time,
                    clone_flags: None,
                    pending_vfork: pts.1.pending_vfork.clone(),

                    // For a child thread, we use the parent to initialize our rng state:
                    prng: thread_rng_from_parent("USER RAND", &pts.1.prng, dettid),
                    chaos_prng: thread_rng_from_parent("CHAOSRAND", &pts.1.chaos_prng, dettid),

                    // For comparing progress to other threads, it is important that our
                    // child thread start at a sensible place, rather than starting back
                    // at zero:
                    thread_logical_time: child_logical_time,
                    // A new thread gets a new clock, so we've committed 0 ticks
                    committed_clock_value: 0,

                    end_of_timeslice: None,
                    max_timeslice_end: None,
                    last_rcb_timer: None,
                    last_rcb_timer_is_max: false,

                    record_or_replay,
                    preemption_points: None,

                    // We only get to the point of creating child threads if we're past the first execve.
                    past_global_first_execve: true,
                    interrupt_at: self.cfg.interrupts_for_thread(dettid),
                }
            }
        }
    }

    async fn handle_thread_start<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Error> {
        let detpid = DetPid::from_raw(guest.pid().into());
        trace!(
            "[tid {}] detcore handle_thread_start, pid={}",
            guest.tid(),
            detpid
        );

        // Delayed initialization of thread_state for this new thread:
        guest.thread_state_mut().detpid = Some(detpid);

        let new_dettid = DetTid::from_raw(guest.tid().into()); // TODO(T78538674): virtualize pid/tid:
        assert_eq!(new_dettid, guest.thread_state().dettid);

        if guest.is_root_thread() {
            // There is no fork event to catch for the root thread.
            debug!(
                "[detcore, dtid {}] root thread start, scheduling.. full config:\n {:?}",
                &new_dettid,
                guest.config()
            );
            create_child_thread(guest, new_dettid, 0, None).await;
        } else if let Some(vfork) = guest.thread_state_mut().pending_vfork.take() {
            create_vfork_child_thread(guest, new_dettid, vfork).await;
        }

        // Except for the root task, let's block until it's our turn to go:
        let th = tool_global::thread_start_request(&self.cfg, guest, self.detpid).await;

        // Finish the delayed initialization of the full threadstate:
        {
            let ts = guest.thread_state_mut();
            ts.preemption_points = th.map(|x| x.into_iter());
            ts.next_timeslice(&self.cfg); // Must be after preemption_points is set.
        }

        // The prehook is a noop for a thread just starting.  Can't end the timeslice.  There's no
        // RCB progress to record.  However, we call it for consistency with all the other handlers.
        self.pre_handler_hook(guest, true).await;
        // ^ precise_branch=true: There should have been ZERO prior instructions before this,
        // because the thread hasn't done anything yet.

        self.record_or_replay
            .handle_thread_start(&mut guest.into_guest())
            .await?;

        self.post_handler_hook(guest).await;
        Ok(())
    }

    async fn handle_post_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        guest.thread_state_mut().past_global_first_execve = true;
        tool_global::mark_past_first_execve(guest).await;
        self.pre_handler_hook(guest, false).await;

        if let Some(ptr) = guest.auxv().at_random() {
            // It is safe to mutate this address since libc has not yet had a
            // chance to modify or copy the auxv table.
            let bytes: [u8; 16] = guest.thread_state_mut().thread_prng().random();
            detlog!(
                "[post_exec, dtid {}] init auxv AT_RANDOM value to {:?}",
                guest.thread_state().dettid,
                bytes
            );
            let ptr = unsafe { ptr.into_mut() };
            guest.memory().write_value(ptr, &bytes)?;
        }

        self.post_handler_hook(guest).await;
        Ok(())
    }

    /// A timer fires to preempt the guest and give other threads a turn.
    async fn handle_timer_event<G: Guest<Self>>(&self, guest: &mut G) {
        info!(
            "[detcore, dtid {}] inbound timer preemption event",
            guest.thread_state().dettid
        );
        if guest.config().preemption_stacktrace {
            let mut file_writer: Box<dyn Write> =
                match &guest.config().preemption_stacktrace_log_file {
                    Some(path) => Box::new(
                        File::create(path).expect("Failed to open preemption stacktrace log file"),
                    ),
                    None => Box::new(std::io::stderr()),
                };
            let ts = guest.thread_state();
            writeln!(
                file_writer,
                "\n>>> Guest tid {} preempted at thread time {} with stack trace:",
                ts.dettid,
                ts.thread_logical_time.as_nanos(),
            )
            .unwrap();
            if let Some(backtrace) = guest.backtrace() {
                if let Ok(pbt) = backtrace.pretty() {
                    writeln!(file_writer, "{}", pbt).unwrap();
                } else {
                    writeln!(file_writer, "{}", backtrace).unwrap();
                }
            } else {
                warn!("Could not read backtrace!");
            }
        }
        // This may LOOK like a noop, but actually all of the logic for ending the timeslice is in
        // the prehook.  All the timer has to do is interrupt the guest and generate an extra call
        // to this prehook.
        self.pre_handler_hook(guest, true).await;
        if guest.config().no_rcb_time && guest.thread_state().last_rcb_timer_is_max {
            let max_timeslice_end = guest
                .thread_state()
                .max_timeslice_end
                .expect("PMU maximum timer requires a deadline");
            guest
                .thread_state_mut()
                .thread_logical_time
                .advance_to(max_timeslice_end);
            if self.cfg.should_trace_schedevent() {
                let dettid = guest.thread_state().dettid;
                let ev = with_guest_time(
                    guest,
                    SchedEvent {
                        dettid,
                        op: Op::OtherInstructions,
                        count: 1,
                        start_rip: None,
                        end_rip: None,
                        end_time: None,
                    },
                );
                let ev = with_guest_rip(guest, ev).await;
                trace_schedevent(guest, ev, false).await;
            }
            if self.cfg.replay_schedule_from.is_some() {
                let fallback_deadline = max_timeslice_end
                    + Duration::from_nanos(u64::from(
                        self.cfg
                            .max_timeslice
                            .expect("PMU maximum must be configured"),
                    ));
                let thread_state = guest.thread_state_mut();
                let replay_deadline = thread_state
                    .end_of_timeslice
                    .filter(|deadline| *deadline > max_timeslice_end)
                    .unwrap_or(fallback_deadline);
                thread_state.end_of_timeslice = Some(replay_deadline);
                thread_state.max_timeslice_end = Some(replay_deadline);
                thread_state.last_rcb_timer = None;
                thread_state.last_rcb_timer_is_max = false;
                thread_state.stats.reset_timeslice();
            } else {
                self.end_timeslice(guest).await;
            }
        }
        self.post_handler_hook(guest).await;
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        self.pre_handler_hook(guest, false).await;

        let dettid = guest.thread_state().dettid;

        if guest.thread_state().guest_past_first_execve() {
            detlog!(
                "[syscall][detcore, dtid {}] inbound syscall: {} = ?",
                dettid,
                call.display(&guest.memory())
            );
        }

        let config = guest.config().clone(); // TODO/FIXME: this is an inefficient and unnecessary copy

        if config.sequentialize_threads && self.cfg.should_trace_schedevent() {
            trace_schedevent(
                guest,
                with_guest_time(
                    guest,
                    SchedEvent::syscall(dettid, call.number(), SyscallPhase::Prehook),
                ),
                true,
            )
            .await;
        }

        let virtualize_time = config.virtualize_time;
        let syscall_cost_ns = syscall_time::cost_ns(call.number());
        let new_count = {
            // which results from not being able to borrow guest twice.
            let thread_state = guest.thread_state_mut();
            thread_state.stats.count_syscall();

            // Every intercepted syscall advances logical time, including configurations that do
            // not serialize threads. This keeps virtual clocks productive during syscall loops.
            thread_state
                .thread_logical_time
                .add_syscall_with_cost(syscall_cost_ns);
            thread_state.account_process_cpu_time();
            thread_state.stats.syscall_count
        };

        let res = match classify_syscall(call.number()) {
            // Rseq is not type-safe in the pinned Reverie revision. Dispatch by Sysno so a
            // future typed representation preserves this explicit policy.
            SyscallClassification::Determinized if call.number() == Sysno::rseq => {
                if config.panic_on_unsupported_syscalls {
                    Err(Error::Errno(Errno::ENOSYS))
                } else {
                    self.passthrough(guest, call).await
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#663)
            // The pinned Reverie revision exposes process_madvise only as a raw call.
            SyscallClassification::Determinized if call.number() == Sysno::process_madvise => {
                match call {
                    Syscall::Other(_, args) => Self::handle_process_madvise(args.arg0, args.arg4),
                    _ => unreachable!("process_madvise unexpectedly gained a typed variant"),
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#715): Deterministic ENOSYS for syscalls the pinned
            // x86_64 kernel leaves unimplemented (sys_ni_syscall). A fixed -ENOSYS is
            // deterministic by construction and identical to the modern kernel's own
            // return, so no guest-visible behavior changes versus the legacy
            // pass-through. These are untyped (Syscall::Other) in the pinned Reverie,
            // so dispatch on the Sysno before the typed match below.
            SyscallClassification::Determinized
                if is_unimplemented_enosys_syscall(call.number()) =>
            {
                Err(Error::Errno(Errno::ENOSYS))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#722): Deterministic EPERM for privileged
            // system-administration syscalls (module load/unload, kexec, reboot,
            // swap, raw I/O ports, root-mount pivot, host/domain name, tty
            // hangup, disk quotas). The deterministic guest does not hold the
            // capabilities these require against the host kernel, so a fixed
            // -EPERM matches the unprivileged errno, never perturbs global host
            // state, and is identical across --verify and record/replay. These
            // are untyped (Syscall::Other) in the pinned Reverie, so dispatch on
            // the Sysno before the typed match below.
            SyscallClassification::Determinized
                if is_privileged_admin_refused_syscall(call.number()) =>
            {
                Err(Error::Errno(Errno::EPERM))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#720): set_mempolicy_home_node is untyped in the
            // pinned Reverie revision. Hermit exposes a single virtual NUMA node,
            // so setting a memory range's home node has no observable effect: a
            // deterministic no-op.
            SyscallClassification::Determinized
                if call.number() == Sysno::set_mempolicy_home_node =>
            {
                Ok(0)
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#724): Deterministic EPERM for privileged mount
            // and namespace administration syscalls (mount/umount2/mount_setattr/
            // move_mount/open_tree/fsopen/fsmount/fsconfig/fspick, unshare, setns,
            // open_by_handle_at, fanotify_init/fanotify_mark, settimeofday). A
            // deterministic container pins the guest's namespaces, mount
            // hierarchy, and virtual clock for the whole run, so these are
            // refused with a fixed -EPERM: the unprivileged errno for the
            // capability-gated operations and a deliberate deterministic refusal
            // otherwise. Never forwarded to the host; identical across --verify
            // and record/replay. Untyped (Syscall::Other) in the pinned Reverie,
            // so dispatch on the Sysno before the typed match below.
            SyscallClassification::Determinized
                if is_mount_ns_admin_refused_syscall(call.number()) =>
            {
                Err(Error::Errno(Errno::EPERM))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#731): Deterministic ENOSYS for the
            // asynchronous and message-passing I/O and IPC interfaces Detcore
            // does not model: Linux native AIO (io_setup/io_destroy/io_submit/
            // io_cancel/io_getevents/io_pgetevents), POSIX message queues
            // (mq_*), and System V message queues (msg*). AIO completion is
            // kernel-driven and lives outside logical time; the message-queue
            // families operate on global, key/name-addressed kernel objects
            // shared with the whole host. A fixed -ENOSYS is the errno a kernel
            // built without AIO/CONFIG_POSIX_MQUEUE/CONFIG_SYSVIPC returns, is
            // never forwarded to the host, and is identical across --verify and
            // record/replay (mirrors the io_uring refusal). Untyped
            // (Syscall::Other) in the pinned Reverie, so dispatch on the Sysno
            // before the typed match below.
            SyscallClassification::Determinized
                if is_unsupported_async_ipc_syscall(call.number()) =>
            {
                Err(Error::Errno(Errno::ENOSYS))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#787): BATCH 38. openat2 is untyped (Syscall::Other)
            // in the pinned Reverie revision. It is a superset of openat whose
            // callers must fall back to openat when it returns ENOSYS (kernels
            // before 5.6 lack openat2), so a fixed -ENOSYS routes them onto the
            // already-determinized openat path with no host dependency and behavior
            // identical across --verify and record/replay.
            SyscallClassification::Determinized if call.number() == Sysno::openat2 => {
                Err(Error::Errno(Errno::ENOSYS))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#787): BATCH 38. The credential-setting family
            // (setuid/setgid and their re-/res-/fs- variants, and setgroups) is
            // untyped (Syscall::Other) in the pinned Reverie. Detcore presents a
            // fixed virtual-root identity (getuid/geteuid/getgid/getegid are
            // virtualized to 0) and never tracks a credential change, so these
            // succeed as deterministic no-ops returning 0 -- the value a real root
            // process gets for a permitted credential change (and the previous
            // fs-id, virtual 0, for setfsuid/setfsgid). That lets privilege-
            // dropping programs proceed instead of fail-closing and is identical
            // across --verify and record/replay.
            SyscallClassification::Determinized
                if is_credential_identity_noop_syscall(call.number()) =>
            {
                Ok(0)
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#773): epoll_pwait2 is untyped (Syscall::Other)
            // in the pinned Reverie revision. It is epoll_pwait with a
            // `struct timespec *` timeout; recent glibc routes epoll_wait/
            // epoll_pwait through it. Handled identically to epoll_pwait
            // (scheduler yield + record/replay forwarding).
            SyscallClassification::Determinized if call.number() == Sysno::epoll_pwait2 => {
                self.handle_epoll_pwait2(guest, call).await
            }
            SyscallClassification::Determinized => match call {
                Syscall::Write(w) => self.handle_write(guest, w).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#547)
                Syscall::Writev(w) => self.handle_writev(guest, w).await,
                Syscall::Openat(o) => self.handle_openat(guest, o).await,
                Syscall::Open(o) => self.handle_openat(guest, o.into()).await,
                Syscall::Creat(o) => self.handle_openat(guest, o.into()).await,
                Syscall::Close(s) => self.handle_close(guest, s).await,
                Syscall::Read(s) => self.handle_read(guest, s).await,
                Syscall::Pread64(s) => self.handle_pread64(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#683)
                Syscall::Pwrite64(s) => self.handle_pwrite64(guest, s).await,
                // This syscall is advisory; fixed success preserves its API contract.
                Syscall::Fadvise64(_) => Ok(0),
                Syscall::Mmap(s) => self.handle_mmap(guest, s).await,
                Syscall::Madvise(s) => self.handle_madvise(guest, s).await,
                Syscall::Munmap(s) => self.handle_munmap(guest, s).await,
                Syscall::Mremap(s) => self.handle_mremap(guest, s).await,
                Syscall::Stat(s) => self.handle_stat_family(guest, s.into()).await,
                Syscall::Lstat(s) => self.handle_stat_family(guest, s.into()).await,
                Syscall::Fstat(s) => self.handle_stat_family(guest, s.into()).await,
                Syscall::Newfstatat(s) => self.handle_stat_family(guest, s.into()).await,
                Syscall::Statx(s) => self.handle_statx(guest, s).await,
                Syscall::Fcntl(s) => self.handle_fcntl(guest, s).await,
                Syscall::Ioctl(s) => self.handle_ioctl(guest, s).await,
                Syscall::Futex(s) => self.handle_futex(guest, s).await,

                Syscall::Clone(s) => self.handle_clone_family(guest, s.into()).await,
                Syscall::Clone3(s) => self.handle_clone_family(guest, s.into()).await,
                Syscall::Fork(s) => self.handle_clone_family(guest, s.into()).await,

                // Forward vfork as vfork (rather than rewriting to fork) so the
                // kernel enforces the CLONE_VFORK parent-blocking contract while the
                // child registers itself and runs to exec/exit.
                Syscall::Vfork(s) => self.handle_clone_family(guest, s.into()).await,
                Syscall::Wait4(s) => self.handle_wait4(guest, s).await,
                Syscall::Waitid(s) => self.handle_waitid(guest, s).await,

                Syscall::Setsid(s) => self.handle_setsid(guest, s).await,
                Syscall::Gettimeofday(s) => {
                    if virtualize_time {
                        self.handle_gettimeofday(guest, s).await
                    } else {
                        self.handle_unsupported_syscall(
                            guest,
                            call,
                            dettid,
                            config.panic_on_unsupported_syscalls,
                        )
                        .await
                    }
                }
                Syscall::Time(s) => {
                    if virtualize_time {
                        self.handle_time(guest, s).await
                    } else {
                        self.handle_unsupported_syscall(
                            guest,
                            call,
                            dettid,
                            config.panic_on_unsupported_syscalls,
                        )
                        .await
                    }
                }
                Syscall::ClockGettime(s) => {
                    if virtualize_time {
                        self.handle_clock_gettime(guest, s).await
                    } else {
                        self.handle_unsupported_syscall(
                            guest,
                            call,
                            dettid,
                            config.panic_on_unsupported_syscalls,
                        )
                        .await
                    }
                }
                Syscall::ClockGetres(s) => {
                    if virtualize_time {
                        self.handle_clock_getres(guest, s).await
                    } else {
                        self.handle_unsupported_syscall(
                            guest,
                            call,
                            dettid,
                            config.panic_on_unsupported_syscalls,
                        )
                        .await
                    }
                }
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::ClockSettime(_) => Err(Error::Errno(Errno::EPERM)),
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Setitimer(s) => self.handle_setitimer(guest, s).await,
                Syscall::ArchPrctl(s) => self.handle_arch_prctl(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Prctl(s) => self.handle_prctl(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Getpriority(s) => self.handle_getpriority(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Setpriority(s) => self.handle_setpriority(guest, s).await,
                Syscall::Uname(s) => self.handle_uname(guest, s).await,
                Syscall::ExitGroup(s) => self.handle_exit_group(guest, s).await,
                Syscall::Exit(s) => self.handle_exit(guest, s).await,

                Syscall::Dup(w) => self.handle_dup(guest, w).await.map_err(Into::into),
                Syscall::Dup2(w) => self.handle_dup2(guest, w).await.map_err(Into::into),
                Syscall::Dup3(w) => self.handle_dup3(guest, w).await.map_err(Into::into),
                Syscall::Pipe(w) => self.handle_pipe2(guest, w.into()).await.map_err(Into::into),
                Syscall::Pipe2(w) => self.handle_pipe2(guest, w).await.map_err(Into::into),
                Syscall::Getrandom(s) => self.handle_getrandom(guest, s).await,
                Syscall::Utime(s) => self.handle_utime(guest, s).await.map_err(Into::into),
                Syscall::Utimes(s) => self.handle_utimes(guest, s).await.map_err(Into::into),
                // NB: lutimes is a libc function not a syscall
                Syscall::Utimensat(s) => self.handle_utimensat(guest, s).await.map_err(Into::into),
                // NB: futimes/futimens are libc functions not a syscall,
                // futimesat is obsolete, return -ENOSYS for simplicity.
                Syscall::Futimesat(_s) => Err(Error::Errno(Errno::ENOSYS)),
                // io_uring completion and memory-sharing semantics are not deterministic.
                Syscall::IoUringSetup(_)
                | Syscall::IoUringEnter(_)
                | Syscall::IoUringRegister(_) => Err(Error::Errno(Errno::ENOSYS)),
                Syscall::Socket(s) => self.handle_socket(guest, s).await,
                Syscall::Socketpair(s) => self.handle_socketpair(guest, s).await,
                Syscall::Connect(s) => self.handle_connect(guest, s).await,
                Syscall::Bind(s) => self.handle_bind(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Setsockopt(s) => self.handle_setsockopt(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Listen(s) => self.handle_listen(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Getsockname(s) => self.handle_getsockname(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Getpeername(s) => self.handle_getpeername(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Getsockopt(s) => self.handle_getsockopt(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#818): shutdown is the lone remaining
                // socket-family syscall; half-closes a tracked socket and
                // forwards via record_or_replay (KVM ratchet round 12).
                Syscall::Shutdown(s) => self.handle_shutdown(guest, s).await,
                Syscall::Eventfd(s) => self.handle_eventfd2(guest, s.into()).await,
                Syscall::Eventfd2(s) => self.handle_eventfd2(guest, s).await,
                Syscall::Signalfd(s) => self.handle_signalfd4(guest, s.into()).await,
                Syscall::Signalfd4(s) => self.handle_signalfd4(guest, s).await,
                Syscall::TimerfdCreate(s) => self.handle_timerfd_create(guest, s).await,
                Syscall::TimerfdSettime(s) => self.handle_timerfd_settime(guest, s).await,
                Syscall::TimerfdGettime(s) => self.handle_timerfd_gettime(guest, s).await,
                Syscall::InotifyInit(s) => {
                    self.handle_inotify_init1(guest, InotifyInit1::from(s))
                        .await
                }
                Syscall::InotifyInit1(s) => self.handle_inotify_init1(guest, s).await,
                Syscall::InotifyAddWatch(s) => self.handle_inotify_add_watch(guest, s).await,
                Syscall::InotifyRmWatch(s) => self.handle_inotify_rm_watch(guest, s).await,
                Syscall::MemfdCreate(s) => self.handle_memfd_create(guest, s).await,
                Syscall::Userfaultfd(s) => self.handle_userfaultfd(guest, s).await,
                Syscall::Accept(s) => self.handle_accept4(guest, s.into()).await,
                Syscall::Accept4(s) => self.handle_accept4(guest, s).await,

                Syscall::Nanosleep(s) => self.handle_nanosleep_family(guest, s.into()).await,
                Syscall::ClockNanosleep(s) => self.handle_nanosleep_family(guest, s.into()).await,
                Syscall::SchedYield(s) => self.handle_sched_yield(guest, s).await,

                // NB: getdents is not recommended, (g)libc should call getdents64 only
                // see: sysdeps/unix/sysv/linux/getdents.c.
                Syscall::Getdents(s) => self.handle_getdents(guest, s).await,
                Syscall::Getdents64(s) => self.handle_getdents64(guest, s).await,

                Syscall::Poll(s) => self.handle_poll(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#686): Review scratch fd sets and scheduler polling.
                Syscall::Pselect6(s) => self.handle_pselect6(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#800): select is the timeval sibling of pselect6.
                Syscall::Select(s) => self.handle_select(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                Syscall::Ppoll(s) => self.handle_ppoll(guest, s).await,
                Syscall::EpollCreate(s) => {
                    self.handle_epoll_create1(guest, EpollCreate1::from(s))
                        .await
                }
                Syscall::EpollCreate1(s) => self.handle_epoll_create1(guest, s).await,
                Syscall::EpollCtl(s) => self.handle_epoll_ctl(guest, s).await,
                Syscall::EpollPwait(s) => self.handle_epoll_pwait(guest, s).await,
                Syscall::EpollWait(s) => self.handle_epoll_wait(guest, s).await,
                Syscall::EpollWaitOld(s) => panic!(
                    "Not handling deprecated syscall: {}",
                    s.display(&guest.memory())
                ),
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#549)
                // The obsolete x86_64 entry point is absent from modern Linux kernels.
                Syscall::EpollCtlOld(_) => Err(Error::Errno(Errno::ENOSYS)),

                Syscall::SchedGetaffinity(s) => self.handle_sched_getaffinity(guest, s).await,
                Syscall::SchedSetaffinity(s) => self.handle_sched_setaffinity(guest, s).await,

                // ===== BATCH 3: NUMA memory-placement and Linux CPU-scheduling
                // policy. Hermit exposes a single virtual NUMA node and replaces
                // the Linux scheduler with Detcore, so these are inoperative and
                // are virtualized to fixed, host-independent results (see the
                // determinism argument in syscall_classification.rs). Setters and
                // count-returning calls are no-ops; getters emulate a default
                // single-node / SCHED_OTHER answer.
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#720)
                Syscall::Mbind(_) => Ok(0),
                Syscall::SetMempolicy(_) => Ok(0),
                Syscall::GetMempolicy(s) => self.handle_get_mempolicy(guest, s).await,
                Syscall::MigratePages(_) => Ok(0),
                Syscall::MovePages(s) => self.handle_move_pages(guest, s).await,
                Syscall::SchedSetscheduler(_) => Ok(0),
                Syscall::SchedSetparam(_) => Ok(0),
                // Report the fixed default policy SCHED_OTHER (0).
                Syscall::SchedGetscheduler(_) => Ok(0),
                Syscall::SchedGetparam(s) => self.handle_sched_getparam(guest, s).await,
                Syscall::SchedRrGetInterval(s) => self.handle_sched_rr_get_interval(guest, s).await,

                // ===== BATCH 51: fail-closed utility syscalls with no deterministic
                // effect under Hermit. Detcore replaces the Linux scheduler, exposes a
                // single virtual CPU, and serializes guest threads, so a thread's
                // Linux scheduling attributes (sched_getattr) and I/O priority
                // (ioprio_set) are inert, and an advisory whole-file lock (flock) is
                // never contended inside the serialized container. Emulated to fixed,
                // host-independent results (see syscall_classification.rs); re-enables
                // chrt, ionice, and flock under --strict.
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#791)
                Syscall::SchedGetattr(s) => self.handle_sched_getattr(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#791)
                Syscall::IoprioSet(s) => self.handle_ioprio_set(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#791)
                Syscall::Flock(s) => self.handle_flock(guest, s).await,

                Syscall::Recvfrom(s) => self.handle_sendrecv(guest, s).await,
                Syscall::Recvmsg(s) => self.handle_sendrecv(guest, s).await,
                Syscall::Sendto(s) => self.handle_sendrecv(guest, s).await,
                Syscall::Sendmsg(s) => self.handle_sendrecv(guest, s).await,
                Syscall::Sendmmsg(s) => self.handle_sendrecv(guest, s).await,

                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#788): recvmmsg is the multi-message form of
                // recvmsg and shares its NonblockableSyscall impl. Route it
                // through handle_sendrecv like the other datagram syscalls: the
                // fd is made temporarily nonblocking, the kernel fills the
                // mmsghdr array atomically, and the Detcore scheduler owns any
                // blocking, so the timeout argument (deliberately ignored, see
                // helpers.rs) does not introduce nondeterminism.
                Syscall::Recvmmsg(s) => self.handle_sendrecv(guest, s).await,
                Syscall::RtSigtimedwait(s) => self.handle_rt_sigtimedwait(guest, s).await,
                Syscall::RtSigsuspend(s) => self.handle_rt_sigsuspend(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::RtSigpending(s) => self.handle_rt_sigpending(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Kill(s) => self.handle_kill(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Tgkill(s) => self.handle_tgkill(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#812)
                Syscall::Tkill(s) => self.handle_tkill(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#812)
                Syscall::RtSigqueueinfo(s) => self.handle_rt_sigqueueinfo(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#812)
                Syscall::RtTgsigqueueinfo(s) => self.handle_rt_tgsigqueueinfo(guest, s).await,

                Syscall::Execve(s) => self.handle_execveat(guest, s.into()).await,
                Syscall::Execveat(s) => self.handle_execveat(guest, s).await,

                Syscall::Getcpu(s) => self.handle_getcpu(guest, s).await,
                Syscall::RtSigprocmask(s) => self.handle_rt_sigprocmask(guest, s).await,
                Syscall::RtSigaction(s) => self.handle_rt_sigaction(guest, s).await,
                Syscall::Alarm(s) => self.handle_alarm(guest, s).await,
                Syscall::Pause(s) => self.handle_pause(guest, s).await,

                Syscall::Getrusage(s) => self.handle_getrusage(guest, s).await,
                Syscall::Sysinfo(s) => self.handle_sysinfo(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                Syscall::Times(s) => self.handle_times(guest, s).await,
                Syscall::Prlimit64(s) => self.handle_prlimit64(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Getrlimit(s) => self.handle_getrlimit(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Setrlimit(s) => self.handle_setrlimit(guest, s).await,

                // POSIX per-process timers. Arming is tracked against the virtual
                // clock so these verify deterministically under --strict; timer
                // expiration signals are not delivered (see handle_timer_create).
                Syscall::TimerCreate(s) => self.handle_timer_create(guest, s).await,
                Syscall::TimerSettime(s) => self.handle_timer_settime(guest, s).await,
                Syscall::TimerGettime(s) => self.handle_timer_gettime(guest, s).await,
                Syscall::TimerGetoverrun(s) => self.handle_timer_getoverrun(guest, s).await,
                Syscall::TimerDelete(s) => self.handle_timer_delete(guest, s).await,

                // Serialized threads share a total memory order, so process-wide
                // memory barriers are trivially satisfied and can be no-ops.
                Syscall::Membarrier(s) => self.handle_membarrier(guest, s).await,

                // Filesystem statistics: passthrough is record/replay-aware so the
                // (otherwise host-dependent) result is captured and reproduced.
                // statfs/fstatfs run the real syscall, then canonicalize the
                // host-varying fields (free blocks/inodes, fsid) so the result is
                // deterministic under --verify (a bare passthrough diverged, e.g.
                // for tar).
                Syscall::Statfs(s) => self.handle_statfs(guest, s).await,
                Syscall::Fstatfs(s) => self.handle_fstatfs(guest, s).await,

                unexpected => {
                    self.handle_unsupported_syscall(
                        guest,
                        unexpected,
                        dettid,
                        config.panic_on_unsupported_syscalls,
                    )
                    .await
                }
            },
            // faccessat2 and fchmodat2 are untyped in the pinned Reverie revision; the
            // reviewed classification table routes them, and every other reviewed
            // PassThrough syscall, through the blanket arm below.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-644): Keep dispatch aligned with the reviewed classification.
            SyscallClassification::PassThrough => self.passthrough(guest, call).await,
            SyscallClassification::Unsupported => {
                self.handle_unsupported_syscall(
                    guest,
                    call,
                    dettid,
                    config.panic_on_unsupported_syscalls,
                )
                .await
            }
        };

        detlog!(
            "[syscall][detcore, dtid {}] finish syscall #{}: {} = {:?}",
            dettid,
            new_count,
            Self::display_syscall_finished(&call, &guest.memory()),
            res
        );

        self.detlog_memory_maps(guest)?;

        if config.sequentialize_threads && self.cfg.should_trace_schedevent() {
            trace_schedevent(
                guest,
                with_guest_time(
                    guest,
                    SchedEvent::syscall(dettid, call.number(), SyscallPhase::Posthook),
                ),
                true,
            )
            .await;
        }

        self.post_handler_hook(guest).await;

        // Defense-in-depth: before returning to the guest, force the
        // syscall-clobbered registers (%rcx/%r11 on x86-64) to deterministic
        // values so that even a misbehaving guest cannot observe nondeterminism
        // (or Reverie's private trampoline address) through them.
        self.canonicalize_syscall_clobbers(guest).await;

        res
    }

    async fn on_exit_thread<G: GlobalRPC<Self::GlobalState>>(
        &self,
        tid: Tid,
        global_state: &G,
        mut thread_state: Self::ThreadState,
        exit_status: ExitStatus,
    ) -> Result<(), Error> {
        let dettid = thread_state.dettid;
        info!(
            "[detcore, dtid {}] thread exit hook, deregistering from scheduler.",
            dettid
        );
        // Close the final in-progress timeslice so this thread contributes its
        // last (partial) slice to the run report, even if it never exhausted a
        // full slice.
        let now = thread_state.thread_logical_time.as_nanos();
        thread_state.stats.close_final_timeslice(now);
        // Reverie invokes this callback while the backend still owns the exit
        // event, before the guest parent can consume it with wait. Ptrace also
        // guarantees that the process leader exits after the other threads, so
        // the final published aggregate is complete when wait returns.
        let detpid = thread_state.detpid.expect("Missing DetPid");
        if dettid == detpid {
            thread_state.record_exited_child_process_cpu_time(detpid);
        } else {
            thread_state.account_process_cpu_time();
        }
        let mm_id = thread_state.mm_id;
        deregister_thread(
            dettid,
            thread_state.thread_logical_time.clone(),
            &self.cfg,
            global_state,
            detpid,
            mm_id,
            thread_state.stats.timeslice_stats,
        )
        .await;

        self.record_or_replay
            .on_exit_thread(
                tid,
                global_state,
                thread_state.record_or_replay,
                exit_status,
            )
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod subscription_tests {
    use super::*;

    fn strict_config(passthru_opt: bool) -> Config {
        Config {
            sequentialize_threads: true,
            deterministic_io: true,
            passthru_opt,
            ..Default::default()
        }
    }

    #[test]
    fn strict_subscriptions_intercept_every_event_by_default() {
        let subscriptions = <Detcore as Tool>::subscriptions(&strict_config(false));

        assert_eq!(subscriptions, Subscription::all());
        assert!(
            subscriptions
                .iter_syscalls()
                .any(|sysno| sysno == Sysno::ppoll)
        );
    }

    #[test]
    fn passthru_opt_uses_the_partial_subscription_set() {
        let subscriptions = <Detcore as Tool>::subscriptions(&strict_config(true));

        assert_ne!(subscriptions, Subscription::all());
        assert!(
            subscriptions
                .iter_syscalls()
                .any(|sysno| sysno == Sysno::clock_gettime)
        );
        assert!(
            subscriptions
                .iter_syscalls()
                .any(|sysno| sysno == Sysno::rt_sigsuspend)
        );
        assert!(
            subscriptions
                .iter_syscalls()
                .any(|sysno| sysno == Sysno::ppoll)
        );
        assert!(
            subscriptions
                .iter_syscalls()
                .any(|sysno| sysno == Sysno::madvise)
        );
        assert!(
            subscriptions
                .iter_syscalls()
                .any(|sysno| sysno == Sysno::arch_prctl)
        );
        assert!(
            subscriptions
                .iter_syscalls()
                .any(|sysno| sysno == Sysno::writev)
        );
        assert!(
            subscriptions
                .iter_syscalls()
                .any(|sysno| sysno == Sysno::pwrite64)
        );
    }
}

#[cfg(test)]
mod rcb_overshoot_tests {
    use std::fmt::Write;
    use std::sync::Arc;
    use std::sync::Mutex;

    use tracing::Event;
    use tracing::Id;
    use tracing::Level;
    use tracing::Metadata;
    use tracing::Subscriber;
    use tracing::field::Field;
    use tracing::field::Visit;
    use tracing::span::Attributes;
    use tracing::span::Record;
    use tracing::subscriber::with_default;

    use super::report_rcb_overshoot;

    struct ErrorSubscriber(Arc<Mutex<Option<String>>>);

    struct EventVisitor(String);

    impl Visit for EventVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?}", field.name(), value);
        }
    }

    impl Subscriber for ErrorSubscriber {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == Level::ERROR
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            if *event.metadata().level() == Level::ERROR {
                let mut visitor = EventVisitor(String::new());
                event.record(&mut visitor);
                *self.0.lock().unwrap() = Some(visitor.0);
            }
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    #[test]
    fn default_overshoot_policy_emits_error_and_returns() {
        let error = Arc::new(Mutex::new(None));
        with_default(ErrorSubscriber(error.clone()), || {
            report_rcb_overshoot(false, 16_249, 139, 100);
        });

        let error = error.lock().unwrap().take().expect("missing ERROR event");
        assert!(error.contains("PMU RCB overshoot"), "{error}");
        assert!(error.contains("16249"), "{error}");
        assert!(error.contains("139"), "{error}");
        assert!(error.contains("100"), "{error}");
    }

    #[test]
    #[should_panic(expected = "PMU RCB overshoot")]
    fn opt_in_overshoot_policy_panics() {
        report_rcb_overshoot(true, 16_249, 139, 100);
    }
}

#[cfg(test)]
mod timeslice_timer_tests {
    use super::*;

    #[test]
    fn manual_interrupts_can_shorten_but_not_extend_maximum() {
        assert_eq!(choose_rcb_timer(100, 100, Some(150)), (50, false));
        assert_eq!(choose_rcb_timer(100, 100, Some(250)), (100, true));
        assert_eq!(choose_rcb_timer(100, 100, None), (100, true));
    }

    #[test]
    fn pmu_duration_conversion_applies_clock_multiplier() {
        let duration = crate::types::LogicalTime::from_nanos(100);
        assert_eq!(duration.into_rcbs_with_multiplier(2.0), 5);
        assert_eq!(duration.into_rcbs_with_multiplier(0.5), 20);
        assert_eq!(
            crate::types::LogicalTime::from_nanos(101).into_rcbs_with_multiplier(2.0),
            5
        );
    }

    #[test]
    #[should_panic(expected = "max_timeslice must be at least one RCB")]
    fn detcore_constructor_validates_programmatic_config() {
        let config = Config {
            max_timeslice: std::num::NonZeroU64::new(1),
            ..Default::default()
        };

        let _ = <Detcore as Tool>::new(Pid::from_raw(1), &config);
    }
}
