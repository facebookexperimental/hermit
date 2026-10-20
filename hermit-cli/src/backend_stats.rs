/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hermit-side enablement and reporting for backend statistics.

use std::fmt;

use reverie::BackendStatsRequest;
use reverie::BackendStatsSnapshot;
use reverie::BackendStatsSource;
use reverie::DispatchStats;

use crate::Backend;

const TARGET: &str = "hermit::backend_stats";

/// Collect when something will read the record: the DEBUG report, or the
/// dispatch record in a requested `--summary-json`.
pub(crate) fn request(summary_json: &Option<std::path::PathBuf>) -> BackendStatsRequest {
    BackendStatsRequest::new(
        summary_json.is_some() || tracing::enabled!(target: TARGET, tracing::Level::DEBUG),
    )
}

/// Report the selected backend's own run statistics.
///
/// ⚠️ THIS RECORD IS EMITTED AT DEBUG, NOT INFO, AND THAT IS LOAD-BEARING.
/// `ComparedLogScope::Info` is the `BitwiseInfoV1` parity envelope — "every INFO
/// message, exactly" — so anything emitted here at INFO is compared between two
/// runs as though it were guest behaviour. This record is not guest behaviour:
/// it is hermit describing its own harness, and BOTH its fields identify the
/// harness rather than the guest. `backend` is the backend's name, and `stats`
/// is that backend's own instrumentation (ptrace activity counters for ptrace,
/// patch-shape counters for e9patch, and so on).
///
/// Returns the backend's dispatch record for the run summary. It is printed
/// here at DEBUG with the snapshot, for the same reason: dispatch counts
/// describe the harness, and differ between backends by design.
///
/// Measured on a real `ptrace` run before this change: of 303 INFO records, this
/// was the ONE record naming a backend. So two backends executing an identical
/// guest could never agree under the Info envelope, however correct the backends
/// were — a divergence by construction rather than a behavioural difference, and
/// the only such record in the stream.
///
/// Same-backend verification prints the same values on both sides, so nothing
/// was red; the ceiling was on any future cross-backend comparison. DEBUG keeps
/// the record fully available to `--log debug` while removing it from the
/// envelope that is compared, which is the narrow fix. Widening or renaming the
/// Info scope itself would change what a published parity claim MEANS, and that
/// is an owner decision, not this one.
pub(crate) fn report<S>(
    selected_backend: Backend,
    request: BackendStatsRequest,
    source: &S,
) -> Option<DispatchStats>
where
    S: BackendStatsSource,
{
    // ⚠️ HOISTED ABOVE THE COLLECTION GATE ON PURPOSE, AND IT MUST STAY THERE.
    // This asks "is the stats source wired to the backend we actually selected?"
    // -- a wiring invariant that is true or false regardless of whether anyone
    // asked for the record. Below the `else { return; }` it only ran when the
    // level probe happened to be on, so a LOG-LEVEL CHANGE COULD DISABLE A
    // CORRECTNESS CHECK: it did not start failing, it stopped executing, and a
    // check that stops executing reports nothing. `BACKEND_NAME` is an
    // associated const, so this needs no snapshot and costs nothing to keep
    // unconditional. Reporting and checking are two different questions; only
    // the first belongs behind the gate.
    assert_eq!(
        selected_backend.as_str(),
        S::Snapshot::BACKEND_NAME,
        "backend statistics source does not match selected backend"
    );
    let snapshot = request.collect(source)?;
    let dispatch = snapshot.dispatch_stats();
    tracing::debug!(
        target: TARGET,
        backend = %selected_backend.as_str(),
        stats = %snapshot,
        dispatch = %DisplayDispatch(dispatch.as_ref()),
        "backend run complete",
    );
    dispatch
}

/// [`report`] for a backend whose source exists only when collection was
/// enabled, such as the ptrace tracer's.
pub(crate) fn report_if_collected<S>(
    selected_backend: Backend,
    request: BackendStatsRequest,
    source: Option<&S>,
) -> Option<DispatchStats>
where
    S: BackendStatsSource,
{
    report(selected_backend, request, source?)
}

struct DisplayDispatch<'a>(Option<&'a DispatchStats>);

