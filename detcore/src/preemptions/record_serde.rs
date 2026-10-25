/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Preserve legacy encoding, but never let an old reader ignore known omissions.

use std::collections::BTreeMap;
use std::fmt;

use chrono::DateTime;
use chrono::Utc;
use detcore_model::host_input::HostInputSet;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeSeed;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde::de::value::MapAccessDeserializer;
use serde::de::value::SeqAccessDeserializer;
use serde::de::value::StringDeserializer;
use serde::ser::SerializeMap;

use super::HostObservedCount;
use super::PreemptionRecord;
use super::ReservedInputMetadata;
use super::ThreadHistory;
use super::TimeSkip;
use super::callback_history;
use crate::types::DetTid;
use crate::types::SchedEvent;

const ENVELOPE: &str = "incomplete_preemption_record";
const DEPARTURE: &str = "replay_departed_at";
const VERSION: u32 = 1;

// A private remote definition keeps the old field order/omissions without
// exposing a public raw serializer that could bypass the marked envelope.
#[derive(Serialize, Deserialize)]
#[serde(remote = "PreemptionRecord")]
struct RawRecord {
    per_thread: BTreeMap<DetTid, ThreadHistory>,
    global: Vec<SchedEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    epoch: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pipe_wake_latency: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signal_state_copy: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    cap_preemption_charge: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chaos_timeslice: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    guest_ids: Option<detcore_model::pid::GuestIds>,
    #[serde(default, skip_serializing_if = "ReservedInputMetadata::is_missing")]
    reserved_input: ReservedInputMetadata,
    #[serde(skip)]
    replay_departed_at: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    callback_checkpoints: BTreeMap<DetTid, Vec<callback_history::Checkpoint>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rng_seed: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    expired_wait_runs_charged_once: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_inputs: Option<HostInputSet>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    queued_sleeps_registered_at_first_pass: bool,
    #[serde(default, skip_serializing_if = "HostObservedCount::is_zero")]
    store_exclusive_failures: HostObservedCount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    format_version: Option<u32>,
    #[serde(default, skip_serializing_if = "super::lists_no_time_skips")]
    time_skips: Option<Vec<TimeSkip>>,
}

struct LegacyFields<'a>(&'a PreemptionRecord);

impl Serialize for LegacyFields<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        RawRecord::serialize(self.0, serializer)
    }
}

#[derive(Serialize)]
struct MarkedRef<'a> {
    version: u32,
    replay_departed_at: u64,
    record: LegacyFields<'a>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Marked {
    version: u32,
    replay_departed_at: u64,
    #[serde(deserialize_with = "deserialize_payload")]
    record: PreemptionRecord,
}

impl Serialize for PreemptionRecord {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Some(replay_departed_at) = self.replay_departed_at else {
            return RawRecord::serialize(self, serializer);
        };
        // Fail before invoking any output-producing serializer operation.
        // There is no versioned binary envelope for this standalone record.
        if !serializer.is_human_readable() {
            return Err(serde::ser::Error::custom(
                "marked preemption records require the incomplete_preemption_record JSON envelope",
            ));
        }
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(
            ENVELOPE,
            &MarkedRef {
                version: VERSION,
                replay_departed_at,
                record: LegacyFields(self),
            },
        )?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for PreemptionRecord {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if !deserializer.is_human_readable() {
            // Keep the old positional decoder, including its pre-existing
            // limitations for fields omitted by conditional serialization.
            return RawRecord::deserialize(deserializer);
        }
        deserializer.deserialize_any(RecordVisitor)
    }
}

struct RecordVisitor;

impl<'de> Visitor<'de> for RecordVisitor {
    type Value = PreemptionRecord;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a legacy preemption record or an incomplete-preemption envelope")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let first = map.next_key::<String>()?;
        if first.as_deref() == Some(ENVELOPE) {
            let marked: Marked = map.next_value()?;
            if map.next_key::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom(
                    "an incomplete-preemption envelope cannot contain other top-level fields",
                ));
            }
            if marked.version != VERSION {
                return Err(serde::de::Error::custom(format!(
                    "unsupported incomplete_preemption_record version {}",
                    marked.version
                )));
            }
            let mut record = marked.record;
            record.replay_departed_at = Some(marked.replay_departed_at);
            Ok(record)
        } else {
            legacy_map(map, first, true)
        }
    }

    fn visit_seq<A: SeqAccess<'de>>(self, sequence: A) -> Result<Self::Value, A::Error> {
        // Preserve the old unmarked positional human-readable representation.
        RawRecord::deserialize(SeqAccessDeserializer::new(sequence))
    }
}

