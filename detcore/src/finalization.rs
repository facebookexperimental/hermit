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
