/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The identity of a run's RCB clock, which decides which recorded histories
//! the run may execute and what identity the histories it writes state.
//!
//! A run is prepared in four phases before anything that counts or writes
//! exists ([`prepare_run`]):
//!
//! 1. qualify: the backend's clock contract, the host's PMU profile and the
//!    process's installed PMU configuration give the effective identity,
//!    checked against the host's prepared claim; nothing is changed. Whether
//!    Detcore observes the counter (a maximum timeslice) or charges it as
//!    virtual time (`no_rcb_time`) does not qualify or unqualify it;
//! 2. admit every supplied executable input against it;
//! 3. pin the qualified clock key once, under the PMU state's lock, so that
//!    no counter of another key is installed afterwards;
//! 4. build the scheduler and writer from the admitted inputs and the
//!    committed identity.
//!
//! A refusal in phases 1 to 3 comes before any pin, writer, counter or guest.

use std::path::Path;

use detcore_model::config::RcbClockClaim;
use reverie::pmu_config::ClockKey;
use reverie::pmu_config::PinTicket;
use reverie::pmu_config::PmuState;
use reverie::pmu_config::QualifiedClock;
use reverie::pmu_config::RcbMetric;
use reverie::pmu_config::UnqualifiedReason;

use crate::config::Config;
use crate::preemptions::HistoryIdentity;
use crate::preemptions::boundary_recording::HistoryReader;
use crate::preemptions::boundary_recording::Recording;
use crate::scheduler::AdmittedSchedule;

/// The identity of the RCB coordinates a run counts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProducerIdentity {
    /// A qualified clock: its metric and the key it pins.
    Known {
        /// What the clock counts.
        metric: RcbMetric,
        /// The clock key pinned for the run.
        key: ClockKey,
    },
    /// Not a qualified clock, for the reason given.
    Unqualified(String),
}

impl ProducerIdentity {
    /// The identity a history this run writes states.
    pub fn history_identity(&self) -> HistoryIdentity {
        match self {
            Self::Known { metric, .. } => HistoryIdentity::Known(*metric),
            Self::Unqualified(reason) => HistoryIdentity::Unqualified(reason.clone()),
        }
    }
}

/// Phase 1's result: the effective identity, and for a qualified clock the
/// ticket phase 3 commits. It authorizes nothing by itself.
#[derive(Debug)]
pub struct ValidatedClock {
    producer: ProducerIdentity,
    ticket: Option<PinTicket>,
}

/// Phase 3's result, the only authority a writer or executed history uses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedClock {
    producer: ProducerIdentity,
}

impl CommittedClock {
    /// The run's committed identity.
    pub fn producer(&self) -> &ProducerIdentity {
        &self.producer
    }

    /// The identity of the coordinates this run writes.
    pub fn history_identity(&self) -> HistoryIdentity {
        self.producer.history_identity()
    }
}

/// Why a run cannot be prepared, as policy rather than as a broken file.
#[derive(Debug)]
pub enum PrepareError {
    /// A named refusal: the run must not start.
    Refused(String),
    /// A file could not be read or parsed; the ordinary loader's panic class.
    Unreadable(anyhow::Error),
}

/// Where phase 1 reads the host and the PMU state. Production reads this
/// host's CPUID and the process's PMU state; tests supply both.
pub struct ClockSources<'a> {
    /// This host's qualification.
    pub host: Result<QualifiedClock, UnqualifiedReason>,
    /// The process's PMU state.
    pub state: &'a PmuState,
}

impl ClockSources<'static> {
    /// This host and this process.
    pub fn process() -> Self {
        // Unit tests do not depend on the host running them: unless a test
        // supplies its own sources, they see a fixed qualified host and a PMU
        // state of their own, which no real counter reads.
        #[cfg(test)]
        return test_sources::current().unwrap_or_else(test_sources::default_sources);
        #[cfg(not(test))]
        Self {
            host: reverie::pmu_config::resolve_host_clock(),
            state: reverie::pmu_config::pmu_state(),
        }
    }
}

