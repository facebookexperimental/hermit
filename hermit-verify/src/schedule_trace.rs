/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::Parser;
use colored::Colorize;
use detcore::edit_distance::iterable_bubble_sort;
use detcore::preemptions::PreemptionRecord;
use detcore::preemptions::boundary_recording::Recording;
use detcore_model::pid::DetTid;
use detcore_model::schedule::SchedEvent;
use detcore_model::schedule::boundary::Record;
use detcore_model::schedule::boundary::Schedule;

use crate::CommonOpts;

#[derive(Debug, Parser)]
pub enum SchedTraceOpts {
    /// Inspect and report statistics on one schedule.
    Inspect(InspectOpts),

    /// Just dump the raw schedule
    Print(PrintOpts),

    /// Difference between two preempt files
    Diff(DiffOpts),

    /// Run the interpolation between two preempt files
    Interpolate(InterpolateOpts),
}

impl SchedTraceOpts {
    pub fn main(&self, common_args: &CommonOpts) -> anyhow::Result<bool> {
        match self {
            SchedTraceOpts::Diff(x) => x.run(common_args),
            SchedTraceOpts::Interpolate(x) => x.run(common_args),
            SchedTraceOpts::Inspect(x) => x.run(common_args),
            SchedTraceOpts::Print(x) => x.run(common_args),
        }
    }
}

#[derive(Debug, Parser)]
pub struct DiffOpts {
    /// First schedule file to compare.
    file_a: PathBuf,
    /// Second schedule file to compare.
    file_b: PathBuf,
}

impl DiffOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let first_schedule = legacy_schedule(&self.file_a, "schedule diff/interpolation")?;
        let second_schedule = legacy_schedule(&self.file_b, "schedule diff/interpolation")?;
        println!("first schedule length: {}", first_schedule.len());
        println!("second schedule length: {}", second_schedule.len());
        let bubbles = iterable_bubble_sort(&first_schedule, &second_schedule);
        eprintln!(
            ":: {}",
            format!(
                "Diffs found in schedules : Swap distance  = {}, Edit distance = {} ",
                bubbles.swap_distance(),
                bubbles.edit_distance()
            )
            .yellow()
            .bold()
        );
        Ok(true)
    }
}

#[derive(Debug, Parser)]
pub struct InspectOpts {
    /// Schedule to inspect.
    pub sched_file: PathBuf,

    /// Print O(N) output proportional to the length of the schedule.
    #[clap(long, short = 'v')]
    pub verbose: bool,
}

/// Return all events which are right before a preemption.
fn preempt_events(sched: &[SchedEvent]) -> Vec<SchedEvent> {
    let mut acc = Vec::new();
    for ix in 0..sched.len().saturating_sub(1) {
        if sched[ix].dettid != sched[ix + 1].dettid {
            acc.push(sched[ix].clone());
        }
    }
    acc
}

/// Split apart an event series into a per-thread record.
fn per_thread_events(events: &Vec<SchedEvent>) -> BTreeMap<DetTid, Vec<SchedEvent>> {
    let mut acc = BTreeMap::new();
    for evt in events {
        let vec: &mut Vec<SchedEvent> = acc.entry(evt.dettid).or_default();
        vec.push(evt.clone());
    }
    acc
}

impl InspectOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let schedule = match Recording::read(&self.sched_file)? {
            Recording::Legacy(record) => record.into_global(),
            Recording::Boundaries(record) => {
                print!("{}", inspect_boundaries(record.schedule(), self.verbose)?);
                return Ok(true);
            }
        };
        println!("Schedule length: {}", schedule.len());

        let preempts = preempt_events(&schedule);
        println!("Context-switch/preemption events: {}", preempts.len());
        println!("Per thread pre-preemption events:");
        let per_thread_preempts = per_thread_events(&preempts);
        for (tid, vec) in &per_thread_preempts {
            println!("  tid {}: {} preemptions", tid, vec.len());
            if self.verbose {
                for evt in vec {
                    println!("    {}", evt);
                }
            }
        }

        if self.verbose {
            println!("Full context switch event list in order, with before/after events:");
            let mut count = 0;
            for ix in 0..schedule.len().saturating_sub(1) {
                if schedule[ix].dettid != schedule[ix + 1].dettid {
                    count += 1;
                    println!(" {: >2}: {} => {}", count, schedule[ix], schedule[ix + 1]);
                }
            }
        }

        Ok(true)
    }
}

#[derive(Debug, Parser)]
pub struct PrintOpts {
    /// Schedule to inspect.
    pub sched_file: PathBuf,