impl fmt::Display for DisplayDispatch<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(record) => record.fmt(formatter),
            None => formatter.write_str("none"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    struct CountingSource {
        snapshots: Cell<usize>,
    }

    struct CountingSnapshot;

    impl fmt::Display for CountingSnapshot {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("counting")
        }
    }

    impl BackendStatsSnapshot for CountingSnapshot {
        const BACKEND_NAME: &'static str = "ptrace";

        fn dispatch_stats(&self) -> Option<DispatchStats> {
            Some(DispatchStats::new(
                Self::BACKEND_NAME,
                reverie::DispatchCounters {
                    ptrace_seccomp_stops: Some(7),
                    ..reverie::DispatchCounters::ZERO
                },
                reverie::SiteCounters::NONE_PATCHED,
            ))
        }
    }

    impl BackendStatsSource for CountingSource {
        type Snapshot = CountingSnapshot;

        fn backend_stats(&self) -> Self::Snapshot {
            self.snapshots.set(self.snapshots.get() + 1);
            CountingSnapshot
        }
    }

    #[test]
    fn disabled_report_does_not_snapshot_backend() {
        let source = CountingSource {
            snapshots: Cell::new(0),
        };

        assert_eq!(
            report(Backend::Ptrace, BackendStatsRequest::DISABLED, &source),
            None
        );
        assert_eq!(source.snapshots.get(), 0);
        let record = report(Backend::Ptrace, BackendStatsRequest::ENABLED, &source)
            .expect("an enabled report returns the backend's dispatch record");
        assert_eq!(source.snapshots.get(), 1);
        assert_eq!(record.backend, "ptrace");
        assert_eq!(record.counters.dispatches(), Some(7));
    }

    #[test]
    fn a_requested_summary_collects_without_debug_logging() {
        assert!(request(&Some(std::path::PathBuf::from("/summary.json"))).is_enabled());
    }

    #[test]
    #[should_panic(expected = "backend statistics source does not match selected backend")]
    fn report_rejects_a_mismatched_backend_source_in_release_builds() {
        let source = CountingSource {
            snapshots: Cell::new(0),
        };

        report(Backend::Liteinst, BackendStatsRequest::ENABLED, &source);
    }

    /// The check must READ the source's own `BACKEND_NAME`, not compare against a
    /// hardcoded `"ptrace"`.
    ///
    /// ⚠️ WHY A SECOND SOURCE EXISTS PURELY FOR THIS. Every other test in this
    /// module uses `CountingSource`, whose `BACKEND_NAME` is `"ptrace"` — so
    /// replacing the right-hand side of the assert with the literal `"ptrace"`
    /// leaves all of them green. Review found exactly that mutation surviving.
    /// A matched NON-ptrace pair is the only shape that distinguishes "reads the
    /// source's declared name" from "happens to equal ptrace".
    struct LiteinstLikeSource;

    struct LiteinstLikeSnapshot;

    impl fmt::Display for LiteinstLikeSnapshot {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("liteinst-like")
        }
    }

    impl BackendStatsSnapshot for LiteinstLikeSnapshot {
        const BACKEND_NAME: &'static str = "liteinst";

        fn dispatch_stats(&self) -> Option<DispatchStats> {
            None
        }
    }

    impl BackendStatsSource for LiteinstLikeSource {
        type Snapshot = LiteinstLikeSnapshot;

        fn backend_stats(&self) -> Self::Snapshot {
            LiteinstLikeSnapshot
        }
    }

    #[test]
    fn report_accepts_a_matched_non_ptrace_source() {
        // Both gate states, because the hoist means the assert now runs in both.
        report(
            Backend::Liteinst,
            BackendStatsRequest::DISABLED,
            &LiteinstLikeSource,
        );
        report(
            Backend::Liteinst,
            BackendStatsRequest::ENABLED,
            &LiteinstLikeSource,
        );
    }

    /// The wiring check must fire even when NOBODY ASKED FOR THE RECORD.
    ///
    /// ⚠️ THIS IS THE REGRESSION IT GUARDS, and it is a whole class. The assert
    /// used to sit BELOW `request.collect(..)`'s early return, so it ran only
    /// when the level probe was on. #2587 moved that probe from INFO to DEBUG
    /// for an unrelated and correct reason — keeping harness output out of the
    /// parity envelope — and the check silently stopped executing across the CI
    /// corpus, which injects `--log info` and never `--log debug`. It did not
    /// fail. It stopped running, and a check that stops running reports nothing.
    ///
    /// The sibling test above passes `ENABLED`, so it could not have caught
    /// this: it exercises the path where the gate is already open. This one
    /// passes `DISABLED`, which is what CI actually does.
    #[test]
    #[should_panic(expected = "backend statistics source does not match selected backend")]
    fn report_checks_backend_wiring_even_when_stats_are_not_collected() {
        let source = CountingSource {
            snapshots: Cell::new(0),
        };

        report(Backend::Liteinst, BackendStatsRequest::DISABLED, &source);
    }
}
