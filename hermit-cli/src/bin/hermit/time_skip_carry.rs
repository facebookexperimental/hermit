/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Carry recorded jumps of global time into the trial schedules that analyze
//! and bisect splice from recorded ones.
//!
//! A record lists the jumps its run made (`PreemptionRecord::time_skips`),
//! each at a position in its event stream, and a replay of the record makes
//! exactly those jumps. A trial schedule that analyze or bisect builds from a
//! recorded schedule's events carries none of them by itself, so its replay
//! would make no jump where the recording did and diverge there. This module
//! remembers every recorded schedule, and gives a trial
//! schedule the jumps of the one recording whose events it repeats furthest,
//! up to where they part: one recording's history, never a mix of several,
//! which could disagree.

use std::collections::BTreeSet;
use std::hash::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::Mutex;

use detcore::preemptions::PreemptionRecord;
use detcore::preemptions::ReservedInputMetadata;
use detcore::preemptions::TimeSkip;
use detcore::types::SchedEvent;
use detcore_model::pid::GuestIds;

/// Every recorded schedule seen so far: the hash of each prefix of its events,
/// and its jumps in stream order, none for a schedule that made none.
#[derive(Debug, Default)]
pub struct TimeSkipSources {
    sources: Vec<Source>,
    /// Every timing format read (`None` means legacy format 1). A trial
    /// cannot combine formats with different jump/switch-in charging rules.
    formats: BTreeSet<Option<u32>>,
}

#[derive(Debug, PartialEq)]
struct Source {
    /// Entry `i` hashes the recording's first `i` events (`prefix_hashes`).
    prefix_hashes: Vec<u64>,
    skips: Vec<TimeSkip>,
    guest_ids: GuestIds,
    reserved_input: ReservedInputMetadata,
}

/// One [`TimeSkipSources`] that every copy of the analyze options shares.
pub type SharedTimeSkipSources = Arc<Mutex<TimeSkipSources>>;

/// The hash of each prefix of `events`, from the empty one on: entry `i`
/// covers `events[..i]`. An event's timestamp is left out, because analyze and
/// bisect drop timestamps from the schedules they search.
fn prefix_hashes(events: &[SchedEvent]) -> Vec<u64> {
    let mut hash = 0u64;
    std::iter::once(hash)
        .chain(events.iter().map(move |event| {
            let mut hasher = DefaultHasher::new();
            hash.hash(&mut hasher);
            (
                event.dettid,
                event.op,
                event.count,
                event.start_rip,
                event.end_rip,
            )
                .hash(&mut hasher);
            hash = hasher.finish();
            hash
        }))
        .collect()
}

/// How many leading events two schedules share, from their prefix hashes:
/// the hash of a prefix covers every event before it, so prefixes match up to
/// some length and differ from there on.
fn shared_events(left: &[u64], right: &[u64]) -> usize {
    let shared = left.len().min(right.len());
    // The empty prefixes always match; find the last matching entry.
    let (mut matching, mut differing) = (0, shared);
    while matching + 1 < differing {
        let middle = (matching + differing) / 2;
        if left[middle] == right[middle] {
            matching = middle;
        } else {
            differing = middle;
        }
    }
    if shared > 0 && left[shared - 1] == right[shared - 1] {
        shared - 1
    } else {
        matching
    }
}

impl TimeSkipSources {
    /// Remember `record`. One that made no jump counts as much as one that
    /// did: a trial that repeats it furthest makes no jump either. Its format
    /// counts below the background-starts format
    /// (`detcore::preemptions::format_without_background_starts`): that format
    /// and the one below it share their timing rules and differ only in a
    /// list of starts no spliced trial keeps, so a recording that notes its
    /// starts and the re-record of its schedule replay share one format here.
    pub fn remember(&mut self, record: &PreemptionRecord) {
        self.formats
            .insert(detcore::preemptions::format_without_background_starts(
                record.format_version(),
            ));
        let source = Source {
            prefix_hashes: prefix_hashes(record.schedevents()),
            skips: record.time_skips().unwrap_or_default().to_vec(),
            guest_ids: record.guest_ids().unwrap_or_default(),
            reserved_input: record.reserved_input().clone(),
        };
        if !self.sources.contains(&source) {
            self.sources.push(source);
        }
    }

