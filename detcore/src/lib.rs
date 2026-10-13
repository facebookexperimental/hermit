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
//! Detcore MUST NEVER depend on or import a concrete Reverie backend or support
//! crate -- any `reverie-*` crate other than the abstract `reverie-core`
//! interface. Choosing and instantiating a backend, and running a detcore tool
//! against it, is the sole responsibility of the `hermit-cli` package. There
//! are no backend-specific hacks in detcore: any tracing-mechanism-specific
//! behavior belongs behind the Reverie abstraction, not here.
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
mod digest;
mod dirents;
/// Schedule-alignment and edit-distance algorithms shared by Hermit tools.
#[allow(missing_docs)]
pub mod edit_distance;
pub mod exit_dependencies;
mod fatal_core;
mod fd;
mod host_inputs;
pub mod in_guest_site_patching;
mod io_buffers;
mod iovecs;
#[allow(unused)]
mod ivar;
pub mod logdiff;
mod memory;
pub mod netlink_route;
pub mod nonwaiting_write;
mod procfs;
mod procfs_inode;
mod procmaps;
pub mod random;
mod record_or_replay;
mod resources;
mod scheduler;
mod shared_open_files;
mod sigalrm_phase1;
mod sock_diag;
mod stat;
mod syscall_classification;
mod syscall_time;
mod syscalls;
#[cfg(test)]
mod test_pages;
mod tool_global;
mod tool_local;
pub mod util;

pub mod detlog;
pub mod preemptions;
pub mod types;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

pub use config::BlockingMode;
pub use config::CONFIG_FINGERPRINT_ENV;
pub use config::Config;
pub use config::FatalCoreCapture;
pub use config::InheritedSeccomp;
pub use config::LITEINST_CONFIG_FINGERPRINT_ENV;
pub use config::RunsPostFork;
pub use config::SchedHeuristic;
pub use config::config_fingerprint_mismatch;
pub use config::config_wire_fingerprint;
pub use config::decode_backend_config_json;
pub use config::encode_backend_config_json;
pub use config::from_legacy_backend_json;
pub use config::to_legacy_backend_json;
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1120): Review the public canonical Detcore root identity.
pub use consts::ROOT_DETPID;
pub use digest::Digest;
#[cfg(test)]
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
pub use scheduler::TerminalDeadlockHook;
pub use scheduler::runqueue::DEFAULT_PRIORITY;
pub use scheduler::runqueue::FIRST_PRIORITY;
pub use scheduler::runqueue::LAST_PRIORITY;
pub use scheduler::set_terminal_deadlock_hook;
pub use tool_global::BackendFailureCleanup;
pub use tool_global::GlobalState;
use tool_global::ThreadDeregistration;
use tool_global::acknowledge_robust_list_exit_time;
pub use tool_global::cancel_exec;
use tool_global::create_child_thread;
use tool_global::create_vfork_child_thread;
use tool_global::deregister_thread;
pub use tool_global::format_unsupported_syscall_warning;
pub use tool_global::prepare_exec;
use tool_global::report_unsupported_syscall;
use tool_global::robust_list_wakes_after_exec;
use tool_global::robust_list_wakes_after_exit;
pub use tool_global::shared_open_file_reply;
pub use tool_global::shared_open_file_request;
pub use tool_local::initial_resource_limit;

fn select_thread_exit_detpid(
    thread_detpid: Option<DetPid>,
    process_detpid: DetPid,
) -> (DetPid, bool) {
    match thread_detpid {
        Some(detpid) => (detpid, false),
        None => (process_detpid, true),
    }
}

fn select_thread_start_detpid(thread_detpid: Option<DetPid>, guest_pid: Pid) -> DetPid {
    thread_detpid.unwrap_or_else(|| DetPid::from_raw(guest_pid.into()))
}

fn is_root_thread_start(is_root_process: bool, dettid: DetTid, detpid: DetPid) -> bool {
    is_root_process && dettid == detpid
}

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
pub use fd::OpenFileLease;
pub use fd::OpenFileModel;
pub use fd::SharedOpenFileChannel;
pub use fd::SharedOpenFileError;
pub use fd::install_shared_open_file_channel;
pub use shared_open_files::ChunkedOpenFileChannel;
pub use shared_open_files::OpenFileControl;
pub use shared_open_files::OpenFileControlReply;
pub use shared_open_files::OpenFileControlTransport;
pub use tool_local::Detcore;
pub use tool_local::FileMetadata;
/// Returns whether the audited runtime policy classifies `sysno` as unsupported.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the copied-DBT-child classification surface.
pub fn is_unsupported_syscall(sysno: Sysno) -> bool {
    matches!(
        syscall_classification::classify_syscall(sysno),
        syscall_classification::SyscallClassification::Unsupported
    )
}

/// Every syscall in the pinned x86_64 table, including the final entry.
///
/// Backends that sweep the classification table must use this rather than
/// `Sysno::iter()`, which stops one short and silently drops the last row.
pub fn all_pinned_syscalls() -> impl Iterator<Item = Sysno> {
    syscall_classification::all_pinned_syscalls()
}

/// Returns whether the audited runtime policy classifies `sysno` as
/// `Determinized` — that is, Detcore either models the syscall with a handler
/// or applies an explicit deterministic refusal policy to it.
///
/// This is the complement of the refusal boundary below. A backend that
/// executes guest syscalls outside `handle_syscall_event` needs BOTH: the
/// refusal set tells it which syscalls to answer with a fixed errno, and this
/// predicate tells it which syscalls Detcore claims to determinize at all.
/// Running a `Determinized` syscall natively is a determinism hole even when it
/// is not in the refusal set, because the modelling that makes it deterministic
/// lives in a handler the backend never entered.
pub fn is_determinized_syscall(sysno: Sysno) -> bool {
    matches!(
        syscall_classification::classify_syscall(sysno),
        syscall_classification::SyscallClassification::Determinized
    )
}

/// Returns whether `sysno` is a kernel-keyring syscall (`add_key`,
/// `request_key`, `keyctl`) that Detcore hides behind a deterministic
/// `CONFIG_KEYS`-absent boundary under the default fail-closed policy.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-916): Exposed so the copied-DBT-child policy can preserve
// the same keyring isolation boundary that the reclassification (PR-848) moved
// out of the Unsupported set.
pub fn is_kernel_keyring_syscall(sysno: Sysno) -> bool {
    syscall_classification::is_kernel_keyring_syscall(sysno)
}

/// Returns whether Detcore deterministically refuses `sysno` with a fixed
/// errno when the fail-closed policy is active, without consulting the host.
///
/// A backend that lets a guest execute a syscall outside Detcore's
/// `handle_syscall_event` dispatcher consults this to enforce the same fixed
/// refusal the ptrace path enforces. The only production consumer is the DBT
/// copied pre-exec child policy in `detcore-dbt`, which runs without a Detcore
/// tool; the KVM backend, like ptrace, delivers these syscalls to
/// `handle_syscall_event`. It deliberately excludes emulated / no-op /
/// host-forwarding families (credential no-ops, `timer_create`, AF_UNIX
/// autobind), because fail-closing a copied child for those would diverge from
/// the ptrace path rather than match it.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-978): Review the copied-DBT-child deterministic-refusal surface.
pub fn is_deterministically_refused_syscall(sysno: Sysno) -> bool {
    syscall_classification::is_deterministically_refused_syscall(sysno)
}

/// Returns whether `sysno` is refused by the default fail-closed policy but
/// forwarded under the explicit compatibility opt-out. The legacy
/// `strict_only` name is retained for API compatibility.
pub fn is_strict_only_deterministic_refusal_syscall(sysno: Sysno) -> bool {
    syscall_classification::is_strict_only_deterministic_refusal_syscall(sysno)
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
use crate::syscall_classification::is_futex2_enosys_syscall;
use crate::syscall_classification::is_host_kernel_probe_syscall;
use crate::syscall_classification::is_host_security_identity_probe_syscall;
use crate::syscall_classification::is_landlock_sandbox_syscall;
use crate::syscall_classification::is_mount_introspection_enosys_syscall;
use crate::syscall_classification::is_mount_ns_admin_refused_syscall;
use crate::syscall_classification::is_optional_memory_feature_syscall;
use crate::syscall_classification::is_ownership_change_noop_syscall;
use crate::syscall_classification::is_perf_event_enosys_syscall;
use crate::syscall_classification::is_privileged_admin_refused_syscall;
use crate::syscall_classification::is_privileged_observation_refused_syscall;
use crate::syscall_classification::is_process_isolation_refused_syscall;
use crate::syscall_classification::is_remap_file_pages_enosys_syscall;
use crate::syscall_classification::is_unimplemented_enosys_syscall;
use crate::syscall_classification::is_unsupported_async_ipc_syscall;
use crate::syscall_classification::is_zero_copy_pipe_syscall;
use crate::syscalls::helpers::with_guest_rip;
use crate::syscalls::helpers::with_guest_time;
use crate::syscalls::time::guest_clock_time;
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
        "{} prehook: PMU RCB overshoot! Clock_value: {}. Stepped forward {} RCBs, but should have trapped at {}",
        reverie::SKID_OVERSHOOT_MARKER,
        clock_value,
        delta_rcbs,
        last_timer
    );
    if panic_on_rcb_overshoot {
        panic!("{}", message);
    }
    reverie::record_skid_overshoot();
    error!("{}", message);
}

