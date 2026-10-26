/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A mode for analyzing a hermit run.

use std::cmp;
use std::fs::File;
use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use anyhow::bail;
use colored::Colorize;
use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use detcore::preemptions::boundary_recording::Recording;
use detcore::types::DetTid;
use detcore::types::SchedEvent;
use detcore::util::truncated;
use detcore_model::config::MaybeTimeslice;
use hermit::Error;
use rand::RngExt as _;
use rand::SeedableRng;
use rand_pcg::Pcg64Mcg;
use regex::Regex;
use reverie::PrettyBacktrace;
use reverie::process::ExitStatus;
use reverie::process::Output;

use crate::app::analyze::consts::*;
use crate::app::analyze::race_report::TracedReplay;
use crate::app::analyze::races::RacingAccesses;
use crate::app::analyze::races::render_racing_accesses;
use crate::app::analyze::rundata::RunData;
use crate::app::analyze::stop;
use crate::app::analyze::stop::StopSignals;
use crate::app::analyze::stop::Stopped;
use crate::app::analyze::types::AnalyzeOpts;
use crate::app::analyze::types::ContinuationTimeslice;
use crate::app::analyze::types::ExitStatusConstraint;
use crate::app::analyze::types::Report;
use crate::app::analyze::types::ReportCriticalEvent;
use crate::app::analyze::workspace::Retention;
use crate::app::analyze::workspace::Workspace;
use crate::app::analyze::workspace::note;
use crate::app::container::RunTimeoutMarker;
use crate::app::global_opts::GlobalOpts;
use crate::app::logdiff::LogDiffCLIOpts;
use crate::app::run::RunOpts;
use crate::app::schedule_search::Config;
use crate::app::schedule_search::CriticalSchedule;
use crate::app::schedule_search::INEXACT_COUNTER_BRANCH_JITTER;
use crate::app::schedule_search::NoSingleSwap;
use crate::app::schedule_search::TrialOutcome;
use crate::app::schedule_search::search_for_critical_schedule;
use crate::app::analyze::race_report::TracedReplay;
use crate::app::analyze::races::RacingAccesses;
use crate::app::analyze::races::render_racing_accesses;
use crate::app::analyze::verdict::AnalyzeFailure;
use crate::app::analyze::verdict::AnalyzePrecondition;
use crate::app::analyze::verdict::AnalyzeVerdict;
use crate::app::analyze::verdict::PreconditionClass;
use crate::app::analyze::verdict::Verdict;

/// Compare only preemptions, not recorded schedules.
fn preempt_files_equal(path1: &Path, path2: &Path) -> bool {
    let mut pr1 = PreemptionReader::new(path1).load_all();
    let mut pr2 = PreemptionReader::new(path2).load_all();
    pr1.preemptions_only();
    pr2.preemptions_only();
    if matches!(pr1.guest_ids(), Some(detcore::GuestIds::Chosen { .. }))
        || matches!(pr2.guest_ids(), Some(detcore::GuestIds::Chosen { .. }))
    {
        if pr1.validate_combined_reserved_inputs(&pr2).is_err() {
            return false;
        }
    } else {
        // Before this field existed, non-Chosen comparisons ignored this
        // unknown JSON member. Keep that exact comparison domain: every old
        // field below still participates, while full data equality elsewhere
        // continues to distinguish opaque metadata for round-trip checks.
        pr2 = pr2.with_reserved_input(pr1.reserved_input().clone());
    }
    pr1 == pr2
}

fn yellow_msg(msg: &str) {
    eprintln!(":: {}", msg.yellow().bold());
}

fn prefix_report_banner(report: &Report) -> String {
    let mut banner =
        "\n------------------------------ hermit analyze report ------------------------------"
            .to_owned();
    if let Some(warning) = &report.branch_counter_warning {
        banner.push('\n');
        banner.push_str(warning);
    }
    banner
}

/// The report's sentence naming the two continuations' recorded schedules,
/// `with` and `without` the deciding event (file names in the workspace).
/// The report outlives a workspace `retention` removes (on stdout, in a
/// `--report-file`), so the sentence then says the files are gone and how to
/// keep them.
fn continuation_files_sentence(
    decision: usize,
    with: &str,
    without: &str,
    retention: Retention,
) -> String {
    let gone = match retention {
        Retention::Keep { .. } => "",
        Retention::Remove => {
            " Analyze removes its workspace when it finishes, so run it again with --keep-tmp \
             to keep these files."
        }
    };
    format!(
        "Event numbers after #{decision} are positions in each continuation's recorded \
         schedule, in the workspace: {with} with the event, {without} without it.{gone}\n"
    )
}

/// What stack1 and stack2 of a `--prefix-search` report are, given the
/// thread the target schedule runs at the deciding event (`chosen`) and the
/// one the scheduler runs there without that event (`instead`, `None` when
/// the run without it ended first). Two threads are a pair whose order
/// decides the outcome. One thread is not a pair at all: the scheduler runs
/// it there either way, so the report says so instead of calling the thread
/// a race with itself.
fn stack_pair_sentences(
    chosen: detcore::types::DetTid,
    instead: Option<detcore::types::DetTid>,
) -> String {
    if instead == Some(chosen) {
        format!(
            "stack1 and stack2 both belong to thread {chosen}: the scheduler runs it at that \
             point with or without the event, so they are not two threads racing.\nWhat the \
             event can change is how thread {chosen} runs from there, not which thread runs: look \
             at what it does before another thread runs, and at that thread.\n"
        )
    } else {
        "stack1 is the thread the target schedule runs at that point; stack2 is the thread that \
         runs there instead when the scheduler continues without chaos.\nThe outcome depends on \
         which of the two runs first: add synchronization that makes the order irrelevant, or \
         that enforces the passing order.\n"
            .to_owned()
    }
}

/// `--minimize`'s knock-outs removed every intervention and the replay still
/// meets the target, so there is no baseline schedule to bisect against.
#[derive(Debug)]
pub struct NothingToKnockOut;

impl std::fmt::Display for NothingToKnockOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "--minimize knocked out every intervention and the run still meets the target, so \
             there is no baseline schedule to bisect against"
        )
    }
}

impl std::error::Error for NothingToKnockOut {}

/// `--minimize`'s baseline: knock interventions out of `record`, the latest
/// first, until `replay(try, knocked_out)` misses the target and returns the
/// run that did; [`NothingToKnockOut`] when none is left. Each knock-out
/// starts from the record the previous one replayed, not from what that run
/// recorded: a run's recording also holds the priority changes chaos makes
/// after the replayed record ends, which every run makes again, so knocking
/// those out never ends.
fn knock_out_until_missed<T>(
    mut record: PreemptionRecord,
    mut replay: impl FnMut(usize, &PreemptionRecord) -> anyhow::Result<Option<T>>,
) -> anyhow::Result<T> {
    for attempt in 1.. {
        let Some(knocked_out) = knock_out_latest(&record) else {
            break;
        };
        if let Some(missed) = replay(attempt, &knocked_out)? {
            return Ok(missed);
        }
        record = knocked_out;
    }
    Err(NothingToKnockOut.into())
}

/// `record` with its latest intervention knocked out, for `--minimize`'s
/// baseline; `None` when it has none left. Threads at the default priority
/// with no change are dropped first: replay gives a thread the record does
/// not name that same priority, and a knock-out that removes a whole thread
/// (once no priority change is left) must not pick one of them, which would
/// change nothing.
fn knock_out_latest(record: &PreemptionRecord) -> Option<PreemptionRecord> {
    let pruned = record.without_default_threads();
    let knocked_out = pruned.with_latest_preempt_removed();
    (knocked_out != pruned).then_some(knocked_out)
}

/// The result of `--prefix-search`: replaying the first `decision` events of
/// `schedule` and then scheduling without chaos, with timeslices of
/// `continuation`, misses the target, while replaying event `decision` too
/// meets it.
pub struct PrefixDecision {
    pub schedule: PreemptionRecord,
    pub decision: usize,
    pub continuation: MaybeTimeslice,
}

/// The outcome and recorded schedule of a trial that completed.
struct CompletedScheduleTrial {
    target_met: bool,
    schedule: Vec<SchedEvent>,
}

/// Keep the run's typed failure and its diagnostic files; neither a partial
/// schedule nor a captured stack establishes a completed program outcome.
fn incomplete_trial(error: Error, root: &Path) -> Error {
    let remedy = if error.downcast_ref::<RunTimeoutMarker>().is_some() {
        "If the guest should terminate, raise the applicable bound named in the error: \
         for hermit analyze's trial bound, increase --trial-timeout; for a run timeout, \
         use a larger --timeout in the run arguments. The smaller configured bound still \
         applies. Otherwise investigate why the replay does not finish."
    } else if error
        .downcast_ref::<crate::app::container::PolicyRefusal>()
        .is_some()
    {
        "Inspect the named refusal and trial diagnostics; use a supported operation or \
         configuration before retrying. A refusal cannot establish a target or baseline outcome."
    } else {
        "Resolve the reported run error before retrying analysis."
    };
    error.context(format!(
        "analysis trial did not complete, so no program outcome was established. \
         Trial files are retained with prefix {}. {remedy}",
        root.display()
    ))
}

/// The coarsest timeslice `--continuation-timeslice=auto` tries, in virtual
/// nanoseconds: `hermit run`'s default `--max-timeslice` (200 ms).
pub const COARSEST_CONTINUATION_TIMESLICE: NonZeroU64 = NonZeroU64::new(200_000_000).unwrap();

/// The continuation timeslices `--continuation-timeslice=auto` tries, in
/// order, for a run whose own maximum is `run`: that, then
/// [`COARSEST_CONTINUATION_TIMESLICE`] if it is coarser. Never a shorter one:
/// a trial's `--max-timeslice` also ends the replayed prefix's slices that
/// its schedule does not end, so a shorter one would change the prefix. A run
/// without PMU timeslices (`None`) continues as it is.
fn continuation_candidates(run: MaybeTimeslice) -> Vec<MaybeTimeslice> {
    let Some(run) = run else {
        return vec![None];
    };
    let mut candidates = vec![Some(run), Some(run.max(COARSEST_CONTINUATION_TIMESLICE))];
    candidates.dedup();
    candidates
}

/// What the endpoint trials of a prefix search showed for one continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Continuation {
    /// Replaying none of the schedule misses the target, and replaying all
    /// of it meets it: a search can find the event in between that decides.
    Usable,
    /// The scheduler alone, replaying none of the schedule, meets the target.
    MeetsTargetAlone,
    /// Replaying the whole schedule finished and missed the target.
    ReplayMisses,
}

/// Check the two endpoint trials of a prefix search over `len` events, each
/// a completed outcome: whether the trial met the target.
fn check_continuation(
    len: usize,
    mut trial: impl FnMut(usize) -> anyhow::Result<bool>,
) -> anyhow::Result<Continuation> {
    if len == 0 {
        bail!(
            "the target schedule is empty: there is no scheduling event to locate with a prefix search"
        );
    }
    if trial(0)? {
        return Ok(Continuation::MeetsTargetAlone);
    }
    Ok(if trial(len)? {
        Continuation::Usable
    } else {
        Continuation::ReplayMisses
    })
}

/// Pick the first of `candidates` (continuation timeslices) whose endpoint
/// trials allow a prefix search over `len` events, each candidate's trials
/// run by `trial(prefix, timeslice)`. When none does, the error says what
/// each one showed and what to do instead.
fn choose_continuation(
    candidates: &[MaybeTimeslice],
    len: usize,
    mut trial: impl FnMut(usize, MaybeTimeslice) -> anyhow::Result<bool>,
) -> anyhow::Result<MaybeTimeslice> {
    let mut outcomes = Vec::new();
    for &timeslice in candidates {
        match check_continuation(len, |prefix| trial(prefix, timeslice))? {
            Continuation::Usable => return Ok(timeslice),
            outcome => {
                eprintln!(
                    ":: Continuing without chaos {}, {}",
                    describe_continuation(timeslice),
                    describe_outcome(outcome)
                );
                outcomes.push((timeslice, outcome));
            }
        }
    }
    let alone: Vec<String> = outcomes
        .iter()
        .filter(|(_, outcome)| *outcome == Continuation::MeetsTargetAlone)
        .map(|(timeslice, _)| describe_continuation(*timeslice))
        .collect();
    // Only the empty prefix ran at each of these continuations, and prefix
    // outcomes need not be monotone: a longer prefix can still miss the
    // target. So the error states what those trials observed, not that no
    // event or chaos decision matters.
    if alone.len() == outcomes.len() {
        // A completed analysis, not a failure (`AnalyzeVerdict`).
        return Err(AnalyzeVerdict::NoDecidingEvent(format!(
            "no event of the target schedule decides the outcome: without chaos, the scheduler \
             alone already meets the target criteria {}. The outcome does not depend on a chaos \
             decision, so debug that run directly: the reproducer of the first trial \
             (prefix_round_001.repro in the workspace) runs it. To bisect against a run that \
             misses the target instead, pass --continuation-timeslice with a timeslice at which \
             the scheduler alone misses it, or use the default search with a passing \
             --run2-seed.",
            alone.join(" and ")
        ))
        .into());
    }
    let remedy = "Schedule replay does not reproduce this run where replaying the whole target \
                  schedule misses the target; check that the target run replays (--selfcheck).";
    Err(AnalyzePrecondition::no_prefix_continuation(format!(
        "no continuation allows a prefix search: {}. {remedy} The default search with a \
         passing --run2-seed needs no continuation.",
        outcomes
            .iter()
            .map(|(timeslice, outcome)| format!(
                "{} {}",
                describe_continuation(*timeslice),
                describe_outcome(*outcome)
            ))
            .collect::<Vec<_>>()
            .join("; ")
    ))
    .into())
}

/// Whether the bisection's `error` says that it has not established an exact
/// one-swap pair of schedules with opposite outcomes ([`NoSingleSwap`]): a
/// prefix search, which needs no baseline, then searches for a decision
/// within the failing schedule.
fn no_swap_separates(error: &Error) -> bool {
    error.downcast_ref::<NoSingleSwap>().is_some()
}

/// `error` from bisecting analyze's own endpoints, pointing an endpoint that
/// did not reproduce its outcome at replay: analyze recorded both endpoints
/// itself, so Hermit's replay, or the host's branch counter, did not
/// reproduce the run.
fn point_endpoint_misses_at_replay(error: Error) -> Error {
    match error.downcast::<AnalyzePrecondition>() {
        Ok(precondition)
            if matches!(
                precondition.class,
                PreconditionClass::TargetNotReproduced | PreconditionClass::BaselineNotReproduced
            ) =>
        {
            precondition
                .with_hint(
                    "Analyze recorded this endpoint itself, so its replay did not reproduce \
                     the run: --selfcheck checks that the target run replays, and `hermit \
                     host-capabilities` whether this host's branch counter is exact.",
                )
                .into()
        }
        Ok(precondition) => precondition.into(),
        Err(error) => error,
    }
}

/// How analyze goes on after bisecting its own endpoints
/// ([`own_bisection_end`]).
enum OwnBisectionEnd {
    /// The critical pair, to report.
    Critical(CriticalSchedule),
    /// The bisection has not established an exact one-swap pair of schedules
    /// with opposite outcomes (the error says why): a prefix search, which
    /// needs no baseline, searches for a decision within the failing schedule.
    SearchPrefixes(Error),
}

/// What the bisection of analyze's own endpoints, `bisected`, leads to. An
/// endpoint that did not reproduce is pointed at replay
/// ([`point_endpoint_misses_at_replay`]); any other error ends the analysis.
fn own_bisection_end(
    bisected: anyhow::Result<CriticalSchedule>,
) -> anyhow::Result<OwnBisectionEnd> {
    match bisected {
        Ok(critical) => Ok(OwnBisectionEnd::Critical(critical)),
        Err(error) if no_swap_separates(&error) => Ok(OwnBisectionEnd::SearchPrefixes(error)),
        Err(error) => Err(point_endpoint_misses_at_replay(error)),
    }
}

/// --selfcheck's conclusion from its rerun of the target: whether it met the
/// target, whether its log matched, and (checked last) whether it recorded
/// the preemptions it replayed. Any difference is replay not reproducing,
/// Hermit's or the host's, and a failure (`AnalyzeFailure`), not an input
/// analyze cannot use.
fn selfcheck_conclusion(
    rerun_matched: bool,
    logs_agree: bool,
    fixed_point: impl FnOnce() -> Result<(), String>,
) -> anyhow::Result<()> {
    const HOST: &str = "`hermit host-capabilities` reports whether this host's branch counter \
                        is exact.";
    let difference = if !rerun_matched {
        "First run matched criteria but second run did not.".to_string()
    } else if !logs_agree {
        "Log differences found, aborting because --selfcheck requires perfect reproducibility \
         of the target run!"
            .to_string()
    } else if let Err(difference) = fixed_point() {
        difference
    } else {
        return Ok(());
    };
    Err(AnalyzeFailure::ReplayNotReproducible(format!("{difference} {HOST}")).into())
}

/// The verdict of a prefix search whose empty prefix meets the target when
/// continued without chaos at `continuation` (`error`, an
/// [`EmptyPrefixMeetsTarget`]), naming any coarser one of `candidates` it did
/// not try.
fn empty_prefix_verdict(
    error: &Error,
    candidates: &[Option<NonZeroU64>],
    continuation: Option<NonZeroU64>,
) -> AnalyzeVerdict {
    // The empty prefix's first trial was stopped, so the continuation looked
    // usable and any coarser candidate went untried; name it rather than let
    // the host's speed decide.
    let untried: Vec<String> = candidates
        .iter()
        .skip_while(|&&candidate| candidate != continuation)
        .skip(1)
        .map(|&candidate| describe_continuation(candidate))
        .collect();
    let untried = if untried.is_empty() {
        String::new()
    } else {
        format!(
            " The search did not try continuing {}, because the empty prefix's first trial did \
             not finish; --continuation-timeslice selects one.",
            untried.join(" or ")
        )
    };
    AnalyzeVerdict::NoDecidingEvent(format!(
        "no event of the target schedule decides the outcome: {error}, so without chaos {}, the \
         scheduler alone, replaying none of the target schedule, meets the target. The outcome \
         does not depend on a chaos decision, so debug that run directly: the reproducer of the \
         last trial in the workspace runs it. To bisect against a run that misses the target \
         instead, pass --continuation-timeslice with a timeslice at which the scheduler alone \
         misses it, or use the default search with a passing --run2-seed.{untried}",
        describe_continuation(continuation)
    ))
}

/// "at a 200000000 ns timeslice", or "without PMU timeslices".
fn describe_continuation(timeslice: MaybeTimeslice) -> String {
    match timeslice {
        Some(nanos) => format!("at a {nanos} ns timeslice"),
        None => "without PMU timeslices".to_owned(),
    }
}

fn describe_outcome(outcome: Continuation) -> &'static str {
    match outcome {
        Continuation::Usable => "a prefix search can run",
        Continuation::MeetsTargetAlone => {
            "the scheduler alone, replaying none of the target schedule, meets the target"
        }
        Continuation::ReplayMisses => "replaying the whole target schedule misses the target",
    }
}

/// The binary search of `--prefix-search` over a schedule of `len` events
/// whose endpoints [`check_continuation`] found usable: returns `k` such that
/// the prefix of `k` events misses the target and the prefix of `k + 1` meets
/// it. Uses about log2(len) trials.
fn search_prefix_boundary(
    len: usize,
    mut meets_target: impl FnMut(usize) -> anyhow::Result<bool>,
) -> anyhow::Result<usize> {
    let (mut passing, mut failing) = (0, len);
    while failing - passing > 1 {
        let mid = passing + (failing - passing) / 2;
        if meets_target(mid)? {
            failing = mid;
        } else {
            passing = mid;
        }
    }
    Ok(passing)
}

/// Why the critical pair a search settled on may not be a race, if there is a reason.
///
/// Swapping two events of one thread is no interleaving any execution can take, so
/// the outcome change at such a pair comes from the replay falling off the schedule.
fn critical_pair_caveat(crit: &CriticalSchedule) -> Option<String> {
    let index = crit.critical_event_index;
    let schedule = &crit.failing_schedule;
    let same_thread =
        index > 0 && index < schedule.len() && schedule[index - 1].dettid == schedule[index].dettid;
    if !same_thread {
        return None;
    }
    Some(format!(
        "the critical events #{} and #{index} both belong to thread {}, so swapping them is \
         not an interleaving and this pair is not a race.",
        index - 1,
        schedule[index].dettid,
    ))
}

/// How long one thread runs from an event on, in one schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slice {
    /// Its events, from the first one on.
    events: usize,
    /// The retired conditional branches among them.
    branches: u64,
    /// The event at which another thread runs next, and that thread; `None`
    /// if none does before the schedule ends.
    next: Option<(usize, DetTid)>,
}

/// The name [`continued_record`]'s refusal starts with.
const UNATTACHED_CHECKPOINTS: &str = "HERMIT_CONTINUATION_CHECKPOINTS_UNATTACHED";

/// A record that replays `continued`, the schedule a completed prefix trial
/// of the first `len` events of `schedule` realized (recorded at
/// `continued_path`), under the callback checkpoints of that prefix: a replay
/// records none of its own, but its replayed part ended its slices where the
/// recording did.
///
/// A checkpoint attaches where its thread's event counts add up to the
/// progress it recorded ([`PreemptionRecord::with_schedule`]). A completed
/// trial can still have replayed the prefix with other counts (a count-only
/// mismatch is soft, and a branch counter that miscounts can cause one), and
/// then a checkpoint has no event endpoint. That is a refusal, before any
/// replay of the record: replayed without the checkpoints, `continued` need
/// not end the prefix's slices where the search's trial did, so a stack taken
/// from it could come from no run the search made. The error names the
/// recorded continuation, which stays in the workspace for diagnosis.
fn continued_record(
    schedule: &PreemptionRecord,
    len: usize,
    continued: &[SchedEvent],
    continued_path: &Path,
) -> anyhow::Result<PreemptionRecord> {
    let prefix = schedule.schedule_prefix(len);
    if let Some(record) = prefix.with_schedule(continued.to_vec()) {
        return Ok(record);
    }
    let expected = prefix.schedevents();
    let drift = expected
        .iter()
        .zip(continued)
        .position(|(expected, ran)| expected != ran)
        .map_or_else(String::new, |index| {
            format!(
                "; its event #{index} is {}, where the prefix has {}",
                continued[index], expected[index]
            )
        });
    Err(
        anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
            "{UNATTACHED_CHECKPOINTS}: analysis cannot replay the completed continuation recorded \
             at {} under the callback checkpoints of the target schedule's first {len} events: \
             they do not attach to its events{drift}. A checkpoint attaches where its thread's \
             event counts add up to the progress it recorded, so a continuation that replayed \
             those events with other counts (a soft count mismatch, as on a branch counter that \
             fails validation) leaves it no event endpoint. Without the checkpoints a replay need \
             not end the continuation's slices where the search's trial did, so no stack is taken \
             from one and no opposite-outcome pair was established. The recorded continuation \
             and its trial files are kept in the workspace; inspect them and stabilize replay \
             before retrying analysis.",
            continued_path.display()
        )),
    )
}

/// How many events of the search's schedule keep their callback
/// checkpoints in a race candidate's check of the `role` run: that run's own
/// prefix (the baseline replayed one event fewer than the target's `len`),
/// up to `unchanged`, where the edit starts.
fn check_prefix_len(len: usize, role: &str, unchanged: usize) -> usize {
    let own = if role == "baseline" {
        len.saturating_sub(1)
    } else {
        len
    };
    own.min(unchanged)
}

/// The record a race candidate's check replays: `recorded`'s first
/// `unchanged` events with their callback checkpoints, and `edited` (which
/// starts with those events) as its schedule. Refused when the checkpoints do
/// not fit the edited events, never replayed as a bare schedule: without them
/// the unchanged events would not end their slices where the run did, and the
/// check would measure another run than the one it names. The refusal is a
/// [`PolicyRefusal`](crate::app::container::PolicyRefusal) named
/// `HERMIT_CANDIDATE_CHECK_CHECKPOINTS` (docs/ERROR_CATALOG.md); it is the
/// check's error, which its report names, and the analysis goes on.
fn candidate_check_record(
    recorded: &PreemptionRecord,
    unchanged: usize,
    edited: &[SchedEvent],
) -> anyhow::Result<PreemptionRecord> {
    recorded
        .schedule_prefix(unchanged)
        .with_schedule(edited.to_vec())
        .ok_or_else(|| {
            anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
                "HERMIT_CANDIDATE_CHECK_CHECKPOINTS: the callback checkpoints of the recording's \
                 first {unchanged} events do not fit the edited schedule, and replaying it without \
                 them would not follow the run it checks"
            ))
        })
}

/// The run of `thread`'s events in `schedule` from event `from` on.
fn slice_from(schedule: &[SchedEvent], from: usize, thread: DetTid) -> Slice {
    let mut slice = Slice {
        events: 0,
        branches: 0,
        next: None,
    };
    for (index, event) in schedule.iter().enumerate().skip(from) {
        if event.dettid != thread {
            slice.next = Some((index, event.dettid));
            break;
        }
        slice.events += 1;
        if event.op == detcore_model::schedule::Op::Branch {
            slice.branches += u64::from(event.count);
        }
    }
    slice
}

/// Whether two stacks have the same frames, whichever threads they belong
/// to.
fn same_frames(first: &PrettyBacktrace, second: &PrettyBacktrace) -> bool {
    first.iter().eq(second.iter())
}

/// The events at which the deciding thread's slice ends in the continuation
/// with the deciding event and in the one without it, when the stacks of the
/// threads that run next (`next_stacks`) tell nothing apart: both captured,
/// with the same frames (often threads just started, at their first
/// instruction). `None` otherwise.
fn slice_end_events(
    with: &Slice,
    without: &Slice,
    next_stacks: [Option<&PrettyBacktrace>; 2],
) -> Option<[usize; 2]> {
    match (next_stacks, with.next, without.next) {
        ([Some(first), Some(second)], Some((with_next, _)), Some((without_next, _)))
            if same_frames(first, second) =>
        {
            Some([with_next - 1, without_next - 1])
        }
        _ => None,
    }
}

/// The header of a `--prefix-search` report whose deciding event `event`,
/// #`decision`, is a timing decision: `with` and `without` are how long its
/// thread then runs with and without that event replayed. `slice_ends`, the
/// last events of those slices, is set when stack1 and stack2 show where the
/// deciding thread stops instead of the thread that runs next.
fn timing_decision_header(
    decision: usize,
    event: &SchedEvent,
    (with, without): (&Slice, &Slice),
    criteria: &str,
    continuation: MaybeTimeslice,
    slice_ends: Option<[usize; 2]>,
) -> String {
    let thread = event.dettid;
    let then = |slice: &Slice, stack: &str| match slice.next {
        Some((index, next)) if slice_ends.is_none() => {
            format!("then thread {next} runs at event #{index} ({stack})")
        }
        Some((index, next)) => format!("then thread {next} runs at event #{index}"),
        None => "and no other thread runs after it".to_owned(),
    };
    let mut header = format!(
        "The scheduling decision at event #{decision} of the target schedule decides the \
         outcome, and it is a timing decision: with or without it, the scheduler runs thread \
         {thread} there, so what decides is how long thread {thread} runs, not which thread \
         runs.\nWith event #{decision} replayed ({event}), thread {thread} runs {} events ({} \
         retired conditional branches) from event #{decision} on, {}, and the run meets the \
         target criteria ({criteria}).\nWithout it, scheduling without chaos {}, thread {thread} \
         runs {} events ({} retired conditional branches), {}, and the run does not.\n",
        with.events,
        with.branches,
        then(with, "stack1"),
        describe_continuation(continuation),
        without.events,
        without.branches,
        then(without, "stack2"),
    );
    header.push_str(&format!(
        "Replaying the first {decision} events and then scheduling without chaos \
         {} does not meet the target criteria; replaying event #{decision} too ({event}) does \
         ({criteria}).\n",
        describe_continuation(continuation),
    ));
    if let Some([with_end, without_end]) = slice_ends {
        header.push_str(&format!(
            "The thread that runs next has the same stack in both continuations (often a \
             thread just started), so stack1 and stack2 show where thread {thread} is when its \
             slice ends instead: at event #{with_end} with the event, at event #{without_end} \
             without it.\n"
        ));
    }
    header.push_str(&format!(
        "Thread {thread}'s own stack at event #{decision} is printed first (deciding_event in \
         the report file).\nThe outcome depends on what thread {thread} does before the other \
         thread runs: add synchronization that makes its timing irrelevant, or that enforces \
         the passing order.\n"
    ));
    header
}

/// What a stack-trace replay of a critical pair gave.
pub(super) struct StackReplay {
    /// It met the target criteria (it finished: [`AnalyzeOpts::replay_for_stacks`]).
    pub(super) met_target: bool,
    /// The stacks at its schedule's events `index - 1` and `index`.
    stacks: [Option<PrettyBacktrace>; 2],
    /// Where it left its schedule before passing event `index`, if it did.
    left_schedule: Option<Departure>,
    /// The threads it ran at events `index - 1` and `index`, if it
    /// recorded them.
    ran: [Option<DetTid>; 2],
    /// Its access trace, when it recorded one.
    traced: Option<TracedReplay>,
}

impl StackReplay {
    /// Whether the replay followed its schedule through event `event`, so
    /// that a stack it took there is that event's.
    fn followed_through(&self, event: usize) -> bool {
        match &self.left_schedule {
            None => true,
            Some(Departure::OtherEvent { index, .. }) => *index > event,
            Some(Departure::Ended { events }) => *events > event,
        }
    }
}

/// Where a replay left the schedule it replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Departure {
    /// At this event it ran another event than the schedule's: another
    /// thread, operation or instruction pointer, compared as the scheduler
    /// does when it takes a stack (the count and time aside).
    OtherEvent {
        index: usize,
        expected: SchedEvent,
        ran: SchedEvent,
    },
    /// It ended after this many events.
    Ended { events: usize },
}

impl std::fmt::Display for Departure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Departure::OtherEvent {
                index,
                expected,
                ran,
            } => write!(
                f,
                "ran {ran} at event #{index}, where the schedule has {expected}"
            ),
            Departure::Ended { events } => write!(f, "ended after {events} events"),
        }
    }
}

/// Ends the analysis unless every stack-trace replay observed the outcome the
/// search observed for its schedule. Each replay is `(name, whether the
/// search's run met the target, whether the replay met it)`. A replay that
/// observed the opposite outcome cannot certify the decision, in either
/// direction, so no opposite-outcome pair is established; `inspect` names the
/// diagnostics to read.
fn require_repeated_outcomes(replays: &[(&str, bool, bool)], inspect: &str) -> anyhow::Result<()> {
    let opposite: Vec<String> = replays
        .iter()
        .filter(|(_, expected, observed)| expected != observed)
        .map(|(replay, expected, _)| {
            format!(
                "{replay} observed the opposite of the search's run, which {} the target",
                if *expected { "met" } else { "missed" }
            )
        })
        .collect();
    if opposite.is_empty() {
        return Ok(());
    }
    bail!(
        "the stack-trace replays did not repeat the search's outcomes ({}); no opposite-outcome \
         pair was established. Inspect {inspect} diagnostics and stabilize replay before \
         retrying analysis.",
        opposite.join("; ")
    )
}

/// Whether `ran` is the event `expected`, as the scheduler compares them when
/// it takes a stack: the count and the time aside, and only instruction
/// pointers present in the recorded event constrain the observed ones.
fn same_event(expected: &SchedEvent, ran: &SchedEvent) -> bool {
    expected.dettid == ran.dettid
        && expected.op == ran.op
        && (expected.start_rip.is_none() || expected.start_rip == ran.start_rip)
        && (expected.end_rip.is_none() || expected.end_rip == ran.end_rip)
}

/// The first of the first `until` events of `requested` at which `realized`
/// ran another event, or ended.
pub(super) fn departure(
    requested: &[SchedEvent],
    realized: &[SchedEvent],
    until: usize,
) -> Option<Departure> {
    let until = until.min(requested.len());
    for (index, expected) in requested[..until].iter().enumerate() {
        match realized.get(index) {
            None => return Some(Departure::Ended { events: index }),
            Some(ran) if !same_event(expected, ran) => {
                return Some(Departure::OtherEvent {
                    index,
                    expected: expected.clone(),
                    ran: ran.clone(),
                });
            }
            Some(_) => {}
        }
    }
    None
}

/// The thread a stack was taken on, as the guest numbers it (which is the
/// scheduler's numbering under Hermit's thread-id virtualization).
pub(super) fn stack_thread(stack: &PrettyBacktrace) -> Option<DetTid> {
    let thread = serde_json::to_value(stack)
        .ok()?
        .get("thread_id")?
        .as_i64()?;
    Some(DetTid::from_raw(i32::try_from(thread).ok()?))
}

