/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Coverage guard for the just_spin split-trace fixture, in addition to the
//! verifier's existing comparisons. Counts describe completed timer correction
//! sequences with work, not timer callbacks or a census of ptrace operations.

use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use detcore::preemptions::PreemptionReader;
use detcore_model::schedule::Op;
use detcore_model::schedule::SchedEvent;
use detcore_model::time::LogicalTime;

pub(super) fn check_artifacts(directory: &Path) -> Result<()> {
    let read_log = |name: &str| {
        let path = directory.join(name);
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    };
    // TraceReplayOpts records execution 1, then replays it in execution 2.
    // Use their separate artifacts, never the combined verifier stdout.
    let record_log = read_log("1_log")?;
    let replay_log = read_log("2_log")?;
    let record = corrections(&record_log).context("record log 1_log")?;
    let replay = corrections(&replay_log).context("replay log 2_log")?;
    let record_unavailable = timer_unavailable(&record_log, &read_log("1_std_err")?)
        .context("record timer availability")?;
    let replay_unavailable = timer_unavailable(&replay_log, &read_log("2_std_err")?)
        .context("replay timer availability")?;
    let read_schedule = |name: &str| {
        let path = directory.join(name);
        PreemptionReader::try_new(&path)
            .map(PreemptionReader::into_inner)
            .with_context(|| format!("reading {}", path.display()))
    };
    let mut original = read_schedule("1_sched.bak")?;
    let edited = read_schedule("1_sched")?.into_global();
    // Editing drops callback/background-run metadata and its now-redundant
    // stopless marks. Apply that metadata-only operation to the original,
    // without calling the branch splitter: counts, RIPs, times and instruction
    // offsets must still match exactly in split_pairs below.
    original.split_map(|event, _| [event]);
    let pairs = split_pairs(&original.into_global(), &edited)?;
    eprintln!(
        "split trace coverage: record_corrections={record} replay_corrections={replay} split_pairs={pairs}"
    );
    ensure!(
        record_unavailable == replay_unavailable,
        "record/replay timer availability disagrees: record={record_unavailable:?} replay={replay_unavailable:?}"
    );
    if let Some(reason) = record_unavailable {
        ensure!(
            record == 0 && replay == 0 && pairs == 0,
            "PMU-unavailable evidence contradicts correction work or split fragments; \
             record_corrections={record} replay_corrections={replay} split_pairs={pairs}"
        );
        // The verifier's comparisons already passed. Only the added PMU
        // coverage requirement is inapplicable, based on these two runs.
        eprintln!("split trace coverage: COVERAGE_NOT_APPLICABLE reason=pmu-unavailable: {reason}");
        return Ok(());
    }
    require_coverage(record, replay, pairs)
}

/// Only an actual startup downgrade, corroborated by its effective controller
/// configuration, can establish unavailability. Explicitly disabling the timer,
/// an environment label, or absent correction work cannot do so.
fn timer_unavailable(log: &str, stderr: &str) -> Result<Option<String>> {
    ensure!(
        stderr.is_empty() || stderr.ends_with('\n'),
        "unterminated stderr"
    );
    let mut unavailable = None;
    for line in stderr
        .lines()
        .filter(|line| line.contains("--max-timeslice requires"))
    {
        ensure!(unavailable.is_none(), "duplicate timer downgrade warning");
        unavailable = Some(
            if line
                == "WARNING: --max-timeslice requires user-space perf counters, but perf_event_open is unavailable; continuing with --max-timeslice=disabled. Check the host perf_event_paranoid value and container seccomp policy."
            {
                "no-perf-events".to_owned()
            } else {
                let reason = line
                .strip_prefix("WARNING: --max-timeslice requires a performance-counter profile for this CPU, but ")
                .and_then(|text| text.strip_suffix("; continuing with --max-timeslice=disabled."))
                .filter(|reason| !reason.is_empty() && reason.trim() == *reason)
                .context("malformed timer downgrade warning")?;
                format!("no-pmu-profile: {reason}")
            },
        );
    }
    let mut enabled = None;
    for line in log
        .lines()
        .filter(|line| line.contains("Configured maximum timeslice"))
    {
        ensure!(enabled.is_none(), "duplicate controller timer summary");
        let summary = line
            .split_once("hermit::controller: hermit: Configured maximum timeslice: ")
            .map(|(_, summary)| summary)
            .context("timer summary without its controller target")?;
        enabled = Some(if summary == "disabled; timer: none" {
            false
        } else {
            let maximum = summary
                .strip_suffix(" virtual ns; timer: enabled")
                .context("malformed controller timer summary")?;
            ensure!(
                number(maximum)? > 0,
                "nonpositive controller maximum timeslice"
            );
            true
        });
    }
    let enabled = enabled.context("missing controller timer summary")?;
    ensure!(
        enabled == unavailable.is_none(),
        "controller timer summary contradicts startup downgrade evidence"
    );
    Ok(unavailable)
}

