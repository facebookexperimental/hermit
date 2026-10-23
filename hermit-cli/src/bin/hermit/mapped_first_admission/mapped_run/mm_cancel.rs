//! Physical cancellation controls over the actual production lexical root.
//! New-mechanism reruns are UNRUN; the prior four failures remain evidence.
//! No public flag, environment delay or fake RPC Tool.
use detcore::initial_mm_probe::Control;
use detcore::initial_mm_probe::Pause;
use detcore::initial_mm_probe::Point;
use detcore::initial_mm_probe::RegistrationResult;
use detcore::initial_mm_probe::Snapshot;

use super::*;

pub(super) struct Cancellation {
    point: Point,
    control: Option<Control>,
    pause: Option<Pause>,
    at: Option<Snapshot>,
    retired: Option<Snapshot>,
    final_state: Option<Snapshot>,
    confirmed: bool,
    watch_drained: bool,
    services_joined: bool,
    errors: Vec<String>,
    root_errors: Vec<String>,
    intended_primary: bool,
    cancelled_requests: Vec<reverie_rpc_transport::CancelledRequest>,
}
impl Cancellation {
    fn new(point: Point) -> Self {
        Self {
            point,
            control: None,
            pause: None,
            at: None,
            retired: None,
            final_state: None,
            confirmed: false,
            watch_drained: false,
            services_joined: false,
            errors: Vec::new(),
            root_errors: Vec::new(),
            intended_primary: false,
            cancelled_requests: Vec::new(),
        }
    }
    pub(super) fn arm(
        &mut self,
        global: &detcore::GlobalState,
        tid: i32,
    ) -> Result<(), &'static str> {
        self.control = Some(global.install_initial_mm_probe(self.point, tid)?);
        Ok(())
    }
    pub(super) async fn reached(&mut self) -> Result<Pause, &'static str> {
        self.control
            .as_mut()
            .ok_or("MM control absent")?
            .reached()
            .await
    }
    pub(super) fn retain_pause(&mut self, pause: Pause) {
        self.at = Some(pause.snapshot.clone());
        self.pause = Some(pause);
    }
    pub(super) fn after_reap(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        receipt: &CleanupReceipt,
        admissions: &[Admission],
        global: &detcore::GlobalState,
        trial: &Trial,
    ) {
        self.confirmed = receipt.snapshot().confirmed;
        if !self.confirmed {
            self.errors
                .push("MM cancellation lacks confirmed physical reap".into());
            // cleanup_all must never return here; do not pretend a release
            // would repair lost ownership. Existing retained-frame policy applies.
            return;
        }
        if let Some(slot) = admissions.first() {
            match runtime.block_on(slot.owner.exit_admission().drain(WAIT)) {
                Ok(())
                    if slot.owner.exit_admission().reported_exits()
                        == [slot.owner.watched_process().process_id()] =>
                {
                    self.watch_drained = true
                }
                other => self
                    .errors
                    .push(format!("MM cancellation watch: {other:?}")),
            }
        } else {
            self.errors.push("MM cancellation admission absent".into());
        }
        match global.initial_mm_probe_snapshot() {
            Ok(snapshot) => self.retired = Some(snapshot),
            Err(error) => self.errors.push(error.into()),
        }
        // Keep Pause alive until the SAME server has dropped the actual
        // pending receive_rpc and joined its connection. Do not resume a normal
        // reply to the dead client, including on cancellation/timeout failure.
        if self.watch_drained
            && let Some(slot) = admissions.first()
        {
            // Production-route refusal control: an unconfirmed/default
            // receipt supplies NO child-death authority, even with the real
            // serving/admission handles. It must not publish cancellation.
            let absent = CleanupReceipt::default();
            match trial.cancel_reaped_requests(&absent, &slot.owner) {
                Err(error) if error.to_string() == "MM RPC cancellation before confirmed reap" => {}
                other => self.errors.push(format!(
                    "MM cancellation accepted absent reap or refused elsewhere: {other:?}"
                )),
            }
            match runtime.block_on(trial.cancel_reaped_mm_request(receipt, &slot.owner)) {
                Ok(outcomes) => {
                    self.cancelled_requests = outcomes;
                    // Receiver has already been dropped and task joined;
                    // release cannot run the cancelled callback or issue a reply.
                    drop(self.pause.take());
                }
                Err(error) => self
                    .errors
                    .push(format!("MM owned RPC cancellation: {error}")),
            }
        }
    }
    pub(super) fn capture_state(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        trial: &Trial,
        admissions: &[Admission],
        global: &detcore::GlobalState,
    ) {
        match runtime.block_on(trial.mm_cancel_connections_idle()) {
            Ok(()) => self.services_joined = true,
            Err(error) => self
                .errors
                .push(format!("MM actual RPC drain/join: {error}")),
        }
        if admissions.iter().any(|slot| !slot.drained) {
            self.errors
                .push("MM actual admission drain unfinished".into());
        }
        match global.initial_mm_probe_snapshot() {
            Ok(snapshot) => self.final_state = Some(snapshot),
            Err(error) => self.errors.push(error.into()),
        }
    }
    pub(super) fn finish_outcome(
        &mut self,
        primary: &io::Result<Option<ExitStatus>>,
        errors: &[String],
    ) {
        self.root_errors = errors.to_vec();
        self.intended_primary = intended_primary(primary);
    }
}

