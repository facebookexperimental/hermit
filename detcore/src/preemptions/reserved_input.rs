/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Provenance for the private input mapping, at the standalone JSON boundary.
//!
//! None of these types is a Config, ThreadHistory, binary-event or RPC field.
//! An imported value is preserved verbatim as JSON data, including `null` and
//! values from a future producer. Only Chosen execution interprets it.

use detcore_model::pid::GuestIds;
use serde::Deserialize;
use serde::Serialize;

use super::PreemptionRecord;

const CONTRACT: &str = "sealed-private-input-v1";
const BASE: &str = "0x71010000";

/// An optional JSON member, distinguishing absence from a present null.
///
/// Construction and editing never confer provenance. Values are imported from
/// a recording or supplied to the live writer by authenticated backend setup.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct ReservedInputMetadata(Option<serde_json::Value>);

impl ReservedInputMetadata {
    pub(super) fn is_missing(&self) -> bool {
        self.0.is_none()
    }

    /// The stored JSON value. `None` is an absent member, not a present null.
    pub fn as_json(&self) -> Option<&serde_json::Value> {
        self.0.as_ref()
    }

    /// Require equal recognized provenance when combining Chosen inputs.
    /// Editors call this before choosing an unchanged endpoint as well.
    pub fn require_same_for_chosen(&self, other: &Self) -> Result<(), RecordAdmissionRefusal> {
        let first = self.recognized("first transformation input")?;
        let second = other.recognized("second transformation input")?;
        if first != second {
            return Err(provenance_refusal(
                "transformation inputs",
                "mixed reserved-input layouts",
            ));
        }
        Ok(())
    }

    fn recognized(&self, source: &str) -> Result<ReservedInputLayout, RecordAdmissionRefusal> {
        let value = self.0.as_ref().ok_or_else(|| {
            provenance_refusal(
                source,
                "missing reserved_input provenance (legacy or unqualified recording)",
            )
        })?;
        let layout: ReservedInputLayout =
            serde_json::from_value(value.clone()).map_err(|error| {
                provenance_refusal(
                    source,
                    &format!("malformed or unknown reserved_input descriptor: {error}"),
                )
            })?;
        layout.require_supported(source)?;
        Ok(layout)
    }

    pub(super) fn require_matching_layout(
        &self,
        source: &str,
        expected: &ReservedInputLayout,
    ) -> Result<(), RecordAdmissionRefusal> {
        let actual = self.recognized(source)?;
        if actual != *expected {
            return Err(provenance_refusal(
                source,
                &format!(
                    "reserved-input layout differs: recorded ABI {} at {} with {} bytes, runtime ABI {} at {} with {} bytes",
                    actual.abi,
                    actual.base,
                    actual.page_bytes,
                    expected.abi,
                    expected.base,
                    expected.page_bytes,
                ),
            ));
        }
        Ok(())
    }
}

impl Serialize for ReservedInputMetadata {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ReservedInputMetadata {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Option<Value> would collapse a present null into absence.
        serde_json::Value::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}

/// A recognized layout, supplied to the live writer only at authenticated
/// target-image setup. The constructor is crate-private so CLI editors cannot
/// stamp a record from requested configuration or a host page-size probe.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReservedInputLayout {
    contract: String,
    base: String,
    page_bytes: u64,
    abi: String,
}

impl ReservedInputLayout {
    /// Convert actual backend setup facts, never a requested layout, to the
    /// JSON contract. The caller owns the setup receipt and its target-image
    /// lifetime; this conversion alone is not that authority.
    pub(crate) fn from_authenticated_setup(
        abi: &str,
        base: u64,
        page_bytes: u64,
    ) -> Result<Self, RecordAdmissionRefusal> {
        let layout = Self {
            contract: CONTRACT.to_owned(),
            base: format!("0x{base:x}"),
            page_bytes,
            abi: abi.to_owned(),
        };
        layout.require_supported("authenticated target setup")?;
        Ok(layout)
    }

