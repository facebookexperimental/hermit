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

use serde::Deserialize;
use serde::Serialize;

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

/// A completed analysis that names no event. Analyze writes its report and
/// exits as for any other completed analysis (0, or `--success-exit-code`),
/// with the class line [`AnalyzeVerdict::class_line`] on stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnalyzeVerdict {
    /// [`Verdict::NoDecidingEvent`], with the explanation analyze prints and
    /// the report carries.
    NoDecidingEvent(String),
}

impl AnalyzeVerdict {
    /// The report's `verdict`.
    pub fn verdict(&self) -> Verdict {
        match self {
            AnalyzeVerdict::NoDecidingEvent(_) => Verdict::NoDecidingEvent,
        }
    }

    /// The line analyze prints on stderr, which a script can read without
    /// parsing the report.
    pub fn class_line(&self) -> String {
        let class = match self {
            AnalyzeVerdict::NoDecidingEvent(_) => "no-deciding-event",
        };
        format!("HERMIT_ANALYZE_VERDICT class={class}")
    }
}

impl std::fmt::Display for AnalyzeVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalyzeVerdict::NoDecidingEvent(explanation) => f.write_str(explanation),
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
