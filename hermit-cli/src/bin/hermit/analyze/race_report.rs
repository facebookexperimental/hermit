/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The report's "Racing accesses" section (design section 8): the final
//! replays on each side of the deciding event record an access trace, the
//! finder ([`races`](super::races)) compares the two, and one more replay of
//! the target continuation takes the two accesses' stacks.
//!
//! The section never fails the analysis: whatever stops it (a backend or mode
//! the access trace refuses, a replay that left its schedule, an unreadable
//! trace) becomes status `unavailable` with the reason, and a stack replay
//! that goes wrong leaves the stacks out with a note.

use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;

use colored::Colorize;
use detcore::preemptions::PreemptionRecord;
use detcore::preemptions::boundary_recording::Recording;
use detcore::types::SchedEvent;
use detcore_model::access_trace::AccessTrace;
use detcore_model::access_trace::EventIndex;
use detcore_model::access_trace::SyscallRecord;
use detcore_model::access_trace::TraceRecord;
use detcore_model::access_trace::read_access_trace;
use detcore_model::pid::DetPid;
use reverie::PrettyBacktrace;

use crate::app::analyze::phases::Departure;
use crate::app::analyze::phases::departure;
use crate::app::analyze::phases::stack_thread;
use crate::app::analyze::races::OutcomeCriteria;
use crate::app::analyze::races::RaceStatus;
use crate::app::analyze::races::RacingAccesses;
use crate::app::analyze::races::Window;
use crate::app::analyze::races::find_racing_accesses;
use crate::app::analyze::races::outcome_point;
use crate::app::analyze::rundata::RunData;
use crate::app::analyze::types::AnalyzeOpts;
use crate::app::container::RunTimeoutMarker;

/// Events before the deciding event each traced replay records, so the trace
/// shows the state the window starts from (who opened a descriptor, who last
/// closed it).
pub const RACE_LOOKBACK: u64 = 20_000;

/// A final replay asked to record an access trace, before it runs.
pub struct TraceRequest {
    /// Which side it is, for the report: "target" or "baseline".
    role: &'static str,
    /// Where it writes the trace, or why it writes none.
    trace: Result<PathBuf, String>,
}

/// A final replay that ran with an access trace.
pub struct TracedReplay {
    role: &'static str,
    trace: Result<PathBuf, String>,
    /// Where it left the schedule it had to follow up to the deciding event,
    /// if it did.
    departure: Option<Departure>,
    /// The schedule it realized (empty if it recorded none), recorded at
    /// `realized_path`.
    pub realized: Vec<SchedEvent>,
    pub realized_path: PathBuf,
    /// Why the schedule it recorded cannot be read, if it cannot: then
    /// whether it followed its schedule is unknown.
    schedule_error: Option<String>,
}

impl TraceRequest {
    /// The replay ran and recorded the schedule it realized at
    /// `realized_path`, which must follow `requested` through its first
    /// `until` events.
    pub fn ran(self, requested: &[SchedEvent], realized_path: &Path, until: usize) -> TracedReplay {
        let read = if realized_path.exists() {
            Recording::read(realized_path)
                .and_then(|recording| recording.into_legacy("the racing-accesses section"))
                .map(PreemptionRecord::into_global)
        } else {
            Ok(Vec::new())
        };
        match read {
            Ok(realized) => {
                let departure = departure(requested, &realized, until);
                self.ran_with(departure, realized, realized_path)
            }
            Err(error) => {
                let mut replay = self.ran_with(None, Vec::new(), realized_path);
                replay.schedule_error = Some(format!("{error:#}"));
                replay
            }
        }
    }

    /// The replay ran, realized `realized` (recorded at `realized_path`) and
    /// left its schedule as `departure` says.
    pub fn ran_with(
        self,
        departure: Option<Departure>,
        realized: Vec<SchedEvent>,
        realized_path: &Path,
    ) -> TracedReplay {
        TracedReplay {
            role: self.role,
            trace: self.trace,
            departure,
            realized,
            realized_path: realized_path.to_path_buf(),
            schedule_error: None,
        }
    }
}