/// Phase 1. The effective identity comes from the backend's clock contract,
/// the host's profile and the installed PMU configuration, whether or not
/// Detcore consumes the counter; `cfg.rcb_clock_claim` is only checked
/// against it. A `Known` claim the effective clock contradicts is refused; a
/// run nobody prepared is unqualified.
pub fn validate_run_clock(
    cfg: &Config,
    sources: &ClockSources<'_>,
) -> Result<ValidatedClock, String> {
    let effective: Result<(QualifiedClock, PinTicket), String> =
        match cfg.backend.rcb_clock_contract {
            None => Err("no-mechanism-contract".to_owned()),
            Some(_) => match &sources.host {
                Err(reason) => Err(reason.to_string()),
                Ok(host) => sources
                    .state
                    .qualify(host)
                    .map(|ticket| (host.clone(), ticket))
                    .map_err(|refusal| format!("pmu-state: {refusal}")),
            },
        };
    match (cfg.rcb_clock_claim, effective) {
        (RcbClockClaim::Unset, _) => Ok(ValidatedClock {
            producer: ProducerIdentity::Unqualified("claim-unset".to_owned()),
            ticket: None,
        }),
        (RcbClockClaim::Known(claimed), Ok((host, ticket))) if claimed == ticket.key() => {
            Ok(ValidatedClock {
                producer: ProducerIdentity::Known {
                    metric: host.metric,
                    key: ticket.key(),
                },
                ticket: Some(ticket),
            })
        }
        (RcbClockClaim::Known(claimed), Ok((_, ticket))) => Err(format!(
            "hermit refused the run: it was prepared for RCB clock {claimed:?}, but the \
             effective clock is {:?}",
            ticket.key()
        )),
        (RcbClockClaim::Known(claimed), Err(reason)) => Err(format!(
            "hermit refused the run: it was prepared for RCB clock {claimed:?}, but the \
             effective clock is not qualified ({reason})"
        )),
    }
}

impl ValidatedClock {
    /// The effective identity, before it is committed.
    pub fn producer(&self) -> &ProducerIdentity {
        &self.producer
    }

    /// Phase 2: may `recording`, read from `source`, run under this clock?
    pub fn admit(&self, source: &str, recording: &Recording) -> Result<(), String> {
        admit_identity(source, recording.identity(), &self.producer)
    }

    /// Phase 3: pins the qualified key in `state`, rechecking it under the
    /// lock. Unqualified runs pin nothing.
    pub fn commit(self, state: &PmuState) -> Result<CommittedClock, String> {
        if let Some(ticket) = self.ticket {
            state
                .commit_pin(ticket)
                .map_err(|refusal| format!("hermit refused the run: {refusal}"))?;
        }
        Ok(CommittedClock {
            producer: self.producer,
        })
    }
}

/// May a history stating `recorded`, read from `source`, run under
/// `consumer`? Only a qualified history under a qualified clock of the same
/// metric may; a raw event or model need not match.
pub fn admit_identity(
    source: &str,
    recorded: Option<&HistoryIdentity>,
    consumer: &ProducerIdentity,
) -> Result<(), String> {
    match (recorded, consumer) {
        (None, _) => Err(format!(
            "hermit refused to replay {source}: it states no RCB coordinate identity, so the \
             unit of its recorded coordinates is unknown (it was written before identities were \
             recorded, or by hand). Record it again with this build, or replay it with the \
             build that made it."
        )),
        (Some(HistoryIdentity::Unqualified(reason)), _) => Err(format!(
            "hermit refused to replay {source}: it was recorded with an unqualified RCB clock \
             ({reason}), so its coordinates cannot be executed."
        )),
        (Some(HistoryIdentity::Known(metric)), ProducerIdentity::Unqualified(reason)) => {
            Err(format!(
                "hermit refused to replay {source} ({}): this run's RCB clock is not qualified \
                 ({reason}).",
                metric.id()
            ))
        }
        (Some(HistoryIdentity::Known(recorded)), ProducerIdentity::Known { metric, .. }) => {
            if recorded == metric {
                Ok(())
            } else {
                Err(format!(
                    "hermit refused to replay {source}: it counts {}, and this run counts {}.",
                    recorded.id(),
                    metric.id()
                ))
            }
        }
    }
}

/// The identity a history derived from both `a` and `b` (each a source name
/// and the identity it states) keeps: theirs, which must be the same. It is
/// copied from the sources, never taken from the current host, so a
/// transformation cannot change what its coordinates count.
pub fn combined_identity(
    a: (&str, Option<&HistoryIdentity>),
    b: (&str, Option<&HistoryIdentity>),
) -> Result<Option<HistoryIdentity>, String> {
    if a.1 == b.1 {
        return Ok(a.1.cloned());
    }
    let show = |identity: Option<&HistoryIdentity>| match identity {
        Some(identity) => identity.to_string(),
        None => "no stated identity".to_owned(),
    };
    Err(format!(
        "hermit refused to combine {} ({}) with {} ({}): their RCB coordinates do not \
         count the same thing, so no schedule mixing them can be replayed.",
        a.0,
        show(a.1),
        b.0,
        show(b.1),
    ))
}

/// Phases 1 to 3's outcome, with the admitted inputs phase 4 builds from.
pub(crate) struct PreparedRun {
    /// The committed identity.
    pub(crate) clock: CommittedClock,
    /// The admitted `replay_schedule_from` recording, with its stack
    /// selections resolved.
    pub(crate) schedule: Option<AdmittedSchedule>,
    /// The admitted `replay_preemptions_from` history.
    pub(crate) history: Option<HistoryReader>,
}