    fn require_supported(&self, source: &str) -> Result<(), RecordAdmissionRefusal> {
        if self.contract != CONTRACT {
            return Err(provenance_refusal(
                source,
                "unknown reserved-input contract",
            ));
        }
        if self.base != BASE
            || !matches!(self.abi.as_str(), "x86_64" | "aarch64")
            || !matches!(self.page_bytes, 4096 | 16384 | 65536)
        {
            return Err(provenance_refusal(
                source,
                "unsupported reserved-input ABI, address or page extent",
            ));
        }
        Ok(())
    }

    fn native() -> Result<Self, RecordAdmissionRefusal> {
        let abi = std::env::consts::ARCH;
        // Admission observes the native ABI granule; it neither chooses a turn
        // nor changes guest virtual time. Setup later authenticates the VMA.
        let page_bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_bytes = u64::try_from(page_bytes).map_err(|_| {
            provenance_refusal("runtime", "cannot establish the native page extent")
        })?;
        Self::from_authenticated_setup(abi, 0x7101_0000, page_bytes)
    }

    fn metadata(&self) -> ReservedInputMetadata {
        ReservedInputMetadata(Some(serde_json::to_value(self).expect("layout serializes")))
    }
}

/// A schedule admission policy failure, not malformed JSON or a replay desync.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RecordAdmissionRefusal(pub String);

impl std::fmt::Display for RecordAdmissionRefusal {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(&self.0)
    }
}

impl std::error::Error for RecordAdmissionRefusal {}

fn provenance_refusal(source: &str, reason: &str) -> RecordAdmissionRefusal {
    RecordAdmissionRefusal(format!(
        "HERMIT_CHOSEN_JSON_INPUT_PROVENANCE: {source}: {reason}; Chosen replay requires a recording produced with this reserved-input layout. The guest has not started. Record the run again with this Hermit, or replay the original with its producing Hermit; editing or overriding provenance cannot qualify an old recording."
    ))
}

impl PreemptionRecord {
    /// Preserve the source's optional metadata on an edit. This is not a live
    /// producer certificate; unmarked/unknown input remains unmarked/unknown.
    pub fn with_reserved_input(mut self, metadata: ReservedInputMetadata) -> Self {
        self.reserved_input = metadata;
        self
    }

    /// The exact optional standalone JSON metadata, without interpreting it.
    pub fn reserved_input(&self) -> &ReservedInputMetadata {
        &self.reserved_input
    }

    /// Reconcile numbering and private-input provenance on the complete
    /// decoded record that an authoritative replay loader will install.
    /// A record of the removed stripped-file heuristic rewrite is
    /// refused here, on the decoded bytes every loader installs: no
    /// supported rewrite reproduces its instructions, whatever the
    /// replay's own rewrite, which `validate_config_replay_inputs`
    /// compares in full at launch.
    pub fn validate_replay_inputs(
        &self,
        source: &str,
        guest_ids: GuestIds,
    ) -> Result<(), RecordAdmissionRefusal> {
        if self.rewrite_exclusives_stripped_heuristic {
            let refusal = super::rewrite_exclusives_mismatch(
                source,
                self.recorded_rewrite_exclusives(),
                None,
            )
            .expect("a heuristic record is always refused");
            return Err(RecordAdmissionRefusal(refusal));
        }
        if let Some(reason) =
            super::guest_ids_mismatch(source, self.guest_ids.unwrap_or_default(), guest_ids)
        {
            return Err(RecordAdmissionRefusal(reason));
        }
        if matches!(guest_ids, GuestIds::Chosen { .. }) {
            self.reserved_input
                .require_matching_layout(source, &ReservedInputLayout::native()?)?;
        }
        Ok(())
    }

    /// Validate a combination before any endpoint or identity shortcut. A
    /// Chosen input requires the same recognized descriptor on every source.
    /// Non-Chosen edits retain their former admission and opaque metadata.
    pub fn validate_combined_reserved_inputs(
        &self,
        other: &Self,
    ) -> Result<(), RecordAdmissionRefusal> {
        if !matches!(self.guest_ids, Some(GuestIds::Chosen { .. }))
            && !matches!(other.guest_ids, Some(GuestIds::Chosen { .. }))
        {
            return Ok(());
        }
        self.reserved_input
            .require_same_for_chosen(&other.reserved_input)
    }
}

