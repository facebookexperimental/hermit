/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! File-format boundary between legacy editors and explicit slice records.
//! The scheduler consumes the complete stream; legacy editors must refuse it.
//! History-only replay and read-only inspection never rewrite that stream.

use std::path::Path;

use anyhow::Context;
use anyhow::bail;
use detcore_model::schedule::boundary::Schedule;
use serde::Deserialize;
use serde::Serialize;

use super::PreemptionRecord;

/// Keep the preemption history and every existing run setting alongside the
/// complete boundary stream. No public access returns its legacy component:
/// doing so would let an editor silently discard the protocol and markers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "BoundaryWire", into = "BoundaryWire")]
pub struct BoundaryRecording {
    preemptions: PreemptionRecord,
    schedule: Schedule,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundaryWire {
    preemptions: PreemptionRecord,
    schedule: Schedule,
}

/// The wrapper is intentional. Old readers require top-level `per_thread`
/// and `global`; they therefore refuse this format even for zero markers.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    slice_boundary_recording: BoundaryRecording,
}

/// Whether a record is in the slice-boundary format. The probe builds none of
/// the record: it skips every value, so reading the format of a schedule of
/// millions of events does not cost a JSON tree of it, several times the
/// size of its text.
#[derive(Deserialize)]
struct FormatProbe {
    // A present null must not look like an absent protocol marker.
    #[serde(default, deserialize_with = "marker_present")]
    slice_boundary_recording: bool,
    #[serde(default, deserialize_with = "marker_present")]
    incomplete_preemption_record: bool,
}

fn marker_present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer).map(|_| true)
}

impl FormatProbe {
    fn is_boundary_recording(bytes: &[u8]) -> serde_json::Result<bool> {
        serde_json::from_slice::<Self>(bytes).map(|probe| probe.slice_boundary_recording)
    }
}

pub(super) fn read_settings<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("cannot read preemption record {}: {}", path.display(), e))?;
    let format: FormatProbe = serde_json::from_slice(&bytes)
        .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e))?;
    if !format.slice_boundary_recording && !format.incomplete_preemption_record {
        // Each old metadata reader still deserializes only its own field,
        // from the original bytes, preserving its legacy acceptance contract.
        return serde_json::from_slice(&bytes)
            .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e));
    }
    // Settings do not depend on the record's format, which the replay itself
    // checks and refuses (`Recording::decode`), with the refusal's exit status.
    let recording = Recording::decode_unchecked_format(&bytes)
        .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e))?;
    serde_json::from_value(
        super::record_serde::settings_value(recording.metadata()).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e))
}

impl BoundaryRecording {
    /// Combine a validated preemption history with the sole global record
    /// stream. The legacy history must not contain another global stream. The
    /// history's format is checked by [`Recording::decode`], which refuses it
    /// as a policy refusal (`RecordFormatRefusal`): a deserializer would lose
    /// that error's type.
    pub fn new(preemptions: PreemptionRecord, schedule: Schedule) -> anyhow::Result<Self> {
        preemptions
            .validate_contents()
            .map_err(anyhow::Error::msg)?;
        if !preemptions.global.is_empty() {
            bail!("slice-boundary recording has a second, legacy global event stream");
        }
        Ok(Self {
            preemptions,
            schedule,
        })
    }

    /// A read-only copy of every recorded thread history. It contains no
    /// global stream and cannot flatten or rewrite the boundary protocol.
    pub fn thread_histories(
        &self,
    ) -> std::collections::BTreeMap<crate::types::DetTid, super::ThreadHistory> {
        self.preemptions.extract_all()
    }

    /// The complete stored stream, including boundary indices.
    pub fn schedule(&self) -> &Schedule {
        &self.schedule
    }
}

impl TryFrom<BoundaryWire> for BoundaryRecording {
    type Error = anyhow::Error;

    fn try_from(wire: BoundaryWire) -> Result<Self, Self::Error> {
        Self::new(wire.preemptions, wire.schedule)
    }
}

impl From<BoundaryRecording> for BoundaryWire {
    fn from(recording: BoundaryRecording) -> Self {
        Self {
            preemptions: recording.preemptions,
            schedule: recording.schedule,
        }
    }
}

