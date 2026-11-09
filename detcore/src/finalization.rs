/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Consuming finalization preserves the execution's primary error.

use std::fmt;

/// A typed replay cursor refused its terminal state. This marker is constructed
/// only at terminal cursor validation, not for recording I/O or summary errors.
#[derive(Debug)]
pub struct TerminalReplayRefusal(pub(crate) detcore_model::schedule::boundary::Error);

impl fmt::Display for TerminalReplayRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "terminal replay refusal: {}", self.0)
    }
}

impl std::error::Error for TerminalReplayRefusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Priority-history replay of a callback-annotated record ended with recorded
/// checkpoints unconsumed (`Config::annotated_priority_replay`). Constructed
/// only by the scheduler's terminal summary, after the record it re-records
/// is flushed; the run ends as a policy refusal.
#[derive(Debug)]
pub struct PriorityHistoryRefusal(pub(crate) String);

impl fmt::Display for PriorityHistoryRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "terminal replay refusal: {}", self.0)
    }
}

impl std::error::Error for PriorityHistoryRefusal {}

/// Accepted faithful replay findings or a refused finding publication. These
/// remain distinct: a refused payload contributes no accepted finding or marker.
/// Constructed only after recording and summary finalization are attempted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayDepartureError {
    pub(crate) summary: Option<detcore_model::replay_departure::Summary>,
    pub(crate) publication_refusal: Option<(
        crate::types::DetTid,
        crate::tool_global::replay_departure::Refusal,
    )>,
}

impl ReplayDepartureError {
    /// The bounded observations retained by the coordinator.
    pub fn summary(&self) -> Option<&detcore_model::replay_departure::Summary> {
        self.summary.as_ref()
    }

    /// Find the authoritative departure cause, including Reverie's transparent
    /// Tool wrapper. Secondary finalization observations never reclassify the
    /// operation's primary failure.
    pub fn in_error(error: &anyhow::Error) -> Option<&Self> {
        if let Some(pair) = error.downcast_ref::<FinalizationFailure>() {
            return Self::in_error(pair.primary());
        }
        if let Some(departure) = error.downcast_ref::<Self>() {
            return Some(departure);
        }
        error
            .chain()
            .find_map(|cause| match cause.downcast_ref::<reverie::Error>() {
                Some(reverie::Error::Tool(inner)) => Self::in_error(inner),
                _ => None,
            })
    }
}

impl fmt::Display for ReplayDepartureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HERMIT_REPLAY_DEPARTURE: ")?;
        if let Some(summary) = &self.summary {
            let first = &summary.first;
            write!(
                f,
                "{} nonadvancing change-point detection(s); first {:?} in thread {} at {}, preceding {}; ",
                summary.count, first.source, first.thread, first.observed, first.previous
            )?;
        }
        if let Some((thread, reason)) = &self.publication_refusal {
            write!(
                f,
                "finding publication from thread {thread} refused ({reason:?}); "
            )?;
        }
        f.write_str("refusing faithful replay completion")
    }
}

impl std::error::Error for ReplayDepartureError {}

/// An earlier error plus a later finalization failure. Only the primary is in
/// the cause chain: a secondary policy refusal must not reclassify an I/O or
/// backend failure. Both full diagnostics remain available through Display.
#[derive(Debug)]
pub struct FinalizationFailure {
    primary: anyhow::Error,
    secondary: anyhow::Error,
}

impl FinalizationFailure {
    /// The original error, before finalization was attempted.
    pub fn primary(&self) -> &anyhow::Error {
        &self.primary
    }

    /// The additional failure encountered while releasing the original owner.
    pub fn secondary(&self) -> &anyhow::Error {
        &self.secondary
    }
}

impl fmt::Display for FinalizationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}; additionally, finalization failed: {:#}",
            self.primary, self.secondary
        )
    }
}

impl std::error::Error for FinalizationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.primary.as_ref())
    }
}

/// Combine an already evaluated operation with its already evaluated cleanup.
/// Cleanup is never skipped, and failure of either operation prevents success.
pub fn combine_finalization<T>(
    primary: anyhow::Result<T>,
    finalization: anyhow::Result<()>,
) -> anyhow::Result<T> {
    match (primary, finalization) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(primary), Err(secondary)) => Err(FinalizationFailure { primary, secondary }.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Primary;
    impl fmt::Display for Primary {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("primary identity")
        }
    }
    impl std::error::Error for Primary {}

    #[derive(Debug)]
    struct Secondary;
    impl fmt::Display for Secondary {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("secondary identity")
        }
    }
    impl std::error::Error for Secondary {}

    #[test]
    fn both_failures_remain_owned_but_only_primary_classifies() {
        let error =
            combine_finalization::<()>(Err(Primary.into()), Err(Secondary.into())).unwrap_err();
        let pair = error.downcast_ref::<FinalizationFailure>().unwrap();
        assert!(pair.primary().is::<Primary>());
        assert!(pair.secondary().is::<Secondary>());
        assert!(error.chain().any(|cause| cause.is::<Primary>()));
        assert!(!error.chain().any(|cause| cause.is::<Secondary>()));
        assert!(format!("{error:#}").contains("secondary identity"));
    }

    #[test]
    fn either_failure_refuses_and_success_keeps_value() {
        assert_eq!(combine_finalization(Ok(7), Ok(())).unwrap(), 7);
        assert!(
            combine_finalization::<()>(Err(Primary.into()), Ok(()))
                .unwrap_err()
                .is::<Primary>()
        );
        assert!(
            combine_finalization(Ok(7), Err(Secondary.into()))
                .unwrap_err()
                .is::<Secondary>()
        );
    }
}