    /// The single timing format all sources share, or the current format
    /// below the background-starts format before any source is read
    /// (`remember`). There is no proved conversion between
    /// legacy, format 2 and format 3 charging rules, even when a selected
    /// prefix comes from just one of the sources.
    pub fn trial_format(&self) -> anyhow::Result<Option<u32>> {
        anyhow::ensure!(
            self.formats.len() <= 1,
            "cannot combine recordings with different timing formats {:?}; \
             record both inputs with the same Hermit version and timing format",
            self.formats
                .iter()
                .map(|format| format.unwrap_or(1))
                .collect::<Vec<_>>()
        );
        Ok(self.formats.first().copied().unwrap_or(
            detcore::preemptions::format_without_background_starts(Some(
                detcore::preemptions::PREEMPTION_RECORD_FORMAT,
            )),
        ))
    }

    /// Admit all Chosen source metadata before a trial or endpoint shortcut.
    /// Non-Chosen metadata is opaque and adds no new source-admission rule.
    pub fn validate_reserved_inputs(&self) -> anyhow::Result<()> {
        if !self
            .sources
            .iter()
            .any(|source| matches!(source.guest_ids, GuestIds::Chosen { .. }))
        {
            return Ok(());
        }
        if let Some(first) = self.sources.first() {
            for source in &self.sources {
                if let Some(refusal) = detcore::preemptions::guest_ids_mismatch(
                    "transformation source",
                    first.guest_ids,
                    source.guest_ids,
                ) {
                    anyhow::bail!("{refusal}");
                }
                first
                    .reserved_input
                    .require_same_for_chosen(&source.reserved_input)?;
            }
        }
        Ok(())
    }

    /// Carry the same source's numbering/provenance as its selected prefix.
    /// This copies a producer declaration; it never creates one from defaults.
    pub fn trial_identity_metadata(
        &self,
        events: &[SchedEvent],
    ) -> anyhow::Result<Option<(GuestIds, ReservedInputMetadata)>> {
        self.validate_reserved_inputs()?;
        Ok(self
            .selected(events)
            .map(|(_, source)| (source.guest_ids, source.reserved_input.clone())))
    }

    fn selected(&self, events: &[SchedEvent]) -> Option<(usize, &Source)> {
        let hashes = prefix_hashes(events);
        let mut best: Option<(usize, &Source)> = None;
        for source in &self.sources {
            let shared = shared_events(&source.prefix_hashes, &hashes);
            if best.is_none_or(|(furthest, _)| shared > furthest) {
                best = Some((shared, source));
            }
        }
        best
    }

