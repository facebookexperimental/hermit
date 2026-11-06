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

use std::fs::File;
use std::io::Cursor;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

use anyhow::Context;
use anyhow::bail;
use detcore_model::schedule::boundary::BorrowedSchedule;
use detcore_model::schedule::boundary::Schedule;
use serde::Deserialize;
use serde::Serialize;

use super::HistoryIdentity;
use super::PreemptionRecord;

/// The only format of a history that states the identity of its RCB
/// coordinates. Its sole top-level key replaces the required legacy
/// `per_thread`/`global` and the `slice_boundary_recording` marker, so a
/// reader built before it refuses the file instead of reading coordinates
/// in another unit.
const RCB_HISTORY_FORMAT: u32 = 2;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityEnvelope<R> {
    rcb_history: IdentityBody<R>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityBody<R> {
    format: u32,
    identity: IdentityWire,
    /// The legacy record, or the `slice_boundary_recording` envelope.
    recording: R,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum IdentityWire {
    Known(String),
    Unqualified(UnqualifiedWire),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnqualifiedWire {
    unqualified: String,
}

impl From<&HistoryIdentity> for IdentityWire {
    fn from(identity: &HistoryIdentity) -> Self {
        match identity {
            HistoryIdentity::Known(metric) => Self::Known(metric.id().to_owned()),
            HistoryIdentity::Unqualified(reason) => Self::Unqualified(UnqualifiedWire {
                unqualified: reason.clone(),
            }),
        }
    }
}

impl TryFrom<IdentityWire> for HistoryIdentity {
    type Error = anyhow::Error;

    fn try_from(wire: IdentityWire) -> anyhow::Result<Self> {
        match wire {
            IdentityWire::Known(id) => reverie::pmu_config::RcbMetric::from_id(&id)
                .map(Self::Known)
                .ok_or_else(|| anyhow::anyhow!("unknown RCB coordinate identity {id:?}")),
            IdentityWire::Unqualified(UnqualifiedWire { unqualified }) => {
                Ok(Self::Unqualified(unqualified))
            }
        }
    }
}

/// Serializes `record` alone: in the `rcb_history` envelope if it states an
/// identity, otherwise in the legacy format.
pub(super) fn encode_record(record: &PreemptionRecord) -> serde_json::Result<String> {
    match record.identity() {
        None => serde_json::to_string(record),
        Some(identity) => serde_json::to_string(&IdentityEnvelope {
            rcb_history: IdentityBody {
                format: RCB_HISTORY_FORMAT,
                identity: identity.into(),
                recording: record,
            },
        }),
    }
}

/// Keep the preemption history and every existing run setting alongside the
/// complete boundary stream. No public access returns its legacy component:
/// doing so would let an editor silently discard the protocol and markers.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(try_from = "BoundaryWire")]
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

#[derive(Serialize)]
#[serde(rename = "BoundaryWire")]
struct BorrowedBoundary<'a> {
    preemptions: &'a PreemptionRecord,
    schedule: BorrowedSchedule<'a>,
}

#[derive(Serialize)]
#[serde(rename = "Envelope")]
pub(super) struct BorrowedEnvelope<'a> {
    slice_boundary_recording: BorrowedBoundary<'a>,
}

impl<'a> BorrowedEnvelope<'a> {
    pub(super) fn new(
        preemptions: &'a PreemptionRecord,
        records: &'a [detcore_model::schedule::boundary::Record],
    ) -> anyhow::Result<Self> {
        preemptions.validate().map_err(anyhow::Error::msg)?;
        if !preemptions.global.is_empty() {
            bail!("slice-boundary recording has a second, legacy global event stream");
        }
        Ok(Self {
            slice_boundary_recording: BorrowedBoundary {
                preemptions,
                schedule: Schedule::borrow_records(records)?,
            },
        })
    }
}

#[derive(Serialize)]
#[serde(rename = "BoundaryWire")]
struct BorrowedInfoBoundary<'a> {
    preemptions: super::record_serde::InfoProjection<'a>,
    schedule: BorrowedSchedule<'a>,
}

/// A borrowed envelope as the INFO comparison sees it: the exact boundary
/// serialization with the preemption history projected through
/// `record_serde::InfoProjection` (no host-observed store-exclusive count).
#[derive(Serialize)]
#[serde(rename = "Envelope")]
pub(super) struct BorrowedInfoEnvelope<'a> {
    slice_boundary_recording: BorrowedInfoBoundary<'a>,
}

impl<'a> BorrowedInfoEnvelope<'a> {
    pub(super) fn new(
        preemptions: &'a PreemptionRecord,
        records: &'a [detcore_model::schedule::boundary::Record],
    ) -> anyhow::Result<Self> {
        let envelope = BorrowedEnvelope::new(preemptions, records)?;
        Ok(Self {
            slice_boundary_recording: BorrowedInfoBoundary {
                preemptions: super::record_serde::InfoProjection(preemptions),
                schedule: envelope.slice_boundary_recording.schedule,
            },
        })
    }
}

impl Serialize for BoundaryRecording {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(rename = "BoundaryWire")]
        struct Wire<'a> {
            preemptions: &'a PreemptionRecord,
            schedule: &'a Schedule,
        }
        Wire {
            preemptions: &self.preemptions,
            schedule: &self.schedule,
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
impl FormatProbe {
    fn is_boundary_recording(bytes: &[u8]) -> serde_json::Result<bool> {
        serde_json::from_slice::<Self>(bytes).map(|probe| probe.slice_boundary_recording)
    }

    fn has_rcb_history(bytes: &[u8]) -> serde_json::Result<bool> {
        serde_json::from_slice::<Self>(bytes).map(|probe| probe.rcb_history)
    }
}

/// Which protocol markers a record carries. The probe builds none of the
/// record: it skips every value, so reading the format of a schedule of
/// millions of events does not cost a JSON tree of it, several times the
/// size of its text.
#[derive(Deserialize)]
struct FormatProbe {
    // A present null must not look like an absent protocol marker.
    #[serde(default, deserialize_with = "marker_present")]
    slice_boundary_recording: bool,
    #[serde(default, deserialize_with = "marker_present")]
    incomplete_preemption_record: bool,
    #[serde(default, deserialize_with = "marker_present")]
    rcb_history: bool,
}

fn marker_present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer).map(|_| true)
}

/// A `format_version` as [`probe_format`] reads it: a format number, or
/// `None` for anything else, which the record's ordinary decoding reports.
struct StatedFormat(Option<u32>);

impl<'de> Deserialize<'de> for StatedFormat {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Self(
            value.as_u64().and_then(|format| u32::try_from(format).ok()),
        ))
    }
}

/// The envelope field, present (even as null) or absent, so that a null
/// envelope is not taken for a legacy record.
fn present_envelope<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// What [`probe_format`] reads of a record: the format it states (`None`
/// when absent or not a number) and how many jumps of global time it lists.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ProbedFormat {
    pub(super) format: Option<u32>,
    pub(super) time_skips: usize,
}