    /// Print the indices as well as the raw events.
    #[clap(long, short = 'i')]
    indices: bool,

    /// Print only events from the given tid, leaving a blank line anywhere the tid was preempted.
    #[clap(long)]
    tid: Option<i32>,

    /// Strip times before printing.
    #[clap(long)]
    strip_times: bool,
}

impl PrintOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let mut recording = Recording::read(&self.sched_file)?;
        if self.strip_times {
            recording.strip_times();
        }
        let schedule = match recording {
            Recording::Legacy(record) => record.into_global(),
            Recording::Boundaries(record) => {
                print!(
                    "{}",
                    print_boundaries(record.schedule(), self.indices, self.tid)?
                );
                return Ok(true);
            }
        };
        let mut last_skipped = false;
        let mut thread_counts: BTreeMap<DetTid, u64> = BTreeMap::new();
        for (global_ix, ev) in schedule.iter().enumerate() {
            let entry = thread_counts.entry(ev.dettid).or_default();
            let current_thread_ix: u64 = *entry;
            *entry += 1;

            if let Some(tid) = self.tid {
                if ev.dettid.as_raw() == tid {
                    last_skipped = false;
                } else {
                    if !last_skipped {
                        println!();
                    }
                    last_skipped = true;
                    continue;
                }
            }

            let mut ev = ev.clone();
            if self.strip_times {
                ev.end_time = None;
            }

            if self.indices {
                println!(
                    "{: <9} {}",
                    format!("({},{})", global_ix, current_thread_ix),
                    ev
                );
            } else {
                println!("{}", ev);
            }
        }
        Ok(true)
    }
}

#[derive(Debug, Parser)]
pub struct InterpolateOpts {
    /// Interpolate file a and b by how much percentage. Currently, it takes an integer between 0 and 100.
    interpolate: u8,

    /// First log to compare.
    file_a: PathBuf,

    /// Second log to compare.
    file_b: PathBuf,
}