/// Preserve missing/unknown metadata when inspecting either JSON envelope.
pub fn read_recorded_reserved_input(
    path: &std::path::Path,
) -> Result<ReservedInputMetadata, String> {
    #[derive(Deserialize)]
    struct MetadataOnly {
        #[serde(default)]
        reserved_input: ReservedInputMetadata,
    }
    Ok(super::boundary_recording::read_settings::<MetadataOnly>(path)?.reserved_input)
}

/// Early launch preflight. Each authoritative loader repeats the check on
/// the same immutable decoded bytes it installs; this pathname check is not
/// that final authority. Both Config inputs are checked independently.
/// The run's exclusive-loop rewrite must be the one each record states
/// (`super::rewrite_exclusives_mismatch` against the final config), so
/// every launch path -- the CLI and the public library entries alike --
/// refuses another rewrite before any backend starts the guest.
pub fn validate_config_replay_inputs(config: &crate::Config) -> anyhow::Result<()> {
    for (option, path) in [
        ("--replay-schedule-from", &config.replay_schedule_from),
        ("--replay-preemptions-from", &config.replay_preemptions_from),
    ] {
        if let Some(path) = path {
            let source = format!("{option} {}", path.display());
            let recording = super::boundary_recording::Recording::read(path)?;
            recording.validate_replay_inputs(&source, config.guest_ids)?;
            if let Some(refusal) = super::rewrite_exclusives_mismatch(
                &source,
                recording.recorded_rewrite_exclusives(),
                super::config_rewrite_exclusives(config),
            ) {
                return Err(RecordAdmissionRefusal(refusal).into());
            }
        }
    }
    Ok(())
}

/// Live-writer state. Failure is sticky: a later exec cannot retroactively
/// qualify an earlier executed target image. Launcher bootstrap never calls
/// this observer, and Default/Drop do not mint a producer declaration.
#[derive(Debug, Default)]
pub(super) enum ProducerQualification {
    #[default]
    Unobserved,
    Qualified(ReservedInputLayout),
    Disqualified,
}

