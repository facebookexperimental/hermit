//! Captured execution borrows the original mapped owner through complete cleanup.
use std::future::Future;
use std::io;

use reverie::ExitStatus;
use reverie::process::ParentStdio;
use reverie_inguest::launcher::exec_stop::CleanupState;
use tokio::time::Instant;

use super::Capture;
use super::Limits;

/// Owned by the caller, outside run_inner/catch_unwind/block_on.
/// Limits and deadline are fixed before setup; the sole actual ParentStdio is
/// transferred only after retain_spawn. Errors/partial bytes survive return.
pub(in super::super) struct CaptureRequest {
    limits: Limits,
    pub(in super::super) deadline: Option<Instant>,
    pub(in super::super) capture: Option<Capture>,
    pub(in super::super) secondary: Vec<io::Error>,
    pub(in super::super) cleanup: Option<CleanupState>,
}
impl CaptureRequest {
    pub fn new(limits: Limits, deadline: Option<Instant>) -> Self {
        Self {
            limits,
            deadline,
            capture: None,
            secondary: Vec::new(),
            cleanup: None,
        }
    }
    pub fn secondary(&self) -> &[io::Error] {
        &self.secondary
    }
    pub fn check(&self) -> io::Result<()> {
        check(self.deadline)
    }
    // Nonfallible transfer BEFORE initial()?; it must not mask launch refusal.
    pub fn retain(&mut self, parent: Option<ParentStdio>) {
        self.capture = parent.map(|parent| Capture::new(parent, self.limits, self.deadline));
    }
    pub fn register(&mut self) -> io::Result<()> {
        self.capture
            .as_mut()
            .ok_or_else(|| io::Error::other("actual capture endpoints absent"))?
            .register()
    }
    /// The original complete observer/provider/execution expression is borrowed
    /// intact. EOF-first awaits that SAME expression under the SAME deadline.
    pub async fn drive<F>(&mut self, original: F) -> io::Result<ExitStatus>
    where
        F: Future<Output = io::Result<ExitStatus>>,
    {
        let capture = self
            .capture
            .as_mut()
            .ok_or_else(|| io::Error::other("actual capture absent"))?;
        let result = within(self.deadline, async {
            tokio::pin!(original);
            tokio::select! {
                result = &mut original => result,
                result = capture.pump() => { result?; original.await }
            }
        })
        .await;
        if result.is_err() {
            // The borrowed execution/pump has been dropped. Preserve its actual
            // typed error unchanged; cancellation is separate capture evidence.
            capture.cancel();
        }
        result
    }
}

pub(in super::super) fn check(deadline: Option<Instant>) -> io::Result<()> {
    if deadline.is_some_and(|end| Instant::now() >= end) {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "captured qualification deadline",
        ))
    } else {
        Ok(())
    }
}

/// A ready future can win Tokio's timeout poll. Check before and after success;
/// an actual original Err remains the primary even if time also elapsed.
pub(in super::super) async fn within<F, T>(deadline: Option<Instant>, future: F) -> io::Result<T>
where
    F: Future<Output = io::Result<T>>,
{
    check(deadline)?;
    let result = match deadline {
        Some(end) => tokio::time::timeout_at(end, future).await.map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "captured qualification deadline")
        })?,
        None => future.await,
    };
    let value = result?;
    check(deadline)?;
    Ok(value)
}

/// Capture policy is separate from the original execution/cleanup owner.
#[derive(Clone, Copy)]
pub(crate) enum CapturePolicy {
    /// Match the shared verifier's complete-output, no-implicit-timer behavior.
    Full,
    #[cfg(test)]
    Qualification { deadline: Instant },
}
impl CapturePolicy {
    pub(super) fn observation(
        self,
    ) -> crate::mapped_first_admission::runtime_support::init_trial::ObservationPolicy {
        use crate::mapped_first_admission::runtime_support::init_trial::ObservationPolicy;
        match self {
            Self::Full => ObservationPolicy::Production,
            #[cfg(test)]
            Self::Qualification { .. } => ObservationPolicy::Qualification,
        }
    }
    pub(super) fn parts(self) -> (Limits, Option<Instant>) {
        match self {
            Self::Full => (Limits::unbounded(), None),
            #[cfg(test)]
            Self::Qualification { deadline } => (
                Limits {
                    stdout: 128 * 1024,
                    stderr: 128 * 1024,
                    total: 256 * 1024,
                },
                Some(deadline),
            ),
        }
    }
}

/// One shared-verifier execution. These are the original caller's inputs,
/// log sink and descriptor-free forwarding plan, not a new launch owner.
pub(crate) struct VerificationCapture<'a> {
    pub command: reverie::process::Command,
    pub config: detcore::Config,
    pub retained: &'a crate::mapped_first_admission::RetainedImage,
    pub records: detcore::detlog::ForwardedRecordSink,
    pub print_summary: bool,
    pub summary_json: &'a Option<std::path::PathBuf>,
    pub policy: CapturePolicy,
    pub forwarded: &'a mut crate::run::ForwardedDetlogPlan,
}

pub(crate) fn run_verification(
    mut run: VerificationCapture<'_>,
) -> anyhow::Result<reverie::process::Output> {
    use reverie::process::Stdio;
    use reverie_inguest::launcher::exec_stop::CleanupConfirmation;
    use reverie_inguest::launcher::exec_stop::CleanupOwner;

    // Refuse before even rewinding the shared verification stdin snapshot.
    // The inner owner repeats admission for direct, uncaptured callers.
    super::super::check_mapped_command(&run.command)?;
    let (limits, deadline) = run.policy.parts();
    check(deadline)?;
    run.command
        .stdin(hermit::output_backend_stdin()?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Caller-owned storage outlives every catch/select and the full original
    // late cleanup. Public verification captures complete output; finite smoke
    // controls explicitly retain their original bounds.
    let mut request = CaptureRequest::new(limits, deadline);
    // SAFETY: the original container bridge retains the qualified image,
    // namespace and original physical owner.
    let result = unsafe {
        super::super::run_captured(
            run.command,
            run.config,
            run.retained,
            super::super::RunOutput {
                records: run.records,
                observation: run.policy.observation(),
                print_summary: run.print_summary,
                summary_json: run.summary_json,
            },
            &mut request,
            Some(run.forwarded),
        )
    };
    // The complete original Result is primary, including late source/services
    // and GlobalState finalization errors. No Output escapes an earlier error.
    let status = result?;
    request.check()?;
    let cleanup = request
        .cleanup
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("actual verification capture cleanup absent"))?;
    anyhow::ensure!(
        cleanup.child_created
            && cleanup.owner == CleanupOwner::Ptracer
            && cleanup.confirmed
            && cleanup.confirmation == Some(CleanupConfirmation::NormalWait)
            && !cleanup.kill_requested
            && cleanup.kill_errno.is_none()
            && cleanup.wait_error.is_none()
            && cleanup.terminal_status == Some(status),
        "verification capture original terminal wait differs: {cleanup:?}"
    );
    anyhow::ensure!(
        request.secondary().is_empty(),
        "verification capture secondary failures"
    );
    let capture = request
        .capture
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("actual verification capture endpoints absent"))?;
    anyhow::ensure!(
        capture.retained_stdin().is_none(),
        "verification capture retained an unsupported stdin feeder"
    );
    let output = capture.finish(Ok(status))?;
    check(deadline)?;
    Ok(output)
}
