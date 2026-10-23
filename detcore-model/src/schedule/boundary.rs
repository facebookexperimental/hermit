/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Explicit slice-boundary protocol. A complete typed stream keeps ordinary
//! events and actual scheduling boundaries in one causal order.
//!
//! A boundary is evidence of an existing scheduling request, not an instruction
//! or permission to introduce a request. Its identity must come from the actual
//! semantic callback, before the completed slice is reset. Replay expectations
//! cannot supply observed identities. The controller must report any error here
//! as a failed replay, irrespective of the legacy `die_on_desync` setting.

use std::collections::BTreeMap;
use std::fmt;

use reverie_syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use super::InstructionPointer;
use super::SchedEvent;
use super::SigWrapper;
use crate::pid::DetTid;
use crate::time::LogicalTime;

/// Stable owner identity. These are logical generations, never host PIDs or
/// addresses. Exec advances the address-space generation; thread creation must
/// allocate a new thread identity even if a deterministic TID is reused.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Serialize,
    Deserialize
)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    /// Current deterministic TID, which Linux can rebind on non-leader exec.
    pub tid: DetTid,
    /// Incarnation of the current TID slot, including an exec replacement.
    pub thread_generation: u64,
    /// Logical thread's creation TID, retained when exec rebinds its current TID.
    pub birth_tid: DetTid,
    /// Creation incarnation; a later thread reusing that TID is distinct.
    pub birth_generation: u64,
    /// Creator component of the actual model address-space identity.
    pub address_space_creator: DetTid,
    /// Exec-generation component of the actual model address-space identity.
    pub address_space_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    Prehook,
    Posthook,
    /// Existing no-RCB maximum body, after its time/ordinary-event update.
    TimerMaximum,
}

/// The mechanism actually delivered to Detcore. RIP availability does not
/// distinguish these causes, and stripping a timestamp must not erase them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Cause {
    SoftwareCheckin,
    PmuTimer,
    NoRcbTimer,
    ExplicitSchedYield,
}

/// A semantic entry into Detcore. Hidden backend stops, injected-call retries
/// and dropped host signals are not entries. A signal delivery number comes
/// from the already selected logical delivery, not its host arrival time.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Entry {
    Syscall {
        syscall: Sysno,
        occurrence: u64,
    },
    Rdtsc,
    Cpuid,
    PmuTimer,
    NoRcbTimer,
    ThreadStart,
    Exec,
    SignalEntry {
        signal: SigWrapper,
        occurrence: u64,
        announced: bool,
    },
    Signal {
        signal: SigWrapper,
        occurrence: u64,
        announced: bool,
        delivery: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub owner: Owner,
    pub cause: Cause,
    pub phase: Phase,
    pub entry: Entry,
    /// Committed guest branch count within this address-space generation.
    pub committed_rcbs: u64,
    /// One-based semantic check-in number, independent of host callbacks.
    pub checkin: u64,
    /// One-based completed slice number. Adjacent zero-RCB slices at the same
    /// check-in have different slice numbers.
    pub slice: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Boundary {
    pub identity: Identity,
    /// Always one. A boundary must never be run-length encoded.
    pub count: u32,
    pub start_rip: Option<InstructionPointer>,
    pub end_rip: Option<InstructionPointer>,
    pub end_time: Option<LogicalTime>,
}

/// The stored stream index includes boundary records. Keeping a single stream
/// prevents an inspector from accidentally indexing only guest operations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Record {
    Event(SchedEvent),
    TimesliceEnd(Boundary),
}

