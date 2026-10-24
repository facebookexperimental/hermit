//! Opt-in hello consumer of the original initialized mapping and actual stats.
//! The lexical physical root and all leases stay outside these borrowed futures.
use std::path::Path;

#[cfg(not(feature = "loader-rcb-observation"))]
use reverie::process::ExitStatus;
use reverie_inguest::launcher::InitialImageStatsCollector;
use reverie_inguest::launcher::InitialImageStatsServer;
use reverie_rpc_transport::ConnectionMonitor;
use reverie_rpc_transport::RpcError;
use reverie_rpc_transport::RpcIssueMonitor;

use super::*;

fn need(condition: bool, message: &str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message.to_owned()))
    }
}

pub(crate) struct HelloConsumer {
    stats_task: Option<tokio::task::JoinHandle<Result<(), RpcError>>>,
    stats_connections: ConnectionMonitor,
    stats_issues: RpcIssueMonitor,
    collector: Option<InitialImageStatsCollector>,
    // The actual same admission, not a new readiness/cleanup owner.
    admission: Arc<super::super::MappedFirstAdmission>,
    handover_attempted: bool,
}
pub(crate) struct HelloReports {
    pub(crate) source: reverie_liteinst::LiteinstBackendStatsSource,
    pub(crate) detlog: Vec<u8>,
    pub(crate) drained: Arc<super::super::MappedFirstAdmission>,
}
impl HelloConsumer {
    /// Call synchronously under the original current-thread runtime, before
    /// any initializer instruction. `trial.start` must use this same admission.
    pub(crate) fn bind(
        trial: &Trial,
        rpc_path: &Path,
        owned_directory: &Path,
        directory_fd: &std::fs::File,
    ) -> io::Result<Self> {
        let serving = trial
            .serving
            .as_ref()
            .ok_or_else(|| rpc("hello RPC not serving"))?;
        let admission = serving.owner.clone();
        let directory_fd = directory_fd.try_clone()?;
        let (server, collector) =
            InitialImageStatsServer::bind(rpc_path, owned_directory, admission.clone())
                .map_err(rpc)?;
        let stats_connections = server.connection_monitor();
        let stats_issues = server.issue_monitor();
        let task = tokio::spawn(DirectoryBoundServer {
            server: Box::pin(server.serve()),
            _directory_fd: directory_fd,
        });
        Ok(Self {
            stats_task: Some(task),
            stats_connections,
            stats_issues,
            collector: Some(collector),
            admission,
            handover_attempted: false,
        })
    }

    /// # Safety
    /// The caller fulfils the reviewed R handover contract, including actual
    /// fresh phase4 runtime routing, genuine shared initialization and fixed
    /// gate/terminal roles. It retains physical/source/FD/backing and all service
    /// owners in a noncancellable synchronous root until confirmed retirement.
    /// No repeated, fabricated or bootstrap-only terminal may reach handover.
    #[cfg(not(feature = "loader-rcb-observation"))]
    pub(crate) async unsafe fn handover_status(
        &mut self,
        trial: &Trial,
        mapping: &mut PreparedMapping<'_>,
    ) -> io::Result<ExitStatus> {
        need(!self.handover_attempted, "hello handover already attempted")?;
        self.handover_attempted = true;
        let serving = trial
            .serving
            .as_ref()
            .ok_or_else(|| rpc("hello RPC absent"))?;
        need(
            Arc::ptr_eq(&serving.owner, &self.admission),
            "hello admission changed",
        )?;
        need(
            self.admission.initial_used.load(Ordering::Acquire)
                && !self.admission.failed.load(Ordering::Acquire)
                && self.admission.watched_process().watch_is_healthy(),
            "hello genuine admission unavailable",
        )?;
        need(
            self.stats_issues.snapshot().is_empty()
                && serving.issues.snapshot().is_empty()
                && self
                    .stats_task
                    .as_ref()
                    .is_some_and(|task| !task.is_finished())
                && serving
                    .task
                    .as_ref()
                    .is_some_and(|task| !task.is_finished()),
            "hello service failed before release",
        )?;
        let deadline = Instant::now() + WAIT;
        let status = tokio::select! {
            // No physical owner is moved; dropping this future retains mapping.
            status = unsafe { mapping.handover_and_wait(deadline) } => status.map_err(rpc),
            _ = serving.issues.failed() => Err(rpc(format!("hello RPC issue: {:?}", serving.issues.snapshot()))),
            _ = self.stats_issues.failed() => Err(rpc(format!("hello stats issue: {:?}", self.stats_issues.snapshot()))),
        }?;
        Ok(status)
    }