#[derive(Debug)]
pub(super) struct CancellationRequested;
impl std::fmt::Display for CancellationRequested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("intentional physical MM checkpoint cancellation")
    }
}
impl std::error::Error for CancellationRequested {}

fn intended_primary(primary: &io::Result<Option<ExitStatus>>) -> bool {
    primary
        .as_ref()
        .err()
        .and_then(io::Error::get_ref)
        .is_some_and(|error| error.is::<CancellationRequested>())
}

fn complete_cancel_outcome(primary: bool, errors: &[String]) -> bool {
    primary && errors.is_empty()
}

pub(crate) fn cancel_at(
    point: Point,
    command: Command,
    config: detcore::Config,
    retained: &RetainedImage,
) -> anyhow::Result<()> {
    // The existing RunOpts container bridge supplied the real command,
    // complete identity-bearing Config and SAME parent-retained image.
    anyhow::ensure!(
        config.sequentialize_threads,
        "physical MM control requires the actual sequential scheduler"
    );
    let records = Arc::new(std::sync::Mutex::new((Vec::<u8>::new(), false)));
    let sink_records = records.clone();
    let sink: detcore::detlog::ForwardedRecordSink = Box::new(move |bytes| {
        let mut records = sink_records.lock().unwrap();
        if records
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > 8 * 1024 * 1024)
        {
            records.1 = true;
        } else {
            records.0.extend_from_slice(bytes);
        }
    });
    let mut cancellation = Cancellation::new(point);
    // SAFETY: this physical fixture needs the SAME bounded original-thread,
    // source/image valid-path, XState, signal/RPC/lease and external subtree
    // containment contract as existing mapped initializer controls. This is not
    // an invocation recipe or permission to execute an unqualified image.
    let result = unsafe {
        run_inner(
            command,
            config,
            retained,
            sink,
            false,
            &None,
            Some(&mut cancellation),
        )
    };
    // Retain the complete genuine primary/transport/cleanup outcome, including
    // actual transport/cleanup failures. Any nonempty cleanup error still FAILS.
    // The separately retained cancellation outcome must come from the owned
    // request select and real task join; no blanket Err-as-success rule.
    eprintln!(
        "MM_CANCEL point={point:?} result={result:?} at={:?} retired={:?} final={:?} root_errors={:?} errors={:?} cancelled_requests={:?}",
        cancellation.at,
        cancellation.retired,
        cancellation.final_state,
        cancellation.root_errors,
        cancellation.errors,
        cancellation.cancelled_requests
    );
    let error = result.expect_err("cancel-only fixture unexpectedly returned Guest status");
    anyhow::ensure!(
        complete_cancel_outcome(cancellation.intended_primary, &cancellation.root_errors),
        "different primary or late cleanup failure: {error:#}"
    );
    anyhow::ensure!(
        cancellation.confirmed && cancellation.watch_drained && cancellation.services_joined,
        "incomplete real physical/service cleanup"
    );
    anyhow::ensure!(
        cancellation.errors.is_empty() && cancellation.root_errors.is_empty(),
        "retained cancellation/cleanup failures need exact classification"
    );
    let at = cancellation
        .at
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("actual checkpoint absent"))?;
    anyhow::ensure!(
        at.requested_is_initial_current,
        "actual lifecycle did not send the initial-current MM: {at:?}"
    );
    match point {
        Point::BeforeRegistration => {
            anyhow::ensure!(
                !at.registered && !at.next_turn && !at.holds_grant,
                "unexpected pre-registration scheduler state: {at:?}"
            );
        }
        Point::RegisteredBeforeGrant => {
            anyhow::ensure!(
                at.registered
                    && at.current_matches
                    && at.accepts_current
                    && !at.accepts_old_initial
                    && at.root == Some(at.tid)
                    && at.next_turn
                    && !at.holds_grant,
                "wrong registered birth: {at:?}"
            );
        }
        Point::ThreadStartup | Point::PostExec => {
            anyhow::ensure!(
                at.registered
                    && at.current_matches
                    && at.accepts_current
                    && !at.accepts_old_initial
                    && at.root == Some(at.tid),
                "wrong real startup/post-exec birth: {at:?}"
            );
        }
    }
    let final_state = cancellation
        .final_state
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("real final scheduler snapshot absent"))?;
    // New cancellation contract: an unfinished handler is CANCELLED, not
    // resumed to manufacture a reply after its client has been physically reaped.
    let expected_registration = match point {
        Point::BeforeRegistration | Point::RegisteredBeforeGrant => None,
        Point::ThreadStartup | Point::PostExec => Some(RegistrationResult::Continue),
    };
    anyhow::ensure!(
        final_state.registration_result == expected_registration,
        "registration ran after cancellation or earlier completion differs: {final_state:?}"
    );
    anyhow::ensure!(
        cancellation.cancelled_requests.len() == 1
            && cancellation.cancelled_requests[0].process_id() == at.tid
            && cancellation.cancelled_requests[0].sender() == at.tid
            && cancellation.cancelled_requests[0].connection() == 1
            && cancellation.cancelled_requests[0].request() > 0,
        "actual cancelled-request/join outcome absent or differs"
    );
    for retired in [&cancellation.retired, &cancellation.final_state] {
        let retired = retired
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("real retired snapshot absent"))?;
        anyhow::ensure!(
            !retired.next_turn && !retired.holds_grant,
            "orphan request/grant remains: {retired:?}"
        );
        // BeforeRegistration may legitimately have no registered row to kill.
        if point != Point::BeforeRegistration {
            anyhow::ensure!(
                retired.killed && !retired.accepts_current && !retired.accepts_old_initial,
                "dead birth can acquire authority: {retired:?}"
            );
        }
    }
    let records = records.lock().unwrap();
    anyhow::ensure!(
        !records.1,
        "actual cancellation DETLOG exceeded retained bound"
    );
    eprintln!("MM_CANCEL retained_detlog_bytes={}", records.0.len());
    retained.verify()?;
    Ok(())
}

