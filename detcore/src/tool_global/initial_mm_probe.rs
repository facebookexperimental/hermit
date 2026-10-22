//! Explicit host-test rendezvous over the real GlobalState, not a scheduler,
//! lifecycle owner, RPC replacement or execution capability.
use tokio::sync::oneshot;

use super::*;

/// One real lifecycle edge at which a physical cancellation control pauses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    /// Before the root self-registration performs its ordinary admission checks.
    BeforeRegistration,
    /// After registration publishes its slot, before the initial grant request.
    RegisteredBeforeGrant,
    /// Before the actual thread-start callback requests scheduler resources.
    ThreadStartup,
    /// After successful image-state updates, before the final response recheck.
    PostExec,
}

/// The outcome actually returned by the registration handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationResult {
    /// The real handler accepted registration.
    Continue,
    /// The real handler refused the retired birth.
    ThreadExited,
    /// More than one registration result was observed for this control.
    Duplicate,
}

/// State read under the real scheduler mutex. Acceptance includes the killed
/// guard as well as the current address-space generation.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// The actual self-registering task selected by this control.
    pub tid: i32,
    /// Debug representation of the address-space generation in the actual RPC.
    pub requested_mm: String,
    /// Whether the RPC names the exec generation of this initial root.
    pub requested_is_initial_current: bool,
    /// Generation currently registered in the scheduler, if any.
    pub current_mm: Option<String>,
    /// Root task bound by ordinary self-registration, if it has occurred.
    pub root: Option<i32>,
    /// Whether the scheduler has recorded registration for this task.
    pub registered: bool,
    /// Whether the registered generation equals the actual RPC generation.
    pub current_matches: bool,
    /// Whether the current-generation RPC passes incarnation and killed checks.
    pub accepts_current: bool,
    /// Whether the previous initial generation passes those same checks.
    pub accepts_old_initial: bool,
    /// Whether the scheduler has logically retired this task.
    pub killed: bool,
    /// Whether a request/response slot still exists for the task.
    pub next_turn: bool,
    /// Whether the task currently owns the serial execution grant.
    pub holds_grant: bool,
    /// Actual registration outcome observed so far, including duplicate results.
    pub registration_result: Option<RegistrationResult>,
}

/// An observed lifecycle pause whose release resumes the existing handler.
pub struct Pause {
    /// The actual lifecycle edge reached by the handler.
    pub point: Point,
    /// Scheduler state captured before pausing, with no mutex held afterward.
    pub snapshot: Snapshot,
    release: Option<oneshot::Sender<()>>,
}
impl Pause {
    /// Normal and failure paths may release only after physical retirement.
    /// Dropping this object also wakes the real handler; it does not fabricate
    /// a reply, grant or cleanup result.
    pub fn release(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

/// The receiving side of one test-only lifecycle rendezvous.
pub struct Control {
    reached: oneshot::Receiver<Pause>,
}
impl Control {
    /// Wait for the selected actual handler edge; sender loss is an error.
    pub async fn reached(&mut self) -> Result<Pause, &'static str> {
        (&mut self.reached)
            .await
            .map_err(|_| "MM checkpoint sender disappeared")
    }
}

pub(super) struct Probe {
    tid: DetTid,
    point: Point,
    reached: Mutex<Option<oneshot::Sender<Pause>>>,
    registration_result: Mutex<Option<RegistrationResult>>,
}

// GlobalState derives Debug. Do not lock either rendezvous mutex merely to
// format its owning state (including on cleanup/error diagnostics).
impl std::fmt::Debug for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Probe")
            .field("tid", &self.tid)
            .field("point", &self.point)
            .finish_non_exhaustive()
    }
}

