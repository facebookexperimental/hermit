/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The host kernel's spelling of `pidfd_send_signal`'s self sentinels.
//!
//! The guest sees Linux 6.17's ABI, whichever kernel runs it:
//! `PIDFD_SELF_THREAD` is -10000 and `PIDFD_SELF_THREAD_GROUP` is -10001
//! (include/uapi/linux/fcntl.h, v6.17). Linux 6.15 and 6.16 spell the group
//! sentinel -20000 (include/uapi/linux/pidfd.h, v6.15 and v6.16), and older
//! kernels have neither, so a canonical call forwarded unchanged to a 6.16
//! kernel fails with EBADF where the guest's kernel would have signalled it.
//!
//! The guest's call stays canonical through Detcore's model, the recording
//! and the replay. Only the physical injection is respelled, by
//! [`PhysicalSpelling`], and only to a spelling the backend qualified on the
//! kernel that executes its injections (`Guest::pidfd_self_spelling`). A
//! backend that could not qualify it gets a typed refusal
//! ([`PidfdSelfQualificationRefusal`]), never a guess.

use std::os::fd::RawFd;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::PidfdSelfSpelling;
use reverie::Tool;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;

/// `PIDFD_SELF_THREAD` as the guest spells it (Linux 6.17's ABI).
pub(crate) const GUEST_SELF_THREAD: RawFd = reverie::pidfd_self::PIDFD_SELF_THREAD;
/// `PIDFD_SELF_THREAD_GROUP` as the guest spells it (Linux 6.17's ABI).
pub(crate) const GUEST_SELF_THREAD_GROUP: RawFd =
    reverie::pidfd_self::PIDFD_SELF_THREAD_GROUP_MODERN;

/// What the preflight of a self-sentinel `pidfd_send_signal` decided
/// ([`preflight`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SentinelPreflight {
    /// The executing kernel spells the sentinel as the guest does: forward
    /// the call unchanged.
    Canonical,
    /// The executing kernel spells it differently: forward the call through
    /// this respelling.
    Respell(PhysicalRespelling),
    /// The backend declares that its executor does not perform the call.
    Unsupported,
    /// The backend could not qualify the executing kernel's spelling.
    Unknown,
}

/// The preflight of the guest's `pidfd_send_signal` `call`, whose `pidfd` is
/// one of the guest's self sentinels, given the backend's qualified
/// `spelling`.
pub(crate) fn preflight(
    spelling: PidfdSelfSpelling,
    call: Syscall,
    pidfd: RawFd,
) -> SentinelPreflight {
    let physical = match pidfd {
        GUEST_SELF_THREAD => spelling.thread(),
        GUEST_SELF_THREAD_GROUP => spelling.thread_group(),
        _ => unreachable!("preflight of {pidfd}, which is not a self sentinel"),
    };
    match (physical, spelling) {
        (Some(physical), _) if physical == pidfd => SentinelPreflight::Canonical,
        (Some(physical), _) => {
            let (_, canonical) = call.into_parts();
            SentinelPreflight::Respell(PhysicalRespelling::new(canonical, physical))
        }
        (None, PidfdSelfSpelling::Unsupported) => SentinelPreflight::Unsupported,
        (None, _) => SentinelPreflight::Unknown,
    }
}

/// One guest call and the physical form its injection takes: the same call
/// with the descriptor in the executing kernel's spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalRespelling {
    canonical: SyscallArgs,
    physical: SyscallArgs,
}

impl PhysicalRespelling {
    /// `canonical`'s respelling with `pidfd` as its descriptor. The kernel
    /// reads the descriptor as an `int`, so only the register's low 32 bits
    /// change, and every other register is kept bit for bit.
    fn new(canonical: SyscallArgs, pidfd: RawFd) -> Self {
        let physical = SyscallArgs {
            arg0: (canonical.arg0 & !0xffff_ffff) | pidfd as u32 as usize,
            ..canonical
        };
        Self {
            canonical,
            physical,
        }
    }

    /// The physical form of an injection of `call`, if `call` is exactly the
    /// canonical call: `pidfd_send_signal` with all six registers equal.
    /// Every other call is injected as it is.
    pub(crate) fn physical<S: SyscallInfo>(&self, call: S) -> Option<Syscall> {
        let (number, args) = call.into_parts();
        (number == Sysno::pidfd_send_signal && args == self.canonical)
            .then(|| Syscall::from_raw(number, self.physical))
    }
}