    // Explicit observation-only alternative, selected only in the diagnostic
    // feature build. The caller binds actual continuation symbols from the
    // SAME retained runtime and persists the full data outside the INFO stream.
    /// # Safety
    /// The caller fulfils the reviewed R handover contract, including actual
    /// fresh phase4 runtime routing, genuine shared initialization and fixed
    /// gate/terminal roles. It retains physical/source/FD/backing and all service
    /// owners in a noncancellable synchronous root until confirmed retirement.
    /// No repeated, fabricated or bootstrap-only terminal may reach handover.
    /// Additionally the exact unsafe handover_and_observe_rcb contract applies:
    /// actual same-image continuation coordinates, one thread, no asynchronous
    /// guest handlers/clone/exec, and identical externally bound PMU/CPU domain.
    /// This is a distinct retained-ptrace measurement, not production timing.
    #[cfg(feature = "loader-rcb-observation")]
    pub(crate) async unsafe fn handover_rcb_status(
        &mut self,
        trial: &Trial,
        mapping: &mut PreparedMapping<'_>,
        coordinates: &reverie_inguest::launcher::exec_stop::physical::RcbObservationCoordinates,
    ) -> io::Result<reverie_inguest::launcher::exec_stop::physical::RcbObservation> {
        need(!self.handover_attempted, "hello handover already attempted")?;
        self.handover_attempted = true;
        let serving = trial
            .serving
            .as_ref()
            .ok_or_else(|| rpc("hello RPC absent"))?;
        need(
            Arc::ptr_eq(&serving.owner, &self.admission),
            "hello admission changed",
        )?;
        need(
            self.admission.initial_used.load(Ordering::Acquire)
                && !self.admission.failed.load(Ordering::Acquire)
                && self.admission.watched_process().watch_is_healthy(),
            "hello genuine admission unavailable",
        )?;
        need(
            self.stats_issues.snapshot().is_empty()
                && serving.issues.snapshot().is_empty()
                && self
                    .stats_task
                    .as_ref()
                    .is_some_and(|task| !task.is_finished())
                && serving
                    .task
                    .as_ref()
                    .is_some_and(|task| !task.is_finished()),
            "hello service failed before release",
        )?;
        let deadline = Instant::now() + WAIT;
        let status = tokio::select! {
            // No physical owner is moved; dropping this future retains mapping.
            status = unsafe { mapping.handover_and_observe_rcb(coordinates, deadline) } => status.map_err(rpc),
            _ = serving.issues.failed() => Err(rpc(format!("hello RPC issue: {:?}", serving.issues.snapshot()))),
            _ = self.stats_issues.failed() => Err(rpc(format!("hello stats issue: {:?}", self.stats_issues.snapshot()))),
        }?;
        Ok(status)
    }