/// A recording whose format remains explicit through file transformations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Recording {
    /// The unchanged legacy preemption/event representation.
    Legacy(PreemptionRecord),
    /// A versioned boundary stream and its associated preemption history.
    Boundaries(BoundaryRecording),
}

impl Recording {
    /// Refuse a known omitted change point in either stored format.
    pub fn require_faithful_replay(&self) -> anyhow::Result<()> {
        self.metadata().require_faithful_replay()
    }

    /// A retained source observation of an omitted replay change point.
    pub fn replay_departed_at(&self) -> Option<u64> {
        self.metadata().replay_departed_at()
    }

    /// Require an explicit matching execution policy. The strict verifier must
    /// not silently fall back to an incomplete legacy stream, and ordinary
    /// recovery must not silently acquire strict EOF/desync semantics.
    pub fn require_strict_mode(&self, strict: bool) -> anyhow::Result<()> {
        match (self, strict) {
            (Self::Legacy(_), false) | (Self::Boundaries(_), true) => Ok(()),
            (Self::Legacy(_), true) => {
                anyhow::bail!("strict schedule boundaries require a complete boundary recording")
            }
            (Self::Boundaries(_), false) => {
                anyhow::bail!("complete boundary recording requires --strict-schedule-boundaries")
            }
        }
    }

    /// Validate and parse either format without a malformed-new-to-legacy
    /// fallback.
    /// A record whose format this Hermit refuses fails with a
    /// `RecordFormatRefusal` in its chain, whichever format it is in.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        let recording = Self::decode_unchecked_format(bytes)?;
        recording
            .metadata()
            .validate_format()
            .map_err(|refusal| anyhow::Error::new(super::RecordFormatRefusal(refusal)))?;
        Ok(recording)
    }

    /// [`Self::decode`], except for the record's format.
    fn decode_unchecked_format(bytes: &[u8]) -> anyhow::Result<Self> {
        // Inspect only to dispatch. Deserialize the ORIGINAL bytes again so
        // duplicate fields in the envelope are rejected. A malformed new
        // recording never falls back to legacy.
        if FormatProbe::is_boundary_recording(bytes)? {
            let envelope: Envelope = serde_json::from_slice(bytes)?;
            Ok(Self::Boundaries(envelope.slice_boundary_recording))
        } else {
            let legacy: PreemptionRecord = serde_json::from_slice(bytes)?;
            legacy.validate_contents().map_err(anyhow::Error::msg)?;
            Ok(Self::Legacy(legacy))
        }
    }

    /// Read a complete recording, preserving errors as failures.
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        Self::decode(&std::fs::read(path).with_context(|| format!("reading {}", path.display()))?)
            .with_context(|| format!("reading schedule {}", path.display()))
    }

    /// Serialize the same format with the conventional terminal newline.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = match self {
            Self::Legacy(record) => serde_json::to_vec(record)?,
            Self::Boundaries(record) => serde_json::to_vec(&Envelope {
                slice_boundary_recording: record.clone(),
            })?,
        };
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Write the complete serialized recording, without changing its format.
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        // Complete validation/serialization precedes opening the destination.
        let bytes = self.encode()?;
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
    }

    /// Give a legacy-only operation its input, or refuse a boundary format
    /// before that operation can discard its protocol or write an output.
    pub fn into_legacy(self, operation: &'static str) -> anyhow::Result<PreemptionRecord> {
        match self {
            Self::Legacy(record) => Ok(record),
            Self::Boundaries(_) => {
                Err(detcore_model::schedule::boundary::Error::UnsupportedEdit { operation }.into())
            }
        }
    }

    /// Remove only optional event timestamps, retaining boundary identities,
    /// RIP observations, preemption history and run settings.
    pub fn strip_times(&mut self) {
        match self {
            Self::Legacy(record) => {
                record.strip_event_times();
            }
            Self::Boundaries(record) => record.schedule.strip_times(),
        }
    }

    /// Check the full decoded input that this loader will install. The
    /// standalone metadata remains outside every returned ThreadHistory.
    pub fn validate_replay_inputs(
        &self,
        source: &str,
        guest_ids: detcore_model::pid::GuestIds,
    ) -> Result<(), super::RecordAdmissionRefusal> {
        self.metadata().validate_replay_inputs(source, guest_ids)
    }

    /// The opaque private-input metadata, without flattening this envelope.
    pub fn reserved_input(&self) -> &super::ReservedInputMetadata {
        self.metadata().reserved_input()
    }

    /// Numbering stored alongside either complete event representation.
    pub fn guest_ids(&self) -> detcore_model::pid::GuestIds {
        self.metadata().guest_ids().unwrap_or_default()
    }

    /// The recorded virtual-time epoch, if the recording states one.
    pub fn epoch(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.metadata().epoch()
    }

    /// The recorded pipe-wake latency, preserving `Some(0)` versus off.
    pub fn pipe_wake_latency(&self) -> Option<u64> {
        self.metadata().pipe_wake_latency()
    }

    /// The recorded signal-copy setting, including the legacy unknown case.
    pub fn signal_state_copy(&self) -> Option<bool> {
        self.metadata().signal_state_copy()
    }

    /// The host files the recorded run's guest opened, if the recording says.
    pub fn host_inputs(&self) -> Option<&detcore_model::host_input::HostInputSet> {
        self.metadata().host_inputs()
    }

    /// The recorded jumps of global time (`PreemptionRecord::time_skips`).
    pub fn time_skips(&self) -> Option<&[super::TimeSkip]> {
        self.metadata().time_skips()
    }

    /// Whether the recording lists every jump of global time its run made
    /// (`PreemptionRecord::lists_its_time_skips`).
    pub fn lists_its_time_skips(&self) -> bool {
        self.metadata().lists_its_time_skips()
    }

    /// Whether a replay of this recording charges a replayed switch-in under
    /// which guest code runs (`PreemptionRecord::charges_replayed_switch_ins`).
    pub fn charges_replayed_switch_ins(&self) -> bool {
        self.metadata().charges_replayed_switch_ins()
    }

    /// The recording's format (`PreemptionRecord::format_version`).
    pub fn format_version(&self) -> Option<u32> {
        self.metadata().format_version()
    }

    fn metadata(&self) -> &PreemptionRecord {
        match self {
            Self::Legacy(record) => record,
            Self::Boundaries(record) => &record.preemptions,
        }
    }
}