/// A Guest whose injections of one canonical call reach the kernel in its
/// physical spelling ([`PhysicalRespelling`]). Everything else, including
/// every other injection, is forwarded to the wrapped Guest unchanged.
///
/// Detcore wraps the Guest in this below its model and the record or replay
/// tool, which therefore see, record and compare only the canonical call.
pub(crate) struct PhysicalSpelling<'a, G: ?Sized, L> {
    inner: &'a mut G,
    respelling: PhysicalRespelling,
    _tool: std::marker::PhantomData<fn() -> L>,
}

impl<'a, G: ?Sized, L> PhysicalSpelling<'a, G, L> {
    pub(crate) fn new(inner: &'a mut G, respelling: PhysicalRespelling) -> Self {
        Self {
            inner,
            respelling,
            _tool: std::marker::PhantomData,
        }
    }
}

#[reverie::tool]
impl<'a, G, L> GlobalRPC<L::GlobalState> for PhysicalSpelling<'a, G, L>
where
    G: Guest<L> + ?Sized,
    L: Tool,
{
    async fn send_rpc(
        &self,
        message: <L::GlobalState as GlobalTool>::Request,
    ) -> <L::GlobalState as GlobalTool>::Response {
        self.inner.send_rpc(message).await
    }

    fn try_send_rpc_sync(
        &self,
        message: <L::GlobalState as GlobalTool>::Request,
    ) -> Result<<L::GlobalState as GlobalTool>::Response, <L::GlobalState as GlobalTool>::Request>
    {
        self.inner.try_send_rpc_sync(message)
    }

    fn config(&self) -> &<L::GlobalState as GlobalTool>::Config {
        self.inner.config()
    }
}

