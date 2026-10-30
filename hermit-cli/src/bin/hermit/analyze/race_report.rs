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
//! the access trace refuses, an unreadable trace) becomes status
//! `unavailable` with the reason, and a stack replay that goes wrong leaves
//! the stacks out with a note. A replay that left its schedule before the
//! deciding event (the two-seed search's interleavings cannot always be
//! realized) is compared as it ran: from the first event at which the two
//! replays run different threads, the event the stack recovery uses too.

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
use detcore_model::schedule::Op;
use detcore_model::schedule::SyscallPhase;
use reverie::PrettyBacktrace;
use reverie::syscalls::Sysno;

use crate::app::analyze::phases::Departure;
use crate::app::analyze::phases::departure;
use crate::app::analyze::phases::stack_thread;
use crate::app::analyze::races::CandidateCheck;
use crate::app::analyze::races::CheckVerdict;
use crate::app::analyze::races::CrossedThread;
use crate::app::analyze::races::MovedEvents;
use crate::app::analyze::races::Outcome;
use crate::app::analyze::races::OutcomeCriteria;
use crate::app::analyze::races::RaceCandidate;
use crate::app::analyze::races::RaceStatus;
use crate::app::analyze::races::RacingAccess;
use crate::app::analyze::races::RacingAccesses;
use crate::app::analyze::races::Window;
use crate::app::analyze::races::find_racing_accesses;
use crate::app::analyze::races::outcome;
use crate::app::analyze::races::uncounted_signal_outcome;
use crate::app::analyze::rundata::RunData;
use crate::app::analyze::rundata::guest_reached_a_refusal;
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
    ///
    /// When a replay left its schedule before `decision`, the two replays
    /// are compared as they ran instead, from the first event at which their
    /// realized schedules run different threads: before it they ran alike,
    /// so the window lemma holds there, and the traces record what ran. That
    /// takes `target_met`, the target replay still meeting the target: two
    /// runs that both pass would otherwise yield a confident pair, so
    /// without it a replay that left its schedule leaves the section
    /// unavailable.
    ///
    /// Candidates are checked by intervention, a control replay and an
    /// edited one each for at most [`CHECKED_CANDIDATES`]: `check_run` builds
    /// a replay that follows the first `replay` events of a schedule of the
    /// `role` run and then continues as the search's trials did. It gets the
    /// run's name, `role`, the schedule, how many of its first events are as
    /// that run realized them (whose callback checkpoints still apply), and
    /// `replay`.
    pub(super) fn racing_accesses(
        &self,
        decision: usize,
        target: TracedReplay,
        baseline: TracedReplay,
        target_met: bool,
        stack_run: impl FnOnce(&str) -> anyhow::Result<RunData>,
        mut check_run: impl FnMut(&str, &str, &[SchedEvent], usize, usize) -> anyhow::Result<RunData>,
    ) -> RacingAccesses {
        let mut decision = decision as EventIndex;
        let realized = if target_met {
            realized_window(decision, &target, &baseline)
        } else {
            None
        };
        if let Some((split, _)) = &realized {
            decision = *split;
        }
        let as_ran = realized.is_some();
        let traces = [&target, &baseline].map(|replay| self.read_trace(decision, replay, as_ran));
        let [target_trace, baseline_trace] = match traces {
            [Ok(target), Ok(baseline)] => [target, baseline],
            [Err(reason), _] | [_, Err(reason)] => {
                return RacingAccesses::unavailable(decision, reason);
            }
        };
        // A replay that left its schedule before its trace begins left too
        // early to compare the two as they ran: no --access-trace-from of the
        // final replays reaches back there.
        if as_ran {
            let first = target_trace
                .trace
                .header
                .from
                .max(baseline_trace.trace.header.from);
            if decision < first {
                let role = [&target, &baseline]
                    .into_iter()
                    .find(|replay| replay.departure.is_some())
                    .map_or("target", |replay| replay.role);
                return RacingAccesses::unavailable(
                    decision,
                    format!(
                        "the {role} replay left its schedule too early: the two replays first \
                         run different threads at event {decision}, before their traces begin \
                         (event {first})"
                    ),
                );
            }
        }
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
        let Outcome {
            point: outcome,
            unknown: unknown_outcome,
        } = outcome(&target_trace.trace, decision, &criteria);
        let uncounted_signal =
            uncounted_signal_outcome(&target_trace.trace, decision, &criteria, outcome.as_ref());
        let outcome_thread = outcome.as_ref().map(|o| o.thread);
        // The thread the target replay ran at the deciding event, as it
        // realized its schedule (which follows the requested one up to there).
        let thread = usize::try_from(decision)
            .ok()
            .and_then(|at| target.realized.get(at))
            .map(|event| event.dettid);
        let mut result = find_racing_accesses(
            &target_trace.trace,
            &baseline_trace.trace,
            Window {
                decision,
                outcome,
                thread,
            },
            outcome_thread,
        );
        result.notes.extend(unknown_outcome);
        for (role, read) in [("target", &target_trace), ("baseline", &baseline_trace)] {
            if read.capped {
                result.notes.push(format!(
                    "--race-window={} kept that many records of the {role} trace after the \
                     deciding event; raise it to read further.",
                    self.race_window
                ));
            }
        }
        result.notes.extend(uncounted_signal);
        if let Some((_, note)) = realized {
            result.notes.insert(0, note);
        }
        result.trace_files = vec![target_trace.file.clone(), baseline_trace.file.clone()];
        if result.status == RaceStatus::Candidates {
            let mut replay =
                |name: &str, role: &str, schedule: &[SchedEvent], reversal: &Reversal| {
                    check_replay(name, role, schedule, reversal, &mut check_run)
                };
            for (i, candidate) in result
                .candidates
                .iter_mut()
                .take(CHECKED_CANDIDATES)
                .enumerate()
            {
                candidate.check = Some(check_candidate(
                    i + 1,
                    candidate,
                    [&target, &baseline],
                    &mut replay,
                ));
            }
            promote_the_deciding_candidate(&mut result);
        }
        if matches!(
            result.status,
            RaceStatus::Found
                | RaceStatus::Timing
                | RaceStatus::Candidates
                | RaceStatus::FoundByIntervention
        ) {
            self.attach_stacks(&mut result, &target_trace.trace, stack_run);
        }
        result
    }

    /// Read one side's trace, or say why the section cannot use it. With
    /// `as_ran`, the replays are compared as they ran, so leaving the
    /// schedule does not disqualify one.
    fn read_trace(
        &self,
        decision: EventIndex,
        replay: &TracedReplay,
        as_ran: bool,
    ) -> Result<ReadTrace, String> {
        let role = replay.role;
        if let Some(error) = &replay.schedule_error {
            return Err(format!(
                "the {role} replay's realized schedule {} cannot be read ({error}), so whether \
                 it followed its schedule is unknown",
                replay.realized_path.display()
            ));
        }
        if let Some(departure) = replay.departure.as_ref().filter(|_| !as_ran) {
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
        // The reported pair's accesses, or the first candidate's: one replay
        // takes both. The other candidates rank lower and keep no stacks, so
        // a report pays for one replay whatever the status.
        let RacingAccesses {
            accesses,
            candidates,
            notes,
            ..
        } = result;
        let pair: &mut [RacingAccess] = match candidates.first_mut() {
            Some(first) if accesses.is_empty() => &mut first.accesses,
            _ => accesses.as_mut_slice(),
        };
        let entries: Vec<Option<EventIndex>> = pair
            .iter()
            .map(|access| entry_event(target, access.thread, access.nth))
            .collect();
        match self.replay_for_access_stacks(&entries, stack_run) {
            Ok(stacks) => {
                for ((access, entry), stack) in pair.iter_mut().zip(&entries).zip(stacks) {
                    if access.ended_first_by.is_some() {
                        notes.push(format!(
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
                        Some(_) => notes.push(format!(
                            "The stack replay ran another thread than thread {} at event {}, so \
                             that access's stack is left out.",
                            access.thread,
                            entry.unwrap_or_default()
                        )),
                        None => notes.push(format!(
                            "The stack replay took no stack for thread {}'s access.",
                            access.thread
                        )),
                    }
                }
            }
            Err(error) => notes.push(format!(
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

/// How a check's replay ended, and where it left the schedule it followed
/// (or why its recorded schedule gives none).
type CheckOutcome = (anyhow::Result<TrialEnd>, Result<Option<Departure>, String>);

/// How a check's replay that completed ended. One that did not complete has
/// no outcome: its launch's error leaves the check unchecked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrialEnd {
    /// It met the target criteria.
    Met,
    /// It did not.
    Missed,
}

/// Check `candidate` (the `index`th) by intervention: replay one of the
/// two runs ([`edit_for`]) with the candidate's turn order reversed
/// ([`reverse_turns`], which says what it moved), then continue as the
/// search's trials did. A control replay of the same run without the
/// reversal, to the same point and continued the same way, comes first:
/// unless it ends as the run did, a changed outcome could come from where
/// the replay stops following the schedule rather than from the
/// reversal. Where a replay left its schedule is read before its outcome.
fn check_candidate(
    index: usize,
    candidate: &RaceCandidate,
    [target, baseline]: [&TracedReplay; 2],
    replay: &mut impl FnMut(&str, &str, &[SchedEvent], &Reversal) -> CheckOutcome,
) -> CandidateCheck {
    let unchecked =
        |edited: &str, edit: String, trial: Option<String>, why: String| CandidateCheck {
            verdict: CheckVerdict::NotChecked,
            edited: edited.to_string(),
            edit,
            moved: None,
            trial,
            detail: format!("was not checked: {why}"),
        };
    let Some(edit) = edit_for(candidate) else {
        return unchecked(
            "target",
            "its two calls".to_string(),
            None,
            "one of them has no event in the target, and the baseline does not run them the \
             other way round"
                .to_string(),
        );
    };
    let schedule = if edit.met_decides {
        &baseline.realized
    } else {
        &target.realized
    };
    let reversal = match reverse_turns(schedule, edit.earlier, edit.later) {
        Ok(reversal) => reversal,
        Err(why) => {
            return unchecked(
                edit.role,
                format!("events #{} and #{}", edit.later, edit.earlier),
                None,
                why,
            );
        }
    };
    let trial = format!("racing_candidate_{index}");
    let control = format!("{trial}_control");
    eprintln!(
        ":: Checking race candidate {index}: reversing {} in the {} run, after a control \
         replay without the reversal",
        reversal.edit, edit.role
    );
    let (ended, left) = replay(&control, edit.role, schedule, &reversal);
    if let Err(why) = control_reproduces(ended, left, edit.met_decides) {
        return CandidateCheck {
            moved: Some(reversal.moved),
            ..unchecked(edit.role, reversal.edit, Some(control), why)
        };
    }
    let (ended, left) = replay(&trial, edit.role, &reversal.schedule, &reversal);
    let (verdict, detail) = verdict_of(ended, left, edit.met_decides);
    CandidateCheck {
        verdict,
        edited: edit.role.to_string(),
        edit: reversal.edit,
        moved: Some(reversal.moved),
        trial: Some(trial),
        detail,
    }
}

/// At most this many candidates are checked by intervention, each with a
/// control replay and an edited one.
pub const CHECKED_CANDIDATES: usize = 3;

/// Which run a candidate's check edits, and the two events it reverses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Edit {
    role: &'static str,
    /// The event that runs first in the edited run, and the one after it.
    earlier: usize,
    later: usize,
    /// Whether the reversal decides when the replay meets the target (an
    /// edited baseline, which misses it) rather than when it misses it (an
    /// edited target).
    met_decides: bool,
}

/// The edit that reverses `candidate`'s order: in the target, where its
/// first access runs before its second; or, when an access has no event in
/// the target (a wait still pending at the outcome), in the baseline, where
/// they run the other way round. `None` when neither run holds both.
fn edit_for(candidate: &RaceCandidate) -> Option<Edit> {
    let [first, second] = &candidate.accesses;
    match (
        first.event,
        second.event,
        first.baseline_event,
        second.baseline_event,
    ) {
        (Some(a), Some(b), ..) if a < b => Some(Edit {
            role: "target",
            earlier: usize::try_from(a).ok()?,
            later: usize::try_from(b).ok()?,
            met_decides: false,
        }),
        (_, _, Some(a), Some(b)) if b < a => Some(Edit {
            role: "baseline",
            earlier: usize::try_from(b).ok()?,
            later: usize::try_from(a).ok()?,
            met_decides: true,
        }),
        _ => None,
    }
}

/// A schedule with one candidate's turn order reversed.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Reversal {
    /// The edited schedule.
    pub schedule: Vec<SchedEvent>,
    /// How many of its first events are as the run realized them: the
    /// edited span starts there.
    pub unchanged: usize,
    /// How many of its events a replay follows: through the end of the
    /// edited span, after which it continues without them.
    pub replay: usize,
    /// `moving 7 events of thread 53 before thread 21's turn at #39925,
    /// crossing thread 3 (2 events)`.
    pub edit: String,
    /// The same, for the report's JSON.
    pub moved: MovedEvents,
}

/// The run of consecutive events of one thread in `schedule` that holds
/// `index`: that thread's turn.
fn turn_at(schedule: &[SchedEvent], index: usize) -> std::ops::Range<usize> {
    let thread = schedule[index].dettid;
    let start = schedule[..index]
        .iter()
        .rposition(|event| event.dettid != thread)
        .map_or(0, |i| i + 1);
    let end = schedule[index..]
        .iter()
        .position(|event| event.dettid != thread)
        .map_or(schedule.len(), |i| index + i);
    start..end
}

/// Whether `event` is the return of a timed sleep, which cannot come before
/// its deadline.
fn is_timed_return(event: &SchedEvent) -> bool {
    matches!(
        event.op,
        Op::Syscall(
            Sysno::nanosleep | Sysno::clock_nanosleep,
            SyscallPhase::Posthook
        )
    )
}

/// `schedule` with the turn holding event `later` (thread Y's) run before
/// the turn holding event `earlier` (thread X's). Over the span from the
/// start of X's turn to the end of Y's, Y's events move to its front; when
/// one of them is a timed sleep's return, which cannot run before its
/// deadline, X's events move to its back instead. That does not always
/// help: holding X back pulls the other threads' events earlier, Y's sleep
/// return among them, which can then still land before its deadline; such
/// a replay leaves the schedule there, and the check is not_realized,
/// naming the timed wait ([`not_realized`]). Each thread keeps the order of
/// its own events, and the moved events cross every event of the other
/// threads in the span: the result says how many moved and which threads,
/// besides the pair, they crossed. An error when the two events are on one
/// thread or not in that order.
pub(super) fn reverse_turns(
    schedule: &[SchedEvent],
    earlier: usize,
    later: usize,
) -> Result<Reversal, String> {
    if earlier >= later || later >= schedule.len() {
        return Err(format!(
            "the realized schedule does not hold events #{earlier} and #{later} in that order"
        ));
    }
    let (x, y) = (schedule[earlier].dettid, schedule[later].dettid);
    if x == y {
        return Err("the two calls are on one thread".to_string());
    }
    let span = turn_at(schedule, earlier).start..turn_at(schedule, later).end;
    let events = &schedule[span.clone()];
    let advance = !events.iter().any(|e| e.dettid == y && is_timed_return(e));
    let (mover, other, past, direction) = if advance {
        (y, x, earlier, "before")
    } else {
        (x, y, later, "after")
    };
    let moved: Vec<SchedEvent> = events
        .iter()
        .filter(|e| e.dettid == mover)
        .cloned()
        .collect();
    let rest: Vec<SchedEvent> = events
        .iter()
        .filter(|e| e.dettid != mover)
        .cloned()
        .collect();
    let mut crossed: Vec<CrossedThread> = Vec::new();
    for event in rest.iter().filter(|e| e.dettid != other) {
        match crossed.iter_mut().find(|c| c.thread == event.dettid) {
            Some(c) => c.events += 1,
            None => crossed.push(CrossedThread {
                thread: event.dettid,
                events: 1,
            }),
        }
    }
    let mut edited = schedule[..span.start].to_vec();
    if advance {
        edited.extend(moved.iter().cloned());
        edited.extend(rest);
    } else {
        edited.extend(rest);
        edited.extend(moved.iter().cloned());
    }
    edited.extend_from_slice(&schedule[span.end..]);
    let count = |n: usize| format!("{n} event{}", if n == 1 { "" } else { "s" });
    let crossing = if crossed.is_empty() {
        "crossing no other thread".to_string()
    } else {
        format!(
            "crossing {}",
            crossed
                .iter()
                .map(|c| format!("thread {} ({})", c.thread, count(c.events)))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Ok(Reversal {
        edit: format!(
            "moving {} of thread {mover} {direction} thread {other}'s turn at #{past}, {crossing}",
            count(moved.len())
        ),
        moved: MovedEvents {
            thread: mover,
            events: moved.len(),
            direction: direction.to_string(),
            past_event: past as EventIndex,
            crossed,
        },
        schedule: edited,
        unchanged: span.start,
        replay: span.end,
    })
}

/// Replay the first `reversal.replay` events of `schedule` (edited, or not
/// for a control) of the `role` run with `check_run`, then continue as the
/// search's trials did: how it ended, and where it left `schedule`, read
/// from the schedule it recorded.
fn check_replay(
    runname: &str,
    role: &str,
    schedule: &[SchedEvent],
    reversal: &Reversal,
    check_run: &mut impl FnMut(&str, &str, &[SchedEvent], usize, usize) -> anyhow::Result<RunData>,
) -> CheckOutcome {
    let mut run = match check_run(runname, role, schedule, reversal.unchanged, reversal.replay) {
        Ok(run) => run.with_schedule_recording(),
        Err(error) => {
            return (
                Err(error.context("its replay could not be set up")),
                Ok(None),
            );
        }
    };
    let path = run.sched_path_out().to_path_buf();
    // Only a replay that completed has an outcome; one that did not is an
    // error, which leaves the check unchecked.
    let ended = run.launch().map(|()| {
        if run.is_a_match() {
            TrialEnd::Met
        } else {
            TrialEnd::Missed
        }
    });
    let left =
        realized_schedule(&path).map(|realized| departure(schedule, &realized, reversal.replay));
    (ended, left)
}

/// The schedule a check's replay recorded at `path`, or why it gives none:
/// absent, unreadable (a replay cut short leaves a partial one) or empty.
fn realized_schedule(path: &Path) -> Result<Vec<SchedEvent>, String> {
    if !path.exists() {
        return Err("its replay recorded no schedule".to_string());
    }
    let realized = Recording::read(path)
        .and_then(|recording| recording.into_legacy("a race candidate's check"))
        .map(PreemptionRecord::into_global)
        .map_err(|error| format!("the schedule its replay recorded cannot be read ({error:#})"))?;
    if realized.is_empty() {
        return Err("the schedule its replay recorded is empty".to_string());
    }
    Ok(realized)
}

/// Whether a check's control replay (the run's schedule without the
/// reversal, replayed to the same point and continued the same way) ended
/// as the run did: met the target for an edited target, missed it for an
/// edited baseline (`met_decides`). Otherwise why the check cannot count.
fn control_reproduces(
    ended: anyhow::Result<TrialEnd>,
    left: Result<Option<Departure>, String>,
    met_decides: bool,
) -> Result<(), String> {
    let ended = ended.map_err(|error| format!("its control replay failed ({error:#})"))?;
    match left {
        Err(why) => return Err(format!("its control replay: {why}")),
        Ok(Some(left)) => {
            return Err(format!(
                "its control replay, without the reversal, left the schedule: the replay {left}"
            ));
        }
        Ok(None) => {}
    }
    match (ended, met_decides) {
        (TrialEnd::Met, false) | (TrialEnd::Missed, true) => Ok(()),
        (TrialEnd::Met, true) => Err(
            "the continuation alone changes the outcome: replayed to the same point without the \
             reversal and continued, the baseline run meets the target"
                .to_string(),
        ),
        (TrialEnd::Missed, false) => Err(
            "the continuation alone changes the outcome: replayed to the same point without the \
             reversal and continued, the target run misses the target"
                .to_string(),
        ),
    }
}

/// What a check's replay showed: `left`, where it left the edited schedule,
/// is read before its outcome; then the outcome decides when it differs
/// from the edited run's (met for an edited baseline, missed for an edited
/// target, as `met_decides` says).
fn verdict_of(
    ended: anyhow::Result<TrialEnd>,
    left: Result<Option<Departure>, String>,
    met_decides: bool,
) -> (CheckVerdict, String) {
    let ended = match ended {
        Ok(ended) => ended,
        Err(error) => {
            return (
                CheckVerdict::NotChecked,
                format!("was not checked: its replay failed ({error:#})"),
            );
        }
    };
    match left {
        Err(why) => return (CheckVerdict::NotChecked, format!("was not checked: {why}")),
        Ok(Some(left)) => {
            return (CheckVerdict::NotRealized, not_realized(&left));
        }
        Ok(None) => {}
    }
    match (ended, met_decides) {
        (TrialEnd::Met, true) => (
            CheckVerdict::Decides,
            "meets the target; the unedited control misses it".to_string(),
        ),
        (TrialEnd::Missed, false) => (
            CheckVerdict::Decides,
            "misses the target; the unedited control meets it".to_string(),
        ),
        (TrialEnd::Met, false) => (
            CheckVerdict::DoesNotDecide,
            "still meets the target, as the unedited control does".to_string(),
        ),
        (TrialEnd::Missed, true) => (
            CheckVerdict::DoesNotDecide,
            "still misses the target, as the unedited control does".to_string(),
        ),
    }
}

/// Why a check's replay shows nothing: where it left the edited schedule.
/// Reaching a timed sleep's return early is named, since no reversal that
/// runs the return sooner can be replayed: the sleep has not expired there.
fn not_realized(left: &Departure) -> String {
    match left {
        Departure::OtherEvent { expected, .. } if is_timed_return(expected) => format!(
            "cannot be replayed as edited: the edit moves thread {}'s timed wait return before \
             its deadline (the replay {left})",
            expected.dettid
        ),
        _ => format!("cannot be replayed as edited: the replay {left}"),
    }
}

/// With exactly one candidate whose check decides, report it as found by
/// intervention: its accesses become the pair, and the candidates stay
/// listed with their checks.
fn promote_the_deciding_candidate(result: &mut RacingAccesses) {
    let deciding: Vec<usize> = result
        .candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.check
                .as_ref()
                .is_some_and(|check| check.verdict == CheckVerdict::Decides)
        })
        .map(|(i, _)| i)
        .collect();
    let [index] = deciding.as_slice() else {
        return;
    };
    let candidate = &result.candidates[*index];
    let Some(check) = &candidate.check else {
        return;
    };
    result.reason = format!(
        "reversing this turn order ({}) in the {} run {} ({}.repro in the workspace), and no \
         other checked candidate's reversal changes the outcome",
        check.edit,
        check.edited,
        check.detail,
        check.trial.as_deref().unwrap_or("the check")
    );
    result.status = RaceStatus::FoundByIntervention;
    result.resource = candidate.resource.clone();
    result.accesses = candidate.accesses.to_vec();
    result.order = Some(candidate.order.clone());
}

/// Where to compare `target` and `baseline` as they ran, when one of them
/// left its schedule before `decision`: the first event at which their
/// realized schedules run different threads (or the shorter one ends), with
/// the note that says so. `None` when both followed their schedules, or a
/// realized schedule is missing or unreadable.
fn realized_window(
    decision: EventIndex,
    target: &TracedReplay,
    baseline: &TracedReplay,
) -> Option<(EventIndex, String)> {
    let (role, departure) = [target, baseline]
        .into_iter()
        .find_map(|replay| Some((replay.role, replay.departure.as_ref()?)))?;
    if [target, baseline]
        .iter()
        .any(|replay| replay.schedule_error.is_some() || replay.realized.is_empty())
    {
        return None;
    }
    let split = first_thread_difference(&target.realized, &baseline.realized)? as EventIndex;
    Some((
        split,
        format!(
            "The {role} replay left its schedule at or before the deciding event {decision} (it \
             {departure}), so the two replays are compared as they ran, from event {split}, the \
             first at which they run different threads; event numbers here are theirs."
        ),
    ))
}

/// The first index at which `a` and `b` run different threads, or at which
/// the shorter one ends; `None` when they run the same threads throughout.
fn first_thread_difference(a: &[SchedEvent], b: &[SchedEvent]) -> Option<usize> {
    a.iter()
        .zip(b)
        .position(|(x, y)| x.dettid != y.dettid)
        .or_else(|| (a.len() != b.len()).then(|| a.len().min(b.len())))
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
        TraceRecord::WatchSummary(_) => return false,
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
    use detcore_model::pid::DetTid;

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
        let reason = options.read_trace(0, &replay, false).err().unwrap();
        assert!(
            reason.starts_with("the target replay's realized schedule")
                && reason.contains("cannot be read"),
            "{reason}"
        );
    }

    #[test]
    fn the_first_thread_difference_is_where_realized_schedules_part() {
        let events = |tids: &[i32]| -> Vec<SchedEvent> {
            tids.iter()
                .map(|&t| SchedEvent::branches(detcore::types::DetTid::from_raw(t), 10))
                .collect()
        };
        assert_eq!(
            first_thread_difference(&events(&[3, 3, 5, 7]), &events(&[3, 3, 5, 9])),
            Some(3)
        );
        assert_eq!(
            first_thread_difference(&events(&[3, 5]), &events(&[3, 5, 7])),
            Some(2)
        );
        assert_eq!(
            first_thread_difference(&events(&[3, 5]), &events(&[3, 5])),
            None
        );
    }

    /// A target replay that left its schedule before the deciding event is
    /// compared as it ran, from the first event at which the two replays
    /// run different threads, with a note; with no realized schedule to go
    /// by, the section stays unavailable.
    #[test]
    fn a_replay_that_left_its_schedule_is_compared_as_it_ran() {
        let dir = tempfile::tempdir().unwrap();
        let tid = detcore::types::DetTid::from_raw;
        let events = |tids: &[i32]| -> Vec<SchedEvent> {
            tids.iter()
                .map(|&t| SchedEvent::branches(tid(t), 10))
                .collect()
        };
        // One call each, entered before event 3 and alike in both runs, and
        // a call after it whose result differs.
        let write_trace_from = |name: &str, result: i64, from: u64| {
            let path = dir.path().join(name);
            let lines = [
                format!(
                    r#"{{"kind":"header","format":"hermit-access-trace","version":2,"from":{from}}}"#
                ),
                r#"{"kind":"syscall","pre":1,"post":2,"tid":3,"pid":3,"nth":1,"sysno":"getpid","args":[0,0,0,0,0,0],"result":3}"#
                    .to_string(),
                format!(
                    r#"{{"kind":"syscall","pre":4,"post":5,"tid":5,"pid":3,"nth":1,"sysno":"getppid","args":[0,0,0,0,0,0],"result":{result}}}"#
                ),
                r#"{"kind":"end","events":6,"records":2}"#.to_string(),
            ];
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            path
        };
        let write_trace = |name: &str, result: i64| write_trace_from(name, result, 0);
        let replay = |role, trace, departure, realized| TracedReplay {
            role,
            trace: Ok(trace),
            departure,
            realized,
            realized_path: dir.path().join(format!("{role}_out.preempts")),
            schedule_error: None,
        };
        let left = Departure::OtherEvent {
            index: 1,
            expected: SchedEvent::branches(tid(3), 10),
            ran: SchedEvent::branches(tid(3), 12),
        };
        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let no_stacks = |_: &str| -> anyhow::Result<RunData> { anyhow::bail!("no stacks here") };
        let no_checks =
            |_: &str, _: &str, _: &[SchedEvent], _: usize, _: usize| -> anyhow::Result<RunData> {
                anyhow::bail!("no checks here")
            };
        let r = options.racing_accesses(
            1,
            replay(
                "target",
                write_trace("target.access", 1),
                Some(left.clone()),
                events(&[3, 3, 3, 5, 5]),
            ),
            replay(
                "baseline",
                write_trace("baseline.access", 2),
                None,
                events(&[3, 3, 3, 7, 5]),
            ),
            true,
            no_stacks,
            no_checks,
        );
        assert_ne!(r.status, RaceStatus::Unavailable, "{}", r.reason);
        assert_eq!(r.decision, 3);
        assert!(
            r.notes[0].starts_with(
                "The target replay left its schedule at or before the deciding event 1 (it ran"
            ) && r.notes[0].contains("compared as they ran, from event 3"),
            "{:?}",
            r.notes
        );

        // Without the realized schedules there is nothing to compare by.
        let r = options.racing_accesses(
            1,
            replay(
                "target",
                write_trace("target.access", 1),
                Some(left.clone()),
                Vec::new(),
            ),
            replay(
                "baseline",
                write_trace("baseline.access", 2),
                None,
                Vec::new(),
            ),
            true,
            no_stacks,
            no_checks,
        );
        assert_eq!(r.status, RaceStatus::Unavailable);
        assert!(
            r.reason.starts_with("the target replay left its schedule"),
            "{}",
            r.reason
        );

        // A target replay that left its schedule and no longer met the
        // target is not compared as it ran: two passing runs would give a
        // confident pair.
        let r = options.racing_accesses(
            1,
            replay(
                "target",
                write_trace("target.access", 1),
                Some(left.clone()),
                events(&[3, 3, 3, 5, 5]),
            ),
            replay(
                "baseline",
                write_trace("baseline.access", 2),
                None,
                events(&[3, 3, 3, 7, 5]),
            ),
            false,
            no_stacks,
            no_checks,
        );
        assert_eq!(r.status, RaceStatus::Unavailable);
        assert!(
            r.reason.starts_with("the target replay left its schedule"),
            "{}",
            r.reason
        );

        // Traces that begin after the first thread difference: the replay
        // left its schedule too early, not a matter of --access-trace-from.
        let r = options.racing_accesses(
            1,
            replay(
                "target",
                write_trace_from("target_late.access", 1, 4),
                Some(left.clone()),
                events(&[3, 3, 3, 5, 5]),
            ),
            replay(
                "baseline",
                write_trace_from("baseline_late.access", 2, 4),
                None,
                events(&[3, 3, 3, 7, 5]),
            ),
            true,
            no_stacks,
            no_checks,
        );
        assert_eq!(r.status, RaceStatus::Unavailable);
        assert_eq!(
            r.reason,
            "the target replay left its schedule too early: the two replays first run different \
             threads at event 3, before their traces begin (event 4)"
        );
    }

    /// The threads of `schedule`, one letter per event.
    fn threads(schedule: &[SchedEvent]) -> String {
        schedule
            .iter()
            .map(|e| char::from(b'a' + e.dettid.as_raw() as u8))
            .collect()
    }

    /// A schedule with one event per letter, thread `a` = 0.
    fn schedule_of(letters: &str) -> Vec<SchedEvent> {
        letters
            .bytes()
            .map(|b| SchedEvent::branches(DetTid::from_raw(i32::from(b - b'a')), 1))
            .collect()
    }

    #[test]
    fn a_reversal_moves_the_later_turn_and_says_what_it_crossed() {
        // y's turn moves before x's: from the start of x's turn to the end
        // of y's; the events before and after stay as they ran.
        let r = reverse_turns(&schedule_of("axxyyb"), 2, 3).unwrap();
        assert_eq!(threads(&r.schedule), "ayyxxb");
        assert_eq!((r.unchanged, r.replay), (1, 5));
        assert_eq!(
            r.edit,
            "moving 2 events of thread 24 before thread 23's turn at #2, crossing no other thread"
        );
        // A third thread between them is crossed, and said.
        let r = reverse_turns(&schedule_of("xzzwyy"), 0, 4).unwrap();
        assert_eq!(threads(&r.schedule), "yyxzzw");
        assert_eq!(
            r.edit,
            "moving 2 events of thread 24 before thread 23's turn at #0, crossing thread 25 (2 \
             events), thread 22 (1 event)"
        );
        assert_eq!(
            r.moved,
            MovedEvents {
                thread: DetTid::from_raw(24),
                events: 2,
                direction: "before".to_string(),
                past_event: 0,
                crossed: vec![
                    CrossedThread {
                        thread: DetTid::from_raw(25),
                        events: 2
                    },
                    CrossedThread {
                        thread: DetTid::from_raw(22),
                        events: 1
                    },
                ],
            }
        );
        // When y's turn holds a timed sleep's return, which cannot come
        // before its deadline, x's events are held back instead.
        let mut schedule = schedule_of("xzy");
        schedule[2].op = Op::Syscall(Sysno::nanosleep, SyscallPhase::Posthook);
        let r = reverse_turns(&schedule, 0, 2).unwrap();
        assert_eq!(threads(&r.schedule), "zyx");
        assert_eq!(
            r.edit,
            "moving 1 event of thread 23 after thread 24's turn at #2, crossing thread 25 (1 \
             event)"
        );
        // One thread, the wrong order, or past the schedule: refused.
        assert!(reverse_turns(&schedule_of("xxx"), 0, 2).is_err());
        assert!(reverse_turns(&schedule_of("xy"), 1, 0).is_err());
        assert!(reverse_turns(&schedule_of("xy"), 0, 5).is_err());
    }

    /// A candidate whose two accesses run at `target` and `baseline` events.
    fn candidate_at(target: [Option<u64>; 2], baseline: [Option<u64>; 2]) -> RaceCandidate {
        let access = |i: usize| RacingAccess {
            thread: DetTid::from_raw(3 + i as i32),
            nth: 1,
            at: crate::app::analyze::races::AccessPoint::Entry,
            event: target[i],
            baseline_event: baseline[i],
            call: "read(3, 0x1000, 8)".to_string(),
            result: "8".to_string(),
            baseline_result: Some("8".to_string()),
            baseline_differs_from: None,
            ended_first_by: None,
            stack: None,
        };
        RaceCandidate {
            resource: None,
            accesses: [access(0), access(1)],
            order: crate::app::analyze::races::RaceOrder {
                target: [0, 1],
                baseline: [1, 0],
            },
            reaches_outcome: true,
            events_before_outcome: None,
            evidence: "the evidence".to_string(),
            check: None,
        }
    }

    #[test]
    fn a_check_edits_the_target_or_else_the_baseline() {
        assert_eq!(
            edit_for(&candidate_at([Some(10), Some(20)], [Some(30), Some(25)])),
            Some(Edit {
                role: "target",
                earlier: 10,
                later: 20,
                met_decides: false,
            })
        );
        // A wait still pending at the outcome has no target event: the
        // baseline, where the two run the other way round, is edited.
        assert_eq!(
            edit_for(&candidate_at([Some(10), None], [Some(30), Some(25)])),
            Some(Edit {
                role: "baseline",
                earlier: 25,
                later: 30,
                met_decides: true,
            })
        );
        assert_eq!(
            edit_for(&candidate_at([Some(10), None], [Some(25), None])),
            None
        );
    }

    #[test]
    fn a_replay_that_leaves_the_edit_is_not_realized_whatever_its_outcome() {
        let left = Departure::OtherEvent {
            index: 4,
            expected: SchedEvent::branches(DetTid::from_raw(3), 1),
            ran: SchedEvent::branches(DetTid::from_raw(5), 1),
        };
        for ended in [TrialEnd::Met, TrialEnd::Missed] {
            let (verdict, detail) = verdict_of(Ok(ended), Ok(Some(left.clone())), false);
            assert_eq!(verdict, CheckVerdict::NotRealized);
            assert!(
                detail.starts_with("cannot be replayed as edited: the replay ran"),
                "{detail}"
            );
        }
        // Leaving it where a timed sleep was to return says so.
        let mut nap = SchedEvent::branches(DetTid::from_raw(7), 1);
        nap.op = Op::Syscall(Sysno::clock_nanosleep, SyscallPhase::Posthook);
        let early = Departure::OtherEvent {
            index: 4,
            expected: nap,
            ran: SchedEvent::branches(DetTid::from_raw(5), 1),
        };
        let (verdict, detail) = verdict_of(Ok(TrialEnd::Met), Ok(Some(early)), false);
        assert_eq!(verdict, CheckVerdict::NotRealized);
        assert!(
            detail.starts_with(
                "cannot be replayed as edited: the edit moves thread 7's timed wait return \
                 before its deadline (the replay ran"
            ),
            "{detail}"
        );
        assert_eq!(
            verdict_of(Ok(TrialEnd::Missed), Ok(None), false),
            (
                CheckVerdict::Decides,
                "misses the target; the unedited control meets it".to_string()
            )
        );
        assert_eq!(
            verdict_of(Ok(TrialEnd::Met), Ok(None), false).0,
            CheckVerdict::DoesNotDecide
        );
        assert_eq!(
            verdict_of(Ok(TrialEnd::Met), Ok(None), true),
            (
                CheckVerdict::Decides,
                "meets the target; the unedited control misses it".to_string()
            )
        );
        assert_eq!(
            verdict_of(Ok(TrialEnd::Missed), Ok(None), true).0,
            CheckVerdict::DoesNotDecide
        );
        assert_eq!(
            verdict_of(Err(anyhow::anyhow!("refused")), Ok(None), false).0,
            CheckVerdict::NotChecked
        );
        // A schedule that cannot be read decides nothing.
        assert_eq!(
            verdict_of(Ok(TrialEnd::Missed), Err("unreadable".to_string()), false),
            (
                CheckVerdict::NotChecked,
                "was not checked: unreadable".to_string()
            )
        );
    }

    #[test]
    fn exactly_one_deciding_candidate_is_found_by_intervention() {
        let checked = |verdict| {
            let mut c = candidate_at([Some(10), Some(20)], [Some(30), Some(25)]);
            c.check = Some(CandidateCheck {
                verdict,
                edited: "target".to_string(),
                edit: "moving 1 event of thread 4 before thread 3's turn at #10, crossing no \
                       other thread"
                    .to_string(),
                moved: None,
                trial: Some("racing_candidate_1".to_string()),
                detail: "misses the target; the unedited control meets it".to_string(),
            });
            c
        };
        let mut r = RacingAccesses::unavailable(5, "x".to_string());
        r.status = RaceStatus::Candidates;
        r.candidates = vec![
            checked(CheckVerdict::NotRealized),
            checked(CheckVerdict::Decides),
            checked(CheckVerdict::DoesNotDecide),
        ];
        let mut one = r.clone();
        promote_the_deciding_candidate(&mut one);
        assert_eq!(one.status, RaceStatus::FoundByIntervention);
        assert_eq!(one.accesses, r.candidates[1].accesses.to_vec());
        assert_eq!(
            one.reason,
            "reversing this turn order (moving 1 event of thread 4 before thread 3's turn at #10, \
             crossing no other thread) in the target run misses the target; the unedited control \
             meets it (racing_candidate_1.repro in the workspace), and no other checked \
             candidate's reversal changes the outcome"
        );
        let text = crate::app::analyze::races::render_racing_accesses(&one);
        assert!(
            text.starts_with("Racing accesses, found by intervention, not as an order the two runs show: reversing")
                && text.contains("Candidates checked:\n  1. the evidence; not shown to decide.")
                && text.contains("  2. the evidence.\n")
                && text.contains("   Check: decides: reversing this turn order (moving 1 event of thread 4 before thread 3's turn at #10, crossing no other thread) in the target run misses the target; the unedited control meets it (racing_candidate_1.repro)."),
            "{text}"
        );

        // Two that decide: still candidates.
        let mut two = r.clone();
        two.candidates[2] = checked(CheckVerdict::Decides);
        promote_the_deciding_candidate(&mut two);
        assert_eq!(two.status, RaceStatus::Candidates);
        assert!(two.accesses.is_empty());
    }

    #[test]
    fn a_check_counts_only_when_its_control_ends_as_the_run_did() {
        // An edited target must meet the target without the reversal, an
        // edited baseline miss it.
        assert_eq!(
            control_reproduces(Ok(TrialEnd::Met), Ok(None), false),
            Ok(())
        );
        assert_eq!(
            control_reproduces(Ok(TrialEnd::Missed), Ok(None), true),
            Ok(())
        );
        assert_eq!(
            control_reproduces(Ok(TrialEnd::Missed), Ok(None), false),
            Err(
                "the continuation alone changes the outcome: replayed to the same point without \
                 the reversal and continued, the target run misses the target"
                    .to_string()
            )
        );
        assert!(
            control_reproduces(Ok(TrialEnd::Met), Ok(None), true)
                .unwrap_err()
                .starts_with("the continuation alone changes the outcome")
        );
        let left = Departure::Ended { events: 3 };
        assert!(
            control_reproduces(Ok(TrialEnd::Met), Ok(Some(left)), false)
                .unwrap_err()
                .starts_with("its control replay, without the reversal, left the schedule")
        );
        assert!(control_reproduces(Ok(TrialEnd::Met), Err("x".to_string()), false).is_err());
        assert!(control_reproduces(Err(anyhow::anyhow!("refused")), Ok(None), false).is_err());
    }

    #[test]
    fn a_missing_empty_or_partial_realized_schedule_gives_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            realized_schedule(&dir.path().join("absent.preempts")),
            Err("its replay recorded no schedule".to_string())
        );
        let empty = dir.path().join("empty.preempts");
        std::fs::write(&empty, "").unwrap();
        assert!(realized_schedule(&empty).is_err());
        let no_events = dir.path().join("no_events.preempts");
        PreemptionRecord::from_sched_events(Vec::new())
            .write_to_disk(&no_events)
            .unwrap();
        assert_eq!(
            realized_schedule(&no_events),
            Err("the schedule its replay recorded is empty".to_string())
        );
        let partial = dir.path().join("partial.preempts");
        std::fs::write(&partial, "{\"version\": 1, \"events\": [").unwrap();
        assert!(realized_schedule(&partial).is_err());
    }

    /// A check replays the unedited schedule first, as a control, and the
    /// edited one only when the control ends as the run did.
    #[test]
    fn a_check_replays_its_control_first_and_counts_only_when_it_reproduces() {
        let dir = tempfile::tempdir().unwrap();
        let realized: Vec<SchedEvent> = [3, 3, 4, 4]
            .iter()
            .map(|&t| SchedEvent::branches(DetTid::from_raw(t), 1))
            .collect();
        let run = |role| TracedReplay {
            role,
            trace: Err("no trace".to_string()),
            departure: None,
            realized: realized.clone(),
            realized_path: dir.path().join(format!("{role}.preempts")),
            schedule_error: None,
        };
        let (target, baseline) = (run("target"), run("baseline"));
        let candidate = candidate_at([Some(1), Some(2)], [Some(2), Some(1)]);
        let tids = |schedule: &[SchedEvent]| -> Vec<i32> {
            schedule.iter().map(|e| e.dettid.as_raw()).collect()
        };
        let check = |control: TrialEnd, edited: TrialEnd| {
            let mut calls = Vec::new();
            let mut replay = |name: &str, _: &str, schedule: &[SchedEvent], _: &Reversal| {
                calls.push((name.to_string(), tids(schedule)));
                let end = if name.ends_with("_control") {
                    control
                } else {
                    edited
                };
                (Ok(end), Ok(None))
            };
            let result = check_candidate(1, &candidate, [&target, &baseline], &mut replay);
            (result, calls)
        };
        let (decides, calls) = check(TrialEnd::Met, TrialEnd::Missed);
        assert_eq!(decides.verdict, CheckVerdict::Decides);
        assert_eq!(decides.trial.as_deref(), Some("racing_candidate_1"));
        assert_eq!(
            calls,
            vec![
                ("racing_candidate_1_control".to_string(), vec![3, 3, 4, 4]),
                ("racing_candidate_1".to_string(), vec![4, 4, 3, 3]),
            ]
        );
        let (unchecked, calls) = check(TrialEnd::Missed, TrialEnd::Missed);
        assert_eq!(unchecked.verdict, CheckVerdict::NotChecked);
        assert!(
            unchecked
                .detail
                .starts_with("was not checked: the continuation alone changes the outcome"),
            "{}",
            unchecked.detail
        );
        assert_eq!(calls.len(), 1);
    }

    /// The racing-access stack replay is a trial launched through
    /// `RunData::launch` and `run_now`, whose refusals carry `run_error`'s
    /// diagnostics. A refusal its guest reached still takes the path of a
    /// replay that did not finish: the stacks it took are used, and the
    /// analysis goes on. A refusal decided before the guest started ends the
    /// analysis with its error, as before. Only `hermit run`'s result is
    /// replaced (`AnalyzeOpts::hermit_run`).
    #[test]
    fn a_refusal_the_stack_replay_reached_uses_the_stacks_it_took() {
        use std::sync::Arc;

        use reverie::process::ExitStatus;
        use reverie::process::RunError;

        use crate::app::analyze::rundata::HermitRun;
        use crate::app::analyze::rundata::RunResult;

        let workspace = tempfile::tempdir().unwrap();
        let entries: [Option<EventIndex>; 2] = [Some(7), Some(9)];
        for guest_started in [true, false] {
            let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
            options.tmp_dir = Some(
                workspace
                    .path()
                    .join(format!("guest_started_{guest_started}")),
            );
            std::fs::create_dir_all(options.tmp_dir.as_ref().unwrap()).unwrap();
            options.hermit_run = HermitRun(Some(Arc::new(move |run: &RunData| {
                // The replay took the first access's stack before Hermit
                // refused it.
                let taken = run.root_path().with_extension("access1.stack");
                std::fs::write(
                    &taken,
                    r#"{"thread_id": 3, "thread_name": null, "frames": []}"#,
                )
                .unwrap();
                Err(if guest_started {
                    crate::app::container::classify_container_result::<RunResult>(Err(
                        RunError::ExitStatus(ExitStatus::Exited(
                            detcore_model::HERMIT_POLICY_REFUSAL_EXIT,
                        )),
                    ))
                    .unwrap_err()
                } else {
                    anyhow::Error::new(crate::app::container::PolicyRefusal)
                        .context("unsupported configuration at admission")
                })
            })));
            let replayed = options.replay_for_access_stacks(&entries, |name| {
                RunData::new_baseline(&options, name.to_owned())
            });
            if guest_started {
                let stacks = replayed.unwrap();
                assert_eq!(stacks.len(), 2);
                assert!(stacks[0].is_some(), "{stacks:?}");
                assert!(stacks[1].is_none(), "{stacks:?}");
            } else {
                let error = replayed.unwrap_err();
                assert!(
                    error
                        .downcast_ref::<crate::app::container::PolicyRefusal>()
                        .is_some(),
                    "{error:#}"
                );
                assert!(
                    format!("{error:#}").contains("unsupported configuration at admission"),
                    "{error:#}"
                );
            }
        }
    }
}