impl InterpolateOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let next_sched = self.interpolated_record()?;
        let dir = tempfile::Builder::new()
            .prefix("hermit_internal")
            .tempdir()?;
        let tmpdir_path = dir.keep();
        eprintln!(":: Temp workspace: {}", tmpdir_path.display());
        let sched_path = tmpdir_path.join(format!("interpolate_{}.events", self.interpolate));
        next_sched.write_to_disk(&sched_path).unwrap();

        Ok(true)
    }

    fn interpolated_record(&self) -> anyhow::Result<PreemptionRecord> {
        let guest_ids = Self::common_guest_ids(&self.file_a, &self.file_b)?;
        let latency = Self::common_pipe_wake_latency(&self.file_a, &self.file_b)?;
        let copy = Self::common_signal_state_copy(&self.file_a, &self.file_b)?;
        let charged_once = Self::common_expired_wait_charge(&self.file_a, &self.file_b)?;
        let first_pass = Self::common_sleep_registration(&self.file_a, &self.file_b)?;
        let first_record =
            Recording::read(&self.file_a)?.into_legacy("schedule diff/interpolation")?;
        let second_record =
            Recording::read(&self.file_b)?.into_legacy("schedule diff/interpolation")?;
        let first_schedule = first_record.schedevents();
        let second_schedule = second_record.schedevents();
        let (_swap_dist, _edit_dist, requested_schedule) = {
            let mut bubbles = iterable_bubble_sort(first_schedule, second_schedule);
            (
                bubbles.swap_distance(),
                bubbles.edit_distance(),
                bubbles
                    .interpolate(self.interpolate)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        // Preserve main's first-input guest-ID refusal policy and its
        // existing option reconciliation before comparing the other metadata,
        // retaining each input's replay departure (4860) through the merge.
        let first_departed = first_record.replay_departed_at();
        let second_departed = second_record.replay_departed_at();
        let reconciled = |record: PreemptionRecord| {
            record
                .retain_replay_departure(first_departed)
                .retain_replay_departure(second_departed)
                .with_guest_ids(guest_ids)
                .with_pipe_wake_latency(latency)
                .with_signal_state_copy(copy)
                .with_expired_wait_runs_charged_once(charged_once)
                .with_queued_sleeps_registered_at_first_pass(first_pass)
        };
        reconciled(first_record)
            .interpolate_events(&reconciled(second_record), requested_schedule)
            .map_err(anyhow::Error::msg)
    }

    /// Numbering must agree just as it must for replay. The refusal policy
    /// does not change the IDs; retain the first input's policy when the
    /// chosen bases agree. Host-numbered inputs remain marked as such.
    fn common_guest_ids(
        file_a: &std::path::Path,
        file_b: &std::path::Path,
    ) -> anyhow::Result<detcore_model::pid::GuestIds> {
        let read = |path: &std::path::Path| {
            detcore::preemptions::read_recorded_guest_ids(path).map_err(anyhow::Error::msg)
        };
        let (a, b) = (read(file_a)?, read(file_b)?);
        if !a.numbers_like(&b) {
            anyhow::bail!(
                "{} and {} were recorded with incompatible guest process IDs ({a} and {b}); \
                 no interpolation of them can be replayed. Use schedules recorded with the \
                 same guest ID mode and --pid-base.",
                file_a.display(),
                file_b.display(),
            );
        }
        Ok(a)
    }

    /// The `--pipe-wake-latency` both schedules were recorded under, which
    /// the interpolated schedule carries; schedules recorded under different
    /// values are refused (<https://github.com/rrnewton/hermit/issues/4380>).
    fn common_pipe_wake_latency(
        file_a: &std::path::Path,
        file_b: &std::path::Path,
    ) -> anyhow::Result<Option<u64>> {
        let read = |path: &std::path::Path| {
            detcore::preemptions::read_recorded_pipe_wake_latency(path).map_err(anyhow::Error::msg)
        };
        let (a, b) = (read(file_a)?, read(file_b)?);
        if a != b {
            anyhow::bail!(
                "{} and {} were recorded under different --pipe-wake-latency values ({:?} and \
                 {:?}); no interpolation of them can be replayed",
                file_a.display(),
                file_b.display(),
                a,
                b
            );
        }
        Ok(a)
    }

    /// How both schedules' runs charged a run of expired timed waits
    /// (`--expired-wait-charge`), which the interpolated schedule carries;
    /// schedules recorded under different rules are refused, since no single
    /// replay could charge as both did.
    fn common_expired_wait_charge(
        file_a: &std::path::Path,
        file_b: &std::path::Path,
    ) -> anyhow::Result<bool> {
        let read = |path: &std::path::Path| {
            detcore::preemptions::read_recorded_expired_wait_runs_charged_once(path)
                .map_err(anyhow::Error::msg)
        };
        let (a, b) = (read(file_a)?, read(file_b)?);
        if a != b {
            let show =
                |once| detcore_model::config::ExpiredWaitCharge::from_charged_once(once).as_str();
            anyhow::bail!(
                "{} and {} were recorded under different --expired-wait-charge rules ({} and \
                 {}); no interpolation of them can be replayed",
                file_a.display(),
                file_b.display(),
                show(a),
                show(b)
            );
        }
        Ok(a)
    }

    /// How both schedules' runs registered queued sleeps
    /// (`--sleep-registration`), which the interpolated schedule carries;
    /// schedules recorded under different rules are refused, since no single
    /// replay could register as both did.
    fn common_sleep_registration(
        file_a: &std::path::Path,
        file_b: &std::path::Path,
    ) -> anyhow::Result<bool> {
        let read = |path: &std::path::Path| {
            detcore::preemptions::read_recorded_queued_sleeps_registered_at_first_pass(path)
                .map_err(anyhow::Error::msg)
        };
        let (a, b) = (read(file_a)?, read(file_b)?);
        if a != b {
            let show = |first_pass| {
                detcore_model::config::SleepRegistration::from_registered_at_first_pass(first_pass)
                    .as_str()
            };
            anyhow::bail!(
                "{} and {} were recorded under different --sleep-registration rules ({} and \
                 {}); no interpolation of them can be replayed",
                file_a.display(),
                file_b.display(),
                show(a),
                show(b)
            );
        }
        Ok(a)
    }

    /// The signal-state copy setting the interpolated schedule carries: the
    /// one the schedules state, where they agree or only one states it.
    /// Schedules recorded under different settings are refused.
    fn common_signal_state_copy(
        file_a: &std::path::Path,
        file_b: &std::path::Path,
    ) -> anyhow::Result<Option<bool>> {
        let read = |path: &std::path::Path| {
            detcore::preemptions::read_recorded_signal_state_copy(path).map_err(anyhow::Error::msg)
        };
        match (read(file_a)?, read(file_b)?) {
            (Some(a), Some(b)) if a != b => anyhow::bail!(
                "{} and {} were recorded with the signal-state copy {} and {}; no \
                 interpolation of them can be replayed",
                file_a.display(),
                file_b.display(),
                if a { "on" } else { "off" },
                if b { "on" } else { "off" },
            ),
            (a, b) => Ok(a.or(b)),
        }
    }
}

fn legacy_schedule(
    path: &std::path::Path,
    operation: &'static str,
) -> anyhow::Result<Vec<SchedEvent>> {
    Ok(Recording::read(path)?.into_legacy(operation)?.into_global())
}

fn record_tid(record: &Record) -> DetTid {
    match record {
        Record::Event(event) => event.dettid,
        Record::TimesliceEnd(boundary) => boundary.identity.owner.tid,
    }
}

// Inspection is read-only and names stored indices in the complete stream.
// It never projects away markers to pass them to a legacy editor.
fn inspect_boundaries(schedule: &Schedule, verbose: bool) -> anyhow::Result<String> {
    use std::fmt::Write;
    let records = schedule.records();
    let boundaries = records
        .iter()
        .filter(|r| matches!(r, Record::TimesliceEnd(_)))
        .count();
    let switches = records
        .windows(2)
        .filter(|p| record_tid(&p[0]) != record_tid(&p[1]))
        .count();
    let mut result = format!(
        "Slice-boundary schedule: {} stored records, {} ordinary events, {} slice boundaries\nContext-switch endpoints: {}\n",
        records.len(),
        records.len() - boundaries,
        boundaries,
        switches,
    );
    if verbose {
        for (index, record) in records.iter().enumerate() {
            writeln!(result, "{index}: {}", serde_json::to_string(record)?)?;
        }
    }
    Ok(result)
}

fn print_boundaries(
    schedule: &Schedule,
    indices: bool,
    tid: Option<i32>,
) -> anyhow::Result<String> {
    use std::fmt::Write;
    let mut result = String::new();
    let mut counts = BTreeMap::<DetTid, u64>::new();
    let mut last_skipped = false;
    for (index, record) in schedule.records().iter().enumerate() {
        let owner = record_tid(record);
        let count = counts.entry(owner).or_default();
        let thread_index = *count;
        *count += 1;
        if tid.is_some_and(|tid| tid != owner.as_raw()) {
            if !last_skipped {
                result.push('\n');
            }
            last_skipped = true;
            continue;
        }
        last_skipped = false;
        if indices {
            write!(result, "{: <9} ", format!("({index},{thread_index})"))?;
        }
        writeln!(result, "{}", serde_json::to_string(record)?)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interpolation_input(events: Vec<SchedEvent>) -> PreemptionRecord {
        // Nondefault current-main metadata must survive together with the new
        // format/jump fields. Deserialize the public record representation,
        // as the command does, rather than constructing private fields.
        let mut value =
            serde_json::to_value(PreemptionRecord::from_sched_events(events).with_current_format())
                .unwrap();
        value["epoch"] = serde_json::json!("2026-01-01T00:00:00Z");
        value["cap_preemption_charge"] = serde_json::json!(true);
        value["chaos_timeslice"] = serde_json::json!(123_456);
        value["rng_seed"] = serde_json::json!(987);
        value["store_exclusive_failures"] = serde_json::json!(7);
        value["host_inputs"] = serde_json::json!({"digest": "source-inputs", "files": []});
        serde_json::from_value(value).unwrap()
    }

    fn interpolate_records(
        first: &PreemptionRecord,
        second: &PreemptionRecord,
        interpolate: u8,
    ) -> anyhow::Result<PreemptionRecord> {
        let directory = tempfile::tempdir().unwrap();
        let file_a = directory.path().join("a.events");
        let file_b = directory.path().join("b.events");
        first.write_to_disk(&file_a).unwrap();
        second.write_to_disk(&file_b).unwrap();
        InterpolateOpts {
            interpolate,
            file_a,
            file_b,
        }
        .interpolated_record()
    }

    #[test]
    fn interpolation_preserves_source_metadata_and_refuses_unmapped_time_history() {
        use detcore::preemptions::TimeSkip;
        use detcore::preemptions::TimeSkipReason;
        use detcore_model::pid::GuestIds;
        use detcore_model::time::LogicalTime;
        let events: Vec<_> = [3, 4, 5]
            .map(|tid| SchedEvent::branches(DetTid::from_raw(tid), 1))
            .into();
        let first = interpolation_input(events.clone()).with_time_skips(vec![TimeSkip {
            event: 1,
            target: LogicalTime::from_nanos(1_000),
            reason: TimeSkipReason::FifoOpenAlone,
        }]);
        // A=A through the command's actual read/interpolate path preserves
        // every serialized field, including host-observed counts (whose Eq
        // implementation intentionally ignores their value).
        for percentage in [0, 50, 100] {
            let result = interpolate_records(&first, &first, percentage).unwrap();
            assert_eq!(
                serde_json::to_value(&result).unwrap(),
                serde_json::to_value(first.clone().with_guest_ids(GuestIds::Namespace)).unwrap()
            );
            assert!(result.charges_replayed_switch_ins());
            assert_eq!(result.time_skips(), first.time_skips());
        }
        let mut reverse = events.clone();
        reverse.reverse();
        let second = interpolation_input(reverse).with_time_skips(vec![TimeSkip {
            event: 2,
            target: LogicalTime::from_nanos(2_000),
            reason: TimeSkipReason::IdleGuestPollers,
        }]);
        let mut endpoint_schedules = Vec::new();
        for percentage in [0, 100] {
            let result = interpolate_records(&first, &second, percentage).unwrap();
            let source = if result.schedevents() == first.schedevents() {
                &first
            } else {
                assert_eq!(result.schedevents(), second.schedevents());
                &second
            };
            assert_eq!(
                serde_json::to_value(&result).unwrap(),
                serde_json::to_value(source.clone().with_guest_ids(GuestIds::Namespace)).unwrap()
            );
            endpoint_schedules.push(result.into_global());
        }
        assert_ne!(endpoint_schedules[0], endpoint_schedules[1]);
        let error = interpolate_records(&first, &second, 50).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("mixed schedule with recorded time jumps"),
            "{error}"
        );

        let other_format = first.clone().with_format(Some(2));
        let error = interpolate_records(&first, &other_format, 50).unwrap_err();
        assert!(error.to_string().contains("record formats"), "{error}");
    }

    #[test]
    fn mixed_interpolation_keeps_compatible_metadata_and_rejects_raw_count_mismatches() {
        let events: Vec<_> = [3, 4, 5]
            .map(|tid| SchedEvent::branches(DetTid::from_raw(tid), 1))
            .into();
        let first = interpolation_input(events.clone());
        let mut reverse = events;
        reverse.reverse();
        let second = interpolation_input(reverse);
        for format in [None, Some(2), Some(3)] {
            let first = first.clone().with_format(format);
            let second = second.clone().with_format(format);
            let result = interpolate_records(&first, &second, 50).unwrap();
            assert_ne!(result.schedevents(), first.schedevents());
            assert_ne!(result.schedevents(), second.schedevents());
            let mut expected = serde_json::to_value(&first).unwrap();
            expected["global"] = serde_json::to_value(result.schedevents()).unwrap();
            // Main's namespace numbering retains the legacy omitted field.
            assert_eq!(result.guest_ids(), None);
            assert_eq!(serde_json::to_value(&result).unwrap(), expected);
            for (field, changed) in [
                ("store_exclusive_failures", serde_json::json!(8)),
                ("epoch", serde_json::json!("2026-01-02T00:00:00Z")),
                ("rng_seed", serde_json::json!(988)),
                ("chaos_timeslice", serde_json::json!(123_457)),
                ("cap_preemption_charge", serde_json::json!(false)),
                (
                    "host_inputs",
                    serde_json::json!({"digest": "other-inputs", "files": []}),
                ),
            ] {
                let mut value = serde_json::to_value(&second).unwrap();
                value[field] = changed;
                let incompatible = serde_json::from_value(value).unwrap();
                let error = interpolate_records(&first, &incompatible, 50).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("incompatible recording metadata"),
                    "{field}: {error}"
                );
            }
        }
    }

    fn synthetic_chosen_reserved_input() -> serde_json::Value {
        // A recognized descriptor for synthetic editor inputs, not a live
        // setup receipt or a way to qualify an existing recording.
        serde_json::json!({
            "contract": "sealed-private-input-v1",
            "base": "0x71010000",
            "page_bytes": 4096,
            "abi": "x86_64",
        })
    }

    #[test]
    fn identity_interpolation_preserves_chosen_guest_ids() {
        use detcore_model::pid::GuestIds;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chosen.json");
        let ids = GuestIds::Chosen {
            base: 5000,
            refuse_host_ids: true,
        };
        let events = vec![
            SchedEvent::branches(DetTid::from_raw(5000), 3),
            SchedEvent::branches(DetTid::from_raw(5001), 2),
        ];
        let mut input =
            serde_json::to_value(PreemptionRecord::from_sched_events(events.clone())).unwrap();
        input["guest_ids"] = serde_json::to_value(ids).unwrap();
        let unmarked = input.clone();
        let unmarked_path = directory.path().join("unmarked.json");
        std::fs::write(&unmarked_path, serde_json::to_vec(&unmarked).unwrap()).unwrap();
        input["reserved_input"] = synthetic_chosen_reserved_input();
        std::fs::write(&path, serde_json::to_vec(&input).unwrap()).unwrap();
        assert_eq!(
            detcore::preemptions::read_recorded_guest_ids(&path).unwrap(),
            ids
        );
        for interpolate in [0, 50, 100] {
            let record = InterpolateOpts {
                interpolate,
                file_a: path.clone(),
                file_b: path.clone(),
            }
            .interpolated_record()
            .unwrap();
            assert_eq!(record.guest_ids(), Some(ids));
            assert_eq!(
                record.reserved_input().as_json(),
                Some(&input["reserved_input"])
            );
            assert_eq!(record.into_global(), events);
            // Identity and endpoint shortcuts must still refuse the original
            // unmarked fixture, including either unmarked input position.
            for (file_a, file_b, source) in [
                (&unmarked_path, &unmarked_path, "first"),
                (&unmarked_path, &path, "first"),
                (&path, &unmarked_path, "second"),
            ] {
                let error = InterpolateOpts {
                    interpolate,
                    file_a: file_a.clone(),
                    file_b: file_b.clone(),
                }
                .interpolated_record()
                .unwrap_err();
                assert!(
                    error.to_string().starts_with(&format!(
                        "HERMIT_CHOSEN_JSON_INPUT_PROVENANCE: {source} transformation input: missing reserved_input provenance"
                    )),
                    "{error}"
                );
            }
            assert_eq!(
                std::fs::read(&path).unwrap(),
                serde_json::to_vec(&input).unwrap()
            );
            assert_eq!(
                std::fs::read(&unmarked_path).unwrap(),
                serde_json::to_vec(&unmarked).unwrap()
            );
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
        }
    }

    #[test]
    fn interpolation_requires_compatible_guest_numbering_before_output() {
        use detcore_model::pid::GuestIds;
        let directory = tempfile::tempdir().unwrap();
        let events = vec![SchedEvent::branches(DetTid::from_raw(5000), 3)];
        let inputs = [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::to_value(GuestIds::Namespace).unwrap()),
            Some(serde_json::to_value(GuestIds::Host).unwrap()),
            Some(
                serde_json::to_value(GuestIds::Chosen {
                    base: 5000,
                    refuse_host_ids: false,
                })
                .unwrap(),
            ),
            Some(
                serde_json::to_value(GuestIds::Chosen {
                    base: 5000,
                    refuse_host_ids: true,
                })
                .unwrap(),
            ),
            Some(
                serde_json::to_value(GuestIds::Chosen {
                    base: 6000,
                    refuse_host_ids: true,
                })
                .unwrap(),
            ),
        ];
        let paths: Vec<_> = inputs
            .iter()
            .enumerate()
            .map(|(index, ids)| {
                let path = directory.path().join(format!("{index}.json"));
                let mut record =
                    serde_json::to_value(PreemptionRecord::from_sched_events(events.clone()))
                        .unwrap();
                if let Some(ids) = ids {
                    record["guest_ids"] = ids.clone();
                    if matches!(
                        serde_json::from_value::<GuestIds>(ids.clone()),
                        Ok(GuestIds::Chosen { .. })
                    ) {
                        record["reserved_input"] = synthetic_chosen_reserved_input();
                    }
                }
                std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
                path
            })
            .collect();
        let options = CommonOpts::parse_from(["test"]);
        for (a, file_a) in paths.iter().enumerate() {
            for (b, file_b) in paths.iter().enumerate() {
                // Explicit oracle, including both input orders: omission,
                // null and Namespace agree; Chosen policies at one base agree.
                let compatible =
                    a == b || (a < 3 && b < 3) || ([4, 5].contains(&a) && [4, 5].contains(&b));
                let interpolation = InterpolateOpts {
                    interpolate: 50,
                    file_a: file_a.clone(),
                    file_b: file_b.clone(),
                };
                let before = (
                    std::fs::read(file_a).unwrap(),
                    std::fs::read(file_b).unwrap(),
                );
                if compatible {
                    let record = interpolation.interpolated_record().unwrap();
                    assert_eq!(
                        record.guest_ids().unwrap_or_default(),
                        detcore::preemptions::read_recorded_guest_ids(file_a).unwrap()
                    );
                    let first: PreemptionRecord = serde_json::from_slice(&before.0).unwrap();
                    assert_eq!(record.reserved_input(), first.reserved_input());
                    assert_eq!(record.into_global(), events);
                } else {
                    let error = interpolation.run(&options).unwrap_err();
                    assert!(
                        error.to_string().contains("incompatible guest process IDs"),
                        "{error}"
                    );
                }
                assert_eq!(std::fs::read(file_a).unwrap(), before.0);
                assert_eq!(std::fs::read(file_b).unwrap(), before.1);
                assert_eq!(
                    std::fs::read_dir(directory.path()).unwrap().count(),
                    paths.len()
                );
            }
        }
    }

    #[test]
    fn boundary_inspection_keeps_every_record_index_and_editors_refuse_the_format() {
        use detcore::preemptions::boundary_recording::BoundaryRecording;
        use detcore_model::schedule::boundary::Boundary;
        use detcore_model::schedule::boundary::Cause;
        use detcore_model::schedule::boundary::Entry;
        use detcore_model::schedule::boundary::Identity;
        use detcore_model::schedule::boundary::Owner;
        use detcore_model::schedule::boundary::Phase;
        let tid = DetTid::from_raw(3);
        let owner = Owner {
            tid,
            thread_generation: 0,
            birth_tid: tid,
            birth_generation: 0,
            address_space_creator: tid,
            address_space_generation: 0,
        };
        let schedule = Schedule::new(vec![
            Record::Event(SchedEvent::branches(tid, 3)),
            Record::TimesliceEnd(Boundary {
                identity: Identity {
                    owner,
                    cause: Cause::SoftwareCheckin,
                    phase: Phase::Prehook,
                    entry: Entry::Cpuid,
                    committed_rcbs: 3,
                    checkin: 1,
                    slice: 1,
                },
                count: 1,
                start_rip: None,
                end_rip: None,
                end_time: Some(detcore_model::time::LogicalTime::from_nanos(19)),
            }),
            Record::Event(SchedEvent::branches(DetTid::from_raw(4), 2)),
        ])
        .unwrap();
        let inspection = inspect_boundaries(&schedule, true).unwrap();
        assert!(inspection.contains("3 stored records, 2 ordinary events, 1 slice boundaries"));
        assert!(inspection.contains("Context-switch endpoints: 1"));
        for (index, record) in schedule.records().iter().enumerate() {
            assert!(inspection.contains(&format!(
                "{index}: {}\n",
                serde_json::to_string(record).unwrap()
            )));
        }
        let printed = print_boundaries(&schedule, false, None).unwrap();
        let decoded: Vec<Record> = printed
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(decoded, schedule.records());
        let filtered = print_boundaries(&schedule, true, Some(3)).unwrap();
        assert!(filtered.starts_with("(0,0)"));
        assert!(filtered.lines().nth(1).unwrap().starts_with("(1,1)"));
        assert_eq!(filtered.lines().count(), 3);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recording.json");
        let recording = Recording::Boundaries(
            BoundaryRecording::new(PreemptionRecord::from_sched_events(vec![]), schedule).unwrap(),
        );
        recording.write(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(legacy_schedule(&path, "schedule diff/interpolation").is_err());
        let options = CommonOpts::parse_from(["test"]);
        assert!(
            DiffOpts {
                file_a: path.clone(),
                file_b: path.clone()
            }
            .run(&options)
            .is_err()
        );
        assert!(
            InterpolateOpts {
                interpolate: 50,
                file_a: path.clone(),
                file_b: path.clone()
            }
            .run(&options)
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert!(
            inspect_boundaries(&Schedule::new(vec![]).unwrap(), true)
                .unwrap()
                .contains("0 stored records")
        );
        assert!(preempt_events(&[]).is_empty());
    }

    /// Two schedules recorded under different signal-state copy settings
    /// cannot be interpolated; one that does not say takes the other's.
    #[test]
    fn interpolate_refuses_schedules_recorded_under_different_signal_state_copy_settings() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, copy: Option<bool>| {
            let path = dir.path().join(name);
            let record =
                PreemptionRecord::from_sched_events(Vec::new()).with_signal_state_copy(copy);
            std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
            path
        };
        let unsaid = write("unsaid.json", None);
        let on = write("on.json", Some(true));
        let off = write("off.json", Some(false));
        let common = InterpolateOpts::common_signal_state_copy;
        assert_eq!(common(&unsaid, &unsaid).unwrap(), None);
        assert_eq!(common(&on, &on).unwrap(), Some(true));
        assert_eq!(common(&unsaid, &off).unwrap(), Some(false));
        assert_eq!(common(&on, &unsaid).unwrap(), Some(true));
        for (a, b) in [(&on, &off), (&off, &on)] {
            let error = common(a, b).unwrap_err();
            assert!(error.to_string().contains("signal-state copy"), "{error}");
        }
    }

    /// Two schedules whose runs charged expired timed waits differently
    /// cannot be interpolated; agreeing ones give their rule to the result.
    #[test]
    fn interpolate_refuses_schedules_recorded_under_different_expired_wait_charges() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, once: bool| {
            let path = dir.path().join(name);
            let record = PreemptionRecord::from_sched_events(Vec::new())
                .with_expired_wait_runs_charged_once(once);
            std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
            path
        };
        let each = write("each.json", false);
        let once = write("once.json", true);
        assert!(!InterpolateOpts::common_expired_wait_charge(&each, &each).unwrap());
        assert!(InterpolateOpts::common_expired_wait_charge(&once, &once).unwrap());
        let error = InterpolateOpts::common_expired_wait_charge(&each, &once).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("different --expired-wait-charge rules (each and once)"),
            "{error}"
        );
    }

    /// Two schedules whose runs registered queued sleeps differently cannot
    /// be interpolated; agreeing ones give their rule to the result.
    #[test]
    fn interpolate_refuses_schedules_recorded_under_different_sleep_registrations() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, first_pass: bool| {
            let path = dir.path().join(name);
            let record = PreemptionRecord::from_sched_events(Vec::new())
                .with_queued_sleeps_registered_at_first_pass(first_pass);
            std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
            path
        };
        let chosen = write("when-chosen.json", false);
        let first = write("first-pass.json", true);
        assert!(!InterpolateOpts::common_sleep_registration(&chosen, &chosen).unwrap());
        assert!(InterpolateOpts::common_sleep_registration(&first, &first).unwrap());
        let error = InterpolateOpts::common_sleep_registration(&chosen, &first).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("different --sleep-registration rules (when-chosen and first-pass)"),
            "{error}"
        );
    }

    /// Two schedules recorded under different `--pipe-wake-latency` values,
    /// off and `0` included, cannot be interpolated: no replay could
    /// reproduce the result (https://github.com/rrnewton/hermit/issues/4380).
    #[test]
    fn interpolate_refuses_schedules_recorded_under_different_pipe_wake_latencies() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, latency: Option<u64>| {
            let path = dir.path().join(name);
            let record =
                PreemptionRecord::from_sched_events(Vec::new()).with_pipe_wake_latency(latency);
            std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
            path
        };
        let off = write("off.json", None);
        let zero = write("zero.json", Some(0));
        let one_ms = write("one-ms.json", Some(1_000_000));
        assert_eq!(
            InterpolateOpts::common_pipe_wake_latency(&off, &off).unwrap(),
            None
        );
        assert_eq!(
            InterpolateOpts::common_pipe_wake_latency(&zero, &zero).unwrap(),
            Some(0)
        );
        for (a, b) in [(&off, &zero), (&zero, &one_ms), (&one_ms, &off)] {
            let error = InterpolateOpts::common_pipe_wake_latency(a, b).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("different --pipe-wake-latency values"),
                "{error}"
            );
        }
    }
}