#[reverie::tool]
impl<'a, G, L> Guest<L> for PhysicalSpelling<'a, G, L>
where
    G: Guest<L> + ?Sized,
    L: Tool,
{
    type Memory = G::Memory;
    type Stack = G::Stack;

    fn tid(&self) -> Pid {
        self.inner.tid()
    }

    fn pid(&self) -> Pid {
        self.inner.pid()
    }

    fn controller_capture_fact(&self) -> Option<reverie::ControllerCaptureFact> {
        self.inner.controller_capture_fact()
    }

    fn ppid(&self) -> Option<Pid> {
        self.inner.ppid()
    }

    fn is_command_bootstrap(&self) -> bool {
        self.inner.is_command_bootstrap()
    }

    fn is_backend_runtime_bootstrap(&self) -> bool {
        self.inner.is_backend_runtime_bootstrap()
    }

    fn is_runtime_owned_fd(&self, fd: i32) -> bool {
        self.inner.is_runtime_owned_fd(fd)
    }

    fn runtime_private_memory_regions(&self) -> Option<reverie::RuntimePrivateMemory> {
        self.inner.runtime_private_memory_regions()
    }

    fn runtime_private_memory_is_current(
        &self,
        snapshot: &reverie::RuntimePrivateMemorySnapshot,
    ) -> bool {
        self.inner.runtime_private_memory_is_current(snapshot)
    }

    fn try_mapping_capture(
        &self,
        prepared: reverie::PreparedMappingCapture<<L::GlobalState as GlobalTool>::Request>,
    ) -> Result<<L::GlobalState as GlobalTool>::Response, reverie::MappingCaptureError> {
        self.inner.try_mapping_capture(prepared)
    }

    fn is_main_thread(&self) -> bool {
        self.inner.is_main_thread()
    }

    fn is_root_process(&self) -> bool {
        self.inner.is_root_process()
    }

    fn is_root_thread(&self) -> bool {
        self.inner.is_root_thread()
    }

    fn initial_stack_pointer(&self) -> Option<u64> {
        self.inner.initial_stack_pointer()
    }

    fn auxv(&self) -> reverie::Auxv {
        self.inner.auxv()
    }

    fn memory(&self) -> Self::Memory {
        self.inner.memory()
    }

    unsafe fn tool_syscall(&self, number: Sysno, args: [usize; 6]) -> Result<usize, Errno> {
        // SAFETY: forwarding preserves the caller's syscall and safety obligations.
        unsafe { self.inner.tool_syscall(number, args) }
    }

    fn thread_state_mut(&mut self) -> &mut L::ThreadState {
        self.inner.thread_state_mut()
    }

    fn thread_state(&self) -> &L::ThreadState {
        self.inner.thread_state()
    }

    async fn regs(&mut self) -> libc::user_regs_struct {
        self.inner.regs().await
    }

    async fn set_regs(&mut self, regs: libc::user_regs_struct) -> Result<(), Error> {
        self.inner.set_regs(regs).await
    }

    async fn stack(&mut self) -> Self::Stack {
        self.inner.stack().await
    }

    async fn daemonize(&mut self) {
        self.inner.daemonize().await
    }

    async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
        match self.respelling.physical(syscall) {
            Some(physical) => self.inner.inject(physical).await,
            None => self.inner.inject(syscall).await,
        }
    }

    async fn inject_with_purpose<S: SyscallInfo>(
        &mut self,
        syscall: S,
        purpose: reverie::InjectionPurpose,
    ) -> Result<i64, Error> {
        match self.respelling.physical(syscall) {
            Some(physical) => self.inner.inject_with_purpose(physical, purpose).await,
            None => self.inner.inject_with_purpose(syscall, purpose).await,
        }
    }

    async fn inject_with_retry<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
        match self.respelling.physical(syscall) {
            Some(physical) => self.inner.inject_with_retry(physical).await,
            None => self.inner.inject_with_retry(syscall).await,
        }
    }

    async fn inject_restartable<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
        match self.respelling.physical(syscall) {
            Some(physical) => self.inner.inject_restartable(physical).await,
            None => self.inner.inject_restartable(syscall).await,
        }
    }

    async fn tail_inject<S: SyscallInfo>(&mut self, syscall: S) -> reverie::Never {
        #![allow(unreachable_code)]
        match self.respelling.physical(syscall) {
            Some(physical) => self.inner.tail_inject(physical).await,
            None => self.inner.tail_inject(syscall).await,
        }
    }

    async fn cancel_current_thread(&mut self) -> reverie::Never {
        self.inner.cancel_current_thread().await
    }

    async fn retire_current_thread(&mut self) -> reverie::Never {
        self.inner.retire_current_thread().await
    }

    async fn defer_signal_delivery(&mut self, event: reverie::SignalEvent) -> Result<(), Error> {
        self.inner.defer_signal_delivery(event).await
    }

    async fn defer_signal_delivery_before_syscall(
        &mut self,
        event: reverie::SignalEvent,
    ) -> Result<(), Error> {
        self.inner.defer_signal_delivery_before_syscall(event).await
    }

    async fn queue_child_exit_signal(
        &mut self,
        event: reverie::SignalEvent,
    ) -> reverie::ChildExitSignalOutcome {
        self.inner.queue_child_exit_signal(event).await
    }

    async fn queue_process_alarm_signal(
        &mut self,
        event: reverie::SignalEvent,
    ) -> reverie::ProcessAlarmSignalOutcome {
        self.inner.queue_process_alarm_signal(event).await
    }

    fn signal_info(&self) -> Option<[u8; reverie::SIGNAL_INFO_SIZE]> {
        self.inner.signal_info()
    }

    fn set_signal_info(&mut self, info: [u8; reverie::SIGNAL_INFO_SIZE]) -> Result<(), Errno> {
        self.inner.set_signal_info(info)
    }

    async fn deliver_signal(
        &mut self,
        delivery: reverie::SignalDelivery,
    ) -> reverie::SignalDeliveryOutcome {
        self.inner.deliver_signal(delivery).await
    }

    fn signal_report_path(&self) -> Option<reverie::SignalReportPath> {
        self.inner.signal_report_path()
    }

    fn signal_task_identity(&self) -> Option<reverie::SignalTaskIdentity> {
        self.inner.signal_task_identity()
    }

    fn signal_control_mode(&self) -> reverie::BackendSignalControlMode {
        self.inner.signal_control_mode()
    }

    fn parked_signal_site(&self) -> Option<reverie::CallbackSignalSite> {
        self.inner.parked_signal_site()
    }

    fn polled_read_signal_site(
        &self,
        call: reverie::syscalls::Read,
    ) -> Option<reverie::CallbackSignalSite> {
        self.inner.polled_read_signal_site(call)
    }

    fn captured_write_signal_site(
        &self,
        call: reverie::syscalls::Write,
    ) -> Option<reverie::CallbackSignalSite> {
        self.inner.captured_write_signal_site(call)
    }

    fn parent_death_syscall_preflight(
        &self,
        call: Syscall,
    ) -> Result<reverie::ParentDeathSyscallAdmission, Error> {
        self.inner.parent_death_syscall_preflight(call)
    }

    fn signal_observation_lease(&self) -> Option<reverie::ParkedObservationLease> {
        self.inner.signal_observation_lease()
    }

    async fn observe_parked_signal(
        &mut self,
        site: reverie::CallbackSignalSite,
        lease: reverie::ParkedObservationLease,
    ) -> Result<reverie::ParkedSignalObservation, reverie::SignalObservationFailure> {
        self.inner.observe_parked_signal(site, lease).await
    }

    async fn terminate_from_parked_signal(
        &mut self,
        selection: reverie::PreparedSignalToken,
    ) -> Result<reverie::Never, reverie::SignalObservationFailure> {
        self.inner.terminate_from_parked_signal(selection).await
    }

    fn parked_signal_failure_context(&self) -> Option<reverie::ParkedSignalFailureContext> {
        self.inner.parked_signal_failure_context()
    }

    async fn cancel_parked_signal(
        &mut self,
        context: reverie::ParkedSignalFailureContext,
    ) -> Result<reverie::Never, reverie::SignalObservationFailure> {
        self.inner.cancel_parked_signal(context).await
    }

    async fn set_blocked_signals(&mut self, mask: u64) -> Result<u64, reverie::MaskChangeRefused> {
        self.inner.set_blocked_signals(mask).await
    }

    fn resume_signal_mask(&self) -> Option<u64> {
        self.inner.resume_signal_mask()
    }

    fn set_timer(&mut self, sched: reverie::TimerSchedule) -> Result<(), Error> {
        self.inner.set_timer(sched)
    }

    fn set_timer_precise(&mut self, sched: reverie::TimerSchedule) -> Result<(), Error> {
        self.inner.set_timer_precise(sched)
    }

    fn read_clock(&mut self) -> Result<u64, Error> {
        self.inner.read_clock()
    }

    fn backtrace(&mut self) -> Option<reverie::Backtrace> {
        self.inner.backtrace()
    }

    fn has_cpuid_interception(&self) -> bool {
        self.inner.has_cpuid_interception()
    }

    fn detlog_memory_regions(&self) -> Option<Vec<reverie::DetlogMemoryRegion>> {
        self.inner.detlog_memory_regions()
    }

    fn take_unsupported_refusal(&mut self) -> Option<reverie::UnsupportedRefusal> {
        self.inner.take_unsupported_refusal()
    }

    fn injected_syscalls(&self) -> Option<u64> {
        self.inner.injected_syscalls()
    }

    fn pidfd_self_spelling(&self) -> PidfdSelfSpelling {
        self.inner.pidfd_self_spelling()
    }

    fn storable_memory_ranges(&self) -> Result<Vec<(u64, u64)>, Error> {
        self.inner.storable_memory_ranges()
    }

    fn user_address_limit(&self) -> Result<reverie::UserAddressLimit, Error> {
        self.inner.user_address_limit()
    }
}