/// Phases 1 to 3 against `sources`.
pub(crate) fn prepare_run_with(
    cfg: &Config,
    sources: &ClockSources<'_>,
) -> Result<PreparedRun, PrepareError> {
    let validated = validate_run_clock(cfg, sources).map_err(PrepareError::Refused)?;
    let schedule = match &cfg.replay_schedule_from {
        None => None,
        Some(path) => {
            let recording = Recording::read(path).map_err(|error| {
                // A format refusal keeps its policy exit, as the scheduler's
                // loader gave it before preparation existed; only unreadable
                // or malformed files keep the loader's panic class.
                if error
                    .chain()
                    .any(|cause| cause.is::<crate::preemptions::RecordFormatRefusal>())
                {
                    PrepareError::Refused(format!("{error:#}"))
                } else {
                    PrepareError::Unreadable(error)
                }
            })?;
            recording
                .require_strict_mode(cfg.strict_schedule_boundaries)
                .map_err(|error| PrepareError::Refused(error.to_string()))?;
            // The same replay-input validation the priority route makes in
            // `read_for_replay`, before identity admission, so a record that
            // fails both takes the same refusal on either route. The
            // scheduler validates the installed record again at construction.
            recording
                .validate_replay_inputs(&input_name("--replay-schedule-from", path), cfg.guest_ids)
                .map_err(|refusal| PrepareError::Refused(refusal.to_string()))?;
            validated
                .admit(&input_name("--replay-schedule-from", path), &recording)
                .map_err(PrepareError::Refused)?;
            Some(
                AdmittedSchedule::new(cfg, recording)
                    .map_err(|refusal| PrepareError::Refused(refusal.to_owned()))?,
            )
        }
    };
    let history = match &cfg.replay_preemptions_from {
        None => None,
        Some(path) => {
            let history = HistoryReader::read_for_replay(path, cfg.guest_ids).map_err(|error| {
                if error.chain().any(|cause| {
                    cause.is::<crate::preemptions::RecordAdmissionRefusal>()
                        || cause.is::<crate::preemptions::RecordFormatRefusal>()
                }) {
                    PrepareError::Refused(format!("{error:#}"))
                } else {
                    PrepareError::Unreadable(error)
                }
            })?;
            history
                .require_legacy_mode()
                .map_err(|error| PrepareError::Refused(error.to_string()))?;
            validated
                .admit(
                    &input_name("--replay-preemptions-from", path),
                    history.recording(),
                )
                .map_err(PrepareError::Refused)?;
            Some(history)
        }
    };
    // The priority history's callback checkpoints must agree with the launch
    // decision; the scheduler installs them from this same object.
    if let Some(refusal) = crate::scheduler::priority_checkpoint_mode_refusal(
        cfg.annotated_priority_replay,
        history.as_ref().map(HistoryReader::callback_checkpoints),
    ) {
        return Err(PrepareError::Refused(refusal));
    }
    // Faithful replay refuses a departed record before the pin too, unless
    // the run explicitly explores edited histories; construction repeats
    // the check on the installed objects.
    if !cfg.replay_edited_preemptions {
        if let Some(schedule) = &schedule {
            schedule
                .recording
                .require_faithful_replay()
                .map_err(|error| PrepareError::Refused(error.to_string()))?;
        }
        if let Some(history) = &history {
            history
                .recording()
                .require_faithful_replay()
                .map_err(|error| PrepareError::Refused(error.to_string()))?;
        }
    }
    let clock = validated
        .commit(sources.state)
        .map_err(PrepareError::Refused)?;
    Ok(PreparedRun {
        clock,
        schedule,
        history,
    })
}

/// Phases 1 to 3 for this process. A refusal ends the run with the policy
/// exit, before any guest runs; an unreadable file keeps the loader's panic.
pub(crate) fn prepare_run(cfg: &Config) -> PreparedRun {
    match prepare_run_with(cfg, &ClockSources::process()) {
        Ok(prepared) => prepared,
        Err(PrepareError::Refused(reason)) => crate::scheduler::exit_on_scheduler_refusal(&reason),
        Err(PrepareError::Unreadable(error)) => panic!("{error:?}"),
    }
}

fn input_name(option: &str, path: &Path) -> String {
    format!("{option} {}", path.display())
}

#[cfg(test)]
pub(crate) mod test_sources {
    use std::cell::RefCell;

    use super::*;

