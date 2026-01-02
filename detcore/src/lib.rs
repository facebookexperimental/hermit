/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore is a Reverie tool that determinizes the execution of a process.

#![feature(nonzero_ops)]
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
#[allow(unused)]
mod mvar;
mod procmaps;
mod record_or_replay;
mod resources;
mod scheduler;
mod stat;
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
pub use config::SchedHeuristic;
use rand::Rng;
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
use reverie::syscalls::Fork;
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
use tool_global::deregister_thread;
pub use tool_local::Detcore;
pub use tool_local::FileMetadata;
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
use crate::syscalls::helpers::with_guest_rip;
use crate::syscalls::helpers::with_guest_time;
use crate::tool_global::resource_request;
use crate::tool_global::trace_schedevent;
use crate::tool_global::unrecoverable_shutdown;
use crate::types::SigWrapper;

#[macro_use]
extern crate bitflags;

impl<T: RecordOrReplay> Detcore<T> {
    async fn passthrough<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

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
        if self.cfg.use_rcb_time() {
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
            thread_state.thread_logical_time.add_rcbs(delta_rcbs);
            thread_state.committed_clock_value = clock_value;

            if thread_state.end_of_timeslice.is_some() {
                if let Some(last_timer) = thread_state.last_rcb_timer {
                    if delta_rcbs > last_timer && precise_timers {
                        panic!(
                            "prehook: Missed expected preemption! Clock_value: {}. Stepped forward {:?} RCBs, but should have trapped at {:?}",
                            clock_value, delta_rcbs, last_timer
                        );
                        // TODO: turn the above panic into a warning again if we see any weirdness
                        // on certain platforms.
                        // We can repair the state and keep going, using the below:
                        /*
                        thread_state.end_of_timeslice = None;
                        thread_state.last_rcb_timer = None;
                        thread_state.next_timeslice(&self.cfg);
                        */
                    }
                    // Otherwise we're very early, at the prehook of handle_thread_start.
                }
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
            if self.cfg.should_trace_schedevent() {
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
        let thread_state = guest.thread_state();
        if let Some(slice_end) = thread_state.end_of_timeslice {
            let now_ns = thread_state.thread_logical_time.as_nanos();
            if slice_end < now_ns {
                trace!(
                    "prehook [dtid {}] Time {} is beyond end of slice {}",
                    dettid, now_ns, slice_end
                );
                self.end_timeslice(guest).await;
            }
        }

        if let Some(vec) = evs {
            for ev in vec {
                trace_schedevent(guest, ev, false).await;
            }
        }
    }

    /// A common hook called at the end of *every* handler, just before returning control
    /// to the guest. This resets the preemption timeout for the next timeslice.
    ///
    /// However, note that the thread's timeslice (turn) may have expired DURING this handler.
    /// Therefore the timeslice can end in the posthook as well as in the prehook.
    async fn post_handler_hook<G: Guest<Self>>(&self, guest: &mut G) {
        let dettid = guest.thread_state().dettid;
        let timeout_delta_ns = guest.config().preemption_timeout;
        let current_time = guest.thread_state_mut().thread_logical_time.as_nanos();

        // If the preemption timeout is set, then end_of_timeslice must always be.
        if let Some(slice_end) = guest.thread_state().end_of_timeslice {
            let mut slice_end = slice_end;
            assert!(timeout_delta_ns.is_some());
            // TODO: get rid of fractional NANOS_PER_RCB so it's clear that this does not lose precision:
            let epsilon = Duration::from_nanos(NANOS_PER_RCB as u64);

            // If there is less than a single RCB worth of time left... we call it.
            if current_time + epsilon > slice_end {
                // Our timeslice wasn't over yet at the start of the handler.  But the
                // elapse of time during the handler (e.g. executing a logical syscall)
                // has pushed us over the threshold to where our time is used up.
                // Therefore, we don't let the guest/mutator proceed executing more code yet.
                // Rather, we go back to the scheduler and wait for our turn again.
                trace!(
                    "posthook [dtid {}] Time {} beyond (or close enough to) end of slice {}! Ending slice.",
                    dettid, current_time, slice_end
                );
                let current_slice = guest.thread_state().stats.timeslice_count;
                self.end_timeslice(guest).await;
                // After ending time slice, need to reread this:
                slice_end = guest
                    .thread_state()
                    .end_of_timeslice
                    .expect("internal invariant");
                let current_time = guest.thread_state().thread_logical_time.as_nanos();
                trace!(
                    "posthook [dtid {}] after ending timeslice T{}, next end is {}, current time {}",
                    dettid, current_slice, slice_end, current_time,
                );
            }
            if current_time + epsilon > slice_end {
                panic!(
                    "Unhandled! Ended time slice, but current time {} is still beyond (or too near) timeslice end {}",
                    current_time, slice_end
                );
            }

            let ns_remaining = slice_end - current_time;
            let mut rcbs_remaining = ns_remaining.into_rcbs();

            if !guest.thread_state().interrupt_at.is_empty() {
                let current_rcbs = guest.thread_state().thread_logical_time.rcbs();

                if let Some(next_interrupt) = guest
                    .thread_state()
                    .interrupt_at
                    .range((current_rcbs + 1)..)
                    .next()
                {
                    rcbs_remaining = next_interrupt - current_rcbs;

                    debug!(
                        "[dtid: {}] current rcbs: {}, next interrupt_at: {}",
                        dettid, current_rcbs, next_interrupt
                    )
                }
            }

            trace!(
                "posthook [dtid {}] After time consumed by handler/injection, {} remaining in slice ({} rcbs).",
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
            guest.thread_state_mut().last_rcb_timer = Some(rcbs_remaining);

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
            assert!(timeout_delta_ns.is_none());
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
    ///  - ends timeslice (mutating thread stats, end_of_timeslice)
    ///  - priority change / yield RPC
    async fn end_timeslice<G: Guest<Self>>(&self, guest: &mut G) {
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

        if cfg!(debug_assertions) {
            Subscription::all()
        } else {
            let mut subscription = Subscription::none();
            subscription.syscalls([
                Sysno::write,
                Sysno::openat,
                Sysno::open,
                Sysno::creat,
                Sysno::close,
                Sysno::read,
                Sysno::mmap,
                Sysno::fcntl,
                Sysno::futex,
                Sysno::clone,
                Sysno::clone3,
                Sysno::fork,
                Sysno::vfork,
                Sysno::wait4,
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
                Sysno::memfd_create,
                Sysno::userfaultfd,
                Sysno::accept,
                Sysno::accept4,
                Sysno::nanosleep,
                Sysno::clock_nanosleep,
                Sysno::sched_yield,
                Sysno::poll,
                Sysno::epoll_create,
                Sysno::epoll_create1,
                Sysno::epoll_ctl,
                Sysno::epoll_pwait,
                Sysno::epoll_wait,
                Sysno::epoll_wait_old,
                Sysno::epoll_ctl_old,
                Sysno::recvfrom,
                Sysno::rt_sigtimedwait,
                Sysno::execve,
                Sysno::execveat,
                Sysno::getcpu,
                Sysno::rt_sigprocmask,
                Sysno::rt_sigaction,
                Sysno::sysinfo,
                // TODO(T137258824): add proper Select / PSelect6
                // Sysno::pselect6,
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
            intercepted.cpuid(eax).unwrap()
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

                ThreadState {
                    dettid,
                    detpid: None, // Initialized later.
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
                            Arc::new(Mutex::new(pts.1.file_metadata.lock().unwrap().clone()))
                        }
                    },
                    clone_flags: None,

                    // For a child thread, we use the parent to initialize our rng state:
                    prng: thread_rng_from_parent("USER RAND", &pts.1.prng, dettid),
                    chaos_prng: thread_rng_from_parent("CHAOSRAND", &pts.1.chaos_prng, dettid),

                    // For comparing progress to other threads, it is important that our
                    // child thread start at a sensible place, rather than starting back
                    // at zero:
                    thread_logical_time: pts.1.thread_logical_time.clone(),
                    // A new thread gets a new clock, so we've committed 0 ticks
                    committed_clock_value: 0,

                    end_of_timeslice: None,
                    last_rcb_timer: None,

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
        self.pre_handler_hook(guest, false).await;

        if let Some(ptr) = guest.auxv().at_random() {
            // It is safe to mutate this address since libc has not yet had a
            // chance to modify or copy the auxv table.
            let bytes: [u8; 16] = guest.thread_state_mut().thread_prng().r#gen();
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
        let new_count = {
            // which results from not being able to borrow guest twice.
            let thread_state = guest.thread_state_mut();
            thread_state.stats.count_syscall();

            // Add the syscall to our thread's logical progress, advancing logical time.
            if config.sequentialize_threads {
                thread_state.thread_logical_time.add_syscall();
            } else {
                let bumpit = matches!(
                    call,
                    Syscall::Gettimeofday(_)
                        | Syscall::Time(_)
                        | Syscall::ClockGettime(_)
                        | Syscall::Write(_)
                );
                // TODO(T86591083): remove this conditional to bump logical time unconditionally:
                if bumpit && virtualize_time {
                    thread_state.thread_logical_time.add_syscall();
                }
            }
            thread_state.stats.syscall_count
        };

        let res = match call {
            Syscall::Write(w) => self.handle_write(guest, w).await,
            Syscall::Openat(o) => self.handle_openat(guest, o).await,
            Syscall::Open(o) => self.handle_openat(guest, o.into()).await,
            Syscall::Creat(o) => self.handle_openat(guest, o.into()).await,
            Syscall::Close(s) => self.handle_close(guest, s).await,
            Syscall::Read(s) => self.handle_read(guest, s).await,
            Syscall::Mmap(s) => self.handle_mmap(guest, s).await,
            Syscall::Stat(s) => self.handle_stat_family(guest, s.into()).await,
            Syscall::Lstat(s) => self.handle_stat_family(guest, s.into()).await,
            Syscall::Fstat(s) => self.handle_stat_family(guest, s.into()).await,
            Syscall::Newfstatat(s) => self.handle_stat_family(guest, s.into()).await,
            Syscall::Statx(s) => self.handle_statx(guest, s).await,
            Syscall::Fcntl(s) => self.handle_fcntl(guest, s).await,
            Syscall::Futex(s) => self.handle_futex(guest, s).await,

            // TODO(): fix vfork and handle CLONE_VFORK cases here:
            Syscall::Clone(s) => self.handle_clone_family(guest, s.into()).await,
            Syscall::Clone3(s) => self.handle_clone_family(guest, s.into()).await,
            Syscall::Fork(s) => self.handle_clone_family(guest, s.into()).await,

            // Our child-thread-creation protocol doesn't support vfork blocking the parent thread yet:
            Syscall::Vfork(_s) => self.handle_clone_family(guest, Fork::new().into()).await,
            Syscall::Wait4(s) => self.handle_wait4(guest, s).await,

            Syscall::Setsid(s) => self.handle_setsid(guest, s).await,
            Syscall::Gettimeofday(s) if virtualize_time => self.handle_gettimeofday(guest, s).await,
            Syscall::Time(s) if config.virtualize_time => self.handle_time(guest, s).await,
            Syscall::ClockGettime(s) if virtualize_time => {
                self.handle_clock_gettime(guest, s).await
            }
            Syscall::ClockGetres(s) if virtualize_time => self.handle_clock_getres(guest, s).await,
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
            Syscall::Socket(s) => self.handle_socket(guest, s).await,
            Syscall::Socketpair(s) => self.handle_socketpair(guest, s).await.map_err(Into::into),
            Syscall::Connect(s) => self.handle_connect(guest, s).await,
            Syscall::Bind(s) => self.handle_bind(guest, s).await,
            Syscall::Eventfd(s) => self.handle_eventfd2(guest, s.into()).await,
            Syscall::Eventfd2(s) => self.handle_eventfd2(guest, s).await.map_err(Into::into),
            Syscall::Signalfd(s) => self
                .handle_signalfd4(guest, s.into())
                .await
                .map_err(Into::into),
            Syscall::Signalfd4(s) => self.handle_signalfd4(guest, s).await.map_err(Into::into),
            Syscall::TimerfdCreate(s) => self
                .handle_timerfd_create(guest, s)
                .await
                .map_err(Into::into),
            Syscall::MemfdCreate(s) => self.handle_memfd_create(guest, s).await.map_err(Into::into),
            Syscall::Userfaultfd(s) => self.handle_userfaultfd(guest, s).await.map_err(Into::into),
            Syscall::Accept(s) => self.handle_accept4(guest, s.into()).await,
            Syscall::Accept4(s) => self.handle_accept4(guest, s).await,

            Syscall::Nanosleep(s) => self.handle_nanosleep_family(guest, s.into()).await,
            Syscall::ClockNanosleep(s) => self
                .handle_nanosleep_family(guest, s.into())
                .await
                .map_err(Into::into),
            Syscall::SchedYield(s) => self.handle_sched_yield(guest, s).await,

            // NB: getdents is not recommended, (g)libc should call getdents64 only
            // see: sysdeps/unix/sysv/linux/getdents.c.
            Syscall::Getdents(s) => self.handle_getdents(guest, s).await,
            Syscall::Getdents64(s) => self.handle_getdents64(guest, s).await,

            Syscall::Poll(s) => self.handle_poll(guest, s).await,
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
            Syscall::EpollCtlOld(s) => panic!(
                "Not handling deprecated syscall: {}",
                s.display(&guest.memory())
            ),

            Syscall::SchedGetaffinity(s) => self.handle_sched_getaffinity(guest, s).await,
            Syscall::SchedSetaffinity(s) => self.handle_sched_setaffinity(guest, s).await,

            Syscall::Recvfrom(s) => self.handle_sendrecv(guest, s).await,
            Syscall::Recvmsg(s) => self.handle_sendrecv(guest, s).await,
            Syscall::Sendto(s) => self.handle_sendrecv(guest, s).await,
            Syscall::Sendmsg(s) => self.handle_sendrecv(guest, s).await,
            Syscall::Sendmmsg(s) => self.handle_sendrecv(guest, s).await,

            // TODO: handle timeout behavior:
            // Syscall::Recvmmsg(_) => self.handle_recvmmsg(guest, call).await,
            Syscall::RtSigtimedwait(s) => self.handle_rt_sigtimedwait(guest, s).await,

            Syscall::Execve(s) => self.handle_execveat(guest, s.into()).await,
            Syscall::Execveat(s) => self.handle_execveat(guest, s).await,

            Syscall::Getcpu(s) => self.handle_getcpu(guest, s).await,
            Syscall::RtSigprocmask(s) => self.handle_rt_sigprocmask(guest, s).await,
            Syscall::RtSigaction(s) => self.handle_rt_sigaction(guest, s).await,
            Syscall::Alarm(s) => self.handle_alarm(guest, s).await,
            Syscall::Pause(s) => self.handle_pause(guest, s).await,

            // These are to allow execution of a minimal rust executable
            // (namely //hermetic_infra/detcore:get-syscall-support)
            Syscall::Brk(_) => self.passthrough(guest, call).await,
            Syscall::Readlink(_) => self.passthrough(guest, call).await,
            Syscall::Access(_) => self.passthrough(guest, call).await,
            Syscall::Mprotect(_) => self.passthrough(guest, call).await,
            Syscall::ArchPrctl(_) => self.passthrough(guest, call).await,
            Syscall::SetTidAddress(_) => self.passthrough(guest, call).await,
            Syscall::SetRobustList(_) => self.passthrough(guest, call).await,
            Syscall::Prlimit64(_) => self.passthrough(guest, call).await,
            Syscall::Readlinkat(_) => self.passthrough(guest, call).await,
            Syscall::Madvise(_) => self.passthrough(guest, call).await,
            Syscall::Munmap(_) => self.passthrough(guest, call).await,
            Syscall::Prctl(_) => self.passthrough(guest, call).await,
            Syscall::Sigaltstack(_) => self.passthrough(guest, call).await,
            Syscall::Sysinfo(s) => self.handle_sysinfo(guest, s).await,

            // TODO(#30) handle key mgmt syscalls, virtualizing serial numbers:
            Syscall::AddKey(_) => self.passthrough(guest, call).await,
            Syscall::Keyctl(_) => self.passthrough(guest, call).await,
            Syscall::RequestKey(_) => self.passthrough(guest, call).await,

            _ => {
                if config.panic_on_unsupported_syscalls {
                    error!(
                        "[detcore, dtid {}] inbound syscall: {} = ?",
                        dettid,
                        call.display(&guest.memory()),
                    );
                    panic!("unsupported syscall: {:?}", call);
                }
                self.passthrough(guest, call).await
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
        res
    }

    async fn on_exit_thread<G: GlobalRPC<Self::GlobalState>>(
        &self,
        tid: Tid,
        global_state: &G,
        thread_state: Self::ThreadState,
        exit_status: ExitStatus,
    ) -> Result<(), Error> {
        let dettid = thread_state.dettid;
        info!(
            "[detcore, dtid {}] thread exit hook, deregistering from scheduler.",
            dettid
        );
        let detpid = thread_state.detpid.expect("Missing DetPid");
        deregister_thread(
            dettid,
            thread_state.thread_logical_time.clone(),
            &self.cfg,
            global_state,
            detpid,
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
