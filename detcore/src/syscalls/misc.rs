/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Miscellaneous virtualized syscalls.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

use reverie::BackendCapabilities;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::ArchPrctlCmd;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sysno;

use crate::consts::DEFAULT_HOSTNAME;
use crate::detlog;
#[cfg(test)]
use crate::random::GETRANDOM_MAX_BYTES;
use crate::random::RANDOM_FILL_CHUNK_BYTES;
#[cfg(test)]
use crate::random::getrandom_request_len;
#[cfg(test)]
use crate::random::validate_getrandom_flags;
use crate::random::write_random_chunk;
use crate::record_or_replay::RecordOrReplay;
use crate::scheduler::HostTimedSignalScope;
use crate::tool_global::SigalrmControl;
use crate::tool_global::create_session;
use crate::tool_global::record_host_timed_signals;
use crate::tool_global::refuse_sigalrm;
use crate::tool_global::registered_process_of;
use crate::tool_global::set_child_subreaper;
use crate::tool_global::set_process_group;
use crate::tool_local::Detcore;
use crate::types::DetPid;

const ARCH_GET_XCOMP_SUPP: libc::c_int = 0x1021;
const ARCH_GET_XCOMP_PERM: libc::c_int = 0x1022;
const ARCH_REQ_XCOMP_PERM: libc::c_int = 0x1023;
const ARCH_GET_XCOMP_GUEST_PERM: libc::c_int = 0x1024;
const ARCH_REQ_XCOMP_GUEST_PERM: libc::c_int = 0x1025;

const ARCH_SHSTK_ENABLE: libc::c_int = 0x5001;
const ARCH_SHSTK_DISABLE: libc::c_int = 0x5002;
const ARCH_SHSTK_LOCK: libc::c_int = 0x5003;
const ARCH_SHSTK_UNLOCK: libc::c_int = 0x5004;
const ARCH_SHSTK_STATUS: libc::c_int = 0x5005;
const ARCH_SHSTK_VALID_MASK: usize = 0b11;

const SECCOMP_SET_MODE_STRICT: u32 = 0;
const SECCOMP_SET_MODE_FILTER: u32 = 1;
const SECCOMP_GET_ACTION_AVAIL: u32 = 2;
const SECCOMP_GET_NOTIF_SIZES: u32 = 3;
const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1;

fn seccomp_result(op: u32, flags: u32, has_args: bool) -> Result<i64, Errno> {
    if op > SECCOMP_GET_NOTIF_SIZES {
        return Err(Errno::EINVAL);
    }
    if op == SECCOMP_SET_MODE_STRICT && (flags != 0 || has_args) {
        return Err(Errno::EINVAL);
    }
    if op == SECCOMP_SET_MODE_FILTER && flags & !SECCOMP_FILTER_FLAG_TSYNC != 0 {
        return Err(Errno::EINVAL);
    }
    if matches!(
        op,
        SECCOMP_SET_MODE_FILTER | SECCOMP_GET_ACTION_AVAIL | SECCOMP_GET_NOTIF_SIZES
    ) && !has_args
    {
        return Err(Errno::EFAULT);
    }

    // Hermit cannot enforce a guest-installed BPF policy in every backend.
    // Report the operation as unsupported instead of claiming a filter exists.
    Err(Errno::EOPNOTSUPP)
}

/// Whether Detcore sends parent-death signals itself
/// (https://github.com/rrnewton/hermit/issues/4440): only where a thread's
/// exit reaches the scheduler in order (sequentialized threads) and the
/// guest's tasks are host tasks it can signal (ptrace).
fn delivers_parent_death_signal(config: &crate::config::Config) -> bool {
    config.sequentialize_threads && config.backend.host_task_reads_reach_guest_threads
}

/// Whether Detcore holds parent-death signals itself, arming the kernel with
/// 0 and answering `PR_GET_PDEATHSIG` from its own record: wherever it sends
/// them ([`delivers_parent_death_signal`]), and on an in-guest run's default
/// route (`Config::in_guest_refuses_host_timed_producers`), where it cannot
/// send them yet and refuses the run when one would fire
/// (`Scheduler::deliver_parent_death_signals`; D28a of A5 step 3c,
/// https://github.com/rrnewton/hermit/issues/4485).
fn holds_parent_death_signal(config: &crate::config::Config) -> bool {
    delivers_parent_death_signal(config) || config.in_guest_refuses_host_timed_producers
}

/// Where `prctl(PR_SET_PDEATHSIG, signal)` goes ([`parent_death_signal_route`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParentDeathSignalRoute {
    /// Detcore keeps the value (0 disarms) and arms the kernel with 0.
    Hold,
    /// The kernel takes the call, as on Linux.
    Kernel,
    /// EPERM and a WARN: a signal Detcore cannot hold, which the kernel would
    /// send at a host-timed moment.
    Refuse,
}

/// Where `prctl(PR_SET_PDEATHSIG, signal)` goes, where Detcore holds
/// parent-death signals (`holds`) and refuses the ones it cannot hold
/// (`refuses_unheld`, an in-guest run's default route). Detcore holds 0 and
/// every signal it sends itself ([`detcore_sends_parent_death_signal`]).
/// SIGKILL keeps the kernel's path everywhere: it cannot be blocked or left
/// pending, so the in-guest runtime's mask does not make it host-timed in the
/// way the refusal is about (design section 16.32). Any other signal (SIGSTOP,
/// SIGALRM, a realtime signal) is refused where unheld signals are; a value
/// above 64 always goes to the kernel, which refuses it with EINVAL.
fn parent_death_signal_route(
    signal: libc::c_ulong,
    holds: bool,
    refuses_unheld: bool,
) -> ParentDeathSignalRoute {
    if holds && (signal == 0 || detcore_sends_parent_death_signal(signal)) {
        ParentDeathSignalRoute::Hold
    } else if refuses_unheld
        && (1..=64).contains(&signal)
        && signal != libc::SIGKILL as libc::c_ulong
    {
        ParentDeathSignalRoute::Refuse
    } else {
        ParentDeathSignalRoute::Kernel
    }
}

/// Whether parent-death signal `signal` is one Detcore sends itself: a
/// standard signal (1 to 31) other than SIGKILL and SIGSTOP, which the
/// kernel keeps acting on as before (a SIGKILL'd child is never counted as
/// stuck, `Scheduler::child_is_stuck`), and SIGALRM, whose scheduler sends go
/// to the process's alarm ledger.
fn detcore_sends_parent_death_signal(signal: libc::c_ulong) -> bool {
    (1..=31).contains(&signal)
        && ![libc::SIGKILL, libc::SIGSTOP, libc::SIGALRM].contains(&(signal as libc::c_int))
}

fn is_supported_prctl_option(option: libc::c_int) -> bool {
    matches!(
        option,
        libc::PR_SET_NAME
            | libc::PR_GET_NAME
            | libc::PR_SET_THP_DISABLE
            | libc::PR_GET_THP_DISABLE
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#919)
            //
            // Dumpability is per-process state initialized deterministically by
            // ordinary exec and subsequently controlled only by the guest. Keep
            // Linux's 0/1 validation and state transitions rather than refusing
            // applications that explicitly disable or restore core-dump access.
            | libc::PR_SET_DUMPABLE
            | libc::PR_GET_DUMPABLE
            // PR_SET_TIMERSLACK and PR_GET_TIMERSLACK are deliberately absent:
            // `handle_prctl` emulates them against per-thread Detcore state.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#802)
            //
            // PR_{SET,GET}_KEEPCAPS only read/toggle the calling thread's
            // "keep capabilities across a UID change" flag. The result is a pure
            // function of the guest's own prior prctl calls (0/1), never host
            // state, so passthrough is deterministic and bitwise-identical across
            // runs. Supporting it lets `setpriv` (and the `date`/privilege-drop
            // wrappers that call it) run under --strict instead of aborting with
            // "keep process capabilities failed: Function not implemented".
            | libc::PR_SET_KEEPCAPS
            | libc::PR_GET_KEEPCAPS
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#824)
            //
            // PR_{SET,GET}_PDEATHSIG only read/toggle the calling thread's
            // parent-death-signal attribute. PR_SET_PDEATHSIG validates its
            // signal argument (a valid signal or 0 succeeds, anything else
            // faults EINVAL) and PR_GET_PDEATHSIG reports the value the guest
            // previously set. The result is a pure function of the guest's own
            // prior prctl calls and its argument, never host state, so
            // passthrough is deterministic and bitwise-identical across runs.
            // The registered signal fires when the parent thread exits, and the
            // kernel posts it at a moment set by host timing, so `handle_prctl`
            // first records a nonzero PR_SET_PDEATHSIG signal as host-timed for
            // the caller's process: no gated wait of that process ends for it
            // (https://github.com/rrnewton/hermit/issues/3146). Supporting it lets
            // `setpriv --pdeathsig` run under --strict instead of aborting with
            // "set parent death signal failed: Function not implemented".
            | libc::PR_SET_PDEATHSIG
            | libc::PR_GET_PDEATHSIG
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1125): Review KVM capability-control prctl forwarding.
            //
            // PR_CAPBSET_DROP and PR_CAP_AMBIENT change and read the calling
            // thread's capability bounding and ambient sets. They reach the
            // kernel on every backend (the host's, or KVM's provider), as
            // PR_CAPBSET_READ and capget already do, so each backend answers as
            // native Linux does for the same credentials: a drop or a raise
            // the caller is not entitled to fails with EPERM, an unknown
            // capability with EINVAL. The result is a function of the
            // container's initial credentials and the guest's own earlier
            // calls. They used to reach the kernel on KVM only, and fail with
            // ENOSYS everywhere else.
            | libc::PR_CAPBSET_DROP
            | libc::PR_CAP_AMBIENT
    )
}