/// A trace as the section reads it: at most `--race-window` records after
/// the deciding event.
struct ReadTrace {
    trace: AccessTrace,
    file: String,
    /// Set when `--race-window` stopped the read.
    capped: bool,
}

impl AnalyzeOpts {
    /// The `--access-trace-from` of the final replays for a deciding event at
    /// `decision`; `None` with `--no-racing-accesses`.
    pub(super) fn race_trace_from(&self, decision: usize) -> Option<u64> {
        (!self.no_racing_accesses).then(|| (decision as u64).saturating_sub(RACE_LOOKBACK))
    }

    /// The `--access-trace-max-records` of the final replays: the records a
    /// trace keeps from `--access-trace-from` on, which are the lookback's
    /// before the deciding event and `--race-window` after it. A trace cut
    /// there has no end line, so the section reads it as truncated.
    pub(super) fn race_trace_max_records(&self) -> u64 {
        RACE_LOOKBACK.saturating_add(self.race_window as u64)
    }

    /// Ask `run`, a final replay on side `role`, to record its schedule and an
    /// access trace from `from`. A run the access trace would be refused on
    /// (another backend, unsequentialized threads) runs without it, and the
    /// request keeps the refusal as the reason.
    pub(super) fn request_trace(
        &self,
        run: RunData,
        role: &'static str,
        from: Option<u64>,
    ) -> (RunData, Option<TraceRequest>) {
        let Some(from) = from else {
            return (run, None);
        };
        let (run, trace) = run
            .with_schedule_recording()
            .with_access_trace(from, self.race_trace_max_records());
        (run, Some(TraceRequest { role, trace }))
    }

    /// The section for `target` and `baseline`, two replays whose schedules
    /// agree before `decision`. `stack_run` builds the replay of the target
    /// continuation the stacks are taken from (it gets the run's name).
    pub(super) fn racing_accesses(
        &self,
        decision: usize,
        target: TracedReplay,
        baseline: TracedReplay,
        stack_run: impl FnOnce(&str) -> anyhow::Result<RunData>,
    ) -> RacingAccesses {
        let decision = decision as EventIndex;
        let traces = [&target, &baseline].map(|replay| self.read_trace(decision, replay));
        let [target_trace, baseline_trace] = match traces {
            [Ok(target), Ok(baseline)] => [target, baseline],
            [Err(reason), _] | [_, Err(reason)] => {
                return RacingAccesses::unavailable(decision, reason);
            }
        };
        // The root process's thread runs the first event of every schedule;
        // as a thread-group leader its thread id is its process id.
        let root = target
            .realized
            .first()
            .map(|event| DetPid::from_raw(event.dettid.as_raw()));
        let criteria = OutcomeCriteria {
            stdout: self.target_stdout.as_ref(),
            stderr: self.target_stderr.as_ref(),
            exit_code: &self.target_exit_code,
            root,
        };
        let outcome = outcome_point(&target_trace.trace, decision, &criteria);
        let outcome_thread = outcome.as_ref().map(|o| o.thread);
        let mut result = find_racing_accesses(
            &target_trace.trace,
            &baseline_trace.trace,
            Window { decision, outcome },
            outcome_thread,
        );
        for (role, read) in [("target", &target_trace), ("baseline", &baseline_trace)] {
            if read.capped {
                result.notes.push(format!(
                    "--race-window={} kept that many records of the {role} trace after the \
                     deciding event; raise it to read further.",
                    self.race_window
                ));
            }
        }
        result.trace_files = vec![target_trace.file.clone(), baseline_trace.file.clone()];
        if matches!(result.status, RaceStatus::Found | RaceStatus::Timing) {
            self.attach_stacks(&mut result, &target_trace.trace, stack_run);
        }
        result
    }