    thread_local! {
        static SOURCES: RefCell<Option<(Result<QualifiedClock, UnqualifiedReason>, &'static PmuState)>> =
            const { RefCell::new(None) };
    }

    /// Runs `f` with `host` and a fresh, leaked PMU state as this thread's
    /// clock sources.
    pub(crate) fn with<R>(
        host: Result<QualifiedClock, UnqualifiedReason>,
        f: impl FnOnce(&'static PmuState) -> R,
    ) -> R {
        let state: &'static PmuState = Box::leak(Box::new(PmuState::new()));
        SOURCES.with(|slot| *slot.borrow_mut() = Some((host, state)));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                SOURCES.with(|slot| *slot.borrow_mut() = None);
            }
        }
        let _reset = Reset;
        f(state)
    }

    /// The qualified host unit tests see by default (AMD family 1Ah model 11h).
    pub(crate) fn test_host() -> QualifiedClock {
        reverie::pmu_config::qualify_host("AuthenticAMD", 0x1A, 0x11).unwrap()
    }

    /// The identity of histories the default test host writes.
    pub(crate) fn known() -> HistoryIdentity {
        HistoryIdentity::Known(test_host().metric)
    }

    /// A synthetic callback-annotated priority history stating `identity`:
    /// thread 7's two rows, the second ended by a write prehook checkpoint
    /// at the retained SPR record's coordinates, with the events that reach
    /// it. It records no run.
    pub(crate) fn annotated_history(identity: Option<HistoryIdentity>) -> String {
        use detcore_model::schedule::Op;
        use detcore_model::schedule::SchedEvent;
        use reverie::syscalls::Sysno;

        use crate::DEFAULT_PRIORITY;
        use crate::preemptions::PreemptionWriter;
        use crate::preemptions::callback_history::Origin;
        use crate::preemptions::callback_history::Site;
        use crate::slice_boundary::callbacks::Callback;
        use crate::types::DetTid;
        use crate::types::LogicalTime;

        let tid = DetTid::from_raw(7);
        let mut writer = PreemptionWriter::new(None);
        if let Some(identity) = identity {
            writer = writer.with_identity(identity);
        }
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_schedevent(SchedEvent::branches(tid, 2_459_579));
        let first = LogicalTime::from_nanos(1_792_852_217_000_000_000);
        writer.insert_reprioritization(
            tid,
            first,
            2_459_579,
            DEFAULT_PRIORITY,
            DEFAULT_PRIORITY,
            None,
        );
        writer.insert_schedevent(SchedEvent::branches(tid, 8_299_750));
        writer.insert_schedevent(SchedEvent {
            op: Op::OtherInstructions,
            count: 154,
            ..SchedEvent::branches(tid, 0)
        });
        let second = LogicalTime::from_nanos(1_792_852_217_684_087_955);
        writer.insert_reprioritization(
            tid,
            second,
            10_759_329,
            DEFAULT_PRIORITY,
            DEFAULT_PRIORITY,
            None,
        );
        writer.insert_callback_checkpoint(
            tid,
            Origin {
                callback: Callback::Syscall(Sysno::write),
                site: Site::Prehook,
                progress: 10_759_483,
                last_op: Some(Op::OtherInstructions),
                time: second,
                rcbs: 10_759_329,
            },
        );
        writer.into_string()
    }

    /// `cfg` prepared as `prepare_backend_config` prepares a ptrace run on the
    /// default test host.
    pub(crate) fn qualified(mut cfg: Config) -> Config {
        cfg.backend.rcb_clock_contract =
            Some(reverie::RcbClockContract::PerfCounterOnTargetThreadUserOnly);
        cfg.rcb_clock_claim = RcbClockClaim::Known(test_host().key());
        cfg
    }

