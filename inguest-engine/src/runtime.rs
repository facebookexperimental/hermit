//! One concrete runtime parameter for the one compiled Detcore Tool host.
//!
//! Backend service objects are constructed by the existing typed installer
//! inside this DSO. No Rust trait object, future, reference, allocation or
//! ownership token crosses the C shim boundary.

use std::io;

use reverie::Pid;
use reverie_inguest::guest::event::SyscallEvent;
use reverie_inguest::guest::host::HostRuntime;
use reverie_inguest::guest::tool_install::PatchHost;
use reverie_inguest::guest::tool_install::TrapHost;
use reverie_inguest::guest::tool_install::TrapOptions;
use reverie_inguest::guest::tool_install::TrapRuntime;

pub(crate) struct EngineRuntime {
    services: Box<dyn HostRuntime>,
}

impl EngineRuntime {
    pub(crate) fn new(services: Box<dyn HostRuntime>) -> Self {
        Self { services }
    }
}

impl HostRuntime for EngineRuntime {
    fn fork_child_rebind(&self) {
        self.services.fork_child_rebind();
    }

    fn emit_stage(&self, stage: &[u8]) {
        self.services.emit_stage(stage);
    }

    fn observe_native_entry(
        &self,
        stage: reverie_inguest::guest::native_services::NativeEntryStage,
        observation: reverie_inguest::guest::native_services::NativeEntryObservation<'_, '_>,
    ) {
        self.services.observe_native_entry(stage, observation);
    }

    fn fork_child_reset(&self, event: &SyscallEvent) {
        self.services.fork_child_reset(event);
    }

    fn cpuid_interception_enabled(&self) -> bool {
        self.services.cpuid_interception_enabled()
    }

    fn exit_process_stats(&self, tid: Pid) -> io::Result<()> {
        self.services.exit_process_stats(tid)
    }

    fn read_clock(&self) -> io::Result<u64> {
        self.services.read_clock()
    }

    fn signal_action_supported(&self, number: i64, args: [u64; 6]) -> bool {
        self.services.signal_action_supported(number, args)
    }

    fn reserved_signal_mask(&self) -> u64 {
        self.services.reserved_signal_mask()
    }
}

/// Add the same real allocator preflight as the LiteInst root to TrapHost;
/// all continuation, vDSO, signal, clock, RPC and lifecycle mechanisms stay
/// the existing shared implementation.
pub(crate) struct FrameHookHost(TrapHost);

impl FrameHookHost {
    pub(crate) fn new() -> Self {
        Self(TrapHost::new(TrapOptions::default()))
    }
}

// SAFETY: TrapHost owns every delegated mechanism and keeps its original
// PatchHost contract. The added step only checks the current allocator.
unsafe impl PatchHost for FrameHookHost {
    type Runtime = TrapRuntime;

    fn patcher(&self) -> &'static reverie_inguest::guest::runtime_seam::PatcherRef {
        self.0.patcher()
    }

    fn preflight(&self) -> io::Result<()> {
        reverie_liteinst::preflight_tool_allocator()?;
        self.0.preflight()
    }

    fn stage_stream(&self) -> io::Result<bool> {
        self.0.stage_stream()
    }

    fn process_forks_allowed(&self) -> io::Result<bool> {
        self.0.process_forks_allowed()
    }

    fn sigalrm_handlers(&self) -> io::Result<bool> {
        self.0.sigalrm_handlers()
    }

    fn runtime_config(&self) -> io::Result<reverie_inguest::lifecycle::RuntimeConfig> {
        self.0.runtime_config()
    }

    fn runtime(&self, stats: reverie_inguest::stats::GuestStatsHooks) -> TrapRuntime {
        self.0.runtime(stats)
    }
}