impl ProducerQualification {
    pub(super) fn observe(&mut self, layout: Option<ReservedInputLayout>) -> ReservedInputMetadata {
        *self = match (&*self, layout) {
            (Self::Unobserved, Some(layout)) => Self::Qualified(layout),
            (Self::Qualified(previous), Some(layout)) if *previous == layout => {
                Self::Qualified(layout)
            }
            _ => Self::Disqualified,
        };
        match self {
            Self::Qualified(layout) => layout.metadata(),
            _ => ReservedInputMetadata::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preemptions::PreemptionReader;
    use crate::preemptions::PreemptionWriter;
    use crate::preemptions::TimeSkip;
    use crate::preemptions::TimeSkipReason;
    use crate::preemptions::boundary_recording::BoundaryRecording;
    use crate::preemptions::boundary_recording::HistoryReader;
    use crate::preemptions::boundary_recording::Recording;
    use crate::types::DetTid;
    use crate::types::LogicalTime;
    use crate::types::SchedEvent;

    fn chosen() -> GuestIds {
        GuestIds::Chosen {
            base: 5000,
            refuse_host_ids: true,
        }
    }

    fn marked() -> PreemptionRecord {
        let layout = ReservedInputLayout::native().unwrap();
        PreemptionRecord::from_sched_events(vec![SchedEvent::branches(DetTid::from_raw(5000), 2)])
            .with_guest_ids(chosen())
            .with_current_format()
            .with_time_skips(vec![TimeSkip {
                event: 0,
                target: LogicalTime::from_nanos(1000),
                reason: TimeSkipReason::IdleGuestPollers,
            }])
            .with_reserved_input(layout.metadata())
    }

    #[test]
    fn absence_null_and_unknown_provenance_remain_distinct_in_both_envelopes() {
        for value in [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::json!({
                "future_contract": [1, null, "opaque"], "nested": {"extra": false}
            })),
        ] {
            let original = PreemptionRecord::default()
                .with_reserved_input(ReservedInputMetadata(value.clone()));
            for recording in [
                Recording::Legacy(original.clone()),
                Recording::Boundaries(
                    BoundaryRecording::new(
                        original.clone(),
                        detcore_model::schedule::boundary::Schedule::new(vec![]).unwrap(),
                    )
                    .unwrap(),
                ),
            ] {
                let decoded = Recording::decode(&recording.encode().unwrap()).unwrap();
                assert_eq!(decoded.reserved_input().as_json(), value.as_ref());
                decoded
                    .validate_replay_inputs("non-Chosen", GuestIds::Namespace)
                    .unwrap();
                assert_eq!(decoded.encode().unwrap(), recording.encode().unwrap());
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("record.json");
                recording.write(&path).unwrap();
                assert_eq!(
                    read_recorded_reserved_input(&path).unwrap().as_json(),
                    value.as_ref()
                );
                assert_eq!(
                    HistoryReader::read(&path).unwrap().to_string().as_bytes(),
                    recording.encode().unwrap()
                );
            }
        }
    }

    #[test]
    fn chosen_requires_recognized_matching_provenance_without_overrides() {
        let record = marked();
        record
            .validate_replay_inputs("qualified", chosen())
            .unwrap();
        for value in [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::json!(42)),
            Some(serde_json::json!({"contract": "future"})),
        ] {
            let legacy = record
                .clone()
                .with_reserved_input(ReservedInputMetadata(value));
            let refusal = legacy
                .validate_replay_inputs("old fixture", chosen())
                .unwrap_err();
            assert!(
                refusal
                    .0
                    .starts_with("HERMIT_CHOSEN_JSON_INPUT_PROVENANCE:")
            );
            assert!(refusal.0.contains("guest has not started"));
            assert!(refusal.0.contains("Record the run again"));
            // Changing strict/tolerant policy is not a provenance override.
            assert!(
                legacy
                    .validate_replay_inputs(
                        "old fixture",
                        GuestIds::Chosen {
                            base: 5000,
                            refuse_host_ids: false,
                        }
                    )
                    .is_err()
            );
        }
        for (field, value) in [
            ("contract", serde_json::json!("sealed-private-input-v2")),
            ("base", serde_json::json!("0x71020000")),
            ("page_bytes", serde_json::json!(8192)),
            ("abi", serde_json::json!("unsupported")),
            ("extra", serde_json::json!(true)),
        ] {
            let mut raw = serde_json::to_value(&record).unwrap();
            raw["reserved_input"][field] = value;
            let changed: PreemptionRecord = serde_json::from_value(raw).unwrap();
            let refusal = changed
                .validate_replay_inputs("changed", chosen())
                .unwrap_err();
            assert!(
                refusal
                    .0
                    .starts_with("HERMIT_CHOSEN_JSON_INPUT_PROVENANCE:")
            );
        }
        let mut other = ReservedInputLayout::native().unwrap();
        other.page_bytes = if other.page_bytes == 4096 {
            16384
        } else {
            4096
        };
        let changed = record.with_reserved_input(other.metadata());
        assert!(
            changed
                .validate_replay_inputs("different native page", chosen())
                .unwrap_err()
                .0
                .contains("layout differs")
        );
    }

    #[test]
    fn numbering_gate_cannot_be_bypassed_by_an_unlabelled_or_relabelled_record() {
        let record = marked();
        assert!(
            record
                .validate_replay_inputs("wrong runtime", GuestIds::Namespace)
                .is_err()
        );
        assert!(
            record
                .validate_replay_inputs(
                    "wrong base",
                    GuestIds::Chosen {
                        base: 6000,
                        refuse_host_ids: true,
                    }
                )
                .is_err()
        );
        let unlabelled = record.clone().with_guest_ids(GuestIds::Namespace);
        assert!(
            unlabelled
                .validate_replay_inputs("unlabelled", chosen())
                .is_err()
        );
        assert!(
            record
                .with_guest_ids(GuestIds::Host)
                .validate_replay_inputs("host IDs", chosen())
                .is_err()
        );
    }

