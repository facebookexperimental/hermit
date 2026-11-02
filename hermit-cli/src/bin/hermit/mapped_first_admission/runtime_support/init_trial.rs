//! Bounded intermediate genuine initializer experiment, not a landing qualifier.
//! Existing real RPC/DETLOG owners run while the original ptracer observes.
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Instant;

use detcore::detlog::NativePhysicalWatchIdentity;
use reverie::Tool;
use reverie_inguest::launcher::exec_stop::physical::BootstrapEntryCoordinates;
use reverie_inguest::launcher::exec_stop::physical::ObservationDomain;
use reverie_inguest::launcher::exec_stop::physical::PreparedMapping;

use super::Admission;
use super::DirectoryBoundServer;
use super::Serving;
use super::WAIT;
use super::need;
use super::rpc;

const RECORD_LIMIT: usize = 8 * 1024 * 1024;

/// The sink owns the complete stream in both modes, and successful completion
/// requires forwarded records in both. Only finite qualification additionally
/// retains the historical bounded byte observation. This never selects RPC,
/// stats or physical cleanup policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservationPolicy {
    Production,
    Qualification,
}
impl ObservationPolicy {
    pub(crate) fn is_qualification(self) -> bool {
        self == Self::Qualification
    }
}

pub(crate) struct RecordObservation {
    pub(crate) messages: u64,
    pub(crate) bytes: u64,
    // None means no duplicate capture was requested, not a complete empty log.
    retained: Option<Vec<u8>>,
}
impl RecordObservation {
    pub(crate) fn required_records_absent(&self) -> bool {
        self.messages == 0 || self.retained.as_ref().is_some_and(Vec::is_empty)
    }
}

struct CapturedRecords {
    messages: u64,
    observed_bytes: u64,
    retained: Option<Vec<u8>>,
    overflow: bool,
    counter_overflow: bool,
}
impl CapturedRecords {
    fn new(policy: ObservationPolicy) -> Self {
        Self {
            messages: 0,
            observed_bytes: 0,
            retained: policy.is_qualification().then(Vec::new),
            overflow: false,
            counter_overflow: false,
        }
    }
    fn observe(&mut self, record: &[u8]) {
        match (
            self.messages.checked_add(1),
            self.observed_bytes.checked_add(record.len() as u64),
        ) {
            (Some(messages), Some(bytes)) => {
                self.messages = messages;
                self.observed_bytes = bytes;
            }
            _ => self.counter_overflow = true,
        }
        if let Some(bytes) = self.retained.as_mut() {
            if record.len() > RECORD_LIMIT.saturating_sub(bytes.len()) {
                self.overflow = true;
            } else {
                bytes.extend_from_slice(record);
            }
        }
    }
    fn snapshot(&self) -> RecordObservation {
        RecordObservation {
            messages: self.messages,
            bytes: self.observed_bytes,
            retained: self.retained.clone(),
        }
    }
}