/// Runtime access to recorded priorities/RCB history, retaining its complete
/// backing format. This reader cannot export a flattened legacy recording.
#[derive(Debug)]
pub(crate) struct HistoryReader(Recording);

impl HistoryReader {
    // Production installs only histories admitted for the actual numbering
    // policy. Format/serialization tests also exercise the raw reader.
    #[cfg(test)]
    pub(crate) fn read(path: &Path) -> anyhow::Result<Self> {
        Recording::read(path).map(Self)
    }

    /// Read and admit the same bytes used for priority replay. A policy error
    /// retains its RecordAdmissionRefusal type for the pre-start exit-122 path.
    pub(crate) fn read_for_replay(
        path: &Path,
        guest_ids: detcore_model::pid::GuestIds,
    ) -> anyhow::Result<Self> {
        let recording = Recording::read(path)?;
        recording.validate_replay_inputs(
            &format!("--replay-preemptions-from {}", path.display()),
            guest_ids,
        )?;
        Ok(Self(recording))
    }

    /// The host files the recorded run's guest opened, if the recording says.
    pub(crate) fn host_inputs(&self) -> Option<&detcore_model::host_input::HostInputSet> {
        self.0.host_inputs()
    }

    /// History-only replay cannot honor boundary records. Admission must not
    /// silently project a new-format recording into legacy priorities/RCBs.
    pub(crate) fn require_legacy_mode(&self) -> anyhow::Result<()> {
        self.0.require_strict_mode(false)
    }

    /// Only explicit edited history exploration may consume known omissions.
    pub(crate) fn require_replay_mode(&self, edited: bool) -> anyhow::Result<()> {
        self.require_legacy_mode()?;
        if !edited {
            self.0.require_faithful_replay()?;
        }
        Ok(())
    }