    #[test]
    fn config_admission_checks_both_actual_inputs_and_preserves_error_classes() {
        let directory = tempfile::tempdir().unwrap();
        let good = directory.path().join("qualified.json");
        let bad = directory.path().join("legacy.json");
        marked().write_to_disk(&good).unwrap();
        marked()
            .with_reserved_input(ReservedInputMetadata::default())
            .write_to_disk(&bad)
            .unwrap();
        for (schedule, priorities) in [
            (Some(bad.clone()), None),
            (None, Some(bad.clone())),
            (Some(good.clone()), Some(bad.clone())),
            (Some(bad.clone()), Some(good.clone())),
        ] {
            let config = crate::Config {
                guest_ids: chosen(),
                replay_schedule_from: schedule,
                replay_preemptions_from: priorities,
                ..crate::Config::default()
            };
            let error = validate_config_replay_inputs(&config).unwrap_err();
            assert!(
                error
                    .chain()
                    .any(|cause| cause.is::<RecordAdmissionRefusal>())
            );
        }
        let config = crate::Config {
            guest_ids: chosen(),
            replay_schedule_from: Some(good.clone()),
            replay_preemptions_from: Some(good.clone()),
            ..crate::Config::default()
        };
        validate_config_replay_inputs(&config).unwrap();
        std::fs::write(&good, b"{broken JSON").unwrap();
        let error = validate_config_replay_inputs(&config).unwrap_err();
        assert!(
            !error
                .chain()
                .any(|cause| cause.is::<RecordAdmissionRefusal>())
        );
    }

    /// The shared launch preflight refuses a replay whose exclusive
    /// rewrite is not the one its record states -- either recorded mode
    /// with rewriting off, a same-mode record of another rewrite format,
    /// a record stating no provenance when the run rewrites, and a
    /// legacy heuristic record inside the boundary envelope -- with the
    /// typed admission refusal, before any backend starts.
    #[test]
    fn config_admission_refuses_another_exclusive_rewrite() {
        use crate::preemptions::RewriteExclusives;
        use crate::preemptions::RewriteExclusivesMode;
        let directory = tempfile::tempdir().unwrap();
        let write = |name: &str, rewrite: Option<RewriteExclusives>| {
            let path = directory.path().join(name);
            PreemptionRecord::from_sched_events(Vec::new())
                .with_rewrite_exclusives(rewrite)
                .write_to_disk(&path)
                .unwrap();
            path
        };
        let plain = write("plain.json", None);
        let memory6 = write(
            "memory6.json",
            Some(RewriteExclusives::current(RewriteExclusivesMode::Memory)),
        );
        let overlay6 = write(
            "overlay6.json",
            Some(RewriteExclusives::current(RewriteExclusivesMode::Overlay)),
        );
        let overlay5 = directory.path().join("overlay5.json");
        std::fs::write(
            &overlay5,
            r#"{"per_thread":{},"global":[],"rewrite_exclusives":{"mode":"overlay","format_version":5}}"#,
        )
        .unwrap();
        let heuristic = directory.path().join("boundary-heuristic.json");
        std::fs::write(
            &heuristic,
            r#"{"slice_boundary_recording":{"preemptions":{"per_thread":{},"global":[],"rewrite_exclusives_stripped_heuristic":true},"schedule":{"protocol":"hermit-slice-boundaries","version":1,"record_count":0,"boundary_count":0,"records":[]}}}"#,
        )
        .unwrap();
        // (in_memory, overlay) of the final config.
        for (path, current, refused) in [
            (&plain, (false, false), false),
            (&plain, (false, true), true),
            (&plain, (true, false), true),
            (&memory6, (false, false), true),
            (&memory6, (false, true), true),
            (&memory6, (true, false), false),
            (&overlay6, (false, false), true),
            (&overlay6, (true, false), true),
            (&overlay6, (false, true), false),
            (&overlay5, (false, true), true),
            (&heuristic, (false, false), true),
            (&heuristic, (false, true), true),
        ] {
            for from_schedule in [true, false] {
                let config = crate::Config {
                    rewrite_exclusives_in_memory: current.0,
                    rewrite_exclusives_overlay: current
                        .1
                        .then(|| std::path::PathBuf::from("/bin/guest"))
                        .into_iter()
                        .collect(),
                    replay_schedule_from: from_schedule.then(|| path.clone()),
                    replay_preemptions_from: (!from_schedule).then(|| path.clone()),
                    ..crate::Config::default()
                };
                let result = validate_config_replay_inputs(&config);
                assert_eq!(result.is_err(), refused, "{path:?} under {current:?}");
                if let Err(error) = result {
                    assert!(
                        error
                            .chain()
                            .any(|cause| cause.is::<RecordAdmissionRefusal>()),
                        "{error:#}"
                    );
                }
            }
        }
    }