/// The stacks of the two racing operations, in the passing schedule's order,
/// each with the replay it came from; `threads` are the threads of the
/// passing schedule's events `index - 1` and `index`. Those events are the
/// failing schedule's `index` and `index - 1`, so each stack can come from
/// either replay: preferably one that followed its schedule through that
/// event, else one taken on that operation's thread (the replay ran that
/// thread there, or the stack says so), said to be possibly another
/// occurrence of the operation. A stack taken on another thread is left
/// out: a replay that left its schedule can take one there.
fn merge_stacks(
    baseline: &StackReplay,
    target: &StackReplay,
    index: usize,
    threads: [DetTid; 2],
) -> [Option<(PrettyBacktrace, String)>; 2] {
    [0, 1].map(|slot| {
        let other = 1 - slot;
        let candidates = [
            ("the baseline replay", baseline, slot, index - 1 + slot),
            ("the target replay", target, other, index - 1 + other),
        ];
        candidates
            .iter()
            .find(|(_, replay, stack, event)| {
                replay.stacks[*stack].is_some() && replay.followed_through(*event)
            })
            .map(|(name, replay, stack, _)| {
                (replay.stacks[*stack].clone().unwrap(), (*name).to_owned())
            })
            .or_else(|| {
                candidates.iter().find_map(|(name, replay, stack, _)| {
                    let taken = replay.stacks[*stack].as_ref()?;
                    let on_its_thread = replay.ran[*stack] == Some(threads[slot])
                        || stack_thread(taken) == Some(threads[slot]);
                    on_its_thread.then_some(())?;
                    replay.stacks[*stack].clone().map(|stack| {
                        (
                            stack,
                            format!(
                                "{name}, which left its schedule before this operation, so the \
                                 stack may be another occurrence of it"
                            ),
                        )
                    })
                })
            })
    })
}

/// Why neither replay captured `which` (the stack at the passing schedule's
/// event `event`).
fn missing_stack_reason(
    which: &str,
    (event, target_event): (usize, usize),
    baseline: &StackReplay,
    target: &StackReplay,
) -> String {
    let describe = |replay: &StackReplay, event: usize| match &replay.left_schedule {
        Some(departure) if !replay.followed_through(event) => {
            format!("left its schedule: it {departure}")
        }
        _ => format!(
            "followed its schedule through event #{event}, but left no readable stack there"
        ),
    };
    format!(
        "{which} is not available: no stack-trace replay captured that operation (event #{event} \
         of the passing schedule). The baseline replay {}; the target replay {}. Schedule replay \
         did not reproduce these runs; check that they replay (--selfcheck) on a host with an \
         exact branch counter (hermit host-capabilities).",
        describe(baseline, event),
        describe(target, target_event),
    )
}

/// The line of `text` that holds `pattern`'s first match.
fn matched_line<'a>(pattern: &Regex, text: &'a str) -> Option<&'a str> {
    let found = pattern.find(text)?;
    let start = text[..found.start()].rfind('\n').map_or(0, |at| at + 1);
    let end = text[found.start()..]
        .find('\n')
        .map_or(text.len(), |at| found.start() + at);
    Some(&text[start..end])
}

/// Whether the replay at workspace root `replay` met the criteria with
/// another failure than the target run at root `target`: whether the line
/// each output pattern (`patterns`, by stream) matches differs between them.
/// Output the patterns do not match may differ with the interleaving and is
/// not compared.
fn changed_output(
    target: &Path,
    replay: &Path,
    patterns: [(&str, Option<&Regex>); 2],
) -> Option<String> {
    patterns.into_iter().find_map(|(stream, pattern)| {
        let pattern = pattern?;
        let read = |root: &Path| {
            std::fs::read(root.with_extension(stream))
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        };
        let (want, got) = (read(target)?, read(replay)?);
        let (want, got) = (matched_line(pattern, &want)?, matched_line(pattern, &got)?);
        (want != got).then(|| {
            format!(
                "the final target schedule meets the target criteria with another failure than \
                 the target run: the {stream} line that --target-{stream} matches is {got:?}, \
                 where the target run's is {want:?}. The critical pair may belong to that other \
                 failure; narrow --target-{stream} to tell them apart."
            )
        })
    })
}

impl AnalyzeOpts {
    /// The branch-count difference two replays of one control schedule may
    /// show and still count as one realized trace
    /// (`schedule_search::Config::max_branch_count_jitter`): on a host whose
    /// branch counter is not exact only, which analyze says. Elsewhere the two
    /// must be identical, event times included.
    fn replay_branch_jitter(&self) -> Option<u32> {
        let reason = (self.inexact_branch_counter)()?;
        eprintln!(
            ":: {} this host's branch counter is not exact ({reason}), so two replays of one \
             schedule count as the same trace when their branch counts differ by at most {}",
            "NOTE:".yellow().bold(),
            INEXACT_COUNTER_BRANCH_JITTER
        );
        Some(INEXACT_COUNTER_BRANCH_JITTER)
    }

    /// The default search's configuration: `--verbose`, `--run-needleman`,
    /// `--jitter-dist`, and the branch-count difference two replays of one
    /// control schedule may show, on a host whose branch counter is not exact
    /// only ([`Self::replay_branch_jitter`]).
    fn bisect_config(&self) -> Config {
        let mut cfg = Config {
            verbose: self.verbose,
            needleman_search: self.run_needleman,
            max_branch_count_jitter: self.replay_branch_jitter(),
            ..Default::default()
        };
        if let Some(dist) = self.jitter_dist {
            cfg.max_jitter_editdist = dist;
            cfg.max_jitter_swapdist = dist * dist;
        }
        cfg
    }

    pub fn get_tmp(&self) -> anyhow::Result<&Path> {
        if let Some(pb) = &self.tmp_dir {
            Ok(pb.as_path())
        } else {
            bail!("Expected tmp_dir to be set at this point!")
        }
    }

    /// The chaos run of `--search` round `round`, with schedule seed
    /// `sched_seed`. The one that matches becomes the target, whose log
    /// `--selfcheck` compares.
    pub(super) fn search_run(&self, round: u64, sched_seed: u64) -> anyhow::Result<RunData> {
        let runname = format!("search_round_{:0wide$}", round, wide = 3);
        let mut rundat = RunData::new_baseline(self, runname)?
            .with_preemption_recording()
            .compared_by_selfcheck();
        rundat.runopts.det_opts.det_config.sched_seed = Some(sched_seed);
        if self.imprecise_search {
            rundat.runopts.det_opts.det_config.imprecise_timers = true; // TODO: enable this by default when bugs are fixed.
        }
        Ok(rundat)
    }

    /// Launch a chaos run searching for a target (e.g. failing) schudule.
    /// Returns Some if a target schedule is found.
    fn launch_search(&self, round: u64, sched_seed: u64) -> Result<Option<RunData>, Error> {
        yellow_msg(&format!(
            "Searching (round {}) for a target execution, chaos --sched-seed={} ",
            round, sched_seed
        ));
        let mut rundat = self.search_run(round, sched_seed)?;
        rundat.launch()?;
        if rundat.is_a_match() {
            eprintln!(
                ":: {}:\n    {}",
                "Target run established by --search. Reproducer"
                    .green()
                    .bold(),
                rundat.to_repro(),
            );
            Ok(Some(rundat))
        } else {
            Ok(None)
        }
    }

    /// `run` replaying the target's preemption record at `path`: one
    /// minimization edited when `--minimize` ran, which replays as edited
    /// ([`RunData::with_edited_preempts_path_in`]); otherwise the target
    /// run's recording, which replays strictly.
    fn replaying_the_target_record(&self, run: RunData, path: PathBuf) -> RunData {
        if self.minimize {
            run.with_edited_preempts_path_in(path)
        } else {
            run.with_preempts_path_in(path)
        }
    }

    /// The run that replays the preemptions in `preempts_path`, as a
    /// `--minimize` round does.
    pub(super) fn preempts_replay_run(
        &self,
        runname: &str,
        preempts_path: &Path,
    ) -> anyhow::Result<RunData> {
        Ok(RunData::new_baseline(self, runname.to_string())?
            .with_edited_preempts_path_in(preempts_path.to_path_buf())
            .compared_by_selfcheck())
    }

    // TODO: REMOVE. Only used by minimize atm.
    //
    /// Launch a run with preempts provided (to replay). Return true if it matches the
    /// criteria. If provided, additionally record full schedule events from the run to
    /// `record_sched_path`. The last of these runs to match is the minimized target, whose log
    /// --selfcheck compares.
    pub(super) fn launch_from_preempts_to_sched(
        &self,
        runname: &str,
        preempts_path: &Path,
        record_sched_path: Option<&Path>,
    ) -> anyhow::Result<(bool, RunOpts)> {
        let mut run = self.preempts_replay_run(runname, preempts_path)?;
        if let Some(path) = record_sched_path {
            run = run.with_schedule_recording_to(path.to_path_buf());
        }
        run.launch()?;
        let is_match = run.is_a_match();
        Ok((is_match, run.into_runopts()))
    }