impl Record {
    /// Match one complete stored record. Only evidence absent from the stored
    /// record is optional; all semantic identity and count fields are exact.
    pub fn check_observation(&self, index: usize, observed: &Record) -> Result<(), Error> {
        let mut comparable = observed.clone();
        match (self, &mut comparable) {
            (Record::Event(e), Record::Event(o)) => {
                if e.end_time.is_none() {
                    o.end_time = None;
                }
                if e.start_rip.is_none() {
                    o.start_rip = None;
                }
                if e.end_rip.is_none() {
                    o.end_rip = None;
                }
            }
            (Record::TimesliceEnd(e), Record::TimesliceEnd(o)) => {
                if e.end_time.is_none() {
                    o.end_time = None;
                }
                if e.start_rip.is_none() {
                    o.start_rip = None;
                }
                if e.end_rip.is_none() {
                    o.end_rip = None;
                }
            }
            _ => {}
        }
        if self != &comparable {
            return Err(Error::Mismatch {
                index,
                expected: Box::new(self.clone()),
                observed: Box::new(observed.clone()),
            });
        }
        Ok(())
    }
}

/// A separate envelope prevents an old `PreemptionRecord` reader from silently
/// ignoring a new metadata field, including for a recording with zero markers.
/// No implicit conversion to a vector of legacy events is provided.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Wire", into = "Wire")]
pub struct Schedule {
    records: Vec<Record>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    protocol: String,
    version: u32,
    record_count: usize,
    boundary_count: usize,
    records: Vec<Record>,
}

const PROTOCOL: &str = "hermit-slice-boundaries";
const VERSION: u32 = 1;

/// Named failures, carrying the stored stream index where applicable. These
/// are errors, not recovery hints, skipped edits or successful partial replays.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    UnsupportedProtocol {
        protocol: String,
        version: u32,
    },
    RecordCount {
        declared: usize,
        actual: usize,
    },
    BoundaryCount {
        declared: usize,
        actual: usize,
    },
    InvalidBoundary {
        index: usize,
        reason: &'static str,
    },
    InvalidEvent {
        index: usize,
        reason: &'static str,
    },
    Mismatch {
        index: usize,
        expected: Box<Record>,
        observed: Box<Record>,
    },
    Exhausted {
        index: usize,
        observed: Box<Record>,
    },
    Incomplete {
        index: usize,
        remaining: usize,
    },
    UnsupportedEdit {
        operation: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "slice-boundary protocol refusal: {self:?}")
    }
}

impl std::error::Error for Error {}

impl TryFrom<Wire> for Schedule {
    type Error = Error;

    fn try_from(wire: Wire) -> Result<Self, Error> {
        if wire.protocol != PROTOCOL || wire.version != VERSION {
            return Err(Error::UnsupportedProtocol {
                protocol: wire.protocol,
                version: wire.version,
            });
        }
        if wire.record_count != wire.records.len() {
            return Err(Error::RecordCount {
                declared: wire.record_count,
                actual: wire.records.len(),
            });
        }
        let actual = boundary_count(&wire.records);
        if wire.boundary_count != actual {
            return Err(Error::BoundaryCount {
                declared: wire.boundary_count,
                actual,
            });
        }
        Self::new(wire.records)
    }
}

impl From<Schedule> for Wire {
    fn from(schedule: Schedule) -> Self {
        Self {
            protocol: PROTOCOL.into(),
            version: VERSION,
            record_count: schedule.records.len(),
            boundary_count: boundary_count(&schedule.records),
            records: schedule.records,
        }
    }
}

fn boundary_count(records: &[Record]) -> usize {
    records
        .iter()
        .filter(|record| matches!(record, Record::TimesliceEnd(_)))
        .count()
}