    #[test]
    fn authoritative_history_admits_decoded_bytes_not_a_preflight_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        marked().write_to_disk(&path).unwrap();
        let config = crate::Config {
            guest_ids: chosen(),
            replay_preemptions_from: Some(path.clone()),
            ..crate::Config::default()
        };
        validate_config_replay_inputs(&config).unwrap();
        marked()
            .with_reserved_input(ReservedInputMetadata::default())
            .write_to_disk(&path)
            .unwrap();
        let error = HistoryReader::read_for_replay(&path, chosen()).unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<RecordAdmissionRefusal>())
        );
    }

    #[test]
    fn identity_interpolation_validates_all_sources_before_either_endpoint() {
        let original = marked();
        let legacy = original
            .clone()
            .with_reserved_input(ReservedInputMetadata::default());
        for (first, second) in [(&original, &legacy), (&legacy, &original)] {
            let error = first
                .interpolate_events(second, original.global.clone())
                .unwrap_err();
            assert!(error.starts_with("HERMIT_CHOSEN_JSON_INPUT_PROVENANCE:"));
        }
        let mut mixed = original.clone();
        let mut descriptor = ReservedInputLayout::native().unwrap();
        descriptor.abi = if descriptor.abi == "x86_64" {
            "aarch64"
        } else {
            "x86_64"
        }
        .to_owned();
        mixed.reserved_input = descriptor.metadata();
        assert!(
            original
                .interpolate_events(&mixed, original.global.clone())
                .is_err()
        );
        let identity = original
            .interpolate_events(&original, original.global.clone())
            .unwrap();
        assert_eq!(identity.to_string(), original.to_string());
        assert_eq!(identity.format_version(), Some(3));
        assert_eq!(identity.time_skips(), original.time_skips());
    }

    #[test]
    fn non_chosen_interpolation_preserves_opaque_metadata_without_new_admission() {
        let first = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(DetTid::from_raw(3), 2),
            SchedEvent::branches(DetTid::from_raw(5), 3),
        ])
        .with_current_format()
        .with_time_skips(vec![])
        .with_reserved_input(ReservedInputMetadata(Some(
            serde_json::json!({"future": [null, 17]}),
        )));
        let second = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(DetTid::from_raw(7), 2),
            SchedEvent::branches(DetTid::from_raw(9), 3),
        ])
        .with_current_format()
        .with_time_skips(vec![])
        .with_reserved_input(ReservedInputMetadata(Some(serde_json::Value::Null)));
        // The inherited format stores no jumps as None, even in format 3.
        // Check the inputs so this test cannot mistake normalization for a
        // metadata loss introduced by interpolation.
        for source in [&first, &second] {
            assert_eq!(source.time_skips(), None);
            assert_eq!(source.format_version(), Some(3));
            source.validate().unwrap();
            assert!(
                serde_json::to_value(source)
                    .unwrap()
                    .get("time_skips")
                    .is_none()
            );
        }
        let mixed = first
            .interpolate_events(
                &second,
                vec![first.global[0].clone(), second.global[1].clone()],
            )
            .unwrap();
        assert_eq!(mixed.reserved_input(), first.reserved_input());
        assert_eq!(mixed.format_version(), Some(3));
        assert_eq!(mixed.time_skips(), None);
        assert!(
            serde_json::to_value(&mixed)
                .unwrap()
                .get("time_skips")
                .is_none()
        );
        assert_eq!(
            first
                .interpolate_events(&second, second.global.clone())
                .unwrap(),
            second
        );
        // An explicitly stored empty list is invalid under the inherited
        // format contract, including when the requested events are an endpoint.
        let mut invalid = first.clone();
        invalid.time_skips = Some(vec![]);
        for (left, right) in [(&invalid, &second), (&second, &invalid)] {
            for events in [left.global.clone(), right.global.clone()] {
                assert_eq!(
                    left.interpolate_events(right, events).unwrap_err(),
                    "an empty list of time skips is written as no list"
                );
            }
        }
    }

    #[test]
    fn direct_readers_prefixes_and_priority_edits_preserve_provenance() {
        let original = marked();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.json");
        original.write_to_disk(&path).unwrap();
        for record in [
            PreemptionReader::new(&path).load_all(),
            PreemptionReader::try_new(&path).unwrap().into_inner(),
            original.schedule_prefix(original.global.len()),
            original.clone_preemptions_only(),
        ] {
            assert_eq!(record.reserved_input(), original.reserved_input());
        }
        let prefix = original.schedule_prefix(original.global.len());
        assert_eq!(prefix.format_version(), Some(3));
        assert_eq!(prefix.time_skips(), original.time_skips());
        assert_eq!(original.clone_preemptions_only().format_version(), None);
    }

    #[test]
    fn writer_never_qualifies_from_configuration_default_or_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("partial.json");
        {
            let _writer = PreemptionWriter::new(Some(path.clone())).with_guest_ids(chosen());
        }
        let dropped = PreemptionReader::new(&path).load_all();
        assert!(dropped.reserved_input().is_missing());
        for record in [
            PreemptionRecord::default().with_guest_ids(chosen()),
            PreemptionRecord::from_sched_events(vec![]).with_guest_ids(chosen()),
            PreemptionRecord::from_vecs(&Default::default()).with_guest_ids(chosen()),
        ] {
            assert!(record.reserved_input().is_missing());
        }
    }

    /// Serializer coverage only: this imported declaration is not evidence
    /// that a backend established a target image. The physical setup observer
    /// has separate execution controls.
    #[test]
    fn writer_string_flush_and_drop_preserve_the_complete_imported_metadata() {
        let directory = tempfile::tempdir().unwrap();
        for boundaries in [false, true] {
            for metadata in [
                ReservedInputMetadata::default(),
                ReservedInputMetadata(Some(serde_json::Value::Null)),
                ReservedInputMetadata(Some(serde_json::json!({"future": [17, null]}))),
                ReservedInputLayout::native().unwrap().metadata(),
            ] {
                let mut record = marked().with_reserved_input(metadata);
                if boundaries {
                    record.global.clear();
                }
                let make = |path| {
                    let mut writer = PreemptionWriter::new(path);
                    writer.inner = record.clone();
                    if boundaries {
                        writer = writer.with_slice_boundaries().unwrap();
                    }
                    writer
                };
                let string = make(None).into_string();
                let expected = Recording::decode(string.as_bytes()).unwrap();
                assert_eq!(expected.reserved_input(), record.reserved_input());
                assert_eq!(expected.format_version(), Some(3));
                assert_eq!(expected.time_skips(), record.time_skips());
                for explicit_flush in [false, true] {
                    let path = directory.path().join("writer.json");
                    let writer = make(Some(path.clone()));
                    if explicit_flush {
                        writer.flush().unwrap();
                    } else {
                        drop(writer);
                    }
                    let written = Recording::read(&path).unwrap();
                    assert_eq!(written.encode().unwrap(), expected.encode().unwrap());
                }
            }
        }
    }

    #[test]
    fn an_unqualified_target_cannot_be_retroactively_qualified_by_exec_or_flush() {
        let layout = ReservedInputLayout::native().unwrap();
        for first in [Some(layout.clone()), None] {
            let mut qualifier = ProducerQualification::default();
            qualifier.observe(first);
            assert!(qualifier.observe(None).is_missing());
            assert!(qualifier.observe(Some(layout.clone())).is_missing());
        }
        let mut qualifier = ProducerQualification::default();
        assert_eq!(qualifier.observe(Some(layout.clone())), layout.metadata());
        assert_eq!(qualifier.observe(Some(layout.clone())), layout.metadata());
        let mut mismatch = layout;
        mismatch.page_bytes *= 2;
        assert!(qualifier.observe(Some(mismatch)).is_missing());
    }

    /// Exercise the actual writer observer and serialization using synthetic
    /// setup facts. This does not establish that a backend mapped or sealed a
    /// real target; the CLI producer/replay control covers that separate seam.
    #[test]
    fn writer_setup_observer_carries_verified_layout_and_keeps_failure_sticky() {
        use reverie::reserved_input::InputAbi;
        use reverie::reserved_input::InputSetupOutcome;
        use reverie::reserved_input::InputSetupReceipt;
        use reverie::reserved_input::InputUnavailable;
        use reverie::reserved_input::ReservedInputControl;

        let native = ReservedInputLayout::native().unwrap();
        let layout = reverie::reserved_input::ReservedInputLayout {
            base: 0x7101_0000,
            page_bytes: native.page_bytes as usize,
            abi: if native.abi == "x86_64" {
                InputAbi::X86_64
            } else {
                InputAbi::Aarch64
            },
        };
        let control = ReservedInputControl::new_backend().unwrap();
        let task = control.backend_register_root(17, 17).unwrap().task;
        let good = InputSetupReceipt {
            task,
            outcome: InputSetupOutcome::Qualified(layout),
        };
        let skip = TimeSkip {
            event: 0,
            target: LogicalTime::from_nanos(1000),
            reason: TimeSkipReason::IdleGuestPollers,
        };
        for failure in [
            InputSetupOutcome::Unqualified(InputUnavailable::new("test-unqualified-image")),
            InputSetupOutcome::Qualified(reverie::reserved_input::ReservedInputLayout {
                base: layout.base + 4096,
                ..layout
            }),
        ] {
            let mut writer = PreemptionWriter::new(None)
                .with_guest_ids(chosen())
                .with_format(Some(3));
            writer.insert_schedevent(SchedEvent::branches(DetTid::from_raw(5000), 2));
            writer.insert_time_skip(skip);
            assert!(writer.inner.reserved_input().is_missing());
            writer.observe_reserved_input_setup(&good).unwrap();
            let recorded =
                Recording::decode(writer.serialized_string().unwrap().as_bytes()).unwrap();
            assert_eq!(recorded.reserved_input(), &native.metadata());
            assert_eq!(recorded.format_version(), Some(3));
            assert_eq!(recorded.time_skips(), Some([skip].as_slice()));
            recorded
                .validate_replay_inputs("writer observer", chosen())
                .unwrap();

            let unsupported_layout = matches!(failure, InputSetupOutcome::Qualified(_));
            let outcome = writer.observe_reserved_input_setup(&InputSetupReceipt {
                task,
                outcome: failure,
            });
            assert_eq!(outcome.is_err(), unsupported_layout);
            assert!(writer.inner.reserved_input().is_missing());
            writer.observe_reserved_input_setup(&good).unwrap();
            let partial = Recording::decode(writer.into_string().as_bytes()).unwrap();
            assert!(partial.reserved_input().is_missing());
            assert_eq!(partial.format_version(), Some(3));
            assert_eq!(partial.time_skips(), Some([skip].as_slice()));
            assert!(
                partial
                    .validate_replay_inputs("unqualified earlier image", chosen())
                    .unwrap_err()
                    .to_string()
                    .contains("missing reserved_input provenance")
            );
        }
    }
}