/// The respelling armed on this thread for the duration of one
/// `pidfd_send_signal` handler (`ThreadState::pidfd_respelling`), restored to
/// what it was when the handler returns, fails, or its future is dropped.
///
/// The guard owns the handler's Guest, which the handler borrows back from it,
/// so no borrow of the thread state is held across the handler's awaits.
pub(crate) struct ArmedRespelling<'g, T: RecordOrReplay, G: Guest<Detcore<T>>> {
    guest: &'g mut G,
    previous: Option<PhysicalRespelling>,
    _tool: std::marker::PhantomData<fn() -> T>,
}

impl<'g, T: RecordOrReplay, G: Guest<Detcore<T>>> ArmedRespelling<'g, T, G> {
    /// Arms `respelling` on `guest`'s thread.
    pub(crate) fn arm(guest: &'g mut G, respelling: Option<PhysicalRespelling>) -> Self {
        let previous =
            std::mem::replace(&mut guest.thread_state_mut().pidfd_respelling, respelling);
        Self {
            guest,
            previous,
            _tool: std::marker::PhantomData,
        }
    }
}

impl<T: RecordOrReplay, G: Guest<Detcore<T>>> std::ops::Deref for ArmedRespelling<'_, T, G> {
    type Target = G;

    fn deref(&self) -> &G {
        self.guest
    }
}

