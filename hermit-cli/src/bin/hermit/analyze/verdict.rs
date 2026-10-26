/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! What analyze concludes when it names no event, the conclusions that end it
//! before it can look for one, and the failures that keep it from concluding.
//! An [`AnalyzeVerdict`] is a completed analysis (exit 0,
//! `HERMIT_ANALYZE_VERDICT`); an [`AnalyzePrecondition`] says that the inputs
//! do not give analyze what it needs (exit
//! [`hermit::HERMIT_ANALYZE_PRECONDITION_EXIT`], `HERMIT_ANALYZE_PRECONDITION`);
//! an [`AnalyzeFailure`] is a failure of Hermit or the host, still exit 125,
//! but with a class line that says which. `main.rs`'s `failure_exit_code` and
//! `classify_failure` map all three.

use detcore_model::config::MaybeTimeslice;
use serde::Deserialize;
use serde::Serialize;

use crate::analyze::phases::describe_continuation;
use crate::schedule_search::NoSingleSwap;

/// What an analysis concluded: the `verdict` field of its report.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize
)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The report names the two critical events (`critical_event1` and
    /// `critical_event2`), and for a timing decision the deciding one.
    #[default]
    CriticalEvents,
    /// No event of the target schedule decides the outcome: without chaos,
    /// the scheduler alone meets the target. The report says why and names
    /// no event.
    NoDecidingEvent,
}

/// Why analyze concluded [`Verdict::NoDecidingEvent`]: the `verdict_reason`
/// field of its report, `{"cause": "scheduler_alone"}` (with
/// `"timing_in_force"` when the trials keep timing settings), or
/// `{"cause": "timing_semantics", "options": [...], "alternatives": [...],
/// "trial": "..."}`. A cause a later build adds reads as `unknown`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "cause")]
pub enum VerdictReason {
    /// Without chaos, the scheduler alone meets the target.
    SchedulerAlone {
        /// The timing settings every trial keeps from the recording, when
        /// there are any: they are in force, but not shown to cause the
        /// outcome (the empty prefix under their alternatives met the target
        /// too, or could not be tried).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        timing_in_force: Vec<String>,
    },
    /// Without chaos, the scheduler alone meets the target because of timing
    /// settings every trial keeps from the recording, its timing semantics:
    /// the empty prefix under their alternatives (`trial`) missed it.
    TimingSemantics {
        /// The settings, as `hermit run` options (`--pipe-wake-latency=N`).
        options: Vec<String>,
        /// For each, the other value a new failing run can take
        /// (`no --pipe-wake-latency`).
        alternatives: Vec<String>,
        /// The reproducer of the trial that showed it, in the workspace.
        trial: String,
    },
    /// A cause this build does not know, from a report a later one wrote.
    #[serde(other)]
    Unknown,
}

/// A timing setting every trial keeps from the recording, and the other
/// value a run can take instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimingSetting {
    /// As a `hermit run` option, `--pipe-wake-latency=50000`.
    pub option: String,
    /// The other value, as a reader would ask for it, `no
    /// --pipe-wake-latency`.
    pub alternative: String,
    /// The option a run under the alternative drops, `--pipe-wake-latency`.
    pub drops: &'static str,
    /// The argument it adds instead, if any: for a setting whose alternative
    /// is another value, that value (`--pipe-wake-latency=1000`).
    pub adds: Option<String>,
}

/// How the empty prefix fared, continued without chaos as the search's
/// trials were, under the alternatives of the timing settings in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlternativeTiming {
    /// It missed the target: the settings cause the outcome. `trial` names
    /// its reproducer.
    Missed { trial: String },
    /// It met the target too.
    Met { trial: String },
    /// It was not tried, for the reason given.
    Untried { why: String },
}