// An envelope's payload must be genuinely raw: neither a nested envelope nor
// a flat marker (even null) may override or obscure its authoritative marker.
fn deserialize_payload<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<PreemptionRecord, D::Error> {
    struct Payload;
    impl<'de> Visitor<'de> for Payload {
        type Value = PreemptionRecord;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("unmarked legacy fields inside the envelope")
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            legacy_map(map, None, false)
        }
    }
    deserializer.deserialize_map(Payload)
}

fn legacy_map<'de, A: MapAccess<'de>>(
    map: A,
    first: Option<String>,
    allow_flat_marker: bool,
) -> Result<PreemptionRecord, A::Error> {
    let mut departure = None;
    let mut seen = false;
    let mut record = RawRecord::deserialize(MapAccessDeserializer::new(LegacyMap {
        map,
        first,
        allow_flat_marker,
        departure: &mut departure,
        seen: &mut seen,
    }))?;
    record.replay_departed_at = departure;
    Ok(record)
}

struct LegacyMap<'a, A> {
    map: A,
    first: Option<String>,
    allow_flat_marker: bool,
    departure: &'a mut Option<u64>,
    seen: &'a mut bool,
}

impl<'de, A: MapAccess<'de>> MapAccess<'de> for LegacyMap<'_, A> {
    type Error = A::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        loop {
            let key = match self.first.take() {
                Some(key) => Some(key),
                None => self.map.next_key::<String>()?,
            };
            let Some(key) = key else { return Ok(None) };
            if key == ENVELOPE {
                return Err(serde::de::Error::custom(
                    "an incomplete-preemption envelope cannot be mixed with legacy fields",
                ));
            }
            if key != DEPARTURE {
                return seed.deserialize(StringDeserializer::new(key)).map(Some);
            }
            if !self.allow_flat_marker {
                return Err(serde::de::Error::custom(
                    "an envelope payload cannot contain replay_departed_at",
                ));
            }
            if *self.seen {
                return Err(serde::de::Error::duplicate_field("replay_departed_at"));
            }
            *self.seen = true;
            // Historical flat marked JSON is read, but its omission fact is
            // retained and faithful admission rejects it. New writes never
            // emit that old-reader-unsafe representation.
            *self.departure = self.map.next_value()?;
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        self.map.next_value_seed(seed)
    }
}