#[cfg(test)]
mod replay_departure_tests {
    use super::*;

    #[test]
    fn interpolation_retains_departure_from_either_source() {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("a.json");
        let b = directory.path().join("b.json");
        let events = vec![detcore::types::SchedEvent::branches(
            detcore::types::DetTid::from_raw(3),
            1,
        )];
        for left in [None, Some(0), Some(7)] {
            for right in [None, Some(3), Some(9)] {
                PreemptionRecord::from_sched_events(events.clone())
                    .retain_replay_departure(left)
                    .write_to_disk(&a)
                    .unwrap();
                PreemptionRecord::from_sched_events(events.clone())
                    .retain_replay_departure(right)
                    .write_to_disk(&b)
                    .unwrap();
                for interpolate in [0, 50, 100] {
                    let record = InterpolateOpts {
                        interpolate,
                        file_a: a.clone(),
                        file_b: b.clone(),
                    }
                    .interpolated_record()
                    .unwrap();
                    let expected = match (left, right) {
                        (Some(x), Some(y)) => Some(x.min(y)),
                        (x, y) => x.or(y),
                    };
                    assert_eq!(record.replay_departed_at(), expected);
                    assert_eq!(record.schedevents(), &events);
                    assert_eq!(
                        record.require_faithful_replay().is_err(),
                        expected.is_some()
                    );
                    assert_eq!(
                        serde_json::from_str::<PreemptionRecord>(&record.to_string()).unwrap(),
                        record
                    );
                }
            }
        }
    }
}