fn observing_sink(
    records: Arc<Mutex<CapturedRecords>>,
    mut output: detcore::detlog::ForwardedRecordSink,
) -> detcore::detlog::ForwardedRecordSink {
    Box::new(move |record| {
        // Invoke the full sink before local DATA observation. A callback count
        // is not delivery proof: SharedLog's errors remain mandatory at close.
        output(record);
        records.lock().unwrap().observe(record);
    })
}
pub(crate) struct Trial {
    serving: Option<Serving>,
    records: Arc<Mutex<CapturedRecords>>,
    source_registered: bool,
    request_cancellation: Option<reverie_rpc_transport::RequestCancellation>,
}
impl Trial {
    #[cfg(test)]
    pub(crate) fn register(receiver: OwnedFd) -> Self {
        Self::register_with_sink(receiver, Box::new(|_| {}), ObservationPolicy::Qualification)
    }
    /// Register the actual source into the existing full-record sink. Production
    /// counts every callback without imposing the qualification capture bound.
    pub(crate) fn register_with_sink(
        receiver: OwnedFd,
        output: detcore::detlog::ForwardedRecordSink,
        observation: ObservationPolicy,
    ) -> Self {
        let records = Arc::new(Mutex::new(CapturedRecords::new(observation)));
        detcore::detlog::set_forwarded_source(receiver, observing_sink(records.clone(), output));
        Self {
            serving: None,
            records,
            source_registered: true,
            request_cancellation: None,
        }
    }
    pub(crate) fn start(
        &mut self,
        admission: &mut Admission,
        directory: &std::fs::File,
    ) -> io::Result<()> {
        need(self.serving.is_none(), "initializer server already started")?;
        let directory_fd = directory.try_clone()?;
        let issues = admission
            .issues
            .clone()
            .ok_or_else(|| rpc("actual RPC monitor absent"))?;
        let server = admission
            .server
            .take()
            .ok_or_else(|| rpc("actual RPC server absent"))?;
        let connections = server.connection_monitor();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let cancellation = reverie_rpc_transport::RequestCancellation::default();
        {
            self.request_cancellation = Some(cancellation.clone());
        }
        let task = tokio::spawn(DirectoryBoundServer {
            server: Box::pin(async move {
                let shutdown = async move {
                    let _ = stopped.await;
                };
                server
                    .serve_until_with_request_cancellation(shutdown, cancellation)
                    .await
            }),
            _directory_fd: directory_fd,
        });
        self.serving = Some(Serving {
            task: Some(task),
            shutdown: Some(shutdown),
            connections,
            issues,
            owner: admission.owner.clone(),
        });
        Ok(())
    }
    /// Safety: the enclosing root fulfils the bounded valid-path entry contract
    /// for this same retained image and its exact coordinates. Package DATA
    /// supplies neither source qualification nor permission for Guest release.
    #[cfg(test)]
    pub(crate) async unsafe fn observe(
        &self,
        mapped: &mut PreparedMapping<'_>,
        config: &detcore::Config,
        coordinates: BootstrapEntryCoordinates,
    ) -> io::Result<()> {
        let serving = self
            .serving
            .as_ref()
            .ok_or_else(|| rpc("initializer RPC not serving"))?;
        need(
            serving.owner.watched_process().watch_is_healthy(),
            "initial watch failed",
        )?;
        let subscriptions = <detcore::Detcore as Tool>::subscriptions(config);
        let mut reserved_signals = 1_u64 << (libc::SIGSYS - 1);
        if subscriptions.has_cpuid() || subscriptions.has_rdtsc() {
            reserved_signals |= 1_u64 << (libc::SIGSEGV - 1);
        }
        let result = tokio::select! {
            // SAFETY: owner and all leases stay in the outer synchronous root.
            observed = unsafe { mapped.observe_until_terminal(
                &coordinates, ObservationDomain::ToolInitialization { reserved_signals },
                Instant::now() + WAIT,
            ) } => observed.map_err(rpc),
            _ = serving.issues.failed() => Err(rpc(format!("genuine initializer RPC failed: {:?}", serving.issues.snapshot()))),
        }?;
        need(
            serving.issues.snapshot().is_empty(),
            "genuine initializer RPC acquired issue",
        )?;
        need(
            serving.owner.watched_process().watch_is_healthy(),
            "initializer watch failed",
        )?;
        need(
            serving.owner.initial_used.load(Ordering::Acquire)
                && !serving.owner.failed.load(Ordering::Acquire),
            "genuine first-image admission not consumed or failed",
        )?;
        need(
            serving
                .task
                .as_ref()
                .is_some_and(|task| !task.is_finished()),
            "genuine RPC server exited before initializer return",
        )?;
        eprintln!(
            "GENUINE_INITIALIZER_RETURN: bootstrap_sigsys={} tool_init_sigsys={} actual_post_rip={:#x}; Guest=0; runtime_ready observed by retained owner; cancellation follows",
            result.forwarded_sigsys, result.forwarded_tool_init_sigsys, result.registers[16]
        );
        Ok(())
    }
    /// Safety: the enclosing root fulfils the exact V11 bounded-entry contract;
    /// source qualification for production/Guest release is explicitly absent.
    pub(crate) async unsafe fn observe_at(
        &self,
        mapped: &mut PreparedMapping<'_>,
        config: &detcore::Config,
        coordinates: BootstrapEntryCoordinates,
    ) -> io::Result<()> {
        let serving = self
            .serving
            .as_ref()
            .ok_or_else(|| rpc("initializer RPC not serving"))?;
        need(
            serving.owner.watched_process().watch_is_healthy(),
            "initial watch failed",
        )?;
        let subscriptions = <detcore::Detcore as Tool>::subscriptions(config);
        let mut reserved_signals = 1_u64 << (libc::SIGSYS - 1);
        if subscriptions.has_cpuid() || subscriptions.has_rdtsc() {
            reserved_signals |= 1_u64 << (libc::SIGSEGV - 1);
        }
        let _result = tokio::select! {
            // SAFETY: owner and all leases stay in the outer synchronous root.
            observed = unsafe { mapped.observe_until_terminal(
                &coordinates, ObservationDomain::ToolInitialization { reserved_signals },
                Instant::now() + WAIT,
            ) } => observed.map_err(rpc),
            _ = serving.issues.failed() => Err(rpc(format!("genuine initializer RPC failed: {:?}", serving.issues.snapshot()))),
        }?;
        need(
            serving.issues.snapshot().is_empty(),
            "genuine initializer RPC acquired issue",
        )?;
        need(
            serving.owner.watched_process().watch_is_healthy(),
            "initializer watch failed",
        )?;
        need(
            serving.owner.initial_used.load(Ordering::Acquire)
                && !serving.owner.failed.load(Ordering::Acquire),
            "genuine first-image admission not consumed or failed",
        )?;
        need(
            serving
                .task
                .as_ref()
                .is_some_and(|task| !task.is_finished()),
            "genuine RPC server exited before initializer return",
        )?;
        Ok(())
    }
    fn reaped_serving(
        &self,
        receipt: &reverie_inguest::launcher::exec_stop::CleanupReceipt,
        admission: &Arc<super::MappedFirstAdmission>,
    ) -> io::Result<&Serving> {
        need(
            receipt.snapshot().confirmed,
            "MM RPC cancellation before confirmed reap",
        )?;
        let serving = self
            .serving
            .as_ref()
            .ok_or_else(|| rpc("MM cancel server absent"))?;
        need(
            Arc::ptr_eq(&serving.owner, admission),
            "MM cancel admission changed",
        )?;
        let pid = admission.watched_process().process_id();
        need(
            admission.exit_admission().reported_exits() == [pid],
            "MM cancel lacks actual same-child watch completion",
        )?;
        Ok(serving)
    }
    /// Publish synchronously, before entering/polling the runtime again. The
    /// watcher may otherwise wake a GlobalState continuation toward a dead peer.
    pub(crate) fn cancel_reaped_requests(
        &self,
        receipt: &reverie_inguest::launcher::exec_stop::CleanupReceipt,
        admission: &Arc<super::MappedFirstAdmission>,
    ) -> io::Result<bool> {
        need(
            receipt.snapshot().confirmed,
            "MM RPC cancellation before confirmed reap",
        )?;
        let Some(serving) = self.serving.as_ref() else {
            return Ok(false);
        };
        need(
            Arc::ptr_eq(&serving.owner, admission),
            "MM cancel admission changed",
        )?;
        let pid = admission.watched_process().process_id();
        let cancellation = self
            .request_cancellation
            .as_ref()
            .ok_or_else(|| rpc("MM owned request cancellation unavailable"))?;
        // Synchronous intent BEFORE the first watcher await. The SAME lexical
        // receipt + admission establish physical death; draining the watcher is
        // closure evidence, not permission to resume GlobalState toward a reply.
        // Existing issues do not prevent cleanup and are never cleared.
        cancellation.request(pid).map_err(rpc)?;
        Ok(true)
    }
    /// Join only after synchronous intent; preserve watcher and task errors.
    /// An empty/idle drain is not represented as a pending-request cancellation.
    pub(crate) async fn join_reaped_requests(
        &self,
        receipt: &reverie_inguest::launcher::exec_stop::CleanupReceipt,
        admission: &Arc<super::MappedFirstAdmission>,
    ) -> io::Result<reverie_rpc_transport::CancellationDrain> {
        need(
            receipt.snapshot().confirmed,
            "MM RPC cancellation before confirmed reap",
        )?;
        let serving = self
            .serving
            .as_ref()
            .ok_or_else(|| rpc("MM cancel server absent"))?;
        need(
            Arc::ptr_eq(&serving.owner, admission),
            "MM cancel admission changed",
        )?;
        let pid = admission.watched_process().process_id();
        let cancellation = self
            .request_cancellation
            .as_ref()
            .ok_or_else(|| rpc("MM owned request cancellation unavailable"))?;
        let watched = serving
            .owner
            .exit_admission()
            .drain(WAIT)
            .await
            .map_err(|error| rpc(format!("physical watch drain failed: {error:?}")));
        // Always join already-requested cancellation even if watcher close fails.
        let association = self.reaped_serving(receipt, admission).map(|_| ());
        let joined = tokio::time::timeout(WAIT, cancellation.wait_for_drain())
            .await
            .map_err(rpc)
            .and_then(|result| result.map_err(rpc));
        // Retain all watch/identity/issue/join failures, including late failures.
        let mut errors = Vec::new();
        match &joined {
            Err(error) => errors.push(format!("MM actual task drain/join: {error}")),
            Ok(drained) if drained.process_id() != pid => {
                errors.push("MM cancel drained process differs".to_owned())
            }
            Ok(_) => {}
        }
        if let Err(error) = watched {
            errors.push(format!("{error}"));
        }
        if let Err(error) = association {
            errors.push(format!("{error}"));
        }
        if serving.connections.active_connections() != 0 {
            errors.push("MM cancellation drain retained active connection".to_owned());
        }
        if !serving.issues.snapshot().is_empty() {
            errors.push(format!(
                "MM cancellation retained RPC failures: {:?}",
                serving.issues.snapshot()
            ));
        }
        if !errors.is_empty() {
            return Err(rpc(errors.join("; ")));
        }
        joined
    }
    #[cfg(all(test, feature = "mm-cancellation-test-support"))]
    pub(crate) async fn cancel_reaped_mm_request(
        &self,
        receipt: &reverie_inguest::launcher::exec_stop::CleanupReceipt,
        admission: &Arc<super::MappedFirstAdmission>,
    ) -> io::Result<Vec<reverie_rpc_transport::CancelledRequest>> {
        let serving = self.reaped_serving(receipt, admission)?;
        let pid = admission.watched_process().process_id();
        let cancellation = self
            .request_cancellation
            .as_ref()
            .ok_or_else(|| rpc("MM owned request cancellation unavailable"))?;
        // Production cleanup already requested and joined; a physical cut may
        // not substitute an idle or empty drain for its actual pending request.
        let drained = tokio::time::timeout(WAIT, cancellation.wait_for_drain())
            .await
            .map_err(rpc)?
            .map_err(rpc)?;
        let outcomes = drained.cancelled_requests();
        need(
            drained.process_id() == pid
                && drained.idle_connections().is_empty()
                && outcomes.len() == 1
                && outcomes[0].process_id() == pid
                && outcomes[0].sender() == pid
                && outcomes[0].connection() == 1
                && outcomes[0].request() > 0,
            "MM cancel actual joined request differs",
        )?;
        need(
            serving.issues.snapshot().is_empty(),
            "MM cancellation retained RPC failure",
        )?;
        Ok(outcomes.to_vec())
    }
    #[cfg(all(test, feature = "mm-cancellation-test-support"))]
    pub(crate) async fn mm_cancel_connections_idle(&self) -> io::Result<()> {
        let serving = self
            .serving
            .as_ref()
            .ok_or_else(|| rpc("MM cancel server absent"))?;
        tokio::time::timeout(WAIT, serving.connections.wait_for_idle())
            .await
            .map_err(rpc)?;
        need(
            serving.task.is_none(),
            "MM cancel RPC listener was not joined",
        )
    }
    pub(crate) async fn close(&mut self) -> io::Result<Option<Arc<super::MappedFirstAdmission>>> {
        // Original ptracer has already reaped the actual child. Require actual
        // watch and connections to drain before stopping the existing server.
        let closed = match self.serving.as_mut() {
            Some(serving) => serving.close().await.map(|()| Some(serving.owner.clone())),
            None => Ok(None),
        };
        detcore::detlog::clear_forwarded_source();
        self.source_registered = false;
        let records = self.records.lock().map_err(rpc)?;
        need(
            !records.overflow,
            "initializer DETLOG capture exceeded bound",
        )?;
        need(
            !records.counter_overflow,
            "initializer DETLOG observation count overflow",
        )?;
        if let Some(bytes) = records.retained.as_ref() {
            eprintln!(
                "INITIALIZER_DETLOG_CAPTURE: bytes={}; cancelled before Guest; no final strict-cut claim",
                bytes.len()
            );
        }
        closed
    }
}
impl Drop for Trial {
    fn drop(&mut self) {
        // Emergency unregistration is never success evidence. The lexical
        // physical root has retained cancellation through unwind before here.
        if self.source_registered {
            detcore::detlog::clear_forwarded_source();
        }
    }
}