    /// Replay `schedule` (written at `schedule_path`) for the stacks of the
    /// critical pair, its events `index - 1` and `index`, recording the
    /// schedule the replay realized. A replay that does not finish ends the
    /// analysis with its error (`incomplete_trial`): its stacks stay in the
    /// workspace, but only a finished replay's are used.
    pub(super) fn replay_for_stacks(
        &self,
        runname: &str,
        schedule_path: &Path,
        schedule: &[SchedEvent],
        index: usize,
        trace: Option<(&'static str, u64)>,
        launch: impl FnOnce(&mut RunData) -> anyhow::Result<()>,
    ) -> anyhow::Result<StackReplay> {
        let run = RunData::new_baseline(self, runname.to_string())?
            .with_schedule_replay_from(schedule_path.to_path_buf())
            .with_schedule_recording();
        let (role, from) = trace.unzip();
        let (mut run, request) = self.request_trace(run, role.unwrap_or_default(), from);
        let root_path = run.root_path();
        let realized = run.sched_path_out().to_path_buf();
        let paths = [
            root_path.with_extension("stack1"),
            root_path.with_extension("stack2"),
        ];
        run.runopts.det_opts.det_config.stacktrace_event = vec![
            (index as u64 - 1, Some(paths[0].clone())),
            (index as u64, Some(paths[1].clone())),
        ];
        launch(&mut run).map_err(|error| incomplete_trial(error, &root_path))?;
        eprintln!(
            "\n:: {}\n{}",
            "Stack-trace replay finished.  Repro command:"
                .green()
                .bold(),
            run.to_repro()
        );
        let mut stacks = [None, None];
        for (stack, path) in stacks.iter_mut().zip(&paths) {
            let Ok(file) = File::open(path) else {
                continue;
            };
            match serde_json::from_reader(file) {
                Ok(parsed) => *stack = Some(parsed),
                Err(error) => eprintln!(
                    ":: {} cannot read the stack trace in {} ({error}); it is left out",
                    "WARNING:".yellow().bold(),
                    path.display()
                ),
            }
        }
        let realized_path = realized;
        let realized = PreemptionReader::try_new(&realized_path)
            .with_context(|| {
                format!(
                    "completed stack-trace replay {runname} has no readable recorded schedule at \
                     {}; retain the trial diagnostics and retry recording before analysis",
                    realized_path.display()
                )
            })?
            .into_inner()
            .into_global();
        let ran_at = |event: usize| realized.get(event).map(|event| event.dettid);
        let left_schedule = departure(schedule, &realized, index + 1);
        let ran = [ran_at(index - 1), ran_at(index)];
        let traced = request
            .map(|request| request.ran_with(left_schedule.clone(), realized, &realized_path));
        Ok(StackReplay {
            met_target: run.is_a_match(),
            stacks,
            left_schedule,
            ran,
            traced,
        })
    }

    /// One `--prefix-search` trial: replay the first `len` events of `target`,
    /// then let the scheduler go on without chaos, with timeslices of
    /// `continuation`. The trial's record keeps the callback checkpoints
    /// within the prefix ([`PreemptionRecord::schedule_prefix`]), so its
    /// slices end where the recording's did, all but the one running at the
    /// prefix's end: where that slice ends decides what runs next, which is
    /// event `len`'s decision. A trial that replayed it would already make
    /// that decision, and the search would name the event before it, with
    /// the thread that ran up to it on both sides of the pair.
    fn prefix_trial(
        &self,
        runname: &str,
        target: &PreemptionRecord,
        len: usize,
        continuation: MaybeTimeslice,
    ) -> anyhow::Result<RunData> {
        let mut run = RunData::new_baseline(self, runname.to_string())?;
        run.runopts
            .set_analysis_continuation_timeslice(continuation);
        let path = run.sched_path_in().to_path_buf();
        // This is an edited prefix plus a new continuation, not a full replay
        // under the target's drawn maximum. Keep the generated record and
        // the trial's explicit maximum consistent, without changing target.
        let prefix = target
            .schedule_prefix(len)
            .with_chaos_timeslice(continuation.map(NonZeroU64::get));
        self.write_trial_record(prefix, &path)?;
        Ok(run.with_schedule_replay_from(path))
    }

    /// Launch a prefix trial; true when its completed outcome meets the target.
    fn launch_prefix_trial(
        &self,
        run: &mut RunData,
        launch: impl FnOnce(&mut RunData) -> anyhow::Result<()>,
    ) -> anyhow::Result<bool> {
        if self.verbose {
            eprintln!(
                ":: {}\n    {}",
                "Prefix trial, repro command:".yellow().bold(),
                run.to_repro()
            );
        }
        launch(run).map_err(|error| incomplete_trial(error, &run.root_path()))?;
        Ok(run.is_a_match())
    }

    /// `--prefix-search`: find the shortest prefix of the target schedule that,
    /// replayed and then continued without chaos, still meets the target
    /// criteria. Returns the index of that prefix's last event.
    pub fn phase5_prefix_search(
        &self,
        target_record: &PreemptionRecord,
    ) -> anyhow::Result<PrefixDecision> {
        self.phase5_prefix_search_with(target_record, |trial, len, continuation| {
            let mut run = self.prefix_trial(
                &format!("prefix_round_{trial:03}"),
                target_record,
                len,
                continuation,
            )?;
            self.launch_prefix_trial(&mut run, RunData::launch)
        })
    }

    fn phase5_prefix_search_with(
        &self,
        target_record: &PreemptionRecord,
        mut launch: impl FnMut(usize, usize, MaybeTimeslice) -> anyhow::Result<bool>,
    ) -> anyhow::Result<PrefixDecision> {
        let target = target_record.schedevents();
        let mut trial = 0;
        let mut meets_target = |len: usize, continuation: MaybeTimeslice| -> anyhow::Result<bool> {
            trial += 1;
            let is_match = launch(trial, len, continuation)?;
            eprintln!(
                ":: Prefix of {len} of {} events => {}",
                target.len(),
                if is_match {
                    format!("target condition ({})", self.display_criteria())
                } else {
                    "baseline condition".to_string()
                }
            );
            Ok(is_match)
        };

        let run_timeslice = target_record
            .chaos_timeslice()
            .and_then(NonZeroU64::new)
            .or(self.trial_max_timeslice()?);
        self.refuse_unsound_continuation_for(run_timeslice)?;
        let candidates = match self.continuation_timeslice {
            ContinuationTimeslice::Auto => continuation_candidates(run_timeslice),
            ContinuationTimeslice::Nanos(nanos) => vec![Some(nanos)],
        };
        let continuation = choose_continuation(&candidates, target.len(), &mut meets_target)?;
        eprintln!(
            ":: Searching the prefixes continued without chaos {}",
            describe_continuation(continuation)
        );
        let mut meets_target = |len| meets_target(len, continuation);
        let passing = search_prefix_boundary(target.len(), &mut meets_target)?;
        let failing = passing + 1;
        // On a host whose branch counter is not exact, two replays of one
        // prefix can differ, so the boundary is checked once more.
        if meets_target(passing)? || !meets_target(failing)? {
            bail!(
                "prefix boundary did not reproduce opposite outcomes for {passing} and \
                 {failing} events; no deciding event was established. Inspect the prefix trial \
                 diagnostics and stabilize replay before retrying analysis."
            );
        }
        eprintln!(
            ":: The scheduling decision at event #{passing} of {} decides the outcome: {}",
            target.len(),
            target[passing]
        );
        Ok(PrefixDecision {
            schedule: target_record.clone(),
            decision: passing,
            continuation,
        })
    }

    /// Replay the first `len` events of `schedule` as a prefix trial and
    /// return whether it met the target with the stack at schedule position
    /// `position`: a replayed event, or, at `len`, the first event the
    /// scheduler ran after the prefix.
    ///
    /// The stack is requested by schedule position
    /// (`--stacktrace-at-schedule-positions`), not by traced index: the trial
    /// can observe events its schedule does not hold before `position`, such
    /// as a polling retry the recording did not make, and a traced index
    /// would then name a neighbouring event
    /// (https://github.com/rrnewton/hermit/issues/4917).
    fn prefix_stack(
        &self,
        runname: &str,
        schedule: &PreemptionRecord,
        len: usize,
        position: usize,
        continuation: MaybeTimeslice,
    ) -> anyhow::Result<(bool, Option<PrettyBacktrace>)> {
        let (is_match, stack, _) =
            self.traced_prefix_stack(runname, schedule, len, position, continuation, None)?;
        Ok((is_match, stack))
    }

    /// [`Self::prefix_stack`] for the target side of the report: with
    /// `trace_from` set, the trial also records its schedule and an access
    /// trace from that event.
    fn traced_prefix_stack(
        &self,
        runname: &str,
        schedule: &PreemptionRecord,
        len: usize,
        position: usize,
        continuation: MaybeTimeslice,
        trace_from: Option<u64>,
    ) -> anyhow::Result<(bool, Option<PrettyBacktrace>, Option<TracedReplay>)> {
        let (run, stack_path) =
            self.prefix_stack_trial(runname, schedule, len, position, continuation)?;
        let (mut run, request) = self.request_trace(run, "target", trace_from);
        let is_match = self.launch_prefix_trial(&mut run, RunData::launch)?;
        eprintln!(
            ":: {}\n{}",
            "Stack-trace run completed.  Repro command:".green().bold(),
            run.to_repro()
        );
        let stack = match File::open(&stack_path) {
            Ok(file) => Some(serde_json::from_reader(file).with_context(|| {
                format!("cannot parse the stack trace in {}", stack_path.display())
            })?),
            Err(_) => None,
        };
        let traced = request.map(|request| {
            let realized = run.sched_path_out().to_path_buf();
            request.ran(schedule.schedevents(), &realized, len)
        });
        Ok((is_match, stack, traced))
    }

    /// The prefix trial `prefix_stack` launches, and the file its stack goes
    /// to: the stack at schedule `position`, requested by position.
    fn prefix_stack_trial(
        &self,
        runname: &str,
        schedule: &PreemptionRecord,
        len: usize,
        position: usize,
        continuation: MaybeTimeslice,
    ) -> anyhow::Result<(RunData, PathBuf)> {
        let mut run = self.prefix_trial(runname, schedule, len, continuation)?;
        let stack_path = run.root_path().with_extension("stack");
        let config = &mut run.runopts.det_opts.det_config;
        config.stacktrace_event = vec![(position as u64, Some(stack_path.clone()))];
        config.stacktrace_at_schedule_positions = true;
        Ok((run, stack_path))
    }

    /// The completed outcome and schedule of a prefix trial: the replayed
    /// events, then the events the scheduler ran after them, and the file the
    /// schedule is recorded in.
    fn prefix_continuation(
        &self,
        runname: &str,
        schedule: &PreemptionRecord,
        len: usize,
        continuation: MaybeTimeslice,
        trace: Option<(&'static str, u64)>,
        launch: impl FnOnce(&mut RunData) -> anyhow::Result<bool>,
    ) -> anyhow::Result<(CompletedScheduleTrial, PathBuf, Option<TracedReplay>)> {
        let run = self
            .prefix_trial(runname, schedule, len, continuation)?
            .with_schedule_recording();
        let (role, from) = trace.unzip();
        let (mut run, request) = self.request_trace(run, role.unwrap_or_default(), from);
        let path = run.sched_path_out().to_path_buf();
        let target_met = launch(&mut run)?;
        let record = PreemptionReader::try_new(&path).with_context(|| {
            format!(
                "completed prefix trial has no readable recorded schedule at {}; \
                 retain the trial diagnostics and retry recording before analysis",
                path.display()
            )
        })?;
        let record = record.into_inner();
        RunData::require_complete_record(&record, &path)?;
        let traced = request.map(|request| request.ran(schedule.schedevents(), &path, len));
        Ok((
            CompletedScheduleTrial {
                target_met,
                schedule: record.into_global(),
            },
            path,
            traced,
        ))
    }

    /// The report's "Racing accesses" section for a prefix decision at
    /// `decision`: `target` replayed the first `len` events of `schedule` and
    /// `baseline` one fewer, both then continuing at `continuation`. The
    /// stacks come from a replay of the target's continuation. `None` when a
    /// side recorded no trace (`--no-racing-accesses`). `target_met`: whether
    /// the target replay met the target.
    fn prefix_racing_accesses(
        &self,
        decision: usize,
        (schedule, len, continuation): (&PreemptionRecord, usize, MaybeTimeslice),
        target: Option<TracedReplay>,
        baseline: Option<TracedReplay>,
        target_met: bool,
    ) -> Option<RacingAccesses> {
        let (target, baseline) = (target?, baseline?);
        // `continued_record` refuses a record whose callback checkpoints do
        // not attach to its events (UNATTACHED_CHECKPOINTS): a replay then
        // fails with that refusal, never replaying a bare schedule.
        let record = continued_record(schedule, len, &target.realized, &target.realized_path)
            .map_err(|error| format!("{error:#}"));
        // Where each run recorded the schedule a candidate's check edits.
        let realized = [
            ("target", target.realized_path.clone()),
            ("baseline", baseline.realized_path.clone()),
        ];
        let racing = self.racing_accesses(
            decision,
            target,
            baseline,
            target_met,
            |runname| {
                let record = record
                    .as_ref()
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                self.prefix_trial(runname, record, record.schedevents().len(), continuation)
            },
            // A candidate's check replays a schedule of the run it checks
            // under the callback checkpoints of that run's prefix, up to where
            // the edit starts, then continues as the search's trials did.
            |runname, role, edited, unchanged, replay| {
                let edited_from = realized
                    .iter()
                    .find_map(|(side, path)| (*side == role).then_some(path.as_path()))
                    .unwrap_or_else(|| Path::new(runname));
                let edited = continued_record(
                    schedule,
                    check_prefix_len(len, role, unchanged),
                    edited,
                    edited_from,
                )?;
                self.prefix_trial(runname, &edited, replay, continuation)
            },
        );
        Some(racing)
    }

    /// The report of `--prefix-search`: the stacks of the two threads that
    /// could run at the deciding event.
    pub fn phase6_record_prefix_outputs(&self, found: PrefixDecision) -> anyhow::Result<Report> {
        let timeslice = found.continuation;
        self.phase6_record_prefix_outputs_with(
            found,
            |runname, schedule, len, position, trace_from| {
                self.traced_prefix_stack(runname, schedule, len, position, timeslice, trace_from)
            },
            |schedule, len, trace| {
                self.prefix_continuation(
                    "final_baseline_schedule",
                    schedule,
                    len,
                    timeslice,
                    trace,
                    |run| self.launch_prefix_trial(run, RunData::launch),
                )
            },
        )
    }

    fn phase6_record_prefix_outputs_with<S, C>(
        &self,
        found: PrefixDecision,
        mut stack: S,
        continuation: C,
    ) -> anyhow::Result<Report>
    where
        S: FnMut(
            &str,
            &PreemptionRecord,
            usize,
            usize,
            Option<u64>,
        ) -> anyhow::Result<(bool, Option<PrettyBacktrace>, Option<TracedReplay>)>,
        C: FnOnce(
            &PreemptionRecord,
            usize,
            Option<(&'static str, u64)>,
        )
            -> anyhow::Result<(CompletedScheduleTrial, PathBuf, Option<TracedReplay>)>,
    {
        let PrefixDecision {
            schedule,
            decision,
            continuation: timeslice,
        } = found;
        let trace_from = self.race_trace_from(decision);
        let (target_met, chosen, target_trace) = stack(
            "final_target_for_stacktraces",
            &schedule,
            decision + 1,
            decision,
            trace_from,
        )?;
        if !target_met {
            bail!(
                "final target prefix replay did not meet the target criteria; no opposite-outcome \
                 pair was established. Inspect final_target_for_stacktraces diagnostics and \
                 stabilize replay before retrying analysis."
            );
        }
        // The thread the scheduler runs at event #decision once the prefix
        // ends. A stack is requested by a position of the replayed schedule,
        // so the prefix trial without event #decision runs first with its
        // schedule recorded, and the stack is taken from a replay of that
        // schedule's first decision + 1 events.
        let (completed, continued_path, mut baseline_trace) = continuation(
            &schedule,
            decision,
            trace_from.map(|from| ("baseline", from)),
        )?;
        if completed.target_met {
            bail!(
                "final baseline prefix continuation met the target criteria; no opposite-outcome \
                 pair was established. Inspect final_baseline_schedule diagnostics and stabilize \
                 replay before retrying analysis."
            );
        }
        let continued = completed.schedule;
        if continued
            .get(decision)
            .is_some_and(|event| event.dettid == schedule.schedevents()[decision].dettid)
            && let Some(report) = self.report_timing_decision(
                &schedule,
                decision,
                timeslice,
                (target_met, &chosen),
                (&continued, &continued_path, &mut baseline_trace),
            )?
        {
            return Ok(report);
        }
        let instead = if continued.len() > decision {
            let baseline = continued_record(&schedule, decision, &continued, &continued_path)?;
            let (baseline_met, instead, _) = stack(
                "final_baseline_for_stacktraces",
                &baseline,
                decision + 1,
                decision,
                None,
            )?;
            if baseline_met {
                bail!(
                    "final baseline stack replay met the target criteria; no opposite-outcome \
                     pair was established. Inspect final_baseline_for_stacktraces diagnostics \
                     and stabilize replay before retrying analysis."
                );
            }
            instead
        } else {
            eprintln!(
                ":: {} the run without event #{decision} ended after {} events, so no thread \
                 runs there instead and stack2 is empty",
                "WARNING:".yellow().bold(),
                continued.len()
            );
            None
        };

        let racing_accesses = self.prefix_racing_accesses(
            decision,
            (&schedule, decision + 1, timeslice),
            target_trace,
            baseline_trace,
            target_met,
        );

        let report = self.prefix_report(
            schedule.schedevents(),
            decision,
            &continued,
            ([chosen, instead], racing_accesses),
            timeslice,
        );
        println!("{}", prefix_report_banner(&report));
        println!("{}", report.header);
        Self::print_stack(
            "stack1",
            report
                .critical_event1
                .as_ref()
                .map_or(&None, |event| &event.stack),
        );
        Self::print_stack(
            "stack2",
            report
                .critical_event2
                .as_ref()
                .map_or(&None, |event| &event.stack),
        );
        Self::print_racing_accesses(&report.racing_accesses);
        println!("\n{}", report.additional_context);
        Ok(report)
    }

    /// Assemble the prefix report from the search and stack-trace results.
    fn prefix_report(
        &self,
        schedule: &[SchedEvent],
        decision: usize,
        continued: &[SchedEvent],
        ([chosen, instead], racing_accesses): (
            [Option<PrettyBacktrace>; 2],
            Option<RacingAccesses>,
        ),
        continuation: MaybeTimeslice,
    ) -> Report {
        let mut header = format!(
            "The scheduling decision at event #{decision} of the target schedule decides the \
             outcome.\nReplaying the first {decision} events and then scheduling without chaos \
             {} does not meet the target criteria; replaying event #{decision} too ({}) does \
             ({}).\n",
            describe_continuation(continuation),
            schedule[decision],
            self.display_criteria()
        );
        header.push_str(&stack_pair_sentences(
            schedule[decision].dettid,
            continued.get(decision).map(|event| event.dettid),
        ));
        let context = Self::generate_context(schedule, decision.max(1), self.execution_context);

        Report {
            verdict: Verdict::CriticalEvents,
            header,
            additional_context: context,
            baseline_run: false,
            critical_event1: Some(ReportCriticalEvent {
                event_index: decision,
                stack: chosen,
            }),
            critical_event2: Some(ReportCriticalEvent {
                event_index: decision,
                stack: instead,
            }),
            branch_counter_warning: self.branch_counter_warning.clone(),
            deciding_event: None,
            racing_accesses,
        }
    }

    /// The report of `--prefix-search` when the scheduler runs the same
    /// thread at the deciding event without it (`continued`, recorded at
    /// `continued_path`, is the schedule without it) as the target schedule
    /// does: the decision is how long that thread runs, so the report shows
    /// the thread that runs next in each continuation, and the deciding
    /// thread's own stack (`deciding`, from a trial that met the target or
    /// not) separately. `None` when the continuation with the event does not
    /// bear that out (it does not run the thread there, or the thread runs
    /// exactly as long in both): the usual report follows.
    fn report_timing_decision(
        &self,
        schedule: &PreemptionRecord,
        decision: usize,
        continuation: MaybeTimeslice,
        deciding: (bool, &Option<PrettyBacktrace>),
        without_event: (&[SchedEvent], &Path, &mut Option<TracedReplay>),
    ) -> anyhow::Result<Option<Report>> {
        self.report_timing_decision_with(
            schedule,
            decision,
            continuation,
            deciding,
            without_event,
            (
                |record, len| {
                    self.prefix_continuation(
                        "final_target_schedule",
                        record,
                        len,
                        continuation,
                        self.race_trace_from(decision).map(|from| ("target", from)),
                        |run| self.launch_prefix_trial(run, RunData::launch),
                    )
                },
                |runname, variant, len, position| {
                    self.prefix_stack(runname, variant, len, position, continuation)
                },
            ),
        )
    }

    /// [`Self::report_timing_decision`], with `continue_target` running the
    /// continuation of a record's first `len` events and `stack` each
    /// stack-trace replay (its name, record, length and the event to take
    /// the stack at), both giving completed outcomes or ending the analysis.
    fn report_timing_decision_with<CT, S>(
        &self,
        schedule: &PreemptionRecord,
        decision: usize,
        continuation: MaybeTimeslice,
        (target_met, deciding): (bool, &Option<PrettyBacktrace>),
        (continued, continued_path, baseline_trace): (
            &[SchedEvent],
            &Path,
            &mut Option<TracedReplay>,
        ),
        (continue_target, stack): (CT, S),
    ) -> anyhow::Result<Option<Report>>
    where
        CT: FnOnce(
            &PreemptionRecord,
            usize,
        )
            -> anyhow::Result<(CompletedScheduleTrial, PathBuf, Option<TracedReplay>)>,
        S: Fn(
            &str,
            &PreemptionRecord,
            usize,
            usize,
        ) -> anyhow::Result<(bool, Option<PrettyBacktrace>)>,
    {
        let events = schedule.schedevents();
        let thread = events[decision].dettid;
        let (target_completed, target_continued_path, target_trace) =
            continue_target(schedule, decision + 1)?;
        if !target_completed.target_met {
            bail!(
                "final target prefix continuation did not meet the target criteria; no \
                 opposite-outcome pair was established. Inspect final_target_schedule diagnostics \
                 and stabilize replay before retrying analysis."
            );
        }
        let target_continued = target_completed.schedule;
        let with = slice_from(&target_continued, decision, thread);
        let without = slice_from(continued, decision, thread);
        if with.events == 0 || (with.events, with.branches) == (without.events, without.branches) {
            eprintln!(
                ":: {} without event #{decision} the scheduler runs thread {thread} there too, \
                 but the run with it {}; reporting the two threads at that event instead",
                "WARNING:".yellow().bold(),
                if with.events == 0 {
                    "does not run it there (its continuation was not recorded, or left the \
                     schedule)"
                } else {
                    "runs it for exactly as long"
                }
            );
            return Ok(None);
        }
        // Each continuation replays under the checkpoints of the prefix it
        // continued; one they do not attach to ends the analysis before any
        // stack-trace replay.
        let with_record = continued_record(
            schedule,
            decision + 1,
            &target_continued,
            &target_continued_path,
        )?;
        let without_record = continued_record(schedule, decision, continued, continued_path)?;
        let next_stack = |runname: &str, variant: &PreemptionRecord, slice: &Slice| match slice.next
        {
            Some((index, _)) => stack(runname, variant, index + 1, index),
            None => Ok((false, None)),
        };
        let (next_target_met, stack1) =
            next_stack("final_target_next_for_stacktraces", &with_record, &with)?;
        let (baseline_met, stack2) = next_stack(
            "final_baseline_next_for_stacktraces",
            &without_record,
            &without,
        )?;
        let mut replays = vec![
            ("final_target_for_stacktraces", true, target_met),
            ("final_baseline_next_for_stacktraces", false, baseline_met),
        ];
        if with.next.is_some() {
            replays.push(("final_target_next_for_stacktraces", true, next_target_met));
        }
        require_repeated_outcomes(
            &replays,
            "final_target_next_for_stacktraces and final_baseline_next_for_stacktraces",
        )?;
        let next_index = |variant: &[SchedEvent], slice: &Slice| {
            slice.next.map_or(variant.len(), |(index, _)| index)
        };
        let mut critical = [
            (next_index(&target_continued, &with), stack1),
            (next_index(continued, &without), stack2),
        ];
        // When the threads that run next have the same stack in both
        // continuations, those stacks tell nothing apart; where the deciding
        // thread stops does. Its stacks replace them only when both were
        // captured.
        let mut slice_ends = slice_end_events(
            &with,
            &without,
            [critical[0].1.as_ref(), critical[1].1.as_ref()],
        );
        if let Some(ends) = slice_ends {
            let stack_at = |runname: &str,
                            variant: &PreemptionRecord,
                            end: usize,
                            expected: bool|
             -> anyhow::Result<Option<PrettyBacktrace>> {
                // Even a one-event slice has this continuation's endpoint:
                // its operation batch can differ from the target event's,
                // including after a soft count mismatch in the target
                // continuation itself. Capture it from its own record and
                // check the outcome even when no stack was captured.
                let (observed, captured) = stack(runname, variant, end + 1, end)?;
                require_repeated_outcomes(&[(runname, expected, observed)], runname)?;
                Ok(captured)
            };
            let with_end = stack_at(
                "final_target_slice_end_for_stacktraces",
                &with_record,
                ends[0],
                true,
            )?;
            let without_end = stack_at(
                "final_baseline_slice_end_for_stacktraces",
                &without_record,
                ends[1],
                false,
            )?;
            match (with_end, without_end) {
                (Some(with_end), Some(without_end)) => {
                    critical = [(ends[0], Some(with_end)), (ends[1], Some(without_end))];
                }
                _ => slice_ends = None,
            }
        }
        let [(event1, stack1), (event2, stack2)] = critical;
        let mut header = timing_decision_header(
            decision,
            &events[decision],
            (&with, &without),
            &self.display_criteria(),
            continuation,
            slice_ends,
        );
        if slice_ends.is_some()
            && let (Some(first), Some(second)) = (&stack1, &stack2)
            && same_frames(first, second)
        {
            header.push_str(&format!(
                "Thread {thread} has the same stack at both events: what differs is how far it \
                 gets before the other thread runs, not where it stops.\n"
            ));
        }
        let name = |path: &Path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        header.push_str(&continuation_files_sentence(
            decision,
            &name(&target_continued_path),
            &name(continued_path),
            self.workspace_retention(),
        ));
        let context = format!(
            "With event #{decision} replayed:\n{}\nWithout it:\n{}",
            Self::generate_context(
                &target_continued,
                with.next.map_or(decision.max(1), |(index, _)| index),
                self.execution_context
            ),
            Self::generate_context(
                continued,
                without.next.map_or(decision.max(1), |(index, _)| index),
                self.execution_context
            ),
        );

        println!(
            "\n------------------------------ hermit analyze report ------------------------------"
        );
        if let Some(warning) = &self.branch_counter_warning {
            println!("{warning}");
        }
        println!("{}", header);
        // `target_met` is the stack-trace replay's (final_target_for_stacktraces)
        // while `target_trace` is final_target_schedule's, above: both replay the
        // same first `decision + 1` events of `schedule` and continue at
        // `continuation`, so they agree on whether the target is met.
        let racing_accesses = self.prefix_racing_accesses(
            decision,
            (schedule, decision + 1, continuation),
            target_trace,
            baseline_trace.take(),
            target_met,
        );
        println!("Thread {thread} at the deciding event #{decision}:");
        Self::print_stack("the deciding thread's stack", deciding);
        Self::print_stack("stack1", &stack1);
        Self::print_stack("stack2", &stack2);
        Self::print_racing_accesses(&racing_accesses);
        println!("\n{}", context);

        Ok(Some(Report {
            verdict: Verdict::CriticalEvents,
            header,
            additional_context: context,
            baseline_run: false,
            critical_event1: Some(ReportCriticalEvent {
                event_index: event1,
                stack: stack1,
            }),
            critical_event2: Some(ReportCriticalEvent {
                event_index: event2,
                stack: stack2,
            }),
            branch_counter_warning: self.branch_counter_warning.clone(),
            deciding_event: Some(ReportCriticalEvent {
                event_index: decision,
                stack: deciding.clone(),
            }),
            racing_accesses,
        }))
    }

    /// It's weird if no filter is specified.
    fn has_filters(&self) -> bool {
        self.target_stdout.is_some()
            || self.target_stderr.is_some()
            || self.target_exit_code != ExitStatusConstraint::Any
    }

    /// Is the criteria the most common setting of target = failure = nonzero exit.
    fn is_vanilla_criteria(&self) -> bool {
        self.target_stdout.is_none()
            && self.target_stderr.is_none()
            && self.target_exit_code == ExitStatusConstraint::NonZero
    }

    /// Write analyze's final on-target and baseline schedules, stamped
    /// ([`Self::stamp_trial_record`]) and with the callback checkpoints of
    /// the part each shares with one of `sources`, the search's endpoint
    /// records ([`trial_record_from`](crate::analyze::rundata::trial_record_from)), and say where.
    pub fn write_final_schedules(
        &self,
        failing: Vec<SchedEvent>,
        failing_path: &Path,
        passing: Vec<SchedEvent>,
        passing_path: &Path,
        sources: &[&PreemptionRecord],
    ) {
        self.write_trial_schedule_from(failing, sources, failing_path)
            .unwrap();
        eprintln!(
            "Wrote final on-target ({}) schedule to {}",
            self.display_criteria(),
            failing_path.display()
        );
        self.write_trial_schedule_from(passing, sources, passing_path)
            .unwrap();
        eprintln!(
            "Wrote final baseline (off-target) schedule to {}",
            passing_path.display()
        );
    }

    fn display_criteria(&self) -> String {
        let mut strs: Vec<String> = Vec::new();
        match &self.target_exit_code {
            ExitStatusConstraint::Exact(c) => {
                strs.push(format!("exit code={}", c));
            }
            ExitStatusConstraint::NonZero => {
                strs.push("nonzero exit".to_string());
            }
            ExitStatusConstraint::Any => {}
        }
        if self.target_stdout.is_some() {
            strs.push("matching stdout".to_string());
        }
        if self.target_stderr.is_some() {
            strs.push("matching stderr".to_string());
        }
        strs.join(", ")
    }

    /// Create the workspace in `--tmp-dir` and make it `tmp_dir`, where every
    /// run puts its files. The returned guard removes or keeps it, as
    /// `retention` says, when it is dropped.
    fn phase0_initialize(&mut self, retention: Retention) -> anyhow::Result<Workspace> {
        let workspace = Workspace::create(self.tmp_dir.as_deref(), retention)?;
        // Trials name the workspace by its canonical path, so that a relative
        // --tmp-dir, or one through a symlink, names the same directory inside
        // their container, where only a workspace under host /tmp is bound.
        let tmpdir_path = std::fs::canonicalize(workspace.path()).with_context(|| {
            format!(
                "cannot resolve the workspace {}",
                workspace.path().display()
            )
        })?;
        eprintln!(
            ":: Temp workspace: {}{}",
            tmpdir_path.display(),
            match retention {
                Retention::Remove => " (removed when analyze exits; --keep-tmp keeps it)",
                Retention::Keep { .. } => "",
            }
        );
        self.tmp_dir = Some(tmpdir_path);
        Ok(workspace)
    }

    /// What becomes of analyze's workspace: kept with `--keep-tmp`, with
    /// `--verbose`, whose reproducers name its files, and with `--trial-log`
    /// (or `--guest-log`) above warn, whose per-run logs are in it for the
    /// user to read; removed otherwise.
    fn workspace_retention(&self) -> Retention {
        if self.keep_tmp {
            Retention::Keep {
                because: Some("--keep-tmp"),
            }
        } else if self.verbose {
            Retention::Keep {
                because: Some("--verbose"),
            }
        } else if let Some(option) = self.trial_log_files_option() {
            Retention::Keep {
                because: Some(option),
            }
        } else {
            Retention::Remove
        }
    }

    /// Create our workspace and verify the input run matches the criteria, or find one that does.
    /// Returns the results of the target run.
    fn phase1_establish_target_run(&mut self) -> Result<RunData, Error> {
        let run1data = RunData::new_run1_target(self, "run1_target".to_string())?;
        eprintln!(
            // `to_repro` is already a complete `hermit ... run ...` command.
            ":: {} {}",
            "Studying target execution: ".yellow().bold(),
            run1data.to_repro()
        );

        // Run the target, from its seed or by replaying --run1-preemptions, and
        // record the preemptions and schedule the search works from. A replay
        // of the record is the target run like a seed's: it is measured for the
        // trial bound, and --selfcheck compares its log.
        let mut run1data = run1data.with_preemption_recording();
        run1data.launch()?;
        self.record_endpoint(&run1data);

        if !run1data.is_a_match() {
            if let Some(record) = &self.run1_preemptions {
                bail!(
                    "the replay of --run1-preemptions {} did not meet the target criteria ({}). \
                     A record replays the same only under the run arguments it was recorded \
                     with (after `--`), --seed included: it sets the guest's random and fuzz \
                     seeds and is not stored in the record. --search does not apply to a record; \
                     to search, pass --run1-seed.",
                    record.display(),
                    self.display_criteria()
                );
            }
            let target = self.after_a_first_run_that_missed(|| self.do_search())?;
            self.record_endpoint(&target);
            Ok(target)
        } else if self.has_filters() {
            eprintln!(
                ":: {}",
                format!(
                    "First run matched target criteria ({}).",
                    self.display_criteria()
                )
                .green()
                .bold(),
            );
            Ok(run1data)
        } else {
            eprintln!(":: {}", "WARNING: run without any --filter arguments, so accepting ALL runs. This is probably not what you wanted.".red().bold());
            Ok(run1data)
        }
    }

    /// Reduce the set of preemptions needed to match the criteria.
    ///
    /// Takes the input (non-minimized) preemptions as a file path and returns the minimized
    /// preemptions as a data structure in memory.
    ///
    /// # Returns
    /// - The on-target run with minimized schedule.
    fn phase2_minimize(
        &self,
        global: &GlobalOpts,
        mut last_run: RunData,
    ) -> anyhow::Result<RunData> {
        if self.minimize {
            // In this scenario we need to work with preemptions.

            let preempts_path = last_run.preempts_path_out();
            let (min_pr, min_pr_path, min_log_path) = self.minimize(preempts_path, global)?;
            eprintln!(
                ":: {}\n {}",
                "Successfully minimized to these critical interventions:"
                    .green()
                    .bold(),
                truncated(1000, serde_json::to_string(&min_pr).unwrap())
            );

            // TEMP: construct a RunData post-facto, until we finish the minimize overhaul:
            let runname = "minimized".to_string();
            let min_run = RunData::from_minimize_output(
                self,
                runname,
                last_run.runopts.clone(),
                min_pr,
                min_pr_path,
                min_log_path,
            );
            Ok(min_run)
        } else {
            // In this scenario we only care about event traces, and never really need to work with
            // preemptions.  Still, we'll need to do another run to record the trace.
            Ok(last_run)
        }
    }

    /// A weaker log difference that does not expect certain lines to be conserved in preemption replay.
    fn log_diff_preemption_replay(
        &self,
        global: &GlobalOpts,
        run1_log_path: &Path,
        run2_log_path: &Path,
    ) -> ExitStatus {
        if self.verbose {
            yellow_msg("[comparing] with log-diff command:");
            eprintln!(
                "    hermit log-diff --record-envelope=all-records-v1 \
                 --ignore-lines=CHAOSRAND {} {}",
                run1_log_path.display(),
                run2_log_path.display(),
            );
        }
        let mut ldopts = LogDiffCLIOpts::new(run1_log_path, run2_log_path);
        ldopts.more.ignore_lines = vec!["CHAOSRAND".to_string(), "SCHEDRAND".to_string()];
        ldopts.main(global)
    }

    /// Optionally do an extra run to verify that preemptions replay and yield the exact same
    /// execution.a
    pub fn phase3_strict_preempt_replay_check(
        &self,
        global: &GlobalOpts,
        min_run: &mut RunData,
    ) -> Result<(), Error> {
        min_run.require_complete_output()?;
        let min_log_path = &min_run.log_path().unwrap().to_path_buf();
        let min_preempts_path = min_run.preempts_path_out().to_path_buf();

        if self.selfcheck {
            yellow_msg("[selfcheck] Verifying target run preserved under preemption-replay");

            let runname = "run1b_selfcheck";
            let run1b = RunData::new_run1_replay(self, runname.to_string(), &min_run.runopts)?;
            let mut run1b = self
                .replaying_the_target_record(run1b, min_preempts_path.clone())
                .with_preemption_recording();
            eprintln!("    {}", run1b.to_repro());

            run1b.launch()?;
            yellow_msg("[selfcheck] Comparing output from additional run (run1 vs run1b)");
            let run1b_log_path = run1b.log_path().unwrap();
            let status = self.log_diff_preemption_replay(global, min_log_path, run1b_log_path);

            let matched = run1b.is_a_match();
            let run1b_preempts_path = run1b.preempts_path_out();
            selfcheck_conclusion(matched, status.success(), || {
                preempt_files_equal(&min_preempts_path, run1b_preempts_path)
                    .then_some(())
                    .ok_or_else(|| {
                        format!(
                            "The preemptions recorded by the additional run did not match the \
                             preemptions replayed (no fixed point): {} vs {}",
                            min_preempts_path.display(),
                            run1b_preempts_path.display(),
                        )
                    })
            })?;

            eprintln!(
                ":: {}",
                "Identical executions confirmed between target run and its preemption-based replay."
                    .green()
                    .bold()
            );
        }
        Ok(())
    }

    /// Once we have the target MATCHING run in hand (usually crashing/failing), we need to
    /// determine which baseline, non-matching run to use. Then we need to extract the schedule from
    /// it.
    ///
    /// Returns a path to a file containing recorded schedule events for the baseline run.
    pub fn phase4_choose_baseline_sched_events(
        &mut self,
        matching_pr: PreemptionRecord,
    ) -> anyhow::Result<RunData> {
        if self.minimize {
            // Enforced by the clap conflicts_with annotations:
            assert!(self.run2_seed.is_none());
            assert!(self.run2_preemptions.is_none());
            assert!(self.run2_schedule.is_none());

            // If we're minimizing, then we know that ALL interventions in the schedule are critical.
            // Thus omitting any of them is sufficient to exit the target schedule space.
            // Omitting the last one should yield the lowest distance match/non-match schedule pair.
            let newrun = knock_out_until_missed(matching_pr, |ix, knocked_out| {
                let runname = format!("run2_baseline_try{:03}", ix);
                let (still_matching, newrun) =
                    self.save_nearby_non_matching_sched_events(&runname, knocked_out)?;
                Ok((!still_matching).then_some(newrun))
            })?;
            self.record_endpoint(&newrun);
            Ok(newrun)
        } else {
            let runname = "run2_baseline".to_string();
            let mut newrun = RunData::new_baseline(self, runname)?.with_preemption_recording();

            // Tweak the runopts according to several different scenarios:
            let ro = &mut newrun.runopts;
            let from_where;
            if let Some(seed) = self.run2_seed {
                // Replay from seed to record schedule.
                ro.det_opts.det_config.seed = seed;
                from_where = "--run2-seed";
            } else if let Some(path) = &self.run2_preemptions {
                // Replay from preemption record to record schedule.
                ro.det_opts.det_config.replay_preemptions_from = Some(path.clone());
                // TODO: if the file already contains a schedule, we don't need to rerun it.
                // Unless selfcheck is specified.
                from_where = "--run2-preemptions";
            } else if let Some(path) = &self.run2_schedule {
                if self.selfcheck {
                    // TODO: Don't trust the recorded schedule is a baseline, replay and check it.
                }
                let runname = "from_existing_run2_schedule".to_string();
                let fakerun = RunData::from_schedule_trace(self, runname, ro.clone(), path.clone());
                return Ok(fakerun);
            } else {
                from_where = "non-chaos (0 extra preemptions) run";

                // Otherwise we just assume that a *baseline* (non-chaos) run will do the trick.
                ro.det_opts.det_config.chaos = false;
            }
            newrun.launch()?;
            self.record_endpoint(&newrun);
            eprintln!(
                ":: Recorded schedule from {} as baseline run ({})",
                from_where,
                newrun
                    .sched_path_out()
                    .file_name()
                    .unwrap()
                    .to_string_lossy(),
            );
            if newrun.is_a_match() {
                Err(AnalyzePrecondition::baseline_not_reproduced(
                    "Expectations not met... baseline run matched target criteria when it should \
                     not.",
                )
                .into())
            } else {
                eprintln!("Good: baseline run does not match criteria (e.g. pass not fail).");
                Ok(newrun)
            }
        }
    }

    /// Perform the binary search through schedule-space, identifying critical events.
    ///
    /// Each trial replays its schedule with the callback checkpoints of the
    /// part it shares with the `target` or `baseline` record
    /// ([`trial_record_from`](crate::analyze::rundata::trial_record_from)):
    /// an endpoint replayed whole keeps all of them, and a spliced schedule
    /// keeps those its unchanged prefix reaches, so its slices end where the
    /// recording's did up to the first edit.
    pub fn phase5_bisect_traces(
        &self,
        target: &PreemptionRecord,
        baseline: &PreemptionRecord,
    ) -> anyhow::Result<CriticalSchedule> {
        self.phase5_bisect_traces_with(target.schedevents(), baseline.schedevents(), |i, sched| {
            let runname = format!("bisect_round_{:0wide$}", i, wide = 3);
            let mut newrun = RunData::new_baseline(self, runname)?
                .with_schedule_replay() // Default input path
                .with_schedule_recording(); // Default output path
            // Prepare the next synthetic schedule on disk:
            self.write_trial_schedule_from(
                sched.to_owned(),
                &[target, baseline],
                newrun.sched_path_in(),
            )?;

            if self.verbose {
                eprintln!(
                    ":: {}, repro command{}:\n    {}",
                    format!("Testing execution during search (#{})", i)
                        .yellow()
                        .bold(),
                    if self.keep_tmp {
                        ""
                    } else {
                        " (its schedules are removed after the trial; --keep-tmp keeps them)"
                    },
                    newrun.to_repro(),
                );
            }

            newrun
                .launch()
                .map_err(|error| incomplete_trial(error, &newrun.root_path()))?;
            let path = newrun.sched_path_out().to_path_buf();
            let record = PreemptionReader::try_new(&path)?.into_inner();
            RunData::require_complete_record(&record, &path)?;
            // Only a completed trial contributes its realized schedule and jumps.
            self.time_skip_sources.lock().unwrap().remember(&record);
            let completed = CompletedScheduleTrial {
                target_met: newrun.is_a_match(),
                schedule: record.into_global(),
            };
            newrun.discard_schedules();
            Ok(completed)
        })
    }

    /// Run the production trial adapter and search with the supplied launcher.
    /// The launcher returns an observed outcome and trace only after completion.
    fn phase5_bisect_traces_with(
        &self,
        target: &[SchedEvent],
        baseline: &[SchedEvent],
        mut launch: impl FnMut(usize, &[SchedEvent]) -> anyhow::Result<CompletedScheduleTrial>,
    ) -> anyhow::Result<CriticalSchedule> {
        let mut i = 0;
        // Unfinished trials have no observed outcome or realized trace. Keep
        // the typed error and the last completed endpoints by ending the search.
        let test_fn = |sched: &[SchedEvent]| -> TrialOutcome {
            i += 1;
            let completed = launch(i, sched)?;
            if completed.target_met {
                eprintln!(" => Target condition ({})", self.display_criteria());
            } else {
                eprintln!(" => Baseline condition (usually absence of crash)");
            }

            Ok((!completed.target_met, completed.schedule))
        };

        let target = target.to_vec(); // TODO: have search_for_critical_schedule borrow only.
        let baseline = baseline.to_vec();
        let cfg = self.bisect_config();
        let crit = search_for_critical_schedule(test_fn, baseline, target, &cfg)?;
        eprintln!(
            "Critical event of final on-target schedule is {}",
            crit.critical_event_index
        );
        if let Some(warning) = critical_pair_caveat(&crit) {
            eprintln!(":: {} {warning}", "WARNING:".yellow().bold());
        }
        Ok(crit)
    }

    /// Bisect a known passing/failing schedule pair and report the critical event ordering.
    pub fn bisect_schedule_pair(
        &mut self,
        good: PreemptionRecord,
        bad: PreemptionRecord,
    ) -> anyhow::Result<ExitStatus> {
        // Bisect keeps its workspace, and says where when it exits.
        let _workspace = self.phase0_initialize(Retention::Keep { because: None })?;
        eprintln!(
            ":: Beginning schedule bisection using good/bad endpoints ({} / {} events)",
            good.schedevents().len(),
            bad.schedevents().len(),
        );

        let critical = self.phase5_bisect_traces(&bad, &good)?;
        let index = critical.critical_event_index;
        if index == 0 || index >= critical.failing_schedule.len() {
            bail!("schedule search returned an invalid critical event index {index}");
        }

        println!("\n---------------- hermit bisect result ----------------");
        println!("Schedule divergence localized at bad event #{index}:");
        println!(
            "  preceding event #{}: {}",
            index - 1,
            critical.failing_schedule[index - 1]
        );
        println!(
            "  critical event  #{index}: {}",
            critical.failing_schedule[index]
        );
        let report = self.phase6_record_outputs(critical, &[&bad, &good], None)?;
        println!("Swapping these adjacent events reproduces the good outcome.");
        if let Some(path) = &self.report_file {
            let text = serde_json::to_string_pretty(&report)?;
            std::fs::write(path, text)?;
            eprintln!(":: Final bisection report written to {}", path.display());
        }

        Ok(ExitStatus::SUCCESS)
    }

    fn generate_context(
        sched: &[SchedEvent],
        critical_event_index: usize,
        window_size: usize,
    ) -> String {
        let mut additional_context = "Execution context for the critical events (*):\n".to_string();
        {
            let start = critical_event_index - 1;
            let start = start - cmp::min(window_size, start);
            let end = cmp::min(critical_event_index + window_size + 1, sched.len());

            for (ix, item) in sched.iter().enumerate().take(end).skip(start) {
                additional_context.push_str(&format!(
                    "  {} {}\n",
                    if ix == critical_event_index || ix == critical_event_index - 1 {
                        "*"
                    } else {
                        " "
                    },
                    item
                ));
            }
        }
        additional_context
    }

    /// Record the schedules on disk as reproducers and report stack-traces of
    /// critical events. `target_run` is the workspace root of the run whose
    /// schedule was the search's target endpoint, if one ran.
    pub fn phase6_record_outputs(
        &self,
        crit: CriticalSchedule,
        sources: &[&PreemptionRecord],
        target_run: Option<&Path>,
    ) -> Result<Report, Error> {
        self.phase6_record_outputs_with(crit, sources, target_run, RunData::launch)
    }

    /// [`Self::phase6_record_outputs`], with `launch` running each
    /// stack-trace replay.
    fn phase6_record_outputs_with(
        &self,
        crit: CriticalSchedule,
        sources: &[&PreemptionRecord],
        target_run: Option<&Path>,
        launch: impl Fn(&mut RunData) -> anyhow::Result<()>,
    ) -> Result<Report, Error> {
        let tmp_dir = self.get_tmp()?;
        let CriticalSchedule {
            failing_schedule,
            passing_schedule,
            critical_event_index,
        } = crit;

        let runname1 = "final_target_for_stacktraces";
        let runname2 = "final_baseline_for_stacktraces";
        let final_failing_path = tmp_dir.join(runname1).with_extension(SCHED_EXT);
        let final_passing_path = tmp_dir.join(runname2).with_extension(SCHED_EXT);

        let passing_context = Self::generate_context(
            &passing_schedule,
            critical_event_index,
            self.execution_context,
        );
        let failing_context = Self::generate_context(
            &failing_schedule,
            critical_event_index,
            self.execution_context,
        );

        self.write_final_schedules(
            failing_schedule.clone(),
            &final_failing_path,
            passing_schedule.clone(),
            &final_passing_path,
            sources,
        );

        let mut header = String::new();
        header.push_str("These two operations, on different threads, are RACING with eachother.\n");
        header.push_str(&format!(
            "The order of events (#{} and #{} in the schedule) determines the program outcome.\n",
            critical_event_index - 1,
            critical_event_index,
        ));
        header.push_str(
            "You must add synchronization to prevent these operations from racing, or give them a different order.\n",
        );

        if self.is_vanilla_criteria() {
            header.push_str(
                "Attached are the stacktraces in the PASSING order, but flipping the order of the two events makes the program FAIL (nonzero exit).\n",
            );
        } else {
            header.push_str(&format!(
                "Attached are the stacktraces in the baseline order, while flipping the order makes the program exhibit the target criteria ({}).\n",
                self.display_criteria()
            ));
        }

        // Both schedules are replayed: the passing schedule's events
        // #index - 1 and #index are the failing schedule's #index and
        // #index - 1, so each racing operation's stack can come from either
        // replay when the other one leaves its schedule first.
        // The schedules agree before event #index - 1, where the racing
        // accesses' window starts.
        let trace_from = self.race_trace_from(critical_event_index - 1);
        let mut baseline = self.replay_for_stacks(
            runname2,
            &final_passing_path,
            &passing_schedule,
            critical_event_index,
            trace_from.map(|from| ("baseline", from)),
            &launch,
        )?;
        let mut target = self.replay_for_stacks(
            runname1,
            &final_failing_path,
            &failing_schedule,
            critical_event_index,
            trace_from.map(|from| ("target", from)),
            &launch,
        )?;
        require_repeated_outcomes(
            &[
                (runname2, false, baseline.met_target),
                (runname1, true, target.met_target),
            ],
            &format!("{runname2} and {runname1}"),
        )?;
        if let Some(change) = target_run.and_then(|run| {
            changed_output(
                run,
                &tmp_dir.join(runname1),
                [
                    ("stdout", self.target_stdout.as_ref()),
                    ("stderr", self.target_stderr.as_ref()),
                ],
            )
        }) {
            eprintln!(":: {} {change}", "WARNING:".yellow().bold());
            header.push_str(&format!("Note: {change}\n"));
        }
        let [stack1, stack2] = merge_stacks(
            &baseline,
            &target,
            critical_event_index,
            [
                passing_schedule[critical_event_index - 1].dettid,
                passing_schedule[critical_event_index].dettid,
            ],
        );
        for (which, stack, event) in [
            ("stack1", &stack1, critical_event_index - 1),
            ("stack2", &stack2, critical_event_index),
        ] {
            match stack {
                Some((_, source)) => header.push_str(&format!("{which} is from {source}.\n")),
                None => {
                    let target_event = 2 * critical_event_index - 1 - event;
                    let reason =
                        missing_stack_reason(which, (event, target_event), &baseline, &target);
                    eprintln!(":: {} {reason}", "WARNING:".yellow().bold());
                    header.push_str(&format!("{reason}\n"));
                }
            }
        }
        let (stack1, stack2) = (
            stack1.map(|(stack, _)| stack),
            stack2.map(|(stack, _)| stack),
        );
        let target_met = target.met_target;
        let racing_accesses = match (target.traced.take(), baseline.traced.take()) {
            (Some(target), Some(baseline)) => {
                let realized = target.realized_path.clone();
                let baseline_realized = baseline.realized_path.clone();
                Some(self.racing_accesses(
                    critical_event_index - 1,
                    target,
                    baseline,
                    target_met,
                    |runname| {
                        Ok(RunData::new_baseline(self, runname.to_string())?
                            .with_schedule_replay_from(realized.clone()))
                    },
                    // A candidate's check replays a schedule of the run it
                    // checks under the callback checkpoints of that run's
                    // recording, up to where the edit starts, then continues
                    // without chaos at the trials' timeslice.
                    |runname, role, edited, unchanged, replay| {
                        let path = if role == "baseline" {
                            &baseline_realized
                        } else {
                            &realized
                        };
                        let recorded = Recording::read(path)
                            .and_then(|recording| recording.into_legacy("a race candidate's check"))
                            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
                        let edited = candidate_check_record(&recorded, unchanged, edited)?;
                        self.prefix_trial(runname, &edited, replay, self.trial_max_timeslice()?)
                    },
                ))
            }
            _ => None,
        };

        // Also print to the screen, the branch-counter caveat first: the
        // report must not read as reproducible where its trials may not
        // have been.
        println!(
            "\n------------------------------ hermit analyze report ------------------------------"
        );
        if let Some(warning) = &self.branch_counter_warning {
            println!("{warning}");
        }
        println!("{}", header);
        Self::print_stack("stack1", &stack1);
        Self::print_stack("stack2", &stack2);
        Self::print_racing_accesses(&racing_accesses);
        println!("\n{}", passing_context);

        if self.verbose {
            println!(
                "\n----------------------- stacks from final on-target run -----------------------"
            );
            Self::print_stack(
                &format!("event #{} (stack2's operation)", critical_event_index - 1),
                &target.stacks[0],
            );
            Self::print_stack(
                &format!("event #{critical_event_index} (stack1's operation)"),
                &target.stacks[1],
            );
            println!("\n{}", failing_context);
        }

        Ok(Report {
            verdict: Verdict::CriticalEvents,
            header,
            additional_context: passing_context,
            baseline_run: true,
            critical_event1: Some(ReportCriticalEvent {
                event_index: critical_event_index - 1,
                stack: stack1,
            }),
            critical_event2: Some(ReportCriticalEvent {
                event_index: critical_event_index,
                stack: stack2,
            }),
            branch_counter_warning: self.branch_counter_warning.clone(),
            deciding_event: None,
            racing_accesses,
        })
    }

    /// Print the report's "Racing accesses" block, when there is one.
    fn print_racing_accesses(racing: &Option<RacingAccesses>) {
        if let Some(racing) = racing {
            println!("\n{}", render_racing_accesses(racing));
        }
    }

    fn print_stack(which: &str, m_stack: &Option<PrettyBacktrace>) {
        if let Some(s) = m_stack {
            println!("{}", s);
        } else {
            println!(" ({} not available) ", which);
        }
    }

    /// Everything `main` does before phase 0 creates the workspace: a
    /// refusal here starts nothing.
    fn prepare(&mut self, global: &GlobalOpts) -> anyhow::Result<()> {
        self.apply_global(global);
        self.refuse_unsupervised_log_cap()?;
        self.refuse_options_trials_do_not_apply()?;
        self.refuse_outputs_trials_overwrite()?;
        self.refuse_unqualified_trial_timeout()?;
        self.refuse_unenforceable_trial_timeout()?;
        self.install_trial_pmu_config()?;
        self.admit_branch_counter(&mut crate::app::tracing::write_stderr_diagnostic)?;
        if let Some(notice) =
            self.small_trial_max_timeslice_notice(crate::app::run::installed_skid_margin)
        {
            crate::app::tracing::write_stderr_diagnostic(&notice);
        }
        // `--run1-schedule` and `--run2-schedule` parse but have no
        // implementation. Refuse them before any workspace or trial, rather
        // than panicking here.
        let unimplemented: Vec<&str> = [
            (self.run1_schedule.is_some(), "--run1-schedule"),
            (self.run2_schedule.is_some(), "--run2-schedule"),
        ]
        .into_iter()
        .filter_map(|(given, option)| given.then_some(option))
        .collect();
        if !unimplemented.is_empty() {
            return Err(
                anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
                    "hermit analyze does not implement {}. Identify a run by a preemption record \
                 (--run1-preemptions or --run2-preemptions, written by `hermit run \
                 --record-preemptions-to`) or by a seed (--run1-seed or --run2-seed).",
                    unimplemented.join(" or ")
                )),
            );
        }
        // The trials that replay these records start from the epoch they were
        // recorded under (https://github.com/rrnewton/hermit/issues/3870).
        let run1 = self.run1_preemptions.clone();
        let run2 = self.run2_preemptions.clone();
        let recordings: Vec<(&str, &Path)> = [
            ("--run1-preemptions", run1.as_deref()),
            ("--run2-preemptions", run2.as_deref()),
        ]
        .into_iter()
        .filter_map(|(option, path)| Some((option, path?)))
        .collect();
        Self::refuse_recordings_numbered_apart(&recordings)?;
        self.adopt_recorded_epoch(&recordings)?;
        // Every trial charges expired timed waits as the target did.
        self.adopt_expired_wait_charge(run1.as_deref())?;
        // Every trial registers queued sleeps as the target did.
        self.adopt_sleep_registration(run1.as_deref())?;
        // Every trial runs under the timeslice its recordings drew.
        self.adopt_recorded_timeslice(&recordings)?;
        self.refuse_unsound_continuation_timeslice()?;
        self.refuse_recordings_under_another_pipe_wake_latency(&recordings)?;
        self.prepare_trial_guest_ids(&recordings)
    }

    /// Take the settings `analyze` inherits from the global options: every
    /// trial runs on the globally selected backend
    /// (`hermit --backend <BACKEND> analyze ...`) and charges the invocation's
    /// shared `--max-log-bytes` budget.
    pub fn apply_global(&mut self, global: &GlobalOpts) {
        self.backend = global.backend;
        self.max_log_bytes = global.max_log_bytes;
        self.log_budget = global.log_budget();
    }

    pub fn main(&mut self, global: &GlobalOpts) -> anyhow::Result<ExitStatus> {
        // Every trial reads the one property snapshot this invocation takes
        // (`RunOpts::take_property_snapshot`); it goes when analyze ends.
        let _remove_property_snapshot = crate::app::property_snapshot::RemoveOnExit;
        self.prepare(global)?;
        let _watchdog = self.max_memory.enforce("hermit analyze")?;
        // From here SIGTERM, SIGINT and SIGHUP end the run in progress and
        // the analysis, which then removes what it created like any other
        // ending, and the process dies of the signal (`raise_or_exit`).
        let stop_signals = StopSignals::install()?;
        let mut workspace = self.phase0_initialize(self.workspace_retention())?;
        let result = self.analyze(global);
        // An analysis that ends with an error keeps its workspace: the error
        // can name trial files and diagnostics in it (`incomplete_trial`, and
        // the checks that the final replays repeat the search's outcomes).
        // One a stop signal ended removes what it created, as before.
        if let Err(error) = &result
            && error.downcast_ref::<Stopped>().is_none()
            && stop::check().is_ok()
        {
            workspace.keep("the error above can name files in it");
        }
        drop(workspace);
        // Restored before the last check, so a signal from here on has its
        // default action rather than being recorded too late to act on.
        drop(stop_signals);
        let Err(Stopped(signal)) = stop::check() else {
            return result;
        };
        match result {
            Err(error) if error.downcast_ref::<Stopped>().is_none() => note(&format!(
                ":: hermit analyze stopped on {signal}, after this error: {error:#}"
            )),
            _ => note(&format!(":: hermit analyze stopped on {signal}.")),
        }
        Ok(ExitStatus::Signaled(signal, false))
    }

    /// The analysis, in the workspace phase 0 created.
    fn analyze(&mut self, global: &GlobalOpts) -> anyhow::Result<ExitStatus> {
        {
            let dummy = RunData::new_baseline(self, "dummy".to_string())?;
            if !dummy.runopts.det_opts.det_config.chaos {
                eprintln!(
                    ":: {} You may want to turn it on explicitly, along with a --max-timeslice that works well for this program.",
                    "WARNING: implicitly activating --chaos.".yellow().bold()
                );
            }
        }

        let run1data = self.phase1_establish_target_run()?;

        let mut min_run = self.phase2_minimize(global, run1data)?;
        self.phase3_strict_preempt_replay_check(global, &mut min_run)?;

        // The record the target endpoint replays, and the one --minimize's
        // baseline knocks a preemption out of, is the record that met the
        // target: a normalized copy renumbers its priorities across the
        // scheduler's fixed ones (the default priority, among others), and
        // can run another schedule. Normalization is written for reading
        // only; analyze keeps using the observed record.
        let (normalized_path, normalized) = min_run.write_normalized_preempts();
        eprintln!(
            ":: {}\n {}",
            format!(
                "Normalized for reading ({}; analyze uses {}), that preemption record \
                 becomes:",
                normalized_path.file_name().unwrap().to_string_lossy(),
                min_run
                    .preempts_path_out()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
            )
            .green()
            .bold(),
            truncated(
                1000,
                serde_json::to_string_pretty(&normalized.clone_preemptions_only()).unwrap()
            )
        );
        let target_preempts = min_run.preempts_out().clone();

        // One endpoint of the bisection search:
        let mut target_endpoint_run = self.save_final_target_sched_events(min_run)?;

        if self.prefix_search {
            return self.prefix_search_and_report(&mut target_endpoint_run);
        }

        // The other endpoint of the bisection search:
        // What we thought was the final_pr can change here:
        let mut baseline_endpoint_run =
            self.phase4_choose_baseline_sched_events(target_preempts)?;

        let baseline_sched_name = baseline_endpoint_run.sched_out_file_name();
        let target_sched_name = target_endpoint_run.sched_out_file_name();
        target_endpoint_run.require_complete_output()?;
        baseline_endpoint_run.require_complete_output()?;
        let target = target_endpoint_run.preempts_out().clone();
        let baseline = baseline_endpoint_run.preempts_out().clone();

        eprintln!(
            ":: {} (event lengths {} / {}): {} {}",
            "Beginning bisection using endpoints".yellow().bold(),
            baseline.schedevents().len(),
            target.schedevents().len(),
            baseline_sched_name,
            target_sched_name,
        );
        let crit_sched = match own_bisection_end(self.phase5_bisect_traces(&target, &baseline))? {
            OwnBisectionEnd::Critical(crit_sched) => crit_sched,
            OwnBisectionEnd::SearchPrefixes(error) => {
                eprintln!(
                    ":: {} {error:#}\n:: Searching within the failing schedule instead \
                     (--prefix-search).",
                    "No single swap separates the endpoints:".yellow().bold(),
                );
                return self.prefix_search_and_report(&mut target_endpoint_run);
            }
        };

        let target_run = target_endpoint_run.root_path();
        let report =
            self.phase6_record_outputs(crit_sched, &[&target, &baseline], Some(&target_run))?;
        self.write_report_and_exit(report)
    }

    /// `--prefix-search` over the target endpoint's schedule, and its report.
    fn prefix_search_and_report(
        &self,
        target_endpoint_run: &mut RunData,
    ) -> anyhow::Result<ExitStatus> {
        let target = target_endpoint_run.preempts_out().clone();
        eprintln!(
            ":: {} ({} events): {}",
            "Beginning prefix search within the target schedule"
                .yellow()
                .bold(),
            target.schedevents().len(),
            target_endpoint_run.sched_out_file_name(),
        );
        let searched = self.phase5_prefix_search(&target);
        self.finish_prefix_search(searched)
    }

    /// Report what the prefix search `searched` found: the deciding event, or
    /// a verdict that names none, which completes the analysis too. Any other
    /// error ends it.
    pub(super) fn finish_prefix_search(
        &self,
        searched: anyhow::Result<PrefixDecision>,
    ) -> anyhow::Result<ExitStatus> {
        let decision = match searched {
            Ok(decision) => decision,
            Err(error) => return self.write_verdict_and_exit(&error.downcast::<AnalyzeVerdict>()?),
        };
        let report = self.phase6_record_prefix_outputs(decision)?;
        self.write_report_and_exit(report)
    }

    /// What follows a first run that missed the target: `--search` (`search`),
    /// or the end of the analysis, its input not giving it a target run.
    pub(super) fn after_a_first_run_that_missed(
        &self,
        search: impl FnOnce() -> anyhow::Result<RunData>,
    ) -> anyhow::Result<RunData> {
        if self.search {
            eprintln!(
                ":: {}",
                "First run did not match target criteria; now searching for a matching run..."
                    .red()
                    .bold()
            );
            search()
        } else {
            Err(AnalyzePrecondition::target_not_reproduced(
                "FAILED. The run did not match the target criteria. Try --search.",
            )
            .into())
        }
    }

    /// Report `verdict`, a completed analysis that names no event: print it
    /// as analyze prints any report, write it to `--report-file`, if given,
    /// and return the exit status of a successful analysis, with the class
    /// line on stderr.
    pub(super) fn write_verdict_and_exit(
        &self,
        verdict: &AnalyzeVerdict,
    ) -> anyhow::Result<ExitStatus> {
        println!(
            "\n------------------------------ hermit analyze report ------------------------------"
        );
        println!("{verdict}");
        self.write_report_file(&Report {
            verdict: verdict.verdict(),
            header: verdict.to_string(),
            additional_context: String::new(),
            baseline_run: false,
            critical_event1: None,
            critical_event2: None,
            deciding_event: None,
            racing_accesses: None,
        });
        eprintln!("{}", verdict.class_line());
        self.success_status()
    }

    /// Write `report` to `--report-file`, if given, and return the exit status
    /// of a successful analysis.
    fn write_report_and_exit(&self, report: Report) -> anyhow::Result<ExitStatus> {
        self.write_report_file(&report);
        self.success_status()
    }

    /// Write `report` to `--report-file`, if given.
    fn write_report_file(&self, report: &Report) {
        if let Some(path) = &self.report_file {
            let txt = serde_json::to_string(report).unwrap();
            std::fs::write(path, txt).expect("Unable to write report file");
            eprintln!(
                ":: {}\n {}",
                "Final analysis report written to:".green().bold(),
                path.display()
            );
        }
    }

    /// The exit status of a successful analysis: 0, or `--success-exit-code`.
    fn success_status(&self) -> anyhow::Result<ExitStatus> {
        self.success_exit_code
            .map_or(Ok(ExitStatus::SUCCESS), |exit_code| {
                Ok(ExitStatus::Exited(exit_code))
            })
    }

    /// This extra run, to record the schedule, thus converting Preemptions to a full Schedule
    /// would be unnecessary if we recorded that each time as we minimize.
    ///
    /// Argument: the last on-target run that was preemption based.
    fn save_final_target_sched_events(&mut self, mut last_run: RunData) -> anyhow::Result<RunData> {
        last_run.require_complete_output()?;
        let pr = last_run.preempts_out();
        if pr.contains_schedevents() {
            let path = last_run.preempts_path_out();
            // This run already has sched events recorded... no extra run necessary.
            yellow_msg(&format!(
                "Using the last run as the target endpoint ({} contains sched events)",
                path.file_name().unwrap().to_string_lossy()
            ));
            self.record_endpoint(&last_run);
            Ok(last_run)
        } else {
            // Verify that the new preemption record does in fact now cause a matching execution,
            // and rerecord during this verification with full recording that include sched events
            yellow_msg(
                "Verify target endpoint preemption record causes criteria to hold and record sched events",
            );
            let runname = "verify_target_endpoint";
            let newrun = RunData::new(self, runname.to_string(), last_run.runopts.clone());
            let mut newrun = self
                .replaying_the_target_record(newrun, last_run.preempts_path_out().to_path_buf())
                .with_schedule_recording();
            newrun.launch()?;
            self.record_endpoint(&newrun);
            eprintln!("    {}", newrun.to_repro());
            if !newrun.is_a_match() {
                return Err(AnalyzePrecondition::target_not_reproduced(
                    "Final preemption record still does not match target criteria",
                )
                .into());
            }
            Ok(newrun)
        }
    }

    // Returns the record, with one knockout, if it still satisfies the criteria that we want it not to.
    fn save_nearby_non_matching_sched_events(
        &self,
        runname: &str,
        non_matching_preempts: &PreemptionRecord,
    ) -> anyhow::Result<(bool, RunData)> {
        // Given preemptions that hermit analyze has determined are critical to match the criteria
        // (most commonly, a failing execution), removing the last critical preemption
        // ([`knock_out_latest`]) should cause the minimal execution change to now no longer match
        // the criteria (most commonly, an execution that now passes).

        // Validate the preemption record
        if let Err(e) = non_matching_preempts.validate() {
            bail!(
                "Hermit analyzer produced corrupt nearby non-matching preemption record, cannot proceed.\n\
                Error: {}\n\n\
                Corrupt record: {}",
                e,
                non_matching_preempts,
            );
        }

        let mut newrun = RunData::new_baseline(self, runname.to_string())?
            .with_preempts_in(non_matching_preempts.clone())
            .with_schedule_recording();

        // Verify that the new preemption record does in fact now cause a non-matching execution,
        // and rerecord during this verification with full recording that include sched events
        yellow_msg(
            "Verify preemption record *without* latest critical preempt causes criteria non-match. Also record sched events.",
        );
        newrun.launch()?;
        eprintln!("    {}", newrun.to_repro());
        if newrun.is_a_match() {
            eprintln!(
                "{}",
                ":: New preemption record still matches criteria! Attempting further knockouts.."
                    .red()
                    .bold(),
            );
            Ok((true, newrun))
        } else {
            Ok((false, newrun))
        }
    }

    /// Search for a target run. Return the run when found.
    fn do_search(&self) -> Result<RunData, Error> {
        self.search_rounds(|round, sched_seed| self.launch_search(round, sched_seed))
    }

    /// The `--search` loop: one chaos trial per round, each with the next
    /// schedule seed from the search RNG, until a trial matches. A trial that
    /// cannot finish (for example, one that `--max-log-bytes` ended) ends the
    /// search with its error unchanged, so main still reports it by its class.
    fn search_rounds(
        &self,
        mut launch: impl FnMut(u64, u64) -> Result<Option<RunData>, Error>,
    ) -> Result<RunData, Error> {
        let search_seed = self.analyze_seed.unwrap_or_else(|| {
            let seed: u64 = rand::random();
            yellow_msg(&format!("WARNING: performing --search with system randomness, use --analyze-seed={} to repro.", seed));
            seed
        });
        yellow_msg(&format!("Failure search using RNG seed {}", search_seed));
        let mut rng = Pcg64Mcg::seed_from_u64(search_seed);

        let mut round = 0;
        loop {
            let sched_seed = rng.random();
            if let Some(mut rundat) = launch(round, sched_seed)? {
                if self.verbose {
                    let preempts = rundat.preempts_path_out();
                    let init_schedule: PreemptionRecord =
                        PreemptionReader::new(preempts).load_all();
                    eprintln!(
                        ":: {}:\nSchedule:\n {}",
                        "Search successfully found a failing run with schedule:"
                            .green()
                            .bold(),
                        truncated(
                            1000,
                            serde_json::to_string(&init_schedule.clone_preemptions_only()).unwrap()
                        ),
                    );
                }
                return Ok(rundat);
            }
            round += 1;
        }
    }

    /// Does a finished trial meet the criteria? A trial that ended with a
    /// scheduler terminal outcome has no guest status to judge (its 121 or 124
    /// is Hermit's), so it is counted neither on nor off target: the trial
    /// fails with the outcome's own class line and status. A deadlock under
    /// Hermit's model may be a gap in Hermit's model rather than the guest's
    /// behaviour; a virtual timeout is a bound Hermit imposed on a guest that
    /// had not finished, and a model gap (a missed wake turned into a
    /// spin-poll) can be what ran the clock past it.
    pub fn trial_matches(
        &self,
        out: &Output,
        terminal_outcome: Option<detcore::TerminalOutcome>,
    ) -> anyhow::Result<bool> {
        match terminal_outcome {
            Some(outcome @ detcore::TerminalOutcome::GuestDeadlock) => Err(Error::new(
                crate::app::container::SchedulerOutcome(outcome),
            )
            .context(
                "an analyze trial ended on a deadlock under Hermit's model, which is \
                     neither on nor off target",
            )),
            Some(outcome @ detcore::TerminalOutcome::VirtualTimeout { .. }) => Err(Error::new(
                crate::app::container::SchedulerOutcome(outcome),
            )
            .context(
                "an analyze trial was stopped by --virtual-timeout, which is neither on \
                     nor off target",
            )),
            None => Ok(self.output_matches(out)),
        }
    }

    /// Does the run meet the criteria we are looking for (e.g. a particular error message).
    /// With `--verbose`, says which criteria it misses.
    pub fn output_matches(&self, out: &Output) -> bool {
        let misses = self.criteria_misses(out);
        if self.verbose {
            for miss in &misses {
                eprintln!("{miss}");
            }
        }
        misses.is_empty()
    }

    /// The target criteria `out` does not meet. (`RunData::launch` prints
    /// the guest's output under `--verbose`.)
    fn criteria_misses(&self, out: &Output) -> Vec<String> {
        let mut misses = Vec::new();
        for (stream, pattern, text) in [
            ("stdout", &self.target_stdout, &out.stdout),
            ("stderr", &self.target_stderr, &out.stderr),
        ] {
            if let Some(pattern) = pattern
                && !pattern.is_match(&String::from_utf8_lossy(text))
            {
                misses.push(format!("Mismatch for {stream} pattern {pattern}"));
            }
        }
        if !self.target_exit_code.is_match(out.status) {
            misses.push(format!(
                "  Exit code {} is not what we're looking for.",
                super::types::run_exit_code(out.status)
            ));
        }
        misses
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser as _;

    use super::*;
    use crate::app::container::LogCapExceeded;
    use crate::app::container::PolicyRefusal;

    #[test]
    fn preemption_file_comparison_preserves_its_original_non_chosen_domain() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.json");
        let second = directory.path().join("second.json");
        let plain = PreemptionRecord::default().with_pipe_wake_latency(Some(3));
        let mut future = serde_json::to_value(&plain).unwrap();
        future["reserved_input"] = serde_json::json!({"future": [null, 17]});
        let opaque: PreemptionRecord = serde_json::from_value(future).unwrap();
        assert_ne!(
            plain, opaque,
            "full data equality must retain the new metadata"
        );
        plain.write_to_disk(&first).unwrap();
        opaque.write_to_disk(&second).unwrap();
        assert!(preempt_files_equal(&first, &second));
        assert!(preempt_files_equal(&second, &first));
        opaque
            .clone()
            .with_pipe_wake_latency(Some(4))
            .write_to_disk(&second)
            .unwrap();
        assert!(
            !preempt_files_equal(&first, &second),
            "an original metadata mismatch still fails"
        );

        let ids = detcore::GuestIds::Chosen {
            base: 5000,
            refuse_host_ids: true,
        };
        let mut value = serde_json::to_value(plain.with_guest_ids(ids)).unwrap();
        value["reserved_input"] = serde_json::json!({
            "contract": "sealed-private-input-v1", "base": "0x71010000",
            "abi": "x86_64", "page_bytes": 4096
        });
        let qualified: PreemptionRecord = serde_json::from_value(value.clone()).unwrap();
        qualified.write_to_disk(&first).unwrap();
        qualified.write_to_disk(&second).unwrap();
        assert!(preempt_files_equal(&first, &second));
        value["reserved_input"]["abi"] = serde_json::json!("aarch64");
        serde_json::from_value::<PreemptionRecord>(value)
            .unwrap()
            .write_to_disk(&second)
            .unwrap();
        assert!(
            !preempt_files_equal(&first, &second),
            "Chosen layouts remain strict"
        );
        opaque.with_guest_ids(ids).write_to_disk(&second).unwrap();
        assert!(
            !preempt_files_equal(&first, &second),
            "Chosen unknown metadata remains inadmissible"
        );
    }

    /// A stack-trace replay certifies the decision only when it observes the
    /// outcome the search did: all four (expected, observed) pairs, for one
    /// replay and for either of the default search's two, as main's final
    /// replays required. Both callers, the default search's report and the
    /// timing decision, check their replays through this helper.
    #[test]
    fn stack_trace_replays_must_repeat_the_searchs_outcomes() {
        for expected in [false, true] {
            for observed in [false, true] {
                let result = require_repeated_outcomes(&[("final", expected, observed)], "final");
                if expected == observed {
                    result.expect("a replay that repeats its outcome is valid");
                } else {
                    let error = result
                        .expect_err("a contradictory final replay cannot certify a candidate")
                        .to_string();
                    assert!(error.contains("observed the opposite"), "{error}");
                    assert!(error.contains("did not repeat"), "{error}");
                    let searched = if expected { "met" } else { "missed" };
                    assert!(
                        error.contains(&format!("which {searched} the target")),
                        "{error}"
                    );
                }
            }
        }
        // The default search's pair: the passing replay must miss, the
        // failing one meet. Either contradiction ends the analysis.
        for (baseline, target) in [(false, true), (true, true), (false, false), (true, false)] {
            let result = require_repeated_outcomes(
                &[("final_passing", false, baseline), ("final", true, target)],
                "final_passing and final",
            );
            assert_eq!(result.is_ok(), (baseline, target) == (false, true));
        }
    }

    /// The default search's report ends the analysis when either stack-trace
    /// replay observes the opposite of the search's outcome: a replay of the
    /// passing schedule that meets the target, or one of the failing schedule
    /// that misses it. The replays run through the report's own path
    /// ([`AnalyzeOpts::phase6_record_outputs`]), with a launch that models
    /// each completed outcome.
    #[test]
    fn the_default_search_report_ends_when_a_replay_contradicts_the_search() {
        for (baseline_met, target_met, contradicted) in [
            (true, true, "final_baseline_for_stacktraces"),
            (false, false, "final_target_for_stacktraces"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
            options.tmp_dir = Some(dir.path().to_path_buf());
            let error = options
                .phase6_record_outputs_with(critical(&[1, 2, 1, 2], 2), &[], None, |run| {
                    let met = if run.name() == "final_target_for_stacktraces" {
                        target_met
                    } else {
                        baseline_met
                    };
                    run.complete_for_test(met, &[])
                })
                .expect_err("a contradicting replay cannot certify the race")
                .to_string();
            assert!(
                error.contains(&format!("{contradicted} observed the opposite")),
                "{error}"
            );
        }
    }

    /// The timing decision's report ends the analysis when a stack-trace
    /// replay of either continuation observes the opposite of its outcome:
    /// the continuation without the event that meets the target, or the one
    /// with it that misses. The replays run through the timing decision's
    /// own path ([`AnalyzeOpts::report_timing_decision`]).
    #[test]
    fn the_timing_decision_ends_when_a_replay_contradicts_the_search() {
        let tid = detcore::types::DetTid::from_raw;
        let events = |tids: &[i32]| -> Vec<SchedEvent> {
            tids.iter()
                .map(|&t| SchedEvent::branches(tid(t), 1))
                .collect()
        };
        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let schedule = PreemptionRecord::from_sched_events(events(&[1, 2, 1, 2]));
        // Without event #1, thread 2 runs one event there; with it, two.
        let continued = events(&[1, 2, 1]);
        for (next_target_met, baseline_met, contradicted) in [
            (true, true, Some("final_baseline_next_for_stacktraces")),
            (false, false, Some("final_target_next_for_stacktraces")),
            (true, false, None),
        ] {
            let result = options.report_timing_decision_with(
                &schedule,
                1,
                None,
                (true, &None),
                (
                    &continued,
                    Path::new("final_baseline_schedule.preempts"),
                    &mut None,
                ),
                (
                    |_, len| {
                        assert_eq!(len, 2);
                        Ok((
                            CompletedScheduleTrial {
                                target_met: true,
                                schedule: events(&[1, 2, 2, 1]),
                            },
                            PathBuf::from("final_target_schedule.preempts"),
                            None,
                        ))
                    },
                    |runname, _, _, _| match runname {
                        "final_target_next_for_stacktraces" => Ok((next_target_met, None)),
                        "final_baseline_next_for_stacktraces" => Ok((baseline_met, None)),
                        other => panic!("unexpected replay {other}"),
                    },
                ),
            );
            match contradicted {
                Some(replay) => {
                    let error = result
                        .expect_err("a contradicting replay cannot certify the decision")
                        .to_string();
                    assert!(
                        error.contains(&format!("{replay} observed the opposite")),
                        "{error}"
                    );
                }
                None => assert!(result.unwrap().is_some()),
            }
        }
    }

    fn timing_report_stack(thread: i32, ip: u64) -> PrettyBacktrace {
        serde_json::from_value(serde_json::json!({
            "thread_id": thread,
            "thread_name": null,
            "frames": [{
                "frame": { "ip": ip, "is_signal": false },
                "symbol": null,
                "locations": []
            }]
        }))
        .unwrap()
    }

    /// Equal next-thread stacks take the slice-end report path. Every replay
    /// on that path must repeat its searched outcome, even without a stack.
    #[test]
    fn timing_reports_require_both_slice_end_replays_to_repeat_the_search() {
        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let with = events(&[5, 3, 3, 3, 7, 5]);
        let without = events(&[5, 3, 3, 5, 7]);
        let record = PreemptionRecord::from_sched_events(with.clone());
        let deciding = Some(timing_report_stack(3, 0x20));
        for (target_met, baseline_met, captured, contradicted) in [
            (true, false, true, None),
            (
                false,
                false,
                true,
                Some("final_target_slice_end_for_stacktraces"),
            ),
            (
                true,
                true,
                true,
                Some("final_baseline_slice_end_for_stacktraces"),
            ),
            (
                false,
                true,
                true,
                Some("final_target_slice_end_for_stacktraces"),
            ),
            (
                false,
                false,
                false,
                Some("final_target_slice_end_for_stacktraces"),
            ),
            (
                true,
                true,
                false,
                Some("final_baseline_slice_end_for_stacktraces"),
            ),
            (true, false, false, None),
        ] {
            let calls = std::cell::RefCell::new(Vec::new());
            let result = options.report_timing_decision_with(
                &record,
                1,
                NonZeroU64::new(200_000_000),
                (true, &deciding),
                (
                    &without,
                    Path::new("final_baseline_schedule.preempts"),
                    &mut None,
                ),
                (
                    |seen, len| {
                        assert_eq!(seen, &record);
                        assert_eq!(len, 2);
                        Ok((
                            CompletedScheduleTrial {
                                target_met: true,
                                schedule: with.clone(),
                            },
                            PathBuf::from("final_target_schedule.preempts"),
                            None,
                        ))
                    },
                    |runname, variant, len, position| {
                        calls.borrow_mut().push(runname.to_owned());
                        let (expected_schedule, expected_position, met, stack) = match runname {
                            "final_target_next_for_stacktraces" => {
                                (&with, 4, true, Some(timing_report_stack(7, 0x10)))
                            }
                            "final_baseline_next_for_stacktraces" => {
                                (&without, 3, false, Some(timing_report_stack(5, 0x10)))
                            }
                            "final_target_slice_end_for_stacktraces" => (
                                &with,
                                3,
                                target_met,
                                captured.then(|| timing_report_stack(3, 0x30)),
                            ),
                            "final_baseline_slice_end_for_stacktraces" => (
                                &without,
                                2,
                                baseline_met,
                                captured.then(|| timing_report_stack(3, 0x40)),
                            ),
                            other => panic!("unexpected replay {other}"),
                        };
                        assert_eq!(variant.schedevents(), expected_schedule);
                        assert_eq!(position, expected_position);
                        assert_eq!(len, expected_position + 1);
                        Ok((met, stack))
                    },
                ),
            );
            match contradicted {
                Some(replay) => {
                    let error = result
                        .expect_err("a contradictory slice-end replay cannot certify the decision")
                        .to_string();
                    assert!(
                        error.contains(&format!("{replay} observed the opposite")),
                        "{error}"
                    );
                    assert!(
                        error.contains("no opposite-outcome pair was established"),
                        "{error}"
                    );
                    assert!(calls.borrow().iter().any(|name| name == replay));
                }
                None => {
                    let report = result
                        .unwrap()
                        .expect("a repeated pair certifies the decision");
                    assert_eq!(calls.borrow().len(), 4);
                    if captured {
                        assert_eq!(report.critical_event1.event_index, 3);
                        assert_eq!(report.critical_event2.event_index, 2);
                        assert!(same_frames(
                            report.critical_event1.stack.as_ref().unwrap(),
                            &timing_report_stack(3, 0x30)
                        ));
                        assert!(same_frames(
                            report.critical_event2.stack.as_ref().unwrap(),
                            &timing_report_stack(3, 0x40)
                        ));
                    } else {
                        assert_eq!(report.critical_event1.event_index, 4);
                        assert_eq!(report.critical_event2.event_index, 3);
                    }
                }
            }
        }
    }

    /// A one-event slice still belongs to its own continuation. Capture both
    /// endpoints and retain the separately labelled original deciding stack.
    #[test]
    fn timing_reports_capture_single_event_slices_from_their_own_records() {
        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        for target_is_short in [false, true] {
            let (with, without, ends) = if target_is_short {
                (events(&[5, 3, 7, 5]), events(&[5, 3, 3, 5, 7]), [1, 2])
            } else {
                (events(&[5, 3, 3, 3, 7, 5]), events(&[5, 3, 5, 7]), [3, 1])
            };
            let record = PreemptionRecord::from_sched_events(with.clone());
            let deciding = Some(timing_report_stack(3, 0x20));
            let calls = std::cell::RefCell::new(Vec::new());
            let report = options
                .report_timing_decision_with(
                    &record,
                    1,
                    NonZeroU64::new(200_000_000),
                    (true, &deciding),
                    (
                        &without,
                        Path::new("final_baseline_schedule.preempts"),
                        &mut None,
                    ),
                    (
                        |seen, len| {
                            assert_eq!(seen, &record);
                            assert_eq!(len, 2);
                            Ok((
                                CompletedScheduleTrial {
                                    target_met: true,
                                    schedule: with.clone(),
                                },
                                PathBuf::from("final_target_schedule.preempts"),
                                None,
                            ))
                        },
                        |runname, variant, len, position| {
                            calls.borrow_mut().push(runname.to_owned());
                            let (expected, end, met, stack) = match runname {
                                "final_target_next_for_stacktraces" => {
                                    (&with, ends[0] + 1, true, timing_report_stack(7, 0x10))
                                }
                                "final_baseline_next_for_stacktraces" => {
                                    (&without, ends[1] + 1, false, timing_report_stack(5, 0x10))
                                }
                                "final_target_slice_end_for_stacktraces" => (
                                    &with,
                                    ends[0],
                                    true,
                                    timing_report_stack(
                                        3,
                                        if target_is_short { 0x20 } else { 0x30 },
                                    ),
                                ),
                                "final_baseline_slice_end_for_stacktraces" => {
                                    (&without, ends[1], false, timing_report_stack(3, 0x40))
                                }
                                other => panic!("unexpected replay {other}"),
                            };
                            assert_eq!(variant.schedevents(), expected);
                            assert_eq!((len, position), (end + 1, end));
                            Ok((met, Some(stack)))
                        },
                    ),
                )
                .unwrap()
                .expect("each continuation certifies its own endpoint");
            assert_eq!(
                *calls.borrow(),
                [
                    "final_target_next_for_stacktraces",
                    "final_baseline_next_for_stacktraces",
                    "final_target_slice_end_for_stacktraces",
                    "final_baseline_slice_end_for_stacktraces",
                ],
                "two next-thread and two slice-end replays"
            );
            assert_eq!(report.critical_event1.event_index, ends[0]);
            assert_eq!(report.critical_event2.event_index, ends[1]);
            assert_eq!(
                report.critical_event1.stack,
                Some(timing_report_stack(
                    3,
                    if target_is_short { 0x20 } else { 0x30 }
                ))
            );
            assert_eq!(
                report.critical_event2.stack,
                Some(timing_report_stack(3, 0x40))
            );
            assert_eq!(report.deciding_event.unwrap().stack, deciding);
        }
    }

    /// Completed, admitted checkpoint-free records can have one-event slices
    /// at the same index but different operation batches. The target can also
    /// realize a different count than the original deciding event. Model the
    /// completed producer through the real file reader, not a native guest.
    fn single_event_timing_report_for_test(
        target_count: u32,
        target_met: bool,
        baseline_met: bool,
        captured: bool,
    ) -> (anyhow::Result<Option<Report>>, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(dir.path().to_path_buf());
        let source =
            PreemptionRecord::from_sched_events(branch_events(&[(5, 1), (3, 100), (7, 1), (5, 1)]));
        source.validate().unwrap();
        let with = branch_events(&[(5, 1), (3, target_count), (7, 1), (5, 1)]);
        let without = branch_events(&[(5, 1), (3, 1), (7, 1), (5, 1)]);
        let (baseline, baseline_path) = recorded_continuation_for_test(
            &options,
            "single_event_baseline",
            &source,
            1,
            false,
            &without,
        );
        assert!(!baseline.target_met);
        let calls = std::cell::RefCell::new(Vec::new());
        let deciding = Some(timing_report_stack(3, 0x20));
        let result = options.report_timing_decision_with(
            &source,
            1,
            None,
            (true, &deciding),
            (&baseline.schedule, &baseline_path, &mut None),
            (
                |seen, len| {
                    assert_eq!(seen, &source);
                    assert_eq!(len, 2);
                    let (completed, path) = recorded_continuation_for_test(
                        &options,
                        "single_event_target",
                        seen,
                        len,
                        true,
                        &with,
                    );
                    Ok((completed, path, None))
                },
                |runname, variant, len, position| {
                    calls.borrow_mut().push(runname.to_owned());
                    let (expected, prefix_len, end, met, stack) = match runname {
                        "final_target_next_for_stacktraces" => {
                            (&with, 2, 2, true, Some(timing_report_stack(7, 0x10)))
                        }
                        "final_baseline_next_for_stacktraces" => {
                            (&without, 1, 2, false, Some(timing_report_stack(7, 0x10)))
                        }
                        "final_target_slice_end_for_stacktraces" => (
                            &with,
                            2,
                            1,
                            target_met,
                            captured.then(|| {
                                timing_report_stack(
                                    3,
                                    if target_count == 100 { 0x20 } else { 0x30 },
                                )
                            }),
                        ),
                        "final_baseline_slice_end_for_stacktraces" => (
                            &without,
                            1,
                            1,
                            baseline_met,
                            captured.then(|| timing_report_stack(3, 0x40)),
                        ),
                        other => panic!("unexpected replay {other}"),
                    };
                    variant.validate().unwrap();
                    assert_eq!(variant.schedevents(), expected);
                    assert_prefix_metadata(variant, &source, prefix_len);
                    assert_eq!((len, position), (end + 1, end));
                    Ok((met, stack))
                },
            ),
        );
        let retained = PreemptionReader::try_new(&baseline_path)
            .unwrap()
            .into_inner();
        assert_eq!(retained.schedevents(), &without);
        (result, calls.into_inner())
    }

    #[test]
    fn one_event_timing_slices_report_their_own_dynamic_endpoints() {
        for target_count in [100, 101] {
            let (result, calls) =
                single_event_timing_report_for_test(target_count, true, false, true);
            let report = result
                .unwrap()
                .expect("opposite completed outcomes certify the report");
            assert_eq!(
                report.critical_event2.stack,
                Some(timing_report_stack(3, 0x40)),
                "the one-branch baseline endpoint must not use the target deciding stack"
            );
            assert_eq!(
                report.critical_event1.stack,
                Some(timing_report_stack(
                    3,
                    if target_count == 100 { 0x20 } else { 0x30 }
                )),
                "the realized target endpoint must come from its own continued record"
            );
            assert_eq!(
                (
                    report.critical_event1.event_index,
                    report.critical_event2.event_index
                ),
                (1, 1)
            );
            let deciding = report.deciding_event.unwrap();
            assert_eq!(deciding.event_index, 1);
            assert_eq!(deciding.stack, Some(timing_report_stack(3, 0x20)));
            assert!(
                report.header.contains(&format!(
                    "runs 1 events ({target_count} retired conditional branches)"
                )),
                "{}",
                report.header
            );
            assert!(
                report
                    .header
                    .contains("runs 1 events (1 retired conditional branches)"),
                "{}",
                report.header
            );
            assert!(
                !report.header.contains("same stack at both events"),
                "{}",
                report.header
            );
            assert_eq!(
                calls,
                [
                    "final_target_next_for_stacktraces",
                    "final_baseline_next_for_stacktraces",
                    "final_target_slice_end_for_stacktraces",
                    "final_baseline_slice_end_for_stacktraces",
                ]
            );
        }
    }

    #[test]
    fn one_event_timing_slice_replays_must_repeat_their_own_outcomes() {
        for (target_met, baseline_met, contradicted, expected_calls) in [
            (true, true, "final_baseline_slice_end_for_stacktraces", 4),
            (false, false, "final_target_slice_end_for_stacktraces", 3),
        ] {
            for captured in [true, false] {
                let (result, calls) =
                    single_event_timing_report_for_test(100, target_met, baseline_met, captured);
                let error = result
                    .expect_err(
                        "a contradictory one-event slice replay cannot certify the decision",
                    )
                    .to_string();
                assert!(
                    error.contains(&format!("{contradicted} observed the opposite")),
                    "{error}"
                );
                assert!(
                    error.contains("no opposite-outcome pair was established"),
                    "{error}"
                );
                assert_eq!(calls.len(), expected_calls);
                assert_eq!(calls.last().unwrap(), contradicted);
            }
        }
        let (result, calls) = single_event_timing_report_for_test(100, true, false, false);
        let report = result
            .unwrap()
            .expect("missing stacks do not invent a contradictory outcome");
        assert_eq!(calls.len(), 4);
        assert_eq!(
            (
                report.critical_event1.event_index,
                report.critical_event2.event_index
            ),
            (2, 2)
        );
        assert_eq!(
            report.critical_event1.stack,
            Some(timing_report_stack(7, 0x10))
        );
        assert_eq!(
            report.critical_event2.stack,
            Some(timing_report_stack(7, 0x10))
        );
    }

    /// The timing report creates another prefix continuation after the
    /// search. Its real run configuration must retain the selected maximum
    /// through launch validation instead of restoring range/seed defaults.
    #[test]
    fn timing_report_continuations_keep_the_selected_maximum_at_validation() {
        let chosen = NonZeroU64::new(500_000_000);
        for (selection, recorded) in [
            (&["--max-timeslice=4321"][..], Some(4321)),
            (&["--chaos-timeslice-range=1000..5000000"][..], None),
            (&["--chaos-seeded-timeslice"][..], None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut args = vec!["analyze", "--prefix-search", "--", "--chaos", "--seed=7"];
            args.extend_from_slice(selection);
            args.push("/bin/true");
            let mut options = AnalyzeOpts::try_parse_from(args).unwrap();
            options.tmp_dir = Some(dir.path().to_path_buf());
            let with = events(&[5, 3, 3, 3, 7, 5]);
            let without = events(&[5, 3, 3, 5, 7]);
            let record =
                PreemptionRecord::from_sched_events(with.clone()).with_chaos_timeslice(recorded);
            let report = options
                .report_timing_decision_with(
                    &record,
                    1,
                    chosen,
                    (true, &None),
                    (
                        &without,
                        Path::new("final_baseline_schedule.preempts"),
                        &mut None,
                    ),
                    (
                        |target, len| {
                            options.prefix_continuation(
                                "final_target_schedule",
                                target,
                                len,
                                chosen,
                                None,
                                |run| {
                                    let written = PreemptionReader::try_new(run.sched_path_in())?
                                        .into_inner();
                                    for _ in 0..2 {
                                        run.runopts.validate_args_with_perf_support(true)?;
                                        assert_eq!(
                                            run.runopts.det_opts.det_config.max_timeslice,
                                            chosen
                                        );
                                        assert!(!run.runopts.det_opts.det_config.chaos);
                                    }
                                    assert_eq!(
                                        written.chaos_timeslice(),
                                        chosen.map(NonZeroU64::get)
                                    );
                                    run.complete_for_test(true, &with)?;
                                    Ok(true)
                                },
                            )
                        },
                        |runname, _, _, _| match runname {
                            "final_target_next_for_stacktraces" => Ok((true, None)),
                            "final_baseline_next_for_stacktraces" => Ok((false, None)),
                            other => panic!("unexpected replay {other}"),
                        },
                    ),
                )
                .unwrap()
                .expect("the report preserves its tested continuation");
            assert!(
                report.header.contains("500000000 ns timeslice"),
                "{}",
                report.header
            );
            assert_eq!(
                record.chaos_timeslice(),
                recorded,
                "the source record is unchanged"
            );
        }
    }

    /// Capturing both stacks is not proof that the candidate completed. This
    /// drives the stack-trace replay used by production reporting
    /// ([`AnalyzeOpts::replay_for_stacks`]), whose completed outcome the
    /// report then compares with the search's, rather than only the stack
    /// launcher.
    #[test]
    fn a_timed_out_final_replay_cannot_validate_the_expected_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(dir.path().to_path_buf());
        let stack: PrettyBacktrace = serde_json::from_value(serde_json::json!({
            "thread_id": 3,
            "thread_name": "guest",
            "frames": [{
                "frame": { "ip": 4096, "is_signal": false },
                "symbol": null,
                "locations": []
            }]
        }))
        .unwrap();
        let bytes = serde_json::to_vec(&stack).unwrap();
        // Each of the two replays the report takes, the passing and the
        // failing one.
        for runname in ["unfinished_final_passing", "unfinished_final"] {
            let result = options.replay_for_stacks(
                runname,
                &dir.path().join("candidate.json"),
                &[],
                1,
                None,
                |run| {
                    assert!(!run.has_launched());
                    for extension in ["stack1", "stack2"] {
                        std::fs::write(run.root_path().with_extension(extension), &bytes)?;
                    }
                    Err(Error::new(RunTimeoutMarker))
                },
            );
            let error = result
                .err()
                .expect("captured stacks must not certify an unfinished candidate");
            assert!(
                error.downcast_ref::<RunTimeoutMarker>().is_some(),
                "{error:#}"
            );
            assert!(
                format!("{error:#}").contains("no program outcome was established"),
                "{error:#}"
            );
            for extension in ["stack1", "stack2"] {
                assert_eq!(
                    std::fs::read(dir.path().join(format!("{runname}.{extension}"))).unwrap(),
                    bytes,
                    "retain captured diagnostics"
                );
            }
        }
    }

    /// Exercise analyze's production adapter, not an error supplied directly
    /// to schedule_search: an unfinished midpoint must not replace a completed
    /// passing endpoint or supply the requested trace as an observed trace.
    #[test]
    fn a_timed_out_midpoint_cannot_replace_the_completed_baseline() {
        use detcore::types::DetTid;
        use detcore::types::SyscallPhase;
        use reverie::syscalls::Sysno;

        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let a = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let b = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let c = SchedEvent::syscall(DetTid::from_raw(7), Sysno::getpid, SyscallPhase::Prehook);
        let baseline = vec![a.clone(), b.clone(), c.clone()];
        let target = vec![c, b, a];
        let mut trials = 0;
        let result = options.phase5_bisect_traces_with(&target, &baseline, |trial, requested| {
            trials = trial;
            match trial {
                1 => {
                    assert_eq!(requested, baseline);
                    Ok(CompletedScheduleTrial {
                        target_met: false,
                        schedule: baseline.clone(),
                    })
                }
                2 => {
                    assert_eq!(requested, target);
                    Ok(CompletedScheduleTrial {
                        target_met: true,
                        schedule: target.clone(),
                    })
                }
                3 => Err(Error::new(RunTimeoutMarker)),
                _ => bail!("the search launched another trial after the midpoint timed out"),
            }
        });
        let error = result.expect_err("a timed-out midpoint must stop the analysis");
        assert!(
            error.downcast_ref::<RunTimeoutMarker>().is_some(),
            "{error:#}"
        );
        assert_eq!(trials, 3, "no trial may follow the unfinished midpoint");
    }

    /// A refusal after completed endpoint observations must not become a
    /// replacement baseline with a requested trace presented as realized.
    #[test]
    fn a_refused_midpoint_cannot_replace_the_completed_baseline() {
        use detcore::types::DetTid;
        use detcore::types::SyscallPhase;
        use reverie::syscalls::Sysno;

        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let a = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let b = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let c = SchedEvent::syscall(DetTid::from_raw(7), Sysno::getpid, SyscallPhase::Prehook);
        let baseline = vec![a.clone(), b.clone(), c.clone()];
        let target = vec![c, b, a];
        for contextual in [false, true] {
            let mut trials = 0;
            let result =
                options.phase5_bisect_traces_with(&target, &baseline, |trial, requested| {
                    trials = trial;
                    match trial {
                        1 => {
                            assert_eq!(requested, baseline);
                            Ok(CompletedScheduleTrial {
                                target_met: false,
                                schedule: baseline.clone(),
                            })
                        }
                        2 => {
                            assert_eq!(requested, target);
                            Ok(CompletedScheduleTrial {
                                target_met: true,
                                schedule: target.clone(),
                            })
                        }
                        3 => Err(if contextual {
                            Error::new(PolicyRefusal)
                                .context("unsupported operation: restart_syscall")
                        } else {
                            Error::new(PolicyRefusal)
                        }),
                        _ => bail!(
                            "the search launched another trial after the midpoint was refused"
                        ),
                    }
                });
            let error = result.expect_err("a refused midpoint cannot supply a completed candidate");
            assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
            assert_eq!(trials, 3, "no trial may follow the refused midpoint");
            if contextual {
                assert!(format!("{error:#}").contains("unsupported operation: restart_syscall"));
            }
        }
    }

    fn stopped_trial(kind: &str) -> Error {
        match kind {
            "timeout" => Error::new(RunTimeoutMarker),
            "refusal" => Error::new(PolicyRefusal),
            "contextual refusal" => Error::new(PolicyRefusal)
                .context("unsupported operation: restart_syscall without an interrupted wait"),
            "log limit" => Error::new(LogCapExceeded),
            _ => unreachable!(),
        }
    }

    fn assert_stopped_trial(error: &Error, kind: &str) {
        assert!(
            match kind {
                "timeout" => error.downcast_ref::<RunTimeoutMarker>().is_some(),
                "refusal" | "contextual refusal" => {
                    error.downcast_ref::<PolicyRefusal>().is_some()
                }
                "log limit" => error.downcast_ref::<LogCapExceeded>().is_some(),
                _ => unreachable!(),
            },
            "{kind}: {error:#}"
        );
    }

    #[test]
    fn bisection_requires_completed_outcomes_and_preserves_trial_errors() {
        use detcore::types::DetTid;
        use detcore::types::SyscallPhase;
        use reverie::syscalls::Sysno;

        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let a = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let b = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let baseline = vec![a.clone(), b.clone()];
        let target = vec![b, a];
        // A guest's ordinary exit 122 is an observed outcome, not PolicyRefusal.
        let output = |status| Output {
            status: ExitStatus::Exited(status),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(!options.output_matches(&output(0)));
        assert!(options.output_matches(&output(122)));
        let completed = options
            .phase5_bisect_traces_with(&target, &baseline, |_, requested| {
                Ok(CompletedScheduleTrial {
                    target_met: options.output_matches(&output(if requested == baseline {
                        0
                    } else {
                        122
                    })),
                    schedule: requested.to_vec(),
                })
            })
            .unwrap();
        assert_eq!(completed.passing_schedule, baseline);
        assert_eq!(completed.failing_schedule, target);

        for kind in ["timeout", "refusal", "contextual refusal", "log limit"] {
            let mut trials = 0;
            let error = options
                .phase5_bisect_traces_with(&target, &baseline, |_, _| {
                    trials += 1;
                    Err(stopped_trial(kind))
                })
                .unwrap_err();
            assert_stopped_trial(&error, kind);
            assert_eq!(trials, 1, "{kind}");
        }
    }

    #[test]
    fn unfinished_prefix_and_stack_trials_keep_the_error_and_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(dir.path().to_path_buf());
        for kind in ["timeout", "refusal", "contextual refusal", "log limit"] {
            let mut run = RunData::new_baseline(&options, "prefix".to_owned()).unwrap();
            let error = options
                .launch_prefix_trial(&mut run, |_| Err(stopped_trial(kind)))
                .unwrap_err();
            assert_stopped_trial(&error, kind);
            assert!(!run.has_launched());

            let error = options
                .replay_for_stacks("final", Path::new("schedule.json"), &[], 1, None, |run| {
                    std::fs::write(run.root_path().with_extension("stack2"), "retained stack")?;
                    Err(stopped_trial(kind))
                })
                .err()
                .expect("captured stacks cannot establish a completed outcome");
            assert_stopped_trial(&error, kind);
            let message = format!("{error:#}");
            assert!(
                message.contains("no program outcome was established"),
                "{message}"
            );
            assert!(
                message.contains(&dir.path().display().to_string()),
                "{message}"
            );
            if kind == "timeout" {
                assert!(message.contains("larger --timeout"), "{message}");
                let remedy = error.to_string();
                assert!(
                    remedy.contains("for hermit analyze's trial bound, increase --trial-timeout"),
                    "{remedy}"
                );
                assert!(
                    remedy.contains("larger --timeout in the run arguments"),
                    "{remedy}"
                );
                assert!(
                    remedy.contains("The smaller configured bound still applies"),
                    "{remedy}"
                );
            }
            if kind.contains("refusal") {
                assert!(
                    error
                        .to_string()
                        .contains("use a supported operation or configuration before retrying"),
                    "{message}"
                );
                assert!(
                    message.contains("A refusal cannot establish a target or baseline outcome"),
                    "{message}"
                );
            }
            if kind == "contextual refusal" {
                assert!(
                    message.contains("restart_syscall without an interrupted wait"),
                    "{message}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(dir.path().join("final.stack2")).unwrap(),
                "retained stack"
            );
        }
    }

    #[test]
    fn a_prefix_boundary_must_repeat_both_completed_outcomes() {
        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let schedule =
            PreemptionRecord::from_sched_events(critical(&[3, 5, 7], 1).failing_schedule);
        // Trials 1..=4 search the boundary; trials 5 and 6 recheck its two sides.
        for changed_trial in [None, Some(5), Some(6)] {
            let mut trials = 0;
            let result = options.phase5_prefix_search_with(&schedule, |trial, len, _| {
                trials = trial;
                let target_met = len > 1;
                Ok(if Some(trial) == changed_trial {
                    !target_met
                } else {
                    target_met
                })
            });
            if let Some(changed) = changed_trial {
                let error = result
                    .err()
                    .expect("a contradicted boundary is not a decision");
                assert!(
                    error
                        .to_string()
                        .contains("no deciding event was established"),
                    "{error:#}"
                );
                assert_eq!(trials, changed);
            } else {
                assert_eq!(result.unwrap().decision, 1);
                assert_eq!(trials, 6);
            }
        }
    }

    #[test]
    fn a_completed_empty_prefix_record_is_distinct_from_a_missing_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(dir.path().to_path_buf());
        let schedule = PreemptionRecord::from_sched_events(Vec::new());
        let error = options
            .prefix_continuation("final_baseline_schedule", &schedule, 0, None, None, |_| {
                Ok(false)
            })
            .err()
            .expect("a missing record must not stand in for an observed empty trace");
        assert!(
            error.to_string().contains("no readable recorded schedule"),
            "{error:#}"
        );
        let (completed, _, _) = options
            .prefix_continuation("final_baseline_schedule", &schedule, 0, None, None, |run| {
                schedule
                    .write_to_disk(run.sched_path_out())
                    .map_err(anyhow::Error::msg)?;
                Ok(false)
            })
            .unwrap();
        assert!(!completed.target_met);
        assert!(completed.schedule.is_empty());
    }

    #[test]
    fn a_prefix_report_requires_completed_opposite_outcomes_even_without_stacks() {
        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let events = critical(&[3, 5, 7], 1).failing_schedule;
        // The continuation runs another thread at the decision than the
        // target schedule, so the report is the two-thread one: one that runs
        // the same thread there is reported as a timing decision, which
        // replays both continuations.
        let continued = critical(&[3, 7, 5], 1).failing_schedule;
        // None denotes a timeout. All completed stack trials deliberately have
        // no stack artifact: diagnostic availability cannot prove an outcome.
        for (target, continuation, len, baseline, expected_error, expected_stacks) in [
            (Some(true), Some(false), 0, Some(false), None, 1),
            (Some(true), Some(false), 1, Some(false), None, 1),
            (Some(true), Some(false), 3, Some(false), None, 2),
            (
                Some(false),
                Some(false),
                3,
                Some(false),
                Some("final target"),
                1,
            ),
            (
                Some(true),
                Some(true),
                0,
                Some(false),
                Some("baseline prefix continuation"),
                1,
            ),
            (
                Some(true),
                Some(true),
                3,
                Some(false),
                Some("baseline prefix continuation"),
                1,
            ),
            (
                Some(true),
                Some(false),
                3,
                Some(true),
                Some("baseline stack replay"),
                2,
            ),
            (None, Some(false), 3, Some(false), Some("timeout"), 1),
            (Some(true), None, 3, Some(false), Some("timeout"), 1),
            (Some(true), Some(false), 3, None, Some("timeout"), 2),
        ] {
            let mut stacks = 0;
            let mut continuations = 0;
            let result = options.phase6_record_prefix_outputs_with(
                PrefixDecision {
                    schedule: PreemptionRecord::from_sched_events(events.clone()),
                    decision: 1,
                    continuation: None,
                },
                |runname, _, prefix, position, _| {
                    stacks += 1;
                    assert_eq!((prefix, position), (2, 1));
                    let target_met = if runname == "final_target_for_stacktraces" {
                        target
                    } else {
                        baseline
                    };
                    target_met
                        .map(|met| (met, None, None))
                        .ok_or_else(|| Error::new(RunTimeoutMarker))
                },
                |_, prefix, _| {
                    continuations += 1;
                    assert_eq!(prefix, 1);
                    Ok((
                        CompletedScheduleTrial {
                            target_met: continuation.ok_or_else(|| Error::new(RunTimeoutMarker))?,
                            schedule: continued[..len].to_vec(),
                        },
                        PathBuf::from("final_baseline_schedule.preempts"),
                        None,
                    ))
                },
            );
            if let Some(expected) = expected_error {
                let error = result.expect_err("unproven outcomes must not produce a causal report");
                if expected == "timeout" {
                    assert_stopped_trial(&error, "timeout");
                } else {
                    assert!(error.to_string().contains(expected), "{error:#}");
                }
            } else {
                let report = result.unwrap();
                assert!(report.header.contains("decides the outcome"));
                assert!(report.critical_event1.stack.is_none());
                assert!(report.critical_event2.stack.is_none());
            }
            assert_eq!(stacks, expected_stacks);
            assert_eq!(continuations, usize::from(target == Some(true)));
        }
    }

    /// A valid record whose callback checkpoints end thread 3's slices at
    /// progress 10 and 15 and thread 5's at progress 4, with matching history
    /// rows, over `global`, and a recorded 4321 ns maximum timeslice.
    fn checkpointed_record(global: &[SchedEvent]) -> PreemptionRecord {
        let checkpoint = |row: usize, progress: u64, time: u64| {
            serde_json::json!({
                "history_index": row,
                "origin": {
                    "callback": "PmuTimer", "site": "Prehook", "progress": progress,
                    "last_op": "Branch", "time": time, "rcbs": progress
                }
            })
        };
        let record: PreemptionRecord = serde_json::from_value(serde_json::json!({
            "per_thread": {
                "3": {
                    "final_prio": 1000,
                    "prio_changes": [[100, 1000], [300, 1000]],
                    "preemption_rcbs": [10, 15]
                },
                "5": {"final_prio": 1000, "prio_changes": [[200, 1000]], "preemption_rcbs": [4]}
            },
            "global": global,
            "chaos_timeslice": 4321,
            "callback_checkpoints": {
                "3": [checkpoint(0, 10, 100), checkpoint(1, 15, 300)],
                "5": [checkpoint(0, 4, 200)]
            }
        }))
        .unwrap();
        record.validate().unwrap();
        record
    }

    /// Thread and branch count per event.
    fn branch_events(events: &[(i32, u32)]) -> Vec<SchedEvent> {
        events
            .iter()
            .map(|&(tid, count)| SchedEvent::branches(detcore::types::DetTid::from_raw(tid), count))
            .collect()
    }

    /// The threads a record's callback checkpoints name, and how many each.
    fn checkpoint_counts(record: &PreemptionRecord) -> Vec<(String, usize)> {
        let value = serde_json::to_value(record).unwrap();
        let mut counts: Vec<(String, usize)> = value["callback_checkpoints"]
            .as_object()
            .map(|points| {
                points
                    .iter()
                    .map(|(tid, rows)| (tid.clone(), rows.as_array().unwrap().len()))
                    .collect()
            })
            .unwrap_or_default();
        counts.sort();
        counts
    }

    /// Asserts that `result` is the refusal of a continuation (recorded at
    /// `path`) its prefix's checkpoints do not attach to, naming its first
    /// event that differs from the prefix.
    fn assert_unattached_checkpoints<T: std::fmt::Debug>(
        result: anyhow::Result<T>,
        path: &str,
        drift: &str,
    ) {
        let error = result.expect_err("a drifted continuation must not be replayed bare");
        assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
        let message = format!("{error:#}");
        for expected in [
            "HERMIT_CONTINUATION_CHECKPOINTS_UNATTACHED",
            path,
            drift,
            "no opposite-outcome pair was established",
            "kept in the workspace",
        ] {
            assert!(message.contains(expected), "no {expected:?}: {message}");
        }
    }

    /// Compare all prefix metadata, including each checkpoint's full origin
    /// and every priority-history row, independently of the continued events.
    fn assert_prefix_metadata(record: &PreemptionRecord, source: &PreemptionRecord, len: usize) {
        let prefix = source.schedule_prefix(len);
        prefix.validate().unwrap();
        let mut expected = serde_json::to_value(prefix).unwrap();
        let mut actual = serde_json::to_value(record).unwrap();
        expected.as_object_mut().unwrap().remove("global");
        actual.as_object_mut().unwrap().remove("global");
        assert_eq!(
            actual, expected,
            "a continued replay must retain all prefix metadata"
        );
    }

    /// Model a completed producer at the existing launch seam, then use the
    /// production file reader and completed-record admission before reporting.
    /// This exercises file/admission plumbing; it is not a native guest run.
    fn recorded_continuation_for_test(
        options: &AnalyzeOpts,
        runname: &str,
        source: &PreemptionRecord,
        len: usize,
        met: bool,
        events: &[SchedEvent],
    ) -> (CompletedScheduleTrial, PathBuf) {
        let (completed, path, _) = options
            .prefix_continuation(runname, source, len, None, None, |run| {
                let input = PreemptionReader::try_new(run.sched_path_in())?.into_inner();
                input.validate().map_err(anyhow::Error::msg)?;
                let input = serde_json::to_value(&input).unwrap();
                let prefix = serde_json::to_value(source.schedule_prefix(len)).unwrap();
                for field in ["callback_checkpoints", "per_thread"] {
                    assert_eq!(input[field], prefix[field], "prefix input lost {field}");
                }
                run.complete_for_test(met, events)?;
                assert!(run.has_launched());
                assert_eq!(run.is_a_match(), met);
                Ok(run.is_a_match())
            })
            .unwrap();
        let recorded = PreemptionReader::try_new(&path).unwrap().into_inner();
        recorded.validate().unwrap();
        RunData::require_complete_record(&recorded, &path).unwrap();
        assert_eq!(recorded.schedevents(), events);
        assert_eq!(completed.schedule, events);
        assert_eq!(completed.target_met, met);
        (completed, path)
    }

    /// A completed continuation can replay its prefix with other counts (a
    /// soft count mismatch) and still repeat the search's outcome. The
    /// prefix's callback checkpoints then attach to none of its events, so
    /// the two-thread report refuses it, naming the recorded continuation,
    /// before the stack-trace replay of the baseline. Before, it replayed the
    /// bare events instead, without the checkpoints or the rest of the
    /// record. A continuation that replayed the prefix exactly keeps the
    /// prefix's checkpoints, histories and metadata.
    #[test]
    fn a_prefix_report_refuses_a_continuation_its_checkpoints_do_not_attach_to() {
        // Thread 3's progress-10 checkpoint is the one the first two events keep.
        let schedule =
            checkpointed_record(&branch_events(&[(3, 10), (5, 4), (7, 1), (7, 1), (3, 5)]));
        // Without event #2 thread 3 runs there instead: the two-thread report.
        let exact = branch_events(&[(3, 10), (5, 4), (3, 5), (7, 1), (7, 1)]);
        let drifted = branch_events(&[(3, 11), (5, 4), (3, 4), (7, 1), (7, 1)]);
        for continued in [&exact, &drifted] {
            let dir = tempfile::tempdir().unwrap();
            let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
            options.tmp_dir = Some(dir.path().to_path_buf());
            let mut replays = Vec::new();
            let mut artifact = None;
            let result = options.phase6_record_prefix_outputs_with(
                PrefixDecision {
                    schedule: schedule.clone(),
                    decision: 2,
                    continuation: None,
                },
                |runname, record, len, position, _trace_from| {
                    assert_eq!((len, position), (3, 2));
                    replays.push((runname.to_owned(), record.clone()));
                    Ok((runname == "final_target_for_stacktraces", None, None))
                },
                |record, len, _trace| {
                    assert_eq!(len, 2);
                    let (completed, path) = recorded_continuation_for_test(
                        &options,
                        "final_baseline_schedule",
                        record,
                        len,
                        false,
                        continued,
                    );
                    artifact = Some((path.clone(), std::fs::read(&path)?));
                    Ok((completed, path, None))
                },
            );
            let (path, bytes) = artifact.expect("the baseline continuation was recorded");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "retain completed diagnostics"
            );
            if continued == &drifted {
                assert_unattached_checkpoints(
                    result,
                    path.to_str().unwrap(),
                    "its event #0 is (tid3 cnt=11 Branch), where the prefix has (tid3 cnt=10 \
                     Branch)",
                );
                let names: Vec<&str> = replays.iter().map(|(name, _)| name.as_str()).collect();
                assert_eq!(names, ["final_target_for_stacktraces"]);
            } else {
                assert!(result.unwrap().header.contains("decides the outcome"));
                let (name, baseline) = &replays[1];
                assert_eq!(name, "final_baseline_for_stacktraces");
                assert_eq!(baseline.schedevents(), &exact);
                assert_eq!(checkpoint_counts(baseline), [("3".to_owned(), 1)]);
                assert_eq!(baseline.chaos_timeslice(), Some(4321));
                assert_prefix_metadata(baseline, &schedule, 2);
                baseline.validate().unwrap();
            }
        }
    }

    /// The timing report replays both continuations under their prefixes'
    /// checkpoints, so it refuses either one they do not attach to, before
    /// any stack-trace replay, although both outcomes repeat the search's:
    /// the continuation with the deciding event (thread 5's count drifted)
    /// and the one without it (thread 3's). Exact continuations keep the
    /// checkpoints each prefix reaches, and the record's metadata.
    #[test]
    fn a_timing_report_refuses_continuations_their_checkpoints_do_not_attach_to() {
        let schedule =
            checkpointed_record(&branch_events(&[(3, 10), (5, 4), (7, 1), (7, 1), (3, 5)]));
        // Thread 7 runs at event #2 with and without it: two events with it,
        // one without.
        let with = branch_events(&[(3, 10), (5, 4), (7, 1), (7, 1), (3, 5)]);
        let without = branch_events(&[(3, 10), (5, 4), (7, 1), (3, 5), (7, 1)]);
        let with_drifted = branch_events(&[(3, 10), (5, 5), (7, 1), (7, 1), (3, 5)]);
        let without_drifted = branch_events(&[(3, 11), (5, 4), (7, 1), (3, 4), (7, 1)]);
        for (target_continued, continued, refused) in [
            (&with, &without, None),
            (
                &with_drifted,
                &without,
                Some((
                    true,
                    "its event #1 is (tid5 cnt=5 Branch), where the prefix has (tid5 cnt=4 Branch)",
                )),
            ),
            (
                &with,
                &without_drifted,
                Some((
                    false,
                    "its event #0 is (tid3 cnt=11 Branch), where the prefix has (tid3 cnt=10 \
                     Branch)",
                )),
            ),
            (
                &with_drifted,
                &without_drifted,
                Some((
                    true,
                    "its event #1 is (tid5 cnt=5 Branch), where the prefix has (tid5 cnt=4 Branch)",
                )),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
            options.tmp_dir = Some(dir.path().to_path_buf());
            let (baseline, baseline_path) = recorded_continuation_for_test(
                &options,
                "final_baseline_schedule",
                &schedule,
                2,
                false,
                continued,
            );
            let baseline_bytes = std::fs::read(&baseline_path).unwrap();
            let mut target_artifact = None;
            let replays = std::cell::RefCell::new(Vec::new());
            let result = options.report_timing_decision_with(
                &schedule,
                2,
                None,
                (true, &None),
                (&baseline.schedule, &baseline_path, &mut None),
                (
                    |record, len| {
                        assert_eq!(len, 3);
                        let (completed, path) = recorded_continuation_for_test(
                            &options,
                            "final_target_schedule",
                            record,
                            len,
                            true,
                            target_continued,
                        );
                        target_artifact = Some((path.clone(), std::fs::read(&path)?));
                        Ok((completed, path, None))
                    },
                    |runname, record, _, _| {
                        replays
                            .borrow_mut()
                            .push((runname.to_owned(), record.clone()));
                        Ok((runname == "final_target_next_for_stacktraces", None))
                    },
                ),
            );
            let (target_path, target_bytes) =
                target_artifact.expect("the target continuation was recorded");
            for (path, bytes) in [
                (&target_path, &target_bytes),
                (&baseline_path, &baseline_bytes),
            ] {
                assert_eq!(
                    &std::fs::read(path).unwrap(),
                    bytes,
                    "retain completed diagnostics"
                );
            }
            match refused {
                Some((target, drift)) => {
                    let path = if target { &target_path } else { &baseline_path };
                    assert_unattached_checkpoints(result, path.to_str().unwrap(), drift);
                    assert!(replays.borrow().is_empty(), "no stack-trace replay may run");
                }
                None => {
                    assert!(result.unwrap().is_some());
                    let replays = replays.into_inner();
                    let names: Vec<&str> = replays.iter().map(|(name, _)| name.as_str()).collect();
                    assert_eq!(
                        names,
                        [
                            "final_target_next_for_stacktraces",
                            "final_baseline_next_for_stacktraces"
                        ]
                    );
                    let [(_, with_record), (_, without_record)] = &replays[..] else {
                        unreachable!()
                    };
                    assert_eq!(with_record.schedevents(), &with);
                    assert_eq!(without_record.schedevents(), &without);
                    // The first three events reach both checkpoints thread 3
                    // and thread 5 end there; the first two only thread 3's,
                    // as thread 5's slice still runs at their end.
                    assert_eq!(
                        checkpoint_counts(with_record),
                        [("3".to_owned(), 1), ("5".to_owned(), 1)]
                    );
                    assert_eq!(checkpoint_counts(without_record), [("3".to_owned(), 1)]);
                    assert_prefix_metadata(with_record, &schedule, 3);
                    assert_prefix_metadata(without_record, &schedule, 2);
                    for record in [with_record, without_record] {
                        assert_eq!(record.chaos_timeslice(), Some(4321));
                        record.validate().unwrap();
                    }
                }
            }
        }
    }

    fn critical(tids: &[i32], index: usize) -> CriticalSchedule {
        let schedule: Vec<SchedEvent> = tids
            .iter()
            .map(|&tid| SchedEvent::branches(detcore::types::DetTid::from_raw(tid), 1))
            .collect();
        CriticalSchedule {
            failing_schedule: schedule.clone(),
            passing_schedule: schedule,
            critical_event_index: index,
        }
    }

    /// A `--prefix-search` trial replays the target record's first events
    /// together with the callback checkpoints of the slices those events
    /// end, so its slices end where the recording's did; a checkpoint past
    /// the prefix is dropped, and so is the one at its end while an event
    /// follows: that slice end is the next event's decision.
    /// A check of the baseline keeps the checkpoints of the baseline's own
    /// prefix, one event shorter than the target's, and neither keeps any
    /// past where the edit starts.
    #[test]
    fn a_check_keeps_the_checkpoints_of_its_own_runs_prefix() {
        assert_eq!(check_prefix_len(10, "target", 20), 10);
        assert_eq!(check_prefix_len(10, "baseline", 20), 9);
        assert_eq!(check_prefix_len(10, "target", 4), 4);
        assert_eq!(check_prefix_len(10, "baseline", 4), 4);
    }

    #[test]
    fn a_prefix_trial_keeps_the_target_checkpoints_within_its_prefix() {
        let parent = tempfile::tempdir().unwrap();
        let tmp_dir = format!("--tmp-dir={}", parent.path().display());
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", tmp_dir.as_str(), "--", "/bin/true"]).unwrap();
        let _workspace = options.phase0_initialize(Retention::Remove).unwrap();
        let branches = |tid: i32, count: u32| {
            serde_json::json!({
                "dettid": tid, "op": "Branch", "count": count,
                "start_rip": null, "end_rip": null, "end_time": null
            })
        };
        let checkpoint = |row: usize, progress: u64, time: u64| {
            serde_json::json!({
                "history_index": row,
                "origin": {
                    "callback": "PmuTimer", "site": "Prehook", "progress": progress,
                    "last_op": "Branch", "time": time, "rcbs": progress
                }
            })
        };
        let target: PreemptionRecord = serde_json::from_value(serde_json::json!({
            "per_thread": {
                "3": {
                    "final_prio": 1000,
                    "prio_changes": [[100, 1000], [300, 1000]],
                    "preemption_rcbs": [10, 15]
                },
                "5": {"final_prio": 1000, "prio_changes": [[200, 1000]], "preemption_rcbs": [4]}
            },
            "global": [branches(3, 10), branches(5, 4), branches(3, 5)],
            "callback_checkpoints": {
                "3": [checkpoint(0, 10, 100), checkpoint(1, 15, 300)],
                "5": [checkpoint(0, 4, 200)]
            }
        }))
        .unwrap();
        target.validate().unwrap();
        for (len, kept) in [(0, 0), (1, 0), (2, 1), (3, 3)] {
            let mut run = options
                .prefix_trial("prefix_round", &target, len, None)
                .unwrap();
            let written: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(run.sched_path_in()).unwrap())
                    .unwrap();
            assert_eq!(written["global"].as_array().unwrap().len(), len);
            let points: usize = ["3", "5"]
                .iter()
                .map(|tid| {
                    written["callback_checkpoints"][tid]
                        .as_array()
                        .map_or(0, |points| points.len())
                })
                .sum();
            assert_eq!(points, kept, "prefix of {len}: {written}");
            PreemptionReader::new(run.sched_path_in())
                .load_all()
                .validate()
                .unwrap();
        }
    }

    /// A prefix stack trial asks for its stack by schedule position
    /// (`--stacktrace-at-schedule-positions`), not by traced index: a trial
    /// that makes a polling retry its recording did not make would otherwise
    /// print a neighbouring event's stack
    /// (https://github.com/rrnewton/hermit/issues/4917). The request names the
    /// position, its stack file, and the replayed prefix, and the trial's
    /// repro command carries the option.
    #[test]
    fn a_prefix_stack_trial_requests_its_stack_by_schedule_position() {
        let parent = tempfile::tempdir().unwrap();
        let tmp_dir = format!("--tmp-dir={}", parent.path().display());
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", tmp_dir.as_str(), "--", "/bin/true"]).unwrap();
        let _workspace = options.phase0_initialize(Retention::Remove).unwrap();
        let tid = detcore::types::DetTid::from_raw(3);
        let target = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(tid, 10),
            SchedEvent::branches(tid, 4),
            SchedEvent::branches(tid, 5),
        ]);
        let (run, stack_path) = options
            .prefix_stack_trial("final_target_for_stacktraces", &target, 3, 2, None)
            .unwrap();
        let config = &run.runopts.det_opts.det_config;
        assert!(config.stacktrace_at_schedule_positions);
        assert_eq!(config.stacktrace_event, vec![(2, Some(stack_path))]);
        assert!(config.replay_schedule_from.is_some());
        assert!(
            run.to_repro()
                .contains(" --stacktrace-at-schedule-positions"),
            "{}",
            run.to_repro()
        );
    }

    /// A candidate's check keeps the callback checkpoints of the unchanged
    /// events it replays, and refuses an edit they do not fit instead of
    /// replaying a bare schedule.
    #[test]
    fn a_candidate_check_refuses_checkpoints_that_do_not_fit_its_edit() {
        let branches = |tid: i32, count: u32| {
            serde_json::json!({
                "dettid": tid, "op": "Branch", "count": count,
                "start_rip": null, "end_rip": null, "end_time": null
            })
        };
        let checkpoint = |row: usize, progress: u64, time: u64| {
            serde_json::json!({
                "history_index": row,
                "origin": {
                    "callback": "PmuTimer", "site": "Prehook", "progress": progress,
                    "last_op": "Branch", "time": time, "rcbs": progress
                }
            })
        };
        let recorded: PreemptionRecord = serde_json::from_value(serde_json::json!({
            "per_thread": {
                "3": {
                    "final_prio": 1000,
                    "prio_changes": [[100, 1000], [300, 1000]],
                    "preemption_rcbs": [10, 15]
                },
                "5": {"final_prio": 1000, "prio_changes": [[200, 1000]], "preemption_rcbs": [4]}
            },
            "global": [branches(3, 10), branches(5, 4), branches(3, 5)],
            "callback_checkpoints": {
                "3": [checkpoint(0, 10, 100), checkpoint(1, 15, 300)],
                "5": [checkpoint(0, 4, 200)]
            }
        }))
        .unwrap();
        recorded.validate().unwrap();
        let event = |value| serde_json::from_value::<SchedEvent>(value).unwrap();
        // The first two events unchanged, then the edit: their checkpoints
        // carry over.
        let fits = [branches(3, 10), branches(5, 4), branches(5, 2)].map(event);
        let kept = candidate_check_record(&recorded, 2, &fits).unwrap();
        assert_eq!(kept.schedevents(), &fits[..]);
        assert_eq!(
            serde_json::to_value(&kept).unwrap()["callback_checkpoints"]["3"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        // An edit that leaves out an unchanged event the checkpoints name.
        let error = candidate_check_record(&recorded, 2, &[event(branches(5, 4))]).unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::app::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        let error = error.to_string();
        assert!(
            error.starts_with(
                "HERMIT_CANDIDATE_CHECK_CHECKPOINTS: the callback checkpoints of the recording's \
                 first 2 events do not fit"
            ),
            "{error}"
        );
    }

    /// The final prefix report repeats the trial's counter caveat in its
    /// JSON and immediately below its banner, and omits it for exact counters.
    #[test]
    fn a_prefix_report_keeps_the_branch_counter_caveat_in_json_and_under_its_banner() {
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", "--prefix-search", "--", "/bin/true"]).unwrap();
        let schedule = vec![SchedEvent::branches(
            detcore::types::DetTid::from_raw(3),
            10,
        )];
        for inexact in [false, true] {
            options.inexact_branch_counter = if inexact {
                || Some("SpecLockMap is enabled".to_owned())
            } else {
                || None
            };
            let mut warnings = Vec::new();
            options
                .admit_branch_counter(&mut |line| warnings.push(line.to_owned()))
                .unwrap();
            assert_eq!(warnings.len(), usize::from(inexact));
            let expected = warnings.first().map(|line| line.trim_end());
            let report = options.prefix_report(&schedule, 0, &schedule, ([None, None], None), None);
            let json = serde_json::to_value(&report).unwrap();
            assert_eq!(
                json.get("branch_counter_warning")
                    .and_then(|value| value.as_str()),
                expected
            );
            let banner = prefix_report_banner(&report);
            let mut lines = banner.lines();
            assert_eq!(lines.next(), Some(""));
            assert_eq!(
                lines.next(),
                Some(
                    "------------------------------ hermit analyze report ------------------------------"
                )
            );
            assert_eq!(lines.next(), expected);
            assert_eq!(lines.next(), None);
        }
    }

    /// The sentences a `--prefix-search` report gives its stacks present two
    /// threads at the deciding event as the pair whose order decides the
    /// outcome, and one thread, which the scheduler runs there without the
    /// event too, as that one thread: never as a race with itself.
    #[test]
    fn a_prefix_report_says_when_both_stacks_are_one_thread() {
        let (reader, binder) = (
            detcore::types::DetTid::from_raw(7),
            detcore::types::DetTid::from_raw(9),
        );
        let pair = stack_pair_sentences(binder, Some(reader));
        assert!(
            pair.contains("stack2 is the thread that runs there instead"),
            "{pair}"
        );
        assert!(pair.contains("which of the two runs first"), "{pair}");

        let one = stack_pair_sentences(reader, Some(reader));
        assert!(one.contains("both belong to thread 7"), "{one}");
        assert!(one.contains("not two threads racing"), "{one}");
        for race in ["runs there instead", "which of the two runs first"] {
            assert!(!one.contains(race), "{race:?} in\n{one}");
        }

        // A run without the event that ended before it keeps the usual
        // sentences: its stack2 is empty, and a warning says why.
        assert_eq!(stack_pair_sentences(reader, None), pair);
    }

    /// The timing report names the continuations' recorded schedules in the
    /// workspace. The report outlives a workspace analyze removes, so the
    /// sentence then says the files are gone and how to keep them; with
    /// --keep-tmp or --verbose it does not.
    #[test]
    fn the_report_says_when_the_files_it_names_are_removed() {
        let removed =
            continuation_files_sentence(3, "with.events", "without.events", Retention::Remove);
        assert!(
            removed.starts_with(
                "Event numbers after #3 are positions in each continuation's recorded schedule, \
                 in the workspace: with.events with the event, without.events without it."
            ) && removed.contains("Analyze removes its workspace when it finishes")
                && removed.contains("--keep-tmp"),
            "{removed}"
        );
        for kept in ["--keep-tmp", "--verbose"] {
            let sentence = continuation_files_sentence(
                3,
                "with.events",
                "without.events",
                Retention::Keep {
                    because: Some(kept),
                },
            );
            assert!(!sentence.contains("removes"), "{sentence}");
        }
    }

    /// A bisection trial replays its schedule with the callback checkpoints,
    /// per-thread histories and epoch of the endpoint record it shares the
    /// longest prefix with (on a tie, one it replays whole, then the first),
    /// as far as that prefix reaches: an endpoint replayed whole keeps all of
    /// its checkpoints, and one that shares nothing is written as events
    /// alone. A schedule edited right after its first event keeps none of the
    /// target's checkpoints, though it takes the target's histories and
    /// epoch: the one at that event's progress ended its slice, which decides
    /// the edited event, so the scheduler decides it, as in a prefix trial
    /// (`PreemptionRecord::schedule_prefix`).
    #[test]
    fn a_bisection_trial_keeps_the_checkpoints_of_the_part_it_shares_with_an_endpoint() {
        let parent = tempfile::tempdir().unwrap();
        let tmp_dir = format!("--tmp-dir={}", parent.path().display());
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", tmp_dir.as_str(), "--", "/bin/true"]).unwrap();
        let retention = options.workspace_retention();
        let _workspace = options.phase0_initialize(retention).unwrap();
        let branches = |tid: u64, count: u32| {
            serde_json::json!({
                "dettid": tid, "op": "Branch", "count": count,
                "start_rip": null, "end_rip": null, "end_time": null
            })
        };
        let checkpoint = |row: usize, progress: u64, time: u64| {
            serde_json::json!({
                "history_index": row,
                "origin": {
                    "callback": "PmuTimer", "site": "Prehook", "progress": progress,
                    "last_op": "Branch", "time": time, "rcbs": progress
                }
            })
        };
        let target: PreemptionRecord = serde_json::from_value(serde_json::json!({
            "per_thread": {"3": {
                "final_prio": 1000,
                "prio_changes": [[100, 1000], [200, 1000]],
                "preemption_rcbs": [10, 15]
            }},
            "global": [branches(3, 10), branches(3, 5)],
            "epoch": "2026-01-01T00:00:00Z",
            "callback_checkpoints": {"3": [checkpoint(0, 10, 100), checkpoint(1, 15, 200)]}
        }))
        .unwrap();
        target.validate().unwrap();
        let events = |list: Vec<serde_json::Value>| -> Vec<SchedEvent> {
            serde_json::from_value(serde_json::Value::Array(list)).unwrap()
        };
        // The baseline has the same events and no checkpoints: on the tie the
        // target, listed first, is the source.
        let baseline = PreemptionRecord::from_sched_events(target.schedevents().clone());
        let path = parent.path().join("trial.preempts");
        // A second endpoint whose schedule only starts with the target's
        // first event, and keeps no checkpoints.
        let short = PreemptionRecord::from_sched_events(events(vec![branches(3, 10)]));
        // (sources, schedule, checkpoints kept, whether the target's histories
        // and epoch come with them)
        for (sources, schedule, kept, from_target) in [
            (
                [&target, &baseline],
                events(vec![branches(3, 10), branches(3, 5)]),
                2,
                true,
            ),
            (
                [&target, &baseline],
                events(vec![branches(3, 10), branches(5, 5), branches(3, 5)]),
                0,
                true,
            ),
            (
                [&target, &baseline],
                events(vec![branches(5, 5), branches(3, 10)]),
                0,
                false,
            ),
            // The longer shared prefix wins, whichever source is listed first.
            (
                [&short, &target],
                events(vec![branches(3, 10), branches(3, 5)]),
                2,
                true,
            ),
            // A source that ends where the schedule goes on keeps its last
            // slice end: no event of its own follows it.
            (
                [&target, &short],
                events(vec![branches(3, 10), branches(3, 5), branches(5, 1)]),
                2,
                true,
            ),
            // On a tie, the source replayed whole wins.
            ([&target, &short], events(vec![branches(3, 10)]), 0, false),
        ] {
            options
                .write_trial_schedule_from(schedule.clone(), &sources, &path)
                .unwrap();
            let written = PreemptionReader::new(&path).load_all();
            written.validate().unwrap();
            assert_eq!(written.schedevents(), &schedule);
            let points = serde_json::to_value(&written).unwrap()["callback_checkpoints"]["3"]
                .as_array()
                .map_or(0, |points| points.len());
            assert_eq!(points, kept, "{schedule:?}");
            assert_eq!(written.epoch().is_some(), from_target, "{schedule:?}");
            assert_eq!(
                serde_json::to_value(&written).unwrap()["per_thread"]
                    .as_object()
                    .map_or(0, |rows| rows.len()),
                usize::from(from_target),
                "{schedule:?}"
            );
        }
    }

    /// A workspace kept for the level `--guest-log` gave names the option, and
    /// HERMIT_LOG too when the level came from that variable.
    #[test]
    fn a_workspace_kept_for_the_guest_log_level_names_its_source() {
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", "-g", "debug", "--", "/bin/true"]).unwrap();
        assert_eq!(
            options.workspace_retention(),
            Retention::Keep {
                because: Some("--guest-log")
            }
        );
        options.guest_log_from_env = true;
        assert_eq!(
            options.workspace_retention(),
            Retention::Keep {
                because: Some("--guest-log (HERMIT_LOG)")
            }
        );
    }

    /// `--tmp-dir` names the directory the workspace is created in, and is
    /// created if it is missing. The workspace goes when analyze is done
    /// with it, unless `--keep-tmp`, `--verbose`, or `--trial-log` above warn
    /// keeps it (not `--selfcheck`, though it writes run logs there too);
    /// the `--tmp-dir` stays.
    #[test]
    fn the_workspace_is_created_in_the_tmp_dir() {
        for (option, kept) in [
            (None, None),
            (Some("--keep-tmp"), Some("--keep-tmp")),
            (Some("--verbose"), Some("--verbose")),
            (Some("--selfcheck"), None),
            (Some("--trial-log=debug"), Some("--trial-log")),
            (Some("--trial-log=warn"), None),
        ] {
            let parent = tempfile::tempdir().unwrap();
            let requested = parent.path().join("not-yet-created");
            let tmp_dir = format!("--tmp-dir={}", requested.display());
            let mut argv = vec!["analyze", tmp_dir.as_str(), "--", "/bin/true"];
            argv.splice(1..1, option);
            let mut options = AnalyzeOpts::try_parse_from(argv).unwrap();
            let retention = options.workspace_retention();
            assert_eq!(
                retention,
                match kept {
                    Some(because) => Retention::Keep {
                        because: Some(because),
                    },
                    None => Retention::Remove,
                },
                "{option:?}"
            );
            let guard = options.phase0_initialize(retention).unwrap();
            let workspace = options.get_tmp().unwrap().to_path_buf();
            assert_eq!(
                workspace.parent(),
                Some(std::fs::canonicalize(&requested).unwrap().as_path())
            );
            assert!(workspace.is_dir(), "{}", workspace.display());
            drop(guard);
            assert_eq!(
                workspace.exists(),
                kept.is_some(),
                "{option:?}: {}",
                workspace.display()
            );
            assert!(requested.is_dir(), "{}", requested.display());
        }

        // Through a symlink, the workspace is named by its canonical path.
        let parent = tempfile::tempdir().unwrap();
        let real = parent.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let tmp_dir = format!("--tmp-dir={}", link.display());
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", tmp_dir.as_str(), "--", "/bin/true"]).unwrap();
        let _guard = options.phase0_initialize(Retention::Remove).unwrap();
        let workspace = options.get_tmp().unwrap();
        assert_eq!(
            workspace.parent(),
            Some(std::fs::canonicalize(&real).unwrap().as_path()),
            "{}",
            workspace.display()
        );
    }

    /// The configuration the default search runs with lets two replays of
    /// one schedule differ by a few branches and still count as one trace
    /// only on a host whose branch counter is not exact.
    #[test]
    fn replays_tolerate_branch_skid_only_on_an_inexact_counter() {
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.inexact_branch_counter = || None;
        assert_eq!(
            options.bisect_config().max_branch_count_jitter,
            None,
            "an exact counter"
        );
        options.inexact_branch_counter = || Some("SpecLockMap is enabled".to_string());
        assert_eq!(
            options.bisect_config().max_branch_count_jitter,
            Some(INEXACT_COUNTER_BRANCH_JITTER)
        );
    }

    /// The prefix search finds the event at which the outcome flips, wherever
    /// it lies, in about log2(len) trials once its endpoints differ.
    #[test]
    fn the_prefix_search_finds_the_deciding_event() {
        for (len, decision) in [
            (1, 0),
            (2, 1),
            (37_710, 0),
            (37_710, 33_225),
            (37_710, 37_709),
        ] {
            let mut trials = 0;
            let found = search_prefix_boundary(len, |prefix| {
                trials += 1;
                Ok(prefix > decision)
            })
            .unwrap();
            assert_eq!(found, decision, "len {len}");
            assert!(
                trials <= (len as f64).log2().ceil() as usize,
                "{trials} trials"
            );
        }
    }

    /// `auto` continues at the run's own timeslice, then at hermit's default
    /// 200 ms when that is coarser, never at a shorter one; a run without PMU
    /// timeslices continues as it is.
    #[test]
    fn auto_continuations_only_get_coarser() {
        let ns = |nanos: u64| NonZeroU64::new(nanos);
        assert_eq!(continuation_candidates(ns(200_000_000)), [ns(200_000_000)]);
        assert_eq!(
            continuation_candidates(ns(1_000)),
            [ns(1_000), ns(200_000_000)]
        );
        assert_eq!(continuation_candidates(ns(500_000_000)), [ns(500_000_000)]);
        assert_eq!(continuation_candidates(None), [None]);
    }

    #[test]
    fn an_empty_target_schedule_is_refused_before_endpoint_trials() {
        let mut trials = 0;
        let error = choose_continuation(&[NonZeroU64::new(1000)], 0, |_, _| {
            trials += 1;
            Ok(false)
        })
        .unwrap_err();
        assert_eq!(trials, 0, "an empty schedule has no distinct endpoints");
        assert!(
            error.to_string().contains("target schedule is empty"),
            "{error}"
        );
        assert!(!error.to_string().contains("replay"), "{error}");
    }

    #[test]
    fn a_drawn_prefix_applies_its_chosen_continuation_at_launch() {
        use clap::CommandFactory as _;

        let dir = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from([
            "analyze",
            "--prefix-search",
            "--",
            "--max-timeslice=4321",
            "/bin/true",
        ])
        .unwrap();
        options.tmp_dir = Some(dir.path().to_path_buf());
        let target = PreemptionRecord::from_sched_events(vec![SchedEvent::branches(
            detcore::types::DetTid::from_raw(3),
            1,
        )])
        .with_chaos_timeslice(Some(4321));
        for nanos in [4321, 200_000_000] {
            let chosen = NonZeroU64::new(nanos);
            let mut run = options
                .prefix_trial("drawn_prefix", &target, 1, chosen)
                .unwrap();
            let written = PreemptionReader::try_new(run.sched_path_in())
                .unwrap()
                .into_inner();
            assert_eq!(written.chaos_timeslice(), Some(nanos));
            assert_eq!(written.schedevents(), target.schedevents());
            for _ in 0..2 {
                run.runopts.validate_args_with_perf_support(true).unwrap();
                assert_eq!(run.runopts.det_opts.det_config.max_timeslice, chosen);
            }
            let command = shell_words::split(&format!("hermit run {}", run.runopts)).unwrap();
            let matches = crate::app::Args::command()
                .try_get_matches_from(&command)
                .unwrap();
            let args = crate::app::args_from_matches_with_clock(&matches, || {
                panic!("the reproducer must preserve its fixed epoch");
            })
            .unwrap();
            let crate::app::Subcommand::Run(mut replay) = args.command else {
                panic!("not a run command");
            };
            replay.validate_args_with_perf_support(true).unwrap();
            assert_eq!(replay.det_opts.det_config.max_timeslice, chosen);
        }
        assert_eq!(
            target.chaos_timeslice(),
            Some(4321),
            "keep the source record intact"
        );

        let original = dir.path().join("original.json");
        target.write_to_disk(&original).unwrap();
        let replay_arg = format!("--replay-schedule-from={}", original.display());
        let matches = crate::app::Args::command()
            .try_get_matches_from([
                "hermit",
                "run",
                "--max-timeslice=200000000",
                &replay_arg,
                "/bin/true",
            ])
            .unwrap();
        let args =
            crate::app::args_from_matches_with_clock(&matches, || std::time::UNIX_EPOCH).unwrap();
        let crate::app::Subcommand::Run(mut mismatched) = args.command else {
            panic!("not a run command");
        };
        let error = mismatched
            .validate_args_with_perf_support(true)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("pass --max-timeslice=4321"),
            "{error:#}"
        );
    }

    #[test]
    fn prefix_candidates_use_the_recorded_and_resolved_maximum() {
        for (recorded, range, expected) in [
            (Some(4321), None, 4321),
            (Some(500_000_000), None, 500_000_000),
            (None, Some("1000..5000000"), 5_000_000),
            (None, Some("1000..500000000"), 500_000_000),
        ] {
            let mut argv = vec!["analyze".to_string(), "--prefix-search".into(), "--".into()];
            if let Some(range) = range {
                argv.extend(["--chaos".into(), format!("--chaos-timeslice-range={range}")]);
            }
            argv.push("/bin/true".into());
            let options = AnalyzeOpts::try_parse_from(argv).unwrap();
            let target = PreemptionRecord::from_sched_events(vec![SchedEvent::branches(
                detcore::types::DetTid::from_raw(3),
                1,
            )])
            .with_chaos_timeslice(recorded);
            let mut tried = Vec::new();
            let found = options
                .phase5_prefix_search_with(&target, |_, len, timeslice| {
                    tried.push(timeslice);
                    Ok(len != 0)
                })
                .unwrap();
            assert_eq!(found.continuation, NonZeroU64::new(expected));
            assert!(
                tried
                    .iter()
                    .all(|timeslice| *timeslice == NonZeroU64::new(expected))
            );
        }
        for (recorded, continuation, admitted) in [
            (4321, 4321, true),
            (4321, 4320, false),
            (500_000_000, 200_000_000, false),
        ] {
            let options = AnalyzeOpts::try_parse_from([
                "analyze".to_owned(),
                "--prefix-search".into(),
                format!("--continuation-timeslice={continuation}"),
                "--".into(),
                "/bin/true".into(),
            ])
            .unwrap();
            let target = PreemptionRecord::from_sched_events(vec![SchedEvent::branches(
                detcore::types::DetTid::from_raw(3),
                1,
            )])
            .with_chaos_timeslice(Some(recorded));
            let mut trials = 0;
            let found = options.phase5_prefix_search_with(&target, |_, len, timeslice| {
                trials += 1;
                assert_eq!(timeslice, NonZeroU64::new(continuation));
                Ok(len != 0)
            });
            if admitted {
                assert_eq!(found.unwrap().continuation, NonZeroU64::new(continuation));
                assert!(trials > 0);
            } else {
                let error = found
                    .err()
                    .expect("an unsound continuation must be refused");
                assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
                assert_eq!(trials, 0, "refuse before starting a prefix trial");
            }
        }
    }

    #[test]
    fn continuation_admission_follows_recorded_timeslice_adoption() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("drawn.json");
        for (recorded, continuation, admitted) in [
            (4321, 4321, true),
            (4321, 4320, false),
            (500_000_000, 500_000_000, true),
            (500_000_000, 200_000_000, false),
        ] {
            PreemptionRecord::from_sched_events(Vec::new())
                .with_chaos_timeslice(Some(recorded))
                .write_to_disk(&record)
                .unwrap();
            let args = crate::app::Args::try_parse_from([
                "hermit".to_owned(),
                "analyze".into(),
                "--prefix-search".into(),
                format!("--run1-preemptions={}", record.display()),
                format!("--continuation-timeslice={continuation}"),
                "--".into(),
                "/bin/true".into(),
            ])
            .unwrap();
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
                panic!("not analyze");
            };
            let prepared = options.prepare(&args.global);
            if admitted {
                prepared.unwrap();
                assert_eq!(
                    options.trial_max_timeslice().unwrap(),
                    NonZeroU64::new(recorded)
                );
            } else {
                let error = prepared.unwrap_err();
                assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
                assert!(
                    format!("{error:#}").contains(&format!(
                        "shorter than the run's --max-timeslice={recorded}"
                    )),
                    "{error:#}"
                );
            }
            assert!(
                options.tmp_dir.is_none(),
                "preflight must not create a workspace"
            );
        }
    }

    /// The search continues at the first timeslice whose endpoint trials
    /// differ: one at which the scheduler alone meets the target, or the
    /// whole schedule's replay misses it, is skipped. When every timeslice
    /// fails, the error names each, says what it showed, and what to do.
    #[test]
    fn the_prefix_search_continues_at_the_first_usable_timeslice() {
        let ns = |nanos: u64| NonZeroU64::new(nanos);
        let candidates = [ns(1_000), ns(200_000_000)];
        // At 1000 ns the scheduler alone already meets the target (a race
        // found only at a short timeslice); at 200 ms it does not.
        let mut trials = Vec::new();
        let chosen = choose_continuation(&candidates, 50, |prefix, timeslice| {
            trials.push((prefix, timeslice));
            Ok(prefix > 0 || timeslice == ns(1_000))
        })
        .unwrap();
        assert_eq!(chosen, ns(200_000_000));
        assert_eq!(
            trials,
            [(0, ns(1_000)), (0, ns(200_000_000)), (50, ns(200_000_000))]
        );
        assert_eq!(
            choose_continuation(&candidates, 50, |prefix, _| Ok(prefix > 0)).unwrap(),
            ns(1_000),
            "the first usable one wins"
        );

        let alone = choose_continuation(&candidates, 50, |_, _| Ok(true)).unwrap_err();
        // A completed analysis that names no event, not a failure.
        assert!(
            matches!(
                alone.downcast_ref::<AnalyzeVerdict>(),
                Some(AnalyzeVerdict::NoDecidingEvent(_))
            ),
            "{alone:#}"
        );
        let message = alone.to_string();
        assert!(
            message.contains("the scheduler alone already meets the target criteria at a 1000 ns timeslice and at a 200000000 ns timeslice"),
            "{message}"
        );
        assert!(
            message.contains("prefix_round_001.repro") && message.contains("--run2-seed"),
            "{message}"
        );
        let replay = choose_continuation(&candidates, 50, |_, _| Ok(false)).unwrap_err();
        assert!(
            replay.to_string().contains(
                "at a 1000 ns timeslice replaying the whole target schedule misses the target"
            ),
            "{replay}"
        );
        assert!(replay.to_string().contains("--selfcheck"), "{replay}");
        assert_eq!(
            replay
                .downcast_ref::<AnalyzePrecondition>()
                .map(|precondition| precondition.class.name()),
            Some("no-prefix-continuation"),
            "{replay:#}"
        );
        assert_eq!(crate::failure_exit_code(&replay), 2);
        assert_eq!(
            crate::classify_failure(&replay),
            "HERMIT_ANALYZE_PRECONDITION class=no-prefix-continuation"
        );
    }

    /// A prefix search whose empty prefix already meets the target concludes
    /// that no event decides the outcome, naming a coarser continuation it did
    /// not try.
    #[test]
    fn an_empty_prefix_that_meets_the_target_is_a_verdict() {
        let ns = |nanos: u64| NonZeroU64::new(nanos);
        let candidates = [ns(1_000), ns(200_000_000)];
        let error = Error::new(EmptyPrefixMeetsTarget);
        let AnalyzeVerdict::NoDecidingEvent(explanation) =
            empty_prefix_verdict(&error, &candidates, ns(1_000));
        assert!(
            explanation.starts_with("no event of the target schedule decides the outcome: ")
                && explanation
                    .contains("The search did not try continuing at a 200000000 ns timeslice"),
            "{explanation}"
        );
        let AnalyzeVerdict::NoDecidingEvent(explanation) =
            empty_prefix_verdict(&error, &candidates, ns(200_000_000));
        assert!(!explanation.contains("did not try"), "{explanation}");
    }

    /// A verdict that names no event is reported as a completed analysis:
    /// its report carries `"verdict": "no_deciding_event"` and the
    /// explanation, and analyze exits as it does after naming an event (0, or
    /// `--success-exit-code`). `main.rs` never sees it as an error.
    #[test]
    fn a_verdict_is_reported_and_ends_analyze_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("report.json");
        let verdict = AnalyzeVerdict::NoDecidingEvent("no event decides the outcome".into());
        for (extra, status) in [
            (&[][..], ExitStatus::SUCCESS),
            (&["--success-exit-code=3"][..], ExitStatus::Exited(3)),
        ] {
            let mut argv = vec![
                "analyze".to_string(),
                format!("--report-file={}", report.display()),
            ];
            argv.extend(extra.iter().map(|flag| flag.to_string()));
            argv.extend(["--".to_string(), "/bin/true".to_string()]);
            let options = AnalyzeOpts::try_parse_from(argv).unwrap();
            // Through the prefix search's end, as analyze reaches it.
            let searched = Err(Error::new(verdict.clone()));
            assert_eq!(options.finish_prefix_search(searched).unwrap(), status);
            let text = std::fs::read_to_string(&report).unwrap();
            let written: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                written,
                serde_json::json!({
                    "verdict": "no_deciding_event",
                    "header": "no event decides the outcome",
                    "additional_context": "",
                    "baseline_run": false,
                })
            );
            // One schema reads both kinds of report.
            let parsed: Report = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed.verdict, Verdict::NoDecidingEvent);
            assert_eq!(parsed.critical_event1, None);
            std::fs::remove_file(&report).unwrap();
        }
        // Any other end of the search ends the analysis, writing no report.
        let options = AnalyzeOpts::try_parse_from([
            "analyze".to_string(),
            format!("--report-file={}", report.display()),
            "--".to_string(),
            "/bin/true".to_string(),
        ])
        .unwrap();
        let other = options
            .finish_prefix_search(Err(
                AnalyzePrecondition::no_prefix_continuation("none").into()
            ))
            .unwrap_err();
        assert!(
            other.downcast_ref::<AnalyzePrecondition>().is_some(),
            "{other:#}"
        );
        assert!(!report.exists());
        assert_eq!(
            verdict.class_line(),
            "HERMIT_ANALYZE_VERDICT class=no-deciding-event"
        );
    }

    /// A first run that misses the target ends the analysis without --search,
    /// an input that gives it no target run (exit 2, `target-not-reproduced`),
    /// and searches with it.
    #[test]
    fn a_first_run_that_misses_ends_the_analysis_or_searches() {
        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let mut searched = false;
        let error = options
            .after_a_first_run_that_missed(|| {
                searched = true;
                bail!("searched")
            })
            .err()
            .unwrap();
        assert!(!searched);
        assert_eq!(crate::failure_exit_code(&error), 2, "{error:#}");
        assert_eq!(
            crate::classify_failure(&error),
            "HERMIT_ANALYZE_PRECONDITION class=target-not-reproduced"
        );
        assert!(error.to_string().contains("Try --search"), "{error}");

        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--search", "--", "/bin/true"]).unwrap();
        let error = options
            .after_a_first_run_that_missed(|| bail!("searched"))
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "searched");
    }

    /// Both of the bisection's ends that no single reordering separates the
    /// endpoints send analyze to a prefix search; other ends do not. An
    /// endpoint of analyze's own that did not reproduce is pointed at replay.
    #[test]
    fn bisection_ends_are_answered_by_their_kind() {
        for swaps_left in [0, 1] {
            assert!(no_swap_separates(&Error::new(NoSingleSwap {
                unshared_events: 2,
                swaps_left,
                trimmed_failing_passed: false,
            })));
        }
        assert!(!no_swap_separates(&Error::new(
            AnalyzePrecondition::target_not_reproduced("no")
        )));

        for precondition in [
            AnalyzePrecondition::target_not_reproduced("the failing endpoint did not."),
            AnalyzePrecondition::baseline_not_reproduced("the passing endpoint did not."),
        ] {
            let class = precondition.class;
            let pointed = point_endpoint_misses_at_replay(precondition.into());
            let pointed_precondition = pointed.downcast_ref::<AnalyzePrecondition>().unwrap();
            assert_eq!(pointed_precondition.class, class);
            let message = pointed.to_string();
            assert!(
                message.contains("--selfcheck") && message.contains("hermit host-capabilities"),
                "{message}"
            );
        }
        let other = point_endpoint_misses_at_replay(anyhow::anyhow!("other"));
        assert_eq!(other.to_string(), "other");
    }

    /// Analyze's own bisection goes on to a prefix search when no swap
    /// separates its endpoints, and an endpoint that did not reproduce ends it
    /// pointed at replay: --selfcheck and `hermit host-capabilities`, with the
    /// class kept (exit 2). Other errors pass through unchanged.
    #[test]
    fn analyze_own_bisection_points_endpoint_misses_at_replay() {
        let separable = own_bisection_end(Err(Error::new(NoSingleSwap {
            unshared_events: 1,
            swaps_left: 0,
            trimmed_failing_passed: false,
        })));
        assert!(matches!(separable, Ok(OwnBisectionEnd::SearchPrefixes(_))));
        for (precondition, class) in [
            (
                AnalyzePrecondition::target_not_reproduced(
                    "the failing schedule endpoint did not reproduce the target failure",
                ),
                "target-not-reproduced",
            ),
            (
                AnalyzePrecondition::baseline_not_reproduced(
                    "the passing schedule endpoint did not reproduce a passing outcome",
                ),
                "baseline-not-reproduced",
            ),
        ] {
            let Err(error) = own_bisection_end(Err(precondition.into())) else {
                panic!("an endpoint miss ended no analysis")
            };
            let message = error.to_string();
            assert!(
                message.contains("--selfcheck") && message.contains("`hermit host-capabilities`"),
                "{message}"
            );
            assert_eq!(crate::failure_exit_code(&error), 2);
            assert_eq!(
                crate::classify_failure(&error),
                format!("HERMIT_ANALYZE_PRECONDITION class={class}")
            );
        }
        let Err(other) = own_bisection_end(Err(anyhow::anyhow!("other"))) else {
            panic!("an error ended no analysis")
        };
        assert_eq!(other.to_string(), "other");
    }

    /// --selfcheck's differences are replay not reproducing, Hermit's or the
    /// host's: exit 125 with `class=replay-not-reproducible`, never an input
    /// analyze cannot use. The first difference found is reported.
    #[test]
    fn a_selfcheck_difference_is_replay_not_reproducing() {
        assert!(selfcheck_conclusion(true, true, || Ok(())).is_ok());
        for (matched, logs, fixed_point, expected) in [
            (false, true, true, "second run did not"),
            (true, false, true, "Log differences found"),
            (true, true, false, "no fixed point"),
        ] {
            let error = selfcheck_conclusion(matched, logs, || {
                if fixed_point {
                    Ok(())
                } else {
                    Err("no fixed point".to_string())
                }
            })
            .unwrap_err();
            assert!(
                error.to_string().contains(expected)
                    && error.to_string().contains("hermit host-capabilities"),
                "{error}"
            );
            assert_eq!(crate::failure_exit_code(&error), 125);
            assert_eq!(
                crate::classify_failure(&error),
                "HERMIT_INTERNAL_FAILURE class=replay-not-reproducible"
            );
        }
    }

    /// `--success-exit-code` refuses the statuses that already mean something
    /// for analyze, naming why.
    #[test]
    fn a_success_exit_code_that_means_something_else_is_refused() {
        for code in ["2", "120", "122", "125", "127"] {
            let error = AnalyzeOpts::try_parse_from([
                "analyze",
                "--success-exit-code",
                code,
                "--",
                "/bin/true",
            ])
            .unwrap_err()
            .to_string();
            assert!(
                error.contains(&format!("{code} is")) && error.contains("could not be told apart"),
                "{code}: {error}"
            );
        }
        for code in ["0", "1", "3", "119", "128"] {
            let options = AnalyzeOpts::try_parse_from([
                "analyze",
                "--success-exit-code",
                code,
                "--",
                "/bin/true",
            ])
            .unwrap();
            assert_eq!(options.success_exit_code, Some(code.parse().unwrap()));
        }
    }

    /// Prefix outcomes need not be monotone. A guest whose thread B stores 2
    /// twice and whose thread A stores 1 once, printing the final value, can
    /// meet a target of "2" with none and all of the schedule replayed, yet
    /// miss it at a cut between B's stores. The endpoint trials of such a
    /// schedule try only the empty prefix, so when it meets the target at
    /// every continuation the error states that observation (there is no miss
    /// to bisect against) and claims nothing about the prefixes no trial ran.
    /// Where the endpoints differ, interior flips do not matter: the search
    /// still ends on an adjacent pair whose outcomes differ.
    #[test]
    fn an_empty_prefix_that_meets_the_target_claims_nothing_about_longer_ones() {
        let ns = |nanos: u64| NonZeroU64::new(nanos);
        let candidates = [ns(1_000), ns(200_000_000)];
        let len = 50;
        // Met with 0 and with all 50 events replayed, missed for 20 to 29.
        let interior_flip = |prefix: usize| !(20..30).contains(&prefix);
        let mut trials = Vec::new();
        let error = choose_continuation(&candidates, len, |prefix, timeslice| {
            trials.push((prefix, timeslice));
            Ok(interior_flip(prefix))
        })
        .unwrap_err();
        assert_eq!(
            trials,
            [(0, ns(1_000)), (0, ns(200_000_000))],
            "only the empty prefix runs at each continuation"
        );
        let message = error.to_string();
        for claim in ["decides the outcome", "does not depend on a chaos decision"] {
            assert!(
                !message.contains(claim),
                "one trial per continuation cannot establish {claim:?}: {message}"
            );
        }
        for observed in [
            "no continuation gives the prefix search a miss to bisect against",
            "replaying none of the target schedule and then scheduling without chaos, the \
             scheduler alone already meets the target criteria at a 1000 ns timeslice and at a \
             200000000 ns timeslice",
            "a longer prefix may still miss the target",
            "prefix_round_001.repro",
            "pass --continuation-timeslice",
            "--run2-seed",
        ] {
            assert!(message.contains(observed), "no {observed:?}: {message}");
        }

        // Missed with none replayed, met with all; flips inside (met for 10 to
        // 19, missed for 20 to 29). The search ends at one of the flips.
        let flips = |prefix: usize| prefix >= 10 && !(20..30).contains(&prefix);
        assert_eq!(
            choose_continuation(&candidates, len, |prefix, _| Ok(flips(prefix))).unwrap(),
            ns(1_000)
        );
        let passing = search_prefix_boundary(len, |prefix| Ok(flips(prefix))).unwrap();
        assert!(
            !flips(passing) && flips(passing + 1),
            "prefixes of {passing} and {} events",
            passing + 1
        );
    }

    /// Only a criterion the run misses is reported. Before, `--verbose` printed "Mismatch for stderr pattern"
    /// for every run, right before saying that the stderr matched.
    #[test]
    fn only_the_criteria_a_run_misses_are_reported() {
        let options = AnalyzeOpts::try_parse_from([
            "analyze",
            "--target-stderr=double-close",
            "--target-stdout=^$",
            "--",
            "/bin/true",
        ])
        .unwrap();
        let output = |stdout: &str, stderr: &str, code: i32| Output {
            status: ExitStatus::Exited(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        let met = output("", "fdsan: double-close of fd 5\n", 1);
        assert!(options.criteria_misses(&met).is_empty());
        assert!(options.output_matches(&met));

        let missed = options.criteria_misses(&output("ok\n", "all good\n", 0));
        assert_eq!(missed.len(), 3, "{missed:?}");
        assert_eq!(missed[0], "Mismatch for stdout pattern ^$");
        assert_eq!(missed[1], "Mismatch for stderr pattern double-close");
        assert!(missed[2].contains("Exit code 0 is not what we're looking for"));
    }

    fn events(tids: &[i32]) -> Vec<SchedEvent> {
        tids.iter()
            .map(|&tid| SchedEvent::branches(DetTid::from_raw(tid), 10))
            .collect()
    }

    /// A replay leaves its schedule at the first event that differs from the
    /// schedule's as the scheduler compares them when it takes a stack
    /// (another thread, operation or instruction pointer, but not another
    /// count), or where it ends; only the events up to the critical pair
    /// count.
    #[test]
    fn a_replay_leaves_its_schedule_at_the_first_other_event() {
        let requested = events(&[3, 5, 3, 5, 7]);
        assert_eq!(departure(&requested, &events(&[3, 5, 3, 5, 7]), 5), None);
        let mut other_count = events(&[3, 5, 3, 5, 7]);
        other_count[2].count = 99;
        assert_eq!(departure(&requested, &other_count, 5), None);
        let realized = events(&[3, 5, 9, 5, 7]);
        assert_eq!(
            departure(&requested, &realized, 5),
            Some(Departure::OtherEvent {
                index: 2,
                expected: requested[2].clone(),
                ran: realized[2].clone(),
            })
        );
        let mut other_op = requested.clone();
        other_op[1] = SchedEvent::syscall(
            DetTid::from_raw(5),
            reverie::syscalls::Sysno::write,
            detcore_model::schedule::SyscallPhase::Posthook,
        );
        assert!(matches!(
            departure(&requested, &other_op, 5),
            Some(Departure::OtherEvent { index: 1, .. })
        ));
        assert_eq!(departure(&requested, &events(&[3, 5, 3, 5, 9]), 4), None);
        assert_eq!(
            departure(&requested, &events(&[3, 5]), 5),
            Some(Departure::Ended { events: 2 })
        );
    }

    /// An unrecorded RIP is a wildcard, not a departure. This classification
    /// determines which replay supplies the report's stack, not just wording.
    #[test]
    fn recorded_rip_wildcards_preserve_the_replays_stack_source() {
        let tid = DetTid::from_raw;
        for (start, end, observed_start, observed_end) in [
            (None, None, Some(0x1000), None),
            (None, None, None, Some(0x2000)),
            (None, None, Some(0x1000), Some(0x2000)),
            (Some(0x1000), None, Some(0x1000), Some(0x2000)),
            (None, Some(0x2000), Some(0x1000), Some(0x2000)),
        ] {
            let mut requested = events(&[3, 5]);
            requested[0].start_rip = start.and_then(std::num::NonZeroUsize::new);
            requested[0].end_rip = end.and_then(std::num::NonZeroUsize::new);
            let mut observed = requested.clone();
            observed[0].start_rip = observed_start.and_then(std::num::NonZeroUsize::new);
            observed[0].end_rip = observed_end.and_then(std::num::NonZeroUsize::new);
            observed[0].count = 99;
            let left_schedule = departure(&requested, &observed, 2);
            assert_eq!(
                left_schedule, None,
                "extra unrecorded RIP information is not a departure"
            );
            let baseline = StackReplay {
                met_target: false,
                stacks: [
                    Some(timing_report_stack(3, 0x31)),
                    Some(timing_report_stack(5, 0x51)),
                ],
                left_schedule,
                ran: [Some(tid(3)), Some(tid(5))],
                traced: None,
            };
            let target = StackReplay {
                met_target: true,
                stacks: [
                    Some(timing_report_stack(5, 0x52)),
                    Some(timing_report_stack(3, 0x32)),
                ],
                left_schedule: None,
                ran: [Some(tid(5)), Some(tid(3))],
                traced: None,
            };
            assert_eq!(
                merge_stacks(&baseline, &target, 1, [tid(3), tid(5)]),
                [
                    Some((
                        timing_report_stack(3, 0x31),
                        "the baseline replay".to_owned()
                    )),
                    Some((
                        timing_report_stack(5, 0x51),
                        "the baseline replay".to_owned()
                    )),
                ]
            );
        }
        let mut requested = events(&[3, 5]);
        requested[0].start_rip = std::num::NonZeroUsize::new(0x1000);
        requested[0].end_rip = std::num::NonZeroUsize::new(0x2000);
        assert_eq!(departure(&requested, &requested, 2), None);
        for (start, end) in [
            (None, Some(0x2000)),
            (Some(0x1001), Some(0x2000)),
            (Some(0x1000), None),
            (Some(0x1000), Some(0x2001)),
        ] {
            let mut observed = requested.clone();
            observed[0].start_rip = start.and_then(std::num::NonZeroUsize::new);
            observed[0].end_rip = end.and_then(std::num::NonZeroUsize::new);
            assert_eq!(
                departure(&requested, &observed, 2),
                Some(Departure::OtherEvent {
                    index: 0,
                    expected: requested[0].clone(),
                    ran: observed[0].clone(),
                }),
                "a specified RIP must still be present and equal"
            );
        }
        for other in [
            SchedEvent::branches(tid(7), 10),
            SchedEvent::syscall(
                tid(3),
                reverie::syscalls::Sysno::write,
                detcore_model::schedule::SyscallPhase::Posthook,
            ),
        ] {
            let requested = events(&[3, 5]);
            let observed = vec![other.clone(), requested[1].clone()];
            assert_eq!(
                departure(&requested, &observed, 2),
                Some(Departure::OtherEvent {
                    index: 0,
                    expected: requested[0].clone(),
                    ran: other,
                }),
                "wildcard RIPs do not excuse a changed thread or operation"
            );
        }
    }

    /// A stack either replay captured is reported, in the passing order, with
    /// the replay it came from: the target replay's events #index - 1 and
    /// #index are the passing schedule's #index and #index - 1. A replay that
    /// followed its schedule through the operation is preferred; a stack from
    /// one that did not is kept only if that replay ran the operation's
    /// thread there, and is said to be possibly another occurrence. One
    /// neither gives says where each replay left its schedule. Before, a
    /// baseline replay that left its schedule early reported both stacks as
    /// unavailable, without a reason.
    #[test]
    fn each_racing_stack_comes_from_a_replay_that_reached_it() {
        let tid = DetTid::from_raw;
        let on = |thread: i32, name: &str| -> PrettyBacktrace {
            serde_json::from_value(serde_json::json!({
                "thread_id": thread, "thread_name": name, "frames": []
            }))
            .unwrap()
        };
        let replay = |stacks, left_schedule, ran| StackReplay {
            met_target: false,
            stacks,
            left_schedule,
            ran,
            traced: None,
        };
        // The passing schedule runs thread 5's read at #1093, then thread 7's
        // close at #1094; the failing one the other way round.
        let threads = [tid(5), tid(7)];
        let left = Some(Departure::OtherEvent {
            index: 735,
            expected: SchedEvent::branches(tid(5), 10),
            ran: SchedEvent::branches(tid(9), 10),
        });
        let baseline = replay([None, None], left.clone(), [Some(tid(9)), Some(tid(9))]);
        let target = replay(
            [Some(on(7, "close")), None],
            None,
            [Some(tid(7)), Some(tid(5))],
        );
        let [first, second] = merge_stacks(&baseline, &target, 1094, threads);
        assert_eq!(first, None);
        assert_eq!(
            second,
            Some((on(7, "close"), "the target replay".to_owned())),
            "the target's first is the passing second"
        );

        let ended = StackReplay {
            left_schedule: Some(Departure::Ended { events: 1093 }),
            ..target
        };
        let reason = missing_stack_reason("stack1", (1093, 1094), &baseline, &ended);
        assert!(
            reason.contains("The baseline replay left its schedule: it ran (tid9 cnt=10 Branch) at event #735, where the schedule has (tid5 cnt=10 Branch); the target replay left its schedule: it ended after 1093 events."),
            "{reason}"
        );
        assert!(reason.contains("--selfcheck"), "{reason}");
        let followed_without = replay([None, None], None, [Some(tid(7)), Some(tid(5))]);
        let reason = missing_stack_reason("stack1", (1093, 1094), &baseline, &followed_without);
        assert!(
            reason.contains("the target replay followed its schedule through event #1094, but left no readable stack there"),
            "{reason}"
        );
        let ended_early = replay(
            [None, None],
            Some(Departure::Ended { events: 1000 }),
            [None, None],
        );

        // The baseline replay took its stacks but left its schedule first:
        // the target replay, which followed its own, is preferred.
        let early = replay(
            [Some(on(5, "early-read")), Some(on(9, "early-write"))],
            left,
            [Some(tid(5)), Some(tid(9))],
        );
        let followed = replay(
            [Some(on(7, "close")), Some(on(5, "read"))],
            None,
            [Some(tid(7)), Some(tid(5))],
        );
        assert_eq!(
            merge_stacks(&early, &followed, 1094, threads),
            [
                Some((on(5, "read"), "the target replay".to_owned())),
                Some((on(7, "close"), "the target replay".to_owned())),
            ]
        );
        // With no replay that followed its schedule, a stack taken on the
        // operation's thread is kept with a caveat; one taken on another
        // thread (thread 9 where the schedule has thread 7) is left out.
        let [read, close] = merge_stacks(&early, &ended_early, 1094, threads);
        let (read, source) = read.unwrap();
        assert_eq!(read, on(5, "early-read"));
        assert!(source.contains("may be another occurrence"), "{source}");
        assert_eq!(close, None);
        // A replay that left its schedule before the pair keeps a stack it
        // took on the operation's thread.
        let left_with = replay(
            [Some(on(7, "close")), Some(on(9, "other"))],
            Some(Departure::Ended { events: 1000 }),
            [None, None],
        );
        let [read, close] = merge_stacks(&baseline, &left_with, 1094, threads);
        assert_eq!(read, None);
        assert_eq!(close.unwrap().0, on(7, "close"));

        let both = replay(
            [Some(on(5, "read")), Some(on(7, "close"))],
            None,
            [Some(tid(5)), Some(tid(7))],
        );
        assert_eq!(
            merge_stacks(&both, &followed, 1094, threads).map(|stack| stack.unwrap().1),
            ["the baseline replay", "the baseline replay"],
            "the baseline replay's own stacks come first"
        );
    }

    /// The final target schedule meets the criteria with another failure
    /// when the line its output pattern matches differs from the target
    /// run's; output the pattern does not match, which can differ with the
    /// interleaving, and line endings around the match are not compared.
    #[test]
    fn a_target_replay_with_another_failure_than_the_target_run_is_named() {
        let workspace = tempfile::tempdir().unwrap();
        let (target, replay) = (
            workspace.path().join("run1_target"),
            workspace.path().join("final_target_for_stacktraces"),
        );
        let pattern = Regex::new("fdsan: ").unwrap();
        let compare = |target_stderr: &str, replay_stderr: &str| {
            std::fs::write(target.with_extension("stderr"), target_stderr).unwrap();
            std::fs::write(replay.with_extension("stderr"), replay_stderr).unwrap();
            changed_output(
                &target,
                &replay,
                [("stdout", None), ("stderr", Some(&pattern))],
            )
        };
        assert_eq!(
            compare(
                "start\nfdsan: double-close\n",
                "other start\nfdsan: double-close"
            ),
            None
        );
        assert_eq!(compare("x\n\n", "x\n"), None, "no match, no comparison");
        let change = compare(
            "start\nfdsan: double-close\n",
            "start\nfdsan: failed to exchange ownership\n",
        )
        .unwrap();
        assert!(
            change.contains("the stderr line that --target-stderr matches is \"fdsan: failed to exchange ownership\", where the target run's is \"fdsan: double-close\""),
            "{change}"
        );
        assert!(change.contains("narrow --target-stderr"), "{change}");
        assert_eq!(
            changed_output(
                &workspace.path().join("absent"),
                &replay,
                [("stdout", None), ("stderr", Some(&pattern))],
            ),
            None,
            "no target run output (hermit bisect): no comparison"
        );
    }

    /// `--minimize`'s baseline knock-out removes a real intervention each
    /// time: the latest priority change, then whole threads, never a thread
    /// left at the default priority (which would change nothing), and gives
    /// up when none is left. Before, the records later knock-outs start
    /// from (recorded runs) named every thread, and once no priority change
    /// was left a knock-out could remove one of those, changing nothing.
    #[test]
    fn the_baseline_knock_out_removes_a_real_intervention_until_none_is_left() {
        use std::collections::BTreeMap;

        use detcore::types::LogicalTime;
        let at = LogicalTime::from_nanos;
        let record = PreemptionRecord::from_vecs(&BTreeMap::from([
            (DetTid::from_raw(3), vec![(at(0), 4923), (at(500), 1200)]),
            (DetTid::from_raw(5), vec![(at(0), 352)]),
            (DetTid::from_raw(9), vec![]),
        ]));
        let threads = |record: &PreemptionRecord| -> Vec<u32> {
            record
                .extract_all()
                .keys()
                .map(|tid| tid.as_raw() as u32)
                .collect()
        };
        let first = knock_out_latest(&record).unwrap();
        assert_eq!(threads(&first), [3, 5], "thread 9 had nothing to knock out");
        assert_ne!(
            first,
            record.without_default_threads(),
            "the knock-out changed the record"
        );
        let mut knocked = first;
        let mut steps = 1;
        while let Some(next) = knock_out_latest(&knocked) {
            assert!(!threads(&next).contains(&9));
            knocked = next;
            steps += 1;
            assert!(steps < 10, "the knock-outs do not end");
        }
        assert_eq!(
            threads(&knocked.without_default_threads()),
            Vec::<u32>::new()
        );
    }

    /// `--minimize`'s baseline search knocks out one more intervention of the
    /// record it last replayed each time, until a replay misses the target or
    /// none is left; it never starts again from a run's own recording, which
    /// holds more priority changes than it replayed. Before, a target whose
    /// replays kept meeting the target knocked out chaos's own changes
    /// without end.
    #[test]
    fn the_baseline_search_knocks_out_the_replayed_record_until_a_replay_misses() {
        use std::collections::BTreeMap;

        use detcore::types::LogicalTime;
        let at = LogicalTime::from_nanos;
        let record = PreemptionRecord::from_vecs(&BTreeMap::from([
            (DetTid::from_raw(3), vec![(at(0), 4923), (at(500), 1200)]),
            (DetTid::from_raw(5), vec![(at(0), 352)]),
        ]));
        let mut replayed = Vec::new();
        let missed = knock_out_until_missed(record.clone(), |attempt, knocked_out| {
            replayed.push(knocked_out.clone());
            Ok((attempt == 2).then_some(attempt))
        })
        .unwrap();
        assert_eq!(missed, 2);
        assert_eq!(replayed[0], knock_out_latest(&record).unwrap());
        assert_eq!(replayed[1], knock_out_latest(&replayed[0]).unwrap());

        let mut attempts = 0;
        let error = knock_out_until_missed(record, |_, _| {
            attempts += 1;
            Ok(None::<()>)
        })
        .unwrap_err();
        assert!(
            error.downcast_ref::<NothingToKnockOut>().is_some(),
            "{error:#}"
        );
        assert!(
            attempts == 3,
            "{attempts} knock-outs for three interventions"
        );
    }

    /// --minimize keeps the observed record and writes normalization only
    /// for reading. Its timing report must still reject an opposite-outcome
    /// slice-end replay before using that replay's diagnostic stack.
    #[test]
    fn minimized_timing_reports_check_slice_end_outcomes_on_the_original_record() {
        use std::collections::BTreeMap;

        use detcore::types::LogicalTime;

        let dir = tempfile::tempdir().unwrap();
        // The default search can fall back to prefix analysis after minimization.
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", "--minimize", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(dir.path().to_path_buf());
        let with = events(&[5, 3, 3, 3, 7, 5]);
        let without = events(&[5, 3, 3, 5, 7]);
        let record = PreemptionRecord::from_vecs(&BTreeMap::from([
            (
                DetTid::from_raw(3),
                vec![(LogicalTime::from_nanos(0), 4923)],
            ),
            (DetTid::from_raw(5), vec![(LogicalTime::from_nanos(0), 352)]),
            (
                DetTid::from_raw(7),
                vec![(LogicalTime::from_nanos(0), 8111)],
            ),
        ]))
        .with_schedule(with.clone())
        .unwrap();
        let recorded_path = dir.path().join("minimized.preempts");
        record.write_to_disk(&recorded_path).unwrap();
        let runopts =
            crate::app::run::RunOpts::try_parse_from(["hermit-run", "/bin/true"]).unwrap();
        let mut minimized = RunData::from_minimize_output(
            &options,
            "minimized".to_owned(),
            runopts,
            record.clone(),
            recorded_path.clone(),
            dir.path().join("minimized.log"),
        );
        let (normalized_path, normalized) = minimized.write_normalized_preempts();
        assert_ne!(
            normalized.as_vecs(),
            record.as_vecs(),
            "normalization changes priorities"
        );
        assert_ne!(normalized_path, recorded_path);
        assert_eq!(minimized.preempts_path_out(), recorded_path.as_path());
        assert_eq!(minimized.preempts_out(), &record);
        let mut endpoint = options.save_final_target_sched_events(minimized).unwrap();
        let target = endpoint.preempts_out().clone();
        assert_eq!(
            target, record,
            "the report starts from the record that met the target"
        );
        fn timing_report_stack(thread: i32, ip: u64) -> PrettyBacktrace {
            serde_json::from_value(serde_json::json!({
                "thread_id": thread,
                "thread_name": null,
                "frames": [{
                    "frame": { "ip": ip, "is_signal": false },
                    "symbol": null,
                    "locations": []
                }]
            }))
            .unwrap()
        }

        let deciding = Some(timing_report_stack(3, 0x20));
        for (target_met, baseline_met, contradicted) in [
            (true, false, None),
            (false, false, Some("final_target_slice_end_for_stacktraces")),
            (true, true, Some("final_baseline_slice_end_for_stacktraces")),
        ] {
            let result = options.report_timing_decision_with(
                &target,
                1,
                NonZeroU64::new(200_000_000),
                (true, &deciding),
                (
                    &without,
                    Path::new("final_baseline_schedule.preempts"),
                    &mut None,
                ),
                (
                    |seen, len| {
                        assert_eq!(seen, &record);
                        assert_eq!(len, 2);
                        Ok((
                            CompletedScheduleTrial {
                                target_met: true,
                                schedule: with.clone(),
                            },
                            PathBuf::from("final_target_schedule.preempts"),
                            None,
                        ))
                    },
                    |runname, variant, len, position| {
                        assert_eq!(variant.as_vecs(), record.as_vecs());
                        assert_ne!(variant.as_vecs(), normalized.as_vecs());
                        let (expected, end, met, ip, thread) = match runname {
                            "final_target_next_for_stacktraces" => (&with, 4, true, 0x10, 7),
                            "final_baseline_next_for_stacktraces" => (&without, 3, false, 0x10, 5),
                            "final_target_slice_end_for_stacktraces" => {
                                (&with, 3, target_met, 0x30, 3)
                            }
                            "final_baseline_slice_end_for_stacktraces" => {
                                (&without, 2, baseline_met, 0x40, 3)
                            }
                            other => panic!("unexpected replay {other}"),
                        };
                        assert_eq!(variant.schedevents(), expected);
                        assert_eq!((len, position), (end + 1, end));
                        Ok((met, Some(timing_report_stack(thread, ip))))
                    },
                ),
            );
            match contradicted {
                Some(replay) => {
                    let error = result
                        .expect_err("the minimized target does not exempt final replay outcomes")
                        .to_string();
                    assert!(
                        error.contains(&format!("{replay} observed the opposite")),
                        "{error}"
                    );
                }
                None => {
                    let report = result
                        .unwrap()
                        .expect("a repeated pair produces a timing report");
                    assert_eq!(report.critical_event1.event_index, 3);
                    assert_eq!(report.critical_event2.event_index, 2);
                    assert!(same_frames(
                        report.critical_event1.stack.as_ref().unwrap(),
                        &timing_report_stack(3, 0x30)
                    ));
                    assert!(same_frames(
                        report.critical_event2.stack.as_ref().unwrap(),
                        &timing_report_stack(3, 0x40)
                    ));
                }
            }
        }
    }

    /// `--target-exit-code` compares the code `hermit run` exits with: the
    /// guest's own, or 128 plus the signal number. Before, it compared the
    /// raw wait status, so `--target-exit-code=121` never matched a run that
    /// exited 121 (raw 30976), and 134 matched SIGABRT only with a core dump.
    #[test]
    fn a_target_exit_code_is_the_one_hermit_run_exits_with() {
        use nix::sys::signal::Signal;
        let exact = |code: &str| code.parse::<ExitStatusConstraint>().unwrap();
        assert!(exact("121").is_match(ExitStatus::Exited(121)));
        assert!(!exact("121").is_match(ExitStatus::Exited(0)));
        assert!(exact("0").is_match(ExitStatus::Exited(0)));
        for coredump in [false, true] {
            let abort = ExitStatus::Signaled(Signal::SIGABRT, coredump);
            assert!(exact("134").is_match(abort), "core dump {coredump}");
            assert!(!exact("6").is_match(abort), "core dump {coredump}");
            assert!(exact("nonzero").is_match(abort));
        }
        assert!(exact("nonzero").is_match(ExitStatus::Exited(121)));
        assert!(!exact("nonzero").is_match(ExitStatus::Exited(0)));
        // A realtime signal, which nix's Signal cannot name: 128 + 40.
        assert!(exact("168").is_match(ExitStatus::SignaledRaw(40, false)));
        for out_of_range in ["256", "-1", "30976"] {
            assert!(
                out_of_range.parse::<ExitStatusConstraint>().is_err(),
                "{out_of_range} is not an exit code"
            );
        }
        // The line naming a missed exit code prints the code, not the raw
        // status (30976 for an exit of 121).
        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--target-exit-code=3", "--", "/bin/true"])
                .unwrap();
        let missed = options.criteria_misses(&Output {
            status: ExitStatus::Exited(121),
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
        assert_eq!(missed, ["  Exit code 121 is not what we're looking for."]);
    }

    /// A report written before the deciding event was reported still reads.
    #[test]
    fn a_report_without_a_deciding_event_reads() {
        let report: Report = serde_json::from_value(serde_json::json!({
            "header": "h",
            "additional_context": "c",
            "baseline_run": true,
            "critical_event1": {"event_index": 1, "stack": null},
            "critical_event2": {"event_index": 2, "stack": null},
        }))
        .unwrap();
        assert_eq!(report.deciding_event, None);
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("deciding_event")
        );
        // Nor had it a verdict: it named the critical events, and says so now.
        assert_eq!(report.verdict, Verdict::CriticalEvents);
        assert!(
            serde_json::to_string(&report)
                .unwrap()
                .starts_with(r#"{"verdict":"critical_events","#)
        );
    }

    /// A timing decision: the deciding thread runs on in both continuations,
    /// and the report names how long, and which thread runs next in each.
    #[test]
    fn a_timing_decision_names_the_thread_that_runs_next_in_each_continuation() {
        let with = events(&[5, 3, 3, 3, 7, 5]);
        let without = events(&[5, 3, 3, 5, 7]);
        let thread = DetTid::from_raw(3);
        let with = slice_from(&with, 1, thread);
        let without = slice_from(&without, 1, thread);
        assert_eq!(
            with,
            Slice {
                events: 3,
                branches: 30,
                next: Some((4, DetTid::from_raw(7)))
            }
        );
        assert_eq!(without.next, Some((3, DetTid::from_raw(5))));
        assert_eq!(slice_from(&events(&[3, 3]), 0, thread).next, None);

        let header = timing_decision_header(
            1,
            &SchedEvent::branches(thread, 10),
            (&with, &without),
            "matching stderr",
            NonZeroU64::new(200_000_000),
            None,
        );
        for expected in [
            "Replaying the first 1 events and then scheduling without chaos at a 200000000 ns timeslice does not meet the target criteria; replaying event #1 too",
            "it is a timing decision: with or without it, the scheduler runs thread 3 there",
            "thread 3 runs 3 events (30 retired conditional branches) from event #1 on, then thread 7 runs at event #4 (stack1), and the run meets the target criteria (matching stderr)",
            "Without it, scheduling without chaos at a 200000000 ns timeslice, thread 3 runs 2 events (20 retired conditional branches), then thread 5 runs at event #3 (stack2), and the run does not",
        ] {
            assert!(header.contains(expected), "no {expected:?} in\n{header}");
        }

        // The thread that runs next is where it was in both: stack1 and
        // stack2 show where thread 3 stops instead, and the next threads are
        // named without them.
        let header = timing_decision_header(
            1,
            &SchedEvent::branches(thread, 10),
            (&with, &without),
            "matching stderr",
            NonZeroU64::new(200_000_000),
            Some([3, 2]),
        );
        for expected in [
            "Replaying the first 1 events and then scheduling without chaos at a 200000000 ns timeslice does not meet the target criteria; replaying event #1 too",
            "from event #1 on, then thread 7 runs at event #4, and the run meets",
            "then thread 5 runs at event #3, and the run does not",
            "so stack1 and stack2 show where thread 3 is when its slice ends instead: at event #3 with the event, at event #2 without it.",
        ] {
            assert!(header.contains(expected), "no {expected:?} in\n{header}");
        }
        assert!(!header.contains("(stack1)"), "{header}");
    }

    /// The deciding thread's slice ends replace the next threads' stacks only
    /// when those have the same frames (whichever threads they are) and both
    /// were captured.
    #[test]
    fn slice_ends_are_reported_only_when_the_next_stacks_tell_nothing_apart() {
        let stack = |thread: i32, ips: &[u64]| -> PrettyBacktrace {
            let frames: Vec<_> = ips
                .iter()
                .map(|ip| {
                    serde_json::json!({
                        "frame": {"ip": ip, "is_signal": false},
                        "symbol": null,
                        "locations": []
                    })
                })
                .collect();
            serde_json::from_value(serde_json::json!({
                "thread_id": thread, "thread_name": null, "frames": frames
            }))
            .unwrap()
        };
        let thread = DetTid::from_raw(3);
        let with = slice_from(&events(&[5, 3, 3, 3, 7, 5]), 1, thread);
        let without = slice_from(&events(&[5, 3, 3, 5, 7]), 1, thread);
        let started = [0x10, 0x20];
        // Two different threads, both just started.
        let (seven, five) = (stack(7, &started), stack(5, &started));
        assert_eq!(
            slice_end_events(&with, &without, [Some(&seven), Some(&five)]),
            Some([3, 2])
        );
        let elsewhere = stack(5, &[0x30, 0x20]);
        assert_eq!(
            slice_end_events(&with, &without, [Some(&seven), Some(&elsewhere)]),
            None
        );
        assert_eq!(
            slice_end_events(&with, &without, [Some(&seven), None]),
            None
        );
        let alone = slice_from(&events(&[5, 3, 3]), 1, thread);
        assert_eq!(
            slice_end_events(&alone, &without, [Some(&seven), Some(&five)]),
            None
        );
    }

    #[test]
    fn a_same_thread_critical_pair_is_flagged_and_a_cross_thread_one_is_not() {
        let caveat = critical_pair_caveat(&critical(&[3, 3, 5], 1))
            .expect("two events of thread 3 cannot be reordered");
        assert!(caveat.contains("both belong to thread 3"), "{caveat}");

        assert_eq!(
            critical_pair_caveat(&critical(&[3, 3, 5], 2)),
            None,
            "3 then 5 is a race"
        );
    }

    /// The target's record replays as edited only when `--minimize` edited
    /// it: otherwise it is the target run's recording, whose replay stops
    /// where it diverges.
    #[test]
    fn the_target_record_replays_as_edited_only_after_minimization() {
        let workspace = tempfile::tempdir().unwrap();
        for (minimize, edited) in [(false, false), (true, true)] {
            let mut argv = vec!["analyze", "--", "/bin/true"];
            if minimize {
                argv.insert(1, "--minimize");
            }
            let mut options = AnalyzeOpts::try_parse_from(argv).unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            let run =
                RunData::new_baseline(&options, "verify_target_endpoint".to_string()).unwrap();
            let run =
                options.replaying_the_target_record(run, workspace.path().join("target.preempts"));
            assert_eq!(
                run.into_runopts()
                    .det_opts
                    .det_config
                    .replay_edited_preemptions,
                edited,
                "--minimize: {minimize}"
            );
        }
    }

    /// `analyze` and `bisect` take the small `--max-timeslice` notice from the
    /// run arguments every trial shares, where the host can arm the timer the
    /// trials need, and give none for the default.
    #[test]
    fn trials_get_the_small_max_timeslice_notice_from_their_run_arguments() {
        let parse = |run: &[&str]| {
            let mut argv = vec!["analyze", "--"];
            argv.extend(run);
            AnalyzeOpts::try_parse_from(argv).unwrap()
        };
        let timer = crate::app::run::reverie_timer_support_for_tests();
        let small = parse(&["--max-timeslice=1000", "/bin/true"]);
        // A margin of the test's own, not the process's PMU configuration,
        // which `--skid-margin` tests in this binary must install first.
        let margin: fn() -> u64 = || 10_000;
        let notice = small.small_trial_max_timeslice_notice(margin);
        assert_eq!(notice.is_some(), timer, "{notice:?}");
        if let Some(notice) = notice {
            assert!(
                notice.starts_with("NOTICE: --max-timeslice=1000 "),
                "{notice}"
            );
        }
        assert_eq!(
            parse(&["/bin/true"]).small_trial_max_timeslice_notice(margin),
            None
        );
    }

    /// A trial that deadlocked under Hermit's model is neither on nor off
    /// target, under the default nonzero target or any other: it fails with
    /// the deadlock's outcome, while the same output from a guest that exited
    /// 121 itself is judged as usual.
    #[test]
    fn a_deadlocked_trial_is_neither_on_nor_off_target() {
        let output = Output {
            status: ExitStatus::Exited(detcore_model::HERMIT_GUEST_DEADLOCK_EXIT),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        for target in [None, Some("--target-exit-code=any")] {
            let mut argv = vec!["analyze", "--analyze-seed=7"];
            argv.extend(target);
            argv.extend(["--", "/bin/true"]);
            let options = AnalyzeOpts::try_parse_from(argv).unwrap();
            let error = options
                .trial_matches(&output, Some(detcore::TerminalOutcome::GuestDeadlock))
                .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<crate::app::container::SchedulerOutcome>(),
                    Some(crate::app::container::SchedulerOutcome(
                        detcore::TerminalOutcome::GuestDeadlock
                    ))
                ),
                "{target:?}: {error:#}"
            );
            assert!(
                options.trial_matches(&output, None).unwrap(),
                "{target:?}: a guest's own 121 is judged as usual"
            );
        }
    }

    /// A trial that `--virtual-timeout` stopped is neither on nor off target
    /// either, under the default nonzero target or any other: it fails with
    /// the bound's outcome, while the same output from a guest that exited 124
    /// itself is judged as usual.
    #[test]
    fn a_virtual_timeout_trial_is_neither_on_nor_off_target() {
        let outcome = detcore::TerminalOutcome::VirtualTimeout { bound_ns: 9 };
        let output = Output {
            status: ExitStatus::Exited(outcome.exit_status()),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        for target in [None, Some("--target-exit-code=any")] {
            let mut argv = vec!["analyze", "--analyze-seed=7"];
            argv.extend(target);
            argv.extend(["--", "/bin/true"]);
            let options = AnalyzeOpts::try_parse_from(argv).unwrap();
            let error = options.trial_matches(&output, Some(outcome)).unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<crate::app::container::SchedulerOutcome>(),
                    Some(crate::app::container::SchedulerOutcome(o)) if *o == outcome
                ),
                "{target:?}: {error:#}"
            );
            assert!(
                options.trial_matches(&output, None).unwrap(),
                "{target:?}: a guest's own 124 is judged as usual"
            );
        }
    }

    /// A search round whose trial `--max-log-bytes` ended stops the `--search`
    /// loop and returns that error, still a `LogCapExceeded`, so main reports
    /// it as the cap (123). Before, the loop panicked on any round's error.
    #[test]
    fn a_failing_search_round_ends_the_search_with_its_error() {
        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--analyze-seed=7", "--", "/bin/true"])
                .unwrap();
        for failing_round in 1..=4 {
            let mut rounds = 0;
            let result = options.search_rounds(|round, _sched_seed| {
                assert_eq!(round, rounds, "rounds are numbered from 0");
                rounds += 1;
                if rounds == failing_round {
                    return Err(Error::new(LogCapExceeded));
                }
                // An earlier round finished and did not match.
                Ok(None)
            });
            let Err(error) = result else {
                panic!("round {failing_round} failed, yet the search returned a run")
            };
            assert!(
                error.downcast_ref::<LogCapExceeded>().is_some(),
                "round {failing_round}: the error lost its type: {error:#}"
            );
            assert_eq!(
                rounds, failing_round,
                "the search ran more rounds after round {failing_round} failed"
            );
        }
    }

    const RECORDED: &str = "2000-12-31T23:59:59.123456789Z";
    const RECORDED_RFC3339: &str = "2000-12-31T23:59:59.123456789+00:00";
    const OTHER: &str = "2026-01-01T00:00:00Z";

    /// A preemption record written to `dir` under `name`, with the given
    /// recorded epoch.
    fn record_with_epoch(dir: &Path, name: &str, epoch: Option<&str>) -> PathBuf {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-resources/flaky_cas_sequence_schedules-passing.json");
        let mut record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(source).unwrap()).unwrap();
        if let Some(epoch) = epoch {
            record["epoch"] = serde_json::Value::from(epoch);
        }
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, record.to_string()).unwrap();
        path
    }

    /// Through `prepare`, which `main` runs before phase 0: the trial
    /// configuration `hermit analyze` would use for these records, or its
    /// refusal.
    fn prepared_analysis(
        dir: &Path,
        run1: Option<&str>,
        run2: Option<&str>,
        run_args: &[&str],
    ) -> anyhow::Result<AnalyzeOpts> {
        let run1 = format!(
            "--run1-preemptions={}",
            record_with_epoch(dir, "run1", run1).display()
        );
        let run2 = run2.map(|epoch| {
            format!(
                "--run2-preemptions={}",
                record_with_epoch(dir, "run2", Some(epoch)).display()
            )
        });
        let mut argv = vec!["hermit", "analyze", run1.as_str()];
        argv.extend(run2.as_deref());
        argv.push("--");
        argv.extend(run_args);
        argv.push("/bin/true");
        let args = crate::app::Args::try_parse_from(&argv).unwrap();
        let crate::app::Subcommand::Analyze(mut options) = args.command else {
            panic!("{argv:?} is not analyze")
        };
        options.prepare(&args.global)?;
        options.tmp_dir = Some(dir.to_path_buf());
        Ok(*options)
    }

    /// Analyze runs every trial, the baseline it records without `--chaos`
    /// included, under the `--expired-wait-charge` of its target: the run
    /// arguments' own, or else `--run1-preemptions`'s rule, or else `each`
    /// for the chaos target it records itself. The value is put in the run
    /// arguments, every record analyze writes carries it, and a
    /// `--run2-preemptions` record under the other rule is refused.
    #[test]
    fn analyze_runs_every_trial_under_the_targets_expired_wait_charge() {
        use detcore_model::config::ExpiredWaitCharge;
        let dir = tempfile::tempdir().unwrap();
        let record = |name: &str, once: bool| {
            let path = record_with_epoch(dir.path(), name, None);
            let mut value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            if once {
                value["expired_wait_runs_charged_once"] = serde_json::Value::from(true);
            }
            std::fs::write(&path, value.to_string()).unwrap();
            path
        };
        let prepare = |run1: Option<&Path>, run2: Option<&Path>, run_args: &[&str]| {
            let run1 = run1.map(|path| format!("--run1-preemptions={}", path.display()));
            let run2 = run2.map(|path| format!("--run2-preemptions={}", path.display()));
            let mut argv = vec!["hermit", "analyze"];
            argv.extend(run1.as_deref());
            argv.extend(run2.as_deref());
            argv.push("--");
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = super::super::super::Args::try_parse_from(&argv).unwrap();
            let super::super::super::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.prepare(&args.global).map(|_| options)
        };
        let each = record("each", false);
        let once = record("once", true);
        let written = |options: &AnalyzeOpts| {
            let (failing, passing, trial) = (
                dir.path().join("final-failing.json"),
                dir.path().join("final-passing.json"),
                dir.path().join("trial.json"),
            );
            options.write_final_schedules(Vec::new(), &failing, Vec::new(), &passing, &[]);
            options
                .write_trial_schedule_from(Vec::new(), &[], &trial)
                .unwrap();
            [failing, passing, trial].map(|path| {
                detcore::preemptions::read_recorded_expired_wait_runs_charged_once(&path).unwrap()
            })
        };
        for (run1, run2, run_args, expected) in [
            (None, None, vec![], ExpiredWaitCharge::Each),
            (
                None,
                None,
                vec!["--expired-wait-charge=once"],
                ExpiredWaitCharge::Once,
            ),
            (Some(&each), None, vec![], ExpiredWaitCharge::Each),
            (Some(&once), None, vec![], ExpiredWaitCharge::Once),
            (Some(&once), Some(&once), vec![], ExpiredWaitCharge::Once),
            (Some(&each), Some(&each), vec![], ExpiredWaitCharge::Each),
        ] {
            let case = format!("{run1:?} {run2:?} {run_args:?}");
            let options = prepare(
                run1.map(|p| p.as_path()),
                run2.map(|p| p.as_path()),
                &run_args,
            )
            .unwrap_or_else(|error| panic!("{case}: {error:#}"));
            assert_eq!(options.trial_expired_wait_charge(), expected, "{case}");
            assert_eq!(
                written(&options),
                [expected == ExpiredWaitCharge::Once; 3],
                "{case}"
            );
        }
        // Without --run1-preemptions the trials use `each`, so a plain
        // (`once`) --run2-preemptions baseline is refused.
        assert_eq!(
            prepare(None, Some(&each), &[])
                .unwrap()
                .trial_expired_wait_charge(),
            ExpiredWaitCharge::Each
        );
        assert!(prepare(None, Some(&once), &[]).is_err());
        for (run1, run2, run_args) in [
            (&once, Some(&each), vec![]),
            (&each, Some(&once), vec![]),
            (&once, None, vec!["--expired-wait-charge=each"]),
        ] {
            let case = format!("{run1:?} {run2:?} {run_args:?}");
            let error = prepare(Some(run1), run2.map(|p| p.as_path()), &run_args).expect_err(&case);
            assert!(
                format!("{error:#}").contains("--expired-wait-charge"),
                "{case}: {error:#}"
            );
        }
    }

    /// Analyze runs every trial, the baseline it records without `--chaos`
    /// included, under the `--sleep-registration` of its target: the run
    /// arguments' own, or else `--run1-preemptions`'s rule, or else
    /// `when-chosen` for the chaos target it records itself. The value is put
    /// in the run arguments, every record analyze writes carries it, and a
    /// `--run2-preemptions` record under the other rule is refused.
    #[test]
    fn analyze_runs_every_trial_under_the_targets_sleep_registration() {
        use detcore_model::config::SleepRegistration;
        let dir = tempfile::tempdir().unwrap();
        let record = |name: &str, first_pass: bool| {
            let path = record_with_epoch(dir.path(), name, None);
            let mut value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            if first_pass {
                value["queued_sleeps_registered_at_first_pass"] = serde_json::Value::from(true);
            }
            std::fs::write(&path, value.to_string()).unwrap();
            path
        };
        let prepare = |run1: Option<&Path>, run2: Option<&Path>, run_args: &[&str]| {
            let run1 = run1.map(|path| format!("--run1-preemptions={}", path.display()));
            let run2 = run2.map(|path| format!("--run2-preemptions={}", path.display()));
            let mut argv = vec!["hermit", "analyze"];
            argv.extend(run1.as_deref());
            argv.extend(run2.as_deref());
            argv.push("--");
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = crate::app::Args::try_parse_from(&argv).unwrap();
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.prepare(&args.global).map(|_| options)
        };
        let chosen = record("when-chosen", false);
        let first = record("first-pass", true);
        let written = |options: &AnalyzeOpts| {
            let (failing, passing, trial) = (
                dir.path().join("final-failing.json"),
                dir.path().join("final-passing.json"),
                dir.path().join("trial.json"),
            );
            options.write_final_schedules(Vec::new(), &failing, Vec::new(), &passing, &[]);
            options
                .write_trial_schedule_from(Vec::new(), &[], &trial)
                .unwrap();
            [failing, passing, trial].map(|path| {
                detcore::preemptions::read_recorded_queued_sleeps_registered_at_first_pass(&path)
                    .unwrap()
            })
        };
        for (run1, run2, run_args, expected) in [
            (None, None, vec![], SleepRegistration::WhenChosen),
            (
                None,
                None,
                vec!["--sleep-registration=first-pass"],
                SleepRegistration::FirstPass,
            ),
            (Some(&chosen), None, vec![], SleepRegistration::WhenChosen),
            (Some(&first), None, vec![], SleepRegistration::FirstPass),
            (
                Some(&first),
                Some(&first),
                vec![],
                SleepRegistration::FirstPass,
            ),
            (
                Some(&chosen),
                Some(&chosen),
                vec![],
                SleepRegistration::WhenChosen,
            ),
        ] {
            let case = format!("{run1:?} {run2:?} {run_args:?}");
            let options = prepare(
                run1.map(|p| p.as_path()),
                run2.map(|p| p.as_path()),
                &run_args,
            )
            .unwrap_or_else(|error| panic!("{case}: {error:#}"));
            assert_eq!(options.trial_sleep_registration(), expected, "{case}");
            assert_eq!(
                written(&options),
                [expected == SleepRegistration::FirstPass; 3],
                "{case}"
            );
        }
        // Without --run1-preemptions the trials use `when-chosen`, so a plain
        // (first-pass) --run2-preemptions baseline is refused.
        assert_eq!(
            prepare(None, Some(&chosen), &[])
                .unwrap()
                .trial_sleep_registration(),
            SleepRegistration::WhenChosen
        );
        assert!(prepare(None, Some(&first), &[]).is_err());
        for (run1, run2, run_args) in [
            (&first, Some(&chosen), vec![]),
            (&chosen, Some(&first), vec![]),
            (&first, None, vec!["--sleep-registration=when-chosen"]),
        ] {
            let case = format!("{run1:?} {run2:?} {run_args:?}");
            let error = prepare(Some(run1), run2.map(|p| p.as_path()), &run_args).expect_err(&case);
            assert!(
                format!("{error:#}").contains("--sleep-registration"),
                "{case}: {error:#}"
            );
        }
    }

    /// Analyze refuses, before any trial, a `--run1-preemptions` record made
    /// under another `--pipe-wake-latency` than the trials' own, and any
    /// record in its run arguments; its final schedules carry the trials'
    /// value (https://github.com/rrnewton/hermit/issues/4380).
    #[test]
    fn analyze_refuses_records_under_another_pipe_wake_latency() {
        let dir = tempfile::tempdir().unwrap();
        let record = |name: &str, latency: Option<u64>| {
            let path = record_with_epoch(dir.path(), name, None);
            let mut value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            if let Some(latency) = latency {
                value["pipe_wake_latency"] = serde_json::Value::from(latency);
            }
            std::fs::write(&path, value.to_string()).unwrap();
            path
        };
        let prepare = |run1: &Path, run_args: &[&str]| {
            let run1 = format!("--run1-preemptions={}", run1.display());
            let mut argv = vec!["hermit", "analyze", run1.as_str(), "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = crate::app::Args::try_parse_from(&argv).unwrap();
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.prepare(&args.global).map(|_| options)
        };
        let off = record("off", None);
        let zero = record("zero", Some(0));
        let options = prepare(&zero, &["--pipe-wake-latency=0"]).unwrap();
        assert_eq!(options.trial_pipe_wake_latency(), Some(0));
        // Every record analyze writes carries the trials' latency: the final
        // schedules, each trial's schedule, and minimize's records.
        let written = |options: &AnalyzeOpts| {
            let (failing, passing, trial) = (
                dir.path().join("final-failing.json"),
                dir.path().join("final-passing.json"),
                dir.path().join("trial.json"),
            );
            options.write_final_schedules(Vec::new(), &failing, Vec::new(), &passing, &[]);
            options
                .write_trial_schedule_from(Vec::new(), &[], &trial)
                .unwrap();
            [failing, passing, trial]
                .map(|path| detcore::preemptions::read_recorded_pipe_wake_latency(&path).unwrap())
        };
        assert_eq!(written(&options), [Some(0); 3]);
        assert_eq!(
            options
                .stamp_trial_record(PreemptionRecord::from_vecs(&Default::default()))
                .pipe_wake_latency(),
            Some(0)
        );
        let options_off = prepare(&off, &[]).unwrap();
        assert_eq!(written(&options_off), [None; 3]);
        let replay_flag = format!("--replay-preemptions-from={}", zero.display());
        for (run1, run_args, named) in [
            (&zero, vec![], "--run1-preemptions"),
            (&off, vec!["--pipe-wake-latency=0"], "--run1-preemptions"),
        ] {
            let error = prepare(run1, &run_args).unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
                    .is_some(),
                "{error:#}"
            );
            let message = format!("{error:#}");
            assert!(
                message.contains(named) && message.contains("cannot reproduce the run"),
                "{run_args:?}: {message}"
            );
        }
        // A record in the run arguments is refused before any recording is
        // read, whatever its latency (`RunOpts::options_trials_refuse`).
        let error = prepare(&off, &[replay_flag.as_str()]).unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::app::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        assert!(
            format!("{error:#}").contains("refuse --replay-preemptions-from in the run arguments"),
            "{error:#}"
        );
    }

    /// Every record analyze writes states the trials' signal-state copy
    /// setting, and a recording made under the other setting is refused
    /// before any trial; one that states no setting is accepted.
    #[test]
    fn analyze_stamps_and_checks_the_signal_state_copy_setting() {
        let dir = tempfile::tempdir().unwrap();
        let record = |name: &str, copy: Option<bool>| {
            let path = record_with_epoch(dir.path(), name, None);
            let mut value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            if let Some(copy) = copy {
                value["signal_state_copy"] = serde_json::Value::from(copy);
            }
            std::fs::write(&path, value.to_string()).unwrap();
            path
        };
        let prepare = |run1: &Path, run_args: &[&str]| {
            let run1 = format!("--run1-preemptions={}", run1.display());
            let mut argv = vec!["hermit", "analyze", run1.as_str(), "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = crate::app::Args::try_parse_from(&argv).unwrap();
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.prepare(&args.global).map(|_| options)
        };
        let unsaid = record("unsaid", None);
        let on = record("on", Some(true));
        let off = record("off", Some(false));
        let written = |options: &AnalyzeOpts| {
            let trial = dir.path().join("copy-trial.json");
            options
                .write_trial_schedule_from(Vec::new(), &[], &trial)
                .unwrap();
            detcore::preemptions::read_recorded_signal_state_copy(&trial).unwrap()
        };
        let default = prepare(&on, &[]).unwrap();
        assert!(default.trial_signal_state_copy());
        assert_eq!(written(&default), Some(true));
        let copy_off = prepare(&off, &["--no-signal-state-copy"]).unwrap();
        assert_eq!(written(&copy_off), Some(false));
        assert!(prepare(&unsaid, &[]).is_ok());
        assert!(prepare(&unsaid, &["--no-signal-state-copy"]).is_ok());
        for (run1, run_args) in [(&off, vec![]), (&on, vec!["--no-signal-state-copy"])] {
            let error = prepare(run1, &run_args).unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
                    .is_some(),
                "{error:#}"
            );
            assert!(
                format!("{error:#}").contains("signal-state copy"),
                "{error:#}"
            );
        }
    }

    /// Runs `test` (this module's test of that name) in two child processes,
    /// once with `HERMIT_EPOCH` unset and once set to [`OTHER`], and returns
    /// `None`. Inside such a child it returns the arm, `"unset"` or `"set"`.
    /// `HERMIT_EPOCH` counts as an explicit epoch, so the validation's
    /// forwarded value cannot decide the checks.
    fn epoch_environment_arm(test: &str) -> Option<String> {
        const CHILD: &str = "HERMIT_ANALYZE_EPOCH_TEST_CHILD";
        if let Some(arm) = std::env::var_os(CHILD) {
            return Some(arm.into_string().unwrap());
        }
        let name = format!("{}::{test}", module_path!().split_once("::").unwrap().1);
        for arm in ["unset", "set"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", &name, "--nocapture"])
                .env(CHILD, arm);
            if arm == "set" {
                child.env("HERMIT_EPOCH", OTHER);
            } else {
                child.env_remove("HERMIT_EPOCH");
            }
            let output = child.output().unwrap();
            assert!(output.status.success(), "{arm}: {output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "{arm}: the child ran no test: {output:?}"
            );
        }
        None
    }

    fn refused(error: anyhow::Error, expected: &str) {
        assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
        assert!(error.to_string().contains(expected), "{error:#}");
    }

    /// The trial that replays `--run1-preemptions` starts from the epoch the
    /// record was made under, as `hermit run` replaying it does
    /// (https://github.com/rrnewton/hermit/issues/3870). Before, it started
    /// from the run arguments' epoch, the default when none was given. The
    /// checks run in a child process whose environment fixes `HERMIT_EPOCH`,
    /// which counts as an explicit epoch: once unset, and once set to an epoch
    /// other than the recorded one.
    #[test]
    fn analyze_replays_a_record_from_the_epoch_it_was_recorded_under() {
        let Some(arm) =
            epoch_environment_arm("analyze_replays_a_record_from_the_epoch_it_was_recorded_under")
        else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        if arm == "set" {
            refused(
                prepared_analysis(dir, Some(RECORDED), None, &[]).unwrap_err(),
                &format!(
                    "the explicit virtual-time epoch 2026-01-01T00:00:00+00:00 (from --epoch or \
                     HERMIT_EPOCH) differs from the epoch {RECORDED_RFC3339} that \
                     --run1-preemptions"
                ),
            );
            return;
        }

        // A record without an epoch keeps the default.
        let analysis = prepared_analysis(dir, None, None, &[]).unwrap();
        // No `--epoch` is added; `--expired-wait-charge` and
        // `--sleep-registration` always are.
        assert_eq!(
            analysis.run_arg,
            [
                "--expired-wait-charge=each",
                "--sleep-registration=when-chosen"
            ]
        );
        assert_eq!(analysis.trial_epoch_for_test(), "2026-01-01T00:00:00+00:00");

        // The recorded epoch reaches the trials, from either record.
        let analysis = prepared_analysis(dir, Some(RECORDED), None, &[]).unwrap();
        assert_eq!(analysis.trial_epoch_for_test(), RECORDED_RFC3339);
        let analysis = prepared_analysis(dir, None, Some(RECORDED), &[]).unwrap();
        assert_eq!(analysis.trial_epoch_for_test(), RECORDED_RFC3339);

        // An explicit epoch equal to the recorded one adds no second --epoch.
        let analysis =
            prepared_analysis(dir, Some(RECORDED), None, &["--epoch", RECORDED]).unwrap();
        // No `--epoch` is added; `--expired-wait-charge` and
        // `--sleep-registration` always are.
        assert_eq!(
            analysis.run_arg,
            [
                "--expired-wait-charge=each",
                "--sleep-registration=when-chosen"
            ]
        );
        assert_eq!(analysis.trial_epoch_for_test(), RECORDED_RFC3339);

        // A different explicit epoch is refused, naming the remedy.
        refused(
            prepared_analysis(dir, Some(RECORDED), None, &["--epoch", OTHER]).unwrap_err(),
            &format!("pass --epoch={RECORDED_RFC3339}"),
        );

        // Two records under different epochs are refused, naming both.
        refused(
            prepared_analysis(dir, Some(RECORDED), Some(OTHER), &[]).unwrap_err(),
            &format!(
                "was recorded under the virtual-time epoch {RECORDED_RFC3339} and \
                 --run2-preemptions"
            ),
        );
    }

    /// A network trace the run arguments replay (`--replay-networking`)
    /// releases its inputs at absolute virtual times measured from its own
    /// epoch, and every trial replays it. The trials start from that epoch,
    /// and a trace that disagrees with a preemption record or an explicit
    /// epoch is refused before any trial, as `hermit run` refuses it
    /// (https://github.com/rrnewton/hermit/issues/3875). Before, the trials
    /// replayed it from the run arguments' epoch.
    #[test]
    fn analyze_replays_a_network_trace_from_the_epoch_it_was_recorded_under() {
        let Some(arm) = epoch_environment_arm(
            "analyze_replays_a_network_trace_from_the_epoch_it_was_recorded_under",
        ) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let trace = |name: &str, epoch: &str| {
            let path = dir.join(format!("{name}.trace"));
            detcore_model::network_engine::NetworkEngine::new_record(epoch.parse().unwrap())
                .finish()
                .unwrap()
                .write_framed(std::fs::File::create(&path).unwrap())
                .unwrap();
            format!("--replay-networking={}", path.display())
        };
        let recorded = trace("recorded", RECORDED);
        if arm == "set" {
            refused(
                prepared_analysis(dir, None, None, &[&recorded]).unwrap_err(),
                &format!(
                    "the explicit virtual-time epoch 2026-01-01T00:00:00+00:00 (from --epoch or \
                     HERMIT_EPOCH) differs from the epoch {RECORDED_RFC3339} that \
                     --replay-networking"
                ),
            );
            return;
        }

        // The trace alone sets the trials' epoch.
        let analysis = prepared_analysis(dir, None, None, &[&recorded]).unwrap();
        assert_eq!(analysis.trial_epoch_for_test(), RECORDED_RFC3339);

        // With a preemption record from the same epoch it is accepted.
        let analysis = prepared_analysis(dir, Some(RECORDED), None, &[&recorded]).unwrap();
        assert_eq!(analysis.trial_epoch_for_test(), RECORDED_RFC3339);

        // A trace from another epoch than the record is refused, naming both.
        let other = trace("other", OTHER);
        refused(
            prepared_analysis(dir, Some(RECORDED), None, &[&other]).unwrap_err(),
            &format!(
                "was recorded under the virtual-time epoch {RECORDED_RFC3339} and \
                 --replay-networking"
            ),
        );

        // An explicit epoch other than the trace's is refused.
        refused(
            prepared_analysis(dir, None, None, &[&recorded, "--epoch", OTHER]).unwrap_err(),
            &format!("pass --epoch={RECORDED_RFC3339}"),
        );
    }

    /// Through `main`: `--run1-schedule` and `--run2-schedule` parse but have
    /// no implementation, so `hermit analyze` refuses them before any
    /// workspace or trial, naming the options that work. Before, `main`
    /// reached `unimplemented!()` and the process panicked (exit 101).
    #[test]
    fn analyze_refuses_the_unimplemented_schedule_inputs_before_any_trial() {
        for (options, named) in [
            (
                &["--run1-schedule=/nonexistent/run1.json"][..],
                "--run1-schedule",
            ),
            (
                &["--run2-schedule=/nonexistent/run2.json"][..],
                "--run2-schedule",
            ),
            (
                &[
                    "--run1-schedule=/nonexistent/run1.json",
                    "--run2-schedule=/nonexistent/run2.json",
                ][..],
                "--run1-schedule or --run2-schedule",
            ),
        ] {
            let mut argv = vec!["hermit", "analyze"];
            argv.extend(options);
            argv.extend(["--", "/bin/true"]);
            let args = crate::app::Args::try_parse_from(&argv).unwrap();
            let crate::app::Subcommand::Analyze(mut analyze) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            let error = analyze.main(&args.global).unwrap_err();
            refused(
                error,
                &format!(
                    "hermit analyze does not implement {named}. Identify a run by a preemption \
                     record (--run1-preemptions or --run2-preemptions"
                ),
            );
            assert!(
                analyze.tmp_dir.is_none(),
                "{options:?}: a workspace was created before the refusal"
            );
        }
    }
}