    /// After actual confirmed reap (normal exit OR cancellation), drain actual
    /// watch/RPC/stats and DETLOG before the lexical root drops their leases.
    /// Call even after observation/handover errors; preserve the earlier error.
    pub(crate) async fn close(
        &mut self,
        trial: &mut Trial,
        require_success_report: bool,
    ) -> io::Result<HelloReports> {
        let mut errors = Vec::new();
        // Keep both drain failures. Neither can bypass server abort-and-join.
        let drained = match trial.serving.as_mut() {
            Some(serving) if Arc::ptr_eq(&serving.owner, &self.admission) => {
                match serving.close().await {
                    Ok(()) => Some(serving.owner.clone()),
                    Err(error) => {
                        errors.push(format!("hello RPC/watch close: {error}"));
                        None
                    }
                }
            }
            _ => {
                errors.push("hello retained serving owner absent/changed".to_owned());
                None
            }
        };
        // Serving::close may return early on drain error. Still join its owned
        // listener after physical reap; keep the drain error fatal.
        if let Some(serving) = trial.serving.as_mut() {
            if let Some(shutdown) = serving.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(task) = serving.task.take() {
                task.abort();
                match task.await {
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => {
                        errors.push(format!("hello RPC join after close failure: {error}"))
                    }
                    Ok(Err(error)) => {
                        errors.push(format!("hello RPC server after close failure: {error}"))
                    }
                    Ok(Ok(())) => {}
                }
            }
        }
        if tokio::time::timeout(WAIT, self.stats_connections.wait_for_idle())
            .await
            .is_err()
        {
            errors.push("hello stats connections did not drain".to_owned());
        }
        if let Some(task) = self.stats_task.take() {
            task.abort();
            match task.await {
                Err(error) if error.is_cancelled() => {}
                Err(error) => errors.push(format!("hello stats task: {error}")),
                Ok(Err(error)) => errors.push(format!("hello stats server: {error}")),
                Ok(Ok(())) => errors.push("hello stats server ended unexpectedly".to_owned()),
            }
        } else {
            errors.push("hello stats server already closed".to_owned());
        }
        if !self.stats_issues.snapshot().is_empty() {
            errors.push(format!(
                "hello retained stats issues: {:?}",
                self.stats_issues.snapshot()
            ));
        }
        if let Some(serving) = trial.serving.as_ref()
            && !serving.issues.snapshot().is_empty()
        {
            errors.push(format!(
                "hello retained RPC issues: {:?}",
                serving.issues.snapshot()
            ));
        }
        let source = match self.collector.take() {
            Some(collector) => match collector.into_source().await {
                Ok(source) => Some(source),
                Err(error) => {
                    errors.push(format!("hello stats collector: {error}"));
                    None
                }
            },
            None => {
                errors.push("hello collector already consumed".to_owned());
                None
            }
        };
        detcore::detlog::clear_forwarded_source();
        trial.source_registered = false;
        let records = trial.records.lock().map_err(rpc)?;
        if records.overflow {
            errors.push("hello DETLOG exceeded existing bound".to_owned());
        }
        let detlog = records.bytes.clone();
        if require_success_report {
            if source
                .as_ref()
                .is_none_or(|source| source.snapshot().process_reports() != 1)
            {
                errors.push("single-child hello lacks exactly one real stats report".to_owned());
            }
            if source.as_ref().is_none_or(|source| {
                source
                    .snapshot()
                    .dispatch_paths()
                    .count(&reverie::backend_stats::LiteinstDispatchPath::InGuestPhysicalSigsys)
                    == 0
            }) {
                errors
                    .push("hello final report has no actual physical dispatcher event".to_owned());
            }
            if detlog.is_empty() {
                errors.push("hello produced no genuine forwarded DETLOG".to_owned());
            }
        }
        need(errors.is_empty(), &errors.join("; "))?;
        Ok(HelloReports {
            source: source.ok_or_else(|| rpc("hello actual stats source absent"))?,
            detlog,
            drained: drained.ok_or_else(|| rpc("hello actual watch drain absent"))?,
        })
    }
}
impl Drop for HelloConsumer {
    fn drop(&mut self) {
        // Failure-only emergency service cleanup, never proof of physical reap.
        // The noncancellable root must settle child ownership BEFORE this drop.
        if let Some(task) = self.stats_task.take() {
            self.admission
                .fail(&"hello stats dropped before joined close");
            task.abort();
        }
    }
}