fn rcb_timer_overshot(delta_rcbs: u64, last_timer: u64) -> bool {
    delta_rcbs > last_timer
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

/// The registers a syscall's completion leaves, computed from `entry`, the
/// context at its entry (`rip` at the `syscall` instruction): `rax` holds the
/// result, `rip` follows the instruction, and the instruction's own clobbers
/// hold `rcx` = that `rip` and `r11` = `rflags` (signal phase 1's completed
/// projection, design section 4). Only read, never written back.
fn completed_syscall_projection(
    entry: libc::user_regs_struct,
    result: i64,
) -> libc::user_regs_struct {
    const SYSCALL_INSTRUCTION_LENGTH: u64 = 2;
    let mut completed = entry;
    completed.rax = result as u64;
    completed.rip = entry.rip.wrapping_add(SYSCALL_INSTRUCTION_LENGTH);
    completed.rcx = completed.rip;
    completed.r11 = entry.eflags;
    completed
}

/// What returned control to Detcore, as far as ending a timeslice is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckIn {
    /// A syscall, signal, timer, exec or thread start: any expired slice ends.
    Event,
    /// A trapped `rdtsc` or `cpuid` in the middle of user code: see
    /// [`ThreadState::instruction_trap_ends_timeslice`].
    InstructionTrap,
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Signal phase 1, step I4 (design section 4, "The commit"): when the
    /// caller's process holds a SIGALRM entry that is due for it, run the
    /// signal pipeline ptrace runs at a signal-delivery stop (step 2), take
    /// the entry inside that pipeline's `InboundSignal` turn (step 3), and
    /// hand the delivery to the backend (steps 4 to 8). An entry taken that
    /// the backend cannot deliver is a determinism loss.
    ///
    /// `before_syscall` is the I4 addendum's point c: the delivery happens at
    /// the current syscall's entry and the syscall, not yet run, starts again
    /// after the handler. Returns whether a delivery was handed to the
    /// backend; at a syscall's entry the caller then must not run the syscall.
    ///
    /// `completed` is the syscall's result at its completion (None at its
    /// entry): the signal pipeline then reads the completed register
    /// projection (design section 4).
    async fn deliver_due_sigalrm<G: Guest<Self>>(
        &self,
        guest: &mut G,
        before_syscall: bool,
        completed: Option<i64>,
    ) -> bool {
        if !tool_global::sigalrm_refuses(guest, tool_global::SigalrmControl::DueEntry).await {
            return false;
        }
        if let Some(result) = completed {
            let entry = guest.regs().await;
            guest.thread_state_mut().signal_register_projection =
                Some(completed_syscall_projection(entry, result));
        }
        let pipeline = self.handle_signal_event(guest, Signal::SIGALRM).await;
        guest.thread_state_mut().signal_register_projection = None;
        let delivering = matches!(pipeline, Ok(Some(_)));
        // A suppressed signal's entry is dropped and nothing is delivered.
        if !tool_global::sigalrm_refuses(guest, tool_global::SigalrmControl::TakeEntry).await
            || !delivering
        {
            return false;
        }
        let mut info = [0_u8; reverie::SIGNAL_INFO_SIZE];
        info[0..4].copy_from_slice(&libc::SIGALRM.to_ne_bytes());
        info[8..12].copy_from_slice(&libc::SI_KERNEL.to_ne_bytes());
        let target = reverie::SignalTarget::Process { pid: guest.pid() };
        let delivered = match reverie::SignalEvent::new(libc::SIGALRM, info, target) {
            Ok(event) if before_syscall => guest
                .defer_signal_delivery_before_syscall(event)
                .await
                .is_ok(),
            Ok(event) => guest.defer_signal_delivery(event).await.is_ok(),
            Err(_) => false,
        };
        if !delivered {
            self.stop_after_sigalrm_loss(guest).await;
        }
        if before_syscall {
            // Faithful in its position relative to the guest's syscalls, but
            // not in the interrupted registers the handler sees: recorded as a
            // determinism loss, so verification refuses the run rather than
            // compare it (round 4 of the I4 review; the faithful alternative
            // is a delivery from the trampoline's return, before the syscall
            // instruction runs).
            tool_global::sigalrm_refuses(
                guest,
                tool_global::SigalrmControl::EntryDeliveryUnfaithful,
            )
            .await;
        }
        delivered
    }

    /// A SIGALRM entry this thread took cannot be delivered (or its virtual
    /// mask cannot be read): record the determinism loss in the coordinator,
    /// where the verifier reads it, and stop the process before any guest
    /// instruction runs. Linux would have delivered the signal; continuing
    /// would run the guest as if it never fired.
    ///
    /// The stop is the backend's `exit_group`, not `std::process::exit`: under
    /// in-guest LiteInst the latter is the guest's own `exit`, which runs the
    /// guest's `atexit` handlers and ELF finalizers.
    async fn stop_after_sigalrm_loss<G: Guest<Self>>(&self, guest: &mut G) -> ! {
        tool_global::sigalrm_refuses(guest, tool_global::SigalrmControl::DeliveryLost).await;
        match guest
            .tail_inject(
                reverie::syscalls::ExitGroup::new()
                    .with_status(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            )
            .await {}
    }

    /// Signal phase 1: before a callback can file a scheduler request, make
    /// the scheduler's SIGALRM-blocked bit match the runtime's virtual mask. A
    /// handler's start and return change that mask with no syscall, so the bit
    /// may be stale at any callback (a syscall, or a trapped instruction whose
    /// hooks may end a timeslice). Reads the runtime's virtual bit (one query
    /// it answers from its virtual state) and publishes it when it differs from
    /// the bit this thread last published, inside the thread's turn (no
    /// firing commits while it holds it). Returns the bit.
    async fn sync_sigalrm_eligibility<G: Guest<Self>>(&self, guest: &mut G) -> bool {
        let Ok(blocked) = crate::syscalls::virtual_sigalrm_blocked(guest).await else {
            self.stop_after_sigalrm_loss(guest).await
        };
        if blocked != guest.thread_state().sigalrm_blocked_published {
            tool_global::sigalrm_refuses(
                guest,
                tool_global::SigalrmControl::PublishBlocked(blocked),
            )
            .await;
            guest.thread_state_mut().sigalrm_blocked_published = blocked;
        }
        blocked
    }

    /// Publish the posthook happens-before anchors the current syscall entry
    /// reached at its prehook, now that `res` is the result the guest will
    /// get: after the last observer that can replace it, on every path that
    /// returns a guest result, including an observer's own final errno (the
    /// Codex review of https://github.com/rrnewton/hermit/pull/4048). Which
    /// results are a completion is decided by [`posthook_checkpoint`]; the
    /// pending names are consumed either way.
    async fn publish_posthook_checkpoint<G: Guest<Self>>(
        &self,
        guest: &mut G,
        count: u64,
        res: &Result<i64, Error>,
    ) {
        let posthook = std::mem::take(&mut guest.thread_state_mut().stats.hb_pending_posthook);
        if let Some(checkpoint) = posthook_checkpoint(count, posthook, res) {
            let request = guest.thread_state().mk_request(checkpoint, Permission::R);
            resource_request(guest, request).await;
        }
    }

    /// The completion of a syscall that can resume the guest (a result or an
    /// errno, not a backend failure): in a process that handles SIGALRM, a due
    /// entry is delivered here, where Linux would deliver a signal that became
    /// pending while the call ran (design section 4, point a).
    async fn complete_syscall<G: Guest<Self>>(
        &self,
        guest: &mut G,
        res: Result<i64, Error>,
    ) -> Result<i64, Error> {
        if self.cfg.backend.virtualizes_guest_sigalrm && guest.thread_state().sigalrm_handled {
            let completed = match &res {
                Ok(value) => Some(*value),
                Err(Error::Errno(errno)) => Some(-i64::from(errno.into_raw())),
                Err(_) => None,
            };
            if completed.is_some() {
                self.deliver_due_sigalrm(guest, false, completed).await;
            }
        }
        res
    }

    /// Signal phase 1 at a syscall's entry, before its first scheduler
    /// request, in a process that handles SIGALRM: sync the scheduler's
    /// blocked bit ([`Self::sync_sigalrm_eligibility`]), and while SIGALRM is
    /// unblocked deliver an entry that is already due before this syscall (the
    /// I4 addendum's point c). An entry is due here when it became deliverable
    /// with no syscall completing (a handler returned and unblocked SIGALRM),
    /// or committed during a scheduler yield inside a trapped instruction's
    /// hooks; Linux delivers it before any further guest instruction. Returns
    /// whether a delivery was prepared; the syscall then must not run.
    async fn sigalrm_at_syscall_entry<G: Guest<Self>>(&self, guest: &mut G) -> bool {
        let blocked = self.sync_sigalrm_eligibility(guest).await;
        !blocked && self.deliver_due_sigalrm(guest, true, None).await
    }

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
        exit_signal: libc::c_int,
        physical_ids: Option<(i32, i32)>,
    ) {
        let child_dettid = DetTid::from_raw(child_tid.into());
        guest.thread_state_mut().clone_flags = Some(flags);
        if !flags.contains(CloneFlags::CLONE_THREAD) {
            guest
                .thread_state()
                .prepare_child_process_cpu_time(child_dettid);
        }
        let parent_dettid = guest.thread_state().dettid;
        let parent_pedigree = &mut guest.thread_state_mut().pedigree;
        let child_pedigree = parent_pedigree.fork_mut();
        debug!(
            "[dtid {}] after registering external child (tid {}, pedigree {}) parent's pedigree becomes {}",
            parent_dettid, child_dettid, child_pedigree, parent_pedigree,
        );
        // The kernel clone has already succeeded before an external backend
        // calls this method, so these parent updates are not speculative. If
        // registration observes a retired parent, create_child_thread exits
        // that parent by tail injection; no continuing caller can retry or
        // roll the successful clone back.
        tool_global::create_child_thread(
            guest,
            child_dettid,
            tool_global::child_tid_clear_address(flags, child_tid_addr),
            Some(flags),
            exit_signal,
            physical_ids,
        )
        .await;
        guest.thread_state_mut().clone_flags = None;
    }
    async fn passthrough<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        self.record_or_replay_preserving_tool_errors(guest, call)
            .await
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
                "[detcore, dtid {}] unsupported syscall: {} = ?",
                dettid,
                call.display(&guest.memory()),
            );
            if guest.config().shutdown_on_unsupported_syscall {
                // A fail-closed policy decision: the run named a syscall hermit
                // cannot service and `shutdown_on_unsupported_syscall` says stop.
                unrecoverable_shutdown(guest, detcore_model::HERMIT_POLICY_REFUSAL_EXIT).await;
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

    /// Linux's `restart_syscall`. When no handler runs, the kernel restarts a wait
    /// that returned `ERESTART_RESTARTBLOCK` by running this call at the
    /// interrupted call's address, and the wait resumes with its original deadline
    /// (https://github.com/rrnewton/hermit/issues/3358). Detcore emulates those
    /// waits, so it resumes them from the record the wait kept
    /// (`syscalls::helpers::RestartBlock`). Any other `restart_syscall` keeps the
    /// unsupported-syscall policy.
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3361): Review the
    // emulated `restart_syscall` resumption and the record it consumes.
    async fn handle_restart_syscall<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        restart_block: Option<syscalls::helpers::RestartBlock>,
        dettid: DetTid,
        panic_on_unsupported_syscalls: bool,
    ) -> Result<i64, Error> {
        let rip = guest.regs().await.rip;
        let Some(block) = restart_block.and_then(|block| block.resumed_at(rip)) else {
            trace!(
                "[detcore, dtid {}] restart_syscall at {:#x} restarts no wait Detcore emulated ({:?})",
                dettid, rip, restart_block
            );
            return self
                .handle_unsupported_syscall(guest, call, dettid, panic_on_unsupported_syscalls)
                .await;
        };
        trace!(
            "[detcore, dtid {}] restart_syscall resumes {:?}",
            dettid, block
        );
        match block.call {
            syscalls::helpers::RestartCall::Poll(poll) => {
                self.handle_internal_poll_until(guest, poll, block.deadline)
                    .await
            }
            syscalls::helpers::RestartCall::Futex(futex) => {
                self.handle_futex_from(
                    guest,
                    futex,
                    syscalls::FutexDeadline::Restart(block.deadline),
                )
                .await
            }
        }
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
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-1151)
                if thread_state.chaos_slowdown_active {
                    let factor = thread_state.rcb_time_multiplier();
                    thread_state
                        .thread_logical_time
                        .add_rcbs_with_multiplier(delta_rcbs, factor);
                } else {
                    thread_state.thread_logical_time.add_rcbs(delta_rcbs);
                }
            }
            thread_state.account_process_cpu_time();
            thread_state.committed_clock_value = clock_value;

            if thread_state.end_of_timeslice.is_some() {
                if let Some(last_timer) = thread_state.last_rcb_timer
                    && rcb_timer_overshot(delta_rcbs, last_timer)
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
    /// The guest's registers as Detcore's hooks report them: the completed
    /// projection while a SIGALRM delivery's signal pipeline runs at a
    /// syscall's completion, otherwise the backend's.
    async fn observed_regs<G: Guest<Self>>(&self, guest: &mut G) -> libc::user_regs_struct {
        match guest.thread_state().signal_register_projection {
            Some(projection) => projection,
            None => guest.regs().await,
        }
    }

    /// Detcore's handling of an inbound signal, for both of Reverie's signal
    /// hooks (`Tool::handle_signal_event`, and `Tool::handle_raw_signal_event`
    /// for a realtime signal): the signal takes its turn in the schedule
    /// before it is delivered. Returns the signal to deliver, or `None` to
    /// suppress it.
    async fn handle_inbound_signal<G: Guest<Self>>(
        &self,
        guest: &mut G,
        signal: SigWrapper,
    ) -> Result<Option<SigWrapper>, Errno> {
        if signal.signal() == Some(Signal::SIGINT) && self.cfg.sigint_instakill {
            warn!("Fatal: Exiting hermit container immediately upon SIGINT");
            // ⚠️ NOT A REFUSAL. The operator interrupted the run; hermit examined
            // nothing and decided nothing. Reporting `128 + SIGINT` is what every
            // other tool reports for this, and it keeps 122 meaning one thing.
            unrecoverable_shutdown(guest, detcore_model::HERMIT_SIGINT_DEATH_EXIT).await
        } else {
            if signal.signal() == Some(Signal::SIGCHLD)
                && self.cfg.backend.reports_child_exit_publication
                && !self.child_exit_sigchld_delivers(guest).await
            {
                return Ok(None);
            }
            // The kernel dequeued `signal` before this stop: a hold for it ends
            // here, and this stop's hooks may end the slice
            // (`own_signal_turn_hold`).
            if guest.thread_state().own_signal_turn_hold == Some(signal.raw()) {
                guest.thread_state_mut().own_signal_turn_hold = None;
            }
            self.pre_handler_hook(guest, false, CheckIn::Event).await;

            // For `hermit run --verify`: a guest that faulted on an illegal
            // instruction may have run code that made syscalls Detcore never
            // saw. On ptrace that is the one way past the tracer: a syscall
            // issued from the untraced stub of Reverie's private page is let
            // through, and the instruction after it is `ud2`. So the run
            // reports `/`, the ancestor of every path, and names no host input
            // change. A false alarm only withholds a name. A guest that first
            // rewrites or replaces that code is reported when it does so
            // (`record_untraced_code_change`).
            if signal.signal() == Some(Signal::SIGILL) && self.cfg.record_host_inputs {
                tool_global::record_host_mutation(guest, String::from("/")).await;
            }

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
                        op: Op::SignalReceived(signal),
                        count: 1,
                        start_rip: None,
                        end_rip: None,
                        end_time: Some(nanos),
                    },
                    true,
                )
                .await;
            }

            // Read-write marks this as the delivery itself, which the
            // scheduler's write-only wakes of a signaled sleeper are not
            // (`Scheduler::thread_timer_copies`).
            let request = guest
                .thread_state_mut()
                .mk_request(ResourceID::InboundSignal(signal), Permission::RW);
            resource_request(guest, request).await;
            // A delivered signal may be caught, ignored, or terminate the
            // thread group. Reading the lists is harmless; the collected
            // wakes remain inert unless the ptrace exit callback later reports
            // that this exact signal caused physical exit.
            self.stage_thread_group_robust_list_wakes(
                guest,
                tool_local::RobustListExit::Signal(signal.raw()),
            )
            .await;
            info!(
                "[dtid {}] finish delivering signal (#{}) {}",
                dettid, mycount, signal
            );
            self.record_handler_restore_mask(guest, signal).await;

            self.post_handler_hook(guest, CheckIn::Event).await;
            Ok(Some(signal))
        }
    }

    /// Take back the POSIX timer copies a successful exec flushes
    /// (`Scheduler::take_exec_timer_flush`), one zero-timeout
    /// `rt_sigtimedwait` each, before the new image runs. Linux discards a
    /// queued timer signal at exec (`flush_itimer_signals`) and keeps every
    /// other pending signal; Hermit sends a timer's expiration as an ordinary
    /// signal, which exec would keep
    /// (<https://github.com/rrnewton/hermit/issues/4184>). A copy that cannot
    /// be singled out refuses the run by name.
    async fn flush_exec_timer_copies<G: Guest<Self>>(&self, guest: &mut G) {
        let dettid = guest.thread_state().dettid;
        let refusal = match tool_global::exec_timer_flush(guest).await {
            Err(reason) => Some(reason),
            Ok(flush) => {
                let mut refusal = None;
                // The thread's own queue is dequeued first, as Linux does.
                for set in [flush.own, flush.shared] {
                    for signo in
                        (1..=64).filter(|signo| set & scheduler::kernel_signal_bit(*signo) != 0)
                    {
                        match syscalls::take_pending_signal(guest, signo).await {
                            Ok(taken) if taken == signo as i64 => info!(
                                "[detcore, dtid {}] exec flushed a POSIX timer's pending signal {}",
                                dettid, signo
                            ),
                            other => {
                                refusal = Some(format!(
                                    "taking back a POSIX timer's pending signal {signo} at exec answered {other:?}"
                                ));
                                break;
                            }
                        }
                    }
                }
                refusal
            }
        };
        if let Some(reason) = refusal {
            let message = format!(
                "hermit: thread {dettid} exec'd with a POSIX timer's signal possibly pending: {reason}; Linux discards a queued timer signal at exec, and Detcore cannot single this one out, so the run is refused (https://github.com/rrnewton/hermit/issues/4184)"
            );
            error!("{message}");
            let _ = writeln!(crate::util::RetryingStderr, "{message}");
            tool_global::unrecoverable_shutdown(guest, detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
                .await;
        }
    }

    /// Whether a `SIGCHLD` that stopped this thread is delivered, on a backend
    /// that reports child-exit publication, where both the kernel and the
    /// scheduler send a child's exit `SIGCHLD` (`scheduler::child_exit_sigchld`).
    /// The first one delivered for a child's exit stands for it, and a copy
    /// of either kind that arrives after it is not delivered. The scheduler's
    /// copy (`SI_USER` from this tracer) is delivered with the siginfo Linux
    /// gives the parent. Any other `SIGCHLD` is delivered as it is.
    async fn child_exit_sigchld_delivers<G: Guest<Self>>(&self, guest: &mut G) -> bool {
        use scheduler::child_exit_sigchld::ChildExitSigchldAnswer;
        use scheduler::child_exit_sigchld::OwnCopy;
        let Some(info) = guest.signal_info() else {
            return true;
        };
        // A delivery stop: a kept copy's `InboundSignal` request follows.
        let Some(control) =
            scheduler::child_exit_sigchld::control_for(&info, std::process::id(), true)
        else {
            return true;
        };
        match tool_global::child_exit_sigchld(guest, control).await {
            ChildExitSigchldAnswer::Claimed(dropped) => !dropped,
            ChildExitSigchldAnswer::OwnCopy(OwnCopy::Duplicate) => false,
            ChildExitSigchldAnswer::OwnCopy(OwnCopy::Deliver(own))
            | ChildExitSigchldAnswer::ClaimedFor(own) => {
                if let Err(errno) = guest.set_signal_info(own.to_bytes()) {
                    warn!(
                        "[dtid {}] the SIGCHLD for child {} keeps its siginfo: {}",
                        guest.thread_state().dettid,
                        own.pid,
                        errno
                    );
                }
                true
            }
            _ => true,
        }
    }

    async fn pre_handler_hook<G: Guest<Self>>(
        &self,
        guest: &mut G,
        precise_branch: bool,
        boundary: CheckIn,
    ) {
        // A handler that left early (an error return) may not have cleared
        // these; no request made by this new handler belongs to that syscall,
        // and a wake that syscall made no longer ends the slice.
        guest.thread_state_mut().in_uncharged_bootstrap_syscall = false;
        guest.thread_state_mut().yield_after_futex_wake = false;
        let dettid = guest.thread_state().dettid;
        let evs = self.update_logical_time_rcbs(guest, precise_branch).await;

        if guest.thread_state().guest_past_first_execve() {
            detlog_debug!(
                "(pre) registers [dtid {}][rcbs {}]. {}",
                dettid,
                guest.thread_state().thread_logical_time.rcbs(),
                self.observed_regs(guest).await.display()
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

        self.end_timeslice_if_needed(guest, boundary).await;
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    /// Yield when accumulated logical time reaches the syscall-boundary target deadline.
    async fn end_timeslice_if_needed<G: Guest<Self>>(&self, guest: &mut G, boundary: CheckIn) {
        let thread_state = guest.thread_state();
        // A thread that sent a signal to its own process keeps its turn until
        // it dequeues that signal (`own_signal_turn_hold`).
        if let Some(signal) = thread_state.own_signal_turn_hold {
            trace!(
                "[dtid {}] keeps its turn until it takes signal {}",
                thread_state.dettid, signal
            );
            return;
        }
        let Some(slice_end) = thread_state.end_of_timeslice else {
            return;
        };
        let ends = match boundary {
            CheckIn::Event => thread_state.timeslice_expired(),
            CheckIn::InstructionTrap => thread_state.instruction_trap_ends_timeslice(&self.cfg),
        };
        if !ends {
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
    async fn post_handler_hook<G: Guest<Self>>(&self, guest: &mut G, boundary: CheckIn) {
        // Only `kill` and `rt_sigqueueinfo` set the hold, and they wake no
        // futex waiter (https://github.com/rrnewton/hermit/issues/4034).
        assert!(
            guest.thread_state().own_signal_turn_hold.is_none()
                || !guest.thread_state().yield_after_futex_wake,
            "a futex wake must not end the turn a signal to the sender's own process holds"
        );
        if std::mem::take(&mut guest.thread_state_mut().yield_after_futex_wake) {
            // A turn of its own, outside the timeslice bookkeeping: the slice
            // keeps its deadlines, draws nothing from the thread's chaos PRNG
            // and consumes no recorded preemption point, so a replay that
            // makes the same decision stays aligned with its record. (Under
            // a random `--sched-heuristic` the turn's selection draws from the
            // run queue's generator, as every turn does, identically in record
            // and replay.)
            trace!(
                "posthook [dtid {}] FUTEX_WAKE woke a waiter; yielding to it",
                guest.thread_state().dettid
            );
            let request = Self::yield_request(guest);
            resource_request(guest, request).await;
        }
        self.end_timeslice_if_needed(guest, boundary).await;

        let dettid = guest.thread_state().dettid;
        let mut current_time = guest.thread_state().thread_logical_time.as_nanos();

        if let Some(mut max_timeslice_end) = guest.thread_state().max_timeslice_end {
            assert!(guest.config().max_timeslice.is_some());
            let mut replay_rcb_end = guest.thread_state().replay_rcb_end;
            // TODO: get rid of fractional NANOS_PER_RCB so it's clear that this does not lose precision:
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1151)
            let clock_multiplier = guest.config().clock_multiplier.unwrap_or(1.0)
                * guest.thread_state().rcb_time_multiplier().as_f64();
            let epsilon = Duration::from_nanos((NANOS_PER_RCB * clock_multiplier).ceil() as u64);

            if replay_rcb_end.is_none() && current_time + epsilon > max_timeslice_end {
                if guest.thread_state().own_signal_turn_hold.is_some() {
                    // No guest instruction runs before the delivery stop that
                    // ends the hold, and that stop's post-hook ends this slice
                    // and sets the next timer (`own_signal_turn_hold`).
                    trace!(
                        "posthook [dtid {}] past PMU maximum {} while it keeps its turn",
                        dettid, max_timeslice_end
                    );
                    return;
                }
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
                replay_rcb_end = guest.thread_state().replay_rcb_end;
            }
            if replay_rcb_end.is_none() && current_time + epsilon > max_timeslice_end {
                panic!(
                    "Ended time slice, but current time {} is still beyond PMU maximum {}",
                    current_time, max_timeslice_end
                );
            }

            let current_rcbs = guest.thread_state().thread_logical_time.rcbs();
            let current_pmu_rcbs = guest.thread_state().committed_clock_value;
            let (ns_remaining, max_rcbs_remaining) = if let Some(replay_rcb_end) = replay_rcb_end {
                assert!(
                    replay_rcb_end > current_pmu_rcbs,
                    "recorded PMU RCB deadline {} is not ahead of current {}",
                    replay_rcb_end,
                    current_pmu_rcbs
                );
                let logical_remaining = if max_timeslice_end > current_time {
                    max_timeslice_end - current_time
                } else {
                    LogicalTime::ZERO
                };
                (logical_remaining, replay_rcb_end - current_pmu_rcbs)
            } else {
                let logical_remaining = max_timeslice_end - current_time;
                (
                    logical_remaining,
                    logical_remaining.into_rcbs_with_multiplier(clock_multiplier),
                )
            };
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

            if replay_rcb_end.is_none() && ns_remaining.is_zero() {
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
                self.observed_regs(guest).await.display(),
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
        mut explicit_sched_yield: bool,
    ) {
        let chaos = guest.config().chaos;
        loop {
            let thread_state = guest.thread_state();
            let dettid = thread_state.dettid;
            let end_time = thread_state.thread_logical_time.as_nanos();
            info!(
                "[detcore, dtid {}] ending timeslice T{}. {} syscalls and {} signals this timeslice.",
                dettid,
                thread_state.stats.timeslice_count,
                thread_state.stats.timeslice_syscall_count,
                thread_state.stats.timeslice_signal_count,
            );
            let maybe_prio = guest.thread_state_mut().next_timeslice(&self.cfg);

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

            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1151)
            // Multiple scheduler commits can occur without an intervening
            // conditional branch. Exact-RCB replay represents those as
            // adjacent zero-RCB slices, which must be consumed before the
            // guest resumes.
            if !guest.thread_state().timeslice_expired() {
                break;
            }
            explicit_sched_yield = false;
        }
    }

    /// Hash the guest REGISTER FILE and log it.
    ///
    /// # The sampling boundary: GUEST-LOGICAL CONTROL, never handler interior
    ///
    /// This is called from exactly one place -- immediately after a syscall has finished and its
    /// result has been written back, before the guest resumes. At that instant the guest
    /// LOGICALLY HAS CONTROL: the architectural state is what the guest itself would observe at
    /// its own RIP, and it is the same instant at which stack and heap are already hashed.
    ///
    /// It is deliberately NOT sampled anywhere inside a tool handler. A backend that runs its
    /// handler IN-GUEST (sabre, liteinst, e9patch) executes instructions the ptrace reference
    /// never executes, using guest registers as scratch while it does. Registers there
    /// legitimately differ across backends, so comparing them would report correct behaviour as a
    /// divergence and burn the prefix-depth ratchet on artifacts. Handler-interior state is out of
    /// the domain, not excluded from it by a filter -- the same "define the domain" rule the heap
    /// definition follows.
    ///
    /// # What is in the hash, and what is deliberately not
    ///
    /// Included: the general-purpose registers the guest can observe, `rip`, `rsp`, `rflags`,
    /// `orig_rax`, and the TLS bases `fs_base`/`gs_base`.
    ///
    /// EXCLUDED, with reasons rather than by convenience:
    /// * `rcx` and `r11` -- architecturally clobbered by the `SYSCALL` instruction, which stores
    ///   the return RIP and RFLAGS in them. They carry no information beyond `rip`/`eflags`, which
    ///   ARE hashed, and a patching backend that reaches the kernel by some route other than a
    ///   bare `SYSCALL` will leave different values there for a reason that is not a determinism
    ///   defect.
    /// * The segment selectors `cs`/`ss`/`ds`/`es`/`fs`/`gs` -- constant for a 64-bit userspace
    ///   guest, so they add no signal; the TLS BASES are what a guest actually observes and those
    ///   are hashed.
    fn detlog_registers<G: Guest<Self>>(
        &self,
        guest: &mut G,
        regs: &libc::user_regs_struct,
        seq: u64,
    ) {
        if !self.cfg.detlog_regs {
            return;
        }
        // COST TIER: cadence 1 == full (every control point); N > 1 == spot-check every Nth.
        // The cadence index is a PER-THREAD counter, NOT a shared one: a global atomic would be
        // incremented in whatever order threads happen to reach it, so the cadence -- and
        // therefore which points got sampled -- would itself be nondeterministic. A determinism
        // instrument must not have a nondeterministic sampling schedule.
        //
        // It is `stats.syscall_count`, which starts at ZERO, rather than the syscall ORDINAL used
        // in the log (which starts at 2). With the ordinal, a guest whose control points never
        // land on a multiple of the cadence emitted NOTHING and the run still reported PASS -- a
        // spot-tier green backed by zero samples. Indexing from zero makes the first control point
        // of every thread always sampled, so a spot-tier run can never be silently empty.
        let cadence = self.cfg.detlog_regs_cadence.max(1);
        let index = {
            let stats = &mut guest.thread_state_mut().stats;
            let i = stats.regs_sample_index;
            stats.regs_sample_index = i.saturating_add(1);
            i
        };
        let _ = seq;
        if !index.is_multiple_of(cadence) {
            return;
        }
        let tier = if cadence == 1 {
            "full".to_string()
        } else {
            format!("spot-1/{cadence}")
        };
        let mut bytes = Vec::with_capacity(19 * 8);
        for v in [
            regs.rax,
            regs.rbx,
            regs.rdx,
            regs.rsi,
            regs.rdi,
            regs.rbp,
            regs.rsp,
            regs.r8,
            regs.r9,
            regs.r10,
            regs.r12,
            regs.r13,
            regs.r14,
            regs.r15,
            regs.rip,
            regs.eflags,
            regs.orig_rax,
            regs.fs_base,
            regs.gs_base,
        ] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        detlog!(
            "[registers][dtid {}] control_point=syscall-exit tier={} {}",
            guest.thread_state().dettid,
            tier,
            Digest::new(&bytes)
        );
    }

    fn detlog_memory_maps<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), reverie::Error> {
        if !(self.cfg.detlog_stack || self.cfg.detlog_heap) {
            // Don't incur the *significant* performance penalty for reading
            // /proc/maps unless one of these flags is enabled.
            return Ok(());
        }
        // ...and don't incur it when nothing would observe the record either.
        //
        // The hash below is an argument to `detlog!`, so `tracing` already skips
        // it when INFO is disabled. Enumerating the maps is NOT: it happens
        // before the macro, on every syscall, whether or not a record is ever
        // written. Measured on a QEMU/Linux boot with `RUST_LOG` unset, where
        // each run emitted 123 bytes of log in total: no flag 43.71s,
        // `--detlog-stack` 190.37s (4.36x), `--detlog-heap` 207.90s (4.76x).
        // `--detlog-regs` was already inert because everything it does before
        // its own `detlog!` is trivial; this restores the same property here.
        //
        // Skipping is invisible to the guest: enumerating maps and hashing guest
        // memory are host-side observations of the tracee that neither issue a
        // guest syscall nor advance virtual time, so a run that skips them
        // executes the same guest instruction stream as one that does not.
        if !detlog_observed!() {
            return Ok(());
        }
        // Out-of-process backends (e.g. KVM) report their guest memory regions
        // directly, because `guest.pid()` is the host VMM process there and its
        // `/proc/<pid>/maps` describes the VMM, not the guest address space.
        // Reading those host addresses through `guest.memory()` (guest-address
        // space) would fault and abort the syscall. When the backend supplies
        // regions, hash those guest ranges; otherwise fall back to the ptrace
        // path of parsing `/proc/<pid>/maps`.
        if let Some(regions) = guest.detlog_memory_regions() {
            for region in regions {
                let want = match region.kind {
                    reverie::DetlogRegionKind::Stack => self.cfg.detlog_stack,
                    reverie::DetlogRegionKind::Heap => self.cfg.detlog_heap,
                };
                if !want {
                    continue;
                }
                let dettid = guest.thread_state().dettid;
                detlog!(
                    "[memory][dtid {}] {:?} {:#x}-{:#x}->{}",
                    dettid,
                    region.kind,
                    region.start,
                    region.end,
                    procmaps::compute_hash_range(guest, region.start, region.end)?
                )
            }
            return Ok(());
        }
        let mut labelled_heap = false;
        for mmap in procmaps::from_pid(guest.pid(), |map| match map.pathname {
            procmaps::MMapPath::Stack if self.cfg.detlog_stack => true,
            procmaps::MMapPath::Heap if self.cfg.detlog_heap => true,
            _ => false,
        })? {
            labelled_heap |= matches!(mmap.pathname, procmaps::MMapPath::Heap);
            let dettid = guest.thread_state().dettid;
            detlog!(
                "[memory][dtid {}] {}->{}",
                dettid,
                procmaps::display(&mmap),
                procmaps::compute_hash(guest, &mmap)?
            )
        }

        // The kernel labels `[heap]` only for `[mm->start_brk, mm->brk)`. Under a
        // backend that loads the guest itself (DynamoRIO) that break belongs to
        // the loader, so the guest's heap is an unlabelled anonymous mapping and
        // the filter above matches nothing. Emitting no record there is worse
        // than a wrong one: downstream a zero-record heap comparison reads as
        // "compared and matched" rather than "never measured". Fall back to the
        // brk range Detcore observed, which identifies the heap on every backend.
        if self.cfg.detlog_heap && !labelled_heap {
            self.detlog_brk_heap(guest)?;
        }
        Ok(())
    }

    /// Emit the `[heap]` record from the observed program break, for backends
    /// where the kernel does not label the guest's heap.
    ///
    /// Reports `[start_brk, brk)` -- the range Detcore observed -- taking the
    /// non-address columns from the enclosing anonymous mapping, so the record
    /// is textually comparable with the labelled record another backend
    /// produces for the same guest.
    ///
    /// ⚠️ THE EXTENT IS THE OBSERVED BREAK, NOT THE MAPPING THAT CONTAINS IT,
    /// and the two are not the same region. The selector below admits any
    /// anonymous mapping with `address.0 <= start && end <= address.1`, which
    /// is a SUPERSET by construction. Reporting that mapping instead would make
    /// the comparability claim above true only when the arena happens to
    /// coincide with the break, and would fold non-heap bytes into the digest.
    /// On the backend this path exists for, the premise is that the LOADER owns
    /// the break, so those extra bytes are loader arena -- the least
    /// reproducible region in the process. A silently empty record would become
    /// a loudly divergent one, for a reason that is not the guest's heap.
    ///
    /// The labelled path above hashes its mapping directly, which is correct
    /// there because the kernel defines `[heap]` as exactly `[start_brk, brk)`.
    /// Both paths therefore report the same quantity.
    fn detlog_brk_heap<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), reverie::Error> {
        let Some((start, end)) = guest
            .thread_state()
            .memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .brk_heap_range()
        else {
            return Ok(());
        };
        let enclosing = procmaps::from_pid(guest.pid(), |map| {
            matches!(map.pathname, procmaps::MMapPath::Anonymous)
                && map.address.0 <= start
                && end <= map.address.1
        })?;
        let Some(mmap) = enclosing.into_iter().next() else {
            return Ok(());
        };
        let dettid = guest.thread_state().dettid;
        detlog!(
            "[memory][dtid {}] {}->{}",
            dettid,
            procmaps::display_range_as(&mmap, start, end, "[heap]"),
            procmaps::compute_hash_range(guest, start, end)?
        );
        Ok(())
    }
}

/// Render a finished syscall for the `finish syscall` DETLOG line.
///
/// Output pointers are dereferenced only when the kernel can have written
/// them. Linux leaves the output buffer of the syscalls listed in
/// [`failure_leaves_outputs_unwritten`] untouched when they fail with any errno
/// other than EFAULT, so rendering it would publish whatever the guest
/// happened to have there -- typically uninitialized stack -- as if it were a
/// result, and two otherwise identical runs would diverge on it
/// (<https://github.com/rrnewton/hermit/issues/3153>). For those syscalls such
/// a failure renders the pointer arguments without their pointees; the errno
/// is printed next to this rendering from the result itself.
///
/// EFAULT keeps the outputs rendered, because it is also the error of a copy
/// that faulted part way: `copy_to_user()` may already have stored a prefix of
/// the struct, which the guest can read and DETLOG must show. A tool error is
/// not a guest errno and proves nothing about the buffer, so it keeps them
/// rendered too.
///
/// Every other syscall keeps rendering its outputs on failure, because some
/// Linux syscalls do write an output on an error return (for example
/// `gettimeofday` stores `tv` before it faults on a bad `tz`). Hiding such an
/// output would remove real evidence from DETLOG.
fn display_syscall_finished<'a, M: MemoryAccess>(
    syscall: &'a Syscall,
    memory: &'a M,
    result: &Result<i64, Error>,
) -> reverie::syscalls::Display<'a, M, Syscall> {
    match syscall {
        Syscall::Fstat(_) => syscall.display(memory), //FIXME: T136880615 - fstat structure isn't fully deterministic yet
        _ if failure_proves_outputs_unwritten(result)
            && failure_leaves_outputs_unwritten(syscall) =>
        {
            syscall.display(memory)
        }
        _ => syscall.display_with_outputs(memory),
    }
}

/// Syscalls whose output buffer Linux does not write when the syscall fails
/// with an errno other than EFAULT, restricted to those whose output the pinned Reverie formatter dereferences.
///
/// This is deliberately a list of syscalls PROVEN not to write on failure,
/// rather than a list of exceptions that do: a syscall missing from it keeps
/// its outputs rendered, which can at worst show stale guest memory, whereas a
/// syscall wrongly missing from an exception list would silently hide a value
/// the kernel really wrote.
///
/// Kernel behaviour (Linux `fs/stat.c`, `kernel/time/posix-timers.c`):
///
/// - `stat`, `lstat`, `fstat`, `newfstatat`: `vfs_stat()`, `vfs_lstat()`,
///   `vfs_fstat()` and `vfs_fstatat()` return their error before
///   `cp_new_stat()` is called, so the `struct stat` is never copied out.
///   Hermit's `handle_stat_family` likewise returns the injected syscall's
///   error before it rewrites the buffer.
/// - `statx`: `do_statx()` returns the `vfs_statx()` error before
///   `cp_statx()` copies the `struct statx` out.
/// - `clock_gettime`: `put_timespec64()` runs only when
///   `kc->clock_get_timespec()` succeeded (`if (!error && put_timespec64(..))`),
///   and an unknown clock returns `-EINVAL` before that. Hermit's
///   `handle_clock_gettime` writes `tp` only on its success path.
///
/// In every case the one failure that follows a copy attempt is the `-EFAULT`
/// from the copy itself, where `copy_to_user()` may have stored a prefix of
/// the struct before faulting. [`failure_proves_outputs_unwritten`] therefore
/// keeps rendering EFAULT failures, so that partial prefix stays visible.
///
/// `gettimeofday` is intentionally absent: `SYSCALL_DEFINE2(gettimeofday)` in
/// `kernel/time/time.c` stores `tv` and then returns `-EFAULT` if copying
/// `tz` faults, so its rendered output can be kernel-written on failure.
fn failure_leaves_outputs_unwritten(syscall: &Syscall) -> bool {
    matches!(
        syscall,
        Syscall::Stat(_)
            | Syscall::Lstat(_)
            | Syscall::Fstat(_)
            | Syscall::Newfstatat(_)
            | Syscall::Statx(_)
            | Syscall::ClockGettime(_)
    )
}

/// Whether `result` is a failure that, for a syscall listed in
/// [`failure_leaves_outputs_unwritten`], proves Linux stored no output: a
/// guest errno other than EFAULT. EFAULT can follow a partial copy, and a
/// tool error is not the syscall's result.
fn failure_proves_outputs_unwritten(result: &Result<i64, Error>) -> bool {
    matches!(result, Err(Error::Errno(errno)) if *errno != Errno::EFAULT)
}

/// The happens-before request a syscall entry makes once its result is
/// decided, for the posthook anchors it reached at its prehook (`anchors`), or
/// `None` (<https://github.com/rrnewton/hermit/issues/3929>):
/// - a value or a guest errno other than an internal restart code is a
///   completion: Linux returns it unchanged, so the anchors fire
///   ([`ResourceID::HappensBeforeSyscallCheckpoint`]);
/// - an internal restart code (`ERESTARTSYS`, `ERESTARTNOINTR`,
///   `ERESTARTNOHAND`, `ERESTART_RESTARTBLOCK`) is refused
///   ([`ResourceID::HappensBeforePosthookInterrupted`]): the kernel decides
///   only at signal delivery, after this point, whether the guest sees `EINTR`
///   or the entry is restarted and never returns, so neither firing nor not
///   firing would keep the anchor's promise;
/// - a tool or backend failure is not a result the guest receives; nothing
///   fires, and the failure ends the run on its own path.
fn posthook_checkpoint(
    count: u64,
    anchors: Vec<String>,
    result: &Result<i64, Error>,
) -> Option<ResourceID> {
    if anchors.is_empty() {
        return None;
    }
    match result {
        Err(Error::Errno(
            errno @ (Errno::ERESTARTSYS
            | Errno::ERESTARTNOINTR
            | Errno::ERESTARTNOHAND
            | Errno::ERESTART_RESTARTBLOCK),
        )) => Some(ResourceID::HappensBeforePosthookInterrupted {
            count,
            anchors,
            code: errno
                .name()
                .map_or_else(|| errno.to_string(), str::to_owned),
        }),
        Ok(_) | Err(Error::Errno(_)) => {
            Some(ResourceID::HappensBeforeSyscallCheckpoint { count, anchors })
        }
        Err(_) => None,
    }
}

#[reverie::tool]
impl<T: RecordOrReplay> Tool for Detcore<T> {
    type GlobalState = GlobalState;
    type ThreadState = ThreadState<T::ThreadState>;

    /// Only a sequentialized run on a backend with process signal control
    /// can install that control. Whether this run observes dequeues is
    /// decided after installation by [`GlobalState`]'s
    /// `GlobalTool::observe_signal_dequeues`.
    fn may_observe_signal_dequeues(config: &Config) -> bool {
        config.backend.provides_process_signal_control && config.sequentialize_threads
    }

    /// The same runs require the control: real-timer signals on such a run
    /// travel only through acknowledged shared dequeues. Reverie refuses it
    /// before any guest code, with ENOSYS on a runner that offers no control,
    /// as Detcore's own installation did before this check moved into
    /// Reverie, and with EINVAL on a runner whose offer was not taken. So
    /// Detcore's scheduler needs no refusal of its own for a capable backend
    /// without a control.
    fn requires_signal_control(config: &Config) -> bool {
        config.backend.provides_process_signal_control && config.sequentialize_threads
    }

    async fn handle_signal_dequeue<G: Guest<Self>>(
        &self,
        guest: &mut G,
        dequeue: reverie::SignalDequeue,
    ) -> Result<(), Errno> {
        tool_global::signal_dequeued(guest, dequeue).await
    }

    /// Constructor for Detcore process-local state.
    fn new(pid: Pid, cfg: &Config) -> Self {
        let detpid = DetPid::from_raw(pid.into()); // TODO(T78538674): virtualize pid.
        cfg.validate_invariants();
        if let Some(policy) = &cfg.in_guest_detlog_forward_policy {
            // This image's forwarder was set up before the Tool (SaBRe's
            // connect, the LiteInst constructor); without one, or with another
            // policy, refuse.
            detlog::require_forwarding(policy);
        }
        if cfg.in_guest_site_patching_off {
            // Record the host's request; the preload constructor compares it
            // with the settings the runtime captured once installation
            // returns (see in_guest_site_patching).
            in_guest_site_patching::require_site_patching_off();
        }
        // Read before any guest syscall, so a readlink of another process's
        // pipe or socket never depends on the tracer's descriptors then. The
        // outcome is kept; a failure is reported only if a guest needs it.
        let _ = syscalls::anonymous_object_devices();
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
            //
            // `cpuid` is the one exception. With CPUID virtualization off the handler only
            // re-executes the host instruction, so trapping does not change what the guest
            // sees, but the subscription makes Reverie probe and enable CPUID faulting and log
            // an ERROR for each exec on hosts that lack it
            // (https://github.com/rrnewton/hermit/issues/3460). A backend that installs its
            // own CPUID table (KVM) still needs the trap for the guest to see host values.
            let mut subscription = Subscription::all_syscalls();
            subscription.rdtsc();
            if config.virtualize_cpuid || config.backend.virtualizes_cpuid {
                subscription.cpuid();
            }
            subscription
        } else {
            // Explicit performance opt-in: unlisted syscalls bypass Detcore entirely. Keep this
            // path separate so its allow-list can be tightened without weakening the default.
            let mut subscription = Subscription::none();
            subscription.syscalls([
                Sysno::write,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#547)
                Sysno::writev,
                // Timer-slack procfs virtualization must see every scalar and
                // vectored read/write form that Linux accepts for the file.
                Sysno::readv,
                Sysno::preadv,
                Sysno::preadv2,
                Sysno::pwritev,
                Sysno::pwritev2,
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
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-2150): Timer-slack prctl state is
                // virtual and must not bypass Detcore under passthru_opt.
                Sysno::prctl,
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
                // AUTONOMOUS-BOT-IMPLEMENTED
                // The scheduler must observe changes to the address used for
                // its modeled CHILD_CLEARTID wake, including record/replay's
                // passthrough optimization.
                Sysno::set_tid_address,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // Rare (once per thread) but load-bearing: without it the exit
                // hook cannot replay `exit_robust_list()` and robust-mutex
                // waiters are never woken.
                Sysno::set_robust_list,
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
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-862): Keep modeled pidfd creation intercepted.
                Sysno::pidfd_open,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-1175): Keep pidfd signal/get-fd
                // determinization from being bypassed under the passthru opt-in.
                Sysno::pidfd_send_signal,
                Sysno::pidfd_getfd,
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
                // `select` is included by the Determinized sweep below;
                // T137258824 tracks improving its implementation.
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

            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-978): Keep the passthru-opt allow-list in sync
            // with the complete Determinized classification AND with the
            // Unsupported set. Even under the performance opt-in, Detcore must
            // see every syscall that its audited policy says it models or
            // deterministically refuses. Otherwise record/replay can bypass a
            // Detcore handler and execute the syscall natively against the host.
            //
            // `Determinized` and `Unsupported` are disjoint classifications, so
            // the determinized filter alone leaves the Unsupported set
            // unsubscribed: record/replay would silently execute an unsupported
            // syscall on the live host instead of invalidating its determinism
            // claim. Both halves are required; neither implies the other.
            // NOTE: `all_pinned_syscalls()`, not `Sysno::iter()`. The latter
            // stops one short of the end of the table, which silently dropped
            // `lsm_list_modules` out of this sweep.
            subscription.syscalls(crate::all_pinned_syscalls().filter(|sysno| {
                crate::is_determinized_syscall(*sysno) || crate::is_unsupported_syscall(*sysno)
            }));

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
        if self.cfg.backend.virtualizes_guest_sigalrm && guest.thread_state().sigalrm_handled {
            self.sync_sigalrm_eligibility(guest).await;
        }
        self.pre_handler_hook(guest, false, CheckIn::InstructionTrap)
            .await;
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
            intercepted.cpuid(eax, ecx).unwrap_or_else(|| {
                warn!(
                    "[dtid {}] cpuid leaf 0x{:x} subleaf 0x{:x} not in deterministic table; returning zero result",
                    dettid, eax, ecx
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
        self.post_handler_hook(guest, CheckIn::InstructionTrap)
            .await;
        Ok(res)
    }

    async fn handle_rdtsc_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: Rdtsc,
    ) -> Result<RdtscResult, Errno> {
        trace!("handle_rdtsc_event: {:?}", request);
        if self.cfg.backend.virtualizes_guest_sigalrm && guest.thread_state().sigalrm_handled {
            self.sync_sigalrm_eligibility(guest).await;
        }
        self.pre_handler_hook(guest, false, CheckIn::InstructionTrap)
            .await;
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
            // The guest TSC must name the same instant as `clock_gettime`. Both
            // now read the coordinator's clock through the shared per-process
            // floor, so a guest comparing the two -- a clocksource watchdog, a
            // delay loop calibrated against a device timer, a second vCPU
            // reading the TSC -- sees one time base instead of two.
            //
            // The `add_rdtsc()` charge above is folded into global time by the
            // same RPC that reads it back (`send_and_update_time` updates the
            // coordinator before dispatching the request), so consecutive reads
            // from one thread still advance.
            let tsc = guest_clock_time(guest).await;
            Ok(RdtscResult {
                // We treat virtual cycles as equivalent to virtual nanoseconds.
                tsc: tsc.as_nanos(),
                aux: None,
            })
        } else {
            self.record_or_replay
                .handle_rdtsc_event(&mut guest.into_guest(), request)
                .await
        };
        self.post_handler_hook(guest, CheckIn::InstructionTrap)
            .await;
        result
    }

    // Note: we will not see SIGSTKFLT used for timers.
    async fn handle_signal_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        signal: Signal,
    ) -> Result<Option<Signal>, Errno> {
        let delivered = self
            .handle_inbound_signal(guest, SigWrapper::from(signal))
            .await?;
        Ok(delivered.map(|_| signal))
    }

    /// A realtime signal (32 to 64), which `Signal` cannot name: handled as
    /// any other (`handle_inbound_signal`), so it takes its turn in the
    /// schedule (<https://github.com/rrnewton/reverie/issues/207>).
    async fn handle_raw_signal_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        signal: i32,
    ) -> Result<Option<i32>, Errno> {
        let delivered = self
            .handle_inbound_signal(guest, SigWrapper(signal))
            .await?;
        Ok(delivered.map(|signal| signal.raw()))
    }

    /// A `SIGCHLD` the backend passes on without reporting it is dropped when
    /// it is the kernel's copy of a child-exit notification the scheduler
    /// sends itself (`scheduler::child_exit_sigchld`). A copy the scheduler
    /// sent is kept, with the siginfo the tracer gave it: this hook cannot
    /// replace it.
    async fn filter_unreported_signal<R: GlobalRPC<Self::GlobalState>>(
        &self,
        rpc: &R,
        unreported: reverie::UnreportedSignal,
    ) -> bool {
        if unreported.signal() != Signal::SIGCHLD
            || !self.cfg.backend.reports_child_exit_publication
        {
            return true;
        }
        let Some(info) = unreported.siginfo() else {
            return true;
        };
        let tid = DetTid::from_raw(unreported.tid().as_raw());
        // Passed on unreported, a kept copy makes no `InboundSignal` request.
        match scheduler::child_exit_sigchld::control_for(&info, std::process::id(), false) {
            None => true,
            Some(control) => {
                use scheduler::child_exit_sigchld::ChildExitSigchldAnswer;
                use scheduler::child_exit_sigchld::OwnCopy;
                // The control message carries no logical time; the scheduler answers
                // it before any clock accounting.
                let (_, response) = rpc
                    .send_rpc((
                        DetTime::default(),
                        MmId::initial(tid),
                        tool_global::GlobalRequest::ChildExitSigchld(control),
                    ))
                    .await;
                match response {
                    tool_global::GlobalResponse::ChildExitSigchld(
                        ChildExitSigchldAnswer::Claimed(dropped),
                    ) => !dropped,
                    tool_global::GlobalResponse::ChildExitSigchld(
                        ChildExitSigchldAnswer::OwnCopy(OwnCopy::Duplicate),
                    ) => false,
                    tool_global::GlobalResponse::ChildExitSigchld(
                        ChildExitSigchldAnswer::OwnCopy(OwnCopy::Deliver(own))
                        | ChildExitSigchldAnswer::ClaimedFor(own),
                    ) => {
                        warn!(
                            "[dtid {}] the SIGCHLD for child {} passed on unreported keeps its delivered siginfo",
                            tid, own.pid
                        );
                        true
                    }
                    _ => true,
                }
            }
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
                let (_next_parent_pedigree, child_pedigree) = pts.1.pedigree.fork();
                let child_logical_time = pts.1.thread_logical_time.clone_for_child();
                let last_accounted_user_time = child_logical_time.user_cpu_time();
                let last_accounted_system_time = child_logical_time.system_cpu_time();
                if !clone_flags.contains(CloneFlags::CLONE_THREAD) {
                    pts.1.prepare_child_process_cpu_time(dettid);
                }
                let guest_clock = Arc::clone(&pts.1.guest_clock);

                ThreadState {
                    dettid,
                    detpid: None, // Initialized later.
                    thread_start_entered: false,
                    physical_tid: None,
                    signal_task_identity: None,
                    open_file_creator: None,
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
                    pedigree: child_pedigree.clone(),
                    initialized_random_auxv: None,
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
                    tool_shares_guest_descriptor_table: pts.1.tool_shares_guest_descriptor_table,
                    // Linux copies the creating thread's current timer slack
                    // into both fields of every new task (thread or process).
                    timer_slack_ns: pts.1.timer_slack_ns,
                    default_timer_slack_ns: pts.1.timer_slack_ns,
                    // A new process created with CLONE_NEWPID is the init of
                    // a namespace below its creator's; everything else lives
                    // in its creator's.
                    nested_pid_namespace: pts.1.nested_pid_namespace
                        || (clone_flags.contains(CloneFlags::CLONE_NEWPID)
                            && !clone_flags.contains(CloneFlags::CLONE_THREAD)),
                    // A new task starts in its creator's root, and with
                    // CLONE_FS keeps sharing it.
                    changed_root: if clone_flags.contains(CloneFlags::CLONE_FS) {
                        Arc::clone(&pts.1.changed_root)
                    } else {
                        Arc::new(std::sync::atomic::AtomicBool::new(
                            pts.1.changed_root.load(std::sync::atomic::Ordering::SeqCst),
                        ))
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
                    // Wall time belongs to the traced process tree, not to an
                    // individual process. Forked processes and cloned threads
                    // therefore retain one monotonic view of raw logical time.
                    guest_clock,
                    parent_process_cpu_time: if clone_flags.contains(CloneFlags::CLONE_THREAD) {
                        pts.1.parent_process_cpu_time.clone()
                    } else {
                        Some(Arc::clone(&pts.1.process_cpu_time))
                    },
                    parent_cpu_publication: if clone_flags.contains(CloneFlags::CLONE_THREAD) {
                        pts.1.parent_cpu_publication.clone()
                    } else if self.cfg.backend.emulates_child_waits
                        && self.cfg.sequentialize_threads
                    {
                        Some(pts.1.prepare_child_cpu_publication(dettid))
                    } else {
                        None
                    },
                    // A new thread has made no syscall yet.
                    restart_block: None,
                    // A fork child inherits its parent's signal dispositions.
                    sigalrm_handled: pts.1.sigalrm_handled,
                    sigalrm_blocked_published: pts.1.sigalrm_blocked_published,
                    handler_restore_rt_reported: pts.1.handler_restore_rt_reported,
                    signal_register_projection: None,
                    last_accounted_user_time,
                    last_accounted_system_time,
                    thread_cpu_start_user_time: last_accounted_user_time,
                    thread_cpu_start_system_time: last_accounted_system_time,
                    clone_flags: None,
                    pending_vfork: pts.1.pending_vfork.clone(),

                    // Child RNG identity follows the deterministic creation
                    // pedigree, never the backend/host Tid. Guest-visible IDs
                    // and scheduler targeting continue to use `dettid`.
                    prng: tool_local::thread_rng_from_parent_pedigree(
                        "USER RAND",
                        &pts.1.prng,
                        &child_pedigree,
                        tool_local::ChildRngStream::User,
                    ),
                    chaos_prng: tool_local::thread_rng_from_parent_pedigree(
                        "CHAOSRAND",
                        &pts.1.chaos_prng,
                        &child_pedigree,
                        tool_local::ChildRngStream::Chaos,
                    ),

                    // For comparing progress to other threads, it is important that our
                    // child thread start at a sensible place, rather than starting back
                    // at zero:
                    thread_logical_time: child_logical_time,
                    // A new thread gets a new clock, so we've committed 0 ticks
                    committed_clock_value: 0,
                    // A new thread or process is never the backend runtime's
                    // bootstrapping thread, so it starts outside any window.
                    uncharged_bootstrap_syscalls: 0,
                    in_uncharged_bootstrap_syscall: false,
                    yield_after_futex_wake: false,
                    own_signal_turn_hold: None,

                    end_of_timeslice: None,
                    replay_rcb_end: None,
                    // AUTONOMOUS-BOT-IMPLEMENTED
                    // TODO-HUMAN-REVIEW(PR-1151)
                    chaos_epoch: tool_local::chaos_epoch_sentinel(),
                    chaos_slowdown_factor: RcbTimeMultiplier::ONE,
                    chaos_slowdown_active: false,
                    pending_chaos_epochs: Vec::new(),
                    max_timeslice_end: None,
                    last_grant_deadline: None,
                    last_rcb_timer: None,
                    last_rcb_timer_is_max: false,

                    record_or_replay,
                    preemption_points: None,

                    // We only get to the point of creating child threads if we're past the first execve.
                    past_global_first_execve: true,
                    interrupt_at: self.cfg.interrupts_for_thread(dettid),

                    // `copy_process()` sets `p->robust_list = NULL` for every
                    // new task, thread or process alike. The child re-registers
                    // its own head before it can own a robust futex.
                    robust_list_head: None,
                    robust_list_process: if clone_flags.contains(CloneFlags::CLONE_THREAD) {
                        Arc::clone(&pts.1.robust_list_process)
                    } else {
                        Arc::new(Mutex::new(tool_local::RobustListProcessState::default()))
                    },
                }
            }
        }
    }

    async fn handle_thread_start<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Error> {
        guest.thread_state_mut().thread_start_entered = true;
        let new_dettid = DetTid::from_raw(guest.tid().into()); // TODO(T78538674): virtualize pid/tid:
        assert_eq!(new_dettid, guest.thread_state().dettid);
        let detpid = select_thread_start_detpid(guest.thread_state().detpid, guest.pid());
        let is_root_thread = is_root_thread_start(guest.is_root_process(), new_dettid, detpid);
        trace!(
            "[tid {}] detcore handle_thread_start, pid={}",
            guest.tid(),
            detpid
        );

        // Delayed initialization of thread_state for this new thread:
        let thread_state = guest.thread_state_mut();
        thread_state.detpid = Some(detpid);
        if thread_state.recover_process_mm_id(detpid) {
            debug!(
                "[detcore, dtid {}] recovered process memory identity {} for unparented thread state",
                new_dettid, detpid
            );
        }

        if let Some(vfork) = guest.thread_state_mut().pending_vfork.take() {
            create_vfork_child_thread(guest, new_dettid, vfork).await;
        } else if is_root_thread {
            // There is no fork event to catch for the root thread.
            debug!(
                "[detcore, dtid {}] root thread start, scheduling.. full config:\n {:?}",
                &new_dettid,
                guest.config()
            );
            let physical_ids = if guest
                .config()
                .backend
                .requires_thread_directed_process_signals
            {
                Some((
                    guest.pid().as_raw(),
                    guest
                        .thread_state()
                        .physical_tid
                        .expect("backend requires a host thread ID before registration"),
                ))
            } else {
                None
            };
            if let Some(post_exec_mm) =
                create_child_thread(guest, new_dettid, 0, None, libc::SIGCHLD, physical_ids).await
            {
                guest.thread_state_mut().mm_id = post_exec_mm;
            }
        }

        // Except for the root task, let's block until it's our turn to go:
        let th = tool_global::thread_start_request(&self.cfg, guest, detpid).await;

        // Finish the delayed initialization of the full threadstate:
        {
            let ts = guest.thread_state_mut();
            ts.preemption_points = th.map(|x| x.into_iter());
            ts.next_timeslice(&self.cfg); // Must be after preemption_points is set.
        }
        // The start grant's deadline hint arrived before the first slice
        // existed; cap the slice now.
        let deadline = guest.thread_state_mut().last_grant_deadline.take();
        tool_global::cap_timeslice_at_deadline(guest, deadline);

        // The prehook is a noop for a thread just starting.  Can't end the timeslice.  There's no
        // RCB progress to record.  However, we call it for consistency with all the other handlers.
        self.pre_handler_hook(guest, true, CheckIn::Event).await;
        // ^ precise_branch=true: There should have been ZERO prior instructions before this,
        // because the thread hasn't done anything yet.

        self.record_or_replay
            .handle_thread_start(&mut guest.into_guest())
            .await?;

        self.post_handler_hook(guest, CheckIn::Event).await;
        Ok(())
    }

    async fn handle_post_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        // A nonleader exec preserves the survivor's state and PMU counter, but
        // Linux changes its TID. Bind that state before any image callback RPC.
        match tool_global::reconnect_exec(guest).await {
            Err(Errno::EOPNOTSUPP) => {
                let message = "unsupported: preemption recording and replay across nonleader exec";
                error!("{message}");
                // Report even when logging is disabled or redirected. The
                // ordinary refusal helper preserves the policy exit class;
                // reconnect already bound identity, so its diagnostic RPC is
                // authenticated and cannot silently retire an unbound owner.
                let _ = writeln!(crate::util::RetryingStderr, "{message}");
                tool_global::unrecoverable_shutdown(
                    guest,
                    detcore_model::HERMIT_POLICY_REFUSAL_EXIT,
                )
                .await;
            }
            result => result?,
        }
        // Exec gives the task filesystem information of its own
        // (`unshare_fs_struct`): a root it shared through CLONE_FS is now its
        // own, as it stands.
        let changed_root = guest
            .thread_state()
            .changed_root
            .load(std::sync::atomic::Ordering::SeqCst);
        guest.thread_state_mut().changed_root =
            Arc::new(std::sync::atomic::AtomicBool::new(changed_root));
        // The guest's first image is entered by an execve. A backend that traces
        // the guest from before it (ptrace) intercepts that execve and counts it
        // as the guest's first syscall. A backend that starts the guest already
        // executing its image (DBT under DynamoRIO, SaBRe) reaches this callback
        // with nothing counted, so under DBT, which then sees every later
        // syscall, numbers and virtual time ran exactly one syscall behind
        // ptrace's. Count that execve here, at its own cost. (SaBRe also misses
        // the dynamic loader's syscalls before its plugin starts; this closes
        // only the execve's share of that gap.)
        let thread = guest.thread_state_mut();
        if !thread.past_global_first_execve && thread.stats.syscall_count == 0 {
            thread.stats.count_syscall();
            thread
                .thread_logical_time
                .add_syscall_with_cost(syscall_time::cost_ns(Sysno::execve));
        }
        guest.thread_state_mut().past_global_first_execve = true;
        // exec resets a handled signal to its default action.
        guest.thread_state_mut().sigalrm_handled = false;
        guest.thread_state_mut().sigalrm_blocked_published = false;
        // Only a successful exec reaches this callback. Delete the old image's
        // POSIX timer IDs while exec still owns its scheduler turn; the global
        // notification below cancels their deadlines before the pre-handler
        // can yield. alarm()/ITIMER_REAL and the continuous clock survive exec.
        guest
            .thread_state()
            .posix_timers
            .lock()
            .unwrap()
            .clear_for_exec();
        // A successful exec clears the kernel's clear_child_tid registration.
        // Mirror that reset before any replacement-image syscall can run.
        if guest.config().sequentialize_threads {
            tool_global::set_child_tid_address(guest, 0).await;
        }
        // The exec walked the old thread group's robust lists; wake one waiter
        // per owner word it marked FUTEX_OWNER_DIED.
        let robust_wakes = guest.thread_state().take_robust_list_wakes_for_exec();
        let identities: Vec<_> = robust_wakes
            .iter()
            .map(|(owner, wake)| (*owner, wake.futex))
            .collect();
        let counts = robust_list_wakes_after_exec(guest, robust_wakes).await;
        for ((owner, futex), count) in identities.into_iter().zip(counts) {
            info!(
                "[detcore, dtid {}] robust-list owner death woke {} waiter(s) on futex {:?} at exec",
                owner, count, futex,
            );
        }

        tool_global::mark_past_first_execve(guest).await;
        // Linux discards every queued POSIX timer signal at exec
        // (`flush_itimer_signals`), and Hermit's are ordinary signals it sent:
        // take them back before the new image runs
        // (https://github.com/rrnewton/hermit/issues/4184).
        if scheduler::timer_copies_recorded() || scheduler::real_time_expiration_kept_pending() {
            self.flush_exec_timer_copies(guest).await;
        }
        self.pre_handler_hook(guest, false, CheckIn::Event).await;

        let auxv = guest.auxv();
        let initialized = guest
            .thread_state_mut()
            .complete_initial_random_auxv(auxv.at_random().map(|p| p.as_raw()))
            .expect("authenticated initial random handoff no longer matches this image");
        // A backend that wrote AT_RANDOM before this callback (the loader
        // handoff) emitted no record: emit it here from the bytes it wrote, with
        // no second draw or write, so the record sits where every backend puts
        // it, after the root thread's seeding records. The loader's getrandom
        // fills (glibc's early initialization makes one) and resource-limit
        // reads (glibc's startup reads RLIMIT_STACK) emitted none either: their
        // records follow, in order, as when this thread's own handlers serve
        // them after this callback.
        if let Some((bytes, early_requests)) = initialized {
            let dettid = guest.thread_state().dettid;
            random::record_initial_auxv(dettid, &bytes);
            for request in early_requests {
                match request {
                    random::EarlyRequest::Getrandom(fill) => {
                        random::record_early_getrandom(dettid, fill)
                    }
                    random::EarlyRequest::LimitRead {
                        call,
                        resource,
                        current,
                        maximum,
                    } => {
                        syscalls::sysinfo::record_early_limit_read(call, resource, current, maximum)
                    }
                }
            }
        } else if let Some(ptr) = auxv.at_random() {
            // It is safe to mutate this address since libc has not yet had a
            // chance to modify or copy the auxv table.
            let memory = guest.memory();
            let dettid = guest.thread_state().dettid;
            let ptr = unsafe { ptr.into_mut() };
            random::initialize_auxv(
                guest.thread_state_mut().thread_prng(),
                memory,
                ptr.cast(),
                dettid,
            )?;
        }

        // Successful exec never returns through handle_syscall_event, so the
        // nested recorder/replayer needs this callback to commit or retire its
        // pending exec state before the replacement image issues another exec.
        self.record_or_replay
            .handle_post_exec(&mut guest.into_guest())
            .await?;

        self.post_handler_hook(guest, CheckIn::Event).await;
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
                    // Under --max-log-bytes a full stderr must not stop the
                    // run before the cap; see `util::DroppingStderr`.
                    None if crate::nonwaiting_write::diagnostics_must_not_wait() => {
                        Box::new(crate::util::DroppingStderr)
                    }
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
        self.pre_handler_hook(guest, true, CheckIn::Event).await;
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
            } else if guest.thread_state().own_signal_turn_hold.is_none() {
                self.end_timeslice(guest).await;
            }
        }
        self.post_handler_hook(guest, CheckIn::Event).await;
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        // Signal phase 1: before this syscall's first scheduler request. A
        // delivery prepared here runs the guest's handler before the syscall,
        // which then starts again; what this returns is discarded.
        if self.cfg.backend.virtualizes_guest_sigalrm
            && guest.thread_state().sigalrm_handled
            && self.sigalrm_at_syscall_entry(guest).await
        {
            return Ok(0);
        }

        // A thread that keeps its turn for a signal it sent its own process
        // takes that signal before it can make another syscall.
        if let Some(signal) = guest.thread_state_mut().own_signal_turn_hold.take() {
            tracing::error!(
                "[detcore, dtid {}] reached a syscall without taking signal {} it sent to its own \
                 process (https://github.com/rrnewton/hermit/issues/4034)",
                guest.thread_state().dettid,
                signal
            );
        }
        self.pre_handler_hook(guest, false, CheckIn::Event).await;

        // Linux's restart of an interrupted wait is the thread's very next syscall,
        // so the record a wait kept for it lives only until then
        // (`handle_restart_syscall`).
        let restart_block = guest.thread_state_mut().restart_block.take();

        let dettid = guest.thread_state().dettid;

        if guest.thread_state().guest_past_first_execve() {
            detlog!(
                event = crate::detlog::DetLogEvent::Syscall;
                "[syscall][detcore, dtid {}] inbound syscall: {} = ?",
                dettid,
                call.display(&guest.memory())
            );
        }

        // The hot syscall path only reads a few Copy flags from the config, so copy
        // just those out instead of cloning the entire Config on every intercepted
        // syscall (previously flagged inline as an unnecessary copy). guest.config()
        // borrows guest immutably; bind the flags in a tight scope so the borrow ends
        // before the later thread_state_mut()/&mut guest borrows below.
        let (sequentialize_threads, virtualize_time, panic_on_unsupported_syscalls) = {
            let config = guest.config();
            (
                config.sequentialize_threads,
                config.virtualize_time,
                config.panic_on_unsupported_syscalls,
            )
        };

        if sequentialize_threads && self.cfg.should_trace_schedevent() {
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

        let syscall_cost_ns = syscall_time::cost_ns(call.number());
        // The backend-runtime bootstrap window. A backend-resident runtime (the
        // LiteInst preload constructor) issues hundreds to thousands of syscalls
        // on one thread (315 for a small C program, 6,970 for emacs; see
        // `syscall_time::MAX_UNCHARGED_BOOTSTRAP_SYSCALLS`) between its validated
        // begin trap and the trap that ends its bootstrap: the ready report, or
        // the report that preparation failed.
        // Reverie reports that window only for the bootstrapping thread; every
        // other thread and every forked process sees no window.
        //
        // What is withheld: the per-syscall cost below, for the bootstrapping
        // thread only, for each syscall it makes inside the window that does not
        // observe virtual time, up to
        // `syscall_time::MAX_UNCHARGED_BOOTSTRAP_SYSCALLS` per window. That
        // includes syscalls made by guest code the runtime calls into, such as
        // an interposed malloc or open64. Charging the runtime's own syscalls
        // would make guest-visible virtual time (uptime, CLOCK_MONOTONIC, CPU
        // time) depend on which backend ran the program. Every withheld syscall
        // is still counted and fully handled below, so Detcore keeps tracking
        // the fds, mappings and inodes it creates.
        //
        // What is always charged, inside the window too: syscalls that observe
        // virtual time (`syscall_time::observes_virtual_time`), so two clock
        // reads on the bootstrapping thread differ by at least one syscall cost
        // exactly as outside the window; and every syscall past the cap, so a
        // loop inside the window, or a runtime that never reports ready, again
        // advances the clock at each syscall.
        //
        // When it matters: with a syscall-driven clock (no PMU, or
        // --max-timeslice=disabled, which Hermit selects when perf counters are
        // unavailable, or --no-rcb-time) these charges are the thread's only
        // per-syscall progress, and with sequentialized threads the 500x no-RCB
        // multiplier makes each one large. With RCB time on a PMU host, retired
        // branches still advance the thread's clock inside the window and only
        // the syscall costs are withheld.
        //
        // The decision depends only on the window the backend reports (opened
        // and closed by trap instructions the runtime executes), the syscall
        // number and the count of earlier uncharged syscalls in the same
        // window: all functions of the guest's own execution. Record/replay and
        // both --verify runs therefore make the same decisions. The clock is
        // never reset: the first syscall after the window continues from the
        // value the window left. See https://github.com/rrnewton/hermit/issues/3338
        // and https://github.com/rrnewton/hermit/pull/3430#issuecomment-5928691696.
        //
        // The scheduler turns such a syscall needs are withheld the same way.
        // Handling an uncharged syscall can still commit scheduler turns (for
        // example the file resources of a read of /proc/self/maps), and each
        // committed turn normally advances global time by the per-turn
        // scheduler cost. While the syscall is being handled the thread is
        // marked `in_uncharged_bootstrap_syscall`, `tool_global::resource_request`
        // copies that mark into the request, and the scheduler does not
        // advance global time for a marked turn unless it is an IO-polling
        // retry, whose time enforces timeouts. Without this, a LiteInst run of
        // the clock-trajectory fixture reached its first clock read 11.5 ms of
        // virtual time later than with it, and each exec added 12.0 ms more
        // (measured locally), which was enough for its sysinfo uptime to
        // differ from the ptrace backend's on the hosted runner
        // (https://github.com/rrnewton/hermit/issues/3517). A charged syscall
        // (one that observes virtual time, or any past the cap) is not marked,
        // so its turns advance the clock as everywhere else.
        let in_backend_runtime_bootstrap = guest.is_backend_runtime_bootstrap();
        let new_count = {
            // which results from not being able to borrow guest twice.
            let thread_state = guest.thread_state_mut();
            thread_state.stats.count_syscall();

            // Every intercepted syscall advances logical time, including configurations that do
            // not serialize threads. This keeps virtual clocks productive during syscall loops.
            // The one exception is the bootstrap window described above: on the bootstrapping
            // thread, up to MAX_UNCHARGED_BOOTSTRAP_SYSCALLS syscalls per window that do not
            // observe virtual time are left uncharged.
            let charged =
                thread_state.charge_syscall_time(in_backend_runtime_bootstrap, call.number());
            if charged {
                thread_state
                    .thread_logical_time
                    .add_syscall_with_cost(syscall_cost_ns);
            }
            thread_state.in_uncharged_bootstrap_syscall = !charged;
            // This only folds the thread's new user and system time into the process total.
            // An uncharged syscall added none, so it needs no guard.
            thread_state.account_process_cpu_time();
            thread_state.stats.syscall_count
        };

        // Happens-before enforcement checkpoint. When the run carries a
        // happens-before program and an anchor at exactly
        // `Position::SyscallCount(new_count)` names this thread (by dettid, or by
        // a spawn ordinal only the scheduler can resolve), this syscall checks in
        // with the scheduler at its prehook, carrying this thread's running
        // syscall count.
        // The scheduler fires any anchor at that count on this thread and parks
        // the thread (out of the run queue) when that anchor is the AFTER
        // endpoint of a Hard edge whose BEFORE endpoint has not fired yet. For
        // any other (thread, count) the scheduler would find no anchor and do
        // nothing, so the request is skipped: one scheduler round trip per anchor
        // the thread reaches rather than per syscall
        // (https://github.com/rrnewton/hermit/issues/3877), and threads the spec
        // does not name keep their schedule.
        // This is the gate that makes an authored partial order reproduce a known
        // race deterministically (see detcore-model `happens_before`). It
        // requires sequentialized threads (enforced by the CLI) so the scheduler
        // owns ordering.
        //
        // A syscall-occurrence anchor ("the thread's Nth writev to fd 9") is
        // counted here, in this thread's own counters, once per syscall entry;
        // the thread checks in only when a call is some such anchor's nth
        // occurrence, naming the anchors it reached.
        let hb_checkpoint = if guest.config().happens_before.is_some() {
            let dettid = guest.thread_state().dettid;
            let mut counters = std::mem::take(&mut guest.thread_state_mut().stats.hb_occurrences);
            let program = guest
                .config()
                .happens_before
                .as_ref()
                .expect("checked above");
            // A posthook anchor this entry reaches checks in when the call
            // completes, below, not here
            // (https://github.com/rrnewton/hermit/issues/3929).
            let mut reached = if program.has_syscall_occurrence_anchors() {
                let (sysno, args) = call.into_parts();
                program.count_syscall_occurrences(
                    &mut counters,
                    dettid,
                    sysno,
                    [args.arg0, args.arg1],
                    new_count,
                )
            } else {
                Vec::new()
            };
            let posthook: Vec<String> = reached
                .extract_if(.., |name| {
                    program
                        .anchors
                        .get(name)
                        .is_some_and(|a| a.fires_at_posthook())
                })
                .collect();
            let checkpoint = if !reached.is_empty() {
                Some(ResourceID::HappensBeforeSyscallCheckpoint {
                    count: new_count,
                    anchors: reached,
                })
            } else if program.may_have_syscall_count_anchor_at(dettid, new_count) {
                Some(ResourceID::HappensBeforeCheckpoint(new_count))
            } else {
                None
            };
            guest.thread_state_mut().stats.hb_occurrences = counters;
            guest.thread_state_mut().stats.hb_pending_posthook = posthook;
            checkpoint
        } else {
            None
        };
        if let Some(checkpoint) = hb_checkpoint {
            let request = guest.thread_state().mk_request(checkpoint, Permission::R);
            resource_request(guest, request).await;
        }

        // For `hermit run --verify`: a name the guest itself rebinds is not a
        // host input change (see `detcore_model::host_input`). Reported before
        // the call runs, whatever its outcome, so that a change the kernel
        // commits is never missing from the record, even if the run ends
        // before this handler returns.
        if self.cfg.record_host_inputs {
            self.record_host_namespace_change(guest, &call).await;
            self.record_untraced_code_change(guest, &call).await;
        }

        // The inodes whose last name this call removes, stat'ed before it runs
        // and retired once it succeeds, so that a host reusing one cannot hand
        // a new file the old file's number
        // (https://github.com/rrnewton/hermit/issues/3840).
        let removed_inodes = self.last_names_removed_by(guest, &call).await;

        // Only an emulated RNG readv supplies authoritative imported geometry.
        // A generic pre-dispatch snapshot would become stale across pipe waits.
        let mut rng_readv_output = None;
        // Signal phase 1: a process that handles SIGALRM runs only the calls
        // its table places (`sigalrm_phase1`).
        let sigalrm_refusal = if guest.thread_state().sigalrm_handled {
            self.sigalrm_phase1_gate(guest, call)
        } else {
            None
        };
        // A procfs file Detcore renders answers, as Linux does, the calls
        // that would reach the placeholder standing in for it.
        let rendered_procfs_answer = if sigalrm_refusal.is_none() {
            match self.answer_rendered_procfs_operation(guest, &call) {
                Some(answer) => Some(answer),
                None => self
                    .answer_rendered_procfs_path_call(guest, &call)
                    .await
                    .transpose(),
            }
        } else {
            None
        };
        let res = match classify_syscall(call.number()) {
            _ if sigalrm_refusal.is_some() => Err(sigalrm_refusal.expect("checked").into()),
            _ if rendered_procfs_answer.is_some() => rendered_procfs_answer.expect("checked"),
            SyscallClassification::PassThrough if call.number() == Sysno::chroot => {
                let result = self.passthrough(guest, call).await;
                if result.is_ok() {
                    guest
                        .thread_state()
                        .changed_root
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                result
            }
            // Outbound TCP channels under a network trace mode; never with no mode.
            _ if self.network_trace_owns(guest, &call) => {
                self.handle_network_trace_syscall(guest, call).await
            }
            // Rseq is not type-safe in the pinned Reverie revision. Dispatch by Sysno so a
            // future typed representation preserves this explicit policy.
            SyscallClassification::Determinized if call.number() == Sysno::rseq => {
                if panic_on_unsupported_syscalls {
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
            // TODO-HUMAN-REVIEW(PR-1175): The pinned Reverie revision exposes
            // pidfd_send_signal/pidfd_getfd only as raw calls, so dispatch on the
            // Sysno. See the handlers in syscalls/files.rs for the determinism
            // argument.
            SyscallClassification::Determinized if call.number() == Sysno::pidfd_send_signal => {
                match call {
                    Syscall::Other(_, args) => {
                        self.handle_pidfd_send_signal(
                            guest,
                            call,
                            args.arg0 as RawFd,
                            args.arg1 as i32,
                            args.arg3 as u32,
                        )
                        .await
                    }
                    _ => unreachable!("pidfd_send_signal unexpectedly gained a typed variant"),
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1175): pidfd_getfd, likewise untyped.
            SyscallClassification::Determinized if call.number() == Sysno::pidfd_getfd => {
                match call {
                    Syscall::Other(_, args) => {
                        self.handle_pidfd_getfd(
                            guest,
                            call,
                            args.arg0 as RawFd,
                            args.arg1 as RawFd,
                            args.arg2 as u32,
                        )
                        .await
                    }
                    _ => unreachable!("pidfd_getfd unexpectedly gained a typed variant"),
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
            // TODO-HUMAN-REVIEW(PR-852): Review the futex2 fallback contract.
            // Detcore models legacy futex but not the newer vector/sized futex2
            // ABI. Match a kernel without futex2 so runtimes take their
            // established legacy-futex fallback without consulting the host.
            SyscallClassification::Determinized if is_futex2_enosys_syscall(call.number()) => {
                Err(Error::Errno(Errno::ENOSYS))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-836): Host filesystem and mount
            // introspection are outside the deterministic model. Return the
            // portable feature-absence errno so callers use /proc fallbacks.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-859): Extend this boundary to obsolete ustat
            // host-filesystem capacity counters.
            SyscallClassification::Determinized
                if is_mount_introspection_enosys_syscall(call.number()) =>
            {
                Err(Error::Errno(Errno::ENOSYS))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-848): Hide unmodeled shared keyrings and
            // request-key upcalls behind the portable CONFIG_KEYS-absent errno.
            // TODO-HUMAN-REVIEW(PR-916): Fail closed whenever the
            // panic-on-unsupported policy is active; ordinary runs select that
            // policy by default. The explicit compatibility opt-out keeps the
            // pre-848 host pass-through so the guest observes a real working
            // keyring, restoring the enabled rr `keyctl` compatibility test.
            // Under fail-closed execution the deterministic ENOSYS boundary is
            // preserved.
            SyscallClassification::Determinized if is_kernel_keyring_syscall(call.number()) => {
                if panic_on_unsupported_syscalls {
                    Err(Error::Errno(Errno::ENOSYS))
                } else {
                    self.passthrough(guest, call).await
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-855): Fail-closed runs cannot expose
            // unmodeled pipe-buffer ownership or vmsplice page pinning, but
            // preserve host pass-through under the explicit compatibility
            // opt-out, `hermit run --allow-unsupported-syscalls`. splice and tee
            // refuse with EINVAL, the errno Linux returns when a descriptor
            // pair cannot be spliced and the one callers fall back to
            // read/write on: GNU grep 3.12 drains a pipe into /dev/null with
            // splice and falls back on EINVAL, but on ENOSYS it reports a read
            // error and exits 2. vmsplice keeps ENOSYS.
            SyscallClassification::Determinized if is_zero_copy_pipe_syscall(call.number()) => {
                if panic_on_unsupported_syscalls {
                    if call.number() == Sysno::vmsplice {
                        Err(Error::Errno(Errno::ENOSYS))
                    } else {
                        Err(Error::Errno(Errno::EINVAL))
                    }
                } else {
                    self.passthrough(guest, call).await
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-860): Host LSM attributes are outside
            // Detcore's model. Present a stable feature-absence boundary
            // instead of forwarding probes.
            SyscallClassification::Determinized
                if is_host_security_identity_probe_syscall(call.number()) =>
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
            // TODO-HUMAN-REVIEW(PR-844): Enforce a deterministic boundary
            // around host-global process accounting and cross-process memory.
            SyscallClassification::Determinized
                if is_process_isolation_refused_syscall(call.number()) =>
            {
                Err(Error::Errno(Errno::EPERM))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-876): Guest performance events
            // expose host PMU availability, policy, and asynchronous counter
            // state that Detcore does not model. A fixed ENOSYS preserves the
            // portable feature-probe fallback without creating an untracked fd.
            SyscallClassification::Determinized if is_perf_event_enosys_syscall(call.number()) => {
                Err(Error::Errno(Errno::ENOSYS))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-853): Refuse nested tracing, host-object
            // comparison at the deterministic boundary.
            SyscallClassification::Determinized
                if is_privileged_observation_refused_syscall(call.number()) =>
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
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-859): Include System V semaphore and shared-
            // memory objects in the existing CONFIG_SYSVIPC refusal boundary.
            SyscallClassification::Determinized
                if is_unsupported_async_ipc_syscall(call.number()) =>
            {
                Err(Error::Errno(Errno::ENOSYS))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-882): Legacy nonlinear page
            // remapping has host-dependent kernel support and VMA behavior that
            // Detcore does not model. Preserve the documented mmap fallback.
            SyscallClassification::Determinized
                if is_remap_file_pages_enosys_syscall(call.number()) =>
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
            // TODO-HUMAN-REVIEW(#1851): The file-ownership mutation family
            // (chown/fchown/fchownat/lchown) completes the fixed virtual-root
            // identity that the credential query (#1549) and credential set
            // (#787) families already implement. A real root process's chown
            // succeeds for any uid, so 0 is the value the virtual identity must
            // observe; forwarding instead returned the errno of whatever host
            // identity the backend happened to run under (EPERM with no user
            // namespace, EINVAL for an unmapped uid inside a one-uid map, and
            // backend-dependent for in-process backends).
            //
            // The emulation covers the IDENTITY half only. Root privilege
            // waives the ownership permission check; it does not waive pathname,
            // descriptor, or flag errors, so handle_ownership_change_noop
            // translates the target arguments into a side-effect-free metadata
            // lookup and returns 0 only if that validation succeeds. ENOENT,
            // EBADF, EFAULT, ENOTDIR and the fchownat flag EINVAL therefore
            // still reach the guest; the host-identity-dependent EPERM/EINVAL
            // cannot be produced at all. No setattr is attempted, so host
            // ownership, mode bits, and timestamps are never modified, and
            // Detcore does not model per-file ownership, so the success is not
            // observable through a later stat -- see
            // is_ownership_change_noop_syscall and handle_ownership_change_noop
            // for the full boundary, and
            // hermit-cli/tests/chown_virtual_root_identity.rs for the bracket
            // that fails if this arm's RESULT regresses.
            SyscallClassification::Determinized
                if is_ownership_change_noop_syscall(call.number()) =>
            {
                self.handle_ownership_change_noop(guest, call).await
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#827): Deterministic ENOSYS for the Landlock
            // unprivileged-sandbox syscalls (landlock_create_ruleset,
            // landlock_add_rule, landlock_restrict_self). Landlock availability
            // and ABI version depend on the host kernel build
            // (CONFIG_SECURITY_LANDLOCK) and runtime LSM stacking, so forwarding
            // them (the legacy pass-through) is host-dependent and, because a
            // ruleset restricts the whole thread tree, a global-state isolation
            // hole. A fixed -ENOSYS is the errno a kernel built without Landlock
            // returns, so the guest sees a consistent "sandbox unavailable"
            // answer regardless of host; never forwarded to the host and
            // bitwise-identical across --verify and record/replay. Untyped
            // (Syscall::Other) in the pinned Reverie, so dispatch on the Sysno
            // before the typed match below.
            SyscallClassification::Determinized if is_landlock_sandbox_syscall(call.number()) => {
                Err(Error::Errno(Errno::ENOSYS))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-847): Refuse unmodeled host-kernel probes
            // with a fixed ENOSYS so guest behavior does not depend on BPF/LSM
            // configuration or mutable page-cache state.
            SyscallClassification::Determinized if is_host_kernel_probe_syscall(call.number()) => {
                Err(Error::Errno(Errno::ENOSYS))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-838): Review close_range descriptor-table
            // synchronization. The pinned Reverie exposes close_range as a raw
            // call, so dispatch by Sysno before the typed match.
            SyscallClassification::Determinized if call.number() == Sysno::close_range => {
                self.handle_close_range(guest, call).await
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-839): Optional modern memory APIs vary with
            // host kernel configuration, CET support, and pidfd lifecycle.
            // Present the portable feature-absence result instead.
            SyscallClassification::Determinized
                if is_optional_memory_feature_syscall(call.number()) =>
            {
                Err(Error::Errno(Errno::ENOSYS))
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
                Syscall::Read(s) if self.sock_diag_reply_fd(guest, s.fd()) => {
                    self.handle_sock_diag_read(guest, s).await
                }
                Syscall::Read(s) => self.handle_read(guest, s).await,
                Syscall::Pread64(s) => self.handle_pread64(guest, s).await,
                Syscall::Lseek(s) => self.handle_lseek(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-838): Review regular-file sendfile mediation.
                Syscall::Sendfile(s) => self.handle_sendfile(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-887): Present a stable pre-4.5-kernel
                // boundary so callers use determinized read/write copying.
                Syscall::CopyFileRange(_) => Err(Error::Errno(Errno::ENOSYS)),
                // TODO-HUMAN-REVIEW(#794): vectored scatter/gather I/O, mirroring
                // read/pread64/pwrite64/writev.
                Syscall::Readv(s) if self.sock_diag_reply_fd(guest, s.fd()) => {
                    self.handle_sock_diag_readv(guest, s).await
                }
                Syscall::Readv(s) => {
                    self.handle_readv_with_output(guest, s, &mut rng_readv_output)
                        .await
                }
                Syscall::Preadv(s) => {
                    self.handle_preadv_with_output(guest, s, &mut rng_readv_output)
                        .await
                }
                Syscall::Preadv2(s) => {
                    self.handle_preadv2_with_output(guest, s, &mut rng_readv_output)
                        .await
                }
                Syscall::Pwritev(s) => self.handle_pwritev(guest, s).await,
                Syscall::Pwritev2(s) => self.handle_pwritev2(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#683)
                Syscall::Pwrite64(s) => self.handle_pwrite64(guest, s).await,
                // This syscall is advisory; fixed success preserves its API contract.
                Syscall::Fadvise64(_) => Ok(0),
                Syscall::Mmap(s) => self.handle_mmap(guest, s).await,
                Syscall::Madvise(s) => self.handle_madvise(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#775)
                Syscall::Mincore(s) => self.handle_mincore(guest, s).await,
                Syscall::Munmap(s) => self.handle_munmap(guest, s).await,
                Syscall::Mremap(s) => self.handle_mremap(guest, s).await,
                Syscall::Stat(s) => self.handle_stat_family(guest, s.into()).await,
                Syscall::Lstat(s) => self.handle_stat_family(guest, s.into()).await,
                Syscall::Fstat(s) => self.handle_stat_family(guest, s.into()).await,
                Syscall::Newfstatat(s) => self.handle_stat_family(guest, s.into()).await,
                Syscall::Statx(s) => self.handle_statx(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#877)
                Syscall::Readlink(s) => self.handle_readlink(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                Syscall::Readlinkat(s) => self.handle_readlinkat(guest, s).await,
                Syscall::Fcntl(s) => self.handle_fcntl(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-912)
                Syscall::Ioctl(s)
                    if syscalls::socket_timestamp_ioctl::is_socket_timestamp_ioctl(s) =>
                {
                    self.handle_socket_timestamp_ioctl(guest, s).await
                }
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

                Syscall::Setpgid(s) => self.handle_setpgid(guest, s).await,
                Syscall::Setsid(s) => self.handle_setsid(guest, s).await,
                // Without virtual time the guest reads the host clock. Each of these
                // must have a record/replay handler: a recording captures the value
                // the guest observed, and replay returns it without reading the host.
                Syscall::Gettimeofday(s) => {
                    if virtualize_time {
                        self.handle_gettimeofday(guest, s).await
                    } else {
                        self.passthrough(guest, call).await
                    }
                }
                Syscall::Time(s) => {
                    if virtualize_time {
                        self.handle_time(guest, s).await
                    } else {
                        self.passthrough(guest, call).await
                    }
                }
                Syscall::ClockGettime(s) => {
                    if virtualize_time {
                        self.handle_clock_gettime(guest, s).await
                    } else {
                        self.passthrough(guest, call).await
                    }
                }
                Syscall::ClockGetres(s) => {
                    if virtualize_time {
                        self.handle_clock_getres(guest, s).await
                    } else {
                        self.passthrough(guest, call).await
                    }
                }
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::ClockSettime(_) => Err(Error::Errno(Errno::EPERM)),
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-892)
                Syscall::Getitimer(s) => self.handle_getitimer(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#663)
                Syscall::Setitimer(s) => self.handle_setitimer(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-857): Virtual NTP query and fixed mutation refusal.
                Syscall::Adjtimex(s) => {
                    if virtualize_time {
                        self.handle_adjtimex(guest, s).await
                    } else {
                        self.handle_unsupported_syscall(
                            guest,
                            call,
                            dettid,
                            panic_on_unsupported_syscalls,
                        )
                        .await
                    }
                }
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-857): Clock-id form of virtual NTP query.
                Syscall::ClockAdjtime(s) => {
                    if virtualize_time {
                        self.handle_clock_adjtime(guest, s).await
                    } else {
                        self.handle_unsupported_syscall(
                            guest,
                            call,
                            dettid,
                            panic_on_unsupported_syscalls,
                        )
                        .await
                    }
                }
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-857): Empty virtual kernel ring buffer.
                Syscall::Syslog(s) => self.handle_syslog(guest, s).await,
                Syscall::ArchPrctl(s) => self.handle_arch_prctl(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                Syscall::Seccomp(s) => self.handle_seccomp(guest, s).await,
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
                Syscall::Dup2(w) => self.handle_dup2(guest, w).await,
                Syscall::Dup3(w) => self.handle_dup3(guest, w).await,
                Syscall::Pipe(w) => self.handle_pipe2(guest, w.into()).await,
                Syscall::Pipe2(w) => self.handle_pipe2(guest, w).await,
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
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-862): Record/replay and register pidfds.
                Syscall::PidfdOpen(s) => self.handle_pidfd_open(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-899): Host object handles and mount IDs
                // are outside Detcore's filesystem identity model.
                Syscall::NameToHandleAt(_) => Err(Error::Errno(Errno::EOPNOTSUPP)),
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

                // ===== BATCH 51: fail-closed utility syscalls, re-enabling chrt,
                // ionice, and flock under --strict. Detcore replaces the Linux
                // scheduler, exposes a single virtual CPU, and serializes guest
                // threads, so a thread's Linux scheduling attributes (sched_getattr)
                // and I/O priority (ioprio_set) are inert: those two have no
                // deterministic effect and are emulated to fixed, host-independent
                // results (see syscall_classification.rs).
                //
                // flock is NOT in that inert group and is not emulated. This comment
                // used to claim it "is never contended inside the serialized
                // container"; that was measured false -- two open file descriptions
                // in ONE process both held the same LOCK_EX under the old no-op,
                // where native Linux excluded the second -- and the no-op was
                // removed. Serializing THREADS does not make a whole-file lock
                // uncontended, because flock conflicts are between OPEN FILE
                // DESCRIPTIONS. flock is now forwarded to the kernel, like fcntl's
                // POSIX record locks; handle_flock carries the determinism argument
                // and the one case Detcore refuses (a contended BLOCKING request,
                // which it cannot park a thread on deterministically).
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#791)
                Syscall::SchedGetattr(s) => self.handle_sched_getattr(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-841): Review virtual sched_setattr no-op policy.
                Syscall::SchedSetattr(s) => self.handle_sched_setattr(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#791)
                Syscall::IoprioSet(s) => self.handle_ioprio_set(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-881): Review virtual ioprio_get defaults.
                Syscall::IoprioGet(s) => self.handle_ioprio_get(guest, s).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#2373)
                Syscall::Flock(s) => self.handle_flock(guest, s).await,

                // TODO-HUMAN-REVIEW(PR-1064): recvfrom/read/readv/recvmmsg reach
                // a NETLINK_SOCK_DIAG dump exactly as recvmsg does. Until they
                // were routed through the same sanitizer, four of the five
                // usable receive syscalls returned raw host socket inode
                // numbers, which made the determinization optional from the
                // guest's point of view: `socket.recv()` alone was enough to
                // skip it. Non-socket-diag descriptors take the same path as
                // before; the predicate is checked inside.
                Syscall::Recvfrom(s) if self.sock_diag_reply_fd(guest, s.fd()) => {
                    self.handle_sock_diag_recvfrom(guest, s).await
                }
                Syscall::Recvfrom(s) => self.handle_socket_receive(guest, s, s.fd(), true).await,
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-901)
                Syscall::Recvmsg(s) => self.handle_recvmsg(guest, s).await,
                Syscall::Sendto(s) => self.handle_sendrecv(guest, s).await,
                Syscall::Sendmsg(s) => self.handle_sendmsg(guest, s).await,
                Syscall::Sendmmsg(s) => self.handle_sendmmsg(guest, s).await,

                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#788): recvmmsg is the multi-message form of
                // recvmsg and shares its NonblockableSyscall impl. The fd is made
                // temporarily nonblocking, the kernel fills the mmsghdr array
                // atomically, and the Detcore scheduler owns any blocking, so the
                // timeout argument (deliberately ignored, see helpers.rs) does not
                // introduce nondeterminism.
                // TODO-HUMAN-REVIEW(PR-901): Review batched ancillary timestamp rewriting.
                Syscall::Recvmmsg(s) if self.sock_diag_reply_fd(guest, s.fd()) => {
                    self.handle_sock_diag_recvmmsg(guest, s).await
                }
                Syscall::Recvmmsg(s) => self.handle_recvmmsg(guest, s).await,
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

                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(#1549): Credential-query
                // family emulated to the fixed virtual-root identity (0). See
                // syscall_classification.rs for the determinism rationale.
                // getuid/geteuid/getgid/getegid return the constant directly;
                // getresuid/getresgid write the constant to each provided result
                // pointer. Never forwarded to the host, so the answer no longer
                // depends on whether the backend runs the guest in a
                // CLONE_NEWUSER namespace.
                Syscall::Getuid(_)
                | Syscall::Geteuid(_)
                | Syscall::Getgid(_)
                | Syscall::Getegid(_) => Ok(0),
                Syscall::Getresuid(s) => self.handle_getresuid(guest, s).await,
                Syscall::Getresgid(s) => self.handle_getresgid(guest, s).await,
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

                // POSIX per-process timers use the virtual clock and scheduler
                // for deterministic arming and supported signal delivery.
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
                        panic_on_unsupported_syscalls,
                    )
                    .await
                }
            },
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-2985): Review scheduler tracking of set_tid_address.
            // Linux and the backend own the pass-through call. Detcore also
            // mirrors its successful registration into the scheduler because
            // the scheduler supplies the logical CHILD_CLEARTID wake.
            SyscallClassification::PassThrough if call.number() == Sysno::set_tid_address => {
                match call {
                    Syscall::SetTidAddress(s) => self.handle_set_tid_address(guest, s).await,
                    _ => unreachable!("set_tid_address unexpectedly lost its typed variant"),
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-2223): Review observing
            // the robust-list registration without changing its pass-through
            // classification. This is still a pass-through — Linux owns the
            // registration and supplies its result — but Detcore remembers the
            // head address so thread exit can replay `exit_robust_list()`
            // against its own futex waiter pool.
            SyscallClassification::PassThrough if call.number() == Sysno::set_robust_list => {
                match call {
                    Syscall::SetRobustList(s) => self.handle_set_robust_list(guest, s).await,
                    _ => self.passthrough(guest, call).await,
                }
            }
            // faccessat2 and fchmodat2 are untyped in the pinned Reverie revision; the
            // reviewed classification table routes them, and every other reviewed
            // PassThrough syscall, through the blanket arm below.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-644): Keep dispatch aligned with the reviewed classification.
            // A pass-through whose restored mask may deliver a pending real-time
            // timer expiration (https://github.com/rrnewton/hermit/issues/4089).
            SyscallClassification::PassThrough if call.number() == Sysno::rt_sigreturn => {
                self.observe_sigreturn_mask(guest).await;
                self.passthrough(guest, call).await
            }
            SyscallClassification::PassThrough => self.passthrough(guest, call).await,
            // The kernel's restart of a wait Detcore emulates resumes it; any other
            // `restart_syscall` keeps the unsupported-syscall policy below.
            // AUTONOMOUS-BOT-IMPLEMENTED
            SyscallClassification::Unsupported if call.number() == Sysno::restart_syscall => {
                self.handle_restart_syscall(
                    guest,
                    call,
                    restart_block,
                    dettid,
                    panic_on_unsupported_syscalls,
                )
                .await
            }
            SyscallClassification::Unsupported => {
                self.handle_unsupported_syscall(guest, call, dettid, panic_on_unsupported_syscalls)
                    .await
            }
        };

        // A copy may already have changed guest memory before reporting a
        // terminal backend error. Do not perform even the syscall-result
        // display's memory reads, or later observers/posthooks/timer effects.
        // Random-device reads have completed their release RPC before returning;
        // getrandom acquired no file resource. Physical cleanup belongs to the
        // backend failure owner, not to this observer fence.
        if res.as_ref().is_err_and(crate::random::is_copy_failure) {
            return res;
        }

        if res.is_ok() && !removed_inodes.is_empty() {
            self.retire_removed_inodes(guest, removed_inodes).await;
        }

        // With asynchronous exit completion, a descriptor that would make this
        // guest serve another guest's exit ends the run before anything
        // observes the result (crate::exit_dependencies).
        if let Ok(ret) = res {
            self.refuse_exit_dependencies(guest, &call, ret).await;
        }

        detlog!(
            event = crate::detlog::DetLogEvent::SyscallResult {
                finished_syscall_number: new_count,
            };
            "[syscall][detcore, dtid {}] finish syscall #{}: {} = {:?}",
            dettid,
            new_count,
            display_syscall_finished(&call, &guest.memory(), &res),
            res
        );

        // Same guest-logical-control point that already anchors the stack/heap hashes: the
        // syscall is complete and its result written back, so the guest logically has control.
        // Reading registers is itself backend work, so keep the disabled path inert.  In
        // particular, a run that does not request register evidence must not be perturbed by
        // collecting data that will immediately be discarded.
        if self.cfg.detlog_regs {
            let control_point_regs = guest.regs().await;
            let regs_seq = guest.thread_state().stats.syscall_count;
            self.detlog_registers(guest, &control_point_regs, regs_seq);
        }

        // brk is PassThrough, so nothing else records where the guest's heap is.
        // Both brk(NULL) and brk(addr) return the break in effect afterwards.
        if let Syscall::Brk(_) = &call
            && let Ok(brk) = res
            && brk > 0
        {
            guest
                .thread_state()
                .memory_metadata
                .lock()
                .expect("memory metadata mutex poisoned")
                .observe_brk(brk as u64);
        }

        if let Err(error) = self.detlog_memory_maps(guest) {
            let res = Err(error);
            self.publish_posthook_checkpoint(guest, new_count, &res)
                .await;
            return self.complete_syscall(guest, res).await;
        }
        // Same control point again, for the bytes this syscall moved through a guest buffer.
        // Unlike the two mapping hashes above, the extent comes from the syscall's OWN
        // arguments, so it does not matter whether the buffer lives on the stack, in the brk
        // heap, in BSS or in an anonymous mmap -- the last two of which neither mapping hash
        // can see. Only successful calls moved anything.
        if let Ok(ret) = &res
            && self.cfg.detlog_io_buffers
            && let Err(error) = io_buffers::detlog_io_buffers(
                guest,
                &call,
                *ret,
                dettid,
                rng_readv_output.as_deref(),
            )
        {
            let res = Err(error);
            self.publish_posthook_checkpoint(guest, new_count, &res)
                .await;
            return self.complete_syscall(guest, res).await;
        }

        if sequentialize_threads && self.cfg.should_trace_schedevent() {
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

        // Posthook happens-before anchors this call reached fire now that its
        // result is decided and before the guest resumes
        // (https://github.com/rrnewton/hermit/issues/3929).
        self.publish_posthook_checkpoint(guest, new_count, &res)
            .await;

        // The syscall is finished; a turn the post-hook takes (a timeslice
        // end) is the thread's own and advances global time as usual.
        guest.thread_state_mut().in_uncharged_bootstrap_syscall = false;
        self.post_handler_hook(guest, CheckIn::Event).await;

        // Defense-in-depth: unless the backend already owns this guarantee,
        // force the syscall-clobbered registers (%rcx/%r11 on x86-64) to
        // deterministic values before returning to the guest.
        if !self.cfg.backend.virtualizes_syscall_clobbers {
            self.canonicalize_syscall_clobbers(guest).await;
        }

        // Signal phase 1, step I4: a due SIGALRM entry is delivered here.
        self.complete_syscall(guest, res).await
    }

    async fn on_exit_thread<G: GlobalRPC<Self::GlobalState>>(
        &self,
        tid: Tid,
        global_state: &G,
        mut thread_state: Self::ThreadState,
        exit_status: ExitStatus,
    ) -> Result<(), Error> {
        let dettid = thread_state.dettid;
        // Cancellation can consume the backend's transferred state before the
        // successful-exec callback has rebound its logical identity.
        let current = DetTid::from_raw(tid.as_raw());
        let transferred_exec = current != dettid;
        debug!(
            "[detcore, dtid {}] thread exit hook, deregistering from scheduler.",
            dettid
        );
        // Close the final in-progress timeslice so this thread contributes its
        // last (partial) slice to the run report, even if it never exhausted a
        // full slice.
        let now = thread_state.thread_logical_time.as_nanos();
        thread_state.stats.close_final_timeslice(now);
        // Ptrace keeps its exit event stopped while this hook runs. KVM runs
        // this owner hook after worker joins and child waitability publication.
        // Publish final CPU in this synchronous prefix: serial KVM waitid waits
        // for that owned notification before consuming, not for the hook tail.
        // DETERMINISTIC RECOVERY (TODO-HUMAN-REVIEW(PR-1147)). `ThreadState::detpid`
        // is `Option` and starts as `None` ("Initialized later" at the clone site),
        // so a thread that reaches the exit hook before its per-thread identity is
        // populated used to `.expect()` here. That panic fires inside a Reverie
        // teardown callback, while the backend still owns the exit event, which is
        // the worst place to abort: it can wedge the supervisor rather than fail one
        // thread. Fall back to the PROCESS-level `self.detpid`, which is
        // non-optional and preserves the identity source selected for this
        // backend: virtual for the current DBT ABI, physical for ABI v1. Warn so
        // the exceptional window is observable instead of silently papered over.
        //
        // CORRECTED BY #2348, and stated rather than quietly dropped: the normal
        // current-ABI DBT path pre-populates `thread_state.detpid` with the
        // client-published virtual process identity, so thread start and exit
        // agree on that value. This recovery is only for a thread that exits
        // before that initialization. In that exceptional window thread start
        // would fall back to `guest.pid()` (the physical host pid for `DbtGuest`),
        // while this exit path uses `self.detpid`. ABI v1 also intentionally keeps
        // physical callback identities. The warning makes either compatibility
        // or recovery path observable; this comment does not claim those fallback
        // identities are virtual or independently deterministic.
        let (detpid, used_process_detpid) =
            select_thread_exit_detpid(thread_state.detpid, self.detpid);
        if used_process_detpid {
            tracing::warn!(
                "[detcore, dtid {}] thread exited before its per-thread detpid was \
                 initialized; falling back to the process detpid {}",
                dettid,
                self.detpid
            );
        }
        // The transferred survivor is the final leader even if cancellation
        // precedes local rebinding. Its snapshot includes the worker's tail;
        // the earlier displaced-leader snapshot is only a prefix.
        if current == detpid {
            if !thread_state.record_exited_child_process_cpu_time(detpid) {
                return Err(Error::Tool(anyhow::anyhow!(
                    "child CPU publication ownership changed before final exit"
                )));
            }
        } else {
            thread_state.account_process_cpu_time();
        }
        let mm_id = thread_state.mm_id;
        let exit_signal = match &exit_status {
            ExitStatus::Signaled(signal, _) => Some(*signal as i32),
            ExitStatus::SignaledRaw(signal, _) => Some(*signal),
            ExitStatus::Exited(_) => None,
        };
        // Publish each owner's final clock under its own RPC identity before
        // counting that owner in the complete physical-exit barrier. Otherwise
        // the group's maximum could be charged to whichever callback finishes
        // last, followed by a backwards update from its real deregistration.
        // Exec preparation already cleared the old image's robust list. An
        // unbound transfer must authenticate its consuming cleanup before any
        // ordinary RPC can publish the survivor's clock under the leader TID.
        let exit_time_accounted = !transferred_exec
            && (!thread_state.has_matching_robust_list_exit(exit_signal)
                || acknowledge_robust_list_exit_time(
                    thread_state.thread_logical_time.clone(),
                    global_state,
                    mm_id,
                )
                .await);
        if !exit_time_accounted {
            // Preserve benign cleanup for a retired incarnation without
            // acknowledging an unaccounted owner or releasing its staged wakes.
            thread_state.record_robust_list_head(None);
        }
        if exit_time_accounted
            && let Some((_group_time, ready)) = thread_state.take_robust_list_wakes_after_exit(
                exit_signal,
                thread_state.thread_logical_time.clone(),
            )
        {
            let identities: Vec<_> = ready
                .iter()
                .map(|(owner, wake)| (*owner, wake.futex))
                .collect();
            let counts = robust_list_wakes_after_exit(
                thread_state.thread_logical_time.clone(),
                global_state,
                mm_id,
                ready,
            )
            .await;
            for ((owner, futex), count) in identities.into_iter().zip(counts) {
                info!(
                    "[detcore, dtid {}] robust-list owner death woke {} waiter(s) on futex {:?} after physical exit",
                    owner, count, futex,
                );
            }
        }
        let pending_chaos_epochs = thread_state.take_pending_chaos_epochs();
        let deregistration = ThreadDeregistration {
            dettid,
            detpid,
            mm: mm_id,
            thread_start_entered: thread_state.thread_start_entered,
            timeslice_stats: thread_state.stats.timeslice_stats,
            syscall_count: thread_state.stats.syscall_count,
            chaos_epochs: pending_chaos_epochs,
        };
        if transferred_exec {
            tool_global::retire_exec(
                thread_state.thread_logical_time.clone(),
                global_state,
                deregistration,
                exit_status.signal().is_some(),
            )
            .await?;
        } else {
            deregister_thread(
                thread_state.thread_logical_time.clone(),
                &self.cfg,
                global_state,
                deregistration,
            )
            .await;
        }

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

    /// The last row of the pinned table is the one a `Sysno::iter()` sweep
    /// drops. `lsm_list_modules` is Determinized AND deterministically refused,
    /// so before this was fixed it executed natively against the host under
    /// `--passthru-opt` — which is the default for `hermit record` and
    /// `hermit replay` — instead of receiving its fixed refusal.
    #[test]
    fn passthru_opt_covers_the_final_row_of_the_pinned_table() {
        let last = Sysno::last();
        // Guard the premise: if the table endpoint moves, this test must be
        // re-derived rather than silently passing on a different syscall.
        assert_eq!(last, Sysno::lsm_list_modules);
        assert!(crate::is_determinized_syscall(last));
        assert!(crate::is_deterministically_refused_syscall(last));
        // The bug this pins: the final row is absent from `Sysno::iter()`.
        assert!(!Sysno::iter().any(|sysno| sysno == last));

        let subscriptions = <Detcore as Tool>::subscriptions(&strict_config(true));
        assert!(
            subscriptions.iter_syscalls().any(|sysno| sysno == last),
            "{last} must be intercepted under passthru_opt; it is deterministically refused"
        );
    }

    #[test]
    fn passthru_opt_intercepts_every_unsupported_syscall() {
        let subscriptions = <Detcore as Tool>::subscriptions(&strict_config(true));
        let unsupported: Vec<Sysno> = crate::all_pinned_syscalls()
            .filter(|sysno| crate::is_unsupported_syscall(*sysno))
            .collect();

        assert_eq!(unsupported, [Sysno::restart_syscall]);
        for syscall in unsupported {
            assert!(
                subscriptions
                    .iter_syscalls()
                    .any(|subscribed| subscribed == syscall),
                "passthru_opt allowed unsupported {syscall} to bypass Detcore"
            );
        }
    }

    /// `passthru_opt` is not a niche flag: `record_or_replay_config` turns it on
    /// for every `hermit record` / `hermit replay`, so this covers the record
    /// and replay subscription too.
    #[test]
    fn passthru_opt_subscribes_every_determinized_syscall() {
        let determinized: Vec<Sysno> = crate::all_pinned_syscalls()
            .filter(|sysno| crate::is_determinized_syscall(*sysno))
            .collect();
        let subscriptions = <Detcore as Tool>::subscriptions(&strict_config(true));
        let delivered: Vec<Sysno> = subscriptions.iter_syscalls().collect();
        let missing = determinized
            .iter()
            .filter(|sysno| !delivered.contains(sysno))
            .copied()
            .collect::<Vec<_>>();

        assert!(
            missing.is_empty(),
            "passthru_opt let Determinized syscalls bypass Detcore: {}",
            missing
                .iter()
                .map(|sysno| sysno.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(
            delivered.contains(&Sysno::syslog),
            "syslog must reach its deterministic Detcore handler"
        );
    }

    #[test]
    fn passthru_opt_intercepts_thread_exit_registrations() {
        let subscriptions = <Detcore as Tool>::subscriptions(&strict_config(true));
        for syscall in [Sysno::set_tid_address, Sysno::set_robust_list] {
            assert!(
                subscriptions
                    .iter_syscalls()
                    .any(|subscribed| subscribed == syscall),
                "passthru_opt allowed {syscall} to bypass Detcore's thread-exit state"
            );
        }
    }

    #[test]
    fn passthru_opt_leaves_unlisted_passthrough_syscalls_unsubscribed() {
        assert_eq!(
            syscall_classification::classify_syscall(Sysno::chdir),
            syscall_classification::SyscallClassification::PassThrough
        );

        let subscriptions = <Detcore as Tool>::subscriptions(&strict_config(true));
        assert!(
            !subscriptions
                .iter_syscalls()
                .any(|sysno| sysno == Sysno::chdir),
            "chdir is PassThrough and must remain outside the partial subscription"
        );
    }

    /// Io-buffer hashing can only hash a syscall Detcore is subscribed to.
    /// Under `--passthru-opt` the subscription narrows, so the check's coverage
    /// narrows with it -- silently, because a syscall that never reaches
    /// Detcore produces no record and no record is indistinguishable from a
    /// syscall that moved no bytes.
    ///
    /// Measured 2026-08-21 on `f05bf04e4f`, one probe calling `getcwd`,
    /// `recvmsg`, `readv` and `readlink`:
    ///
    /// ```text
    /// hermit --log=info run                --detlog-io-buffers -- ./probe   12 [iobuf] records
    /// hermit --log=info run --passthru-opt --detlog-io-buffers -- ./probe   11 [iobuf] records
    /// ```
    ///
    /// The single lost syscall is `getcwd`, and the reason is structural: 9 of
    /// the 20 are in the literal allow-list and 10 more arrive via the
    /// unconditional Determinized sweep at the end of `subscriptions`, but
    /// `getcwd` is classified `PassThrough`, so neither path picks it up.
    ///
    /// WHY THIS IS A TEST AND NOT A RUNTIME WARNING. The set is stable, so a
    /// warning would print the same sentence on every `--passthru-opt` run
    /// forever and be tuned out within a week. The exposure is not today's
    /// one-syscall gap; it is that 19 of 20 holds only because two
    /// INDEPENDENTLY MAINTAINED lists happen to agree -- the classification
    /// table in `syscall_classification.rs` and the match arms in
    /// `io_buffers.rs`. Reclassifying any one of those ten from `Determinized`
    /// to `PassThrough` would drop it out of the sweep and out of io-buffers'
    /// reach with nothing failing. This asserts the relationship so that edit
    /// cannot land quietly.
    ///
    /// It is deliberately an EQUALITY, not a subset check, so it fails in both
    /// directions: a nineteenth syscall going missing, and `getcwd` becoming
    /// covered while this expectation still claims it is not.
    #[test]
    fn passthru_opt_leaves_io_buffer_hashing_blind_only_for_getcwd() {
        let subscribed: Vec<Sysno> = <Detcore as Tool>::subscriptions(&strict_config(true))
            .iter_syscalls()
            .collect();
        let unreachable: Vec<Sysno> = crate::io_buffers::HASHED_SYSCALLS
            .iter()
            .copied()
            .filter(|sysno| !subscribed.contains(sysno))
            .collect();

        assert_eq!(
            unreachable,
            vec![Sysno::getcwd],
            "--passthru-opt changes which syscalls io-buffer hashing can reach, and the set \
             moved. {} of {} reachable. If a syscall was RECLASSIFIED out of Determinized, that \
             silently shrank an enabled determinism check -- re-derive rather than editing this \
             expectation to match.",
            crate::io_buffers::HASHED_SYSCALLS.len() - unreachable.len(),
            crate::io_buffers::HASHED_SYSCALLS.len()
        );
    }

    /// The other direction, and the reason the check above is worth having:
    /// with the default subscription every buffer-carrying syscall is
    /// reachable, so there is nothing to report on an ordinary run.
    #[test]
    fn the_default_subscription_reaches_every_io_buffer_syscall() {
        let subscriptions = <Detcore as Tool>::subscriptions(&strict_config(false));
        let unreachable: Vec<Sysno> = crate::io_buffers::HASHED_SYSCALLS
            .iter()
            .copied()
            .filter(|sysno| !subscriptions.iter_syscalls().any(|s| s == *sysno))
            .collect();

        assert!(
            unreachable.is_empty(),
            "without --passthru-opt every io-buffer syscall must be reachable; missing {unreachable:?}"
        );
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

        // On a backend without its own CPUID table (ptrace), `--no-virtualize-cpuid`
        // must not subscribe to `cpuid` in either mode. The
        // subscription is what makes Reverie probe and enable CPUID faulting, and
        // a host without faulting then logs an ERROR for each traced exec
        // (https://github.com/rrnewton/hermit/issues/3460). Everything else stays
        // as intercepted as it is with CPUID virtualization on.
        for passthru_opt in [false, true] {
            let virtualized = <Detcore as Tool>::subscriptions(&strict_config(passthru_opt));
            let config = Config {
                virtualize_cpuid: false,
                ..strict_config(passthru_opt)
            };
            let subscriptions = <Detcore as Tool>::subscriptions(&config);

            assert!(virtualized.has_cpuid(), "passthru_opt={passthru_opt}");
            assert!(!subscriptions.has_cpuid(), "passthru_opt={passthru_opt}");
            assert!(subscriptions.has_rdtsc(), "passthru_opt={passthru_opt}");
            assert!(
                subscriptions
                    .iter_syscalls()
                    .eq(virtualized.iter_syscalls()),
                "passthru_opt={passthru_opt}: the syscall set must not depend on CPUID virtualization"
            );
        }

        // KVM installs its own CPUID table, so with virtualization off the guest
        // sees host values only through the trap: keep subscribing there.
        let kvm_host_cpuid = Config {
            virtualize_cpuid: false,
            ..strict_config(false).with_backend(|backend| backend.virtualizes_cpuid = true)
        };
        assert_eq!(
            <Detcore as Tool>::subscriptions(&kvm_host_cpuid),
            Subscription::all()
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
        for sysno in [
            Sysno::read,
            Sysno::write,
            Sysno::pread64,
            Sysno::pwrite64,
            Sysno::readv,
            Sysno::writev,
            Sysno::preadv,
            Sysno::preadv2,
            Sysno::pwritev,
            Sysno::pwritev2,
            Sysno::prctl,
        ] {
            assert!(
                subscriptions
                    .iter_syscalls()
                    .any(|subscribed| subscribed == sysno),
                "timer-slack mediation requires {sysno:?}"
            );
        }
        assert!(
            subscriptions
                .iter_syscalls()
                .any(|sysno| sysno == Sysno::pwrite64)
        );
        assert!(
            subscriptions
                .iter_syscalls()
                .any(|sysno| sysno == Sysno::pidfd_open)
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

    use super::rcb_timer_overshot;
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
        let _ = reverie::take_skid_overshoot_count();
        let error = Arc::new(Mutex::new(None));
        with_default(ErrorSubscriber(error.clone()), || {
            report_rcb_overshoot(false, 16_249, 139, 100);
        });

        let error = error.lock().unwrap().take().expect("missing ERROR event");
        assert!(error.contains(reverie::SKID_OVERSHOOT_MARKER), "{error}");
        assert!(error.contains("PMU RCB overshoot"), "{error}");
        assert!(error.contains("16249"), "{error}");
        assert!(error.contains("139"), "{error}");
        assert!(error.contains("100"), "{error}");
        assert_eq!(
            reverie::take_skid_overshoot_count(),
            1,
            "the log-and-continue path must feed the supervisor's structural count"
        );
    }

    #[test]
    fn exact_rcb_timer_hit_is_not_an_overshoot() {
        assert!(!rcb_timer_overshot(100, 100));
        assert!(!rcb_timer_overshot(99, 100));
        assert!(rcb_timer_overshot(101, 100));
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

#[cfg(test)]
mod process_tree_guest_clock_tests {
    use super::*;

    #[test]
    fn forked_process_shares_guest_clock_domain() {
        let config = Config::default();
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
        let mut parent = ThreadState::new(DetPid::from_raw(1), &config, ());
        parent.clone_flags = Some(CloneFlags::empty());

        let child = <Detcore as Tool>::init_thread_state(
            &tool,
            Tid::from_raw(2),
            Some((Tid::from_raw(1), &parent)),
        );

        assert!(Arc::ptr_eq(&parent.guest_clock, &child.guest_clock));
    }
}

#[cfg(test)]
mod child_rng_identity_tests {
    use super::*;

    fn child_state(
        tool: &Detcore,
        parent: &mut ThreadState<()>,
        host_tid: i32,
        clone_flags: CloneFlags,
    ) -> ThreadState<()> {
        parent.clone_flags = Some(clone_flags);
        <Detcore as Tool>::init_thread_state(
            tool,
            Tid::from_raw(host_tid),
            Some((Tid::from_raw(parent.dettid.as_raw()), parent)),
        )
    }

    fn rng_sample(child: &ThreadState<()>) -> [u64; 4] {
        let mut rng = child.prng.clone();
        std::array::from_fn(|_| rng.random())
    }

    fn chaos_rng_sample(child: &ThreadState<()>) -> [u64; 4] {
        let mut rng = child.chaos_prng.clone();
        std::array::from_fn(|_| rng.random())
    }

    #[test]
    fn common_child_rng_identity_ignores_backend_tid_for_every_clone_shape() {
        let config = Config::default();
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
        for (label, clone_flags) in [
            ("fork", CloneFlags::empty()),
            ("vfork", CloneFlags::CLONE_VM | CloneFlags::CLONE_VFORK),
            ("clone-process", CloneFlags::CLONE_VM),
            (
                "clone-thread",
                CloneFlags::CLONE_VM | CloneFlags::CLONE_THREAD,
            ),
        ] {
            let mut low_tid_parent = ThreadState::new(DetPid::from_raw(1), &config, ());
            let mut high_tid_parent = ThreadState::new(DetPid::from_raw(1), &config, ());
            let low_tid_child = child_state(&tool, &mut low_tid_parent, 2, clone_flags);
            let high_tid_child = child_state(&tool, &mut high_tid_parent, 42_002, clone_flags);

            assert_eq!(format!("{}", low_tid_child.pedigree), "C", "{label}");
            assert_eq!(low_tid_child.dettid.as_raw(), 2, "{label}");
            assert_eq!(high_tid_child.dettid.as_raw(), 42_002, "{label}");
            assert_eq!(
                rng_sample(&low_tid_child),
                rng_sample(&high_tid_child),
                "{label} child RNG depended on the backend Tid"
            );
            assert_eq!(
                chaos_rng_sample(&low_tid_child),
                chaos_rng_sample(&high_tid_child),
                "{label} child chaos RNG depended on the backend Tid"
            );
            assert_ne!(
                rng_sample(&low_tid_child),
                chaos_rng_sample(&low_tid_child),
                "{label} child guest and chaos RNG streams were coupled"
            );
        }
    }

    #[test]
    fn serialized_siblings_receive_distinct_child_rng_streams() {
        let config = Config::default();
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
        let mut parent = ThreadState::new(DetPid::from_raw(1), &config, ());

        let first = child_state(&tool, &mut parent, 2, CloneFlags::empty());
        let first_pedigree = parent.pedigree.fork_mut();
        assert_eq!(format!("{}", first.pedigree), format!("{first_pedigree}"));
        let second = child_state(&tool, &mut parent, 3, CloneFlags::empty());

        assert_eq!(format!("{}", second.pedigree), "PC");
        assert_ne!(rng_sample(&first), rng_sample(&second));
    }
}

#[cfg(test)]
mod thread_exit_identity_tests {
    use super::*;

    #[test]
    fn initialized_thread_exit_identity_is_preserved() {
        let thread_detpid = DetPid::from_raw(41);
        let process_detpid = DetPid::from_raw(7);
        assert_eq!(
            select_thread_exit_detpid(Some(thread_detpid), process_detpid),
            (thread_detpid, false)
        );
    }

    #[test]
    fn missing_thread_exit_identity_uses_process_identity() {
        let process_detpid = DetPid::from_raw(7);
        assert_eq!(
            select_thread_exit_detpid(None, process_detpid),
            (process_detpid, true)
        );
    }
}

#[cfg(test)]
mod completed_syscall_projection_tests {
    use super::*;

    /// Signal phase 1: the signal pipeline at a syscall's completion reads
    /// the result in rax, rip after the two-byte syscall instruction, and the
    /// instruction's clobbers; every other register is the entry's.
    #[test]
    fn a_completed_syscall_shows_its_result_and_return_address() {
        let mut entry: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        entry.rip = 0x40_1000;
        entry.rax = libc::SYS_pause as u64;
        entry.rcx = 0x1111;
        entry.r11 = 0x2222;
        entry.eflags = 0x246;
        entry.rdi = 7;
        entry.rsp = 0x7ffd_0000;
        let completed = completed_syscall_projection(entry, -i64::from(libc::EINTR));
        assert_eq!(completed.rax, (-i64::from(libc::EINTR)) as u64);
        assert_eq!(completed.rip, 0x40_1002);
        assert_eq!(completed.rcx, 0x40_1002);
        assert_eq!(completed.r11, 0x246);
        assert_eq!(completed.rdi, 7);
        assert_eq!(completed.rsp, 0x7ffd_0000);
        assert_eq!(completed.eflags, 0x246);
    }
}

#[cfg(test)]
mod thread_start_identity_tests {
    use super::*;

    #[test]
    fn backend_process_identity_is_preserved() {
        let backend_detpid = DetPid::from_raw(3);
        assert_eq!(
            select_thread_start_detpid(Some(backend_detpid), Pid::from_raw(42_001)),
            backend_detpid
        );
        assert_eq!(
            select_thread_start_detpid(None, Pid::from_raw(42_001)),
            DetPid::from_raw(42_001)
        );
    }

    #[test]
    fn root_thread_uses_deterministic_process_identity() {
        let detpid = DetPid::from_raw(3);
        assert!(is_root_thread_start(true, DetTid::from_raw(3), detpid));
        assert!(!is_root_thread_start(false, DetTid::from_raw(3), detpid));
        assert!(!is_root_thread_start(true, DetTid::from_raw(4), detpid));
    }
}

#[cfg(test)]
mod thread_cpu_time_tests {
    use super::*;

    #[test]
    fn cloned_threads_and_processes_keep_absolute_time_without_inheriting_work() {
        for clone_flags in [
            CloneFlags::CLONE_THREAD,
            CloneFlags::empty(),
            CloneFlags::CLONE_VFORK | CloneFlags::CLONE_VM,
        ] {
            let config = Config::default();
            let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
            let mut parent = ThreadState::new(DetPid::from_raw(1), &config, ());
            parent.thread_logical_time.add_rcbs(7);
            parent.thread_logical_time.add_syscall_with_cost(101);
            parent.clone_flags = Some(clone_flags);
            let child = <Detcore as Tool>::init_thread_state(
                &tool,
                Tid::from_raw(2),
                Some((Tid::from_raw(1), &parent)),
            );
            assert_eq!(
                child.thread_logical_time.as_nanos(),
                parent.thread_logical_time.as_nanos()
            );
            assert_eq!(
                child.thread_logical_time.inherited_nanos(),
                LogicalTime::from_nanos(171)
            );
            let mut global = GlobalTime::new(&config);
            let epoch = global.as_nanos();
            for thread in [&parent, &child] {
                global.update_global_time(
                    thread.dettid,
                    thread.thread_logical_time.as_nanos(),
                    thread.thread_logical_time.inherited_nanos(),
                );
            }
            assert_eq!(global.as_nanos(), epoch + LogicalTime::from_nanos(171));
        }
    }

    #[test]
    fn cloned_thread_and_fork_child_start_with_zero_thread_cpu() {
        for clone_flags in [CloneFlags::CLONE_THREAD, CloneFlags::empty()] {
            let config = Config::default();
            let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
            let mut parent = ThreadState::new(DetPid::from_raw(1), &config, ());

            // Give the parent nonzero CPU before the child exists. The child's
            // absolute logical clock inherits this position for scheduler
            // ordering, but Linux per-thread CPU accounting must not.
            parent.thread_logical_time.add_rcbs(200);
            parent.thread_logical_time.add_syscall();
            parent.clone_flags = Some(clone_flags);

            let mut child = <Detcore as Tool>::init_thread_state(
                &tool,
                Tid::from_raw(2),
                Some((Tid::from_raw(1), &parent)),
            );
            assert_eq!(
                child.thread_cpu_time(),
                (LogicalTime::ZERO, LogicalTime::ZERO),
                "clone flags {clone_flags:?} inherited pre-creation CPU"
            );

            child.thread_logical_time.add_rcbs(4);
            child.thread_logical_time.add_syscall();
            let (user, system) = child.thread_cpu_time();
            assert!(user > LogicalTime::ZERO);
            assert!(system > LogicalTime::ZERO);
            assert!(user < parent.thread_logical_time.user_cpu_time());
            assert!(system <= parent.thread_logical_time.system_cpu_time());
        }
    }
}

/// Regression tests for <https://github.com/rrnewton/hermit/issues/3153>: a
/// failed syscall must not render an output buffer the kernel never wrote.
///
/// Each buffer is pre-filled with sentinel values standing in for the
/// uninitialized guest stack the issue observed, and the rendering is checked
/// through the same `finish syscall` format the DETLOG line uses.
#[cfg(test)]
mod finished_syscall_display_tests {
    use std::ffi::CString;

    use reverie::syscalls::AddrMut;
    use reverie::syscalls::AtFlags;
    use reverie::syscalls::ClockGettime;
    use reverie::syscalls::ClockId;
    use reverie::syscalls::Gettimeofday;
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::Newfstatat;
    use reverie::syscalls::PathPtr;
    use reverie::syscalls::StatPtr;
    use reverie::syscalls::Statx;
    use reverie::syscalls::StatxMask;
    use reverie::syscalls::StatxPtr;
    use reverie::syscalls::Timespec;
    use reverie::syscalls::TimespecMutPtr;
    use reverie::syscalls::Timeval;
    use reverie::syscalls::TimevalMutPtr;

    use super::*;

    /// A value no real `st_size` or timestamp in these tests can take.
    const SENTINEL: i64 = 0x5EED_0BAD_F00D;

    /// Render exactly what the `finish syscall` DETLOG line renders.
    fn finish_line(syscall: &Syscall, result: Result<i64, Error>) -> String {
        let memory = LocalMemory::new();
        format!(
            "{} = {:?}",
            display_syscall_finished(syscall, &memory, &result),
            result
        )
    }

    fn sentinel_stat() -> libc::stat {
        // SAFETY: all-zero is a valid `struct stat`.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        stat.st_mode = libc::S_IFREG | 0o644;
        stat.st_size = SENTINEL;
        stat
    }

    fn sentinel_statx() -> libc::statx {
        // SAFETY: all-zero is a valid `struct statx`.
        let mut statx: libc::statx = unsafe { std::mem::zeroed() };
        statx.stx_mode = (libc::S_IFREG | 0o644) as u16;
        statx.stx_size = SENTINEL as u64;
        statx
    }

    fn newfstatat(path: &CString, stat: &libc::stat) -> Syscall {
        Syscall::Newfstatat(
            Newfstatat::new()
                .with_dirfd(libc::AT_FDCWD)
                .with_path(PathPtr::from_ptr(path.as_ptr()))
                .with_stat(StatPtr::from_ptr(stat as *const libc::stat))
                .with_flags(AtFlags::empty()),
        )
    }

    fn statx(path: &CString, statx: &libc::statx) -> Syscall {
        Syscall::Statx(
            Statx::new()
                .with_dirfd(libc::AT_FDCWD)
                .with_path(PathPtr::from_ptr(path.as_ptr()))
                .with_flags(AtFlags::empty())
                .with_mask(StatxMask::STATX_BASIC_STATS)
                .with_statx(StatxPtr::from_ptr(statx as *const libc::statx)),
        )
    }

    fn assert_no_struct_rendered(line: &str) {
        assert!(!line.contains("st_mode"), "rendered st_mode: {line}");
        assert!(!line.contains("st_size"), "rendered st_size: {line}");
        assert!(
            !line.contains(&SENTINEL.to_string()),
            "rendered the unwritten sentinel: {line}"
        );
    }

    #[test]
    fn failed_newfstatat_renders_pointer_and_errno_but_not_the_buffer() {
        let path = CString::new("/nonexistent/issue-3153").unwrap();
        let stat = sentinel_stat();
        let line = finish_line(&newfstatat(&path, &stat), Err(Errno::ENOENT.into()));

        assert_no_struct_rendered(&line);
        assert!(
            line.contains(&format!("{:p}", &stat as *const libc::stat)),
            "stat pointer missing: {line}"
        );
        assert!(
            line.contains("\"/nonexistent/issue-3153\""),
            "path missing: {line}"
        );
        assert!(line.contains("ENOENT"), "errno missing: {line}");
    }

    #[test]
    fn successful_newfstatat_still_renders_the_buffer() {
        let path = CString::new("/dev/null").unwrap();
        let stat = sentinel_stat();
        let line = finish_line(&newfstatat(&path, &stat), Ok(0));

        assert!(
            line.contains(&format!(
                "{:p} -> {{st_mode=SFlag(S_IFREG) | 0644, st_size={}, ...}}",
                &stat as *const libc::stat, SENTINEL
            )),
            "stat struct missing: {line}"
        );
    }

    #[test]
    fn failed_statx_renders_pointer_and_errno_but_not_the_buffer() {
        let path = CString::new("/nonexistent/issue-3153").unwrap();
        let buf = sentinel_statx();
        let line = finish_line(&statx(&path, &buf), Err(Errno::ENOENT.into()));

        assert_no_struct_rendered(&line);
        assert!(
            line.contains(&format!("{:p}", &buf as *const libc::statx)),
            "statx pointer missing: {line}"
        );
        assert!(line.contains("ENOENT"), "errno missing: {line}");
    }

    #[test]
    fn successful_statx_still_renders_the_buffer() {
        let path = CString::new("/dev/null").unwrap();
        let buf = sentinel_statx();
        let line = finish_line(&statx(&path, &buf), Ok(0));

        assert!(
            line.contains(&format!(
                "{:p} -> {{st_mode=SFlag(S_IFREG) | 0644, st_size={}, ...}}",
                &buf as *const libc::statx, SENTINEL
            )),
            "statx struct missing: {line}"
        );
    }

    #[test]
    fn failed_clock_gettime_does_not_render_the_timespec() {
        let tp = Timespec {
            tv_sec: SENTINEL,
            tv_nsec: 0,
        };
        let call = Syscall::ClockGettime(
            ClockGettime::new()
                .with_clockid(ClockId::CLOCK_MONOTONIC)
                .with_tp(Some(TimespecMutPtr(
                    AddrMut::from_ptr(&tp as *const Timespec).unwrap(),
                ))),
        );

        let failed = finish_line(&call, Err(Errno::EINVAL.into()));
        assert!(!failed.contains("tv_sec"), "rendered tv_sec: {failed}");
        assert!(
            failed.contains(&format!("{:p}", &tp as *const Timespec)),
            "tp pointer missing: {failed}"
        );
        assert!(failed.contains("EINVAL"), "errno missing: {failed}");

        let succeeded = finish_line(&call, Ok(0));
        assert!(
            succeeded.contains(&format!("tv_sec: {SENTINEL}")),
            "timespec missing on success: {succeeded}"
        );
    }

    /// EFAULT is also the error of a copy-out that faulted part way, after
    /// `copy_to_user()` stored a prefix the guest can read, so it must keep
    /// the buffer rendered for every allowlisted syscall.
    #[test]
    fn efault_failures_still_render_a_possible_partial_copy() {
        let path = CString::new("/dev/null").unwrap();
        let stat = sentinel_stat();
        let line = finish_line(&newfstatat(&path, &stat), Err(Errno::EFAULT.into()));
        assert!(
            line.contains(&format!(
                "{:p} -> {{st_mode=SFlag(S_IFREG) | 0644, st_size={}, ...}}",
                &stat as *const libc::stat, SENTINEL
            )),
            "partial stat copy hidden on EFAULT: {line}"
        );
        assert!(line.contains("EFAULT"), "errno missing: {line}");

        let buf = sentinel_statx();
        let line = finish_line(&statx(&path, &buf), Err(Errno::EFAULT.into()));
        assert!(
            line.contains(&format!(
                "{:p} -> {{st_mode=SFlag(S_IFREG) | 0644, st_size={}, ...}}",
                &buf as *const libc::statx, SENTINEL
            )),
            "partial statx copy hidden on EFAULT: {line}"
        );

        let tp = Timespec {
            tv_sec: SENTINEL,
            tv_nsec: 0,
        };
        let call = Syscall::ClockGettime(
            ClockGettime::new()
                .with_clockid(ClockId::CLOCK_MONOTONIC)
                .with_tp(Some(TimespecMutPtr(
                    AddrMut::from_ptr(&tp as *const Timespec).unwrap(),
                ))),
        );
        let line = finish_line(&call, Err(Errno::EFAULT.into()));
        assert!(
            line.contains(&format!("tv_sec: {SENTINEL}")),
            "partial timespec copy hidden on EFAULT: {line}"
        );
    }

    /// A posthook anchor fires on a value or a final guest errno, `EINTR`
    /// included; an internal restart code is refused; a tool error and an
    /// entry with no posthook anchor make no request
    /// (https://github.com/rrnewton/hermit/issues/3929).
    #[test]
    fn a_posthook_checkpoint_fires_only_on_a_final_result() {
        let anchors = || vec!["p".to_owned()];
        let fires = |result: Result<i64, Error>| {
            matches!(
                posthook_checkpoint(9, anchors(), &result),
                Some(ResourceID::HappensBeforeSyscallCheckpoint { count: 9, anchors })
                    if anchors == ["p"]
            )
        };
        assert!(fires(Ok(0)));
        assert!(fires(Ok(6)));
        for errno in [Errno::EINTR, Errno::EAGAIN, Errno::EBADF, Errno::EFAULT] {
            assert!(fires(Err(errno.into())), "{errno}");
        }
        for errno in [
            Errno::ERESTARTSYS,
            Errno::ERESTARTNOINTR,
            Errno::ERESTARTNOHAND,
            Errno::ERESTART_RESTARTBLOCK,
        ] {
            match posthook_checkpoint(9, anchors(), &Err(errno.into())) {
                Some(ResourceID::HappensBeforePosthookInterrupted {
                    count: 9,
                    anchors,
                    code,
                }) => {
                    assert_eq!(anchors, ["p"]);
                    assert_eq!(Some(code.as_str()), errno.name());
                }
                other => panic!("{errno}: {other:?}"),
            }
        }
        assert!(
            posthook_checkpoint(9, anchors(), &Err(Error::Tool(anyhow::anyhow!("tool")))).is_none()
        );
        assert!(posthook_checkpoint(9, Vec::new(), &Ok(0)).is_none());
        assert!(posthook_checkpoint(9, Vec::new(), &Err(Errno::ERESTARTSYS.into())).is_none());
    }

    /// A tool error is not the syscall's result and proves nothing about the
    /// buffer, so only a guest errno other than EFAULT hides it.
    #[test]
    fn only_non_efault_guest_errnos_prove_the_buffer_unwritten() {
        for errno in [Errno::ENOENT, Errno::EINVAL, Errno::EBADF, Errno::ENOMEM] {
            assert!(failure_proves_outputs_unwritten(&Err(errno.into())));
        }
        assert!(!failure_proves_outputs_unwritten(
            &Err(Errno::EFAULT.into())
        ));
        assert!(!failure_proves_outputs_unwritten(&Ok(0)));
        assert!(!failure_proves_outputs_unwritten(&Err(Error::Tool(
            anyhow::anyhow!("tool failure")
        ))));

        let path = CString::new("/dev/null").unwrap();
        let stat = sentinel_stat();
        let line = finish_line(
            &newfstatat(&path, &stat),
            Err(Error::Tool(anyhow::anyhow!("tool failure"))),
        );
        assert!(
            line.contains(&format!("st_size={SENTINEL}")),
            "buffer hidden on a tool error: {line}"
        );
    }

    /// `gettimeofday` stores `tv` before it can fail with `EFAULT` on a bad
    /// `tz`, so a failed call can carry a kernel-written output. It must stay
    /// rendered: suppressing it would hide real evidence from DETLOG.
    #[test]
    fn failed_gettimeofday_still_renders_the_timeval() {
        let tv = Timeval {
            tv_sec: SENTINEL,
            tv_usec: 0,
        };
        let call = Syscall::Gettimeofday(
            Gettimeofday::new()
                .with_tv(Some(TimevalMutPtr(
                    AddrMut::from_ptr(&tv as *const Timeval).unwrap(),
                )))
                .with_tz(None),
        );

        let line = finish_line(&call, Err(Errno::EFAULT.into()));
        assert!(
            line.contains(&format!("tv_sec: {SENTINEL}")),
            "kernel-written timeval hidden on failure: {line}"
        );
        assert!(line.contains("EFAULT"), "errno missing: {line}");
    }

    /// `fstat` never renders its buffer (T136880615); the result must not
    /// change that in either direction.
    #[test]
    fn fstat_never_renders_the_buffer() {
        let stat = sentinel_stat();
        let call = Syscall::Fstat(
            reverie::syscalls::Fstat::new()
                .with_fd(3)
                .with_stat(StatPtr::from_ptr(&stat as *const libc::stat)),
        );
        for result in [Ok(0), Err(Errno::EBADF.into())] {
            assert_no_struct_rendered(&finish_line(&call, result));
        }
    }
}