fn require_coverage(record: usize, replay: usize, pairs: usize) -> Result<()> {
    ensure!(
        record > 0 && replay > 0 && pairs > 0,
        "expected positive-work timer corrections in BOTH record and replay and an actual branch split; \
         record_corrections={record} replay_corrections={replay} split_pairs={pairs}"
    );
    Ok(())
}

fn number(text: &str) -> Result<u64> {
    ensure!(
        !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()),
        "invalid timer count {text:?}"
    );
    text.parse().context("timer count exceeds u64")
}

fn completion_work(message: &str) -> Result<bool> {
    let message = message
        .strip_prefix("Timer reached its target rcb: ")
        .context("missing completion target")?;
    let (rcb, message) = message.split_once(", instr: ").context("missing offset")?;
    let (offset, message) = message
        .split_once(" with ")
        .context("missing step counts")?;
    let (blocks, singles) = message
        .split_once(" block steps or branch runs and ")
        .context("missing block/single step counts")?;
    let singles = singles
        .strip_suffix(" single steps")
        .context("malformed single step count")?;
    number(rcb)?;
    number(offset)?;
    let blocks = number(blocks)?;
    let singles = number(singles)?;
    // attempt_single_step also logs a completion if it was already at target.
    // Such a zero-step completion is not evidence of correction work.
    Ok(blocks > 0 || singles > 0)
}

fn corrections(log: &str) -> Result<usize> {
    ensure!(
        !log.is_empty() && log.ends_with('\n'),
        "empty or unterminated log"
    );
    let mut count = 0;
    for (line_number, line) in log.lines().enumerate() {
        if !line.contains("Timer reached its target") {
            continue;
        }
        // Timestamp, severity and tracing spans are not part of the witness.
        // A recognized but malformed message must not be silently ignored.
        let message = line
            .split_once("reverie_ptrace::timer: ")
            .map(|(_, message)| message.trim_end())
            .with_context(|| {
                format!(
                    "timer completion without its target at line {}",
                    line_number + 1
                )
            })?;
        count +=
            usize::from(completion_work(message).with_context(|| {
                format!("malformed timer completion at line {}", line_number + 1)
            })?);
    }
    Ok(count)
}