/// Internal metadata readers need both the legacy settings and the envelope's
/// marker. This value never becomes a stored record or a public raw serializer.
pub(super) fn settings_value(record: &PreemptionRecord) -> serde_json::Result<serde_json::Value> {
    let mut value = serde_json::to_value(LegacyFields(record))?;
    if let Some(event) = record.replay_departed_at {
        value[DEPARTURE] = event.into();
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preemptions::PreemptionWriter;
    use crate::preemptions::boundary_recording::BoundaryRecording;
    use crate::preemptions::boundary_recording::Recording;
    use crate::preemptions::read_recorded_replay_departure;

    #[test]
    fn marked_envelopes_never_fall_back_to_legacy_fields() {
        let raw = r#"{"per_thread":{},"global":[]}"#;
        let valid = format!(r#"{{"version":1,"replay_departed_at":7,"record":{raw}}}"#);
        let envelope = |body: &str| format!(r#"{{"incomplete_preemption_record":{body}}}"#);
        let marked = envelope(&valid);
        assert_eq!(
            serde_json::from_str::<PreemptionRecord>(&marked)
                .unwrap()
                .replay_departed_at(),
            Some(7)
        );
        let bad = [
            envelope("null"),
            envelope(&valid.replace("\"version\":1", "\"version\":2")),
            envelope(&valid.replace("\"version\":1,", "")),
            envelope(&valid.replace("\"replay_departed_at\":7,", "")),
            envelope(&valid.replace("\"replay_departed_at\":7", "\"replay_departed_at\":null")),
            envelope(&valid.replace("\"version\":1", "\"version\":1,\"version\":1")),
            envelope(&valid.replace(
                "\"replay_departed_at\":7",
                "\"replay_departed_at\":7,\"replay_departed_at\":9",
            )),
            format!(
                r#"{{"incomplete_preemption_record":{valid},"incomplete_preemption_record":{valid}}}"#
            ),
            format!(r#"{{"incomplete_preemption_record":{valid},"per_thread":{{}},"global":[]}}"#),
            format!(r#"{{"per_thread":{{}},"global":[],"incomplete_preemption_record":{valid}}}"#),
            r#"{"per_thread":{},"global":[],"incomplete_preemption_record":null}"#.to_owned(),
            envelope(&valid.replace(raw, &marked)),
            envelope(&valid.replace(
                raw,
                r#"{"per_thread":{},"global":[],"replay_departed_at":7}"#,
            )),
            envelope(&valid.replace(
                raw,
                r#"{"per_thread":{},"global":[],"replay_departed_at":null}"#,
            )),
            r#"{"per_thread":{},"global":[],"replay_departed_at":7,"replay_departed_at":9}"#
                .to_owned(),
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        for input in bad {
            assert!(
                serde_json::from_str::<PreemptionRecord>(&input).is_err(),
                "{input}"
            );
            assert!(Recording::decode(input.as_bytes()).is_err(), "{input}");
            // Every malformed wrapper also fails metadata unwrapping. Flat
            // legacy metadata retains its historical individual-field contract.
            if input.contains(ENVELOPE) {
                std::fs::write(&path, &input).unwrap();
                assert!(
                    crate::preemptions::read_recorded_epoch(&path).is_err(),
                    "{input}"
                );
                assert!(read_recorded_replay_departure(&path).is_err(), "{input}");
            }
        }
    }

    #[test]
    fn historical_flat_departures_are_retained_without_restricting_ordinary_unknown_fields() {
        let ordinary = r#"{"per_thread":{},"global":[],"future_setting":{"x":1}}"#;
        let ordinary: PreemptionRecord = serde_json::from_str(ordinary).unwrap();
        ordinary.require_faithful_replay().unwrap();
        assert_eq!(
            serde_json::to_string(&ordinary).unwrap(),
            r#"{"per_thread":{},"global":[]}"#
        );
        for input in [
            r#"{"replay_departed_at":0,"per_thread":{},"global":[]}"#,
            r#"{"per_thread":{},"global":[],"replay_departed_at":0}"#,
        ] {
            let record: PreemptionRecord = serde_json::from_str(input).unwrap();
            assert_eq!(record.replay_departed_at(), Some(0));
            assert!(record.require_faithful_replay().is_err());
            let rewritten = serde_json::to_string(&record).unwrap();
            assert_eq!(
                rewritten,
                r#"{"incomplete_preemption_record":{"version":1,"replay_departed_at":0,"record":{"per_thread":{},"global":[]}}}"#
            );
            assert_eq!(
                serde_json::from_str::<PreemptionRecord>(&rewritten).unwrap(),
                record
            );
        }
    }

    #[test]
    fn wrapped_settings_and_file_transforms_retain_the_authoritative_departure() {
        let directory = tempfile::tempdir().unwrap();
        for marker in [None, Some(7)] {
            let mut writer = PreemptionWriter::new(None)
                .with_epoch("2001-02-03T04:05:06Z".parse().unwrap())
                .with_pipe_wake_latency(Some(0))
                .with_signal_state_copy(false)
                .with_rng_seed(31)
                .with_expired_wait_runs_charged_once(true)
                .with_cap_preemption_charge(true)
                .with_chaos_timeslice(std::num::NonZeroU64::new(1000));
            if let Some(event) = marker {
                writer.mark_replay_departed(event);
            }
            let record: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
            for boundaries in [false, true] {
                let recording = if boundaries {
                    Recording::Boundaries(
                        BoundaryRecording::new(
                            record.clone(),
                            detcore_model::schedule::boundary::Schedule::new(vec![]).unwrap(),
                        )
                        .unwrap(),
                    )
                } else {
                    Recording::Legacy(record.clone())
                };
                let path = directory
                    .path()
                    .join(format!("{boundaries}-{marker:?}.json"));
                recording.write(&path).unwrap();
                assert_eq!(read_recorded_replay_departure(&path).unwrap(), marker);
                assert_eq!(
                    crate::preemptions::read_recorded_epoch(&path).unwrap(),
                    record.epoch()
                );
                assert_eq!(
                    crate::preemptions::read_recorded_pipe_wake_latency(&path).unwrap(),
                    Some(0)
                );
                assert_eq!(
                    crate::preemptions::read_recorded_signal_state_copy(&path).unwrap(),
                    Some(false)
                );
                assert!(
                    crate::preemptions::read_recorded_expired_wait_runs_charged_once(&path)
                        .unwrap()
                );
                assert!(crate::preemptions::read_recorded_cap_preemption_charge(&path).unwrap());
                assert_eq!(
                    crate::preemptions::read_recorded_chaos_timeslice(&path).unwrap(),
                    Some(1000)
                );
                let stripped =
                    crate::preemptions::strip_times_from_events_file(&path, None).unwrap();
                assert_eq!(Recording::read(&stripped).unwrap(), recording);
                assert_eq!(read_recorded_replay_departure(&stripped).unwrap(), marker);
                assert_eq!(
                    Recording::read(&path)
                        .unwrap()
                        .require_faithful_replay()
                        .is_err(),
                    marker.is_some()
                );
            }
            for copy in [
                record.clone(),
                record.clone_preemptions_only(),
                record.schedule_prefix(0),
                record.normalize(),
                record.with_schedule(vec![]).unwrap(),
            ] {
                assert_eq!(copy.replay_departed_at(), marker);
                assert_eq!(
                    copy.retain_replay_departure(None).replay_departed_at(),
                    marker
                );
            }
        }
    }
}