pub(crate) mod hello_consumer;

#[cfg(test)]
mod observation_policy_tests {
    use super::*;

    #[test]
    fn both_policies_require_forwarded_records_without_a_production_copy() {
        for policy in [
            ObservationPolicy::Production,
            ObservationPolicy::Qualification,
        ] {
            let mut records = CapturedRecords::new(policy);
            let report = records.snapshot();
            assert_eq!((report.messages, report.bytes), (0, 0));
            assert_eq!(report.retained.is_some(), policy.is_qualification());
            assert!(
                report.required_records_absent(),
                "successful completion requires actual forwarded evidence"
            );
            assert!(!records.overflow && !records.counter_overflow);

            let record = b"INFO detcore: DETLOG genuine record\n";
            records.observe(record);
            let report = records.snapshot();
            assert_eq!((report.messages, report.bytes), (1, record.len() as u64));
            assert!(!report.required_records_absent());
            match policy {
                ObservationPolicy::Production => assert!(report.retained.is_none()),
                ObservationPolicy::Qualification => {
                    assert_eq!(report.retained.as_deref(), Some(record.as_slice()));
                }
            }
        }
        let mut finite = CapturedRecords::new(ObservationPolicy::Qualification);
        finite.observe(b"");
        assert!(
            finite.snapshot().required_records_absent(),
            "a callback without bytes cannot satisfy finite nonempty evidence"
        );
    }