impl<T: RecordOrReplay, G: Guest<Detcore<T>>> std::ops::DerefMut for ArmedRespelling<'_, T, G> {
    fn deref_mut(&mut self) -> &mut G {
        self.guest
    }
}

impl<T: RecordOrReplay, G: Guest<Detcore<T>>> Drop for ArmedRespelling<'_, T, G> {
    fn drop(&mut self) {
        self.guest.thread_state_mut().pidfd_respelling = self.previous;
    }
}

/// A `pidfd_send_signal` through a self sentinel that Detcore refused because
/// the backend could not qualify how the kernel executing its injections
/// spells that sentinel (`PidfdSelfSpelling::Unknown`): forwarding the guest's
/// spelling could signal nothing on an older kernel (EBADF where the guest's
/// kernel would have signalled), and respelling it would be a guess.
///
/// A policy refusal: the CLI ends the run with `HERMIT_POLICY_REFUSAL_EXIT`.
/// It is filed with the coordinator before it is returned
/// (`GlobalRequest::PidfdSelfQualificationRefusal`), which is how a backend
/// whose Tool error stops at the guest process (in-guest, DBT) reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidfdSelfQualificationRefusal {
    /// The sentinel the guest passed, in the guest's (Linux 6.17) spelling.
    pub sentinel: RawFd,
}

impl std::fmt::Display for PidfdSelfQualificationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pidfd_send_signal through the self sentinel {} is refused: this backend could \
             not qualify how the kernel that executes its syscalls spells the PIDFD_SELF_* \
             sentinels (Linux 6.15 and 6.16 spell the thread group -20000, 6.17 and later \
             -10001, earlier kernels have neither), so Hermit cannot forward the guest's call \
             faithfully",
            self.sentinel
        )
    }
}

impl std::error::Error for PidfdSelfQualificationRefusal {}

impl<T: RecordOrReplay> Detcore<T> {
    /// The answer to a self-sentinel `pidfd_send_signal` whose backend
    /// declares that its executor does not perform the call
    /// (`PidfdSelfSpelling::Unsupported`): named on stderr, which reaches the
    /// caller with no tracing subscriber and no report descriptor, and
    /// reported as an unsupported syscall; then the unsupported-operation
    /// policy, unchanged. The typed error of a policy that returns one carries
    /// the reason as context.
    pub(crate) async fn refuse_unsupported_pidfd_self<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pidfd: RawFd,
    ) -> Result<i64, Error> {
        use std::io::Write as _;
        let reason = format!(
            "pidfd_send_signal through the self sentinel {pidfd} cannot be serviced: this \
             backend's syscall executor declares that it does not perform pidfd_send_signal"
        );
        let message = format!(
            "hermit: [detcore, dtid {}] {reason}",
            guest.thread_state().dettid
        );
        tracing::error!("{message}");
        let _ = writeln!(crate::util::RetryingStderr, "{message}");
        crate::tool_global::report_unsupported_syscall(guest, Sysno::pidfd_send_signal).await;
        self.refuse_unserviceable_operation_because(
            guest,
            Sysno::pidfd_send_signal,
            Errno::ENOSYS,
            &reason,
        )
        .await
        .map_err(|error| match error {
            Error::Tool(error) => Error::Tool(error.context(reason)),
            error => error,
        })
    }

    /// The typed refusal of a self-sentinel `pidfd_send_signal` whose backend
    /// could not qualify the executing kernel's spelling
    /// ([`PidfdSelfQualificationRefusal`]), whatever the unsupported-syscall
    /// policy: filed with the coordinator, then returned. Nothing is forwarded
    /// and no exit is taken here; the Tool error ends the run through the
    /// backend's own error path.
    pub(crate) async fn refuse_unqualified_pidfd_self<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pidfd: RawFd,
    ) -> Result<i64, Error> {
        let refusal = PidfdSelfQualificationRefusal { sentinel: pidfd };
        tracing::error!("[detcore, dtid {}] {refusal}", guest.thread_state().dettid);
        crate::tool_global::report_pidfd_self_qualification_refusal(guest, refusal.clone()).await;
        Err(Error::Tool(anyhow::Error::new(refusal)))
    }
}
