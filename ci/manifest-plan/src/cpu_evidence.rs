//! Optional per-invocation CPU observations, independent of charged CPU and verdicts.
//!
//! The runner records actual launch, sampling and wait boundaries in this envelope.
//! A missing envelope is historical absence, never a measured zero.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Duration;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value;

use crate::ledger::CellIdentity;
use crate::ledger::RequiredNullable;

pub fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Elapsed {
    pub seconds: u64,
    pub nanoseconds: u32,
}

impl From<Duration> for Elapsed {
    fn from(value: Duration) -> Self {
        Self {
            seconds: value.as_secs(),
            nanoseconds: value.subsec_nanos(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CpuPoint {
    pub poll: u64,
    pub at: Elapsed,
    pub cpu_usec: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellCpuBinding {
    pub run_id: String,
    pub hermit_sha: String,
    pub lane: String,
    pub category: String,
    pub test: String,
    pub mode: String,
    pub backend: RequiredNullable<String>,
    pub outer_attempt: u64,
    pub run_index: RequiredNullable<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellCpuObservationsV1 {
    pub version: u64,
    pub binding: CellCpuBinding,
    pub invocations: Vec<InvocationCpuObservation>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvocationCpuObservation {
    pub ordinal: u64,
    pub role: InvocationRole,
    pub command: CommandIdentity,
    pub launch: LaunchObservation,
    pub live: LiveCpuObservation,
    pub final_wait: FinalWaitObservation,
    pub termination: TerminationPath,
    pub returned_cpu_charge: ReturnedCpuCharge,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvocationRole {
    Preparation,
    Execution {
        attempt_index: String,
        backend: RequiredNullable<String>,
    },
    PtraceNormalization {
        execution_ordinal: u64,
    },
    ParityComparison {
        candidate_execution: u64,
        reference_execution: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandIdentity {
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(deserialize_with = "deserialize_env")]
    pub env_overrides: BTreeMap<String, String>,
}

// A direct typed reader must not silently overwrite duplicate environment keys.
// The schema10 raw reader separately rejects duplicates before Value buffering.
fn deserialize_env<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("unique string environment overrides")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if values.insert(key.clone(), value).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate environment key {key}"
                    )));
                }
            }
            Ok(values)
        }
    }
    deserializer.deserialize_map(Visitor)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchObservation {
    NotStarted { reason: NotStartedReason },
    SpawnFailed { stage: SpawnStage, reason: String },
    Spawned { pid: u32 },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotStartedReason {
    CpuBudgetAlreadyExhausted,
    WallBudgetAlreadyExhausted,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpawnStage {
    StdoutCapture,
    StderrCapture,
    Spawn,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum LiveCpuObservation {
    Disabled,
    Enabled(Box<LiveCpuEnabled>),
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LiveCpuEnabled {
    pub source: LiveCpuSource,
    pub registration: RegistrationObservation,
    pub polls: u64,
    pub source_sample_calls: u64,
    pub valid_polls: u64,
    pub unavailable_polls: u64,
    pub first: RequiredNullable<CpuPoint>,
    pub last: RequiredNullable<CpuPoint>,
    pub high_water: RequiredNullable<CpuPoint>,
    pub timeout_trigger: RequiredNullable<CpuPoint>,
    pub last_error: RequiredNullable<CpuError>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveCpuSource {
    AgentUtilsPairedPidfdStatV1,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RegistrationObservation {
    NotAttempted,
    BoundOnce,
    Unavailable { reason: String },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CpuError {
    pub stage: CpuErrorStage,
    pub reason: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CpuErrorStage {
    Registration,
    Sampling,
    Conversion,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawTimeval {
    pub seconds: i64,
    pub microseconds: i64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitCpuObservation {
    Measured {
        user_usec: u64,
        system_usec: u64,
        total_usec: u64,
    },
    Invalid {
        user: RawTimeval,
        system: RawTimeval,
        reason: String,
    },
}

impl WaitCpuObservation {
    pub(crate) fn from_rusage(usage: &libc::rusage) -> Self {
        let user = RawTimeval {
            seconds: usage.ru_utime.tv_sec,
            microseconds: usage.ru_utime.tv_usec,
        };
        let system = RawTimeval {
            seconds: usage.ru_stime.tv_sec,
            microseconds: usage.ru_stime.tv_usec,
        };
        let measured = (|| {
            let user_usec = raw_usec(&user).ok_or("wait4 returned an invalid user CPU duration")?;
            let system_usec =
                raw_usec(&system).ok_or("wait4 returned an invalid system CPU duration")?;
            let total_usec = user_usec
                .checked_add(system_usec)
                .ok_or("wait4 CPU usage overflowed u64")?;
            Ok::<_, &str>((user_usec, system_usec, total_usec))
        })();
        match measured {
            Ok((user_usec, system_usec, total_usec)) => Self::Measured {
                user_usec,
                system_usec,
                total_usec,
            },
            Err(reason) => Self::Invalid {
                user,
                system,
                reason: reason.into(),
            },
        }
    }

    pub(crate) fn total(&self) -> Result<u64, String> {
        match self {
            Self::Measured { total_usec, .. } => Ok(*total_usec),
            Self::Invalid { reason, .. } => Err(reason.clone()),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum FinalWaitObservation {
    NotApplicable,
    Reaped {
        source: FinalCpuSource,
        pid: u32,
        raw_status: i32,
        at: Elapsed,
        cpu: WaitCpuObservation,
    },
    /// A wait helper receipt without a terminal status. Its CPU observation is
    /// not an exhaustive lifetime measurement or proof that the leader died.
    NonterminalReturn {
        source: FinalCpuSource,
        pid: u32,
        raw_status: i32,
        at: Elapsed,
        cpu: WaitCpuObservation,
    },
    Unavailable {
        operation: WaitOperation,
        errno: RequiredNullable<i32>,
        reason: String,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinalCpuSource {
    Wait4,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitOperation {
    Poll,
    StopGrace,
    BlockingStop,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminationPath {
    NotStarted,
    SpawnFailed,
    CompletedWait4,
    /// The helper returned without a timeout, but did not observe a reap.
    NonterminalWait4Return,
    /// The initial wait returned CPU >= the budget, before any stop branch.
    /// This may occur before the first live poll, even if registration failed.
    FinalWaitCpuBudgetReturn,
    CpuBudgetStop,
    WallBudgetStop,
    AccountingUnavailableStop,
    WaitError,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReturnedCpuCharge {
    Value { cpu_usec: u64, basis: ChargeBasis },
    Unavailable,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChargeBasis {
    FinalWait4,
    MaxTriggerAndFinalWait4,
}

fn require(condition: bool, reason: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(format!("invalid CPU observations: {reason}"))
    }
}
fn nonempty(value: &str) -> bool {
    !value.trim().is_empty()
}
fn nullable<T>(value: &RequiredNullable<T>) -> Option<&T> {
    match value {
        RequiredNullable::Null => None,
        RequiredNullable::Value(value) => Some(value),
    }
}
fn valid_elapsed(value: &Elapsed) -> Result<(), String> {
    require(
        value.nanoseconds < 1_000_000_000,
        "elapsed nanoseconds out of range",
    )
}
fn raw_usec(value: &RawTimeval) -> Option<u64> {
    let seconds = u64::try_from(value.seconds).ok()?;
    let microseconds = u64::try_from(value.microseconds).ok()?;
    (microseconds < 1_000_000).then_some(seconds.checked_mul(1_000_000)?.checked_add(microseconds)?)
}

impl LiveCpuEnabled {
    pub(crate) fn new() -> Self {
        Self {
            source: LiveCpuSource::AgentUtilsPairedPidfdStatV1,
            registration: RegistrationObservation::NotAttempted,
            polls: 0,
            source_sample_calls: 0,
            valid_polls: 0,
            unavailable_polls: 0,
            first: RequiredNullable::Null,
            last: RequiredNullable::Null,
            high_water: RequiredNullable::Null,
            timeout_trigger: RequiredNullable::Null,
            last_error: RequiredNullable::Null,
        }
    }

    /// Summarize an existing monitor poll. This does not call the CPU source.
    pub(crate) fn record_poll(
        &mut self,
        at: Duration,
        source_called: bool,
        observation: &Result<u64, CpuError>,
        triggered: bool,
    ) {
        self.polls += 1;
        self.source_sample_calls += u64::from(source_called);
        match observation {
            Ok(cpu_usec) => {
                self.valid_polls += 1;
                let point = CpuPoint {
                    poll: self.polls,
                    at: at.into(),
                    cpu_usec: *cpu_usec,
                };
                if matches!(self.first, RequiredNullable::Null) {
                    self.first = RequiredNullable::Value(point.clone());
                }
                if nullable(&self.high_water).is_none_or(|high| point.cpu_usec > high.cpu_usec) {
                    self.high_water = RequiredNullable::Value(point.clone());
                }
                self.last = RequiredNullable::Value(point.clone());
                if triggered {
                    self.timeout_trigger = RequiredNullable::Value(point);
                }
            }
            Err(error) => {
                self.unavailable_polls += 1;
                self.last_error = RequiredNullable::Value(error.clone());
            }
        }
    }

    fn validate(&self, launched: bool, final_at: Option<&Elapsed>) -> Result<(), String> {
        require(
            self.valid_polls.checked_add(self.unavailable_polls) == Some(self.polls),
            "poll counts differ",
        )?;
        require(
            self.source_sample_calls <= self.polls,
            "more reader calls than polls",
        )?;
        match &self.registration {
            RegistrationObservation::NotAttempted => require(
                !launched && self.polls == 0 && self.source_sample_calls == 0,
                "registration not attempted after launch",
            )?,
            RegistrationObservation::BoundOnce => require(
                launched && self.source_sample_calls == self.polls,
                "bound reader call count differs",
            )?,
            RegistrationObservation::Unavailable { reason } => {
                require(
                    launched
                        && nonempty(reason)
                        && self.source_sample_calls == 0
                        && self.valid_polls == 0,
                    "unavailable registration claims samples",
                )?;
                if let Some(error) = nullable(&self.last_error) {
                    require(
                        error.stage == CpuErrorStage::Registration,
                        "registration refusal has a different error stage",
                    )?;
                }
            }
        }
        require(
            nullable(&self.last_error).is_some() == (self.unavailable_polls > 0),
            "unavailable polls and error presence differ",
        )?;
        if let Some(error) = nullable(&self.last_error) {
            require(nonempty(&error.reason), "empty live error")?;
            if matches!(self.registration, RegistrationObservation::BoundOnce) {
                require(
                    error.stage != CpuErrorStage::Registration,
                    "bound reader claims registration error",
                )?;
            }
        }
        let (first, last, high) = (
            nullable(&self.first),
            nullable(&self.last),
            nullable(&self.high_water),
        );
        require(
            [first, last, high]
                .iter()
                .all(|point| point.is_some() == (self.valid_polls > 0)),
            "valid polls and point presence differ",
        )?;
        for point in [first, last, high, nullable(&self.timeout_trigger)]
            .into_iter()
            .flatten()
        {
            valid_elapsed(&point.at)?;
            require(
                point.poll > 0 && point.poll <= self.polls,
                "point poll is out of range",
            )?;
            if let Some(at) = final_at {
                require(point.at <= *at, "live point follows final wait")?;
            }
        }
        if let (Some(first), Some(last), Some(high)) = (first, last, high) {
            require(
                first.poll <= high.poll
                    && high.poll <= last.poll
                    && first.at <= high.at
                    && high.at <= last.at,
                "point order differs",
            )?;
            require(
                high.cpu_usec >= first.cpu_usec && high.cpu_usec >= last.cpu_usec,
                "high water is below an endpoint",
            )?;
            require(
                self.valid_polls <= last.poll - first.poll + 1,
                "valid poll count exceeds point interval",
            )?;
            for point in [last, high] {
                if point.poll == first.poll {
                    require(point == first, "same poll has different observations")?;
                }
                if point.poll == last.poll {
                    require(point == last, "same last poll has different observations")?;
                }
            }
            if self.valid_polls == 1 {
                require(
                    first == last && last == high,
                    "single valid poll differs across summaries",
                )?;
            }
        }
        if let Some(trigger) = nullable(&self.timeout_trigger) {
            require(
                Some(trigger) == last && trigger.poll == self.polls,
                "timeout trigger is not the final poll and last valid observation",
            )?;
        }
        Ok(())
    }
}

impl InvocationCpuObservation {
    pub(crate) fn pending(
        ordinal: u64,
        role: InvocationRole,
        command: CommandIdentity,
        cpu_enabled: bool,
    ) -> Self {
        Self {
            ordinal,
            role,
            command,
            launch: LaunchObservation::NotStarted {
                reason: NotStartedReason::WallBudgetAlreadyExhausted,
            },
            live: if cpu_enabled {
                LiveCpuObservation::Enabled(Box::new(LiveCpuEnabled::new()))
            } else {
                LiveCpuObservation::Disabled
            },
            final_wait: FinalWaitObservation::NotApplicable,
            termination: TerminationPath::NotStarted,
            returned_cpu_charge: ReturnedCpuCharge::Unavailable,
        }
    }

    fn returned_without_timeout(&self) -> bool {
        matches!(
            self.termination,
            TerminationPath::CompletedWait4 | TerminationPath::NonterminalWait4Return
        )
    }

    fn returned_timeout(&self) -> Option<bool> {
        match self.termination {
            TerminationPath::CompletedWait4 | TerminationPath::NonterminalWait4Return => {
                Some(false)
            }
            TerminationPath::NotStarted | TerminationPath::FinalWaitCpuBudgetReturn => Some(true),
            TerminationPath::CpuBudgetStop | TerminationPath::WallBudgetStop
                if matches!(self.returned_cpu_charge, ReturnedCpuCharge::Value { .. }) =>
            {
                Some(true)
            }
            // These error bridges do not return a semantic timeout flag.
            _ => None,
        }
    }

    fn completed_successfully(&self) -> bool {
        self.termination == TerminationPath::CompletedWait4
            && matches!(
                self.final_wait,
                FinalWaitObservation::Reaped { raw_status: 0, .. }
            )
    }

    fn validate(&self) -> Result<(), String> {
        require(
            self.ordinal > 0
                && self.command.argv.first().is_some_and(|p| nonempty(p))
                && nonempty(&self.command.cwd),
            "missing invocation identity/command",
        )?;
        let pid = match &self.launch {
            LaunchObservation::Spawned { pid } => {
                require(*pid > 0, "zero launched pid")?;
                Some(*pid)
            }
            LaunchObservation::NotStarted { reason } => {
                require(
                    self.termination == TerminationPath::NotStarted,
                    "not-started termination differs",
                )?;
                require(
                    *reason != NotStartedReason::CpuBudgetAlreadyExhausted
                        || matches!(self.live, LiveCpuObservation::Enabled(_)),
                    "prelaunch CPU exhaustion requires enabled CPU accounting",
                )?;
                None
            }
            LaunchObservation::SpawnFailed { reason, .. } => {
                require(
                    nonempty(reason) && self.termination == TerminationPath::SpawnFailed,
                    "spawn failure termination differs",
                )?;
                None
            }
        };
        let mut final_at = None;
        let final_cpu = match &self.final_wait {
            FinalWaitObservation::NotApplicable => {
                require(pid.is_none(), "launched child has no final disposition")?;
                None
            }
            FinalWaitObservation::Unavailable {
                operation, reason, ..
            } => {
                require(
                    pid.is_some() && nonempty(reason),
                    "unavailable wait without a child/reason",
                )?;
                require(
                    match operation {
                        WaitOperation::Poll => self.termination == TerminationPath::WaitError,
                        WaitOperation::StopGrace | WaitOperation::BlockingStop => matches!(
                            self.termination,
                            TerminationPath::CpuBudgetStop
                                | TerminationPath::WallBudgetStop
                                | TerminationPath::AccountingUnavailableStop
                        ),
                    },
                    "wait operation differs from initiating branch",
                )?;
                None
            }
            FinalWaitObservation::Reaped {
                pid: waited,
                raw_status,
                at,
                cpu,
                ..
            }
            | FinalWaitObservation::NonterminalReturn {
                pid: waited,
                raw_status,
                at,
                cpu,
                ..
            } => {
                require(pid == Some(*waited), "waited pid differs from launched pid")?;
                require(
                    (0..=u16::MAX as i32).contains(raw_status),
                    "raw wait status out of range",
                )?;
                require(
                    (libc::WIFEXITED(*raw_status) || libc::WIFSIGNALED(*raw_status))
                        == matches!(self.final_wait, FinalWaitObservation::Reaped { .. }),
                    "wait receipt terminal state differs from raw status",
                )?;
                valid_elapsed(at)?;
                final_at = Some(at);
                match cpu {
                    WaitCpuObservation::Measured {
                        user_usec,
                        system_usec,
                        total_usec,
                    } => {
                        require(
                            user_usec.checked_add(*system_usec) == Some(*total_usec),
                            "wait CPU sum differs or overflows",
                        )?;
                        Some(*total_usec)
                    }
                    WaitCpuObservation::Invalid {
                        user,
                        system,
                        reason,
                    } => {
                        require(
                            nonempty(reason)
                                && raw_usec(user)
                                    .and_then(|u| raw_usec(system).and_then(|s| u.checked_add(s)))
                                    .is_none(),
                            "invalid wait CPU is actually representable or lacks reason",
                        )?;
                        None
                    }
                }
            }
        };
        let trigger = match &self.live {
            LiveCpuObservation::Disabled => None,
            LiveCpuObservation::Enabled(live) => {
                live.validate(pid.is_some(), final_at)?;
                nullable(&live.timeout_trigger)
            }
        };
        require(
            trigger.is_some() == (self.termination == TerminationPath::CpuBudgetStop),
            "CPU stop and trigger presence differ",
        )?;
        if pid.is_none() {
            require(
                matches!(self.final_wait, FinalWaitObservation::NotApplicable)
                    && matches!(self.returned_cpu_charge, ReturnedCpuCharge::Unavailable),
                "unstarted child claims CPU",
            )?;
            return Ok(());
        }
        require(
            !matches!(
                self.termination,
                TerminationPath::NotStarted | TerminationPath::SpawnFailed
            ),
            "started child marked unstarted",
        )?;
        if self.termination == TerminationPath::AccountingUnavailableStop {
            // The first Err starts the monitor's nonzero grace at the same
            // instant used for its comparison, so stopping requires a later
            // consecutive Err. Counts cannot establish the grace duration.
            let trailing_unavailable = match &self.live {
                LiveCpuObservation::Enabled(live) => match nullable(&live.last) {
                    Some(last) => live.polls.checked_sub(last.poll),
                    None => Some(live.unavailable_polls),
                },
                LiveCpuObservation::Disabled => None,
            };
            require(
                trailing_unavailable.is_some_and(|polls| polls >= 2),
                "accounting stop lacks two trailing unavailable polls",
            )?;
        }
        match self.termination {
            TerminationPath::CompletedWait4 => require(
                matches!(self.final_wait, FinalWaitObservation::Reaped { .. }),
                "completed wait did not reap the leader",
            )?,
            TerminationPath::NonterminalWait4Return => require(
                matches!(
                    self.final_wait,
                    FinalWaitObservation::NonterminalReturn { .. }
                ),
                "nonterminal helper return carries a different wait receipt",
            )?,
            TerminationPath::FinalWaitCpuBudgetReturn => require(
                matches!(self.live, LiveCpuObservation::Enabled(_)),
                "final wait CPU budget return requires enabled CPU accounting",
            )?,
            _ => {}
        }
        let expected = match self.termination {
            TerminationPath::CompletedWait4
            | TerminationPath::NonterminalWait4Return
            | TerminationPath::FinalWaitCpuBudgetReturn => Some((
                final_cpu.ok_or("completed wait has no valid CPU receipt")?,
                ChargeBasis::FinalWait4,
            )),
            TerminationPath::WallBudgetStop => final_cpu.map(|cpu| (cpu, ChargeBasis::FinalWait4)),
            TerminationPath::CpuBudgetStop => final_cpu.map(|cpu| {
                (
                    cpu.max(trigger.expect("validated trigger").cpu_usec),
                    ChargeBasis::MaxTriggerAndFinalWait4,
                )
            }),
            TerminationPath::WaitError => {
                require(final_cpu.is_none(), "wait error carries a valid receipt")?;
                None
            }
            TerminationPath::AccountingUnavailableStop
            | TerminationPath::NotStarted
            | TerminationPath::SpawnFailed => None,
        };
        require(
            match (&self.returned_cpu_charge, expected) {
                (ReturnedCpuCharge::Unavailable, None) => true,
                (ReturnedCpuCharge::Value { cpu_usec, basis }, Some((cpu, source))) => {
                    *cpu_usec == cpu && *basis == source
                }
                _ => false,
            },
            "returned charge differs from its actual branch operands",
        )
    }
}

impl CellCpuBinding {
    pub fn from_source_row(row: &Value) -> Result<Self, String> {
        let string = |name: &str| {
            row.get(name)
                .and_then(Value::as_str)
                .filter(|s| nonempty(s))
                .map(str::to_owned)
                .ok_or_else(|| format!("CPU source row lacks {name}"))
        };
        let backend = match row.get("backend") {
            Some(Value::Null) => RequiredNullable::Null,
            Some(Value::String(value)) if nonempty(value) => RequiredNullable::Value(value.clone()),
            _ => return Err("CPU source row lacks a valid backend".into()),
        };
        let run_index = match row.get("run_index") {
            None | Some(Value::Null) => RequiredNullable::Null,
            Some(value) => RequiredNullable::Value(
                value
                    .as_u64()
                    .ok_or("CPU source run_index is not an exact integer")?,
            ),
        };
        Ok(Self {
            run_id: string("run_id")?,
            hermit_sha: string("hermit_sha")?,
            lane: string("lane")?,
            category: string("category")?,
            test: string("test")?,
            mode: string("mode")?,
            backend,
            outer_attempt: row
                .get("attempt")
                .and_then(Value::as_u64)
                .ok_or("CPU source row lacks attempt")?,
            run_index,
        })
    }
}

impl CellCpuObservationsV1 {
    pub fn validate(&self) -> Result<(), String> {
        require(self.version == 1, "unknown observation version")?;
        let b = &self.binding;
        require(
            [
                &b.run_id,
                &b.hermit_sha,
                &b.lane,
                &b.category,
                &b.test,
                &b.mode,
            ]
            .iter()
            .all(|s| nonempty(s)),
            "empty cell binding",
        )?;
        require(
            b.outer_attempt > 0 && b.outer_attempt <= crate::runner::MAX_ATTEMPTS_PER_CELL,
            "outer attempt out of range",
        )?;
        if let Some(backend) = nullable(&b.backend) {
            require(nonempty(backend), "empty backend")?;
        }
        let mut executions = BTreeSet::new();
        let mut preparation = false;
        let mut normalizations = BTreeSet::new();
        let mut comparison = false;
        let mut verify_candidate: Option<&InvocationCpuObservation> = None;
        for (index, invocation) in self.invocations.iter().enumerate() {
            require(
                u64::try_from(index).ok().and_then(|n| n.checked_add(1))
                    == Some(invocation.ordinal),
                "invocation ordinals are not contiguous",
            )?;
            invocation.validate()?;
            if let Some(previous) = index.checked_sub(1).map(|i| &self.invocations[i]) {
                require(
                    previous.returned_without_timeout()
                        && !matches!(previous.role, InvocationRole::ParityComparison { .. })
                        && (!matches!(previous.role, InvocationRole::Preparation)
                            || previous.completed_successfully()),
                    "invocation follows a stopped, failed preparation, or final comparison",
                )?;
            }
            let execution = |ordinal: u64| -> Result<&InvocationCpuObservation, String> {
                let item = usize::try_from(ordinal)
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .filter(|n| *n < index)
                    .and_then(|n| self.invocations.get(n))
                    .ok_or("CPU role reference is not an earlier invocation")?;
                require(
                    matches!(item.role, InvocationRole::Execution { .. }),
                    "CPU role does not reference execution",
                )?;
                Ok(item)
            };
            match &invocation.role {
                InvocationRole::Preparation => {
                    require(!preparation && index == 0, "repeated or late preparation")?;
                    preparation = true;
                }
                InvocationRole::Execution {
                    attempt_index,
                    backend,
                } => {
                    require(nonempty(attempt_index), "empty execution index")?;
                    let backend = nullable(backend).cloned();
                    require(
                        executions.insert((attempt_index.clone(), backend.clone())),
                        "repeated execution identity",
                    )?;
                    let expected = if attempt_index == "parity-reference" {
                        Some("ptrace")
                    } else {
                        nullable(&b.backend).map(String::as_str)
                    };
                    require(
                        backend.as_deref() == expected,
                        "execution backend differs from its role",
                    )?;
                    if attempt_index == "parity-reference" {
                        require(
                            b.mode == "verify"
                                && nullable(&b.backend).is_some_and(|backend| backend != "ptrace")
                                && verify_candidate
                                    .is_some_and(|prior| prior.completed_successfully()),
                            "parity reference lacks an earlier successful verify candidate",
                        )?;
                    } else if b.mode == "verify" {
                        require(verify_candidate.is_none(), "repeated verify candidate")?;
                        verify_candidate = Some(invocation);
                    }
                }
                InvocationRole::PtraceNormalization { execution_ordinal } => {
                    let prior = execution(*execution_ordinal)?;
                    require(
                        b.mode == "verify"
                            && prior.returned_without_timeout()
                            && invocation.ordinal.checked_sub(1) == Some(*execution_ordinal)
                            && matches!(&prior.role, InvocationRole::Execution { backend, .. }
                                if nullable(backend).map(String::as_str) == Some("ptrace")),
                        "normalization does not reference a verify ptrace return without timeout",
                    )?;
                    require(
                        normalizations.insert(*execution_ordinal),
                        "repeated normalization",
                    )?;
                }
                InvocationRole::ParityComparison {
                    candidate_execution,
                    reference_execution,
                } => {
                    require(
                        !comparison && candidate_execution != reference_execution,
                        "repeated or self comparison",
                    )?;
                    let candidate = execution(*candidate_execution)?;
                    let reference = execution(*reference_execution)?;
                    require(
                        b.mode == "verify"
                            && nullable(&b.backend).is_some_and(|backend| backend != "ptrace"),
                        "comparison is not a verify non-ptrace candidate",
                    )?;
                    require(
                        matches!(&candidate.role, InvocationRole::Execution{attempt_index,..} if attempt_index != "parity-reference")
                            && matches!(&reference.role, InvocationRole::Execution{attempt_index,..} if attempt_index == "parity-reference"),
                        "comparison roles are reversed or foreign",
                    )?;
                    require(
                        candidate.completed_successfully() && reference.completed_successfully(),
                        "comparison operand did not complete with exit zero",
                    )?;
                    comparison = true;
                }
            }
        }
        Ok(())
    }

    pub fn validate_binding(&self, binding: &CellCpuBinding) -> Result<(), String> {
        self.validate()?;
        require(
            self.binding == *binding,
            "source row and observation binding differ",
        )
    }

    pub fn validate_attempt(
        &self,
        index: &str,
        argv: &[String],
        cwd: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        let invocation=self.invocations.iter().find(|item|matches!(&item.role,InvocationRole::Execution{attempt_index,..} if attempt_index == index)).ok_or("CPU observations omit a retained semantic attempt")?;
        require(
            invocation.command.argv == argv
                && invocation.command.cwd == cwd
                && invocation.command.env_overrides == *env,
            "CPU command differs from retained semantic attempt",
        )
    }

    /// Bind available semantic timeout flags and the roles requiring semantic
    /// PASS. Failure records alone do not acquire a PASS requirement. Missing
    /// flags remain unknown, and cannot establish a no-timeout predecessor.
    pub fn require_passing_prerequisites<'a>(
        &self,
        attempts: impl IntoIterator<Item = (&'a str, bool, Option<bool>)>,
    ) -> Result<(), String> {
        self.validate()?;
        let mut passed = BTreeSet::new();
        let mut seen = BTreeMap::new();
        for (index, is_pass, timed_out) in attempts {
            require(
                seen.insert(index, timed_out).is_none(),
                "repeated retained semantic attempt",
            )?;
            let invocation = self.invocations.iter().find(|item| {
                matches!(&item.role, InvocationRole::Execution { attempt_index, .. } if attempt_index == index)
            }).ok_or("CPU observations omit a retained semantic attempt")?;
            if let (Some(actual), Some(expected)) = (timed_out, invocation.returned_timeout()) {
                require(
                    actual == expected,
                    "retained timed_out differs from CPU return branch",
                )?;
            }
            if is_pass {
                passed.insert(index);
            }
        }
        let mut required = BTreeSet::new();
        for invocation in &self.invocations {
            match &invocation.role {
                InvocationRole::Execution { attempt_index, .. }
                    if attempt_index == "parity-reference" =>
                {
                    let candidate = self
                        .invocations
                        .iter()
                        .find_map(|item| match &item.role {
                            InvocationRole::Execution { attempt_index, .. }
                                if attempt_index != "parity-reference" =>
                            {
                                Some(attempt_index.as_str())
                            }
                            _ => None,
                        })
                        .ok_or("reference has no candidate semantic identity")?;
                    required.insert(candidate);
                }
                InvocationRole::ParityComparison {
                    candidate_execution,
                    reference_execution,
                } => {
                    for ordinal in [candidate_execution, reference_execution] {
                        let operand = self
                            .invocations
                            .iter()
                            .find(|item| item.ordinal == *ordinal)
                            .ok_or("comparison has no operand semantic identity")?;
                        let InvocationRole::Execution { attempt_index, .. } = &operand.role else {
                            return Err("comparison semantic prerequisite is not execution".into());
                        };
                        required.insert(attempt_index.as_str());
                    }
                }
                _ => {}
            }
        }
        require(
            required.is_subset(&passed),
            "CPU role lacks a retained passing semantic prerequisite",
        )?;
        for invocation in self
            .invocations
            .iter()
            .take(self.invocations.len().saturating_sub(1))
        {
            if let InvocationRole::Execution { attempt_index, .. } = &invocation.role {
                require(
                    seen.get(attempt_index.as_str()) == Some(&Some(false)),
                    "execution successor lacks a retained no-timeout prerequisite",
                )?;
            }
        }
        Ok(())
    }
}

pub fn validate_cpu_observations_in_source_row(
    row: &Value,
) -> Result<Option<CellCpuObservationsV1>, String> {
    let Some(raw) = row.get("cpu_observations") else {
        return Ok(None);
    };
    let observations: CellCpuObservationsV1 = serde_json::from_value(raw.clone())
        .map_err(|e| format!("malformed CPU observations: {e}"))?;
    observations.validate_binding(&CellCpuBinding::from_source_row(row)?)?;
    let mut prerequisites = Vec::new();
    for attempt in row
        .get("attempts")
        .and_then(Value::as_array)
        .ok_or("CPU source attempts is absent or not an array")?
    {
        let index = attempt
            .get("index")
            .and_then(Value::as_str)
            .ok_or("CPU source attempt lacks index")?;
        let argv: Vec<String> = serde_json::from_value(
            attempt
                .get("argv")
                .cloned()
                .ok_or("CPU source attempt lacks argv")?,
        )
        .map_err(|e| e.to_string())?;
        let cwd = attempt
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or("CPU source attempt lacks cwd")?;
        let env: BTreeMap<String, String> = serde_json::from_value(
            attempt
                .get("env")
                .cloned()
                .ok_or("CPU source attempt lacks env")?,
        )
        .map_err(|e| e.to_string())?;
        observations.validate_attempt(index, &argv, cwd, &env)?;
        let timed_out = attempt
            .get("timed_out")
            .map(|value| {
                value
                    .as_bool()
                    .ok_or("CPU source attempt timed_out is not a boolean")
            })
            .transpose()?;
        prerequisites.push((
            index,
            attempt.get("outcome").and_then(Value::as_str) == Some("PASS")
                && attempt.get("status").and_then(Value::as_i64) == Some(0)
                && matches!(attempt.get("signal"), Some(Value::Null))
                && timed_out == Some(false),
            timed_out,
        ));
    }
    observations.require_passing_prerequisites(prerequisites)?;
    Ok(Some(observations))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellCpuHistoryV1 {
    pub version: u64,
    pub attempts: Vec<CpuAttemptHistory>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CpuAttemptHistory {
    Unrecorded {
        outer_attempt: u64,
    },
    Recorded {
        outer_attempt: u64,
        observations: Box<CellCpuObservationsV1>,
    },
}
impl CpuAttemptHistory {
    pub fn outer_attempt(&self) -> u64 {
        match self {
            Self::Unrecorded { outer_attempt } | Self::Recorded { outer_attempt, .. } => {
                *outer_attempt
            }
        }
    }
    pub fn observations(&self) -> Option<&CellCpuObservationsV1> {
        match self {
            Self::Unrecorded { .. } => None,
            Self::Recorded { observations, .. } => Some(observations),
        }
    }
}
impl CellCpuHistoryV1 {
    pub fn from_source_rows(rows: &[(u64, Value)]) -> Result<Option<Self>, String> {
        let mut attempts = Vec::new();
        let mut recorded = false;
        for (outer_attempt, row) in rows {
            if let Some(observations) = validate_cpu_observations_in_source_row(row)? {
                require(
                    observations.binding.outer_attempt == *outer_attempt,
                    "projected outer attempt differs",
                )?;
                attempts.push(CpuAttemptHistory::Recorded {
                    outer_attempt: *outer_attempt,
                    observations: Box::new(observations),
                });
                recorded = true;
            } else {
                attempts.push(CpuAttemptHistory::Unrecorded {
                    outer_attempt: *outer_attempt,
                });
            }
        }
        Ok(recorded.then_some(Self {
            version: 1,
            attempts,
        }))
    }
    pub fn validate_for_artifact(
        &self,
        run_id: &str,
        hermit_sha: &str,
        identity: &CellIdentity,
        selected_attempt: u64,
    ) -> Result<(), String> {
        require(self.version == 1, "unknown history version")?;
        require(
            !self.attempts.is_empty()
                && self.attempts.len() as u64 <= crate::runner::MAX_ATTEMPTS_PER_CELL,
            "history attempt population out of range",
        )?;
        require(
            selected_attempt > 0 && selected_attempt <= self.attempts.len() as u64,
            "selected attempt absent from CPU history",
        )?;
        let mut recorded = false;
        for (index, attempt) in self.attempts.iter().enumerate() {
            let ordinal = index as u64 + 1;
            require(
                attempt.outer_attempt() == ordinal,
                "history outer attempts are not contiguous",
            )?;
            if let Some(observations) = attempt.observations() {
                observations.validate()?;
                let b = &observations.binding;
                require(
                    b.outer_attempt == ordinal
                        && b.run_id == run_id
                        && b.hermit_sha == hermit_sha
                        && b.lane == identity.lane
                        && b.category == identity.category
                        && b.test == identity.test
                        && b.mode == identity.mode
                        && nullable(&b.backend) == Some(&identity.backend),
                    "artifact and CPU history binding differ",
                )?;
                recorded = true;
            }
        }
        require(recorded, "all-unrecorded history must be absent")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    /// Synthetic decoder input, not a native CPU measurement.
    pub(crate) fn source_row() -> Value {
        json!({"run_id":"run","hermit_sha":"a".repeat(40),"lane":"portable",
            "category":"fixture","test":"fixture/test","mode":"verify","backend":"ptrace","attempt":1,"attempts":[]})
    }

    /// Bind synthetic execution records to the fixture's actual semantic operands.
    pub(crate) fn envelope(row: &Value) -> Value {
        let mut invocations = Vec::new();
        if let Some(attempts) = row.get("attempts").and_then(Value::as_array) {
            for (i, attempt) in attempts.iter().enumerate() {
                invocations.push(json!({
                    "ordinal":i+1,"role":{"kind":"execution","attempt_index":attempt["index"],"backend":if attempt["index"]=="parity-reference" {json!("ptrace")} else {row["backend"].clone()}},
                    "command":{"argv":attempt["argv"],"cwd":attempt["cwd"],"env_overrides":attempt["env"]},
                    "launch":{"state":"spawned","pid":17},"live":{"state":"disabled"},
                    "final_wait":{"state":"reaped","source":"wait4","pid":17,"raw_status":0,"at":{"seconds":1,"nanoseconds":0},"cpu":{"state":"measured","user_usec":3,"system_usec":2,"total_usec":5}},
                    "termination":"completed_wait4","returned_cpu_charge":{"state":"value","cpu_usec":5,"basis":"final_wait4"}
                }));
            }
        }
        json!({"version":1,"binding":{"run_id":row["run_id"],"hermit_sha":row["hermit_sha"],"lane":row["lane"],"category":row["category"],"test":row["test"],"mode":row["mode"],"backend":row["backend"],"outer_attempt":row["attempt"],"run_index":row.get("run_index").cloned().unwrap_or(Value::Null)},"invocations":invocations})
    }

    fn executed_row() -> Value {
        let mut row = source_row();
        row["attempts"] =
            json!([{"index":"verify-1","argv":["native-fixture"],"cwd":"/fixture","env":{}}]);
        row["cpu_observations"] = envelope(&row);
        row
    }
    fn valid(row: &Value) -> bool {
        validate_cpu_observations_in_source_row(row).is_ok()
    }
    fn enabled() -> Value {
        json!({"state":"enabled","source":"agent_utils_paired_pidfd_stat_v1","registration":{"state":"bound_once"},"polls":2,"source_sample_calls":2,"valid_polls":2,"unavailable_polls":0,
            "first":{"poll":1,"at":{"seconds":0,"nanoseconds":100},"cpu_usec":9},
            "last":{"poll":2,"at":{"seconds":0,"nanoseconds":200},"cpu_usec":7},
            "high_water":{"poll":1,"at":{"seconds":0,"nanoseconds":100},"cpu_usec":9},"timeout_trigger":null,"last_error":null})
    }

    fn refused_before_poll() -> Value {
        json!({"state":"enabled","source":"agent_utils_paired_pidfd_stat_v1","registration":{"state":"unavailable","reason":"fixture refusal"},"polls":0,"source_sample_calls":0,"valid_polls":0,"unavailable_polls":0,"first":null,"last":null,"high_water":null,"timeout_trigger":null,"last_error":null})
    }

    #[test]
    fn historical_absence_and_strict_present_source_binding() {
        assert!(valid(&source_row()));
        let row = executed_row();
        assert!(valid(&row));
        for field in [
            "run_id",
            "hermit_sha",
            "lane",
            "category",
            "test",
            "mode",
            "backend",
        ] {
            let mut bad = row.clone();
            bad["cpu_observations"]["binding"][field] = json!("foreign");
            assert!(!valid(&bad), "{field}");
        }
        for (pointer, value) in [
            ("/cpu_observations", Value::Null),
            ("/cpu_observations/version", json!(2)),
            ("/cpu_observations/version", json!(1.0)),
            ("/cpu_observations/binding/outer_attempt", json!(2)),
            ("/cpu_observations/binding/run_index", json!(0)),
            ("/cpu_observations/invocations/0/ordinal", json!(2)),
            (
                "/cpu_observations/invocations/0/command/argv",
                json!(["foreign"]),
            ),
        ] {
            let mut bad = row.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            assert!(!valid(&bad), "{pointer}");
        }
        let mut bad = row.clone();
        bad["cpu_observations"]["extra"] = json!(true);
        assert!(!valid(&bad));
        let mut missing = row.clone();
        missing["cpu_observations"]["binding"]
            .as_object_mut()
            .unwrap()
            .remove("run_index");
        assert!(!valid(&missing));
        let mut missing_attempts = row.clone();
        missing_attempts.as_object_mut().unwrap().remove("attempts");
        assert!(!valid(&missing_attempts));
        missing_attempts
            .as_object_mut()
            .unwrap()
            .remove("cpu_observations");
        assert!(valid(&missing_attempts)); // historical absence adds no source requirements
        let raw = serde_json::to_string(&row).unwrap().replace(
            "\"env_overrides\":{}",
            "\"env_overrides\":{\"A\":\"1\",\"A\":\"2\"}",
        );
        assert!(crate::ledger::read_schema10_source_result(raw.as_bytes()).is_err());
        let raw = serde_json::to_string(&row["cpu_observations"])
            .unwrap()
            .replace(
                "\"env_overrides\":{}",
                "\"env_overrides\":{\"A\":\"1\",\"A\":\"2\"}",
            );
        assert!(serde_json::from_str::<CellCpuObservationsV1>(&raw).is_err());
    }

    #[test]
    fn branch_charges_preserve_independent_live_and_final_values() {
        let mut row = executed_row();
        row["cpu_observations"]["invocations"][0]["live"] = enabled();
        assert!(valid(&row)); // completed wait charges final5, not live high-water9
        let mut wall = row.clone();
        wall["cpu_observations"]["invocations"][0]["termination"] = json!("wall_budget_stop");
        assert!(valid(&wall));
        for mut base in [row.clone(), wall] {
            base["cpu_observations"]["invocations"][0]["returned_cpu_charge"]["cpu_usec"] =
                json!(9);
            assert!(!valid(&base));
        }
        let i = &mut row["cpu_observations"]["invocations"][0];
        i["termination"] = json!("cpu_budget_stop");
        i["live"]["timeout_trigger"] = i["live"]["last"].clone();
        i["returned_cpu_charge"] =
            json!({"state":"value","cpu_usec":7,"basis":"max_trigger_and_final_wait4"});
        assert!(valid(&row));
        let mut later_unavailable = row.clone();
        let live = &mut later_unavailable["cpu_observations"]["invocations"][0]["live"];
        live["polls"] = json!(3);
        live["source_sample_calls"] = json!(3);
        live["unavailable_polls"] = json!(1);
        live["last_error"] = json!({"stage":"sampling","reason":"later unavailable poll"});
        assert!(!valid(&later_unavailable));
        row["cpu_observations"]["invocations"][0]["returned_cpu_charge"]["cpu_usec"] = json!(9);
        assert!(!valid(&row));
    }

    #[test]
    fn unavailable_zero_and_known_reap_are_distinct() {
        for status in [0, 256, libc::SIGTERM, libc::SIGABRT | 128] {
            let mut terminal = executed_row();
            terminal["cpu_observations"]["invocations"][0]["final_wait"]["raw_status"] =
                json!(status);
            assert!(valid(&terminal), "terminal status {status}");
        }
        for status in [127, (libc::SIGSTOP << 8) | 127, 65535, -1, 65536] {
            let mut nonterminal = executed_row();
            nonterminal["cpu_observations"]["invocations"][0]["final_wait"]["raw_status"] =
                json!(status);
            assert!(!valid(&nonterminal), "nonterminal status {status}");
        }
        let mut row = executed_row();
        let i = &mut row["cpu_observations"]["invocations"][0];
        i["live"] = json!({"state":"enabled","source":"agent_utils_paired_pidfd_stat_v1","registration":{"state":"unavailable","reason":"fixture refusal"},"polls":1,"source_sample_calls":0,"valid_polls":0,"unavailable_polls":1,"first":null,"last":null,"high_water":null,"timeout_trigger":null,"last_error":{"stage":"registration","reason":"fixture refusal"}});
        i["termination"] = json!("accounting_unavailable_stop");
        i["returned_cpu_charge"] = json!({"state":"unavailable"});
        assert!(
            !valid(&row),
            "first registration error only starts the grace"
        );
        for termination in ["completed_wait4", "wall_budget_stop"] {
            let mut partial = row.clone();
            partial["cpu_observations"]["invocations"][0]["termination"] = json!(termination);
            partial["cpu_observations"]["invocations"][0]["returned_cpu_charge"] =
                json!({"state":"value","cpu_usec":5,"basis":"final_wait4"});
            assert!(valid(&partial), "one unavailable poll before {termination}");
        }
        row["cpu_observations"]["invocations"][0]["live"]["polls"] = json!(2);
        row["cpu_observations"]["invocations"][0]["live"]["unavailable_polls"] = json!(2);
        assert!(valid(&row));
        // A real-shaped final receipt must survive the unavailable return charge.
        let mut bad = row.clone();
        bad["cpu_observations"]["invocations"][0]["returned_cpu_charge"] =
            json!({"state":"value","cpu_usec":5,"basis":"final_wait4"});
        assert!(!valid(&bad));
        let mut fast = executed_row();
        let i = &mut fast["cpu_observations"]["invocations"][0];
        i["live"] = row["cpu_observations"]["invocations"][0]["live"].clone();
        i["live"]["polls"] = json!(0);
        i["live"]["unavailable_polls"] = json!(0);
        i["live"]["last_error"] = Value::Null;
        assert!(valid(&fast));
        let mut zero = executed_row();
        let i = &mut zero["cpu_observations"]["invocations"][0];
        i["final_wait"]["cpu"] =
            json!({"state":"measured","user_usec":0,"system_usec":0,"total_usec":0});
        i["returned_cpu_charge"]["cpu_usec"] = json!(0);
        assert!(valid(&zero));
        let mut invalid = executed_row();
        let i = &mut invalid["cpu_observations"]["invocations"][0];
        i["termination"] = json!("wait_error");
        i["returned_cpu_charge"] = json!({"state":"unavailable"});
        i["final_wait"]["cpu"] = json!({"state":"invalid","user":{"seconds":-1,"microseconds":0},"system":{"seconds":0,"microseconds":0},"reason":"negative CPU"});
        assert!(valid(&invalid));
        invalid["cpu_observations"]["invocations"][0]["final_wait"]["cpu"]["user"]["seconds"] =
            json!(0);
        assert!(!valid(&invalid));

        // The Invalid variant must retain exactly the conversions refused by
        // the real wait4 helper. Raw tv_usec is bounded; converted CPU totals
        // are not limited to a single second.
        for (state, status) in [
            ("reaped", 0),
            ("nonterminal_return", (libc::SIGSTOP << 8) | 127),
        ] {
            for component in ["user", "system"] {
                for microseconds in [999_999, 1_000_000] {
                    let mut record = executed_row();
                    let i = &mut record["cpu_observations"]["invocations"][0];
                    i["termination"] = json!("wait_error");
                    i["returned_cpu_charge"] = json!({"state":"unavailable"});
                    i["final_wait"]["state"] = json!(state);
                    i["final_wait"]["raw_status"] = json!(status);
                    i["final_wait"]["cpu"] = json!({"state":"invalid","user":{"seconds":0,"microseconds":0},"system":{"seconds":0,"microseconds":0},"reason":"invalid raw timeval"});
                    i["final_wait"]["cpu"][component]["microseconds"] = json!(microseconds);
                    assert_eq!(
                        valid(&record),
                        microseconds == 1_000_000,
                        "raw timeval {state}/{component}/{microseconds}"
                    );
                }
            }
        }
        let mut measured_second = executed_row();
        measured_second["cpu_observations"]["invocations"][0]["final_wait"]["cpu"] = json!({"state":"measured","user_usec":1_000_000,"system_usec":999_999,"total_usec":1_999_999});
        measured_second["cpu_observations"]["invocations"][0]["returned_cpu_charge"]["cpu_usec"] =
            json!(1_999_999);
        assert!(valid(&measured_second));

        // A prior error cannot authorize a grace stop after a newer valid poll:
        // the monitor clears missing_since on Ok and samples no more after stop.
        let mut latest_valid = executed_row();
        let i = &mut latest_valid["cpu_observations"]["invocations"][0];
        i["termination"] = json!("accounting_unavailable_stop");
        i["returned_cpu_charge"] = json!({"state":"unavailable"});
        i["live"] = enabled();
        let point = i["live"]["last"].clone();
        i["live"]["first"] = point.clone();
        i["live"]["high_water"] = point;
        i["live"]["valid_polls"] = json!(1);
        i["live"]["unavailable_polls"] = json!(1);
        i["live"]["last_error"] = json!({"stage":"sampling","reason":"earlier unavailable poll"});
        assert!(!valid(&latest_valid));
        for termination in ["completed_wait4", "wall_budget_stop"] {
            let mut completed = latest_valid.clone();
            completed["cpu_observations"]["invocations"][0]["termination"] = json!(termination);
            completed["cpu_observations"]["invocations"][0]["returned_cpu_charge"] =
                json!({"state":"value","cpu_usec":5,"basis":"final_wait4"});
            assert!(
                valid(&completed),
                "mixed history with latest valid: {termination}"
            );
        }
        let mut latest_unavailable = latest_valid.clone();
        let live = &mut latest_unavailable["cpu_observations"]["invocations"][0]["live"];
        for field in ["first", "last", "high_water"] {
            live[field]["poll"] = json!(1);
        }
        live["last_error"]["reason"] = json!("latest unavailable poll");
        assert!(
            !valid(&latest_unavailable),
            "one trailing error starts grace"
        );
        for termination in ["completed_wait4", "wall_budget_stop"] {
            let mut partial = latest_unavailable.clone();
            partial["cpu_observations"]["invocations"][0]["termination"] = json!(termination);
            partial["cpu_observations"]["invocations"][0]["returned_cpu_charge"] =
                json!({"state":"value","cpu_usec":5,"basis":"final_wait4"});
            assert!(valid(&partial), "one trailing error before {termination}");
        }
        let mut two_trailing_unavailable = latest_unavailable.clone();
        let live = &mut two_trailing_unavailable["cpu_observations"]["invocations"][0]["live"];
        live["polls"] = json!(3);
        live["source_sample_calls"] = json!(3);
        live["unavailable_polls"] = json!(2);
        assert!(valid(&two_trailing_unavailable));
        let mut separated_errors = two_trailing_unavailable.clone();
        let live = &mut separated_errors["cpu_observations"]["invocations"][0]["live"];
        for field in ["first", "last", "high_water"] {
            live[field]["poll"] = json!(2);
        }
        assert!(!valid(&separated_errors), "a valid poll restarts the grace");
        let mut all_unavailable = latest_unavailable.clone();
        let live = &mut all_unavailable["cpu_observations"]["invocations"][0]["live"];
        live["valid_polls"] = json!(0);
        live["unavailable_polls"] = json!(2);
        for field in ["first", "last", "high_water"] {
            live[field] = Value::Null;
        }
        assert!(valid(&all_unavailable));
        let mut one_unavailable = all_unavailable.clone();
        let live = &mut one_unavailable["cpu_observations"]["invocations"][0]["live"];
        live["polls"] = json!(1);
        live["source_sample_calls"] = json!(1);
        live["unavailable_polls"] = json!(1);
        assert!(
            !valid(&one_unavailable),
            "first sampling error starts grace"
        );

        // Complete helper-return/error matrix. These are decoder fixtures, not
        // evidence that a traced child returned a nonterminal status here.
        // In particular, a nonterminal receipt never establishes final lifetime
        // CPU, reaping, or process-group quiescence.
        for (state, status, completed) in [
            ("reaped", 0, "completed_wait4"),
            (
                "nonterminal_return",
                (libc::SIGSTOP << 8) | 127,
                "nonterminal_wait4_return",
            ),
            ("nonterminal_return", 65535, "nonterminal_wait4_return"),
        ] {
            for termination in [
                completed,
                "final_wait_cpu_budget_return",
                "wall_budget_stop",
                "cpu_budget_stop",
                "accounting_unavailable_stop",
                "wait_error",
            ] {
                for invalid_cpu in [false, true] {
                    let mut record = executed_row();
                    let i = &mut record["cpu_observations"]["invocations"][0];
                    i["final_wait"]["state"] = json!(state);
                    i["final_wait"]["raw_status"] = json!(status);
                    i["termination"] = json!(termination);
                    match termination {
                        "final_wait_cpu_budget_return" => i["live"] = refused_before_poll(),
                        "cpu_budget_stop" => {
                            i["live"] = enabled();
                            i["live"]["timeout_trigger"] = i["live"]["last"].clone();
                            i["returned_cpu_charge"] = json!({"state":"value","cpu_usec":7,"basis":"max_trigger_and_final_wait4"});
                        }
                        "accounting_unavailable_stop" => {
                            i["live"] = row["cpu_observations"]["invocations"][0]["live"].clone();
                            i["returned_cpu_charge"] = json!({"state":"unavailable"});
                        }
                        "wait_error" => i["returned_cpu_charge"] = json!({"state":"unavailable"}),
                        _ => {}
                    }
                    if invalid_cpu {
                        i["final_wait"]["cpu"] = json!({"state":"invalid","user":{"seconds":i64::MAX,"microseconds":0},"system":{"seconds":0,"microseconds":0},"reason":"overflowing CPU"});
                        i["returned_cpu_charge"] = json!({"state":"unavailable"});
                    }
                    let expected = if termination == "wait_error" {
                        invalid_cpu
                    } else {
                        !(invalid_cpu
                            && [completed, "final_wait_cpu_budget_return"].contains(&termination))
                    };
                    assert_eq!(
                        valid(&record),
                        expected,
                        "{state}/{status}/{termination}/invalid={invalid_cpu}"
                    );
                    if !expected {
                        continue;
                    }
                    if !invalid_cpu && termination != "accounting_unavailable_stop" {
                        let timeout = termination != completed;
                        record["attempts"][0]["timed_out"] = json!(timeout);
                        assert!(valid(&record), "matching flag {termination}");
                        record["attempts"][0]["timed_out"] = json!(!timeout);
                        assert!(!valid(&record), "contradictory flag {termination}");
                    }
                }
            }
            let mut crossed = executed_row();
            crossed["cpu_observations"]["invocations"][0]["final_wait"]["state"] = json!(state);
            crossed["cpu_observations"]["invocations"][0]["final_wait"]["raw_status"] =
                json!(status);
            crossed["cpu_observations"]["invocations"][0]["termination"] =
                json!(if state == "reaped" {
                    "nonterminal_wait4_return"
                } else {
                    "completed_wait4"
                });
            assert!(!valid(&crossed));
        }
        for status in [0, 256, libc::SIGTERM, -1, 65536] {
            let mut bad = executed_row();
            let i = &mut bad["cpu_observations"]["invocations"][0];
            i["final_wait"]["state"] = json!("nonterminal_return");
            i["final_wait"]["raw_status"] = json!(status);
            i["termination"] = json!("nonterminal_wait4_return");
            assert!(!valid(&bad), "false nonterminal receipt {status}");
        }
        let mut budget = fast.clone();
        budget["cpu_observations"]["invocations"][0]["termination"] =
            json!("final_wait_cpu_budget_return");
        budget["attempts"][0]["timed_out"] = json!(true);
        assert!(valid(&budget)); // zero polls, refused registration, actual wait charge
        let mut disabled = budget.clone();
        disabled["cpu_observations"]["invocations"][0]["live"] = json!({"state":"disabled"});
        assert!(!valid(&disabled));
        let mut triggered = budget.clone();
        triggered["cpu_observations"]["invocations"][0]["live"] = enabled();
        triggered["cpu_observations"]["invocations"][0]["live"]["timeout_trigger"] =
            triggered["cpu_observations"]["invocations"][0]["live"]["last"].clone();
        assert!(!valid(&triggered)); // no fabricated live trigger/stop
        for termination in [
            "wait_error",
            "wall_budget_stop",
            "cpu_budget_stop",
            "accounting_unavailable_stop",
        ] {
            for operation in ["poll", "stop_grace", "blocking_stop"] {
                let mut record = executed_row();
                let i = &mut record["cpu_observations"]["invocations"][0];
                i["termination"] = json!(termination);
                i["final_wait"] = json!({"state":"unavailable","operation":operation,"errno":libc::ECHILD,"reason":"fixture wait refusal"});
                i["returned_cpu_charge"] = json!({"state":"unavailable"});
                if termination == "cpu_budget_stop" {
                    i["live"] = enabled();
                    i["live"]["timeout_trigger"] = i["live"]["last"].clone();
                } else if termination == "accounting_unavailable_stop" {
                    i["live"] = row["cpu_observations"]["invocations"][0]["live"].clone();
                }
                assert_eq!(
                    valid(&record),
                    (termination == "wait_error") == (operation == "poll"),
                    "{termination}/{operation}"
                );
            }
        }
        for flag in [Value::Null, json!(0), json!("false")] {
            let mut bad = fast.clone();
            bad["attempts"][0]["timed_out"] = flag;
            assert!(!valid(&bad));
        }
        for (launch, termination) in [
            (
                json!({"state":"not_started","reason":"cpu_budget_already_exhausted"}),
                "not_started",
            ),
            (
                json!({"state":"not_started","reason":"wall_budget_already_exhausted"}),
                "not_started",
            ),
            (
                json!({"state":"spawn_failed","stage":"stdout_capture","reason":"fixture refusal"}),
                "spawn_failed",
            ),
            (
                json!({"state":"spawn_failed","stage":"stderr_capture","reason":"fixture refusal"}),
                "spawn_failed",
            ),
            (
                json!({"state":"spawn_failed","stage":"spawn","reason":"fixture refusal"}),
                "spawn_failed",
            ),
        ] {
            let mut record = executed_row();
            let i = &mut record["cpu_observations"]["invocations"][0];
            let cpu_exhausted = launch["reason"] == "cpu_budget_already_exhausted";
            i["launch"] = launch;
            i["termination"] = json!(termination);
            i["final_wait"] = json!({"state":"not_applicable"});
            i["returned_cpu_charge"] = json!({"state":"unavailable"});
            assert_eq!(valid(&record), !cpu_exhausted);
            record["cpu_observations"]["invocations"][0]["live"] = refused_before_poll();
            record["cpu_observations"]["invocations"][0]["live"]["registration"] =
                json!({"state":"not_attempted"});
            assert!(valid(&record));
            if termination == "not_started" {
                record["attempts"][0]["timed_out"] = json!(true);
                assert!(valid(&record));
                record["attempts"][0]["timed_out"] = json!(false);
                assert!(!valid(&record));
            }
        }
    }

    #[test]
    fn role_references_are_local_ordered_and_not_numeric_pid_identity() {
        let mut row = executed_row();
        row["backend"] = json!("kvm");
        row["attempts"][0]["index"] = json!("verify-1");
        row["attempts"].as_array_mut().unwrap().push(
            json!({"index":"parity-reference","argv":["reference"],"cwd":"/fixture","env":{}}),
        );
        for attempt in row["attempts"].as_array_mut().unwrap() {
            attempt["outcome"] = json!("PASS");
            attempt["status"] = json!(0);
            attempt["signal"] = Value::Null;
            attempt["timed_out"] = json!(false);
        }
        row["cpu_observations"] = envelope(&row);
        assert!(valid(&row)); // descriptive PID17 may legitimately repeat
        let mut comparison = row["cpu_observations"]["invocations"][0].clone();
        comparison["ordinal"] = json!(3);
        comparison["role"] =
            json!({"kind":"parity_comparison","candidate_execution":1,"reference_execution":2});
        row["cpu_observations"]["invocations"]
            .as_array_mut()
            .unwrap()
            .push(comparison);
        assert!(valid(&row));
        for foreign in [0, 1, 3, 100] {
            let mut bad = row.clone();
            bad["cpu_observations"]["invocations"][2]["role"]["reference_execution"] =
                json!(foreign);
            assert!(!valid(&bad), "reference {foreign}");
        }
        let mut reversed = row.clone();
        reversed["cpu_observations"]["invocations"][2]["role"] =
            json!({"kind":"parity_comparison","candidate_execution":2,"reference_execution":1});
        assert!(!valid(&reversed));
        let mut same_backend = row.clone();
        same_backend["backend"] = json!("ptrace");
        same_backend["cpu_observations"]["binding"]["backend"] = json!("ptrace");
        same_backend["cpu_observations"]["invocations"][0]["role"]["backend"] = json!("ptrace");
        assert!(!valid(&same_backend));
        let mut naked = row.clone();
        naked["mode"] = json!("naked");
        naked["cpu_observations"]["binding"]["mode"] = json!("naked");
        assert!(!valid(&naked));
        let mut normalized = row.clone();
        normalized["cpu_observations"]["invocations"][2]["role"] =
            json!({"kind":"ptrace_normalization","execution_ordinal":2});
        assert!(valid(&normalized));
        let mut nonzero_exit = normalized.clone();
        nonzero_exit["cpu_observations"]["invocations"][1]["final_wait"]["raw_status"] = json!(256);
        nonzero_exit["attempts"][1]["outcome"] = json!("FAIL");
        nonzero_exit["attempts"][1]["status"] = json!(1);
        assert!(valid(&nonzero_exit)); // a nonzero exit still has no timeout
        for termination in ["wall_budget_stop", "cpu_budget_stop"] {
            let mut timed_out_parent = normalized.clone();
            timed_out_parent["attempts"][1]["timed_out"] = json!(true);
            timed_out_parent["attempts"][1]["outcome"] = json!("FAIL");
            let prior = &mut timed_out_parent["cpu_observations"]["invocations"][1];
            prior["termination"] = json!(termination);
            if termination == "cpu_budget_stop" {
                prior["live"] = enabled();
                prior["live"]["timeout_trigger"] = prior["live"]["last"].clone();
                prior["returned_cpu_charge"] =
                    json!({"state":"value","cpu_usec":7,"basis":"max_trigger_and_final_wait4"});
            }
            let mut without_normalization = timed_out_parent.clone();
            without_normalization["cpu_observations"]["invocations"]
                .as_array_mut()
                .unwrap()
                .pop();
            assert!(valid(&without_normalization), "valid {termination} parent");
            assert!(
                !valid(&timed_out_parent),
                "normalization admitted after {termination}"
            );
        }
        // Exercise whole relationships, not just an isolated role label. Failed
        // invocations remain admissible when no impossible successor is claimed.
        fn fail(invocation: &mut Value, kind: &str) {
            match kind {
                "nonzero" => invocation["final_wait"]["raw_status"] = json!(256),
                "signal" => invocation["final_wait"]["raw_status"] = json!(libc::SIGTERM),
                "nonterminal" => {
                    invocation["termination"] = json!("nonterminal_wait4_return");
                    invocation["final_wait"]["state"] = json!("nonterminal_return");
                    invocation["final_wait"]["raw_status"] = json!((libc::SIGSTOP << 8) | 127);
                }
                "final_cpu" => {
                    invocation["termination"] = json!("final_wait_cpu_budget_return");
                    invocation["live"] = refused_before_poll();
                }
                "wall" => invocation["termination"] = json!("wall_budget_stop"),
                "cpu" => {
                    invocation["termination"] = json!("cpu_budget_stop");
                    invocation["live"] = enabled();
                    invocation["live"]["timeout_trigger"] = invocation["live"]["last"].clone();
                    invocation["returned_cpu_charge"] =
                        json!({"state":"value","cpu_usec":7,"basis":"max_trigger_and_final_wait4"});
                }
                _ => unreachable!(),
            }
        }
        fn keep_prefix(row: &mut Value, invocations: usize, attempts: usize) {
            row["cpu_observations"]["invocations"]
                .as_array_mut()
                .unwrap()
                .truncate(invocations);
            row["attempts"].as_array_mut().unwrap().truncate(attempts);
        }
        let mut admitted = Vec::new();
        let mut reject = |label: String, value: &Value| {
            if valid(value) {
                admitted.push(label);
            }
        };
        for kind in [
            "nonzero",
            "signal",
            "nonterminal",
            "wall",
            "cpu",
            "final_cpu",
        ] {
            for operand in [0, 1] {
                let mut bad = row.clone();
                fail(&mut bad["cpu_observations"]["invocations"][operand], kind);
                bad["attempts"][operand]["timed_out"] =
                    json!(["wall", "cpu", "final_cpu"].contains(&kind));
                let mut alone = bad.clone();
                keep_prefix(&mut alone, operand + 1, operand + 1);
                assert!(valid(&alone), "failed operand alone {operand}/{kind}");
                reject(format!("comparison operand {operand}/{kind}"), &bad);
                if operand == 0 {
                    keep_prefix(&mut bad, 2, 2);
                    reject(format!("reference after candidate {kind}"), &bad);
                }
            }
            let mut comparator_failed = row.clone();
            fail(
                &mut comparator_failed["cpu_observations"]["invocations"][2],
                kind,
            );
            assert!(valid(&comparator_failed), "comparator itself {kind}");
        }
        for operand in [0, 1] {
            for field in ["outcome", "status", "signal", "timed_out"] {
                let mut bad = row.clone();
                bad["attempts"][operand]
                    .as_object_mut()
                    .unwrap()
                    .remove(field);
                reject(format!("missing prerequisite {operand}/{field}"), &bad);
            }
            for outcome in ["FAIL", "ERROR"] {
                let mut bad = row.clone();
                bad["attempts"][operand]["outcome"] = json!(outcome);
                reject(format!("semantic prerequisite {operand}/{outcome}"), &bad);
                // Without supplied observations this remains the historical path.
                bad.as_object_mut().unwrap().remove("cpu_observations");
                assert!(valid(&bad));
            }
            for (field, value) in [
                ("outcome", json!(true)),
                ("status", json!("0")),
                ("status", json!(0.0)),
                ("signal", json!("none")),
                ("timed_out", Value::Null),
                ("timed_out", json!("false")),
            ] {
                let mut bad = row.clone();
                bad["attempts"][operand][field] = value;
                reject(format!("malformed prerequisite {operand}/{field}"), &bad);
            }
            let mut missing = row.clone();
            missing["attempts"].as_array_mut().unwrap().remove(operand);
            reject(format!("missing retained operand {operand}"), &missing);
        }
        let mut reference_only = row.clone();
        reference_only["cpu_observations"]["invocations"] =
            json!([row["cpu_observations"]["invocations"][1].clone()]);
        reference_only["cpu_observations"]["invocations"][0]["ordinal"] = json!(1);
        reference_only["attempts"] = json!([row["attempts"][1].clone()]);
        reject("reference without candidate".into(), &reference_only);

        let mut prepared = executed_row();
        let execution = prepared["cpu_observations"]["invocations"][0].clone();
        let mut prep = execution.clone();
        prep["role"] = json!({"kind":"preparation"});
        prepared["cpu_observations"]["invocations"]
            .as_array_mut()
            .unwrap()
            .insert(0, prep);
        prepared["cpu_observations"]["invocations"][1]["ordinal"] = json!(2);
        assert!(valid(&prepared));
        for kind in ["nonzero", "signal", "nonterminal", "wall", "final_cpu"] {
            let mut bad = prepared.clone();
            fail(&mut bad["cpu_observations"]["invocations"][0], kind);
            let mut alone = bad.clone();
            keep_prefix(&mut alone, 1, 0);
            assert!(valid(&alone), "preparation failure alone {kind}");
            reject(format!("execution after preparation {kind}"), &bad);
        }

        let mut ordinary = executed_row();
        ordinary["mode"] = json!("naked");
        ordinary["attempts"][0]["timed_out"] = json!(false);
        let second = ordinary["attempts"][0].clone();
        ordinary["attempts"].as_array_mut().unwrap().push(second);
        ordinary["attempts"][1]["index"] = json!("2");
        ordinary["cpu_observations"] = envelope(&ordinary);
        for kind in ["nonzero", "signal", "nonterminal"] {
            let mut continued = ordinary.clone();
            fail(&mut continued["cpu_observations"]["invocations"][0], kind);
            assert!(
                valid(&continued),
                "ordinary completed failure may continue {kind}"
            );
        }
        for kind in ["wall", "cpu", "final_cpu"] {
            let mut bad = ordinary.clone();
            fail(&mut bad["cpu_observations"]["invocations"][0], kind);
            bad["attempts"][0]["timed_out"] = json!(true);
            let mut alone = bad.clone();
            keep_prefix(&mut alone, 1, 1);
            assert!(valid(&alone));
            reject(format!("execution after ordinary timeout {kind}"), &bad);
        }
        let mut four_roles = row.clone();
        four_roles["cpu_observations"]["invocations"]
            .as_array_mut()
            .unwrap()
            .insert(2, normalized["cpu_observations"]["invocations"][2].clone());
        four_roles["cpu_observations"]["invocations"][3]["ordinal"] = json!(4);
        assert!(valid(&four_roles));
        for kind in ["nonzero", "signal", "nonterminal"] {
            let mut continued = four_roles.clone();
            fail(&mut continued["cpu_observations"]["invocations"][2], kind);
            assert!(
                valid(&continued),
                "completed normalizer may continue {kind}"
            );
            let mut parent = normalized.clone();
            fail(&mut parent["cpu_observations"]["invocations"][1], kind);
            assert!(valid(&parent), "normalization parent need not exit0 {kind}");
        }
        for kind in ["wall", "cpu", "final_cpu"] {
            let mut bad = four_roles.clone();
            fail(&mut bad["cpu_observations"]["invocations"][2], kind);
            let mut alone = bad.clone();
            keep_prefix(&mut alone, 3, 2);
            assert!(valid(&alone));
            reject(
                format!("comparison after normalization timeout {kind}"),
                &bad,
            );
        }
        // A retained semantic timeout cannot be erased by relabeling the CPU
        // path CompletedWait4 to manufacture a later normalization/execution.
        for mut successor in [normalized.clone(), ordinary.clone()] {
            let operand = if successor["mode"] == "verify" { 1 } else { 0 };
            successor["attempts"][operand]["timed_out"] = json!(true);
            reject("relabelled timeout with successor".into(), &successor);
            successor["attempts"][operand]
                .as_object_mut()
                .unwrap()
                .remove("timed_out");
            reject("unknown timeout with successor".into(), &successor);
        }
        // A nonterminal helper return permits normalization without becoming a
        // successful reference prerequisite for a subsequent comparison.
        let mut nonterminal_reference = normalized.clone();
        fail(
            &mut nonterminal_reference["cpu_observations"]["invocations"][1],
            "nonterminal",
        );
        nonterminal_reference["attempts"][1]["outcome"] = json!("FAIL");
        nonterminal_reference["attempts"][1]["status"] = Value::Null;
        assert!(valid(&nonterminal_reference));
        let mut extra = row["cpu_observations"]["invocations"][2].clone();
        extra["ordinal"] = json!(4);
        nonterminal_reference["cpu_observations"]["invocations"]
            .as_array_mut()
            .unwrap()
            .push(extra);
        reject(
            "comparison after nonterminal reference and normalization".into(),
            &nonterminal_reference,
        );
        let mut after_comparison = row.clone();
        let mut extra = execution.clone();
        extra["ordinal"] = json!(4);
        extra["role"] = json!({"kind":"execution","attempt_index":"extra","backend":"kvm"});
        after_comparison["cpu_observations"]["invocations"]
            .as_array_mut()
            .unwrap()
            .push(extra);
        reject("execution after comparison".into(), &after_comparison);
        let mut repeated_verify = ordinary.clone();
        repeated_verify["mode"] = json!("verify");
        repeated_verify["cpu_observations"]["binding"]["mode"] = json!("verify");
        reject("repeated verify candidate".into(), &repeated_verify);
        let mut late_normalization = repeated_verify;
        let mut late = normalized["cpu_observations"]["invocations"][2].clone();
        late["role"]["execution_ordinal"] = json!(1);
        late_normalization["cpu_observations"]["invocations"]
            .as_array_mut()
            .unwrap()
            .push(late);
        reject("nonadjacent normalization".into(), &late_normalization);
        assert!(
            admitted.is_empty(),
            "impossible role relationships admitted: {admitted:?}"
        );

        let mut wrong_backend = normalized.clone();
        wrong_backend["cpu_observations"]["invocations"][2]["role"]["execution_ordinal"] = json!(1);
        assert!(!valid(&wrong_backend));
        let mut naked = normalized.clone();
        naked["mode"] = json!("naked");
        naked["cpu_observations"]["binding"]["mode"] = json!("naked");
        assert!(!valid(&naked));
        normalized["cpu_observations"]["invocations"][2]["role"]["execution_ordinal"] = json!(3);
        assert!(!valid(&normalized));
    }

    #[test]
    fn conditional_history_retains_unrecorded_rows_and_rejects_substitution() {
        let first = source_row();
        let mut second = executed_row();
        second["attempt"] = json!(2);
        second["cpu_observations"]["binding"]["outer_attempt"] = json!(2);
        assert!(
            CellCpuHistoryV1::from_source_rows(&[(1, first.clone())])
                .unwrap()
                .is_none()
        );
        let history = CellCpuHistoryV1::from_source_rows(&[(1, first), (2, second.clone())])
            .unwrap()
            .unwrap();
        assert!(matches!(
            history.attempts[0],
            CpuAttemptHistory::Unrecorded { outer_attempt: 1 }
        ));
        assert!(history.attempts[1].observations().is_some());
        let id = CellIdentity {
            lane: "portable".into(),
            category: "fixture".into(),
            test: "fixture/test".into(),
            mode: "verify".into(),
            backend: "ptrace".into(),
        };
        history
            .validate_for_artifact("run", &"a".repeat(40), &id, 2)
            .unwrap();
        let mut missing = history.clone();
        missing.attempts.remove(0);
        assert!(
            missing
                .validate_for_artifact("run", &"a".repeat(40), &id, 2)
                .is_err()
        );
        let mut substituted = history.clone();
        if let CpuAttemptHistory::Recorded { observations, .. } = &mut substituted.attempts[1] {
            observations.binding.run_id = "foreign".into();
        }
        assert!(
            substituted
                .validate_for_artifact("run", &"a".repeat(40), &id, 2)
                .is_err()
        );
        let mut all_absent = history.clone();
        all_absent.attempts[1] = CpuAttemptHistory::Unrecorded { outer_attempt: 2 };
        assert!(
            all_absent
                .validate_for_artifact("run", &"a".repeat(40), &id, 2)
                .is_err()
        );
        assert!(CellCpuHistoryV1::from_source_rows(&[(1, second)]).is_err());
    }
}