    pub(super) fn default_sources() -> ClockSources<'static> {
        static STATE: PmuState = PmuState::new();
        ClockSources {
            host: Ok(test_host()),
            state: &STATE,
        }
    }

    pub(super) fn current() -> Option<ClockSources<'static>> {
        SOURCES.with(|slot| {
            slot.borrow().as_ref().map(|(host, state)| ClockSources {
                host: host.clone(),
                state,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use reverie::BackendCapabilities;
    use reverie::RcbClockContract;
    use reverie::pmu_config::PmuConfig;
    use reverie::pmu_config::qualify_host;

    use super::*;
    use crate::preemptions::PreemptionRecord;

    const V1: HistoryIdentity = HistoryIdentity::Known(RcbMetric::ConditionalBranchesRetiredUserV1);

    fn amd() -> QualifiedClock {
        qualify_host("AuthenticAMD", 0x1A, 0x11).unwrap()
    }

    fn skylake_server() -> QualifiedClock {
        qualify_host("GenuineIntel", 0x06, 0x55).unwrap()
    }

    fn sapphire_rapids() -> QualifiedClock {
        qualify_host("GenuineIntel", 0x06, 0x8F).unwrap()
    }

    /// A ptrace configuration prepared for `host`, as `prepare_backend_config`
    /// prepares one.
    fn prepared(host: &QualifiedClock) -> Config {
        let mut cfg = Config::default();
        cfg.backend.rcb_clock_contract = Some(RcbClockContract::PerfCounterOnTargetThreadUserOnly);
        cfg.rcb_clock_claim = RcbClockClaim::Known(host.key());
        cfg
    }

    fn on<'a>(host: &QualifiedClock, state: &'a PmuState) -> ClockSources<'a> {
        ClockSources {
            host: Ok(host.clone()),
            state,
        }
    }

    fn write(path: &Path, identity: Option<HistoryIdentity>) {
        PreemptionRecord::from_sched_events_with_identity(Vec::new(), identity)
            .write_to_disk(path)
            .unwrap();
    }

    /// A Known(v1) legacy schedule of `events`.
    fn write_events(path: &Path, events: Vec<detcore_model::schedule::SchedEvent>) {
        PreemptionRecord::from_sched_events_with_identity(events, Some(V1))
            .write_to_disk(path)
            .unwrap();
    }

    fn refusal(result: Result<PreparedRun, PrepareError>) -> String {
        match result {
            Err(PrepareError::Refused(reason)) => reason,
            Err(PrepareError::Unreadable(error)) => panic!("unreadable, not refused: {error:#}"),
            Ok(_) => panic!("prepared, not refused"),
        }
    }

    /// Qualified hosts whose raw selectors differ count one metric, so each
    /// admits the others' histories.
    #[test]
    fn hosts_with_different_raw_events_admit_one_metric() {
        let hosts = [amd(), skylake_server(), sapphire_rapids()];
        let keys: Vec<ClockKey> = hosts.iter().map(QualifiedClock::key).collect();
        assert_ne!(keys[0].raw_event, keys[1].raw_event);
        assert_ne!(keys[0].raw_event, keys[2].raw_event);
        for host in &hosts {
            let state = PmuState::new();
            let validated = validate_run_clock(&prepared(host), &on(host, &state)).unwrap();
            assert_eq!(
                validated.producer(),
                &ProducerIdentity::Known {
                    metric: RcbMetric::ConditionalBranchesRetiredUserV1,
                    key: host.key(),
                }
            );
            admit_identity("h", Some(&V1), validated.producer()).unwrap();
        }
    }

    /// A legacy or unqualified history is refused on every qualified
    /// consumer, and an unprepared consumer refuses a qualified history.
    #[test]
    fn unknown_or_unqualified_histories_and_consumers_are_refused() {
        for host in [amd(), skylake_server(), sapphire_rapids()] {
            let state = PmuState::new();
            let consumer = validate_run_clock(&prepared(&host), &on(&host, &state)).unwrap();
            let legacy = admit_identity("h", None, consumer.producer()).unwrap_err();
            assert!(
                legacy.contains("states no RCB coordinate identity"),
                "{legacy}"
            );
            let unqualified = HistoryIdentity::Unqualified("claim-unset".to_owned());
            let refused = admit_identity("h", Some(&unqualified), consumer.producer()).unwrap_err();
            assert!(
                refused.contains("unqualified RCB clock (claim-unset)"),
                "{refused}"
            );
        }
        let state = PmuState::new();
        let mut unprepared = prepared(&amd());
        unprepared.rcb_clock_claim = RcbClockClaim::Unset;
        let consumer = validate_run_clock(&unprepared, &on(&amd(), &state)).unwrap();
        let refused = admit_identity("h", Some(&V1), consumer.producer()).unwrap_err();
        assert!(refused.contains("not qualified (claim-unset)"), "{refused}");
    }

    /// Whether Detcore observes the counter (a maximum timeslice), delivers
    /// its timer or charges it as virtual time does not change its
    /// qualification.
    #[test]
    fn a_counter_is_qualified_whether_or_not_detcore_observes_it() {
        let host = amd();
        for (max_timeslice, delivers, no_rcb_time) in [
            (Config::default().max_timeslice, true, false),
            (Config::default().max_timeslice, false, false),
            (Config::default().max_timeslice, false, true),
            (None, false, false),
            (None, false, true),
        ] {
            let mut cfg = prepared(&host);
            cfg.max_timeslice = max_timeslice;
            cfg.backend.delivers_preemption_timer_events = delivers;
            cfg.no_rcb_time = no_rcb_time;
            let state = PmuState::new();
            let validated = validate_run_clock(&cfg, &on(&host, &state)).unwrap();
            assert_eq!(validated.producer().history_identity(), V1);
        }
    }

    /// A mechanism without the clock contract is never qualified: unprepared,
    /// it is unqualified; claimed, the claim is refused.
    #[test]
    fn mechanisms_without_a_clock_contract_are_not_qualified() {
        let host = amd();
        for backend in [
            BackendCapabilities::KVM,
            BackendCapabilities::LITEINST_IN_GUEST,
            BackendCapabilities::E9PATCH,
            BackendCapabilities::SABRE,
            BackendCapabilities::DBT,
        ] {
            assert_eq!(backend.rcb_clock_contract, None);
            let state = PmuState::new();
            let mut cfg = prepared(&host);
            cfg.backend = backend;
            let refused = validate_run_clock(&cfg, &on(&host, &state)).unwrap_err();
            assert!(refused.contains("no-mechanism-contract"), "{refused}");
            cfg.rcb_clock_claim = RcbClockClaim::Unset;
            let validated = validate_run_clock(&cfg, &on(&host, &state)).unwrap();
            assert_eq!(
                validated.producer(),
                &ProducerIdentity::Unqualified("claim-unset".to_owned())
            );
            assert_eq!(state.pinned(), None);
        }
    }

    /// A claim the host does not qualify, or that another installed profile
    /// contradicts, is refused, never written as Known.
    #[test]
    fn a_contradicted_claim_is_refused() {
        let state = PmuState::new();
        let unqualified = qualify_host("GenuineIntel", 0x06, 0x9A).unwrap_err();
        let sources = ClockSources {
            host: Err(unqualified),
            state: &state,
        };
        let refused = validate_run_clock(&prepared(&amd()), &sources).unwrap_err();
        assert!(refused.contains("not qualified"), "{refused}");

        let state = PmuState::new();
        state
            .set(PmuConfig::for_family_model(0x06, 0x8F).unwrap())
            .unwrap();
        let refused = validate_run_clock(&prepared(&amd()), &on(&amd(), &state)).unwrap_err();
        assert!(refused.contains("pmu-state"), "{refused}");

        let state = PmuState::new();
        let refused =
            validate_run_clock(&prepared(&sapphire_rapids()), &on(&amd(), &state)).unwrap_err();
        assert!(refused.contains("hermit refused the run"), "{refused}");
    }

    /// An unprepared run executes unqualified: it pins nothing, and what it
    /// writes cannot be replayed.
    #[test]
    fn an_unprepared_run_pins_nothing_and_writes_unqualified() {
        let state = PmuState::new();
        let mut cfg = prepared(&amd());
        cfg.rcb_clock_claim = RcbClockClaim::Unset;
        cfg.record_preemptions = true;
        let prepared = prepare_run_with(&cfg, &on(&amd(), &state)).unwrap();
        assert_eq!(state.pinned(), None);
        let written = prepared.clock.history_identity();
        assert_eq!(
            written,
            HistoryIdentity::Unqualified("claim-unset".to_owned())
        );
        let consumer = validate_run_clock(&prepared_for_replay(), &on(&amd(), &state)).unwrap();
        assert!(admit_identity("h", Some(&written), consumer.producer()).is_err());
    }

    fn prepared_for_replay() -> Config {
        prepared(&amd())
    }

    /// A record-only qualified run pins its key before it writes, and its
    /// writer's identity is the committed one.
    #[test]
    fn a_record_only_run_pins_before_writing() {
        let state = PmuState::new();
        let mut cfg = prepared(&amd());
        cfg.record_preemptions = true;
        let prepared = prepare_run_with(&cfg, &on(&amd(), &state)).unwrap();
        assert_eq!(state.pinned(), Some(amd().key()));
        assert_eq!(prepared.clock.history_identity(), V1);
        assert!(prepared.schedule.is_none() && prepared.history.is_none());
        let other = PmuConfig::for_family_model(0x06, 0x8F).unwrap();
        assert_eq!(state.set(other.clone()), Err(other));
        assert_eq!(state.pinned(), Some(amd().key()));
    }

    /// A ticket whose state changed before the commit is refused there, and
    /// nothing is pinned.
    #[test]
    fn a_stale_ticket_is_refused_at_the_commit() {
        let state = PmuState::new();
        let validated = validate_run_clock(&prepared(&amd()), &on(&amd(), &state)).unwrap();
        state
            .set(PmuConfig::for_family_model(0x06, 0x8F).unwrap())
            .unwrap();
        let refused = validated.commit(&state).unwrap_err();
        assert!(refused.contains("hermit refused the run"), "{refused}");
        assert_eq!(state.pinned(), None);
    }

    /// Both replay inputs are admitted before anything is pinned: a refused
    /// second input leaves no pin, in either order, and two admitted inputs
    /// pin once.
    #[test]
    fn every_input_is_admitted_before_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let known = dir.path().join("known.json");
        let legacy = dir.path().join("legacy.json");
        write(&known, Some(V1));
        write(&legacy, None);
        for (schedule, history, refused) in [
            (&known, &legacy, Some("--replay-preemptions-from")),
            (&legacy, &known, Some("--replay-schedule-from")),
            (&known, &known, None),
        ] {
            let state = PmuState::new();
            let mut cfg = prepared(&amd());
            cfg.replay_schedule_from = Some(schedule.clone());
            cfg.replay_preemptions_from = Some(history.clone());
            let result = prepare_run_with(&cfg, &on(&amd(), &state));
            match refused {
                Some(option) => {
                    let reason = refusal(result);
                    assert!(reason.contains(option), "{reason}");
                    assert_eq!(state.pinned(), None);
                }
                None => {
                    let prepared = result.unwrap();
                    assert_eq!(state.pinned(), Some(amd().key()));
                    assert_eq!(prepared.schedule.unwrap().recording.identity(), Some(&V1));
                    assert_eq!(prepared.history.unwrap().recording().identity(), Some(&V1));
                }
            }
        }
    }

    /// A well-formed envelope this build cannot execute -- a newer
    /// `rcb_history` format or an identity it does not know -- is a
    /// named refusal before the pin on either replay route, never an
    /// unreadable-file panic.
    #[test]
    fn unsupported_rcb_history_envelopes_are_refused_before_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let mutated = |name: &str, change: &dyn Fn(&mut serde_json::Value)| {
            let path = dir.path().join(name);
            write(&path, Some(V1));
            let mut value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            change(&mut value);
            std::fs::write(&path, value.to_string()).unwrap();
            path
        };
        for (path, cause) in [
            (
                mutated("format3.json", &|v| v["rcb_history"]["format"] = 3.into()),
                "unsupported rcb_history format 3",
            ),
            (
                mutated("unknown.json", &|v| {
                    v["rcb_history"]["identity"] = "rcb:cond-retired:user:v2".into()
                }),
                "unknown RCB coordinate identity",
            ),
        ] {
            for schedule in [true, false] {
                let state = PmuState::new();
                let mut cfg = prepared(&amd());
                if schedule {
                    cfg.replay_schedule_from = Some(path.clone());
                } else {
                    cfg.replay_preemptions_from = Some(path.clone());
                }
                let reason = refusal(prepare_run_with(&cfg, &on(&amd(), &state)));
                assert!(reason.contains(cause), "{reason}");
                assert!(reason.contains(&path.display().to_string()), "{reason}");
                assert_eq!(state.pinned(), None);
            }
        }
    }

    /// A slice-boundary schedule's `--stacktrace-event` indices are checked
    /// against its complete stream before the pin: an index outside it, or
    /// one given twice, is refused with nothing pinned, and valid indices
    /// reach construction as checked.
    #[test]
    fn stack_indices_are_checked_before_the_pin() {
        use detcore_model::schedule::SchedEvent;
        use detcore_model::schedule::boundary::Record;
        use detcore_model::schedule::boundary::Schedule;

        use crate::preemptions::boundary_recording::BoundaryRecording;
        use crate::scheduler::ReplayStacks;
        use crate::types::DetTid;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boundaries.json");
        let tid = DetTid::from_raw(1);
        Recording::Boundaries(
            BoundaryRecording::new(
                PreemptionRecord::from_sched_events_with_identity(Vec::new(), Some(V1)),
                Schedule::new(vec![
                    Record::Event(SchedEvent::branches(tid, 1)),
                    Record::Event(SchedEvent::branches(tid, 2)),
                ])
                .unwrap(),
            )
            .unwrap(),
        )
        .write(&path)
        .unwrap();
        let stack = |name: &str| Some(dir.path().join(name));
        for (indices, refused) in [
            (
                vec![(2, None)],
                Some("stack index is outside the complete stream"),
            ),
            (
                vec![(u64::MAX, None)],
                Some("stack index is outside the complete stream"),
            ),
            (
                vec![(1, stack("a")), (1, stack("b"))],
                Some("duplicate stack index"),
            ),
            (vec![(1, stack("a")), (0, None)], None),
        ] {
            let state = PmuState::new();
            let mut cfg = prepared(&amd());
            cfg.sequentialize_threads = true;
            cfg.strict_schedule_boundaries = true;
            cfg.replay_schedule_from = Some(path.clone());
            cfg.stacktrace_event = indices;
            let result = prepare_run_with(&cfg, &on(&amd(), &state));
            match refused {
                Some(cause) => {
                    let reason = refusal(result);
                    assert_eq!(reason, format!("slice-boundary protocol refusal: {cause}"));
                    assert_eq!(state.pinned(), None);
                }
                None => {
                    let prepared = result.unwrap();
                    assert_eq!(state.pinned(), Some(amd().key()));
                    assert_eq!(
                        prepared.schedule.unwrap().stacks,
                        ReplayStacks::Boundaries(std::collections::BTreeMap::from([
                            (0, None),
                            (1, stack("a"))
                        ]))
                    );
                }
            }
        }
    }

    /// A legacy schedule's `--stacktrace-event` indices are resolved before
    /// the pin too. On current main an index at or past the end names the
    /// events the run makes after the schedule runs out, so it resolves
    /// without a recorded event instead of panicking; a valid one reaches
    /// construction resolved the same way.
    #[test]
    fn legacy_stack_indices_are_resolved_before_the_pin() {
        use detcore_model::schedule::SchedEvent;

        use crate::scheduler::ReplayStacks;
        use crate::types::DetTid;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.json");
        let event = SchedEvent::branches(DetTid::from_raw(1), 4);
        write_events(&path, vec![event]);
        let config = |indices| {
            let mut cfg = prepared(&amd());
            cfg.replay_schedule_from = Some(path.clone());
            cfg.stacktrace_event = indices;
            cfg
        };
        let stack = Some(dir.path().join("stack"));
        let state = PmuState::new();
        let prepared =
            prepare_run_with(&config(vec![(1, stack.clone())]), &on(&amd(), &state)).unwrap();
        assert_eq!(state.pinned(), Some(amd().key()));
        assert_eq!(
            prepared.schedule.unwrap().stacks,
            ReplayStacks::Legacy(vec![(1, None, stack)])
        );
    }

    /// A callback-annotated priority history is admitted by identity first,
    /// then its checkpoints are checked against the launch decision, all
    /// before the pin: a legacy annotated history takes the identity
    /// refusal, a disagreement takes the mode refusal, and neither pins. An
    /// agreeing Known history pins and keeps its checkpoints on the admitted
    /// object the scheduler installs from.
    #[test]
    fn an_annotated_priority_history_is_admitted_before_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let annotated = dir.path().join("annotated.json");
        let legacy = dir.path().join("legacy-annotated.json");
        let plain = dir.path().join("plain.json");
        std::fs::write(&annotated, test_sources::annotated_history(Some(V1))).unwrap();
        std::fs::write(&legacy, test_sources::annotated_history(None)).unwrap();
        write(&plain, Some(V1));
        for (history, launch, refused) in [
            (
                Some(&legacy),
                true,
                Some("states no RCB coordinate identity"),
            ),
            (
                Some(&annotated),
                false,
                Some(
                    "HERMIT_CALLBACK_CHECKPOINT_UNQUALIFIED_MODE: the replayed preemption record \
                     carries callback checkpoints",
                ),
            ),
            (
                Some(&plain),
                true,
                Some(
                    "HERMIT_CALLBACK_CHECKPOINT_UNQUALIFIED_MODE: the replayed preemption record \
                     carries no callback checkpoints",
                ),
            ),
            (None, true, Some("names no --replay-preemptions-from")),
            (Some(&annotated), true, None),
        ] {
            let state = PmuState::new();
            let mut cfg = prepared(&amd());
            cfg.replay_preemptions_from = history.cloned();
            cfg.annotated_priority_replay = launch;
            let result = prepare_run_with(&cfg, &on(&amd(), &state));
            match refused {
                Some(cause) => {
                    let reason = refusal(result);
                    assert!(reason.contains(cause), "{reason}");
                    assert_eq!(state.pinned(), None);
                }
                None => {
                    let prepared = result.unwrap();
                    assert_eq!(state.pinned(), Some(amd().key()));
                    let history = prepared.history.unwrap();
                    assert_eq!(history.recording().identity(), Some(&V1));
                    let rows: usize = history.callback_checkpoints().values().map(Vec::len).sum();
                    assert_eq!(rows, 1);
                }
            }
        }
    }

    /// A derived history keeps its sources' identity only when they agree.
    #[test]
    fn a_combined_history_keeps_only_a_common_identity() {
        let unqualified = HistoryIdentity::Unqualified("claim-unset".to_owned());
        assert_eq!(
            combined_identity(("a", Some(&V1)), ("b", Some(&V1))),
            Ok(Some(V1))
        );
        assert_eq!(combined_identity(("a", None), ("b", None)), Ok(None));
        assert_eq!(
            combined_identity(("a", Some(&unqualified)), ("b", Some(&unqualified))),
            Ok(Some(unqualified.clone()))
        );
        for (a, b) in [
            (Some(&V1), None),
            (None, Some(&V1)),
            (Some(&V1), Some(&unqualified)),
        ] {
            let refused = combined_identity(("a", a), ("b", b)).unwrap_err();
            assert!(refused.contains("hermit refused to combine a"), "{refused}");
        }
    }
}