    /// The jumps a replay of `events` must make: those of the remembered
    /// recording whose events `events` repeats furthest, up to the end of the
    /// shared events, in stream order. The run that recorded a jump was in the
    /// state those events lead to when it jumped, so a replay of the same
    /// events is too. One recording's jumps, validated when it was read, are
    /// never mixed with another's, which could disagree. Ties go to the
    /// recording remembered first.
    pub fn carried(&self, events: &[SchedEvent]) -> Vec<TimeSkip> {
        let Some((shared, source)) = self.selected(events) else {
            return Vec::new();
        };
        source
            .skips
            .iter()
            .take_while(|skip| usize::try_from(skip.event).is_ok_and(|event| event <= shared))
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use detcore::preemptions::TimeSkipReason;
    use detcore::types::DetTid;
    use detcore::types::LogicalTime;
    use detcore::types::SyscallPhase;
    use reverie::syscalls::Sysno;

    use super::*;

    fn events(tids: &[i32]) -> Vec<SchedEvent> {
        tids.iter()
            .map(|&tid| {
                SchedEvent::syscall(DetTid::from_raw(tid), Sysno::poll, SyscallPhase::Posthook)
            })
            .collect()
    }

    fn skip(event: u64, target: u64) -> TimeSkip {
        TimeSkip {
            event,
            target: LogicalTime::from_nanos(target),
            reason: TimeSkipReason::IdleGuestPollers,
        }
    }

    /// A trial spliced from a recorded schedule's prefix carries the jumps that
    /// prefix contains, at their positions, and none past the splice point.
    #[test]
    fn a_spliced_prefix_carries_the_jumps_inside_it() {
        let recorded = events(&[3, 3, 5, 3, 5, 5]);
        let record = PreemptionRecord::from_sched_events(recorded.clone())
            .with_time_skips(vec![skip(2, 1_000), skip(5, 2_000)]);
        let mut sources = TimeSkipSources::default();
        sources.remember(&record);

        // The whole schedule carries both.
        assert_eq!(
            sources.carried(&recorded),
            vec![skip(2, 1_000), skip(5, 2_000)]
        );
        // Spliced after event 3: the first jump is inside the prefix, the second
        // is not, because the trial's events diverge before it.
        let mut spliced = recorded[..3].to_vec();
        spliced.extend(events(&[5, 3, 3]));
        assert_eq!(sources.carried(&spliced), vec![skip(2, 1_000)]);
        // A jump exactly at the splice point is inside the prefix.
        assert_eq!(sources.carried(&recorded[..2]), vec![skip(2, 1_000)]);
        // A trial that differs from the first event on carries nothing.
        assert!(sources.carried(&events(&[5, 3, 5])).is_empty());
    }

    /// Timestamps, which analyze and bisect strip, do not stop a prefix from
    /// matching.
    #[test]
    fn a_prefix_matches_without_its_timestamps() {
        let mut recorded = events(&[3, 5, 3]);
        for (index, event) in recorded.iter_mut().enumerate() {
            event.end_time = Some(LogicalTime::from_nanos(index as u64 + 1));
        }
        let record = PreemptionRecord::from_sched_events(recorded.clone())
            .with_time_skips(vec![skip(1, 1_000)]);
        let mut sources = TimeSkipSources::default();
        sources.remember(&record);
        let mut stripped = recorded;
        for event in &mut stripped {
            event.end_time = None;
        }
        assert_eq!(sources.carried(&stripped), vec![skip(1, 1_000)]);
    }

    /// A record without jumps, and a jump past the end of its events, add
    /// nothing.
    #[test]
    fn unplaceable_jumps_are_left_out() {
        let recorded = events(&[3, 5]);
        let mut sources = TimeSkipSources::default();
        sources.remember(
            &PreemptionRecord::from_sched_events(recorded.clone())
                .with_time_skips(vec![skip(9, 1_000)]),
        );
        assert!(sources.carried(&recorded).is_empty());
    }

    /// Two recordings that share their first events but jumped differently
    /// inside them are never mixed: a trial carries the jumps of the one whose
    /// events it repeats furthest, and the result validates (Codex review,
    /// finding 5).
    #[test]
    fn recordings_that_disagree_are_not_mixed() {
        let first = events(&[3, 5, 7]);
        let second = events(&[3, 5, 9]);
        let mut sources = TimeSkipSources::default();
        sources.remember(
            &PreemptionRecord::from_sched_events(first.clone())
                .with_time_skips(vec![skip(1, 2_000)]),
        );
        sources.remember(
            &PreemptionRecord::from_sched_events(second.clone())
                .with_time_skips(vec![skip(2, 1_000)]),
        );
        let carried = sources.carried(&first);
        assert_eq!(carried, vec![skip(1, 2_000)]);
        assert!(
            PreemptionRecord::from_sched_events(first)
                .with_time_skips(carried)
                .validate()
                .is_ok()
        );
        assert_eq!(sources.carried(&second), vec![skip(2, 1_000)]);
    }

    /// A recording that made no jump is a source like any other: a trial that
    /// repeats it further than a recording that jumped makes no jump (Codex
    /// review, round 2).
    #[test]
    fn a_closer_recording_without_jumps_wins() {
        let jumped = events(&[3, 5, 7]);
        let plain = events(&[3, 5, 9]);
        let mut sources = TimeSkipSources::default();
        sources.remember(
            &PreemptionRecord::from_sched_events(jumped.clone())
                .with_time_skips(vec![skip(1, 2_000)]),
        );
        sources.remember(&PreemptionRecord::from_sched_events(plain.clone()).with_format(Some(2)));
        assert!(sources.carried(&plain).is_empty());
        assert_eq!(sources.carried(&jumped), vec![skip(1, 2_000)]);
    }

    /// Timing formats cannot be converted merely by choosing the oldest.
    /// The actual writer/admission controls also check this refusal.
    #[test]
    fn a_trial_refuses_mixed_timing_formats() {
        let mut sources = TimeSkipSources::default();
        let current = detcore::preemptions::format_without_background_starts(Some(
            detcore::preemptions::PREEMPTION_RECORD_FORMAT,
        ));
        assert_eq!(sources.trial_format().unwrap(), current);
        sources.remember(&PreemptionRecord::from_sched_events(events(&[3])).with_current_format());
        assert_eq!(sources.trial_format().unwrap(), current);
        sources.remember(&PreemptionRecord::from_sched_events(events(&[5])));
        let error = sources.trial_format().unwrap_err();
        assert!(error.to_string().contains("different timing formats"));
    }

    fn with_metadata(record: PreemptionRecord, metadata: serde_json::Value) -> PreemptionRecord {
        let mut value = serde_json::to_value(record).unwrap();
        value["reserved_input"] = metadata;
        serde_json::from_value(value).unwrap()
    }

    /// The chosen prefix selects one source's opaque metadata and jumps
    /// together. A non-Chosen future value is not a new admission failure.
    #[test]
    fn metadata_follows_the_same_source_as_its_time_skips() {
        let first = with_metadata(
            PreemptionRecord::from_sched_events(events(&[3, 5, 7]))
                .with_current_format()
                .with_time_skips(vec![skip(1, 1000)]),
            serde_json::json!({"future": [null, 17]}),
        );
        let second = with_metadata(
            PreemptionRecord::from_sched_events(events(&[3, 5, 9]))
                .with_current_format()
                .with_time_skips(vec![skip(2, 2000)]),
            serde_json::Value::Null,
        );
        let mut sources = TimeSkipSources::default();
        sources.remember(&first);
        sources.remember(&second);
        for source in [&first, &second] {
            let (ids, metadata) = sources
                .trial_identity_metadata(source.schedevents())
                .unwrap()
                .unwrap();
            assert_eq!(ids, GuestIds::Namespace);
            assert_eq!(&metadata, source.reserved_input());
            assert_eq!(
                sources.carried(source.schedevents()),
                source.time_skips().unwrap()
            );
            assert_eq!(sources.trial_format().unwrap(), Some(3));
        }
    }

    /// Even when the output exactly matches a qualified endpoint, every
    /// Chosen source must have the same recognized producer declaration.
    #[test]
    fn chosen_sources_are_admitted_before_an_identity_trial() {
        let metadata = serde_json::json!({
            "contract": "sealed-private-input-v1", "base": "0x71010000",
            "abi": "x86_64", "page_bytes": 4096
        });
        let ids = GuestIds::Chosen {
            base: 5000,
            refuse_host_ids: true,
        };
        let record = PreemptionRecord::from_sched_events(events(&[3, 5, 7]))
            .with_current_format()
            .with_time_skips(vec![skip(1, 1000)])
            .with_guest_ids(ids);
        let qualified = with_metadata(record.clone(), metadata.clone());
        for other in [
            record.clone(),
            with_metadata(record.clone(), serde_json::Value::Null),
            with_metadata(
                record.clone(),
                serde_json::json!({"contract": "future-input"}),
            ),
            with_metadata(
                record,
                serde_json::json!({
                    "contract": "sealed-private-input-v1", "base": "0x71010000",
                    "abi": "aarch64", "page_bytes": 4096
                }),
            ),
        ] {
            for sources_in_order in [[&qualified, &other], [&other, &qualified]] {
                let mut sources = TimeSkipSources::default();
                for source in sources_in_order {
                    sources.remember(source);
                }
                let error = sources
                    .trial_identity_metadata(qualified.schedevents())
                    .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("HERMIT_CHOSEN_JSON_INPUT_PROVENANCE:"),
                    "{error}"
                );
            }
        }
        let mut sources = TimeSkipSources::default();
        sources.remember(&qualified);
        assert_eq!(
            sources
                .trial_identity_metadata(qualified.schedevents())
                .unwrap(),
            Some((ids, qualified.reserved_input().clone()))
        );
        assert_eq!(
            sources.carried(qualified.schedevents()),
            qualified.time_skips().unwrap()
        );
        assert_eq!(sources.trial_format().unwrap(), Some(3));
    }
}