impl Boundary {
    /// Validate the mandatory shape without imposing a stream position. The
    /// full schedule separately checks ordering and generation transitions.
    pub fn validate_shape(&self, index: usize) -> Result<(), Error> {
        let boundary = self;
        let id = self.identity;
        let invalid = |reason| Error::InvalidBoundary { index, reason };
        if boundary.count != 1 {
            return Err(invalid("boundary count must be one"));
        }
        if id.owner.tid.as_raw() <= 0
            || id.owner.birth_tid.as_raw() <= 0
            || id.owner.address_space_creator.as_raw() <= 0
            || id.checkin == 0
            || id.slice == 0
        {
            return Err(invalid("nonpositive owner or zero occurrence"));
        }
        match id.entry {
            Entry::Syscall { occurrence: 0, .. }
            | Entry::Signal { delivery: 0, .. }
            | Entry::Signal { occurrence: 0, .. }
            | Entry::SignalEntry { occurrence: 0, .. } => {
                return Err(invalid("zero syscall or signal occurrence"));
            }
            Entry::Signal { signal, .. } | Entry::SignalEntry { signal, .. }
                if !(1..=64).contains(&signal.raw()) =>
            {
                return Err(invalid("invalid logical signal"));
            }
            _ => {}
        }
        if (matches!(id.entry, Entry::SignalEntry { .. }) && id.phase != Phase::Prehook)
            || (matches!(id.entry, Entry::Signal { .. }) && id.phase != Phase::Posthook)
        {
            return Err(invalid("signal entry/delivery phase mismatch"));
        }
        if id.phase == Phase::TimerMaximum && id.entry != Entry::NoRcbTimer {
            return Err(invalid(
                "maximum-body phase requires the actual no-RCB timer",
            ));
        }
        let cause_matches = match id.cause {
            Cause::PmuTimer => id.entry == Entry::PmuTimer,
            Cause::NoRcbTimer => id.entry == Entry::NoRcbTimer,
            Cause::ExplicitSchedYield => matches!(
                id.entry,
                Entry::Syscall {
                    syscall: Sysno::sched_yield,
                    ..
                }
            ),
            Cause::SoftwareCheckin => !matches!(id.entry, Entry::PmuTimer | Entry::NoRcbTimer),
        };
        if !cause_matches {
            return Err(invalid("cause disagrees with actual semantic entry"));
        }
        Ok(())
    }
}

