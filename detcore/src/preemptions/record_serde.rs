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

use super::BackgroundStart;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    post_crash_timeslice: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    post_crash: Option<Vec<crate::post_crash::PostCrashSwitch>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uncapped_slices: Option<u64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rejoins: Option<Vec<crate::scheduler::rejoin_log::Rejoin>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    background_starts: Vec<BackgroundStart>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    rewrite_exclusives_in_memory: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    rows_alone: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    background_starts_unnoted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rewrite_exclusives: Option<super::RewriteExclusives>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    rewrite_exclusives_stripped_heuristic: bool,
}

// The INFO comparison projection: RawRecord's exact serialization except the
// host-observed store-exclusive count, which compares and hashes as equal in
// the record itself and is deliberately excluded from the INFO view (it
// remains in the artifact and full report). Every other field, order, and
// omission matches RawRecord.
#[derive(Serialize)]
#[serde(remote = "PreemptionRecord")]
struct InfoRecord {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    post_crash_timeslice: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    post_crash: Option<Vec<crate::post_crash::PostCrashSwitch>>,
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
    #[serde(skip)]
    store_exclusive_failures: HostObservedCount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    format_version: Option<u32>,
    #[serde(default, skip_serializing_if = "super::lists_no_time_skips")]
    time_skips: Option<Vec<TimeSkip>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rejoins: Option<Vec<crate::scheduler::rejoin_log::Rejoin>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    background_starts: Vec<BackgroundStart>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    rewrite_exclusives_in_memory: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    rows_alone: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    background_starts_unnoted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rewrite_exclusives: Option<super::RewriteExclusives>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    rewrite_exclusives_stripped_heuristic: bool,
}

struct InfoLegacyFields<'a>(&'a PreemptionRecord);

impl Serialize for InfoLegacyFields<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        InfoRecord::serialize(self.0, serializer)
    }
}

/// A `PreemptionRecord` as the INFO comparison sees it: its exact
/// serialization, marked envelope included, minus the host-observed
/// store-exclusive count.
pub(super) struct InfoProjection<'a>(pub(super) &'a PreemptionRecord);