/// The format a preemption record states (`PreemptionRecord::format_version`),
/// and the number of its time skips, read from its bytes without decoding the
/// record: at the top level of a legacy record, and only in the preemptions of
/// a slice-boundary one (a field beside the envelope is the envelope decoder's
/// to refuse). [`Recording::decode`] consults it only when the record cannot
/// be decoded, so that a record of a format this Hermit does not read is
/// refused as such ([`super::RecordFormatRefusal`]) even when its body is one
/// this Hermit cannot read; a record it can decode is refused, or not, by
/// `validate_format` alone. The probe is typed, so a duplicated
/// `format_version` or `preemptions` is an error here, as in the strict
/// decoders, never a format; every other field is skipped unread. A format
/// that is not a number, or a null envelope, reads as none, and an envelope
/// that is not an object is an error, each left to the record's ordinary
/// decoding to report. A record marked incomplete
/// (`PreemptionRecord::replay_departed_at`) is written inside an
/// `incomplete_preemption_record` envelope, as a legacy record or as a
/// slice-boundary recording's preemptions, and states its format only in that
/// envelope's record.
pub(super) fn probe_format(bytes: &[u8]) -> Result<ProbedFormat, serde_json::Error> {
    #[derive(Deserialize)]
    struct Payload {
        #[serde(default)]
        format_version: Option<StatedFormat>,
        #[serde(default)]
        time_skips: Option<Vec<serde::de::IgnoredAny>>,
    }
    #[derive(Deserialize)]
    struct Marked {
        #[serde(default)]
        record: Option<Payload>,
    }
    #[derive(Deserialize)]
    struct Preemptions {
        #[serde(default)]
        format_version: Option<StatedFormat>,
        #[serde(default)]
        time_skips: Option<Vec<serde::de::IgnoredAny>>,
        #[serde(default, deserialize_with = "present_envelope")]
        incomplete_preemption_record: Option<Option<Marked>>,
    }
    type Stated = (Option<StatedFormat>, Option<Vec<serde::de::IgnoredAny>>);
    impl Preemptions {
        // A field beside the incomplete-record envelope is the decoder's to
        // refuse, as one beside the slice-boundary envelope is.
        fn stated(self) -> Stated {
            match self.incomplete_preemption_record {
                Some(marked) => marked
                    .and_then(|marked| marked.record)
                    .map_or((None, None), |record| {
                        (record.format_version, record.time_skips)
                    }),
                None => (self.format_version, self.time_skips),
            }
        }
    }
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        preemptions: Option<Preemptions>,
    }
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        format_version: Option<StatedFormat>,
        #[serde(default)]
        time_skips: Option<Vec<serde::de::IgnoredAny>>,
        #[serde(default, deserialize_with = "present_envelope")]
        incomplete_preemption_record: Option<Option<Marked>>,
        #[serde(default, deserialize_with = "present_envelope")]
        slice_boundary_recording: Option<Option<Envelope>>,
    }
    let probe: Probe = serde_json::from_slice(bytes)?;
    let (stated, skips) = match probe.slice_boundary_recording {
        Some(envelope) => envelope
            .and_then(|envelope| envelope.preemptions)
            .map_or((None, None), Preemptions::stated),
        None => Preemptions {
            format_version: probe.format_version,
            time_skips: probe.time_skips,
            incomplete_preemption_record: probe.incomplete_preemption_record,
        }
        .stated(),
    };
    Ok(ProbedFormat {
        format: stated.and_then(|format| format.0),
        time_skips: skips.map_or(0, |skips| skips.len()),
    })
}

/// The error for a record that `serde` could not decode: a refusal by its
/// format (`super::RecordFormatRefusal`, with `validate_format`'s message) when
/// it states a format this Hermit does not read, else `error` itself.
fn refuse_undecodable_format(bytes: &[u8], error: anyhow::Error) -> anyhow::Error {
    match probe_format(bytes) {
        Ok(probed) if !super::reads_format(probed.format) => {
            anyhow::Error::new(super::RecordFormatRefusal(super::unread_format_message(
                probed.format,
                probed.time_skips,
            )))
        }
        _ => error,
    }
}

pub(super) fn read_settings<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    fn seekable<T: serde::de::DeserializeOwned, R: Read + Seek>(
        input: &mut R,
    ) -> anyhow::Result<T> {
        let start = input.stream_position()?;
        let (probe, digest) = super::streaming::parse_pass::<FormatProbe, _>(&mut *input)?;
        input.seek(SeekFrom::Start(start))?;
        if !probe.slice_boundary_recording
            && !probe.incomplete_preemption_record
            && !probe.rcb_history
        {
            // Each old metadata reader still deserializes only its own field,
            // preserving its legacy acceptance contract.
            let (settings, typed_digest) = super::streaming::parse_pass(&mut *input)?;
            require_same_input(digest, typed_digest)?;
            return Ok(settings);
        }
        // Settings do not depend on the record's format, which the replay
        // itself checks and refuses (`Recording::decode`), with the
        // refusal's exit status.
        let (recording, typed_digest) = Recording::read_fingerprinted(input)?;
        require_same_input(digest, typed_digest)?;
        Ok(serde_json::from_value(
            super::record_serde::settings_value(recording.metadata())
                .map_err(|e| anyhow::anyhow!(e.to_string()))?,
        )?)
    }
    let mut file = File::open(path)
        .map_err(|e| format!("cannot read preemption record {}: {}", path.display(), e))?;
    let rewindable = file
        .stream_position()
        .and_then(|start| file.seek(SeekFrom::Start(start)))
        .is_ok();
    let parsed = if rewindable {
        seekable(&mut file)
    } else {
        // Preserve FIFO/pipe compatibility. Only this unseekable path retains
        // the historical whole-byte buffer; regular histories do not use it.
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(anyhow::Error::from)
            .and_then(|_| seekable(&mut Cursor::new(bytes)))
    };
    parsed.map_err(|e| format!("cannot parse preemption record {}: {e}", path.display()))
}

fn require_same_input(probe: [u8; 32], typed: [u8; 32]) -> anyhow::Result<()> {
    if probe != typed {
        bail!("schedule input changed between complete parsing passes");
    }
    Ok(())
}

/// [`read_settings`] over `bytes` already read from `path`, which only names
/// them in errors.
pub(super) fn settings_in<T: serde::de::DeserializeOwned>(
    path: &Path,
    bytes: &[u8],
) -> Result<T, String> {
    let mut input = Cursor::new(bytes);
    let parsed = (|| -> anyhow::Result<T> {
        let start = input.stream_position()?;
        let (probe, digest) = super::streaming::parse_pass::<FormatProbe, _>(&mut input)?;
        input.seek(SeekFrom::Start(start))?;
        if !probe.slice_boundary_recording
            && !probe.incomplete_preemption_record
            && !probe.rcb_history
        {
            let (settings, typed_digest) = super::streaming::parse_pass::<T, _>(&mut input)?;
            require_same_input(digest, typed_digest)?;
            return Ok(settings);
        }
        let (recording, typed_digest) = Recording::read_fingerprinted(&mut input)?;
        require_same_input(digest, typed_digest)?;
        Ok(serde_json::from_value(
            super::record_serde::settings_value(recording.metadata())
                .map_err(|e| anyhow::anyhow!(e.to_string()))?,
        )?)
    })();
    parsed.map_err(|e| format!("cannot parse preemption record {}: {e}", path.display()))
}

impl BoundaryRecording {
    /// Combine a validated preemption history with the sole global record
    /// stream. The legacy history must not contain another global stream, nor
    /// background starts (`BackgroundStart`), which only the legacy schedule
    /// replayer takes. The history's format is checked by
    /// [`Recording::decode`], which refuses it as a policy refusal
    /// (`RecordFormatRefusal`): a deserializer would lose that error's type.
    pub fn new(preemptions: PreemptionRecord, schedule: Schedule) -> anyhow::Result<Self> {
        preemptions
            .validate_contents()
            .map_err(anyhow::Error::msg)?;
        if !preemptions.global.is_empty() {
            bail!("slice-boundary recording has a second, legacy global event stream");
        }
        if !preemptions.background_starts.is_empty() {
            bail!(
                "slice-boundary recording notes background starts, which a boundary replay does \
                 not take; only a legacy schedule records and replays them"
            );
        }
        // --chaos-instruction-offset refuses --strict-schedule-boundaries, so
        // no slice-boundary recording holds an offset decision, in a thread's
        // rows or in its events.
        if let Some((tid, _)) = preemptions
            .per_thread
            .iter()
            .find(|(_, history)| history.has_instruction_offsets())
        {
            bail!(
                "slice-boundary recording thread {tid}'s rows carry instruction offsets, which a \
                 slice-boundary recording cannot hold; the record is malformed"
            );
        }
        if let Some(index) = schedule.records().iter().position(|record| {
            matches!(
                record,
                detcore_model::schedule::boundary::Record::Event(event)
                    if event.instruction_offset.is_some()
            )
        }) {
            bail!(
                "slice-boundary recording record {index} carries an instruction offset, which a \
                 slice-boundary recording cannot hold; the record is malformed"
            );
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

/// A recording whose format remains explicit through file transformations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Recording {
    /// The unchanged legacy preemption/event representation.
    Legacy(PreemptionRecord),
    /// A versioned boundary stream and its associated preemption history.
    Boundaries(BoundaryRecording),
}

impl Serialize for Recording {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Envelope<'a> {
            slice_boundary_recording: &'a BoundaryRecording,
        }
        match (self, self.metadata().identity()) {
            (Self::Legacy(record), None) => record.serialize(serializer),
            (Self::Legacy(record), Some(identity)) => IdentityEnvelope {
                rcb_history: IdentityBody {
                    format: RCB_HISTORY_FORMAT,
                    identity: identity.into(),
                    recording: record,
                },
            }
            .serialize(serializer),
            (Self::Boundaries(record), None) => Envelope {
                slice_boundary_recording: record,
            }
            .serialize(serializer),
            (Self::Boundaries(record), Some(identity)) => IdentityEnvelope {
                rcb_history: IdentityBody {
                    format: RCB_HISTORY_FORMAT,
                    identity: identity.into(),
                    recording: Envelope {
                        slice_boundary_recording: record,
                    },
                },
            }
            .serialize(serializer),
        }
    }
}