impl GlobalState {
    /// Install before the actual child is released, once for this GlobalState.
    /// Numeric PID here selects a host-test rendezvous only; it cannot admit a
    /// task, set its MM, report death, issue a grant or authorize execution.
    pub fn install_initial_mm_probe(
        &self,
        point: Point,
        tid: i32,
    ) -> Result<Control, &'static str> {
        if !self.cfg.sequentialize_threads {
            return Err("MM cancellation control requires the real sequential scheduler");
        }
        let mut slot = self.initial_mm_probe.lock().unwrap();
        if slot.is_some() {
            return Err("MM cancellation probe already installed");
        }
        let (reached, receiver) = oneshot::channel();
        *slot = Some(Arc::new(Probe {
            tid: DetTid::from_raw(tid),
            point,
            reached: Mutex::new(Some(reached)),
            registration_result: Mutex::new(None),
        }));
        Ok(Control { reached: receiver })
    }

    fn initial_mm_snapshot_locked(
        sched: &Scheduler,
        root: Option<i32>,
        tid: DetTid,
        request_mm: MmId,
    ) -> Snapshot {
        let killed = sched.thread_is_logically_killed(tid);
        Snapshot {
            tid: tid.as_raw(),
            requested_mm: format!("{request_mm:?}"),
            requested_is_initial_current: request_mm == MmId::initial(tid).for_exec(tid),
            current_mm: sched.thread_mm(tid).map(|mm| format!("{mm:?}")),
            root,
            registered: sched.thread_was_registered(tid),
            current_matches: sched.thread_mm(tid) == Some(request_mm),
            accepts_current: !killed && sched.rpc_incarnation_matches(tid, request_mm),
            accepts_old_initial: !killed && sched.rpc_incarnation_matches(tid, MmId::initial(tid)),
            killed,
            next_turn: sched.next_turns.contains_key(&tid),
            holds_grant: sched.holds_serial_grant(tid),
            registration_result: None,
        }
    }

    /// Inspect only the installed test birth. Expected MM1 is derived exactly as
    /// the real initial-image lifecycle does, never stored by this helper.
    pub fn initial_mm_probe_snapshot(&self) -> Result<Snapshot, &'static str> {
        let probe = self
            .initial_mm_probe
            .lock()
            .unwrap()
            .clone()
            .ok_or("MM cancellation probe absent")?;
        let sched = self.sched.lock().unwrap();
        let mut snapshot = Self::initial_mm_snapshot_locked(
            &sched,
            self.root_dettid.get().map(|tid| tid.as_raw()),
            probe.tid,
            MmId::initial(probe.tid).for_exec(probe.tid),
        );
        snapshot.registration_result = *probe.registration_result.lock().unwrap();
        Ok(snapshot)
    }

    pub(super) fn initial_mm_registration_result(
        &self,
        tid: DetTid,
        result: &SchedulerRpcResult<()>,
    ) {
        let probe = self.initial_mm_probe.lock().unwrap().clone();
        let Some(probe) = probe else {
            return;
        };
        if probe.tid != tid {
            return;
        }
        let mut recorded = probe.registration_result.lock().unwrap();
        *recorded = Some(if recorded.is_some() {
            RegistrationResult::Duplicate
        } else {
            match result {
                SchedulerRpcResult::Continue(()) => RegistrationResult::Continue,
                SchedulerRpcResult::ThreadExited => RegistrationResult::ThreadExited,
            }
        });
    }

    pub(super) async fn initial_mm_checkpoint(&self, point: Point, tid: DetTid, request_mm: MmId) {
        let probe = self.initial_mm_probe.lock().unwrap().clone();
        let Some(probe) = probe else {
            return;
        };
        if probe.tid != tid || probe.point != point {
            return;
        }
        let sender = probe.reached.lock().unwrap().take();
        let Some(sender) = sender else {
            return;
        };
        let mut snapshot = {
            let sched = self.sched.lock().unwrap();
            Self::initial_mm_snapshot_locked(
                &sched,
                self.root_dettid.get().map(|tid| tid.as_raw()),
                tid,
                request_mm,
            )
        };
        snapshot.registration_result = *probe.registration_result.lock().unwrap();
        let (release, released) = oneshot::channel();
        // No scheduler mutex survives this send or await. Receiver cancellation
        // cannot mint success: the real handler resumes its unchanged checks.
        if sender
            .send(Pause {
                point,
                snapshot,
                release: Some(release),
            })
            .is_ok()
        {
            let _ = released.await;
        }
    }
}