#[cfg(test)]
mod replay_departure_tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn marked_target_evidence_cannot_pass_selfcheck_or_become_an_endpoint() {
        let directory = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(directory.path().to_path_buf());
        for marker in [None, Some(7)] {
            let record = PreemptionRecord::from_sched_events(vec![SchedEvent::branches(
                detcore::types::DetTid::from_raw(3),
                1,
            )])
            .retain_replay_departure(marker);
            let path = directory.path().join("target.json");
            record.write_to_disk(&path).unwrap();
            let runopts = crate::app::run::RunOpts::try_parse_from(["run", "/bin/true"]).unwrap();
            let make_run = || {
                RunData::from_minimize_output(
                    &options,
                    "target".into(),
                    runopts.clone(),
                    record.clone(),
                    path.clone(),
                    directory.path().join("target.log"),
                )
            };
            // A marked input must stop even before the optional selfcheck
            // launches; the ordinary no-selfcheck endpoint remains accepted.
            let mut run = make_run();
            let mut check = options.clone();
            check.selfcheck = marker.is_some();
            let checked = check.phase3_strict_preempt_replay_check(
                &GlobalOpts::try_parse_from(["hermit"]).unwrap(),
                &mut run,
            );
            let projected = run
                .require_complete_output()
                .map(|()| run.preempts_out().schedevents().clone());
            let promoted = options.clone().save_final_target_sched_events(make_run());
            if marker.is_some() {
                for error in [
                    checked.unwrap_err(),
                    projected.unwrap_err(),
                    promoted.err().expect("marked target was promoted"),
                ] {
                    assert!(
                        error
                            .downcast_ref::<crate::app::container::PolicyRefusal>()
                            .is_some(),
                        "{error:#}"
                    );
                    assert!(format!("{error:#}").contains("HERMIT_INCOMPLETE_REPLAY_RECORD"));
                }
            } else {
                checked.unwrap();
                assert_eq!(projected.unwrap(), record.schedevents().clone());
                promoted.unwrap();
            }
        }
    }
}
