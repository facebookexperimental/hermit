//! Typed successor restoration under the genuinely redeemed native entry.

use std::io;

use detcore::native_exec::DetcoreExecSnapshotV2;
use detcore::native_exec::NativeExecCutBinding;
use detcore::native_exec_control::ExecTicket;
use detcore::native_exec_control::GlobalRequest as Q;
use detcore::native_exec_control::GlobalResponse as R;
use detcore_model::liteinst_native_exec_state as frame;
use reverie_inguest::guest::exec_private::SuccessorBinding;
use reverie_inguest::guest::rpc::CoordinatorRpc;
use reverie_liteinst::NativeSuccessorConsumer;
use reverie_liteinst::native_exec_state::NativeExecStatsV1;
use reverie_rpc_transport::successor::VerifiedSuccessorState;

use super::runtime_packet;

fn refusal(message: &'static str) -> io::Error {
    io::Error::other(message)
}

#[cfg(test)]
#[path = "consumer_tests.rs"]
mod tests;

/// Retains exact admitted frame data; no host decodes the embedded Tool.
pub(crate) struct Consumer {
    ticket: ExecTicket,
    admitted: SuccessorBinding,
    cut: frame::CutBinding,
    stats: Option<NativeExecStatsV1>,
    sequence: Option<u64>,
    restored: bool,
    completion_attempted: bool,
    cut_clock: Option<detcore::types::DetTime>,
}

impl Consumer {
    /// # Safety
    /// Only the covered private native initializer may call this after the
    /// real same-birth registry consumed this successor ticket. The transport
    /// has authenticated the whole version-2 frame against its independently
    /// protected expected digest. `physical_tid` is read from the actual owned
    /// control/current kernel task, never taken from the frame. No loader or
    /// Guest work may run before restoration, real post_exec and Complete.
    pub(crate) unsafe fn new(
        ticket: ExecTicket,
        state: &VerifiedSuccessorState,
        physical_tid: i32,
    ) -> io::Result<(Self, SuccessorBinding)> {
        let (cut, _, _) = frame::decode(state.bytes())
            .map_err(|_| refusal("native successor neutral frame is malformed"))?;
        if cut.physical_tid != physical_tid
            || cut.dettid.as_raw() != physical_tid
            || cut.config_digest != state.expected().config
        {
            return Err(refusal(
                "native successor admitted physical/Config cut differs",
            ));
        }
        let admitted = SuccessorBinding {
            generation: cut.prepared_mm.generation(),
            ticket: state.ticket_digest(),
            state: state.expected().state,
            config: state.expected().config,
        };
        Ok((
            Self {
                ticket,
                admitted,
                cut,
                stats: None,
                sequence: None,
                restored: false,
                completion_attempted: false,
                cut_clock: None,
            },
            admitted,
        ))
    }
}

// SAFETY: the sole caller is the genuinely redeemed private initializer above;
// its actual controller/client/fatal lineage excludes all Guest/observer work.
// The neutral cut was independently checked and retained by the registry,
// including live-table incarnation continuity; typed validation never treats
// the embedded Tool as its own binding or reconstructs kernel authority.
unsafe impl NativeSuccessorConsumer<detcore::Detcore> for Consumer {
    fn qualify_config(&self, config: &detcore::Config) -> io::Result<()> {
        super::qualify_config(config)
    }

    fn decode_verified(
        &mut self,
        config: &detcore::Config,
        state: &VerifiedSuccessorState,
        binding: SuccessorBinding,
    ) -> io::Result<(detcore::Detcore, detcore::ThreadState<()>)> {
        if binding != self.admitted || self.stats.is_some() || self.restored {
            return Err(refusal("native successor decode binding or phase differs"));
        }
        let (cut, tool, runtime) = frame::decode(state.bytes())
            .map_err(|_| refusal("native successor exact composite is malformed"))?;
        if cut != self.cut
            || binding.generation != cut.prepared_mm.generation()
            || binding.state != state.expected().state
            || binding.config != cut.config_digest
            || binding.ticket != state.ticket_digest()
        {
            return Err(refusal("native successor outer admission changed"));
        }
        let actual = runtime_packet::decode(runtime)
            .map_err(|_| refusal("native successor runtime envelope is malformed"))?;
        let stats = NativeExecStatsV1::decode(actual.stats)
            .map_err(|_| refusal("native successor actual stats are unsupported"))?;
        let binding = NativeExecCutBinding {
            dettid: cut.dettid,
            detpid: cut.detpid,
            physical_tid: cut.physical_tid,
            old_mm: cut.old_mm,
            old_files: cut.old_files,
            config_digest: cut.config_digest,
        };
        let snapshot = reverie_rpc_transport::codec::decode_exact_bounded::<
            DetcoreExecSnapshotV2,
            { frame::MAX_BYTES },
        >(tool)
        .map_err(|_| refusal("native successor Tool snapshot is malformed"))?;
        let parts = snapshot
            .into_validated_parts(&binding, config)
            .map_err(|_| refusal("native successor Tool disagrees with admitted neutral cut"))?;
        self.cut_clock = Some(parts.1.thread_logical_time.clone());
        self.stats = Some(stats);
        self.sequence = Some(actual.sequence);
        Ok(parts)
    }

    fn restore_fresh_runtime(&mut self, binding: SuccessorBinding) -> io::Result<()> {
        if binding != self.admitted || self.restored {
            return Err(refusal("native successor restoration is out of order"));
        }
        // Consume the attempt before any fallible physical/runtime operation.
        self.restored = true;
        detcore::detlog::check_empty_forwarding_cut()
            .map_err(|_| refusal("native successor DETLOG cut is not empty"))?;
        unsafe {
            self.stats
                .take()
                .ok_or_else(|| refusal("native successor stats absent"))?
                .restore_fresh()
                .map_err(|_| refusal("native successor runtime is not fresh"))?;
            crate::exec_initial::restore_dispatch_sequence(
                self.sequence
                    .take()
                    .ok_or_else(|| refusal("native successor sequence absent"))?,
            )
            .map_err(|_| refusal("native successor observer is not fresh"))?;
        }
        Ok(())
    }

    fn complete(
        &mut self,
        rpc: &CoordinatorRpc<detcore::GlobalState>,
        binding: SuccessorBinding,
    ) -> io::Result<SuccessorBinding> {
        if binding != self.admitted || !self.restored || self.completion_attempted {
            return Err(refusal("native successor completion is out of order"));
        }
        self.completion_attempted = true;
        // Complete stays scheduler-neutral, but post_exec may have produced
        // records after its last ordinary counted request. The same installed
        // RPC must account for that actual interval before readiness.
        let request = Q::WithForwardedCount(
            detcore::detlog::take_forwarded_since_request(),
            Box::new(Q::CompleteNativeExec {
                ticket: self.ticket,
            }),
        );
        let response = rpc
            .send_blocking_unless_in_flight((
                self.cut_clock
                    .take()
                    .ok_or_else(|| refusal("native successor actual cut clock absent"))?,
                self.cut.prepared_mm,
                request,
            ))
            .map_err(|_| refusal("native successor completion transport failed"))?;
        if response != (None, R::CompleteNativeExec(Ok(()))) {
            return Err(refusal("native successor completion ACK differs"));
        }
        detcore::detlog::check_empty_forwarding_cut()
            .map_err(|_| refusal("native successor completion has unflushed DETLOG"))?;
        // Equality is returned only after the genuine exact typed ACK. The
        // controller still verifies its physical finish proofs before ready.
        Ok(binding)
    }
}