/// A completed analysis that names no event. Analyze writes its report and
/// exits as for any other completed analysis (0, or `--success-exit-code`),
/// with the class line [`AnalyzeVerdict::class_line`] on stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnalyzeVerdict {
    /// [`Verdict::NoDecidingEvent`], with the explanation analyze prints and
    /// the report carries, and its reason.
    NoDecidingEvent {
        /// What analyze prints, and the report's header.
        explanation: String,
        /// The report's `verdict_reason`.
        reason: VerdictReason,
        /// The continuation timeslice at which the scheduler alone met the
        /// target, which a trial under other timing settings uses too.
        continuation: MaybeTimeslice,
    },
}

impl AnalyzeVerdict {
    /// [`Verdict::NoDecidingEvent`]: the scheduler alone meets the target,
    /// continuing at `continuation`, as `explanation` says.
    pub fn no_deciding_event(explanation: impl Into<String>, continuation: MaybeTimeslice) -> Self {
        AnalyzeVerdict::NoDecidingEvent {
            explanation: explanation.into(),
            reason: VerdictReason::SchedulerAlone {
                timing_in_force: Vec::new(),
            },
            continuation,
        }
    }

    /// The continuation timeslice at which the scheduler alone met the
    /// target.
    pub fn continuation(&self) -> MaybeTimeslice {
        match self {
            AnalyzeVerdict::NoDecidingEvent { continuation, .. } => *continuation,
        }
    }

    /// This verdict, reached by trials that all kept `timing` from the
    /// recording, given how the empty prefix fared under the alternatives
    /// (`tried`). Only a trial that missed the target shows the settings
    /// cause the outcome ([`VerdictReason::TimingSemantics`], "because of");
    /// otherwise the scheduler alone meets the target "under" them, which
    /// are in force. Either way, analyzing under the alternatives needs a
    /// new failing run: analyze does not replay a recording under other
    /// values (and a record, stamped with its --pipe-wake-latency, is
    /// refused under another). With no settings, the verdict is unchanged.
    pub fn with_timing(self, timing: &[TimingSetting], tried: AlternativeTiming) -> Self {
        let AnalyzeVerdict::NoDecidingEvent {
            explanation,
            continuation,
            ..
        } = self;
        if timing.is_empty() {
            return AnalyzeVerdict::no_deciding_event(explanation, continuation);
        }
        let options: Vec<String> = timing.iter().map(|t| t.option.clone()).collect();
        let alternatives: Vec<String> = timing.iter().map(|t| t.alternative.clone()).collect();
        let (opts, alts) = (options.join(" and "), alternatives.join(" and "));
        // The trial ran at the first continuation the search tried.
        let at = describe_continuation(continuation);
        let (explanation, reason) = match tried {
            AlternativeTiming::Missed { trial } => (
                format!(
                    "{explanation} The scheduler alone meets the target because of {opts}, the \
                     recording's timing semantics, which every trial keeps: the empty prefix \
                     with {alts}, continued {at}, misses it ({trial} in the workspace). The \
                     failure needs no \
                     chaos decision under them, so no event decides it, and a two-seed search \
                     against a passing seed can end the same way, as it falls back to this \
                     search when no single swap separates its endpoints. To name a deciding \
                     event, analyze a new failing run with {alts} in the run arguments, if one \
                     fails there."
                ),
                VerdictReason::TimingSemantics {
                    options,
                    alternatives,
                    trial,
                },
            ),
            AlternativeTiming::Met { trial } => (
                format!(
                    "{explanation} The trials keep {opts} from the recording, but the empty \
                     prefix with {alts}, continued {at}, meets the target too ({trial} in the \
                     workspace), so the scheduler alone meets it under either."
                ),
                VerdictReason::SchedulerAlone {
                    timing_in_force: options,
                },
            ),
            AlternativeTiming::Untried { why } => (
                format!(
                    "{explanation} The scheduler alone meets the target under {opts}, in force \
                     because every trial keeps them from the recording; whether it does with \
                     {alts} was not tried: {why}. A new failing run with {alts} in the run \
                     arguments can tell."
                ),
                VerdictReason::SchedulerAlone {
                    timing_in_force: options,
                },
            ),
        };
        AnalyzeVerdict::NoDecidingEvent {
            explanation,
            reason,
            continuation,
        }
    }