impl Schedule {
    pub fn new(records: Vec<Record>) -> Result<Self, Error> {
        let mut previous: BTreeMap<Owner, Identity> = BTreeMap::new();
        let mut latest_owner: BTreeMap<DetTid, Owner> = BTreeMap::new();
        let mut latest_birth: BTreeMap<(DetTid, u64), Owner> = BTreeMap::new();
        for (index, record) in records.iter().enumerate() {
            if let Record::Event(event) = record {
                if event.dettid.as_raw() <= 0 || event.count == 0 {
                    return Err(Error::InvalidEvent {
                        index,
                        reason: "event owner and count must be positive",
                    });
                }
                continue;
            }
            let Record::TimesliceEnd(boundary) = record else {
                unreachable!();
            };
            let id = boundary.identity;
            let invalid = |reason| Error::InvalidBoundary { index, reason };
            boundary.validate_shape(index)?;
            if let Some(last) = latest_owner.get(&id.owner.tid)
                && (id.owner.thread_generation < last.thread_generation
                    || (id.owner.thread_generation == last.thread_generation
                        && (id.owner.birth_tid != last.birth_tid
                            || id.owner.birth_generation != last.birth_generation
                            || id.owner.address_space_generation < last.address_space_generation
                            || (id.owner.address_space_generation
                                == last.address_space_generation
                                && id.owner.address_space_creator != last.address_space_creator))))
            {
                return Err(invalid("owner generation moved backwards"));
            }
            let birth = (id.owner.birth_tid, id.owner.birth_generation);
            if let Some(last) = latest_birth.get(&birth)
                && (id.owner.address_space_generation < last.address_space_generation
                    || (id.owner.address_space_generation == last.address_space_generation
                        && id.owner != *last))
            {
                return Err(invalid(
                    "logical survivor moved without a new exec incarnation",
                ));
            }
            if let Some(last) = previous.get(&id.owner) {
                if last.slice.checked_add(1) != Some(id.slice) {
                    return Err(invalid("missing, duplicated or reordered slice occurrence"));
                }
                if id.checkin < last.checkin || id.committed_rcbs < last.committed_rcbs {
                    return Err(invalid("check-in or guest RCB count moved backwards"));
                }
                if id.checkin == last.checkin
                    && (id.entry != last.entry
                        || id.phase != last.phase
                        || id.committed_rcbs != last.committed_rcbs)
                {
                    return Err(invalid("one semantic check-in has conflicting identities"));
                }
            } else if id.slice != 1 {
                return Err(invalid("first completed slice must be one"));
            }
            previous.insert(id.owner, id);
            latest_owner.insert(id.owner.tid, id.owner);
            latest_birth.insert(birth, id.owner);
        }
        Ok(Self { records })
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Only the optional outer timestamps are stripped. Semantic identity and
    /// both RIP observations remain, including in a boundary-only recording.
    pub fn strip_times(&mut self) {
        for record in &mut self.records {
            match record {
                Record::Event(event) => event.end_time = None,
                Record::TimesliceEnd(boundary) => boundary.end_time = None,
            }
        }
    }

    /// Boundary-aware schedule editing is not implemented. Refuse before an
    /// editor receives mutable output or loses the explicit protocol header.
    /// This applies even when this particular recording contains zero markers.
    pub fn into_legacy_events(self) -> Result<Vec<SchedEvent>, Error> {
        Err(Error::UnsupportedEdit {
            operation: "discard protocol into legacy event vector",
        })
    }

    pub fn edit(&mut self, operation: Edit) -> Result<(), Error> {
        Err(Error::UnsupportedEdit {
            operation: operation.name(),
        })
    }

    pub fn cursor(&self) -> Cursor<'_> {
        Cursor {
            schedule: self,
            next: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Edit {
    Split,
    Search,
    Minimize,
}

impl Edit {
    fn name(self) -> &'static str {
        match self {
            Self::Split => "split boundary schedule",
            Self::Search => "search boundary schedule",
            Self::Minimize => "minimize boundary schedule",
        }
    }
}

/// Strict protocol cursor. A failed observation never advances it. There is
/// deliberately no fast-forward or permissive-desync option. Production
/// integration must route these errors through the controller refusal path.
pub struct Cursor<'a> {
    schedule: &'a Schedule,
    next: usize,
}

impl Cursor<'_> {
    pub fn stored_index(&self) -> usize {
        self.next
    }

    pub fn expected(&self) -> Option<&Record> {
        self.schedule.records.get(self.next)
    }

    pub fn observe(&mut self, observed: &Record) -> Result<(), Error> {
        let Some(expected) = self.expected() else {
            return Err(Error::Exhausted {
                index: self.next,
                observed: Box::new(observed.clone()),
            });
        };
        expected.check_observation(self.next, observed)?;
        self.next += 1;
        Ok(())
    }