/// Whether Detcore can admit `PR_SET_CHILD_SUBREAPER`: threads are
/// sequentialized, so every exit reaches the scheduler through
/// `deregister_thread` and re-parents there (`Scheduler::reparent_orphans`);
/// without that, Detcore's record never learns the orphan moved while it still
/// decides ECHILD. And on `backend`, its own record of children decides waits
/// (`tracks_process_children`), the kernel rather than the backend performs
/// them (`!emulates_child_waits`), and a process's exit reaches Detcore only
/// after the kernel's `exit_notify` has re-parented its children (neither
/// asynchronous nor separately reported physical exits), so the record agrees
/// with the kernel. Today that is ptrace, and e9patch preprocessing run under
/// ptrace, in a sequentialized run.
fn models_child_subreapers(sequentialize_threads: bool, backend: &BackendCapabilities) -> bool {
    sequentialize_threads
        && backend.tracks_process_children
        && !backend.emulates_child_waits
        && !backend.process_exits_complete_asynchronously
        && !backend.reports_physical_process_exits
}

/// Is `which` one of the Linux `PRIO_*` target selectors for get/setpriority?
fn is_valid_prio_which(which: i32) -> bool {
    which == libc::PRIO_PROCESS as i32
        || which == libc::PRIO_PGRP as i32
        || which == libc::PRIO_USER as i32
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#806)
/// Deterministic raw `getpriority(2)` result under Hermit's inert-nice model.
///
/// Returns `20 - nice` (nice 0 -> 20) for any valid target regardless of `who`,
/// because nice is inert under the virtualized scheduler and `getpriority` never
/// checks permissions; an unknown `which` faults with `EINVAL`, matching Linux.
fn getpriority_result(which: i32) -> Result<i64, Errno> {
    if is_valid_prio_which(which) {
        Ok(20)
    } else {
        Err(Errno::EINVAL)
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#806)
/// Deterministic raw `setpriority(2)` result: accept any priority change for a
/// valid target as an inert no-op (returns 0), `EINVAL` for an unknown `which`.
fn setpriority_result(which: i32) -> Result<i64, Errno> {
    if is_valid_prio_which(which) {
        Ok(0)
    } else {
        Err(Errno::EINVAL)
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-857): Deterministic empty kernel-log action policy.
fn deterministic_syslog_result(action: i32, len: usize) -> Result<i64, Errno> {
    const SYSLOG_ACTION_CONSOLE_LEVEL: i32 = 8;

    match action {
        0..=7 | 9..=10 => Ok(0),
        SYSLOG_ACTION_CONSOLE_LEVEL if (1..=8).contains(&len) => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

/// Write get_mempolicy's output as Linux's `put_user`/`copy_to_user` do:
/// through the guest's page protections, so a buffer that cannot take all the
/// bytes is `EFAULT`. Any other failure is the backend's, not a guest fault,
/// and is reported as a tool error rather than handed to the guest as an
/// errno get_mempolicy never returns (Claude re-check of
/// https://github.com/rrnewton/hermit/pull/4249).
fn write_get_mempolicy_output<M: MemoryAccess>(
    memory: &mut M,
    address: AddrMut<'_, u8>,
    bytes: &[u8],
) -> Result<(), Error> {
    match memory.write_with_user_access(address, bytes) {
        Ok(written) if written == bytes.len() => Ok(()),
        Ok(_) | Err(Errno::EFAULT) => Err(Errno::EFAULT.into()),
        Err(errno) => Err(Error::Tool(anyhow::anyhow!(
            "get_mempolicy output write failed: {errno}"
        ))),
    }
}

fn from_str(s: &str) -> [i8; 65] {
    let mut ret: [i8; 65] = [0; 65];
    for (i, ch) in s.bytes().take(64).enumerate() {
        ret[i] = ch as i8;
    }
    ret
}

const RANDOM_DEVICE_BYTE_STRIDE: u8 = 73;
const RANDOM_DEVICE_FIRST_BYTE: u8 = 41;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1096): Review the backend-independent random-device stream.
fn canonical_random_device_byte(seed: u64, index: u64) -> u8 {
    let seed_byte = seed.rotate_right(((index % 8) * 8) as u32) as u8;
    (index as u8)
        .wrapping_mul(RANDOM_DEVICE_BYTE_STRIDE)
        .wrapping_add(RANDOM_DEVICE_FIRST_BYTE)
        ^ seed_byte
}

/// Scatter one stream through an already-imported array. Scratch space is
/// independent of request size; only EFAULT becomes a successful copied prefix.
fn fill_canonical_random_iovecs(
    memory: &mut impl MemoryAccess,
    iovecs: &[crate::iovecs::ImportedIovec],
    seed: u64,
    stream_offset: u64,
    hasher: &mut DefaultHasher,
) -> Result<usize, Error> {
    let mut local_words = [0_u64; RANDOM_FILL_CHUNK_BYTES / std::mem::size_of::<u64>()];
    let mut written = 0_usize;
    for iov in iovecs {
        let mut segment_written = 0;
        while segment_written < iov.len {
            let remote_chunk = match iov
                .base
                .checked_add(segment_written)
                .and_then(AddrMut::<u8>::from_raw)
            {
                Some(address) => address,
                None if written == 0 => return Err(Errno::EFAULT.into()),
                None => return Ok(written),
            };
            let chunk_len = (iov.len - segment_written).min(RANDOM_FILL_CHUNK_BYTES);
            let local_buf = unsafe {
                std::slice::from_raw_parts_mut(local_words.as_mut_ptr().cast::<u8>(), chunk_len)
            };
            for (index, byte) in local_buf.iter_mut().enumerate() {
                *byte = canonical_random_device_byte(
                    seed,
                    stream_offset
                        .saturating_add(written as u64)
                        .saturating_add(index as u64),
                );
            }
            let n = match write_random_chunk(memory, remote_chunk, local_buf) {
                Ok(n) => n,
                Err(Errno::EFAULT) if written > 0 => return Ok(written),
                Err(error) => return Err(crate::random::copy_error(error)),
            };
            if n == 0 && written == 0 {
                return Err(Errno::EFAULT.into());
            }
            if cfg!(debug_assertions) {
                Hash::hash_slice(&local_buf[..n], hasher);
            }
            written += n;
            segment_written += n;
            if n < chunk_len {
                return Ok(written);
            }
        }
    }
    Ok(written)
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Validates seccomp capability probes without installing guest filters.
    // TODO-HUMAN-REVIEW(PR-874): Review deterministic seccomp probe validation.
    pub async fn handle_seccomp<G: Guest<Self>>(
        &self,
        _guest: &mut G,
        call: syscalls::Seccomp,
    ) -> Result<i64, Error> {
        seccomp_result(call.op(), call.flags(), call.args().is_some()).map_err(Into::into)
    }

    /// The host xstate facts of an x86-64-v3 run, or `None` under v2.
    fn v3_host_xstate(&self) -> Option<&detcore_model::cpu_profile::HostXstate> {
        (self.cfg.cpu_profile == detcore_model::cpu_profile::CpuProfile::X86_64V3)
            .then_some(self.cfg.cpu_profile_host.as_ref())
            .flatten()
    }

    fn write_arch_prctl_u64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        raw_addr: usize,
        value: u64,
    ) -> Result<i64, Error> {
        let addr = AddrMut::<u64>::from_raw(raw_addr).ok_or(Errno::EFAULT)?;
        guest.memory().write_value(addr, &value)?;
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#539): Confirm the virtual arch_prctl control policy.
    /// Preserve thread-local bases while hiding host CPU feature controls.
    pub async fn handle_arch_prctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ArchPrctl,
    ) -> Result<i64, Error> {
        let cpuid_uses_faulting = self.cfg.virtualize_cpuid && guest.has_cpuid_interception();
        match call.cmd() {
            ArchPrctlCmd::ARCH_SET_FS(_)
            | ArchPrctlCmd::ARCH_SET_GS(_)
            | ArchPrctlCmd::ARCH_GET_FS(_)
            | ArchPrctlCmd::ARCH_GET_GS(_) => Ok(guest.inject(call).await?),

            // When Reverie successfully disables native CPUID, Detcore answers its fault from a
            // fixed table. Preserve that backend state and reject attempts to re-enable CPUID.
            ArchPrctlCmd::ARCH_GET_CPUID(_) if cpuid_uses_faulting => Ok(0),
            ArchPrctlCmd::ARCH_SET_CPUID(value) if cpuid_uses_faulting => {
                if value == 0 {
                    Ok(0)
                } else {
                    Err(Errno::EPERM.into())
                }
            }
            // Reverie cannot faithfully deliver a CPUID fault requested by the tracee. In
            // explicit host-CPUID mode, expose a fixed enabled control state and reject disable.
            ArchPrctlCmd::ARCH_GET_CPUID(_) if !self.cfg.virtualize_cpuid => Ok(1),
            ArchPrctlCmd::ARCH_SET_CPUID(value) if !self.cfg.virtualize_cpuid => {
                if value == 0 {
                    Err(Errno::EPERM.into())
                } else {
                    Ok(0)
                }
            }

            // Ptrace hosts without CPUID-faulting support retain the kernel's honest state.
            ArchPrctlCmd::ARCH_GET_CPUID(_) | ArchPrctlCmd::ARCH_SET_CPUID(_) => {
                Ok(guest.inject(call).await?)
            }

            // The host profile exposes the native XSAVE layout, including
            // dynamic components, so Linux must answer its permission queries
            // and apply requests to the guest task itself. Preserve its masks,
            // pointer checks and errno rather than the fixed-profile policy:
            // Linux v7.1.3, fpu_xstate_prctl, arch/x86/kernel/fpu/xstate.c:1840.
            // https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/arch/x86/kernel/fpu/xstate.c?h=v7.1.3#n1840
            ArchPrctlCmd::Other(
                ARCH_GET_XCOMP_SUPP
                | ARCH_GET_XCOMP_PERM
                | ARCH_GET_XCOMP_GUEST_PERM
                | ARCH_REQ_XCOMP_PERM
                | ARCH_REQ_XCOMP_GUEST_PERM,
                _,
            ) if self.cfg.cpu_profile == detcore_model::cpu_profile::CpuProfile::HostNoRdrand => {
                Ok(guest.inject(call).await?)
            }

            // Under x86-64-v3 the guest's XGETBV and XSAVE run on the host, so
            // the xstate queries answer with the host's masks, captured at
            // startup on a host without dynamic features and bound in the
            // recording; the requests answer as Linux does on such a host:
            // EOPNOTSUPP below XFEATURE_MAX and EINVAL from it up
            // (https://github.com/rrnewton/hermit/issues/4243).
            ArchPrctlCmd::Other(
                cmd @ (ARCH_GET_XCOMP_SUPP | ARCH_GET_XCOMP_PERM | ARCH_GET_XCOMP_GUEST_PERM),
                addr,
            ) if let Some(host) = self.v3_host_xstate() => {
                let mask = match cmd {
                    ARCH_GET_XCOMP_SUPP => host.xcomp_supp,
                    ARCH_GET_XCOMP_PERM => host.xcomp_perm,
                    _ => host.xcomp_guest_perm,
                };
                self.write_arch_prctl_u64(guest, addr, mask)
            }
            ArchPrctlCmd::Other(ARCH_REQ_XCOMP_PERM | ARCH_REQ_XCOMP_GUEST_PERM, index)
                if let Some(host) = self.v3_host_xstate() =>
            {
                if index >= host.xfeature_max as usize {
                    Err(Errno::EINVAL.into())
                } else {
                    Err(Errno::EOPNOTSUPP.into())
                }
            }
            // Expose a conservative virtual CPU with no optional extended-state permissions.
            ArchPrctlCmd::Other(
                ARCH_GET_XCOMP_SUPP | ARCH_GET_XCOMP_PERM | ARCH_GET_XCOMP_GUEST_PERM,
                addr,
            ) => self.write_arch_prctl_u64(guest, addr, 0),
            ArchPrctlCmd::Other(ARCH_REQ_XCOMP_PERM | ARCH_REQ_XCOMP_GUEST_PERM, _) => {
                Err(Errno::EINVAL.into())
            }

            // Keep shadow stacks disabled in the virtual policy. Disabling an already-disabled
            // feature is idempotent; enable/lock/unlock requests cannot be honored.
            ArchPrctlCmd::Other(ARCH_SHSTK_STATUS, addr) => {
                self.write_arch_prctl_u64(guest, addr, 0)
            }
            ArchPrctlCmd::Other(ARCH_SHSTK_DISABLE, features)
                if features != 0 && features & !ARCH_SHSTK_VALID_MASK == 0 =>
            {
                Ok(0)
            }
            ArchPrctlCmd::Other(ARCH_SHSTK_DISABLE, _)
            | ArchPrctlCmd::Other(ARCH_SHSTK_ENABLE | ARCH_SHSTK_LOCK | ARCH_SHSTK_UNLOCK, _) => {
                Err(Errno::EINVAL.into())
            }

            ArchPrctlCmd::Other(_, _) => Err(Errno::EINVAL.into()),
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Preserve deterministic Ruby thread controls, report the container's fixed
    /// capability bounding set, and reject options that expose unmodeled process
    /// or host state.
    pub async fn handle_prctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Prctl,
    ) -> Result<i64, Error> {
        match call.option() {
            // The capability bounding set is fixed by the container launch policy.
            libc::PR_CAPBSET_READ => Ok(self.record_or_replay(guest, call).await?),
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-2150): Timer slack is shared with the
            // virtual `/proc/<tid>/timerslack_ns` channel.  Never pass either
            // setter through to the physical tracee: a large physical slack
            // changes wake timing on Detcore's remaining host-timed waits.
            libc::PR_SET_TIMERSLACK => {
                let requested = call.arg2();
                let state = guest.thread_state_mut();
                // Linux treats zero as reset-to-default. Hermit virtualizes the
                // Linux scheduling policy as SCHED_OTHER, so the kernel's
                // RT/DL no-op branch is unreachable in the guest model.
                state.timer_slack_ns = if requested == 0 {
                    state.default_timer_slack_ns
                } else {
                    requested
                };
                Ok(0)
            }
            libc::PR_GET_TIMERSLACK => Ok(guest.thread_state().timer_slack_ns as i64),
            // The kernel sends a parent-death signal process-directed
            // (`group_send_sig_info` with `PIDTYPE_TGID`) when the parent thread
            // exits, at a moment set by host timing. Record it in this turn,
            // before the call reaches the kernel, for every thread of the
            // caller's process, and never forget it: a later disarm or `exec`
            // does not remove it (`Scheduler::record_host_timed_signals`). Linux
            // accepts 0 (disarm) and 1 to 64; anything else fails with EINVAL in
            // the kernel and arms nothing.
            // A parent-death signal Detcore sends itself is held by Detcore,
            // not the kernel (armed with 0); any other comes from the kernel.
            libc::PR_GET_PDEATHSIG if holds_parent_death_signal(guest.config()) => {
                let held = crate::tool_global::parent_death_signal(guest, None).await;
                if held == 0 {
                    return self.passthrough(guest, call.into()).await;
                }
                let target =
                    AddrMut::<libc::c_int>::from_raw(call.arg2() as usize).ok_or(Errno::EFAULT)?;
                guest
                    .memory()
                    .write_value(target, &held)
                    .map_err(|_| Errno::EFAULT)?;
                Ok(0)
            }
            libc::PR_SET_PDEATHSIG => {
                let signal = call.arg2();
                // A reserved in-guest signal as the parent-death signal; 0
                // disarms and is never reserved, nor is a value the kernel
                // refuses with EINVAL (above 64).
                Self::refuse_reserved_signal_arming(
                    guest,
                    crate::refused_signal_controls::ReservedArming::PrctlPdeathsig,
                    i64::try_from(signal).unwrap_or(-1),
                )?;
                if signal == libc::SIGALRM as libc::c_ulong {
                    refuse_sigalrm(guest, SigalrmControl::ArmProducer).await?;
                }
                // A signal Detcore sends itself when the parent thread leaves
                // (`Scheduler::deliver_parent_death_signals`,
                // https://github.com/rrnewton/hermit/issues/4440), or, on an
                // in-guest run's default route, one it holds and refuses the
                // run over if it would fire (D28a): the kernel is armed with
                // 0, so it never posts a copy of its own at a host-timed
                // moment, and Detcore keeps the value. There, a signal Detcore
                // cannot hold is refused. Any other value takes the kernel's
                // path below, and disarms Detcore's only once the kernel has
                // accepted it (an invalid one, which the kernel refuses with
                // EINVAL, changes nothing).
                let held = holds_parent_death_signal(guest.config());
                let route = parent_death_signal_route(
                    signal,
                    held,
                    guest.config().in_guest_refuses_host_timed_producers,
                );
                if route == ParentDeathSignalRoute::Refuse {
                    tracing::warn!(
                        "{}",
                        crate::host_timed_producers::parent_death_signal_refused_warning(
                            signal as i32,
                            guest.thread_state().dettid.as_raw(),
                        )
                    );
                    return Err(Errno::EPERM.into());
                }
                if route == ParentDeathSignalRoute::Hold {
                    // The call is Detcore's, not recorded: replay decides it
                    // the same way. The kernel may hold a value an earlier
                    // kernel-path call armed, which this one replaces, so it
                    // is disarmed (retried if interrupted). Detcore's record
                    // changes first: a parent that dies at a host-timed moment
                    // between the two then sends the new signal and the old
                    // (a death just before the call, on Linux, sends the old,
                    // which a fatal old one makes the outcome), never neither.
                    // A disarm that fails restores Detcore's previous value.
                    let previous = crate::tool_global::parent_death_signal(guest, None).await;
                    crate::tool_global::parent_death_signal(guest, Some(signal as i32)).await;
                    if let Err(errno) = guest
                        .inject_with_retry(syscalls::Syscall::from(call.with_arg2(0)))
                        .await
                    {
                        crate::tool_global::parent_death_signal(guest, Some(previous)).await;
                        return Err(errno.into());
                    }
                    return Ok(0);
                }
                if (1..=64).contains(&signal)
                    && guest.config().backend.host_timed_signals_are_physical
                {
                    record_host_timed_signals(
                        guest,
                        HostTimedSignalScope::Caller,
                        1_u64 << (signal - 1),
                    )
                    .await;
                }
                let result = self.passthrough(guest, call.into()).await?;
                if held {
                    crate::tool_global::parent_death_signal(guest, Some(0)).await;
                }
                Ok(result)
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-4000)
            //
            // The subreaper flag is per-process state only the guest sets, so
            // the kernel call is deterministic. It is injected in replay too,
            // like setpgid, rather than replayed from the log: the kernel must
            // hold the flag for orphans to become this process's children, so
            // that the waits Detcore injects for them succeed. On success
            // Detcore mirrors it in its thread tree, in this turn, so that an
            // orphan re-parented to this process becomes its child for
            // Detcore's waits too (`Scheduler::reparent_orphans`). The process
            // is the one Detcore registered the calling thread in, resolved
            // before the call: a raw clone child with a non-SIGCHLD exit
            // signal is its own process there, although the backend reports
            // its creator's pid. A thread with no registered process is
            // refused before any effect. Linux treats any nonzero argument as
            // "on". PR_GET_CHILD_SUBREAPER reads the caller's own flag back
            // from the kernel.
            libc::PR_SET_CHILD_SUBREAPER
                if models_child_subreapers(
                    guest.config().sequentialize_threads,
                    &guest.config().backend,
                ) =>
            {
                let dettid = guest.thread_state().dettid;
                let Some(process) = registered_process_of(guest, dettid).await else {
                    return Err(Errno::ENOSYS.into());
                };
                let result = guest.inject(call).await?;
                set_child_subreaper(guest, process, call.arg2() != 0).await;
                Ok(result)
            }
            libc::PR_GET_CHILD_SUBREAPER
                if models_child_subreapers(
                    guest.config().sequentialize_threads,
                    &guest.config().backend,
                ) =>
            {
                Ok(guest.inject(call).await?)
            }
            option if is_supported_prctl_option(option) => {
                self.passthrough(guest, call.into()).await
            }
            _ => Err(Errno::ENOSYS.into()),
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#806)
    /// Report the deterministic default nice value for any scheduling target.
    ///
    /// Under Hermit the Linux nice value is inert: the scheduler is virtualized
    /// and guest threads are serialized onto one virtual CPU, so a process's,
    /// group's, or user's scheduling priority never affects guest-visible
    /// computation. Report the deterministic default nice (0) for every valid
    /// target regardless of `who` — real tools such as `renice -p <pid>` always
    /// pass an explicit pid, and the raw `getpriority(2)` never checks
    /// permissions on a read, so it must never return `EPERM`. An unknown
    /// `which` still faults with `EINVAL`, matching Linux.
    pub async fn handle_getpriority<G: Guest<Self>>(
        &self,
        _guest: &mut G,
        call: syscalls::Getpriority,
    ) -> Result<i64, Error> {
        Ok(getpriority_result(call.which())?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-857): Deterministic syslog(2) virtualization.
    /// Present an empty kernel ring buffer. Reads and size queries return zero,
    /// controls are inert, and invalid actions preserve Linux's EINVAL boundary.
    pub async fn handle_syslog<G: Guest<Self>>(
        &self,
        _guest: &mut G,
        call: syscalls::Syslog,
    ) -> Result<i64, Error> {
        Ok(deterministic_syslog_result(call.priority(), call.len())?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#806)
    /// Accept any priority change as a deterministic no-op.
    ///
    /// Nice values are inert under Hermit's virtualized, serialized scheduler,
    /// so accept the request without touching host scheduling. The guest runs as
    /// a single uid-0 container principal, so a real `setpriority(2)` from the
    /// caller would succeed anyway; never fabricate `EPERM` for tools such as
    /// `nice -n 5 <cmd>`, `renice -p <pid>`, or Python's `os.nice`. An unknown
    /// `which` still faults with `EINVAL`, matching Linux.
    pub async fn handle_setpriority<G: Guest<Self>>(
        &self,
        _guest: &mut G,
        call: syscalls::Setpriority,
    ) -> Result<i64, Error> {
        Ok(setpriority_result(call.which())?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Reject cross-process memory advice without consulting host process state.
    pub fn handle_process_madvise(pidfd: usize, flags: usize) -> Result<i64, Error> {
        if flags != 0 {
            return Err(Errno::EINVAL.into());
        }

        // Linux interprets pidfd as an int. Preserve its deterministic invalid-fd
        // rejection, but never let a valid host pidfd alter another process's memory.
        if (pidfd as libc::c_int) < 0 {
            Err(Errno::EBADF.into())
        } else {
            Err(Errno::EPERM.into())
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1096): Review the backend-independent random-device stream.
    /// Fill guest memory from the canonical stream used by every backend's
    /// `/dev/random` and `/dev/urandom` virtualization.
    pub(super) fn fill_random_device_bytes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        remote_buf: AddrMut<u8>,
        len: usize,
        stream_offset: u64,
    ) -> Result<usize, Error> {
        self.fill_random_device_iovecs(
            guest,
            &[crate::iovecs::ImportedIovec {
                base: remote_buf.as_raw(),
                len,
            }],
            stream_offset,
        )
    }

    pub(super) fn fill_random_device_iovecs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        iovecs: &[crate::iovecs::ImportedIovec],
        stream_offset: u64,
    ) -> Result<usize, Error> {
        let seed = guest.config().rng_seed();
        let mut hasher = DefaultHasher::new();
        let written = fill_canonical_random_iovecs(
            &mut guest.memory(),
            iovecs,
            seed,
            stream_offset,
            &mut hasher,
        )?;
        if cfg!(debug_assertions) {
            detlog!(
                "[dtid {}] USER RAND [/dev/[u]random] Filled guest memory with {} canonical random bytes at offset {}, hash of bytes: {}",
                guest.thread_state().dettid,
                written,
                stream_offset,
                hasher.finish()
            );
        }
        Ok(written)
    }

    /// uname syscall
    pub async fn handle_uname<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Uname,
    ) -> Result<i64, Error> {
        let ret = self.record_or_replay(guest, call).await?;
        if let Some(buf) = call.buf() {
            let mut un = guest.memory().read_value(buf)?;
            let kernel = crate::machine::MachineDescription::of(guest.config()).kernel;

            if !guest.config().has_uts_namespace {
                // FIXME: It should be possible to remove this once all tests
                // are also using namespaces.
                un.nodename = from_str(DEFAULT_HOSTNAME);
                un.domainname = from_str(DEFAULT_HOSTNAME.split('.').next_back().unwrap_or(""));
            }

            // The kernel procfs describes too (`procfs::fixed`).
            un.sysname = from_str(&kernel.sysname);
            un.release = from_str(&kernel.release);
            un.version = from_str(&kernel.version());
            guest.memory().write_value(buf, &un)?;
        }

        Ok(ret)
    }

    /// Fill `getrandom(2)` requests from the current thread's seeded deterministic PRNG.
    /// Supported blocking/source-selection flags share that always-ready stream; invalid Linux
    /// flag combinations are rejected before guest memory is touched.
    pub async fn handle_getrandom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getrandom,
    ) -> Result<i64, Error> {
        let memory = guest.memory();
        let dettid = guest.thread_state().dettid;
        crate::random::getrandom(guest.thread_state_mut().thread_prng(), memory, dettid, call)
    }

    /// setsid system call, answered as Linux answers it from Detcore's model
    /// of the guest's sessions and process groups (`ThreadTree`), the same on
    /// every backend (slice B3a-4c of
    /// <https://github.com/rrnewton/hermit/issues/4266>). The call then goes to
    /// the backend too, so that a kernel that keeps process groups (ptrace's)
    /// stays in step; a backend that keeps none answers ENOSYS (KVM's
    /// executor), and the model is the guest's answer. Any other disagreement
    /// is refused by name, never answered with the backend's errno. A caller
    /// in a nested PID namespace, whose ids are not the model's, is answered
    /// by the kernel and mirrored, as before.
    // TODO-HUMAN-REVIEW(PR-b3a4c): Review setsid from the model.
    pub async fn handle_setsid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setsid,
    ) -> Result<i64, Error> {
        let process = guest.thread_state().detpid.expect("detpid unset");
        let res = if guest.thread_state().nested_pid_namespace {
            let res = guest.inject(call).await?;
            let _ = create_session(guest, process).await;
            res
        } else {
            crate::tool_global::setsid_decision(guest, process).await?;
            self.mirror_group_change(guest, reverie::syscalls::Syscall::Setsid(call))
                .await?;
            let _ = create_session(guest, process).await;
            i64::from(process.as_raw())
        };

        // task is trying to become a daemon process. for more details
        // see: https://notes.shichao.io/apue/ch13/
        if guest.config().kill_daemons {
            guest.daemonize().await;
        }
        Ok(res)
    }

    /// `getpgrp`, `getpgid` and `getsid`, answered from Detcore's model of the
    /// guest's process groups and sessions (`ThreadTree`), which `setpgid` and
    /// `setsid` keep, the same on every backend. The root process's group and
    /// session come from outside the container's PID namespace and read as 0,
    /// as on Linux (`scheduler::OUTSIDE_NAMESPACE`), and so do the namespace
    /// init's, pid 1. A pid that names no task Linux still has (none
    /// registered, an exited thread, a process already reaped, automatically
    /// or by the namespace's init) is ESRCH; a negative one too, as Linux
    /// finds no task for it (`Scheduler::group_or_session_of`). A caller in a
    /// nested PID namespace, whose ids are not the model's, is answered by the
    /// kernel as before.
    // TODO-HUMAN-REVIEW(PR-b3a4c): Review the group and session model answers
    // (slice B3a-4c of https://github.com/rrnewton/hermit/issues/4266).
    pub async fn handle_group_or_session_getter<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: reverie::syscalls::Syscall,
    ) -> Result<i64, Error> {
        if guest.thread_state().nested_pid_namespace {
            return Ok(guest.inject(call).await?);
        }
        let (sysno, args) = reverie::syscalls::SyscallInfo::into_parts(call);
        let pid = if sysno == Sysno::getpgrp {
            0
        } else {
            args.arg0 as libc::pid_t
        };
        let task = match pid {
            0 => guest.thread_state().detpid.expect("detpid unset"),
            pid if pid < 0 => return Err(Errno::ESRCH.into()),
            pid => DetPid::from_raw(pid),
        };
        let answer =
            crate::tool_global::group_or_session_of(guest, task, sysno == Sysno::getsid).await;
        Ok(i64::from(answer.ok_or(Errno::ESRCH)?.as_raw()))
    }

    /// setpgid system call, answered as Linux answers it from Detcore's model
    /// (`ThreadTree::setpgid_decision`), and mirrored to the backend, as
    /// `handle_setsid`. A caller in a nested PID namespace is answered by the
    /// kernel and the model mirrors it, as before: the kernel validates, and
    /// group-selecting waits read the model rather than host `/proc` state.
    // TODO-HUMAN-REVIEW(PR-b3a4c): Review setpgid from the model.
    pub async fn handle_setpgid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setpgid,
    ) -> Result<i64, Error> {
        let caller = guest.thread_state().detpid.expect("detpid unset");
        if !guest.thread_state().nested_pid_namespace {
            let (process, group) =
                crate::tool_global::setpgid_decision(guest, caller, call.pid(), call.pgid())
                    .await?;
            self.mirror_group_change(guest, reverie::syscalls::Syscall::Setpgid(call))
                .await?;
            let _ = set_process_group(guest, process, group).await;
            return Ok(0);
        }
        let res = guest.inject(call).await?;
        // A nested caller's ids are its own PID namespace's; the kernel took
        // them so, and the model records the tasks they name
        // (https://github.com/rrnewton/hermit/issues/4155).
        let process = if call.pid() == 0 {
            caller
        } else {
            self.task_named_after_success(guest, Sysno::setpgid, call.pid())
                .await?
        };
        let group = if call.pgid() == 0 {
            process
        } else {
            self.task_named_after_success(guest, Sysno::setpgid, call.pgid())
                .await?
        };
        let _ = set_process_group(guest, process, group).await;
        Ok(res)
    }

    /// Give the backend a `setsid` or `setpgid` the model accepted, so that a
    /// kernel that keeps process groups stays in step with the model. A
    /// backend that keeps none says so with a typed refusal
    /// (`UnsupportedOperation::ProcessGroups`), and the model's answer stands.
    /// Any other failure, whatever its errno, means the backend's process
    /// groups disagree with the model's: refused by name rather than answered
    /// with the backend's errno. An errno alone never stands for a refusal: a
    /// kernel ENOSYS for another reason would otherwise read as success.
    async fn mirror_group_change<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: reverie::syscalls::Syscall,
    ) -> Result<(), Error> {
        let result = guest.inject(call).await;
        // Ask before anything else is injected: the next injection replaces
        // the backend's record.
        let refusal = result
            .is_err()
            .then(|| guest.take_unsupported_refusal())
            .flatten();
        match result {
            Ok(_) => Ok(()),
            Err(_)
                if refusal.is_some_and(|refusal| {
                    refusal.operation == reverie::UnsupportedOperation::ProcessGroups
                }) =>
            {
                Ok(())
            }
            Err(errno) => {
                let sysno = reverie::syscalls::SyscallInfo::number(&call);
                tracing::error!(
                    "[detcore, dtid {}] {} was accepted by Detcore's process-group model but the backend answered {}: its process groups disagree with the model's (slice B3a-4c of https://github.com/rrnewton/hermit/issues/4266)",
                    guest.thread_state().dettid,
                    sysno,
                    errno,
                );
                self.refuse_unserviceable_operation(guest, sysno, errno)
                    .await
                    .map(|_| ())
            }
        }
    }

    /// The task a positive id the calling thread passed names, after the
    /// kernel accepted it: the id itself at depth 0, translated from the
    /// caller's PID namespace below that. An id the record cannot name is
    /// refused by name; the kernel found it, so the record is incomplete.
    async fn task_named_after_success<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        id: libc::pid_t,
    ) -> Result<DetPid, Error> {
        if !guest.thread_state().nested_pid_namespace {
            return Ok(DetPid::from_raw(id));
        }
        let reason = match crate::tool_global::guest_pid(guest, id).await {
            crate::scheduler::pid_namespaces::GuestPidAnswer::Task { task, .. } => return Ok(task),
            crate::scheduler::pid_namespaces::GuestPidAnswer::NoSuchPid => {
                format!(
                    "the kernel accepted id {id}, which no recorded task of the caller's namespace has"
                )
            }
            crate::scheduler::pid_namespaces::GuestPidAnswer::Untranslatable(reason) => reason,
        };
        tracing::error!(
            "[detcore, dtid {}] {} from a nested PID namespace is not supported: {} (https://github.com/rrnewton/hermit/issues/4155)",
            guest.thread_state().dettid,
            sysno,
            reason,
        );
        match self
            .refuse_unserviceable_operation(guest, sysno, Errno::ENOSYS)
            .await
        {
            Err(error) => Err(error),
            Ok(_) => Err(Errno::ENOSYS.into()),
        }
    }

    /// membarrier (system call).
    ///
    /// `membarrier(2)` issues process-wide memory barriers so that userspace can
    /// use asymmetric fences (e.g. CPython's QSBR, RCU-style reclamation).
    /// Detcore serializes all guest threads onto a single logical CPU with a
    /// total memory order, so any requested barrier is *already* satisfied and
    /// every command is a deterministic no-op. For `MEMBARRIER_CMD_QUERY` we
    /// report the set of commands we emulate so the guest stays on this
    /// controlled path instead of a host-dependent fallback; every other command
    /// returns success without doing anything.
    pub async fn handle_membarrier<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Membarrier,
    ) -> Result<i64, Error> {
        // Values from <linux/membarrier.h>.
        const MEMBARRIER_CMD_QUERY: i32 = 0;
        const MEMBARRIER_CMD_GLOBAL: i32 = 1 << 0;
        const MEMBARRIER_CMD_GLOBAL_EXPEDITED: i32 = 1 << 1;
        const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: i32 = 1 << 2;
        const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 = 1 << 3;
        const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 = 1 << 4;
        const SUPPORTED: i32 = MEMBARRIER_CMD_GLOBAL
            | MEMBARRIER_CMD_GLOBAL_EXPEDITED
            | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
            | MEMBARRIER_CMD_PRIVATE_EXPEDITED
            | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED;

        let cmd = call.cmd();
        if cmd == MEMBARRIER_CMD_QUERY {
            detlog!(
                "[dtid {}] membarrier(QUERY) => reporting emulated commands {:#x}",
                guest.thread_state().dettid,
                SUPPORTED,
            );
            Ok(SUPPORTED as i64)
        } else {
            detlog!(
                "[dtid {}] membarrier(cmd={}) no-op (threads are serialized on one CPU)",
                guest.thread_state().dettid,
                cmd,
            );
            Ok(0)
        }
    }

    /// getcpu system call
    pub async fn handle_getcpu<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getcpu,
    ) -> Result<i64, Error> {
        // Always set the CPU to 0.
        if let Some(cpu) = call.cpu() {
            guest.memory().write_value(cpu, &0)?;
        }

        // Always set the NUMA node to 0.
        if let Some(node) = call.node() {
            guest.memory().write_value(node, &0)?;
        }

        Ok(0)
    }

    /// getresuid under Hermit. Detcore presents a fixed virtual-root identity, so
    /// the real, effective, and saved user IDs are all the constant 0. Under the
    /// ptrace backend the guest runs inside a CLONE_NEWUSER namespace that already
    /// maps the host uid to 0; emulating the same constant here makes in-process
    /// backends (DBT) agree with that golden reference instead of leaking the host
    /// uid, and the fully emulated result is bitwise-identical across --verify and
    /// record/replay.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#1549)
    pub async fn handle_getresuid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getresuid,
    ) -> Result<i64, Error> {
        if let Some(ruid) = call.ruid() {
            guest.memory().write_value(ruid, &0)?;
        }
        if let Some(euid) = call.euid() {
            guest.memory().write_value(euid, &0)?;
        }
        if let Some(suid) = call.suid() {
            guest.memory().write_value(suid, &0)?;
        }
        Ok(0)
    }

    /// getresgid under Hermit. The group-ID counterpart of `handle_getresuid`:
    /// the real, effective, and saved group IDs are all the fixed virtual-root
    /// constant 0, matching the ptrace CLONE_NEWUSER identity and deterministic
    /// across --verify and record/replay.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#1549)
    pub async fn handle_getresgid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getresgid,
    ) -> Result<i64, Error> {
        if let Some(rgid) = call.rgid() {
            guest.memory().write_value(rgid, &0)?;
        }
        if let Some(egid) = call.egid() {
            guest.memory().write_value(egid, &0)?;
        }
        if let Some(sgid) = call.sgid() {
            guest.memory().write_value(sgid, &0)?;
        }
        Ok(0)
    }

    /// get_mempolicy under Hermit. The container exposes a single virtual NUMA
    /// node, so the effective policy is always the default and every address
    /// resolves to node 0. The result is fully emulated (never injected), so it
    /// is bitwise-identical across the two --verify runs and under record/replay,
    /// removing the host-NUMA-topology dependence a passthrough would introduce.
    ///
    /// It follows Linux's `kernel_get_mempolicy` and `do_get_mempolicy`
    /// (mm/mempolicy.c) for a kernel with one node (`nr_node_ids` 1):
    /// - a node mask with `maxnode` below 1 is EINVAL;
    /// - an unknown flag (any bit beyond `MPOL_F_NODE`, `MPOL_F_ADDR` and
    ///   `MPOL_F_MEMS_ALLOWED`, including those above bit 31 of the raw
    ///   argument, which Reverie's typed call drops), `MPOL_F_MEMS_ALLOWED`
    ///   with `MPOL_F_NODE` or `MPOL_F_ADDR`, and an address without
    ///   `MPOL_F_ADDR` are EINVAL;
    /// - `*policy` is 0: `MPOL_DEFAULT`, or node 0 for `MPOL_F_NODE`;
    /// - the node mask is written as `copy_nodes_to_user` writes it: node 0
    ///   alone for `MPOL_F_MEMS_ALLOWED`, as `/proc/<pid>/status` reports
    ///   `Mems_allowed_list` (<https://github.com/rrnewton/hermit/issues/4214>),
    ///   and no node for the default policy, in `ALIGN(maxnode - 1, 64) / 8`
    ///   bytes (none for a `maxnode` of 1), EINVAL beyond a page;
    /// - both are written with the guest's own access rights, so a read-only or
    ///   unmapped buffer is EFAULT, as from `put_user` and `copy_to_user`.
    ///
    /// Limits: `set_mempolicy` and `mbind` succeed without recording a policy,
    /// so this always reports the default policy, where Linux reports, say,
    /// `MPOL_INTERLEAVE` after `set_mempolicy(MPOL_INTERLEAVE)`. For the same
    /// reason `MPOL_F_NODE` without `MPOL_F_ADDR` answers node 0, as Linux does
    /// for an interleave policy (libnuma's `numa_get_interleave_node`), where
    /// it refuses it under the default policy. `MPOL_F_ADDR` does not check
    /// that the address is mapped, where Linux answers EFAULT for an unmapped
    /// one.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#720)
    pub async fn handle_get_mempolicy<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::GetMempolicy,
    ) -> Result<i64, Error> {
        const MPOL_F_NODE: u64 = 1;
        const MPOL_F_ADDR: u64 = 2;
        const MPOL_F_MEMS_ALLOWED: u64 = 4;
        const PAGE: u64 = 4096;
        let mask = call.nodemask();
        if mask.is_some() && call.maxnode() < 1 {
            return Err(Errno::EINVAL.into());
        }
        // The flags argument is an unsigned long; Reverie decodes it as u32.
        let flags = guest.regs().await.r8;
        if flags & !(MPOL_F_NODE | MPOL_F_ADDR | MPOL_F_MEMS_ALLOWED) != 0 {
            return Err(Errno::EINVAL.into());
        }
        let mems_allowed = flags & MPOL_F_MEMS_ALLOWED != 0;
        if mems_allowed {
            if flags & (MPOL_F_NODE | MPOL_F_ADDR) != 0 {
                return Err(Errno::EINVAL.into());
            }
        } else if flags & MPOL_F_ADDR == 0 && call.addr().is_some() {
            return Err(Errno::EINVAL.into());
        }
        if let Some(policy) = call.policy() {
            let policy = AddrMut::<u8>::from_raw(policy.as_raw()).ok_or(Errno::EFAULT)?;
            write_get_mempolicy_output(&mut guest.memory(), policy, &0i32.to_ne_bytes())?;
        }
        if let Some(mask) = mask {
            let bytes = (call.maxnode() - 1).div_ceil(64) * 8;
            if bytes > PAGE.max(8) {
                return Err(Errno::EINVAL.into());
            }
            let mut words = vec![0u8; bytes as usize];
            if mems_allowed && !words.is_empty() {
                words[0] = 1;
            }
            if !words.is_empty() {
                let mask = AddrMut::<u8>::from_raw(mask.as_raw()).ok_or(Errno::EFAULT)?;
                write_get_mempolicy_output(&mut guest.memory(), mask, &words)?;
            }
        }
        Ok(0)
    }

    /// move_pages under Hermit. On a single virtual NUMA node nothing can be
    /// relocated, so report every page as residing on node 0 and succeed. The
    /// answer is a fixed constant, so it is deterministic across --verify and
    /// record/replay.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#720)
    pub async fn handle_move_pages<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::MovePages,
    ) -> Result<i64, Error> {
        // When a status buffer is supplied (either a real move request or a
        // location query with nodes == NULL), report node 0 for every page.
        if let Some(status) = call.status() {
            let count = call.nr_pages() as usize;
            let zeros = vec![0i32; count];
            guest.memory().write_values(status, &zeros)?;
        }
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D28a: on an in-guest run's default route Detcore holds 0 and every
    /// signal it sends itself, as on ptrace; SIGKILL keeps the kernel's path;
    /// SIGSTOP, SIGALRM and the realtime signals are refused; a value above 64
    /// is the kernel's EINVAL. Where Detcore delivers them (ptrace), nothing
    /// is refused. Where it does neither (every other backend), everything is
    /// the kernel's.
    #[test]
    fn parent_death_signals_are_held_refused_or_the_kernels() {
        use ParentDeathSignalRoute::*;
        for signal in 0..=66_u64 {
            let held = signal == 0 || detcore_sends_parent_death_signal(signal);
            let in_guest = parent_death_signal_route(signal, true, true);
            let ptrace = parent_death_signal_route(signal, true, false);
            let other = parent_death_signal_route(signal, false, false);
            assert_eq!(other, Kernel, "{signal}");
            assert_eq!(ptrace, if held { Hold } else { Kernel }, "{signal}");
            let expected = if held {
                Hold
            } else if signal == libc::SIGKILL as u64 || signal > 64 {
                Kernel
            } else {
                Refuse
            };
            assert_eq!(in_guest, expected, "{signal}");
        }
        // The values the brief names.
        for (signal, route) in [
            (libc::SIGUSR1, Hold),
            (libc::SIGUSR2, Hold),
            (0, Hold),
            (libc::SIGKILL, Kernel),
            (libc::SIGSTOP, Refuse),
            (libc::SIGALRM, Refuse),
            (32, Refuse),
            (64, Refuse),
            (65, Kernel),
        ] {
            assert_eq!(
                parent_death_signal_route(signal as u64, true, true),
                route,
                "{signal}"
            );
        }
    }

    /// Detcore holds parent-death signals where it delivers them (ptrace,
    /// sequentialized) and on an in-guest run's default route, and only
    /// delivers them on ptrace.
    #[test]
    fn holding_a_parent_death_signal_is_split_from_delivering_it() {
        let ptrace = crate::config::Config {
            sequentialize_threads: true,
            backend: BackendCapabilities::PTRACE,
            ..Default::default()
        };
        assert!(delivers_parent_death_signal(&ptrace));
        assert!(holds_parent_death_signal(&ptrace));
        let unsequentialized = crate::config::Config {
            sequentialize_threads: false,
            ..ptrace.clone()
        };
        assert!(!delivers_parent_death_signal(&unsequentialized));
        assert!(!holds_parent_death_signal(&unsequentialized));
        let in_guest = crate::config::Config {
            sequentialize_threads: true,
            backend: BackendCapabilities::LITEINST_IN_GUEST,
            in_guest_refuses_host_timed_producers: true,
            ..Default::default()
        };
        assert!(!delivers_parent_death_signal(&in_guest));
        assert!(holds_parent_death_signal(&in_guest));
        let off = crate::config::Config {
            in_guest_refuses_host_timed_producers: false,
            ..in_guest
        };
        assert!(!holds_parent_death_signal(&off));
    }

    mod group_mirror {
        use std::sync::Mutex;

        use reverie::GlobalRPC;
        use reverie::GlobalTool;
        use reverie::Tid;
        use reverie::Tool;
        use reverie::syscalls::Addr;
        use reverie::syscalls::AddrMut;
        use reverie::syscalls::SyscallInfo;

        use super::*;
        use crate::config::Config;
        use crate::tool_global::GlobalState;

        /// A guest whose backend answers the mirrored `setsid` with `answer`,
        /// recording `refusal` as its typed refusal when the answer fails.
        struct MirrorGuest<'a> {
            config: &'a Config,
            global: &'a GlobalState,
            thread: crate::ThreadState<()>,
            sender: Tid,
            answer: Result<i64, Errno>,
            refusal: Option<reverie::UnsupportedOperation>,
            recorded: Option<reverie::UnsupportedRefusal>,
            injections: Mutex<usize>,
            /// The one call this guest expects to be injected.
            expected: Sysno,
        }

        struct NoStack;
        struct NoStackGuard;
        impl Drop for NoStackGuard {
            fn drop(&mut self) {}
        }
        impl reverie::Stack for NoStack {
            type StackGuard = NoStackGuard;
            fn size(&self) -> usize {
                panic!("a mirror must not use the guest stack")
            }
            fn capacity(&self) -> usize {
                panic!("a mirror must not use the guest stack")
            }
            fn push<'stack, T>(&mut self, _: T) -> Addr<'stack, T> {
                panic!("a mirror must not use the guest stack")
            }
            fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
                panic!("a mirror must not use the guest stack")
            }
            fn commit(self) -> Result<Self::StackGuard, Errno> {
                panic!("a mirror must not use the guest stack")
            }
        }

        #[reverie::tool]
        impl GlobalRPC<GlobalState> for MirrorGuest<'_> {
            async fn send_rpc(
                &self,
                message: <GlobalState as GlobalTool>::Request,
            ) -> <GlobalState as GlobalTool>::Response {
                self.global.receive_rpc(self.sender, message).await
            }
            fn config(&self) -> &Config {
                self.config
            }
        }

        #[reverie::tool]
        impl Guest<Detcore> for MirrorGuest<'_> {
            type Memory = reverie::syscalls::LocalMemory;
            type Stack = NoStack;

            fn tid(&self) -> Tid {
                self.sender
            }
            fn pid(&self) -> Tid {
                self.sender
            }
            fn ppid(&self) -> Option<Tid> {
                None
            }
            fn memory(&self) -> Self::Memory {
                reverie::syscalls::LocalMemory::new()
            }
            fn thread_state(&self) -> &crate::ThreadState<()> {
                &self.thread
            }
            fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
                &mut self.thread
            }
            async fn regs(&mut self) -> libc::user_regs_struct {
                panic!("a mirror must not read registers")
            }
            async fn stack(&mut self) -> Self::Stack {
                panic!("a mirror must not use a guest stack")
            }
            async fn daemonize(&mut self) {
                panic!("a mirror must not daemonize")
            }
            async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
                assert_eq!(call.number(), self.expected);
                *self.injections.lock().unwrap() += 1;
                if let Err(errno) = self.answer {
                    self.recorded = self.refusal.map(|operation| {
                        reverie::UnsupportedRefusal::new(operation, errno, "test backend")
                    });
                }
                self.answer
            }
            fn take_unsupported_refusal(&mut self) -> Option<reverie::UnsupportedRefusal> {
                self.recorded.take()
            }
            async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
                panic!("a mirror must not retire the guest")
            }
            fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
                panic!("a mirror must not set a timer")
            }
            fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
                panic!("a mirror must not set a timer")
            }
            fn read_clock(&mut self) -> Result<u64, Error> {
                panic!("a mirror must not read the clock")
            }
        }

        /// The negative control for the mirror of a group change the model
        /// accepted: the backend's success, or its typed statement that it
        /// keeps no process groups, lets the model's answer stand; any other
        /// failure, a bare ENOSYS (as a guest's seccomp filter could give)
        /// or an errno the model did not predict, refuses the run by name in
        /// a fail-closed run, never passing the errno on.
        #[tokio::test]
        async fn a_failed_mirror_stands_only_with_the_typed_process_group_refusal() {
            use reverie::UnsupportedOperation::Flock;
            use reverie::UnsupportedOperation::ProcessGroups;
            let process = DetPid::from_raw(23);
            for (answer, refusal, accepted) in [
                (Ok(23), None, true),
                (Err(Errno::ENOSYS), Some(ProcessGroups), true),
                (Err(Errno::ENOSYS), None, false),
                (Err(Errno::EPERM), None, false),
                (Err(Errno::ENOSYS), Some(Flock), false),
            ] {
                let config = Config {
                    panic_on_unsupported_syscalls: true,
                    exit_on_unsupported_syscall: true,
                    shutdown_on_unsupported_syscall: false,
                    ..Config::default()
                };
                let global = GlobalState::init_global_state(&config).await;
                let tool = Detcore::new(Tid::from_raw(process.as_raw()), &config);
                let mut thread = crate::ThreadState::new(process, &config, ());
                thread.detpid = Some(process);
                let mut guest = MirrorGuest {
                    config: &config,
                    global: &global,
                    thread,
                    sender: Tid::from_raw(process.as_raw()),
                    answer,
                    refusal,
                    recorded: None,
                    injections: Mutex::new(0),
                    expected: Sysno::setsid,
                };
                let result = tool
                    .mirror_group_change(
                        &mut guest,
                        reverie::syscalls::Syscall::Setsid(reverie::syscalls::Setsid::new()),
                    )
                    .await;
                match result {
                    Ok(()) if accepted => {}
                    Err(Error::Tool(error)) if !accepted => assert_eq!(
                        error
                            .downcast_ref::<crate::UnsupportedSyscallError>()
                            .unwrap()
                            .0,
                        Sysno::setsid,
                        "{answer:?} {refusal:?}"
                    ),
                    result => panic!("wrong mirror policy for {answer:?} {refusal:?}: {result:?}"),
                }
                assert_eq!(*guest.injections.lock().unwrap(), 1);
                assert_eq!(guest.recorded, None, "a reported refusal must be taken");
            }
        }

        /// The tripwire for the calls KVM leaves to the Tool, on the path of
        /// the clone3 race (https://github.com/rrnewton/hermit/issues/4531):
        /// a getpgrp from a nested PID namespace, the one Detcore handler
        /// that passes one of them through to the backend, comes back with
        /// the backend's `ToolDecided` refusal, and the tripwire then panics
        /// with its internal-failure marker. The test calls the handler and
        /// then the tripwire directly; it does not cover the tripwire's call
        /// site in `handle_syscall_event`.
        #[tokio::test]
        #[should_panic(expected = "HERMIT_INTERNAL_FAILURE class=tool-decided-injection")]
        async fn an_injected_tool_decided_call_ends_the_run() {
            let process = DetPid::from_raw(23);
            let config = Config::default();
            let global = GlobalState::init_global_state(&config).await;
            let tool = Detcore::new(Tid::from_raw(process.as_raw()), &config);
            let mut thread = crate::ThreadState::new(process, &config, ());
            thread.detpid = Some(process);
            thread.nested_pid_namespace = true;
            let mut guest = MirrorGuest {
                config: &config,
                global: &global,
                thread,
                sender: Tid::from_raw(process.as_raw()),
                answer: Err(Errno::ENOSYS),
                refusal: Some(reverie::UnsupportedOperation::ToolDecided),
                recorded: None,
                injections: Mutex::new(0),
                expected: Sysno::getpgrp,
            };
            let result = tool
                .handle_group_or_session_getter(
                    &mut guest,
                    reverie::syscalls::Syscall::Getpgrp(reverie::syscalls::Getpgrp::new()),
                )
                .await;
            assert_eq!(*guest.injections.lock().unwrap(), 1);
            assert!(
                matches!(result, Err(Error::Errno(Errno::ENOSYS))),
                "{result:?}"
            );
            crate::syscalls::helpers::fail_on_tool_decided_injection::<
                crate::record_or_replay::NoopTool,
                _,
            >(&mut guest, Sysno::getpgrp);
        }
    }

    struct ScatterMemory {
        bytes: Vec<u8>,
        outcomes: std::collections::VecDeque<Result<usize, Errno>>,
        writes: usize,
    }

    impl ScatterMemory {
        fn new(size: usize, outcomes: impl IntoIterator<Item = Result<usize, Errno>>) -> Self {
            Self {
                bytes: vec![0xa5; size],
                outcomes: outcomes.into_iter().collect(),
                writes: 0,
            }
        }
    }

    impl MemoryAccess for ScatterMemory {
        fn read_vectored(
            &self,
            _: &[std::io::IoSlice],
            _: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            panic!("canonical scatter must not read guest bytes")
        }
        fn write_vectored(
            &mut self,
            _: &[std::io::IoSlice],
            _: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            panic!("canonical scatter must use the user-access capability")
        }
        fn write_with_user_access(
            &mut self,
            address: AddrMut<u8>,
            bytes: &[u8],
        ) -> Result<usize, Errno> {
            self.writes += 1;
            let count = self.outcomes.pop_front().unwrap_or(Ok(bytes.len()))?;
            assert!(count <= bytes.len());
            let offset = address.as_raw() - 0x1000;
            self.bytes[offset..offset + count].copy_from_slice(&bytes[..count]);
            Ok(count)
        }
    }

    fn assert_copy_failure(error: Error, expected: Errno) {
        let Error::Tool(error) = error else {
            panic!("copy failure became a guest errno: {error:?}")
        };
        assert_eq!(
            error
                .downcast_ref::<crate::random::RandomCopyFailure>()
                .expect("typed copy failure")
                .errno(),
            expected
        );
    }

    fn scatter(memory: &mut ScatterMemory, lengths: &[usize], offset: u64) -> Result<usize, Error> {
        let mut base = 0x1000;
        let vectors: Vec<_> = lengths
            .iter()
            .map(|&len| {
                let vector = crate::iovecs::ImportedIovec { base, len };
                base += len;
                vector
            })
            .collect();
        fill_canonical_random_iovecs(memory, &vectors, 0, offset, &mut DefaultHasher::new())
    }

    #[test]
    fn get_mempolicy_output_faults_are_efault_and_backend_errors_are_tool_errors() {
        let address = AddrMut::<u8>::from_raw(0x1000).unwrap();
        let write = |outcome| {
            write_get_mempolicy_output(&mut ScatterMemory::new(8, [outcome]), address, &[1; 8])
        };
        assert!(write(Ok(8)).is_ok());
        for fault in [Ok(0), Ok(3), Err(Errno::EFAULT)] {
            assert!(
                matches!(write(fault), Err(Error::Errno(Errno::EFAULT))),
                "{fault:?}"
            );
        }
        for errno in [Errno::ENOSYS, Errno::ESRCH, Errno::EIO] {
            assert!(
                matches!(write(Err(errno)), Err(Error::Tool(_))),
                "{errno:?}"
            );
        }
    }

    #[test]
    fn random_scatter_distinguishes_fault_prefixes_from_backend_errors() {
        // Exercise both an earlier complete iovec and an earlier complete chunk.
        for lengths in [vec![3, 5], vec![RANDOM_FILL_CHUNK_BYTES + 5]] {
            let prefix = if lengths.len() == 2 {
                3
            } else {
                RANDOM_FILL_CHUNK_BYTES
            };
            for error in [Errno::EFAULT, Errno::EIO, Errno::ENOMEM] {
                let mut memory = ScatterMemory::new(prefix + 5, [Ok(prefix), Err(error)]);
                let result = scatter(&mut memory, &lengths, 7);
                match error {
                    Errno::EFAULT => assert_eq!(result.unwrap(), prefix),
                    _ => assert_copy_failure(result.unwrap_err(), error),
                }
                let expected: Vec<_> = (7..7 + prefix as u64)
                    .map(|index| canonical_random_device_byte(0, index))
                    .collect();
                assert_eq!(&memory.bytes[..prefix], expected);
                assert_eq!(&memory.bytes[prefix..], &[0xa5; 5]);
                assert_eq!(memory.writes, 2);
            }
        }
    }

    #[test]
    fn random_scatter_short_or_zero_copy_stops_without_touching_later_segments() {
        for first in [0, 2] {
            let mut memory = ScatterMemory::new(10, [Ok(first)]);
            let result = scatter(&mut memory, &[5, 5], 0);
            if first == 0 {
                assert!(matches!(result, Err(Error::Errno(Errno::EFAULT))));
            } else {
                assert_eq!(result.unwrap(), first);
                assert_eq!(&memory.bytes[..2], &[41, 114]);
            }
            assert!(memory.bytes[first..].iter().all(|&byte| byte == 0xa5));
            assert_eq!(memory.writes, 1);
        }
        let mut memory = ScatterMemory::new(10, [Ok(5), Ok(0)]);
        assert_eq!(scatter(&mut memory, &[5, 5], 0).unwrap(), 5);
        assert!(memory.bytes[5..].iter().all(|&byte| byte == 0xa5));
        assert_eq!(memory.writes, 2);
    }

    #[test]
    fn random_scatter_saturated_bytes_match_partitioned_calls() {
        for (offset, expected) in [(u64::MAX - 2, [78, 151, 224, 224]), (u64::MAX, [224; 4])] {
            let mut single = ScatterMemory::new(4, []);
            assert_eq!(scatter(&mut single, &[4], offset).unwrap(), 4);
            let mut partitioned = ScatterMemory::new(4, []);
            let mut hash = DefaultHasher::new();
            assert_eq!(
                fill_canonical_random_iovecs(
                    &mut partitioned,
                    &[crate::iovecs::ImportedIovec {
                        base: 0x1000,
                        len: 1
                    }],
                    0,
                    offset,
                    &mut hash
                )
                .unwrap(),
                1
            );
            assert_eq!(
                fill_canonical_random_iovecs(
                    &mut partitioned,
                    &[crate::iovecs::ImportedIovec {
                        base: 0x1001,
                        len: 3
                    }],
                    0,
                    offset.saturating_add(1),
                    &mut hash
                )
                .unwrap(),
                3
            );
            assert_eq!(single.bytes, expected);
            assert_eq!(partitioned.bytes, expected);
        }
    }

    #[test]
    fn random_scatter_backend_error_rolls_back_shared_cursor_after_eight_byte_prefix() {
        let fd = crate::fd::DetFd::new(
            3,
            nix::fcntl::OFlag::O_RDONLY,
            crate::fd::FdType::Rng,
            crate::types::OpenFileId::new(crate::types::DetTid::from_raw(1), 0),
        );
        for error in [Errno::EFAULT, Errno::EIO] {
            let mut memory = ScatterMemory::new(8, [Ok(4), Err(error)]);
            let result = fd.with_random_device_stream(|offset| scatter(&mut memory, &[8], offset));
            if error == Errno::EFAULT {
                assert_eq!(result.unwrap(), 4);
            } else {
                assert_copy_failure(result.unwrap_err(), Errno::EIO);
            }
            // First iteration commits four; the EIO iteration must not commit
            // its physically copied prefix or fabricate a successful result.
            assert_eq!(fd.random_device_offset(), 4);
            assert_eq!(&memory.bytes[4..], &[0xa5; 4]);
            assert_eq!(memory.writes, 2);
        }
    }

    #[test]
    fn prctl_support_covers_deterministic_controls() {
        for option in [
            libc::PR_SET_NAME,
            libc::PR_GET_NAME,
            libc::PR_SET_THP_DISABLE,
            libc::PR_GET_THP_DISABLE,
            // Deterministic per-process dumpability state.
            libc::PR_SET_DUMPABLE,
            libc::PR_GET_DUMPABLE,
            // Deterministic per-thread capability-retention flag used by setpriv.
            libc::PR_SET_KEEPCAPS,
            libc::PR_GET_KEEPCAPS,
            // Deterministic per-thread parent-death-signal flag used by setpriv.
            libc::PR_SET_PDEATHSIG,
            libc::PR_GET_PDEATHSIG,
        ] {
            assert!(is_supported_prctl_option(option));
        }

        assert!(!is_supported_prctl_option(libc::PR_SET_NO_NEW_PRIVS));
        // Detcore's serialized-execution contract
        // (`Tool::enforces_serialized_execution`) rests on refusing it.
        assert!(!is_supported_prctl_option(libc::PR_SET_SECCOMP));
        assert!(!is_supported_prctl_option(libc::PR_SET_TIMERSLACK));
        assert!(!is_supported_prctl_option(libc::PR_GET_TIMERSLACK));
    }

    /// The subreaper prctls are admitted only where Detcore's record of
    /// children decides waits and the kernel has re-parented by the time an
    /// exit reaches Detcore: ptrace, and e9patch preprocessing under ptrace,
    /// with threads sequentialized. Without sequentialization no exit reaches
    /// the scheduler, nothing re-parents, and a subreaper's wait would see
    /// ECHILD while its orphan lives, so the call keeps failing with ENOSYS.
    #[test]
    fn child_subreaper_prctls_are_admitted_only_where_detcore_models_them() {
        for backend in [BackendCapabilities::PTRACE, BackendCapabilities::E9PATCH] {
            assert!(models_child_subreapers(true, &backend), "{backend:?}");
            assert!(!models_child_subreapers(false, &backend), "{backend:?}");
        }
        for backend in [
            BackendCapabilities::LITEINST_IN_GUEST,
            BackendCapabilities::SABRE,
            BackendCapabilities::DBT,
            BackendCapabilities::KVM,
        ] {
            assert!(!models_child_subreapers(true, &backend), "{backend:?}");
            assert!(!models_child_subreapers(false, &backend), "{backend:?}");
        }
        assert!(!is_supported_prctl_option(libc::PR_SET_CHILD_SUBREAPER));
        assert!(!is_supported_prctl_option(libc::PR_GET_CHILD_SUBREAPER));
    }

    #[test]
    fn capability_set_prctls_are_supported_on_every_backend() {
        for option in [libc::PR_CAPBSET_DROP, libc::PR_CAP_AMBIENT] {
            assert!(is_supported_prctl_option(option));
        }
        assert!(!is_supported_prctl_option(libc::PR_SET_SECUREBITS));
    }

    #[test]
    fn getrandom_accepts_linux_flags() {
        for flags in [
            0,
            libc::GRND_NONBLOCK as usize,
            libc::GRND_RANDOM as usize,
            (libc::GRND_NONBLOCK | libc::GRND_RANDOM) as usize,
            libc::GRND_INSECURE as usize,
            (libc::GRND_NONBLOCK | libc::GRND_INSECURE) as usize,
            1_usize << 32,
        ] {
            assert!(
                validate_getrandom_flags(flags).is_ok(),
                "valid flags rejected: {flags:#x}"
            );
        }
    }

    #[test]
    fn getrandom_rejects_invalid_flags() {
        for flags in [
            0x8000_0000,
            (1_usize << 32) | 0x8000_0000,
            (libc::GRND_RANDOM | libc::GRND_INSECURE) as usize,
        ] {
            assert_eq!(validate_getrandom_flags(flags), Err(Errno::EINVAL));
        }
    }

    #[test]
    fn getrandom_caps_requests_at_linux_max_rw_count() {
        assert_eq!(getrandom_request_len(16), 16);
        assert_eq!(getrandom_request_len(usize::MAX), GETRANDOM_MAX_BYTES);
    }

    #[test]
    fn canonical_random_device_stream_matches_kvm_root_contract() {
        let first: Vec<_> = (0..8)
            .map(|index| canonical_random_device_byte(0, index))
            .collect();
        assert_eq!(first, [41, 114, 187, 4, 77, 150, 223, 40]);

        let continued: Vec<_> = (8..16)
            .map(|index| canonical_random_device_byte(0, index))
            .collect();
        assert_eq!(continued, [113, 186, 3, 76, 149, 222, 39, 112]);

        let seeded: Vec<_> = (0..16)
            .map(|index| canonical_random_device_byte(17, index))
            .collect();
        assert_eq!(
            seeded,
            [
                56, 114, 187, 4, 77, 150, 223, 40, 96, 186, 3, 76, 149, 222, 39, 112
            ]
        );
        assert_ne!(seeded, [first, continued].concat());
    }

    #[test]
    fn getpriority_reports_default_nice_for_every_target() {
        // Every valid PRIO_* selector reports the default nice (raw 20 = nice 0),
        // regardless of `who`. Real tools such as `renice -p <pid>` pass an
        // explicit pid, and getpriority never checks permissions, so this must
        // never be EPERM (the pre-fix stub only accepted PRIO_PROCESS/who==0).
        for which in [libc::PRIO_PROCESS, libc::PRIO_PGRP, libc::PRIO_USER] {
            assert_eq!(getpriority_result(which as i32), Ok(20));
        }
    }

    #[test]
    fn setpriority_accepts_any_change_for_valid_target() {
        // Nice is inert under Hermit, so any priority change for a valid target
        // succeeds as a no-op — including nonzero nice (`nice -n 5`, os.nice).
        for which in [libc::PRIO_PROCESS, libc::PRIO_PGRP, libc::PRIO_USER] {
            assert_eq!(setpriority_result(which as i32), Ok(0));
        }
    }

    #[test]
    fn get_and_set_priority_reject_unknown_which_with_einval() {
        // Match Linux: an unknown target selector faults with EINVAL, not EPERM.
        for which in [-1, 3, 42] {
            assert_eq!(getpriority_result(which), Err(Errno::EINVAL));
            assert_eq!(setpriority_result(which), Err(Errno::EINVAL));
        }
    }

    #[test]
    fn seccomp_tsync_null_probe_matches_linux_validation() {
        assert_eq!(
            seccomp_result(SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC, false,),
            Err(Errno::EFAULT)
        );
        assert_eq!(
            seccomp_result(SECCOMP_SET_MODE_FILTER, 1 << 31, false),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            seccomp_result(SECCOMP_SET_MODE_FILTER, 0, true),
            Err(Errno::EOPNOTSUPP)
        );
    }

    #[test]
    fn process_madvise_is_rejected_deterministically() {
        assert!(matches!(
            Detcore::<crate::record_or_replay::NoopTool>::handle_process_madvise(
                (-10_000_i32) as usize,
                0
            ),
            Err(Error::Errno(Errno::EBADF))
        ));
        assert!(matches!(
            Detcore::<crate::record_or_replay::NoopTool>::handle_process_madvise(3, 1),
            Err(Error::Errno(Errno::EINVAL))
        ));
        assert!(matches!(
            Detcore::<crate::record_or_replay::NoopTool>::handle_process_madvise(3, 0),
            Err(Error::Errno(Errno::EPERM))
        ));
    }

    #[test]
    fn syslog_exposes_an_empty_log_and_validates_actions() {
        for action in 0..=7 {
            assert_eq!(deterministic_syslog_result(action, 0), Ok(0));
        }
        for action in [9, 10] {
            assert_eq!(deterministic_syslog_result(action, 0), Ok(0));
        }
        assert_eq!(deterministic_syslog_result(8, 1), Ok(0));
        assert_eq!(deterministic_syslog_result(8, 8), Ok(0));
        assert_eq!(deterministic_syslog_result(8, 0), Err(Errno::EINVAL));
        assert_eq!(deterministic_syslog_result(8, 9), Err(Errno::EINVAL));
        assert_eq!(deterministic_syslog_result(11, 0), Err(Errno::EINVAL));
    }
}