    /// The report's `verdict`.
    pub fn verdict(&self) -> Verdict {
        match self {
            AnalyzeVerdict::NoDecidingEvent { .. } => Verdict::NoDecidingEvent,
        }
    }

    /// The report's `verdict_reason`.
    pub fn reason(&self) -> &VerdictReason {
        match self {
            AnalyzeVerdict::NoDecidingEvent { reason, .. } => reason,
        }
    }

    /// The line analyze prints on stderr, which a script can read without
    /// parsing the report.
    pub fn class_line(&self) -> String {
        let class = match self {
            AnalyzeVerdict::NoDecidingEvent { .. } => "no-deciding-event",
        };
        format!("HERMIT_ANALYZE_VERDICT class={class}")
    }
}

impl std::fmt::Display for AnalyzeVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalyzeVerdict::NoDecidingEvent { explanation, .. } => f.write_str(explanation),
        }
    }
}

impl std::error::Error for AnalyzeVerdict {}

/// Which input does not give analyze what it needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreconditionClass {
    /// The target run, or a run of the schedule meant to reproduce it, does
    /// not meet the target criteria: the first run without `--search`, the
    /// failing endpoint of a bisection (`hermit bisect`'s `--bad` too) and
    /// `--minimize`'s last record. (A `--selfcheck` difference is an
    /// [`AnalyzeFailure`].)
    TargetNotReproduced,
    /// The baseline does not miss the target or does not finish: the passing
    /// endpoint of a bisection (`hermit bisect`'s `--good` too), or the
    /// baseline run the user chose.
    BaselineNotReproduced,
    /// No continuation lets a prefix search compare prefixes of the target
    /// schedule (`--prefix-search`).
    NoPrefixContinuation,
    /// The search has not established an exact one-swap pair of schedules
    /// with opposite outcomes between the endpoints ([`NoSingleSwap`]).
    /// Analyze then attempts a prefix search within the target schedule;
    /// `hermit bisect`, which has no target run to search, ends with this.
    EndpointsNotSeparable,
    /// `hermit bisect`'s --good and --bad schedules contain the same events.
    EndpointsIdentical,
}

impl PreconditionClass {
    /// The `class=` of the class line.
    pub fn name(self) -> &'static str {
        match self {
            PreconditionClass::TargetNotReproduced => "target-not-reproduced",
            PreconditionClass::BaselineNotReproduced => "baseline-not-reproduced",
            PreconditionClass::NoPrefixContinuation => "no-prefix-continuation",
            PreconditionClass::EndpointsNotSeparable => "endpoints-not-separable",
            PreconditionClass::EndpointsIdentical => "endpoints-identical",
        }
    }

    /// The class line `main.rs` prints first on stderr.
    pub fn class_line(self) -> String {
        format!("HERMIT_ANALYZE_PRECONDITION class={}", self.name())
    }
}

/// The inputs do not give analyze what it needs, so it cannot look for a
/// deciding event: it ends with exit [`hermit::HERMIT_ANALYZE_PRECONDITION_EXIT`]
/// and [`PreconditionClass::class_line`], and the explanation says what to
/// change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzePrecondition {
    /// Which input.
    pub class: PreconditionClass,
    explanation: String,
}

impl AnalyzePrecondition {
    /// [`PreconditionClass::TargetNotReproduced`].
    pub fn target_not_reproduced(explanation: impl Into<String>) -> Self {
        Self::new(PreconditionClass::TargetNotReproduced, explanation)
    }

    /// [`PreconditionClass::BaselineNotReproduced`].
    pub fn baseline_not_reproduced(explanation: impl Into<String>) -> Self {
        Self::new(PreconditionClass::BaselineNotReproduced, explanation)
    }