/// A passive summary of stored rows, not a replay/terminal certificate. Its
/// memory is proportional to thread count, rather than event/history rows.
#[derive(Debug, Serialize)]
pub struct Inspection {
    /// The stored format: `legacy` or `slice_boundaries`.
    pub format: &'static str,
    /// The structural validation performed, without certifying replay completion.
    pub structural_validation: &'static str,
    /// All rows in the stored global stream, including any boundary markers.
    pub stored_global_rows: usize,
    /// Scheduling-event rows in the stored global stream.
    pub event_rows: usize,
    /// None means the legacy format has no boundary protocol, not zero proof.
    pub boundary_rows: Option<usize>,
    /// The recorded virtual-time epoch, if the history stores one.
    pub epoch: Option<chrono::DateTime<chrono::Utc>>,
    /// Recorded `--pipe-wake-latency` in nanoseconds, or absent/off as `None`.
    pub pipe_wake_latency: Option<u64>,
    /// The recorded signal-state-copy setting, or `None` when unspecified.
    pub signal_state_copy: Option<bool>,
    /// Stored per-thread history counts, keyed by deterministic thread identity.
    pub threads: std::collections::BTreeMap<crate::types::DetTid, ThreadInspection>,
}

/// Counts and the final priority stored in one thread's history.
#[derive(Debug, Serialize)]
pub struct ThreadInspection {
    /// Stored priority-change rows.
    pub priority_rows: usize,
    /// Stored retired-conditional-branch preemption counts.
    pub rcb_rows: usize,
    /// Stored chaos-epoch transitions.
    pub chaos_epoch_rows: usize,
    /// Stored callback checkpoints for this deterministic thread.
    pub callback_rows: usize,
    /// The history's recorded final priority.
    pub final_priority: crate::scheduler::runqueue::Priority,
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
        // A record that cannot be decoded is still refused by a format this
        // Hermit does not read (`probe_format`); one that can is refused, or
        // not, by `validate_format` below.
        Self::read_from(&mut Cursor::new(bytes)).map_err(|error| {
            if error.chain().any(|cause| cause.is::<serde_json::Error>()) {
                refuse_undecodable_format(bytes, error)
            } else {
                error
            }
        })
    }

    /// Stream a seekable input with no raw-byte or Value-tree copy. Every pass
    /// reaches EOF, and hashes must match before the typed result is exposed.
    /// This rejects inconsistent passes; it is not a filesystem snapshot.
    /// A record whose format this Hermit refuses fails with a
    /// `RecordFormatRefusal` in its chain, whichever format it is in.
    pub fn read_from<R: Read + Seek>(input: &mut R) -> anyhow::Result<Self> {
        let start = input.stream_position()?;
        let (recording, _) = Self::read_fingerprinted(input).map_err(|error| {
            if error.chain().any(|cause| cause.is::<serde_json::Error>()) {
                // As in `decode`: a record that cannot be decoded is still
                // refused by a format this Hermit does not read, probed from
                // its bytes, rather than reporting the decoding error.
                let probed = (|| {
                    input.seek(SeekFrom::Start(start))?;
                    let mut bytes = Vec::new();
                    input.read_to_end(&mut bytes)?;
                    anyhow::Ok(bytes)
                })();
                match probed {
                    Ok(bytes) => refuse_undecodable_format(&bytes, error),
                    Err(_) => error,
                }
            } else {
                error
            }
        })?;
        recording
            .metadata()
            .validate_format()
            .map_err(|refusal| anyhow::Error::new(super::RecordFormatRefusal(refusal)))?;
        Ok(recording)
    }

    /// [`Self::read_from`] without the format check, for readers that take a
    /// recording whose format the replay itself refuses (settings).
    /// Inspect only to dispatch. Deserialize the complete input again so
    /// duplicate fields in the envelope are rejected. A malformed new
    /// recording never falls back to legacy.
    fn read_fingerprinted<R: Read + Seek>(input: &mut R) -> anyhow::Result<(Self, [u8; 32])> {
        let start = input.stream_position()?;
        let (probe, digest) = super::streaming::parse_pass::<FormatProbe, _>(&mut *input)?;
        input.seek(SeekFrom::Start(start))?;
        if probe.rcb_history {
            let (envelope, typed_digest) = super::streaming::parse_pass::<
                IdentityEnvelope<Box<serde_json::value::RawValue>>,
                _,
            >(&mut *input)?;
            require_same_input(digest, typed_digest)?;
            if envelope.rcb_history.format != RCB_HISTORY_FORMAT {
                bail!(
                    "unsupported rcb_history format {}",
                    envelope.rcb_history.format
                );
            }
            let identity = HistoryIdentity::try_from(envelope.rcb_history.identity)?;
            let inner = envelope.rcb_history.recording.get().as_bytes();
            if serde_json::from_slice::<FormatProbe>(inner)?.rcb_history {
                bail!("nested rcb_history envelope");
            }
            let (mut recording, _) = Self::read_fingerprinted(&mut Cursor::new(inner))?;
            recording.metadata_mut().identity = Some(identity);
            return Ok((recording, typed_digest));
        }
        let (recording, typed_digest) = if probe.slice_boundary_recording {
            let (envelope, digest) = super::streaming::parse_pass::<Envelope, _>(&mut *input)?;
            (Self::Boundaries(envelope.slice_boundary_recording), digest)
        } else {
            let (legacy, digest) =
                super::streaming::parse_pass::<PreemptionRecord, _>(&mut *input)?;
            legacy.validate_contents().map_err(anyhow::Error::msg)?;
            (Self::Legacy(legacy), digest)
        };
        require_same_input(digest, typed_digest)?;
        Ok((recording, typed_digest))
    }

    /// Read a complete recording, preserving errors as failures.
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
        let rewindable = file
            .stream_position()
            .and_then(|start| file.seek(SeekFrom::Start(start)))
            .is_ok();
        let recording = if rewindable {
            Self::read_from(&mut file)
        } else {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Self::decode(&bytes)
        };
        recording.with_context(|| format!("reading schedule {}", path.display()))
    }

    /// Decode `bytes` already read from `path`, which names them in errors as
    /// [`Self::read`] does.
    pub fn decode_from(path: &Path, bytes: &[u8]) -> anyhow::Result<Self> {
        Self::decode(bytes).with_context(|| format!("reading schedule {}", path.display()))
    }

    /// Serialize the same format with the conventional terminal newline.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = match (self, self.metadata().identity()) {
            (Self::Legacy(record), _) => encode_record(record)?.into_bytes(),
            (Self::Boundaries(record), None) => serde_json::to_vec(&Envelope {
                slice_boundary_recording: record.clone(),
            })?,
            (Self::Boundaries(record), Some(identity)) => serde_json::to_vec(&IdentityEnvelope {
                rcb_history: IdentityBody {
                    format: RCB_HISTORY_FORMAT,
                    identity: identity.into(),
                    recording: Envelope {
                        slice_boundary_recording: record.clone(),
                    },
                },
            })?,
        };
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// The same recording, stating `identity` for its coordinates.
    pub fn with_identity(mut self, identity: Option<HistoryIdentity>) -> Self {
        self.metadata_mut().identity = identity;
        self
    }

    /// The identity of the recording's RCB coordinates; `None` for a legacy
    /// file, whose producer is unknown.
    pub fn identity(&self) -> Option<&HistoryIdentity> {
        self.metadata().identity()
    }

    /// Write the complete serialized recording, without changing its format.
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        super::streaming::write_file(path, self)
    }

    /// Count every stored row by borrowing the fully parsed history. This
    /// repeats the existing metadata validator, never loads/clones its tape.
    pub fn inspect(&self) -> anyhow::Result<Inspection> {
        let metadata = self.metadata();
        metadata.validate().map_err(anyhow::Error::msg)?;
        let (format, stored_global_rows, event_rows, boundary_rows) = match self {
            Self::Legacy(record) => ("legacy", record.global.len(), record.global.len(), None),
            Self::Boundaries(record) => {
                if !metadata.global.is_empty() {
                    bail!("slice-boundary recording has a second, legacy global event stream");
                }
                let records = record.schedule.records();
                let events = records
                    .iter()
                    .filter(|row| {
                        matches!(row, detcore_model::schedule::boundary::Record::Event(_))
                    })
                    .count();
                (
                    "slice_boundaries",
                    records.len(),
                    events,
                    Some(records.len() - events),
                )
            }
        };
        Ok(Inspection {
            format,
            structural_validation: "existing format and history validators passed; not replay certification",
            stored_global_rows,
            event_rows,
            boundary_rows,
            epoch: metadata.epoch(),
            pipe_wake_latency: metadata.pipe_wake_latency(),
            signal_state_copy: metadata.signal_state_copy(),
            threads: metadata
                .per_thread
                .iter()
                .map(|(tid, history)| {
                    (
                        *tid,
                        ThreadInspection {
                            priority_rows: history.prio_changes.len(),
                            rcb_rows: history.preemption_rcbs.len(),
                            chaos_epoch_rows: history.chaos_epochs.len(),
                            callback_rows: metadata
                                .callback_checkpoints
                                .get(tid)
                                .map_or(0, Vec::len),
                            final_priority: history.final_prio,
                        },
                    )
                })
                .collect(),
        })
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
    /// (`PreemptionRecord::lists_its_time_skips`), by its timing format
    /// ([`Self::timing_format`]).
    pub fn lists_its_time_skips(&self) -> bool {
        super::lists_time_skips_in(self.timing_format())
    }

    /// Whether a replay of this recording charges a replayed switch-in under
    /// which guest code runs (`PreemptionRecord::charges_replayed_switch_ins`),
    /// by its timing format ([`Self::timing_format`]).
    pub fn charges_replayed_switch_ins(&self) -> bool {
        super::charges_replayed_switch_ins_in(self.timing_format())
    }

    /// The format whose timing rules the recording follows
    /// (`PreemptionRecord::timing_format`). A slice-boundary recording keeps
    /// its schedule trace in its boundary stream, not in its rows' record, so
    /// it follows its stated format whatever that record says of its rows.
    pub fn timing_format(&self) -> Option<u32> {
        match self {
            Self::Legacy(record) => record.timing_format(),
            Self::Boundaries(record) => record.preemptions.format_version(),
        }
    }

    /// The recording's format (`PreemptionRecord::format_version`).
    pub fn format_version(&self) -> Option<u32> {
        self.metadata().format_version()
    }

    /// What the recording states about its run's exclusive-loop
    /// rewrite, for replay admission
    /// (`super::reserved_input::validate_config_replay_inputs`).
    pub(super) fn recorded_rewrite_exclusives(&self) -> super::RecordedRewriteExclusives {
        self.metadata().recorded_rewrite_exclusives()
    }

    fn metadata(&self) -> &PreemptionRecord {
        match self {
            Self::Legacy(record) => record,
            Self::Boundaries(record) => &record.preemptions,
        }
    }

    fn metadata_mut(&mut self) -> &mut PreemptionRecord {
        match self {
            Self::Legacy(record) => record,
            Self::Boundaries(record) => &mut record.preemptions,
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

    /// The recording this history reads, for admission.
    pub(crate) fn recording(&self) -> &Recording {
        &self.0
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
        super::streaming::fmt_json(&self.0, output, true)
    }
}

#[cfg(test)]
pub(super) mod tests {

    /// Strict slice boundaries refuse --chaos-instruction-offset, so a
    /// slice-boundary recording whose events hold an offset is malformed:
    /// refused when built and when read, as an error rather than a format
    /// refusal. The same recording without the offset is accepted.
    #[test]
    fn a_boundary_recording_holding_an_instruction_offset_is_malformed() {
        let metadata = || -> PreemptionRecord {
            serde_json::from_str(
                &PreemptionWriter::new(None)
                    .with_epoch("2001-02-03T04:05:06Z".parse().unwrap())
                    .with_pipe_wake_latency(Some(0))
                    .with_signal_state_copy(false)
                    .into_string(),
            )
            .unwrap()
        };
        let offset = crate::resources::InstructionOffset {
            rcb_target: 40,
            instructions: 5,
        };
        let plain = SchedEvent::branches(DetTid::from_raw(3), 3);
        let with = plain.clone().with_instruction_offset(Some(offset));
        let error = BoundaryRecording::new(
            metadata(),
            Schedule::new(vec![Record::Event(with.clone())]).unwrap(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("malformed"), "{error:#}");

        let text = String::from_utf8(
            Recording::Boundaries(
                BoundaryRecording::new(
                    metadata(),
                    Schedule::new(vec![Record::Event(plain.clone())]).unwrap(),
                )
                .unwrap(),
            )
            .encode()
            .unwrap(),
        )
        .unwrap();
        Recording::decode(text.as_bytes()).unwrap();
        let plain_json = serde_json::to_string(&plain).unwrap();
        assert!(text.contains(&plain_json), "{text}");
        let spliced = text.replacen(&plain_json, &serde_json::to_string(&with).unwrap(), 1);
        let error = Recording::decode(spliced.as_bytes()).unwrap_err();
        assert!(format!("{error:#}").contains("malformed"), "{error:#}");
        assert!(
            !error
                .chain()
                .any(|cause| cause.is::<crate::preemptions::RecordFormatRefusal>()),
            "{error:#}"
        );

        // An offset in a thread's rows (a slice whose timer stopped it at a
        // syscall or trap leaves no offset on an event): refused when built
        // and when read, and the boundary writer refuses to record one.
        let tid = DetTid::from_raw(3);
        let priority = crate::scheduler::runqueue::DEFAULT_PRIORITY;
        let at = LogicalTime::from_nanos;
        let mut writer = PreemptionWriter::new(None)
            .with_epoch("2001-02-03T04:05:06Z".parse().unwrap())
            .with_pipe_wake_latency(Some(0))
            .with_signal_state_copy(false);
        writer.register_thread(tid, priority);
        writer.insert_reprioritization(tid, at(100), 41, priority, 5, Some(offset));
        let rows: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
        let error =
            BoundaryRecording::new(rows.clone(), Schedule::new(vec![]).unwrap()).unwrap_err();
        assert!(format!("{error:#}").contains("malformed"), "{error:#}");
        let mut envelope: serde_json::Value = serde_json::from_str(&text).unwrap();
        envelope["slice_boundary_recording"]["preemptions"] = serde_json::to_value(&rows).unwrap();
        let error = Recording::decode(envelope.to_string().as_bytes()).unwrap_err();
        assert!(format!("{error:#}").contains("malformed"), "{error:#}");
        assert!(
            !error
                .chain()
                .any(|cause| cause.is::<crate::preemptions::RecordFormatRefusal>()),
            "{error:#}"
        );
        let boundary_writer = std::panic::catch_unwind(|| {
            let mut writer = PreemptionWriter::new(None).with_slice_boundaries().unwrap();
            writer.register_thread(tid, priority);
            writer.insert_reprioritization(tid, at(100), 41, priority, 5, Some(offset));
        });
        assert!(boundary_writer.is_err());
    }

    /// A record marked incomplete states its format inside main's
    /// `incomplete_preemption_record` envelope, as a legacy record and as a
    /// slice-boundary recording's preemptions. The probe reads it there, so a
    /// newer format is refused as such, typed, by `validate_format` and, for a
    /// record that cannot be decoded, by the probe; a format this Hermit
    /// reads decodes as before.
    #[test]
    fn a_marked_record_states_its_format_inside_its_envelope() {
        let refused = |bytes: &[u8]| {
            Recording::decode(bytes).err().is_some_and(|error| {
                error
                    .chain()
                    .any(|cause| cause.is::<crate::preemptions::RecordFormatRefusal>())
            })
        };
        let marked = |format: u32| {
            serde_json::json!({"incomplete_preemption_record": {
                "version": 1,
                "replay_departed_at": 7,
                "record": {"per_thread": {}, "global": [], "format_version": format},
            }})
        };
        let legacy = marked(99).to_string();
        assert_eq!(probe_format(legacy.as_bytes()).unwrap().format, Some(99));
        assert!(refused(legacy.as_bytes()));
        let boundary = serde_json::json!({"slice_boundary_recording": {
            "preemptions": marked(99),
            "schedule": [],
        }})
        .to_string();
        assert_eq!(probe_format(boundary.as_bytes()).unwrap().format, Some(99));
        assert!(refused(boundary.as_bytes()));
        let newest = crate::preemptions::PREEMPTION_RECORD_FORMAT_MAX;
        let mut undecodable = marked(99);
        undecodable["incomplete_preemption_record"]["record"]["global"] = serde_json::json!(5);
        assert!(refused(undecodable.to_string().as_bytes()));
        undecodable["incomplete_preemption_record"]["record"]["format_version"] = newest.into();
        assert!(Recording::decode(undecodable.to_string().as_bytes()).is_err());
        assert!(!refused(undecodable.to_string().as_bytes()));
        let current = marked(newest).to_string();
        assert_eq!(
            probe_format(current.as_bytes()).unwrap().format,
            Some(newest)
        );
        let Recording::Legacy(record) = Recording::decode(current.as_bytes()).unwrap() else {
            panic!("a marked legacy record decodes as one");
        };
        assert_eq!(record.replay_departed_at(), Some(7));
    }

    /// A slice-boundary recording states its format inside its envelope
    /// alone: a `format_version` beside the envelope is the envelope
    /// decoder's to refuse, not a format refusal, and a null or malformed
    /// envelope reaches ordinary decoding. A legacy record states it at the
    /// top level.
    #[test]
    fn an_envelope_states_its_format_only_inside_it() {
        let refused = |bytes: &[u8]| {
            Recording::decode(bytes).err().is_some_and(|error| {
                error
                    .chain()
                    .any(|cause| cause.is::<crate::preemptions::RecordFormatRefusal>())
            })
        };
        let envelope: serde_json::Value =
            serde_json::from_slice(&sample(false).encode().unwrap()).unwrap();
        // Beside the envelope: read as none, refused by the envelope decoder.
        let mut beside = envelope.clone();
        beside["format_version"] = serde_json::json!(99);
        let beside = beside.to_string();
        assert_eq!(probe_format(beside.as_bytes()).unwrap().format, None);
        let error = Recording::decode(beside.as_bytes()).unwrap_err();
        assert!(!refused(beside.as_bytes()), "{error:#}");
        // Inside it: refused by format.
        let mut inside = envelope.clone();
        inside["slice_boundary_recording"]["preemptions"]["format_version"] = serde_json::json!(99);
        let inside = inside.to_string();
        assert_eq!(probe_format(inside.as_bytes()).unwrap().format, Some(99));
        assert!(refused(inside.as_bytes()));
        // A null or malformed envelope: ordinary decoding reports it, never
        // a format refusal (the probe reads none, or cannot read it at all).
        for broken in [
            serde_json::json!({"slice_boundary_recording": null, "format_version": 99}),
            serde_json::json!({"slice_boundary_recording": 7, "format_version": 99}),
            serde_json::json!({"slice_boundary_recording": {"preemptions": {"format_version": "x"}}}),
        ] {
            let broken = broken.to_string();
            assert_eq!(
                probe_format(broken.as_bytes()).map_or(None, |probed| probed.format),
                None,
                "{broken}"
            );
            assert!(Recording::decode(broken.as_bytes()).is_err(), "{broken}");
            assert!(!refused(broken.as_bytes()), "{broken}");
        }
        // A duplicated `format_version` (top level or in the envelope's
        // preemptions) or `preemptions`, in either order: an error of the
        // strict decoders, never a format refusal.
        let preemptions = |format: u32| {
            format!(r#""preemptions":{{"per_thread":{{}},"global":[],"format_version":{format}}}"#)
        };
        let mut duplicated = Vec::new();
        for (first, second) in [(99, 1), (1, 99)] {
            let (a, b) = (
                format!(r#""format_version":{first}"#),
                format!(r#""format_version":{second}"#),
            );
            duplicated.push(format!(r#"{{"per_thread":{{}},"global":[],{a},{b}}}"#));
            duplicated.push(format!(
                r#"{{"slice_boundary_recording":{{"preemptions":{{"per_thread":{{}},"global":[],{a},{b}}},"schedule":[]}}}}"#
            ));
            duplicated.push(format!(
                r#"{{"slice_boundary_recording":{{{},{},"schedule":[]}}}}"#,
                preemptions(first),
                preemptions(second)
            ));
        }
        for record in duplicated {
            assert!(probe_format(record.as_bytes()).is_err(), "{record}");
            assert!(Recording::decode(record.as_bytes()).is_err(), "{record}");
            assert!(!refused(record.as_bytes()), "{record}");
        }
        // A legacy record: the top level.
        let legacy = serde_json::json!({"per_thread": {}, "global": [], "format_version": 99});
        assert_eq!(
            probe_format(legacy.to_string().as_bytes()).unwrap().format,
            Some(99)
        );
        assert!(refused(legacy.to_string().as_bytes()));
    }
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

    pub fn sample(markers: bool) -> Recording {
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
    fn streaming_dispatch_preserves_the_original_value_and_typed_contracts() {
        fn original(bytes: &[u8]) -> anyhow::Result<Recording> {
            // The parent dispatch was the derived two-marker struct probe,
            // followed by typed parsing and the format check. In particular,
            // unknown values are skipped as IgnoredAny rather than first
            // being materialized as serde_json::Value.
            let recording =
                if serde_json::from_slice::<FormatProbe>(bytes)?.slice_boundary_recording {
                    let envelope: Envelope = serde_json::from_slice(bytes)?;
                    Recording::Boundaries(envelope.slice_boundary_recording)
                } else {
                    let record: PreemptionRecord = serde_json::from_slice(bytes)?;
                    record.validate_contents().map_err(anyhow::Error::msg)?;
                    Recording::Legacy(record)
                };
            recording.metadata().validate_format().map_err(|refusal| {
                anyhow::Error::new(crate::preemptions::RecordFormatRefusal(refusal))
            })?;
            Ok(recording)
        }
        let mut inputs: Vec<Vec<u8>> = [
            r#"{"per_thread":{},"global":[]}"#,
            r#"[{},[],null,null,null,false]"#,
            r#"{"per_thread":{},"global":[],"extra":{"slice_boundary_recording":null}}"#,
            r#"{"per_thread":{},"global":[],"extra":1,"extra":2}"#,
            r#"{"per_thread":{},"global":[],"epoch":null,"epoch":null}"#,
            r#"{"per_thread":{},"global":[],"slice_boundary_recording":null}"#,
            r#"{"per_thread":{},"global":[],"slice_boundary_recording":null,"slice_boundary_recording":{}}"#,
            r#"{"per_thread":{},"global":[],"extra":1e999}"#,
            r#"{"per_thread":{},"global":[],"extra":"\ud800"}"#,
            r#"{"per_thread":{},"global":[],"extra":{"$serde_json::private::RawValue":"null"}}"#,
            r#"{"per_thread":{},"global":[],"extra":{"$serde_json::private::RawValue":"bad"}}"#,
            r#"{"per_thread":{},"global":[],"extra":{"$serde_json::private::RawValue":7}}"#,
            r#"{"$serde_json::private::RawValue":"null","per_thread":{},"global":[]}"#,
            r#"{"per_thread":{},"global":[]} true"#,
            r#"{"per_thread":{},"global":[]} trailing"#,
            "null", "[]", "false", "3", "\"text\"", "",
        ].into_iter().map(|text| text.as_bytes().to_vec()).collect();
        inputs.push(
            format!(
                "{{\"per_thread\":{{}},\"global\":[],\"extra\":{}0{}}}",
                "[".repeat(160),
                "]".repeat(160)
            )
            .into_bytes(),
        );
        inputs.push(sample(false).encode().unwrap());
        inputs.push(sample(true).encode().unwrap());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.json");
        for bytes in &inputs {
            let expected = original(bytes);
            std::fs::write(&path, bytes).unwrap();
            for actual in [Recording::decode(bytes), Recording::read(&path)] {
                match (&expected, &actual) {
                    (Ok(expected), Ok(actual)) => assert_eq!(actual, expected),
                    (Err(_), Err(_)) => {}
                    (expected, actual) => {
                        panic!("changed acceptance for {bytes:?}: {expected:?} / {actual:?}")
                    }
                }
            }
        }
        assert!(Recording::decode(br#"[{},[],null,null,null,false]"#).is_err());
        assert!(Recording::decode(br#"{"per_thread":{},"global":[],"extra":1e999}"#).is_ok());
    }

    #[test]
    fn short_reads_full_eof_and_changed_passes_use_the_actual_reader() {
        struct Short {
            input: Cursor<Vec<u8>>,
            maximum: usize,
            largest_request: usize,
        }
        impl Read for Short {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.largest_request = self.largest_request.max(bytes.len());
                let count = self.maximum.min(bytes.len());
                self.input.read(&mut bytes[..count])
            }
        }
        impl Seek for Short {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                self.input.seek(position)
            }
        }
        for recording in [Recording::Legacy(PreemptionRecord::default()), sample(true)] {
            let mut bytes = recording.encode().unwrap();
            bytes.extend_from_slice(b" \r\n\t");
            for maximum in 1..=7 {
                let mut input = Short {
                    input: Cursor::new(bytes.clone()),
                    maximum,
                    largest_request: 0,
                };
                assert_eq!(Recording::read_from(&mut input).unwrap(), recording);
                assert!(input.largest_request <= 8192);
            }
            assert!(Recording::decode(&bytes[..bytes.len() - 8]).is_err());
        }
        struct Changed {
            input: Cursor<Vec<u8>>,
            second: Vec<u8>,
        }
        impl Read for Changed {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.input.read(bytes)
            }
        }
        impl Seek for Changed {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                if matches!(position, SeekFrom::Start(0)) {
                    self.input = Cursor::new(self.second.clone());
                }
                self.input.seek(position)
            }
        }
        let first = b"{\"per_thread\":{},\"global\":[]} ".to_vec();
        let mut input = Changed {
            input: Cursor::new(first),
            second: b"{\"per_thread\":{},\"global\":[]}\n".to_vec(),
        };
        assert!(
            Recording::read_from(&mut input)
                .unwrap_err()
                .to_string()
                .contains("changed between")
        );
    }

    #[test]
    fn nonseekable_path_and_independent_metadata_reads_remain_supported() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("history.fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let bytes = Recording::Legacy(PreemptionRecord::default())
            .encode()
            .unwrap();
        let writer_path = fifo.clone();
        let writer = std::thread::spawn(move || std::fs::write(writer_path, bytes).unwrap());
        assert_eq!(
            Recording::read(&fifo).unwrap(),
            Recording::Legacy(PreemptionRecord::default())
        );
        writer.join().unwrap();
        // Metadata has always ignored unrelated legacy fields, even where the
        // full Recording Value probe rejects their numeric representation.
        let path = directory.path().join("settings.json");
        std::fs::write(
            &path,
            br#"{"epoch":null,"extra":1e999,"pipe_wake_latency":"bad"}"#,
        )
        .unwrap();
        assert_eq!(super::super::read_recorded_epoch(&path), Ok(None));
        assert!(super::super::read_recorded_pipe_wake_latency(&path).is_err());
        assert_eq!(
            super::super::read_recorded_signal_state_copy(&path),
            Ok(None)
        );
    }

    #[test]
    fn inspection_counts_rows_and_keeps_legacy_boundary_capabilities_distinct() {
        let legacy = Recording::Legacy(PreemptionRecord::default());
        let summary = legacy.inspect().unwrap();
        assert_eq!(summary.format, "legacy");
        assert_eq!(summary.boundary_rows, None);
        let summary = sample(true).inspect().unwrap();
        assert_eq!(summary.format, "slice_boundaries");
        assert_eq!(summary.stored_global_rows, 1);
        assert_eq!(summary.event_rows, 0);
        assert_eq!(summary.boundary_rows, Some(1));
        assert_eq!(summary.pipe_wake_latency, Some(0));
        assert_eq!(summary.signal_state_copy, Some(false));
    }

    #[test]
    fn streamed_boundary_files_match_the_original_owned_envelope_bytes() {
        #[derive(Serialize)]
        struct OriginalEnvelope {
            slice_boundary_recording: BoundaryWire,
        }
        let directory = tempfile::tempdir().unwrap();
        for markers in [false, true] {
            let recording = sample(markers);
            let Recording::Boundaries(boundary) = &recording else {
                unreachable!()
            };
            let mut expected = serde_json::to_vec(&OriginalEnvelope {
                slice_boundary_recording: BoundaryWire {
                    preemptions: boundary.preemptions.clone(),
                    schedule: boundary.schedule.clone(),
                },
            })
            .unwrap();
            expected.push(b'\n');
            let path = directory.path().join(format!("{markers}.json"));
            recording.write(&path).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), expected);
            assert_eq!(recording.encode().unwrap(), expected);
        }
    }

    /// Main's admission of a record it can decode is unchanged: a newer
    /// format, a stated 0 and an explicit 1 are refused with
    /// `validate_format`'s message, byte for byte, and a newer record that
    /// fails another check reports that check, as main does. The format probe
    /// fires only when a record cannot be decoded: a newer one is then refused
    /// by its format, and a current one keeps its decoding error. Applying the
    /// probe before decoding fails this test: the empty list of time skips
    /// and the zero-step offset would be reported as a format refusal.
    #[test]
    fn the_format_probe_fires_only_when_a_record_cannot_be_decoded() {
        let newest = crate::preemptions::PREEMPTION_RECORD_FORMAT_MAX;
        let refusal = |bytes: &[u8]| {
            Recording::decode(bytes)
                .unwrap_err()
                .downcast_ref::<crate::preemptions::RecordFormatRefusal>()
                .map(|refusal| refusal.0.clone())
        };
        for format in [newest + 1, 0, 1] {
            let main = format!(
                "record format Some({format}) with 0 time skips: this Hermit reads formats 1 to \
                 {newest}, and time skips need format 2 or later"
            );
            let legacy =
                serde_json::json!({"per_thread": {}, "global": [], "format_version": format});
            assert_eq!(refusal(legacy.to_string().as_bytes()), Some(main.clone()));
            let boundary = serde_json::json!({"slice_boundary_recording": {
                "preemptions": legacy, "schedule": [],
            }});
            assert_eq!(refusal(boundary.to_string().as_bytes()), Some(main));
        }
        // Decodable, newer, and refused for another reason first: main's
        // message for that reason, not a format refusal.
        let empty_skips = serde_json::json!({
            "per_thread": {}, "global": [], "format_version": newest + 1, "time_skips": [],
        });
        assert_eq!(
            refusal(empty_skips.to_string().as_bytes()),
            Some("an empty list of time skips is written as no list".to_owned())
        );
        let mut zero_steps = serde_json::to_value(PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(DetTid::from_raw(3), 20).with_instruction_offset(Some(
                crate::resources::InstructionOffset {
                    rcb_target: 18,
                    instructions: 0,
                },
            )),
        ]))
        .unwrap();
        zero_steps["format_version"] = (newest + 1).into();
        let error = Recording::decode(zero_steps.to_string().as_bytes()).unwrap_err();
        assert!(
            !error.is::<crate::preemptions::RecordFormatRefusal>(),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains("zero steps"), "{error:#}");
        // Undecodable: refused by its format when it states a newer one, with
        // the same message `validate_format` gives a decodable record; a
        // current one keeps its decoding error.
        for (format, refused) in [(newest + 1, true), (newest, false)] {
            let broken =
                serde_json::json!({"per_thread": {}, "global": 5, "format_version": format});
            assert!(
                serde_json::from_slice::<PreemptionRecord>(broken.to_string().as_bytes()).is_err()
            );
            let boundary = serde_json::json!({"slice_boundary_recording": {
                "preemptions": broken, "schedule": [],
            }});
            for bytes in [broken.to_string(), boundary.to_string()] {
                let error = Recording::decode(bytes.as_bytes()).unwrap_err();
                match error.downcast_ref::<crate::preemptions::RecordFormatRefusal>() {
                    Some(refusal) => {
                        assert!(refused, "{bytes}: {error:#}");
                        assert_eq!(
                            refusal.0,
                            crate::preemptions::unread_format_message(Some(format), 0)
                        );
                    }
                    None => assert!(!refused, "{bytes}: {error:#}"),
                }
            }
        }
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
        let identity_probe = |text: &str| FormatProbe::has_rcb_history(text.as_bytes()).unwrap();
        assert!(!identity_probe(r#"{"per_thread": {}, "global": []}"#));
        assert!(identity_probe(r#"{"rcb_history": {"format": 2}}"#));
        assert!(identity_probe(r#"{"rcb_history": null}"#));
        assert!(!identity_probe(
            r#"{"per_thread": {"3": {"rcb_history": 1}}, "global": []}"#
        ));
        assert!(
            FormatProbe::has_rcb_history(br#"{"rcb_history": null, "rcb_history": null}"#).is_err()
        );
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

    /// A boundary replay takes no background starts, so a boundary recording
    /// that notes any is refused by name, by the constructor and by the reader
    /// of a serialized one, beside a valid boundary schedule. A legacy record
    /// keeps a start at either end of its events. The slice-boundary writer
    /// states no format that claims the starts' list, keeps one this Hermit
    /// does not read as stated, and refuses to start after noting a start.
    #[test]
    fn a_boundary_recording_with_background_starts_is_refused_by_name() {
        use crate::preemptions::BACKGROUND_STARTS_FORMAT;
        use crate::preemptions::BackgroundStart;
        use crate::preemptions::PREEMPTION_RECORD_FORMAT_MAX;
        use crate::preemptions::format_without_background_starts;
        let Recording::Boundaries(valid) = sample(true) else {
            unreachable!()
        };
        let start = BackgroundStart {
            after: 0,
            thread: DetTid::from_raw(3),
            call: 1,
        };
        let mut metadata = valid.preemptions.clone();
        metadata.format_version = Some(BACKGROUND_STARTS_FORMAT);
        metadata.background_starts.push(start);
        metadata.validate_contents().unwrap();
        let error = BoundaryRecording::new(metadata, valid.schedule.clone()).unwrap_err();
        assert!(
            error.to_string().contains("notes background starts"),
            "{error}"
        );
        let mut wire: serde_json::Value =
            serde_json::from_slice(&sample(true).encode().unwrap()).unwrap();
        let preemptions = &mut wire["slice_boundary_recording"]["preemptions"];
        preemptions["format_version"] = BACKGROUND_STARTS_FORMAT.into();
        preemptions["background_starts"] = serde_json::to_value(vec![start]).unwrap();
        let error = Recording::decode(&serde_json::to_vec(&wire).unwrap()).unwrap_err();
        assert!(
            format!("{error:#}").contains("notes background starts"),
            "{error:#}"
        );
        let events = vec![SchedEvent::branches(DetTid::from_raw(3), 1)];
        for after in [0, 1] {
            let mut legacy = PreemptionRecord::from_sched_events(events.clone())
                .with_format(Some(BACKGROUND_STARTS_FORMAT));
            legacy
                .background_starts
                .push(BackgroundStart { after, ..start });
            let bytes = Recording::Legacy(legacy.clone()).encode().unwrap();
            assert_eq!(
                Recording::decode(&bytes).unwrap(),
                Recording::Legacy(legacy)
            );
        }
        let boundary_format = |writer: PreemptionWriter| writer.inner.format_version;
        for format in [
            BACKGROUND_STARTS_FORMAT,
            PREEMPTION_RECORD_FORMAT_MAX + 1,
            0,
        ] {
            let expected = format_without_background_starts(Some(format));
            let before = PreemptionWriter::new(None)
                .with_format(Some(format))
                .with_slice_boundaries()
                .unwrap();
            assert_eq!(boundary_format(before), expected, "format {format}");
            let after = PreemptionWriter::new(None)
                .with_slice_boundaries()
                .unwrap()
                .with_format(Some(format));
            assert_eq!(boundary_format(after), expected, "format {format}");
        }
        let noting = PreemptionWriter::new(None)
            .with_slice_boundaries()
            .unwrap()
            .noting_background_starts();
        assert_eq!(
            boundary_format(noting),
            format_without_background_starts(Some(PREEMPTION_RECORD_FORMAT_MAX))
        );
        let mut noted = PreemptionWriter::new(None).noting_background_starts();
        noted.insert_background_start(DetTid::from_raw(3), 1);
        assert!(noted.with_slice_boundaries().is_err());
    }

    #[test]
    fn a_second_event_stream_is_not_accepted_as_metadata() {
        let metadata = PreemptionRecord::from_sched_events_with_identity(
            vec![SchedEvent::branches(DetTid::from_raw(3), 1)],
            None,
        );
        assert!(BoundaryRecording::new(metadata, Schedule::new(vec![]).unwrap()).is_err());
    }

    #[test]
    fn legacy_bytes_and_editor_access_remain_legacy() {
        let legacy = PreemptionRecord::from_sched_events_with_identity(
            vec![SchedEvent::branches(DetTid::from_raw(3), 2)],
            None,
        )
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

    /// The default writer emits the legacy form, never the boundary envelope.
    /// It also says that its run logged readmissions, none here
    /// (`rejoins: []`), which a record built from events alone does not.
    #[test]
    fn writer_default_still_emits_exact_legacy_bytes() {
        let mut writer = PreemptionWriter::new(None);
        let event = SchedEvent::branches(DetTid::from_raw(3), 7);
        writer.insert_schedevent(event.clone());
        let mut legacy = PreemptionRecord::from_sched_events_with_identity(vec![event], None);
        legacy.rejoins = Some(Vec::new());
        assert_eq!(writer.into_string(), legacy.to_string());
    }

    /// A slice-boundary recording stops at every timeslice end, so it marks
    /// no event `stopless`: an event so marked is refused on read, as an
    /// invalid event, whatever else it holds.
    #[test]
    fn a_stopless_mark_is_rejected_on_new_format_read() {
        let event = SchedEvent::branches(DetTid::from_raw(3), 1).taken_without_stop();
        assert!(Schedule::new(vec![Record::Event(event.clone())]).is_err());
        let mut wire: serde_json::Value =
            serde_json::from_slice(&sample(false).encode().unwrap()).unwrap();
        let schedule = &mut wire["slice_boundary_recording"]["schedule"];
        schedule["record_count"] = 1.into();
        schedule["records"] = serde_json::json!([Record::Event(event)]);
        let error = Recording::decode(&serde_json::to_vec(&wire).unwrap()).unwrap_err();
        assert!(format!("{error:#}").contains("`stopless`"), "{error:#}");
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

#[cfg(test)]
mod identity_tests {
    use super::tests::sample;
    use super::*;
    use crate::types::DetTid;
    use crate::types::SchedEvent;

    fn known() -> HistoryIdentity {
        HistoryIdentity::Known(reverie::pmu_config::RcbMetric::ConditionalBranchesRetiredUserV1)
    }

    /// The legacy reader of the base build: `PreemptionRecord`'s own serde
    /// shape, and the boundary dispatch that falls back to it.
    fn base_build_decode(bytes: &[u8]) -> anyhow::Result<()> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        if value.get("slice_boundary_recording").is_some() {
            serde_json::from_slice::<Envelope>(bytes)?;
        } else {
            serde_json::from_slice::<PreemptionRecord>(bytes)?;
        }
        Ok(())
    }

    #[test]
    fn an_identity_recording_round_trips_with_its_identity() {
        for identity in [known(), HistoryIdentity::Unqualified("claim-unset".into())] {
            let legacy = PreemptionRecord::from_sched_events_with_identity(
                vec![SchedEvent::branches(DetTid::from_raw(3), 7)],
                Some(identity.clone()),
            )
            .with_pipe_wake_latency(Some(0));
            let encoded = legacy.to_string();
            assert!(
                encoded.starts_with("{\"rcb_history\":{\"format\":2,"),
                "{encoded}"
            );
            let decoded = Recording::decode(encoded.as_bytes()).unwrap();
            assert_eq!(decoded.identity(), Some(&identity));
            assert_eq!(decoded, Recording::Legacy(legacy.clone()));
            assert_eq!(
                decoded.encode().unwrap(),
                format!("{encoded}\n").into_bytes()
            );

            let Recording::Boundaries(mut boundaries) = sample(true) else {
                unreachable!()
            };
            boundaries.preemptions.identity = Some(identity.clone());
            let boundaries = Recording::Boundaries(boundaries);
            let bytes = boundaries.encode().unwrap();
            let again = Recording::decode(&bytes).unwrap();
            assert_eq!(again.identity(), Some(&identity));
            assert_eq!(again, boundaries);
            assert!(again.require_strict_mode(true).is_ok());
        }
    }

    #[test]
    fn the_base_builds_reader_refuses_every_identity_recording() {
        let empty = PreemptionRecord::from_sched_events_with_identity(vec![], Some(known()));
        let Recording::Boundaries(mut boundaries) = sample(false) else {
            unreachable!()
        };
        boundaries.preemptions.identity = Some(known());
        for bytes in [
            empty.to_string().into_bytes(),
            Recording::Boundaries(boundaries).encode().unwrap(),
        ] {
            assert!(
                base_build_decode(&bytes).is_err(),
                "{}",
                String::from_utf8_lossy(&bytes)
            );
            assert!(Recording::decode(&bytes).is_ok());
        }
    }

    #[test]
    fn a_legacy_file_states_no_identity() {
        for recording in [
            Recording::Legacy(PreemptionRecord::from_sched_events_with_identity(
                vec![],
                None,
            )),
            sample(true),
        ] {
            let decoded = Recording::decode(&recording.encode().unwrap()).unwrap();
            assert_eq!(decoded.identity(), None);
        }
    }

    #[test]
    fn a_malformed_identity_recording_never_falls_back_to_legacy() {
        let good: serde_json::Value = serde_json::from_str(
            &PreemptionRecord::from_sched_events_with_identity(vec![], Some(known())).to_string(),
        )
        .unwrap();
        let mutate = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut value = good.clone();
            f(&mut value);
            serde_json::to_vec(&value).unwrap()
        };
        for bytes in [
            mutate(&|v| v["rcb_history"]["format"] = 3.into()),
            mutate(&|v| v["rcb_history"]["identity"] = "rcb:cond-retired:user:v2".into()),
            mutate(&|v| v["rcb_history"]["identity"] = serde_json::json!({"unqualified": 1})),
            mutate(&|v| v["rcb_history"]["extra"] = 1.into()),
            mutate(&|v| v["per_thread"] = serde_json::json!({})),
            mutate(&|v| {
                let inner = v["rcb_history"].clone();
                v["rcb_history"]["recording"] = serde_json::json!({ "rcb_history": inner });
            }),
            mutate(&|v| v["rcb_history"]["recording"] = serde_json::json!({"global": []})),
        ] {
            assert!(
                Recording::decode(&bytes).is_err(),
                "{}",
                String::from_utf8_lossy(&bytes)
            );
        }
        let text =
            PreemptionRecord::from_sched_events_with_identity(vec![], Some(known())).to_string();
        let duplicated = text.replacen("\"format\":2,", "\"format\":2,\"format\":2,", 1);
        assert_ne!(duplicated, text);
        assert!(Recording::decode(duplicated.as_bytes()).is_err());
    }

    #[test]
    fn transformations_keep_the_identity() {
        let record = PreemptionRecord::from_sched_events_with_identity(
            vec![SchedEvent::branches(DetTid::from_raw(3), 7)],
            Some(known()),
        );
        assert_eq!(record.clone_preemptions_only().identity(), Some(&known()));
        assert_eq!(record.normalize().identity(), Some(&known()));
        assert_eq!(
            record.with_latest_preempt_removed().identity(),
            Some(&known())
        );
        let mut recording = Recording::Legacy(record);
        recording.strip_times();
        let reread = Recording::decode(&recording.encode().unwrap()).unwrap();
        assert_eq!(reread.identity(), Some(&known()));
        assert_eq!(reread, recording);
    }

    /// A record with two priority rows of tid 7, the second ended by a write
    /// prehook checkpoint at the retained SPR record's coordinates, and
    /// RCB-bearing events, as a writer emits it with or without `identity`.
    fn with_rows_and_checkpoint(identity: Option<HistoryIdentity>) -> PreemptionRecord {
        use detcore_model::schedule::Op;
        use reverie::syscalls::Sysno;

        use crate::DEFAULT_PRIORITY;
        use crate::preemptions::PreemptionWriter;
        use crate::preemptions::callback_history::Origin;
        use crate::preemptions::callback_history::Site;
        use crate::slice_boundary::callbacks::Callback;
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
        // Progress counts events, so the write prehook's endpoint is 154
        // past the raw RCB count.
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
        Recording::decode(writer.into_string().as_bytes())
            .unwrap()
            .into_legacy("test")
            .unwrap()
    }

    /// Every transformation carries the identity and otherwise produces
    /// exactly what it produces from the same record without one: every
    /// per-thread row, callback checkpoint, event and RCB is unchanged.
    #[test]
    fn transformations_change_nothing_but_carry_the_identity() {
        let with = with_rows_and_checkpoint(Some(known()));
        let without = with_rows_and_checkpoint(None);
        let tid = DetTid::from_raw(7);
        assert_eq!(with.per_thread[&tid].prio_changes.len(), 2);
        assert_eq!(with.callback_checkpoints[&tid].len(), 1);
        assert_eq!(with.global.len(), 3);
        assert_eq!(with.identity(), Some(&known()));
        assert_eq!(with.clone().with_identity(None), without);
        type Transformation = fn(&PreemptionRecord) -> PreemptionRecord;
        let transformations: [(&str, Transformation); 3] = [
            (
                "clone_preemptions_only",
                PreemptionRecord::clone_preemptions_only,
            ),
            ("normalize", PreemptionRecord::normalize),
            (
                "with_latest_preempt_removed",
                PreemptionRecord::with_latest_preempt_removed,
            ),
        ];
        for (name, transform) in transformations {
            let carried = transform(&with);
            assert_eq!(carried.identity(), Some(&known()), "{name}");
            assert_eq!(carried.with_identity(None), transform(&without), "{name}");
        }
        let reloaded = |record: &PreemptionRecord| {
            let mut recording = Recording::Legacy(record.clone());
            recording.strip_times();
            Recording::decode(&recording.encode().unwrap()).unwrap()
        };
        let carried = reloaded(&with);
        assert_eq!(carried.identity(), Some(&known()));
        assert_eq!(carried.with_identity(None), reloaded(&without));
        let Recording::Boundaries(sampled) = sample(true) else {
            panic!("not a boundary recording")
        };
        let wrapped = |record: &PreemptionRecord| {
            let recording = Recording::Boundaries(
                BoundaryRecording::new(record.clone_preemptions_only(), sampled.schedule().clone())
                    .unwrap(),
            );
            Recording::decode(&recording.encode().unwrap()).unwrap()
        };
        let carried = wrapped(&with);
        assert_eq!(carried.identity(), Some(&known()));
        assert_eq!(carried.with_identity(None), wrapped(&without));
    }
}