/// Every original event is either unchanged or replaced by exactly two valid
/// halves. Comparing all fields also refuses lost endpoints or unrelated edits;
/// a longer schedule or an existing pair of Branch events is not a witness.
fn split_pairs(original: &[SchedEvent], edited: &[SchedEvent]) -> Result<usize> {
    let mut remaining = edited.iter();
    let mut pairs = 0;
    for (index, event) in original.iter().enumerate() {
        let first = remaining
            .next()
            .with_context(|| format!("missing edited event for original event {index}"))?;
        if first == event {
            continue;
        }
        let next = original.get(index + 1).map(|event| &event.op);
        let after_next = original.get(index + 2).map(|event| &event.op);
        ensure!(
            event.op == Op::Branch
                && event.count > 1
                && event.instruction_offset.is_none()
                && (next == Some(&Op::Branch)
                    || next == Some(&Op::OtherInstructions) && after_next == Some(&Op::Branch)),
            "edited event {index} is not an eligible branch split"
        );
        let second = remaining
            .next()
            .with_context(|| format!("missing second fragment for original event {index}"))?;
        let second_count = event.count - event.count / 2;
        let expected_first = SchedEvent {
            count: event.count / 2,
            start_rip: None,
            end_rip: None,
            end_time: event.end_time.map(|time| {
                LogicalTime::from_nanos(time.as_nanos().wrapping_sub(10 * u64::from(second_count)))
            }),
            stopless: false,
            instruction_offset: None,
            ..event.clone()
        };
        let expected_second = SchedEvent {
            count: second_count,
            start_rip: None,
            ..event.clone()
        };
        ensure!(
            first == &expected_first && second == &expected_second,
            "invalid split fragments for original event {index}: {first:?}, {second:?}; original {event:?}"
        );
        pairs += 1;
    }
    ensure!(remaining.next().is_none(), "unmatched extra edited events");
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use detcore_model::pid::DetPid;

    use super::*;

    const WORK: &str = "arbitrary prefix DEBUG reverie_ptrace::timer: Timer reached its target rcb: 100, instr: 0 with 7 block steps or branch runs and 2 single steps\n";
    const NO_WORK: &str = "DEBUG reverie_ptrace::timer: Timer reached its target rcb: 100, instr: 0 with 0 block steps or branch runs and 0 single steps\n";
    const ENABLED: &str = "DEBUG hermit::controller: hermit: Configured maximum timeslice: 10000000 virtual ns; timer: enabled\n";
    const DISABLED: &str =
        "DEBUG hermit::controller: hermit: Configured maximum timeslice: disabled; timer: none\n";
    const NO_PERF: &str = "WARNING: --max-timeslice requires user-space perf counters, but perf_event_open is unavailable; continuing with --max-timeslice=disabled. Check the host perf_event_paranoid value and container seccomp policy.\n";
    const NO_PROFILE: &str = "WARNING: --max-timeslice requires a performance-counter profile for this CPU, but Could not read cpu info; continuing with --max-timeslice=disabled.\n";

    #[test]
    fn each_phase_requires_positive_work() {
        let positive = corrections(WORK).unwrap();
        let zero = corrections(NO_WORK).unwrap();
        assert_eq!((positive, zero), (1, 0));
        assert_eq!(corrections(&format!("{NO_WORK}{WORK}")).unwrap(), 1);
        assert!(require_coverage(positive, positive, 1).is_ok());
        for (record, replay) in [(zero, positive), (positive, zero), (zero, zero)] {
            let error = require_coverage(record, replay, 1).unwrap_err();
            assert!(error.to_string().contains(&format!(
                "record_corrections={record} replay_corrections={replay}"
            )));
        }
        assert!(require_coverage(positive, positive, 0).is_err());
        assert_eq!(timer_unavailable(ENABLED, "").unwrap(), None);
        assert_eq!(
            timer_unavailable(DISABLED, NO_PERF).unwrap().as_deref(),
            Some("no-perf-events")
        );
        assert_eq!(
            timer_unavailable(DISABLED, NO_PROFILE).unwrap().as_deref(),
            Some("no-pmu-profile: Could not read cpu info")
        );
    }

    #[test]
    fn malformed_or_missing_evidence_is_not_coverage() {
        for malformed in [
            WORK.replace("rcb: 100", "rcb: unknown"),
            WORK.replace("instr: 0", "instr: -1"),
            WORK.replace("7 block", "NaN block"),
            WORK.replace("2 single steps", "2 single"),
            WORK.replace("reverie_ptrace::timer:", "another_target:"),
            WORK.trim_end().to_owned(),
            String::new(),
        ] {
            assert!(corrections(&malformed).is_err(), "{malformed:?}");
            // Earlier valid evidence must not hide a later malformed marker.
            if !malformed.is_empty() {
                assert!(corrections(&format!("{WORK}{malformed}")).is_err());
            }
        }
        assert_eq!(corrections("other log messages\n").unwrap(), 0);
        for warning in [
            String::new(),
            NO_PERF.replace("is unavailable", "is available"),
            NO_PERF.replace("WARNING:", "warning:"),
            NO_PERF.replace("container seccomp policy.", "container seccomp policy"),
            NO_PERF.trim_end().to_owned(),
            NO_PROFILE.replace("Could not read cpu info", ""),
            NO_PROFILE.replace("Could not read cpu info", " "),
            NO_PROFILE.replace("--max-timeslice=disabled.", "--max-timeslice=disabled"),
            format!("{NO_PERF}{NO_PERF}"),
            format!("{NO_PERF}{NO_PROFILE}"),
            format!("{NO_PERF}{}", NO_PERF.replace("WARNING:", "warning:")),
        ] {
            assert!(
                timer_unavailable(DISABLED, &warning).is_err(),
                "{warning:?}"
            );
        }
        for log in [
            String::new(),
            WORK.to_owned(),
            DISABLED.replace("hermit::controller:", "another_target:"),
            DISABLED.replace("timer: none", "timer: enabled"),
            format!("{DISABLED}{DISABLED}"),
            format!("{DISABLED}{ENABLED}"),
            ENABLED.to_owned(),
        ] {
            assert!(timer_unavailable(&log, NO_PERF).is_err(), "{log:?}");
        }
        for log in [
            ENABLED.replace("10000000", "0"),
            ENABLED.replace("10000000", "unknown"),
            ENABLED.replace("timer: enabled", "timer: none"),
            format!("{ENABLED}{ENABLED}"),
        ] {
            assert!(timer_unavailable(&log, "").is_err(), "{log:?}");
        }
        let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let error = check_artifacts(directory.path()).unwrap_err();
        assert!(format!("{error:#}").contains("1_log"));
    }

    fn schedules() -> (Vec<SchedEvent>, Vec<SchedEvent>) {
        let tid = DetPid::from_raw(3);
        let original = vec![
            SchedEvent {
                count: 7,
                start_rip: NonZeroUsize::new(0x1000),
                end_rip: NonZeroUsize::new(0x2000),
                end_time: Some(LogicalTime::from_nanos(10000)),
                ..SchedEvent::branches(tid, 7)
            },
            SchedEvent::branches(tid, 1),
        ];
        let edited = vec![
            SchedEvent {
                count: 3,
                end_time: Some(LogicalTime::from_nanos(9960)),
                ..SchedEvent::branches(tid, 3)
            },
            SchedEvent {
                count: 4,
                start_rip: None,
                ..original[0].clone()
            },
            original[1].clone(),
        ];
        (original, edited)
    }

    #[test]
    fn artifacts_bind_each_phase_and_require_both_schedules() {
        let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let path = directory.path();
        let (original, edited) = schedules();
        let write_schedule = |name: &str, events: &[SchedEvent]| {
            let mut writer = detcore::preemptions::PreemptionWriter::new(None);
            for event in events {
                writer.insert_schedevent(event.clone());
            }
            std::fs::write(path.join(name), writer.into_string()).unwrap();
        };
        let write_phase = |phase: usize, work: &str, controller: &str, stderr: &str| {
            std::fs::write(
                path.join(format!("{phase}_log")),
                format!("{controller}{work}"),
            )
            .unwrap();
            std::fs::write(path.join(format!("{phase}_std_err")), stderr).unwrap();
        };
        let expect_error = |expected: &str| {
            let error = check_artifacts(path).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        };
        write_schedule("1_sched.bak", &original);
        write_schedule("1_sched", &edited);
        write_phase(1, WORK, ENABLED, "");
        write_phase(2, WORK, ENABLED, "");
        check_artifacts(path).unwrap();
        for (record, replay, expected) in [
            (NO_WORK, WORK, "record_corrections=0 replay_corrections=1"),
            (WORK, NO_WORK, "record_corrections=1 replay_corrections=0"),
            (
                NO_WORK,
                NO_WORK,
                "record_corrections=0 replay_corrections=0",
            ),
        ] {
            write_phase(1, record, ENABLED, "");
            write_phase(2, replay, ENABLED, "");
            expect_error(expected);
        }
        write_phase(1, WORK, ENABLED, "");
        write_phase(2, WORK, ENABLED, "");
        for phase in [1, 2] {
            let name = format!("{phase}_std_err");
            std::fs::remove_file(path.join(&name)).unwrap();
            expect_error(&name);
            write_phase(phase, WORK, ENABLED, "");
        }

        // A real two-phase downgrade changes only the coverage applicability.
        // It cannot hide malformed logs, missing files or unrelated schedule edits.
        write_schedule("1_sched", &original);
        for warning in [NO_PERF, NO_PROFILE] {
            write_phase(1, NO_WORK, DISABLED, warning);
            write_phase(2, NO_WORK, DISABLED, warning);
            check_artifacts(path).unwrap();
        }
        write_phase(1, NO_WORK, DISABLED, NO_PERF);
        expect_error("record/replay timer availability disagrees");
        write_phase(2, NO_WORK, DISABLED, NO_PERF);
        for phase in [1, 2] {
            write_phase(phase, NO_WORK, ENABLED, "");
            expect_error("record/replay timer availability disagrees");
            write_phase(phase, NO_WORK, DISABLED, "");
            expect_error("contradicts startup downgrade evidence");
            write_phase(phase, NO_WORK, "", NO_PERF);
            expect_error("missing controller timer summary");
            write_phase(phase, WORK, DISABLED, NO_PERF);
            expect_error("contradicts correction work or split fragments");
            write_phase(
                phase,
                &WORK.replace("7 block", "NaN block"),
                DISABLED,
                NO_PERF,
            );
            expect_error("malformed timer completion");
            write_phase(phase, NO_WORK, DISABLED, NO_PERF);
        }
        write_schedule("1_sched", &edited);
        expect_error("contradicts correction work or split fragments");
        write_schedule("1_sched", &original[..1]);
        expect_error("missing edited event");
        std::fs::write(path.join("1_sched"), "not a schedule").unwrap();
        expect_error("1_sched");
        write_schedule("1_sched", &original);
        std::fs::remove_file(path.join("1_sched.bak")).unwrap();
        expect_error("1_sched.bak");
        std::fs::write(path.join("1_sched.bak"), "not a schedule").unwrap();
        expect_error("1_sched.bak");
    }

    #[test]
    fn split_witness_conserves_events_and_endpoints() {
        let (original, edited) = schedules();
        assert_eq!(split_pairs(&original, &edited).unwrap(), 1);
        assert_eq!(split_pairs(&original, &original).unwrap(), 0);
        assert!(split_pairs(&original, &edited[..2]).is_err());
        let mut extra = edited.clone();
        extra.push(original[1].clone());
        assert!(split_pairs(&original, &extra).is_err());
        let mut wrong_count = edited.clone();
        wrong_count[1].count += 1;
        assert!(split_pairs(&original, &wrong_count).is_err());
        let mut lost_endpoint = edited.clone();
        lost_endpoint[1].end_rip = None;
        assert!(split_pairs(&original, &lost_endpoint).is_err());
        let mut wrong_time = edited.clone();
        wrong_time[0].end_time = original[0].end_time;
        assert!(split_pairs(&original, &wrong_time).is_err());
        let mut wrong_offset = edited.clone();
        wrong_offset[1].instruction_offset = Some(detcore_model::schedule::InstructionOffset {
            rcb_target: 100,
            instructions: 1,
        });
        assert!(split_pairs(&original, &wrong_offset).is_err());
        let mut wrong_stop = edited.clone();
        wrong_stop[0].stopless = true;
        assert!(split_pairs(&original, &wrong_stop).is_err());
        let mut changed_tail = edited;
        changed_tail[2].dettid = DetPid::from_raw(5);
        assert!(split_pairs(&original, &changed_tail).is_err());
    }
}