    /// [`PreconditionClass::NoPrefixContinuation`].
    pub fn no_prefix_continuation(explanation: impl Into<String>) -> Self {
        Self::new(PreconditionClass::NoPrefixContinuation, explanation)
    }

    /// [`PreconditionClass::EndpointsIdentical`].
    pub fn endpoints_identical(explanation: impl Into<String>) -> Self {
        Self::new(PreconditionClass::EndpointsIdentical, explanation)
    }

    /// The same conclusion, its explanation followed by `hint`.
    pub fn with_hint(self, hint: &str) -> Self {
        Self {
            explanation: format!("{} {hint}", self.explanation),
            ..self
        }
    }

    fn new(class: PreconditionClass, explanation: impl Into<String>) -> Self {
        Self {
            class,
            explanation: explanation.into(),
        }
    }
}

impl std::fmt::Display for AnalyzePrecondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.explanation)
    }
}

impl std::error::Error for AnalyzePrecondition {}

/// The precondition `error` concludes, if any: an [`AnalyzePrecondition`], or
/// one of the schedule search's typed ends that no single reordering
/// separates the endpoints, which reach `main.rs` only from `hermit bisect`.
pub fn precondition_class(error: &anyhow::Error) -> Option<PreconditionClass> {
    if let Some(precondition) = error.downcast_ref::<AnalyzePrecondition>() {
        return Some(precondition.class);
    }
    if error.downcast_ref::<NoSingleSwap>().is_some() {
        return Some(PreconditionClass::EndpointsNotSeparable);
    }
    None
}

/// Analyze could not conclude, for a reason in Hermit or the host, not in its
/// inputs: a failure of Hermit (exit 125), with a class line that says which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnalyzeFailure {
    /// Replays of one run or schedule did not reproduce it: `--selfcheck`
    /// found a difference, or a jittered control schedule never replayed the
    /// same way twice. Determinism, Hermit's or the host's (an inexact branch
    /// counter), broke.
    ReplayNotReproducible(String),
    /// The event-level search did not converge within its passes.
    SearchDidNotConverge(String),
}

impl AnalyzeFailure {
    /// The line `main.rs` prints first on stderr.
    pub fn class_line(&self) -> String {
        let class = match self {
            AnalyzeFailure::ReplayNotReproducible(_) => "replay-not-reproducible",
            AnalyzeFailure::SearchDidNotConverge(_) => "search-did-not-converge",
        };
        format!("HERMIT_INTERNAL_FAILURE class={class}")
    }
}

impl std::fmt::Display for AnalyzeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalyzeFailure::ReplayNotReproducible(explanation)
            | AnalyzeFailure::SearchDidNotConverge(explanation) => f.write_str(explanation),
        }
    }
}

impl std::error::Error for AnalyzeFailure {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A verdict serializes as its report field does, and old reports without
    /// one read as naming the critical events.
    #[test]
    fn a_verdict_is_written_in_snake_case() {
        assert_eq!(
            serde_json::to_string(&Verdict::NoDecidingEvent).unwrap(),
            "\"no_deciding_event\""
        );
        assert_eq!(
            serde_json::to_string(&Verdict::CriticalEvents).unwrap(),
            "\"critical_events\""
        );
        assert_eq!(Verdict::default(), Verdict::CriticalEvents);
    }