    pub(crate) fn replay_departed_at(&self) -> Option<u64> {
        self.0.replay_departed_at()
    }

    pub(crate) fn thread_initial_priority(
        &self,
        tid: &crate::types::DetTid,
    ) -> Option<crate::scheduler::runqueue::Priority> {
        self.0
            .metadata()
            .per_thread
            .get(tid)
            .map(|history| history.initial_priority())
    }

    pub(crate) fn extract_thread_record(
        &self,
        tid: &crate::types::DetTid,
    ) -> Option<super::ThreadHistory> {
        self.0.metadata().per_thread.get(tid).cloned()
    }

    /// The validated callback checkpoints of this history, per thread, in
    /// row order (`PreemptionRecord::validate`).
    pub(crate) fn callback_checkpoints(
        &self,
    ) -> &std::collections::BTreeMap<crate::types::DetTid, Vec<super::callback_history::Checkpoint>>
    {
        self.0.metadata().callback_checkpoints()
    }

    /// The store-exclusives that failed in the recorded run.
    #[cfg(target_arch = "aarch64")]
    pub(crate) fn store_exclusive_failures(&self) -> u64 {
        self.0.metadata().store_exclusive_failures()
    }
}

impl std::fmt::Display for HistoryReader {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.0.encode().map_err(|_| std::fmt::Error)?;
        output.write_str(std::str::from_utf8(&bytes).map_err(|_| std::fmt::Error)?)
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::schedule::boundary::Boundary;
    use detcore_model::schedule::boundary::Cause;
    use detcore_model::schedule::boundary::Entry;
    use detcore_model::schedule::boundary::Identity;
    use detcore_model::schedule::boundary::Owner;
    use detcore_model::schedule::boundary::Phase;
    use detcore_model::schedule::boundary::Record;
    use reverie::syscalls::Sysno;

    use super::*;
    use crate::preemptions::PreemptionReader;
    use crate::preemptions::PreemptionWriter;
    use crate::preemptions::strip_times_from_events_file;
    use crate::types::DetTid;
    use crate::types::LogicalTime;
    use crate::types::SchedEvent;