#[test]
fn physical_mm_cancel_before_registration() -> anyhow::Result<()> {
    crate::run::physical_mm_cancellation_control(Point::BeforeRegistration)
}
#[test]
fn physical_mm_cancel_registered_before_first_grant() -> anyhow::Result<()> {
    crate::run::physical_mm_cancellation_control(Point::RegisteredBeforeGrant)
}
#[test]
fn physical_mm_cancel_during_thread_startup() -> anyhow::Result<()> {
    crate::run::physical_mm_cancellation_control(Point::ThreadStartup)
}
#[test]
fn physical_mm_cancel_during_post_exec() -> anyhow::Result<()> {
    crate::run::physical_mm_cancellation_control(Point::PostExec)
}

// Exercise the actual acceptance predicate and final-outcome capture, without
// invoking a physical child or claiming these data controls cover cleanup.
#[test]
fn cancellation_primary_is_exact_and_late_errors_refuse() {
    let mut cancellation = Cancellation::new(Point::BeforeRegistration);
    let primary = Err(io::Error::other(CancellationRequested));
    cancellation.finish_outcome(&primary, &[]);
    assert!(complete_cancel_outcome(
        cancellation.intended_primary,
        &cancellation.root_errors
    ));
    for late in [
        "actual runtime source closure",
        "mapped GlobalState still owned after drain",
        "actual global finalization",
    ] {
        cancellation.finish_outcome(&primary, &[late.to_owned()]);
        assert!(!complete_cancel_outcome(
            cancellation.intended_primary,
            &cancellation.root_errors
        ));
        assert_eq!(cancellation.root_errors, [late]);
    }
    let unrelated = Err(io::Error::other(
        "intentional physical MM checkpoint cancellation",
    ));
    cancellation.finish_outcome(&unrelated, &[]);
    assert!(!complete_cancel_outcome(
        cancellation.intended_primary,
        &cancellation.root_errors
    ));
    let missing = Ok(None);
    cancellation.finish_outcome(&missing, &[]);
    assert!(!complete_cancel_outcome(
        cancellation.intended_primary,
        &cancellation.root_errors
    ));
}