    /// A no-deciding-event verdict reached under timing settings the
    /// recording carries says what the trial under their alternatives
    /// showed: "because of" only when it missed the target, "under" when it
    /// met it or was not tried; without settings it stays the scheduler
    /// alone. A cause from a later build reads as unknown.
    #[test]
    fn a_verdict_under_timing_settings_says_what_the_trial_showed() {
        let alone = AnalyzeVerdict::no_deciding_event("No event decides.", None);
        let latency = [TimingSetting {
            option: "--pipe-wake-latency=50000".to_owned(),
            alternative: "no --pipe-wake-latency".to_owned(),
            drops: "--pipe-wake-latency",
            adds: None,
        }];
        let untried = || AlternativeTiming::Untried {
            why: "not asked".to_owned(),
        };
        assert_eq!(alone.clone().with_timing(&[], untried()), alone);
        assert_eq!(
            serde_json::to_value(alone.reason()).unwrap(),
            serde_json::json!({"cause": "scheduler_alone"})
        );
        let trial = "prefix_alternative_timing.repro".to_owned();
        let shown = alone.clone().with_timing(
            &latency,
            AlternativeTiming::Missed {
                trial: trial.clone(),
            },
        );
        assert_eq!(shown.verdict(), Verdict::NoDecidingEvent);
        assert_eq!(
            shown.to_string(),
            "No event decides. The scheduler alone meets the target because of \
             --pipe-wake-latency=50000, the recording's timing semantics, which every trial \
             keeps: the empty prefix with no --pipe-wake-latency, continued without PMU \
             timeslices, misses it (prefix_alternative_timing.repro in the workspace). The \
             failure needs no chaos \
             decision under them, so no event decides it, and a two-seed search against a \
             passing seed can end the same way, as it falls back to this search when no single \
             swap separates its endpoints. To name a deciding event, analyze a new failing run \
             with no --pipe-wake-latency in the run arguments, if one fails there."
        );
        assert_eq!(
            serde_json::to_value(shown.reason()).unwrap(),
            serde_json::json!({
                "cause": "timing_semantics",
                "options": ["--pipe-wake-latency=50000"],
                "alternatives": ["no --pipe-wake-latency"],
                "trial": "prefix_alternative_timing.repro",
            })
        );
        let met = alone
            .clone()
            .with_timing(&latency, AlternativeTiming::Met { trial });
        assert!(
            met.to_string().ends_with(
                "The trials keep --pipe-wake-latency=50000 from the recording, but the empty \
                 prefix with no --pipe-wake-latency, continued without PMU timeslices, meets the \
                 target too (prefix_alternative_timing.repro in the workspace), so the scheduler \
                 alone meets it under either."
            ),
            "{met}"
        );
        assert_eq!(
            serde_json::to_value(met.reason()).unwrap(),
            serde_json::json!({
                "cause": "scheduler_alone",
                "timing_in_force": ["--pipe-wake-latency=50000"],
            })
        );
        let not_tried = alone.with_timing(&latency, untried());
        assert!(
            not_tried.to_string().ends_with(
                "The scheduler alone meets the target under --pipe-wake-latency=50000, in force \
                 because every trial keeps them from the recording; whether it does with no \
                 --pipe-wake-latency was not tried: not asked. A new failing run with no \
                 --pipe-wake-latency in the run arguments can tell."
            ),
            "{not_tried}"
        );
        assert_eq!(not_tried.reason(), met.reason());
        let later: VerdictReason =
            serde_json::from_str(r#"{"cause": "something_new", "detail": 1}"#).unwrap();
        assert_eq!(later, VerdictReason::Unknown);
    }

    /// The schedule search's two typed ends that no single reordering
    /// separates the endpoints conclude `endpoints-not-separable`; a hint
    /// keeps the class and adds to the explanation.
    #[test]
    fn the_search_ends_that_no_swap_separates_are_a_precondition() {
        for swaps_left in [0, 1] {
            let error = anyhow::Error::new(NoSingleSwap {
                unshared_events: 2,
                swaps_left,
                trimmed_failing_passed: false,
            });
            assert_eq!(
                precondition_class(&error),
                Some(PreconditionClass::EndpointsNotSeparable),
                "{error:#}"
            );
        }
        assert_eq!(precondition_class(&anyhow::anyhow!("other")), None);
        let hinted = AnalyzePrecondition::target_not_reproduced("did not.").with_hint("Check.");
        assert_eq!(hinted.class, PreconditionClass::TargetNotReproduced);
        assert_eq!(hinted.to_string(), "did not. Check.");
    }
}