    /// Read one side's trace, or say why the section cannot use it.
    fn read_trace(&self, decision: EventIndex, replay: &TracedReplay) -> Result<ReadTrace, String> {
        let role = replay.role;
        if let Some(error) = &replay.schedule_error {
            return Err(format!(
                "the {role} replay's realized schedule {} cannot be read ({error}), so whether \
                 it followed its schedule is unknown",
                replay.realized_path.display()
            ));
        }
        if let Some(departure) = &replay.departure {
            return Err(format!(
                "the {role} replay left its schedule at or before the deciding event (it \
                 {departure}), \
                 so its accesses are not those of the {role} order"
            ));
        }
        let path = replay
            .trace
            .as_ref()
            .map_err(|refused| format!("the {role} replay records no access trace: {refused}"))?;
        let (trace, capped) = read_capped(path, decision, self.race_window).map_err(|error| {
            format!(
                "the {role} replay's access trace {} cannot be read: {error}",
                path.display()
            )
        })?;
        Ok(ReadTrace {
            trace,
            file: file_name(path),
            capped,
        })
    }

    /// Take both accesses' stacks from one replay of the target continuation,
    /// at their entry events. A stack the replay did not take, or took on
    /// another thread (it left the schedule), is left out with a note.
    fn attach_stacks(
        &self,
        result: &mut RacingAccesses,
        target: &AccessTrace,
        stack_run: impl FnOnce(&str) -> anyhow::Result<RunData>,
    ) {
        let entries: Vec<Option<EventIndex>> = result
            .accesses
            .iter()
            .map(|access| entry_event(target, access.thread, access.nth))
            .collect();
        match self.replay_for_access_stacks(&entries, stack_run) {
            Ok(stacks) => {
                for ((access, entry), stack) in result.accesses.iter_mut().zip(&entries).zip(stacks)
                {
                    if access.ended_first_by.is_some() {
                        result.notes.push(format!(
                            "Thread {}'s {} runs in the baseline only, so the target replay \
                             takes no stack for it.",
                            access.thread,
                            access.call.split('(').next().unwrap_or_default()
                        ));
                        continue;
                    }
                    match stack {
                        Some(stack) if stack_thread(&stack) == Some(access.thread) => {
                            access.stack = Some(stack);
                        }
                        Some(_) => result.notes.push(format!(
                            "The stack replay ran another thread than thread {} at event {}, so \
                             that access's stack is left out.",
                            access.thread,
                            entry.unwrap_or_default()
                        )),
                        None => result.notes.push(format!(
                            "The stack replay took no stack for thread {}'s access.",
                            access.thread
                        )),
                    }
                }
            }
            Err(error) => result.notes.push(format!(
                "The stacks are not available: the stack replay failed ({error:#})."
            )),
        }
    }

    /// Launch `stack_run` with stack events at `entries` and read the stacks
    /// it took, one per entry. A replay that a bound stops or Hermit refuses
    /// still yields what it took.
    fn replay_for_access_stacks(
        &self,
        entries: &[Option<EventIndex>],
        stack_run: impl FnOnce(&str) -> anyhow::Result<RunData>,
    ) -> anyhow::Result<Vec<Option<PrettyBacktrace>>> {
        let runname = "final_racing_access_stacks";
        let mut run = stack_run(runname)?;
        let root = run.root_path();
        let paths: Vec<PathBuf> = (0..entries.len())
            .map(|i| root.with_extension(format!("access{}.stack", i + 1)))
            .collect();
        let mut events: Vec<(u64, Option<PathBuf>)> = entries
            .iter()
            .zip(&paths)
            .filter_map(|(entry, path)| Some(((*entry)?, Some(path.clone()))))
            .collect();
        events.sort_by_key(|(event, _)| *event);
        run.runopts.det_opts.det_config.stacktrace_event = events;
        match run.launch() {
            Ok(()) => {}
            Err(error)
                if error.downcast_ref::<RunTimeoutMarker>().is_some()
                    || guest_reached_a_refusal(&error) =>
            {
                eprintln!(
                    ":: {} the racing-access stack replay did not finish ({error:#}); using the \
                     stacks it took",
                    "WARNING:".yellow().bold(),
                );
            }
            Err(error) => return Err(error),
        }
        Ok(paths.iter().map(|path| read_stack(path)).collect())
    }
}