    pub fn finish(self) -> Result<(), Error> {
        let remaining = self.schedule.records.len() - self.next;
        if remaining == 0 {
            Ok(())
        } else {
            Err(Error::Incomplete {
                index: self.next,
                remaining,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::Op;

    #[test]
    fn birth_identity_is_required_and_cannot_move_without_exec() {
        let first = boundary();
        let bytes = serde_json::to_value(&first).unwrap();
        for missing in ["birth_tid", "birth_generation"] {
            let mut old = bytes.clone();
            old["identity"]["owner"]
                .as_object_mut()
                .unwrap()
                .remove(missing);
            assert!(serde_json::from_value::<Boundary>(old).is_err());
        }
        let mut changed = first.clone();
        changed.identity.owner.tid = DetTid::from_raw(4);
        assert!(
            Schedule::new(vec![
                Record::TimesliceEnd(first.clone()),
                Record::TimesliceEnd(changed.clone())
            ])
            .is_err()
        );
        changed.identity.owner.address_space_creator = DetTid::from_raw(4);
        changed.identity.owner.address_space_generation += 1;
        assert!(
            Schedule::new(vec![
                Record::TimesliceEnd(first.clone()),
                Record::TimesliceEnd(changed.clone())
            ])
            .is_ok()
        );
        let mut retired = first.clone();
        retired.identity.slice += 1;
        assert!(
            Schedule::new(vec![
                Record::TimesliceEnd(first),
                Record::TimesliceEnd(changed),
                Record::TimesliceEnd(retired)
            ])
            .is_err()
        );
    }

    fn boundary() -> Boundary {
        Boundary {
            identity: Identity {
                owner: Owner {
                    tid: DetTid::from_raw(3),
                    thread_generation: 0,
                    birth_tid: DetTid::from_raw(3),
                    birth_generation: 0,
                    address_space_creator: DetTid::from_raw(3),
                    address_space_generation: 0,
                },
                cause: Cause::SoftwareCheckin,
                phase: Phase::Posthook,
                entry: Entry::Syscall {
                    syscall: Sysno::write,
                    occurrence: 7,
                },
                committed_rcbs: 83,
                checkin: 10,
                slice: 1,
            },
            count: 1,
            start_rip: InstructionPointer::new(0x1000),
            end_rip: InstructionPointer::new(0x1002),
            end_time: Some(LogicalTime::from_rcbs(83)),
        }
    }

    fn one() -> Schedule {
        Schedule::new(vec![Record::TimesliceEnd(boundary())]).unwrap()
    }

    #[test]
    fn envelope_round_trip_preserves_every_field() {
        let schedule = one();
        let encoded = serde_json::to_vec(&schedule).unwrap();
        assert_eq!(
            serde_json::from_slice::<Schedule>(&encoded).unwrap(),
            schedule
        );
        assert_eq!(
            serde_json::to_vec(&serde_json::from_slice::<Schedule>(&encoded).unwrap()).unwrap(),
            encoded
        );
    }

    #[test]
    fn empty_schedule_still_requires_supported_protocol() {
        let original = Schedule::new(vec![]).unwrap();
        let value = serde_json::to_value(&original).unwrap();
        assert_eq!(value["protocol"], PROTOCOL);
        assert_eq!(value["version"], VERSION);
        for key in ["protocol", "version", "record_count", "boundary_count"] {
            let mut bad = value.clone();
            bad.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<Schedule>(bad).is_err(),
                "missing {key}"
            );
        }
        for bad_version in [0, 2, u32::MAX] {
            let mut bad = value.clone();
            bad["version"] = bad_version.into();
            assert!(serde_json::from_value::<Schedule>(bad).is_err());
        }
        let mut bad = value;
        bad["protocol"] = "unknown".into();
        assert!(serde_json::from_value::<Schedule>(bad).is_err());
    }

    #[test]
    fn deleting_sole_boundary_cannot_select_legacy_replay() {
        let mut bad = serde_json::to_value(one()).unwrap();
        bad["records"] = serde_json::json!([]);
        assert!(serde_json::from_value::<Schedule>(bad.clone()).is_err());
        bad["record_count"] = 0.into();
        assert!(serde_json::from_value::<Schedule>(bad.clone()).is_err());
        bad["boundary_count"] = 0.into();
        let empty: Schedule = serde_json::from_value(bad).unwrap();
        assert!(matches!(
            empty.clone().into_legacy_events(),
            Err(Error::UnsupportedEdit { .. })
        ));
        assert!(matches!(
            empty.cursor().observe(&Record::TimesliceEnd(boundary())),
            Err(Error::Exhausted { index: 0, .. })
        ));
    }

    #[test]
    fn malformed_fields_are_not_ignored() {
        let value = serde_json::to_value(one()).unwrap();
        for pointer in [
            "",
            "/records/0/TimesliceEnd",
            "/records/0/TimesliceEnd/identity",
            "/records/0/TimesliceEnd/identity/owner",
        ] {
            let mut bad = value.clone();
            bad.pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("future_field".into(), true.into());
            assert!(
                serde_json::from_value::<Schedule>(bad).is_err(),
                "unknown field at {pointer}"
            );
        }
        let mut bad = value.clone();
        bad["records"][0]["TimesliceEnd"]["identity"]["cause"] = "Unknown".into();
        assert!(serde_json::from_value::<Schedule>(bad).is_err());
        let encoded = serde_json::to_vec(&one()).unwrap();
        assert!(serde_json::from_slice::<Schedule>(&encoded[..encoded.len() - 1]).is_err());
    }

    #[test]
    fn counts_and_cause_are_validated_on_deserialization() {
        for count in [0, 2, u32::MAX] {
            let mut bad = serde_json::to_value(one()).unwrap();
            bad["records"][0]["TimesliceEnd"]["count"] = count.into();
            assert!(serde_json::from_value::<Schedule>(bad).is_err());
        }
        for cause in [
            Cause::PmuTimer,
            Cause::NoRcbTimer,
            Cause::ExplicitSchedYield,
        ] {
            let mut b = boundary();
            b.identity.cause = cause;
            assert!(Schedule::new(vec![Record::TimesliceEnd(b)]).is_err());
        }
        for (cause, entry) in [
            (Cause::PmuTimer, Entry::PmuTimer),
            (Cause::NoRcbTimer, Entry::NoRcbTimer),
            (
                Cause::ExplicitSchedYield,
                Entry::Syscall {
                    syscall: Sysno::sched_yield,
                    occurrence: 1,
                },
            ),
        ] {
            let mut b = boundary();
            b.identity.cause = cause;
            b.identity.entry = entry;
            assert!(Schedule::new(vec![Record::TimesliceEnd(b)]).is_ok());
        }
    }

    #[test]
    fn every_mandatory_identity_field_is_exact_before_cursor_advance() {
        let original = boundary();
        let schedule = one();
        let mut mutations = vec![];
        let mut b = original.clone();
        b.identity.owner.tid = DetTid::from_raw(4);
        mutations.push(b);
        let mut b = original.clone();
        b.identity.owner.thread_generation += 1;
        mutations.push(b);
        let mut b = original.clone();
        b.identity.owner.address_space_generation += 1;
        mutations.push(b);
        let mut b = original.clone();
        b.identity.cause = Cause::ExplicitSchedYield;
        mutations.push(b);
        let mut b = original.clone();
        b.identity.phase = Phase::Prehook;
        mutations.push(b);
        let mut b = original.clone();
        b.identity.committed_rcbs += 1;
        mutations.push(b);
        let mut b = original.clone();
        b.identity.checkin += 1;
        mutations.push(b);
        let mut b = original.clone();
        b.identity.slice += 1;
        mutations.push(b);
        let mut b = original.clone();
        b.identity.entry = Entry::Syscall {
            syscall: Sysno::read,
            occurrence: 7,
        };
        mutations.push(b);
        let mut b = original.clone();
        b.identity.entry = Entry::Syscall {
            syscall: Sysno::write,
            occurrence: 8,
        };
        mutations.push(b);
        let mut b = original.clone();
        b.count = 2;
        mutations.push(b);
        let mut b = original.clone();
        b.end_rip = InstructionPointer::new(0x2000);
        mutations.push(b);
        let mut b = original.clone();
        b.start_rip = None;
        mutations.push(b);
        let mut b = original.clone();
        b.end_time = None;
        mutations.push(b);
        let mut cursor = schedule.cursor();
        for mutation in mutations {
            assert!(matches!(
                cursor.observe(&Record::TimesliceEnd(mutation)),
                Err(Error::Mismatch { index: 0, .. })
            ));
            assert_eq!(cursor.stored_index(), 0);
        }
        cursor.observe(&Record::TimesliceEnd(original)).unwrap();
        cursor.finish().unwrap();
    }

    #[test]
    fn strip_times_retains_payload_and_rip_evidence() {
        let mut schedule = one();
        let before = schedule.clone();
        schedule.strip_times();
        let mut expected = boundary();
        expected.end_time = None;
        assert_eq!(schedule.records(), &[Record::TimesliceEnd(expected)]);
        let mut cursor = schedule.cursor();
        cursor.observe(&before.records()[0]).unwrap();
        cursor.finish().unwrap();
        let mut wrong_rcb = boundary();
        wrong_rcb.identity.committed_rcbs += 1;
        assert!(
            schedule
                .cursor()
                .observe(&Record::TimesliceEnd(wrong_rcb))
                .is_err()
        );
    }

    #[test]
    fn adjacent_zero_rcb_boundaries_are_distinct_and_cannot_be_duplicated() {
        let first = boundary();
        let mut second = first.clone();
        second.identity.slice = 2;
        let schedule = Schedule::new(vec![
            Record::TimesliceEnd(first.clone()),
            Record::TimesliceEnd(second.clone()),
        ])
        .unwrap();
        let mut cursor = schedule.cursor();
        cursor
            .observe(&Record::TimesliceEnd(first.clone()))
            .unwrap();
        assert!(
            cursor
                .observe(&Record::TimesliceEnd(first.clone()))
                .is_err()
        );
        assert_eq!(cursor.stored_index(), 1);
        cursor.observe(&Record::TimesliceEnd(second)).unwrap();
        cursor.finish().unwrap();
        assert!(
            Schedule::new(vec![
                Record::TimesliceEnd(first.clone()),
                Record::TimesliceEnd(first)
            ])
            .is_err()
        );
    }

    #[test]
    fn missing_and_reordered_slices_are_refused_even_with_equal_rcbs() {
        let first = boundary();
        for slice in [0, 1, 3, u64::MAX] {
            let mut second = first.clone();
            second.identity.slice = slice;
            assert!(
                Schedule::new(vec![
                    Record::TimesliceEnd(first.clone()),
                    Record::TimesliceEnd(second)
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn repeated_site_requires_actual_checkin_and_syscall_occurrence() {
        let first = boundary();
        let mut second = first.clone();
        second.identity.slice += 1;
        second.identity.checkin += 1;
        second.identity.entry = Entry::Syscall {
            syscall: Sysno::write,
            occurrence: 8,
        };
        let mut schedule = Schedule::new(vec![
            Record::TimesliceEnd(first.clone()),
            Record::TimesliceEnd(second.clone()),
        ])
        .unwrap();
        schedule.strip_times();
        let mut cursor = schedule.cursor();
        cursor
            .observe(&Record::TimesliceEnd(first.clone()))
            .unwrap();
        let mut stale = first;
        stale.identity.slice = 2;
        assert!(cursor.observe(&Record::TimesliceEnd(stale)).is_err());
        assert_eq!(cursor.stored_index(), 1);
        cursor.observe(&Record::TimesliceEnd(second)).unwrap();
    }

    #[test]
    fn exec_generation_does_not_accept_an_old_boundary() {
        let old = boundary();
        let mut new = old.clone();
        new.identity.owner.address_space_generation += 1;
        new.identity.committed_rcbs = 0;
        new.identity.checkin = 1;
        let schedule = Schedule::new(vec![
            Record::TimesliceEnd(old.clone()),
            Record::TimesliceEnd(new.clone()),
        ])
        .unwrap();
        let mut cursor = schedule.cursor();
        cursor.observe(&Record::TimesliceEnd(old.clone())).unwrap();
        assert!(cursor.observe(&Record::TimesliceEnd(old)).is_err());
        cursor.observe(&Record::TimesliceEnd(new)).unwrap();
        cursor.finish().unwrap();
    }

    #[test]
    fn serialized_stream_cannot_return_to_an_old_generation() {
        let old = boundary();
        let mut new = old.clone();
        new.identity.owner.address_space_generation += 1;
        new.identity.committed_rcbs = 0;
        new.identity.checkin = 1;
        let mut stale = old.clone();
        stale.identity.slice = 2;
        assert!(
            Schedule::new(vec![
                Record::TimesliceEnd(old),
                Record::TimesliceEnd(new),
                Record::TimesliceEnd(stale),
            ])
            .is_err()
        );
    }

    #[test]
    fn recorded_cause_is_not_inferred_from_rip_or_rcb() {
        let mut timer = boundary();
        timer.identity.cause = Cause::PmuTimer;
        timer.identity.entry = Entry::PmuTimer;
        let schedule = Schedule::new(vec![Record::TimesliceEnd(timer)]).unwrap();
        assert!(
            schedule
                .cursor()
                .observe(&Record::TimesliceEnd(boundary()))
                .is_err()
        );
    }

    #[test]
    fn missing_extra_and_skipped_records_cannot_report_completion() {
        assert!(matches!(
            one().cursor().finish(),
            Err(Error::Incomplete {
                index: 0,
                remaining: 1
            })
        ));
        let schedule = one();
        let mut cursor = schedule.cursor();
        cursor.observe(&Record::TimesliceEnd(boundary())).unwrap();
        assert!(matches!(
            cursor.observe(&Record::TimesliceEnd(boundary())),
            Err(Error::Exhausted { index: 1, .. })
        ));
        assert_eq!(cursor.stored_index(), 1);
        let event = Record::Event(SchedEvent::branches(DetTid::from_raw(3), 83));
        let schedule = Schedule::new(vec![
            event.clone(),
            Record::TimesliceEnd(boundary()),
            event.clone(),
        ])
        .unwrap();
        let mut cursor = schedule.cursor();
        cursor.observe(&event).unwrap();
        assert!(cursor.observe(&event).is_err());
        assert_eq!(cursor.stored_index(), 1);
        assert!(cursor.finish().is_err());
    }

    #[test]
    fn stored_indices_include_boundaries() {
        let event = Record::Event(SchedEvent::branches(DetTid::from_raw(3), 83));
        let schedule = Schedule::new(vec![
            event.clone(),
            Record::TimesliceEnd(boundary()),
            event.clone(),
        ])
        .unwrap();
        let mut cursor = schedule.cursor();
        for (index, record) in schedule.records().iter().enumerate() {
            assert_eq!(cursor.stored_index(), index);
            assert_eq!(cursor.expected(), Some(record));
            cursor.observe(record).unwrap();
        }
        assert_eq!(cursor.stored_index(), 3);
        cursor.finish().unwrap();
    }

    #[test]
    fn unsupported_edits_fail_before_output_changes() {
        for mut schedule in [one(), Schedule::new(vec![]).unwrap()] {
            let before = serde_json::to_vec(&schedule).unwrap();
            for edit in [Edit::Split, Edit::Search, Edit::Minimize] {
                assert!(matches!(
                    schedule.edit(edit),
                    Err(Error::UnsupportedEdit { .. })
                ));
                assert_eq!(serde_json::to_vec(&schedule).unwrap(), before);
            }
            assert!(schedule.into_legacy_events().is_err());
        }
    }

    #[test]
    fn legacy_operation_tags_are_unchanged() {
        for (op, json, tag) in [
            (Op::Branch, "\"Branch\"", 0),
            (Op::Rdtsc, "\"Rdtsc\"", 1),
            (Op::Cpuid, "\"Cpuid\"", 2),
            (Op::OtherInstructions, "\"OtherInstructions\"", 4),
        ] {
            assert_eq!(serde_json::to_string(&op).unwrap(), json);
            assert_eq!(
                bincode::serde::encode_to_vec(op, bincode::config::standard()).unwrap(),
                vec![tag]
            );
        }
    }
}