impl Serialize for InfoProjection<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Some(replay_departed_at) = self.0.replay_departed_at else {
            return InfoRecord::serialize(self.0, serializer);
        };
        if !serializer.is_human_readable() {
            return Err(serde::ser::Error::custom(
                "marked preemption records require the incomplete_preemption_record JSON envelope",
            ));
        }
        #[derive(Serialize)]
        struct InfoMarkedRef<'a> {
            version: u32,
            replay_departed_at: u64,
            record: InfoLegacyFields<'a>,
        }
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(
            ENVELOPE,
            &InfoMarkedRef {
                version: VERSION,
                replay_departed_at,
                record: InfoLegacyFields(self.0),
            },
        )?;
        map.end()
    }
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

    /// The rule that registered queued sleeps (`--sleep-registration`) is a
    /// run setting, kept by the raw record under its old attributes: a record
    /// written before the field existed reads as `when-chosen` and writes back
    /// byte for byte, a `first-pass` record writes the field and reads it
    /// back, and an incomplete record keeps it inside its envelope's payload.
    /// So a reader of the marked record, of its settings, of a copy, or of a
    /// file holding it never takes the `when-chosen` default for a
    /// `first-pass` run, which would replay its timed wakes in other passes.
    #[test]
    fn the_sleep_registration_rule_survives_the_raw_record_and_the_envelope() {
        let old = r#"{"per_thread":{},"global":[]}"#;
        let record: PreemptionRecord = serde_json::from_str(old).unwrap();
        assert!(!record.queued_sleeps_registered_at_first_pass());
        assert_eq!(serde_json::to_string(&record).unwrap(), old);
        let written =
            serde_json::to_string(&record.with_queued_sleeps_registered_at_first_pass(true))
                .unwrap();
        assert_eq!(
            written,
            r#"{"per_thread":{},"global":[],"queued_sleeps_registered_at_first_pass":true}"#
        );
        let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
        assert!(back.queued_sleeps_registered_at_first_pass());

        let directory = tempfile::tempdir().unwrap();
        for first_pass in [false, true] {
            let mut writer =
                PreemptionWriter::new(None).with_queued_sleeps_registered_at_first_pass(first_pass);
            writer.mark_replay_departed(7);
            let text = writer.into_string();
            let envelope: serde_json::Value = serde_json::from_str(&text).unwrap();
            let payload = &envelope[ENVELOPE]["record"];
            assert_eq!(
                payload.get("queued_sleeps_registered_at_first_pass"),
                first_pass.then_some(&serde_json::Value::Bool(true)),
                "{text}"
            );
            let marked: PreemptionRecord = serde_json::from_str(&text).unwrap();
            assert_eq!(marked.replay_departed_at(), Some(7));
            assert_eq!(marked.queued_sleeps_registered_at_first_pass(), first_pass);
            assert_eq!(
                serde_json::from_str::<PreemptionRecord>(&serde_json::to_string(&marked).unwrap())
                    .unwrap(),
                marked
            );
            assert_eq!(
                settings_value(&marked).unwrap()["queued_sleeps_registered_at_first_pass"]
                    .as_bool()
                    .unwrap_or(false),
                first_pass
            );
            for copy in [
                marked.clone_preemptions_only(),
                marked.schedule_prefix(0),
                marked.normalize(),
                marked.with_schedule(vec![]).unwrap(),
            ] {
                assert_eq!(copy.queued_sleeps_registered_at_first_pass(), first_pass);
            }
            for boundaries in [false, true] {
                let recording = if boundaries {
                    Recording::Boundaries(
                        BoundaryRecording::new(
                            marked.clone(),
                            detcore_model::schedule::boundary::Schedule::new(vec![]).unwrap(),
                        )
                        .unwrap(),
                    )
                } else {
                    Recording::Legacy(marked.clone())
                };
                let path = directory
                    .path()
                    .join(format!("{boundaries}-{first_pass}.json"));
                recording.write(&path).unwrap();
                assert_eq!(read_recorded_replay_departure(&path).unwrap(), Some(7));
                assert_eq!(
                    crate::preemptions::read_recorded_queued_sleeps_registered_at_first_pass(&path)
                        .unwrap(),
                    first_pass
                );
                let stripped =
                    crate::preemptions::strip_times_from_events_file(&path, None).unwrap();
                assert_eq!(
                    crate::preemptions::read_recorded_queued_sleeps_registered_at_first_pass(
                        &stripped
                    )
                    .unwrap(),
                    first_pass
                );
            }
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

    /// `rejoins` (rrnewton/hermit#4751) reads back from a flat record written
    /// before the record's serde moved into `RawRecord`, and survives writing
    /// and reading the current form, plain and inside the incomplete-record
    /// envelope, and the whole record compares equal each time. So do the
    /// popped events a readmission names: an empty list
    /// is written, so that it stays apart from a readmission that names none
    /// (`None`, as in older records), which is not. Likewise a record whose
    /// run logged no readmission keeps its empty list, apart from a record
    /// written before the field existed, which reads as having none.
    #[test]
    fn rejoins_round_trip_through_the_record_serde() {
        let old =
            r#"{"per_thread":{},"global":[],"rejoins":[{"turn":4,"wakes":1,"calls":[[3,2]]}]}"#;
        let record: PreemptionRecord = serde_json::from_str(old).unwrap();
        let expected = vec![crate::scheduler::rejoin_log::Rejoin {
            turn: 4,
            wakes: 1,
            calls: vec![(DetTid::from_raw(3), 2)],
            popped: None,
        }];
        assert_eq!(record.rejoins.as_ref(), Some(&expected));
        let mut whole = PreemptionRecord::from_sched_events(Vec::new());
        whole.rejoins = Some(expected.clone());
        assert_eq!(record, whole);
        for marker in [None, Some(7)] {
            let mut copy = record.clone();
            copy.replay_departed_at = marker;
            let written = serde_json::to_string(&copy).unwrap();
            let read: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(read.rejoins.as_ref(), Some(&expected), "{written}");
            assert_eq!(read.replay_departed_at(), marker, "{written}");
            assert_eq!(read, copy, "{written}");
        }

        use crate::scheduler::rejoin_log::PoppedEvent;
        use crate::scheduler::rejoin_log::TimedEventId;
        let mut named = record;
        named.rejoins = Some(vec![
            crate::scheduler::rejoin_log::Rejoin {
                popped: Some(vec![
                    PoppedEvent {
                        event: TimedEventId::Thread(DetTid::from_raw(5)),
                        deadline: crate::types::LogicalTime::from_nanos(1_000),
                    },
                    PoppedEvent {
                        event: TimedEventId::Posix(crate::types::DetPid::from_raw(3), 1),
                        deadline: crate::types::LogicalTime::from_nanos(2_500),
                    },
                ]),
                ..expected[0].clone()
            },
            crate::scheduler::rejoin_log::Rejoin {
                turn: 5,
                popped: Some(Vec::new()),
                ..expected[0].clone()
            },
            crate::scheduler::rejoin_log::Rejoin {
                turn: 6,
                ..expected[0].clone()
            },
        ]);
        for marker in [None, Some(7)] {
            let mut copy = named.clone();
            copy.replay_departed_at = marker;
            let written = serde_json::to_string(&copy).unwrap();
            assert_eq!(written.matches(r#""popped":"#).count(), 2, "{written}");
            let read: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(read.rejoins, named.rejoins, "{written}");
            assert_eq!(read, copy, "{written}");
        }

        let fresh = PreemptionWriter::new(None).into_string();
        assert!(fresh.contains(r#""rejoins":[]"#), "{fresh}");
        let mut logged_none = named;
        logged_none.rejoins = Some(Vec::new());
        for marker in [None, Some(7)] {
            let mut copy = logged_none.clone();
            copy.replay_departed_at = marker;
            let written = serde_json::to_string(&copy).unwrap();
            assert!(written.contains(r#""rejoins":[]"#), "{written}");
            let read: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(read.rejoins, Some(Vec::new()), "{written}");
            assert_eq!(read, copy, "{written}");
        }
        for older in [
            r#"{"per_thread":{},"global":[]}"#,
            r#"{"incomplete_preemption_record":{"version":1,"replay_departed_at":0,"record":{"per_thread":{},"global":[]}}}"#,
        ] {
            let read: PreemptionRecord = serde_json::from_str(older).unwrap();
            assert_eq!(read.rejoins, None, "{older}");
            let mut whole = PreemptionRecord::from_sched_events(Vec::new());
            whole.replay_departed_at = read.replay_departed_at;
            assert_eq!(read, whole, "{older}");
        }
    }

    /// A flat record written before the RawRecord remote existed (2c8e0352f)
    /// keeps `post_crash_timeslice`: read from that JSON, written and read
    /// again, plain and inside the incomplete-record envelope. A field
    /// missing from RawRecord would be dropped silently here.
    #[test]
    fn a_flat_records_post_crash_timeslice_survives_plain_and_marked_round_trips() {
        let flat = r#"{"per_thread":{},"global":[],"post_crash_timeslice":200000000}"#;
        let record: PreemptionRecord = serde_json::from_str(flat).unwrap();
        assert_eq!(record.post_crash_timeslice(), Some(200_000_000));
        let plain = serde_json::to_string(&record).unwrap();
        assert_eq!(plain, flat);
        assert_eq!(
            serde_json::from_str::<PreemptionRecord>(&plain).unwrap(),
            record
        );
        let mut marked = record.clone();
        marked.replay_departed_at = Some(7);
        let enveloped = serde_json::to_string(&marked).unwrap();
        assert!(
            enveloped.starts_with(r#"{"incomplete_preemption_record":"#),
            "{enveloped}"
        );
        assert!(
            enveloped.contains(r#""post_crash_timeslice":200000000"#),
            "{enveloped}"
        );
        let back: PreemptionRecord = serde_json::from_str(&enveloped).unwrap();
        assert_eq!(back.post_crash_timeslice(), Some(200_000_000));
        assert_eq!(back.replay_departed_at(), Some(7));
        assert_eq!(back, marked);
    }

    /// The same for `post_crash`, both a list of switches and the empty list
    /// that says a run with the switch on took none (absent means a record
    /// that lists nothing).
    #[test]
    fn a_flat_records_post_crash_switches_survive_plain_and_marked_round_trips() {
        let switch = crate::post_crash::PostCrashSwitch {
            dtid: DetTid::from_raw(3),
            signal: 6,
            exec_by: Some(DetTid::from_raw(4)),
            turn: 63906,
            bound: 200_000_000,
        };
        for (flat, expected) in [
            (
                r#"{"per_thread":{},"global":[],"post_crash":[{"dtid":3,"signal":6,"exec_by":4,"turn":63906,"bound":200000000}]}"#,
                vec![switch],
            ),
            (r#"{"per_thread":{},"global":[],"post_crash":[]}"#, vec![]),
        ] {
            let record: PreemptionRecord = serde_json::from_str(flat).unwrap();
            assert_eq!(record.post_crash_complete(), Some(&expected[..]), "{flat}");
            let plain = serde_json::to_string(&record).unwrap();
            assert_eq!(plain, flat);
            assert_eq!(
                serde_json::from_str::<PreemptionRecord>(&plain).unwrap(),
                record
            );
            let mut marked = record.clone();
            marked.replay_departed_at = Some(7);
            let enveloped = serde_json::to_string(&marked).unwrap();
            assert!(
                enveloped.starts_with(r#"{"incomplete_preemption_record":"#),
                "{enveloped}"
            );
            let back: PreemptionRecord = serde_json::from_str(&enveloped).unwrap();
            assert_eq!(
                back.post_crash_complete(),
                Some(&expected[..]),
                "{enveloped}"
            );
            assert_eq!(back.replay_departed_at(), Some(7));
            assert_eq!(back, marked);
        }
    }

    /// `format_version` (rrnewton/hermit#4847) reads back from a flat record
    /// written before the record's serde moved into `RawRecord`, and survives
    /// writing and reading the current form, plain and inside the
    /// incomplete-record envelope, and the whole record compares equal each
    /// time.
    #[test]
    fn format_version_round_trips_through_the_record_serde() {
        let format = crate::preemptions::PREEMPTION_RECORD_FORMAT_MAX;
        let old = format!(r#"{{"per_thread":{{}},"global":[],"format_version":{format}}}"#);
        let record: PreemptionRecord = serde_json::from_str(&old).unwrap();
        assert_eq!(record.format_version, Some(format));
        let mut whole = PreemptionRecord::from_sched_events(Vec::new());
        whole.format_version = Some(format);
        assert_eq!(record, whole);
        for marker in [None, Some(7)] {
            let mut copy = record.clone();
            copy.replay_departed_at = marker;
            let written = serde_json::to_string(&copy).unwrap();
            let read: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(read.format_version, Some(format), "{written}");
            assert_eq!(read.replay_departed_at(), marker, "{written}");
            assert_eq!(read, copy, "{written}");
        }
    }

    /// `background_starts` (rrnewton/hermit#4847) reads back from a flat
    /// record written before the record's serde moved into `RawRecord`, and
    /// survives writing and reading the current form, plain and inside the
    /// incomplete-record envelope, and the whole record compares equal each
    /// time.
    #[test]
    fn background_starts_round_trip_through_the_record_serde() {
        let old = r#"{"per_thread":{},"global":[],"background_starts":[{"after":1,"thread":3,"call":2}]}"#;
        let record: PreemptionRecord = serde_json::from_str(old).unwrap();
        let expected = vec![BackgroundStart {
            after: 1,
            thread: DetTid::from_raw(3),
            call: 2,
        }];
        assert_eq!(record.background_starts, expected);
        let mut whole = PreemptionRecord::from_sched_events(Vec::new());
        whole.background_starts = expected.clone();
        assert_eq!(record, whole);
        for marker in [None, Some(7)] {
            let mut copy = record.clone();
            copy.replay_departed_at = marker;
            let written = serde_json::to_string(&copy).unwrap();
            let read: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(read.background_starts, expected, "{written}");
            assert_eq!(read.replay_departed_at(), marker, "{written}");
            assert_eq!(read, copy, "{written}");
        }
    }

    /// `rewrite_exclusives_in_memory` rides in `RawRecord`: a plain record
    /// written before the envelope reads it back, a plain record written now
    /// keeps its bytes, and a marked record keeps the setting inside its
    /// envelope.
    #[test]
    fn the_in_memory_exclusive_rewrite_survives_the_record_serde() {
        let old = r#"{"per_thread":{},"global":[],"rewrite_exclusives_in_memory":true}"#;
        let record: PreemptionRecord = serde_json::from_str(old).unwrap();
        assert!(record.rewrite_exclusives_in_memory());
        assert_eq!(serde_json::to_string(&record).unwrap(), old);
        let mut writer = PreemptionWriter::new(None).with_rewrite_exclusives_in_memory(true);
        writer.mark_replay_departed(7);
        let marked = writer.into_string();
        assert!(marked.contains(ENVELOPE), "{marked}");
        let back: PreemptionRecord = serde_json::from_str(&marked).unwrap();
        assert!(back.rewrite_exclusives_in_memory());
        assert_eq!(
            back.rewrite_exclusives(),
            Some(crate::preemptions::RewriteExclusives::current(
                crate::preemptions::RewriteExclusivesMode::Memory
            )),
            "{marked}"
        );
        assert_eq!(back.replay_departed_at(), Some(7));

        // The INFO projection is RawRecord's exact serialization minus the
        // host-observed store-exclusive count, so it carries the two marks
        // RawRecord writes after `rewrite_exclusives_in_memory`: a record
        // differing only in either mark projects to other JSON, and so to
        // another summary digest and INFO comparison.
        let plain = PreemptionRecord::default();
        assert_eq!(
            serde_json::to_string(&InfoProjection(&plain)).unwrap(),
            r#"{"per_thread":{},"global":[]}"#
        );
        let mut alone = plain.clone();
        alone.rows_alone = true;
        assert_eq!(
            serde_json::to_string(&alone).unwrap(),
            r#"{"per_thread":{},"global":[],"rows_alone":true}"#
        );
        assert_eq!(
            serde_json::to_string(&InfoProjection(&alone)).unwrap(),
            r#"{"per_thread":{},"global":[],"rows_alone":true}"#
        );
        let mut unnoted = plain.clone();
        unnoted.background_starts_unnoted = true;
        assert_eq!(
            serde_json::to_string(&unnoted).unwrap(),
            r#"{"per_thread":{},"global":[],"background_starts_unnoted":true}"#
        );
        assert_eq!(
            serde_json::to_string(&InfoProjection(&unnoted)).unwrap(),
            r#"{"per_thread":{},"global":[],"background_starts_unnoted":true}"#
        );
        every_record_field_is_named_for_both_mirrors(plain);
    }

    /// Exhaustive on purpose (no `..`): the next field added to
    /// `PreemptionRecord` fails to compile here until it is mirrored in
    /// `RawRecord` and, unless it is the host-observed count the INFO view
    /// excludes, in `InfoRecord` too.
    fn every_record_field_is_named_for_both_mirrors(record: PreemptionRecord) {
        let PreemptionRecord {
            per_thread: _,
            global: _,
            epoch: _,
            pipe_wake_latency: _,
            signal_state_copy: _,
            cap_preemption_charge: _,
            chaos_timeslice: _,
            guest_ids: _,
            replay_departed_at: _,
            reserved_input: _,
            post_crash_timeslice: _,
            post_crash: _,
            callback_checkpoints: _,
            rng_seed: _,
            expired_wait_runs_charged_once: _,
            host_inputs: _,
            queued_sleeps_registered_at_first_pass: _,
            store_exclusive_failures: _,
            format_version: _,
            time_skips: _,
            rejoins: _,
            background_starts: _,
            rewrite_exclusives_in_memory: _,
            rows_alone: _,
            background_starts_unnoted: _,
            rewrite_exclusives: _,
            rewrite_exclusives_stripped_heuristic: _,
        } = record;
        // A stated overlay provenance round-trips byte for byte, as the
        // last field of the raw form.
        let overlay = r#"{"per_thread":{},"global":[],"rewrite_exclusives":{"mode":"overlay","format_version":6}}"#;
        let record: PreemptionRecord = serde_json::from_str(overlay).unwrap();
        assert_eq!(
            record.rewrite_exclusives(),
            Some(crate::preemptions::RewriteExclusives {
                mode: crate::preemptions::RewriteExclusivesMode::Overlay,
                format_version: 6,
            })
        );
        assert!(!record.rewrite_exclusives_in_memory());
        assert_eq!(serde_json::to_string(&record).unwrap(), overlay);
    }

    /// The INFO projection (`InfoRecord`) must carry the rewrite provenance
    /// (`rewrite_exclusives`, `rewrite_exclusives_stripped_heuristic`) in
    /// legacy, marked and boundary forms, as `RawRecord` does. Before the
    /// fix the projection ended at `rewrite_exclusives_in_memory` and both
    /// fields were dropped, so compared summaries collapsed. (PR 4735 P2.)
    #[test]
    fn info_projection_carries_rewrite_provenance_in_every_form() {
        use crate::preemptions::RewriteExclusives;
        use crate::preemptions::RewriteExclusivesMode;
        let overlay6 = RewriteExclusives {
            mode: RewriteExclusivesMode::Overlay,
            format_version: 6,
        };
        let base: PreemptionRecord = serde_json::from_str(
            r#"{"per_thread":{},"global":[],"rewrite_exclusives":{"mode":"overlay","format_version":6},"rewrite_exclusives_stripped_heuristic":true}"#,
        )
        .unwrap();
        assert_eq!(base.rewrite_exclusives(), Some(overlay6));
        assert!(base.rewrite_exclusives_stripped_heuristic());

        // Legacy INFO form carries both fields.
        let legacy_info = serde_json::to_string(&InfoProjection(&base)).unwrap();
        assert!(
            legacy_info.contains(r#""rewrite_exclusives":{"mode":"overlay","format_version":6}"#),
            "{legacy_info}"
        );
        assert!(
            legacy_info.contains(r#""rewrite_exclusives_stripped_heuristic":true"#),
            "{legacy_info}"
        );
        let back: PreemptionRecord = serde_json::from_str(&legacy_info).unwrap();
        assert_eq!(back, base);

        // Off vs Overlay vs differing versions vs heuristic differ in INFO form.
        let off: PreemptionRecord =
            serde_json::from_str(r#"{"per_thread":{},"global":[]}"#).unwrap();
        let off_info = serde_json::to_string(&InfoProjection(&off)).unwrap();
        assert_ne!(off_info, legacy_info);
        let v5: PreemptionRecord = serde_json::from_str(
            r#"{"per_thread":{},"global":[],"rewrite_exclusives":{"mode":"overlay","format_version":5}}"#,
        )
        .unwrap();
        assert_ne!(
            serde_json::to_string(&InfoProjection(&v5)).unwrap(),
            legacy_info
        );
        // Heuristic alone (no rewrite) still differs.
        let heur_only: PreemptionRecord = serde_json::from_str(
            r#"{"per_thread":{},"global":[],"rewrite_exclusives_stripped_heuristic":true}"#,
        )
        .unwrap();
        assert_ne!(
            off_info,
            serde_json::to_string(&InfoProjection(&heur_only)).unwrap()
        );

        // Marked INFO form carries both fields inside its envelope payload.
        let mut marked = base.clone();
        marked.replay_departed_at = Some(7);
        let marked_info = serde_json::to_string(&InfoProjection(&marked)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&marked_info).unwrap();
        let payload = &value[ENVELOPE]["record"];
        assert_eq!(payload["rewrite_exclusives"]["mode"], "overlay");
        assert_eq!(payload["rewrite_exclusives"]["format_version"], 6);
        assert_eq!(payload["rewrite_exclusives_stripped_heuristic"], true);
        let marked_back: PreemptionRecord = serde_json::from_str(&marked_info).unwrap();
        assert_eq!(marked_back.rewrite_exclusives(), Some(overlay6));
        assert!(marked_back.rewrite_exclusives_stripped_heuristic());

        // Boundary INFO form carries both fields.
        let boundary_info = serde_json::to_string(
            &crate::preemptions::boundary_recording::BorrowedInfoEnvelope::new(&base, &[]).unwrap(),
        )
        .unwrap();
        assert!(
            boundary_info.contains(r#""rewrite_exclusives":{"mode":"overlay","format_version":6}"#),
            "{boundary_info}"
        );
        assert!(
            boundary_info.contains(r#""rewrite_exclusives_stripped_heuristic":true"#),
            "{boundary_info}"
        );
        let boundary_off = serde_json::to_string(
            &crate::preemptions::boundary_recording::BorrowedInfoEnvelope::new(&off, &[]).unwrap(),
        )
        .unwrap();
        assert_ne!(boundary_off, boundary_info);
    }

    /// The uncapped-slices mark is a record field main's serde must list: a
    /// record in the plain form written before this serde, with the field,
    /// reads back into it, and a plain record and a marked envelope written
    /// now keep it.
    #[test]
    fn the_uncapped_slices_mark_round_trips_through_the_record_serde() {
        let old: PreemptionRecord =
            serde_json::from_str(r#"{"per_thread":{},"global":[],"uncapped_slices":9400000}"#)
                .unwrap();
        assert_eq!(old.uncapped_slices, Some(9_400_000));
        let mut marked = old.clone();
        marked.replay_departed_at = Some(3);
        for record in [old, marked] {
            let text = serde_json::to_string(&record).unwrap();
            let back: PreemptionRecord = serde_json::from_str(&text).unwrap();
            assert_eq!(back.uncapped_slices, Some(9_400_000), "{text}");
            assert_eq!(back, record, "{text}");
            assert_eq!(back.replay_departed_at, record.replay_departed_at, "{text}");
        }
    }
}