    fn sample(markers: bool) -> Recording {
        let metadata: PreemptionRecord = serde_json::from_str(
            &PreemptionWriter::new(None)
                .with_epoch("2001-02-03T04:05:06Z".parse().unwrap())
                .with_pipe_wake_latency(Some(0))
                .with_signal_state_copy(false)
                .with_expired_wait_runs_charged_once(true)
                .with_queued_sleeps_registered_at_first_pass(true)
                .into_string(),
        )
        .unwrap();
        let records = if markers {
            vec![Record::TimesliceEnd(Boundary {
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
                        occurrence: 1,
                    },
                    committed_rcbs: 17,
                    checkin: 1,
                    slice: 1,
                },
                count: 1,
                start_rip: None,
                end_rip: Some(std::num::NonZeroUsize::new(0x1234).unwrap()),
                end_time: Some(LogicalTime::from_nanos(123)),
            })]
        } else {
            vec![]
        };
        Recording::Boundaries(
            BoundaryRecording::new(metadata, Schedule::new(records).unwrap()).unwrap(),
        )
    }

    #[test]
    fn actual_legacy_reader_rejects_even_an_empty_boundary_recording() {
        for markers in [false, true] {
            let recording = sample(markers);
            let bytes = recording.encode().unwrap();
            assert!(serde_json::from_slice::<PreemptionRecord>(&bytes).is_err());
            assert_eq!(Recording::decode(&bytes).unwrap(), recording);
            assert!(recording.into_legacy("minimize").is_err());
        }
    }

    #[test]
    fn file_strip_preserves_protocol_metadata_and_mandatory_identity() {
        let directory = tempfile::tempdir().unwrap();
        for markers in [false, true] {
            let input = directory.path().join(format!("{markers}.json"));
            let original = sample(markers);
            original.write(&input).unwrap();
            let original_bytes = std::fs::read(&input).unwrap();
            let output = strip_times_from_events_file(&input, None).unwrap();
            let actual = Recording::read(&output).unwrap();
            let mut expected = original.clone();
            expected.strip_times();
            assert_eq!(actual, expected);
            assert_eq!(std::fs::read(&input).unwrap(), original_bytes);
            assert_eq!(actual.epoch(), original.epoch());
            assert_eq!(actual.pipe_wake_latency(), Some(0));
            assert_eq!(actual.signal_state_copy(), Some(false));
            assert_eq!(
                super::super::read_recorded_epoch(&output).unwrap(),
                original.epoch()
            );
            assert_eq!(
                super::super::read_recorded_pipe_wake_latency(&output).unwrap(),
                Some(0)
            );
            assert_eq!(
                super::super::read_recorded_signal_state_copy(&output).unwrap(),
                Some(false)
            );
            assert_eq!(
                super::super::read_recorded_expired_wait_runs_charged_once(&output),
                Ok(true)
            );
            assert_eq!(
                super::super::read_recorded_queued_sleeps_registered_at_first_pass(&output),
                Ok(true)
            );
            assert!(PreemptionReader::try_new(&output).is_err());
            assert!(
                serde_json::from_slice::<PreemptionRecord>(&std::fs::read(output).unwrap())
                    .is_err()
            );
        }
    }

    /// The format is the presence of the top-level marker, null included,
    /// read without building any value.
    #[test]
    fn the_format_is_the_presence_of_the_top_level_marker() {
        let probe = |text: &str| FormatProbe::is_boundary_recording(text.as_bytes()).unwrap();
        assert!(!probe(r#"{"per_thread": {}, "global": []}"#));
        assert!(probe(r#"{"slice_boundary_recording": {"preemptions": 1}}"#));
        assert!(probe(r#"{"slice_boundary_recording": null}"#));
        assert!(!probe(
            r#"{"per_thread": {"3": {"slice_boundary_recording": 1}}, "global": []}"#
        ));
        assert!(
            FormatProbe::is_boundary_recording(
                br#"{"slice_boundary_recording": null, "slice_boundary_recording": null}"#
            )
            .is_err()
        );
        assert!(FormatProbe::is_boundary_recording(b"{\"global\": [").is_err());
    }

    #[test]
    fn unknown_or_broken_envelope_never_falls_back_to_valid_legacy_fields() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&sample(false).encode().unwrap()).unwrap();
        value["per_thread"] = serde_json::json!({});
        value["global"] = serde_json::json!([]);
        assert!(Recording::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        value["slice_boundary_recording"] = serde_json::Value::Null;
        assert!(Recording::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        let body = String::from_utf8(sample(false).encode().unwrap()).unwrap();
        let duplicated = format!(
            "{{{}, {}}}",
            body.trim()[1..body.trim().len() - 1].to_owned(),
            body.trim()[1..body.trim().len() - 1].to_owned()
        );
        assert!(Recording::decode(duplicated.as_bytes()).is_err());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad.json");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(super::super::read_recorded_epoch(&path).is_err());
        assert!(super::super::read_recorded_pipe_wake_latency(&path).is_err());
        assert!(super::super::read_recorded_signal_state_copy(&path).is_err());
    }

    /// A record of a format this Hermit refuses is refused as a policy
    /// refusal (`RecordFormatRefusal`) in either envelope, while its settings
    /// stay readable, so the replay itself refuses it with that exit status.
    #[test]
    fn a_refused_format_is_a_refusal_in_either_envelope() {
        let refused = |value: &serde_json::Value| {
            Recording::decode(&serde_json::to_vec(value).unwrap())
                .unwrap_err()
                .chain()
                .any(|cause| cause.is::<super::super::RecordFormatRefusal>())
        };
        let mut boundary: serde_json::Value =
            serde_json::from_slice(&sample(false).encode().unwrap()).unwrap();
        boundary["slice_boundary_recording"]["preemptions"]["format_version"] =
            serde_json::json!(99);
        assert!(refused(&boundary));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("refused.json");
        std::fs::write(&path, serde_json::to_vec(&boundary).unwrap()).unwrap();
        assert!(super::super::read_recorded_epoch(&path).unwrap().is_some());
        let mut plain = serde_json::to_value(PreemptionRecord::default()).unwrap();
        plain["format_version"] = serde_json::json!(99);
        assert!(refused(&plain));
    }

    #[test]
    fn boundary_history_reader_preserves_the_complete_format_and_original_thread_histories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.json");
        let tid = crate::types::DetTid::from_raw(3);
        let mut writer = PreemptionWriter::new(Some(path.clone()))
            .with_slice_boundaries()
            .unwrap();
        writer.register_thread(tid, crate::scheduler::runqueue::DEFAULT_PRIORITY);
        let expected_history = writer.inner.per_thread.get(&tid).cloned();
        writer.flush().unwrap();
        let before = std::fs::read(&path).unwrap();
        let history = HistoryReader::read(&path).unwrap();
        assert!(history.require_legacy_mode().is_err());
        assert_eq!(
            history.thread_initial_priority(&tid),
            Some(crate::scheduler::runqueue::DEFAULT_PRIORITY)
        );
        assert_eq!(history.extract_thread_record(&tid), expected_history);
        assert_eq!(history.to_string().as_bytes(), before);
        assert!(super::super::PreemptionReader::try_new(&path).is_err());
        let mut bad: serde_json::Value = serde_json::from_slice(&before).unwrap();
        bad["slice_boundary_recording"]["schedule"]["version"] = 999.into();
        std::fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(HistoryReader::read(&path).is_err());
        assert_ne!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn a_second_event_stream_is_not_accepted_as_metadata() {
        let metadata =
            PreemptionRecord::from_sched_events(vec![SchedEvent::branches(DetTid::from_raw(3), 1)]);
        assert!(BoundaryRecording::new(metadata, Schedule::new(vec![]).unwrap()).is_err());
    }

    #[test]
    fn legacy_bytes_and_editor_access_remain_legacy() {
        let legacy =
            PreemptionRecord::from_sched_events(vec![SchedEvent::branches(DetTid::from_raw(3), 2)])
                .with_pipe_wake_latency(Some(7))
                .with_signal_state_copy(Some(true));
        let mut expected = serde_json::to_vec(&legacy).unwrap();
        expected.push(b'\n');
        let record = Recording::Legacy(legacy.clone());
        assert_eq!(record.encode().unwrap(), expected);
        assert_eq!(
            Recording::decode(&expected)
                .unwrap()
                .into_legacy("split")
                .unwrap(),
            legacy
        );
    }

    #[test]
    fn legacy_metadata_readers_keep_their_individual_field_contracts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.json");
        std::fs::write(&path, r#"{"epoch":null,"pipe_wake_latency":"invalid"}"#).unwrap();
        assert_eq!(super::super::read_recorded_epoch(&path), Ok(None));
        assert!(super::super::read_recorded_pipe_wake_latency(&path).is_err());
        assert_eq!(
            super::super::read_recorded_signal_state_copy(&path),
            Ok(None)
        );
        std::fs::write(&path, r#"{"epoch":null,"epoch":null}"#).unwrap();
        assert!(super::super::read_recorded_epoch(&path).is_err());
    }

    #[test]
    fn writer_keeps_events_markers_and_all_metadata_in_one_stored_stream() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recording.json");
        let mut writer = PreemptionWriter::new(Some(path.clone()))
            .with_epoch("2001-02-03T04:05:06Z".parse().unwrap())
            .with_pipe_wake_latency(Some(0))
            .with_signal_state_copy(false)
            .with_slice_boundaries()
            .unwrap();
        let tid = DetTid::from_raw(3);
        writer.register_thread(tid, crate::scheduler::runqueue::DEFAULT_PRIORITY);
        let metadata = writer.inner.clone();
        let Recording::Boundaries(sample) = sample(true) else {
            unreachable!()
        };
        let Record::TimesliceEnd(boundary) = sample.schedule.records()[0].clone() else {
            unreachable!()
        };
        let first = SchedEvent::branches(tid, 17);
        let last = SchedEvent::syscall(tid, Sysno::write, crate::types::SyscallPhase::Posthook);
        writer.insert_schedevent(first.clone());
        assert_eq!(writer.insert_slice_boundary(boundary.clone()).unwrap(), 1);
        writer.insert_schedevent(last.clone());
        writer.flush().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert!(serde_json::from_slice::<PreemptionRecord>(&bytes).is_err());
        let Recording::Boundaries(actual) = Recording::decode(&bytes).unwrap() else {
            panic!("writer flattened the boundary protocol")
        };
        assert_eq!(actual.preemptions, metadata);
        assert_eq!(
            actual.schedule.records(),
            &[
                Record::Event(first),
                Record::TimesliceEnd(boundary),
                Record::Event(last),
            ]
        );
    }

    #[test]
    fn writer_refuses_invalid_order_before_overwriting_the_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retained.json");
        std::fs::write(&path, b"retained original").unwrap();
        let Recording::Boundaries(sample) = sample(true) else {
            unreachable!()
        };
        let Record::TimesliceEnd(boundary) = sample.schedule.records()[0].clone() else {
            unreachable!()
        };
        let mut writer = PreemptionWriter::new(Some(path.clone()))
            .with_slice_boundaries()
            .unwrap();
        writer.insert_slice_boundary(boundary.clone()).unwrap();
        writer.insert_slice_boundary(boundary.clone()).unwrap();
        assert!(writer.flush().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"retained original");

        let mut legacy = PreemptionWriter::new(Some(path.clone()));
        assert!(legacy.insert_slice_boundary(boundary).is_err());
        legacy.insert_schedevent(SchedEvent::branches(DetTid::from_raw(3), 1));
        assert!(legacy.with_slice_boundaries().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"retained original");
    }

    #[test]
    fn writer_default_still_emits_exact_legacy_bytes() {
        let mut writer = PreemptionWriter::new(None);
        let event = SchedEvent::branches(DetTid::from_raw(3), 7);
        writer.insert_schedevent(event.clone());
        assert_eq!(
            writer.into_string(),
            PreemptionRecord::from_sched_events(vec![event]).to_string()
        );
    }

    #[test]
    fn zero_count_or_nonpositive_event_owner_is_rejected_on_new_format_read() {
        for (tid, count) in [(3, 0), (0, 1), (-1, 1)] {
            let event = SchedEvent::branches(DetTid::from_raw(tid), count);
            assert!(Schedule::new(vec![Record::Event(event.clone())]).is_err());
            let mut wire: serde_json::Value =
                serde_json::from_slice(&sample(false).encode().unwrap()).unwrap();
            let schedule = &mut wire["slice_boundary_recording"]["schedule"];
            schedule["record_count"] = 1.into();
            schedule["records"] = serde_json::json!([Record::Event(event)]);
            assert!(Recording::decode(&serde_json::to_vec(&wire).unwrap()).is_err());
        }
    }
}

#[cfg(test)]
mod explicit_mode_tests {
    use super::*;

    #[test]
    fn explicit_mode_admission_never_falls_back_or_changes_legacy_policy() {
        let legacy = Recording::Legacy(PreemptionRecord::default());
        let typed = Recording::Boundaries(
            BoundaryRecording::new(PreemptionRecord::default(), Schedule::new(vec![]).unwrap())
                .unwrap(),
        );
        assert!(legacy.require_strict_mode(false).is_ok());
        assert!(typed.require_strict_mode(true).is_ok());
        assert!(legacy.require_strict_mode(true).is_err());
        assert!(typed.require_strict_mode(false).is_err());
    }
    #[test]
    fn ordinary_history_admits_legacy_and_retains_read_error_panics() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        std::fs::write(
            &path,
            Recording::Legacy(PreemptionRecord::default())
                .encode()
                .unwrap(),
        )
        .unwrap();
        assert!(
            HistoryReader::read(&path)
                .unwrap()
                .require_legacy_mode()
                .is_ok()
        );
        for contents in [None, Some("not a JSON recording")] {
            match contents {
                None => std::fs::remove_file(&path).unwrap(),
                Some(value) => std::fs::write(&path, value).unwrap(),
            }
            let cfg = crate::Config {
                replay_preemptions_from: Some(path.clone()),
                ..crate::Config::default()
            };
            assert!(std::panic::catch_unwind(|| crate::GlobalState::for_tests(&cfg)).is_err());
        }
    }
}