    #[test]
    fn full_sink_receives_beyond_finite_bound_without_a_production_copy() {
        for policy in [
            ObservationPolicy::Production,
            ObservationPolicy::Qualification,
        ] {
            let captured = Arc::new(Mutex::new(CapturedRecords::new(policy)));
            let delivered = Arc::new(Mutex::new(Vec::new()));
            let output = delivered.clone();
            let mut sink = observing_sink(
                captured.clone(),
                Box::new(move |bytes| {
                    output.lock().unwrap().extend_from_slice(bytes);
                }),
            );
            let mut chunk = vec![0; 64 * 1024];
            for ordinal in 0..129 {
                chunk.fill(ordinal);
                sink(&chunk);
            }
            let records = captured.lock().unwrap();
            let report = records.snapshot();
            assert_eq!(report.messages, 129);
            assert_eq!(report.bytes, 129 * 64 * 1024);
            let delivered = delivered.lock().unwrap();
            assert_eq!(delivered.len(), 129 * 64 * 1024);
            for (ordinal, actual) in delivered.chunks_exact(chunk.len()).enumerate() {
                assert!(actual.iter().all(|byte| *byte == ordinal as u8));
            }
            assert!(!records.counter_overflow);
            match policy {
                ObservationPolicy::Production => {
                    assert!(report.retained.is_none());
                    assert!(!records.overflow);
                }
                ObservationPolicy::Qualification => {
                    assert_eq!(report.retained.as_ref().unwrap().len(), RECORD_LIMIT);
                    assert!(
                        records.overflow,
                        "same finite overflow stays fatal at close"
                    );
                }
            }
        }
    }

    #[test]
    fn production_counter_overflow_is_sticky_instead_of_wrapping() {
        for policy in [
            ObservationPolicy::Production,
            ObservationPolicy::Qualification,
        ] {
            let mut records = CapturedRecords::new(policy);
            records.messages = u64::MAX;
            records.observe(b"one");
            assert!(records.counter_overflow);
            records.observe(b"two");
            assert!(records.counter_overflow);
            assert_eq!(records.messages, u64::MAX);
            let mut bytes = CapturedRecords::new(policy);
            bytes.observed_bytes = u64::MAX;
            bytes.observe(b"one");
            bytes.observe(b"two");
            assert!(bytes.counter_overflow);
            assert_eq!(bytes.observed_bytes, u64::MAX);
            assert_eq!(bytes.messages, 0, "partial counter updates are refused");
        }
    }
}