/// Whether Hermit refused a run after its guest started: the container's
/// exit-122 arm returns a bare `PolicyRefusal`. A refusal decided before the
/// guest starts carries its reason as context instead.
fn guest_reached_a_refusal(error: &anyhow::Error) -> bool {
    error.chain().count() == 1
        && error
            .downcast_ref::<crate::app::container::PolicyRefusal>()
            .is_some()
}

/// The trace at `path`, read up to `window` records whose last event is at or
/// after `decision`; whether the window stopped the read.
fn read_capped(
    path: &Path,
    decision: EventIndex,
    window: usize,
) -> anyhow::Result<(AccessTrace, bool)> {
    let mut kept = Vec::new();
    let mut after = 0;
    let mut capped = false;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let reaches = serde_json::from_str::<TraceRecord>(&line)
            .is_ok_and(|record| record_reaches(&record, decision));
        if reaches {
            if after == window {
                capped = true;
                break;
            }
            after += 1;
        }
        kept.extend_from_slice(line.as_bytes());
        kept.push(b'\n');
    }
    Ok((read_access_trace(&kept[..])?, capped))
}

/// Whether a record's last event is at or after `decision` (a call that
/// never returned reaches every event).
fn record_reaches(record: &TraceRecord, decision: EventIndex) -> bool {
    let last = match record {
        TraceRecord::Syscall(sys) => sys.post.unwrap_or(EventIndex::MAX),
        TraceRecord::Signal(signal) => signal.event,
        TraceRecord::Edge(edge) => edge.to.event,
        TraceRecord::WatchHit(hit) => hit.after,
        TraceRecord::Header(_) | TraceRecord::End(_) => return false,
    };
    last >= decision
}

/// The entry (prehook) event of `thread`'s call `nth` in `trace`.
fn entry_event(
    trace: &AccessTrace,
    thread: detcore::types::DetTid,
    nth: u64,
) -> Option<EventIndex> {
    trace.records.iter().find_map(|record| match record {
        TraceRecord::Syscall(SyscallRecord {
            tid, nth: n, pre, ..
        }) if *tid == thread && *n == nth => Some(*pre),
        _ => None,
    })
}

fn read_stack(path: &Path) -> Option<PrettyBacktrace> {
    let file = File::open(path).ok()?;
    match serde_json::from_reader(file) {
        Ok(stack) => Some(stack),
        Err(error) => {
            eprintln!(
                ":: {} cannot read the stack trace in {} ({error}); it is left out",
                "WARNING:".yellow().bold(),
                path.display()
            );
            None
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    /// A traced replay asks the writer to stop after the lookback's records
    /// and the window's, so a long run does not write its whole trace.
    #[test]
    fn a_traced_replay_carries_the_record_cap() {
        let workspace = tempfile::tempdir().unwrap();
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", "--race-window=500", "--", "/bin/true"])
                .unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&options, "final_target_schedule".to_owned()).unwrap();
        let (run, request) = options.request_trace(run, "target", options.race_trace_from(30_000));
        assert!(request.unwrap().trace.is_ok());
        let repro = run.to_repro();
        for flag in [
            "--access-trace-from=10000",
            "--access-trace-max-records=20500",
        ] {
            assert!(repro.contains(flag), "no {flag} in {repro}");
        }
    }

    /// A realized schedule that cannot be read makes the section
    /// unavailable with that reason, not with a departure computed against
    /// an empty schedule, and does not panic.
    #[test]
    fn an_unreadable_realized_schedule_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let realized_path = dir.path().join("final_target_for_stacktraces_out.preempts");
        std::fs::write(&realized_path, b"{ not a schedule").unwrap();
        let request = TraceRequest {
            role: "target",
            trace: Ok(dir.path().join("final_target_for_stacktraces.access")),
        };
        let requested = vec![SchedEvent::branches(
            detcore::types::DetTid::from_raw(3),
            10,
        )];
        let replay = request.ran(&requested, &realized_path, 1);
        assert_eq!(replay.departure, None);
        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let reason = options.read_trace(0, &replay).err().unwrap();
        assert!(
            reason.starts_with("the target replay's realized schedule")
                && reason.contains("cannot be read"),
            "{reason}"
        );
    }
}
