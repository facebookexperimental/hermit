/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Skid overshoots counted as events, not as reports.
//!
//! Two sites report a precise PMU timer event that took effect past its
//! target. Reverie reports it where its timer finds the thread past the
//! target, usually with a [`reverie::SkidOvershootDetail`] naming the thread
//! and the target. Detcore's prehook reports it when the thread ran more
//! branches since its last handler than its timer was armed for
//! ([`record_rcb_overshoot_report`], called by `report_rcb_overshoot`). Both
//! increment Reverie's process-global count
//! ([`reverie::record_skid_overshoot`]), so one late event usually counts
//! twice. That count stays a count of reports: a refusal, and `--verify`'s
//! attribution of a divergence to skid, need only that it is nonzero, and
//! Detcore's report is what keeps it nonzero for an overshoot no Reverie site
//! saw.
//!
//! Detcore keys each report by thread and target, the pair a detail names, and
//! [`SkidOvershootCounts::count`] counts a Detcore report whose pair matches a
//! detail once.
//!
//! The pair is matched rather than the order of detection, because the two
//! sites detect in either order. Reverie records a late delivery before the
//! Tool's timer callback, whose prehook reports it again; it records an event
//! that another stop overtook only when that stop is settled, after the Tool's
//! callback for the stop, and so after the prehook's report and after any other
//! thread the prehook yielded to has run.

use std::collections::HashMap;
use std::sync::Mutex;

use reverie::SkidOvershootDetail;
use reverie::Tid;

/// One skid overshoot Detcore's prehook found: the thread, and the Reverie
/// clock value at which the timer Detcore had armed for it was due. These are
/// the `tid` and `rcb_target` of a [`SkidOvershootDetail`] that Reverie records
/// for the same event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RcbOvershootReport {
    /// The thread whose timer was late.
    pub(crate) tid: Tid,
    /// The Reverie clock value at which its timer was due.
    pub(crate) rcb_target: u64,
}

/// How many [`RcbOvershootReport`]s are kept between takes; later ones are
/// counted in [`RcbOvershootReports::unkept`]. Large enough that no run seen so
/// far comes near it, and bounded, as Reverie bounds its details.
pub(crate) const RCB_OVERSHOOT_REPORTS_KEPT: usize = 1 << 16;

/// Detcore's reports since the last [`take_rcb_overshoot_reports`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RcbOvershootReports {
    /// The first [`RCB_OVERSHOOT_REPORTS_KEPT`] reports, in detection order.
    pub(crate) kept: Vec<RcbOvershootReport>,
    /// Reports past the first [`RCB_OVERSHOOT_REPORTS_KEPT`].
    pub(crate) unkept: u64,
}

impl RcbOvershootReports {
    /// All of Detcore's reports, kept or not.
    fn total(&self) -> u64 {
        u64::try_from(self.kept.len())
            .unwrap_or(u64::MAX)
            .saturating_add(self.unkept)
    }
}

static DETCORE_STORE: Mutex<RcbOvershootReports> = Mutex::new(RcbOvershootReports {
    kept: Vec::new(),
    unkept: 0,
});

/// Detcore's own reports, since the last [`take_rcb_overshoot_reports`].
fn detcore_store() -> std::sync::MutexGuard<'static, RcbOvershootReports> {
    DETCORE_STORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record a report of Detcore's prehook. Its caller also counts it with
/// [`reverie::record_skid_overshoot`].
pub(crate) fn record_rcb_overshoot_report(report: RcbOvershootReport) {
    let mut store = detcore_store();
    if store.kept.len() < RCB_OVERSHOOT_REPORTS_KEPT {
        store.kept.push(report);
    } else {
        store.unkept = store.unkept.saturating_add(1);
    }
}

/// Take Detcore's reports since the last take and start anew.
pub(crate) fn take_rcb_overshoot_reports() -> RcbOvershootReports {
    std::mem::take(&mut *detcore_store())
}

/// The skid overshoots of one run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkidOvershootCounts {
    /// Distinct overshoot events: a Detcore report and a Reverie report of the
    /// same thread and target count once. At most `reports`, and zero only
    /// when `reports` is.
    pub events: u64,
    /// Reports of every site, Reverie's process-global count.
    pub reports: u64,
}

impl SkidOvershootCounts {
    /// Count the events among `all_reports`, Reverie's count of every site's
    /// reports, given the `details` Reverie kept and `detcore`, Detcore's own
    /// reports, all taken since the same point.
    ///
    /// A Detcore report whose thread and target match a detail is that
    /// detail's event. A Reverie report can lack a detail, since Reverie
    /// keeps only its first [`reverie::SKID_OVERSHOOT_DETAILS_KEPT`] and its
    /// block-step guard records none. A Detcore report left unmatched is then
    /// taken to be such a report's event, while both remain: a Detcore report
    /// whose event Reverie also reported is the usual case.
    ///
    /// So the count is exact whenever every Detcore report has a Reverie
    /// report with the same thread and target, however many details Reverie
    /// kept. It counts one event twice only if the two sites name different
    /// targets for it while Reverie keeps that event's detail, and it
    /// undercounts only a Detcore report with no Reverie twin that is taken
    /// for a detail-less Reverie report of another event.
    pub(crate) fn count(
        all_reports: u64,
        details: &[SkidOvershootDetail],
        detcore: &RcbOvershootReports,
    ) -> Self {
        let detcore_reports = detcore.total();
        let reverie_reports = all_reports.saturating_sub(detcore_reports);
        let mut unmatched: HashMap<(Tid, u64), u64> = HashMap::new();
        for detail in details {
            *unmatched
                .entry((detail.tid, detail.rcb_target))
                .or_default() += 1;
        }
        let mut matched = 0u64;
        for report in &detcore.kept {
            if let Some(left) = unmatched.get_mut(&(report.tid, report.rcb_target))
                && *left > 0
            {
                *left -= 1;
                matched += 1;
            }
        }
        // A detail with no count of its own (a take that drained one store
        // and not the other) is no report to merge.
        let detailed = u64::try_from(details.len())
            .unwrap_or(u64::MAX)
            .min(reverie_reports);
        let matched = matched.min(detailed);
        let paired = (detcore_reports - matched).min(reverie_reports - detailed);
        // matched + paired is at most reverie_reports and at most
        // detcore_reports: less than all_reports while either is nonzero, so
        // events is zero only when all_reports is.
        let counts = Self {
            events: all_reports - matched - paired,
            reports: all_reports,
        };
        debug_assert!(
            counts.events <= counts.reports && (counts.events > 0) == (counts.reports > 0),
            "{counts:?}"
        );
        counts
    }
}

/// Take every site's skid overshoots since the last take: Reverie's count and
/// details and Detcore's reports, counted with [`SkidOvershootCounts::count`].
/// Returns the details as well, in detection order.
pub fn take_skid_overshoots() -> (SkidOvershootCounts, Vec<SkidOvershootDetail>) {
    let all_reports = reverie::take_skid_overshoot_count();
    let details = reverie::take_skid_overshoot_details();
    let detcore = take_rcb_overshoot_reports();
    (
        SkidOvershootCounts::count(all_reports, &details, &detcore),
        details,
    )
}

/// [`take_skid_overshoots`]'s counts, read without taking them, as a report
/// written before the run's final take does. Reverie returns its details only
/// by taking them, so they are put back in order: unchanged, since this runs
/// once the run's threads have stopped and no site records in between, and
/// all of them, since no more than Reverie keeps were taken. A Reverie
/// accessor that reads the details without taking them, as
/// `reverie::skid_overshoot_count` reads the count, would replace the take
/// and re-record; that is the clean follow-up, left out of this change to
/// keep Reverie's pin.
pub fn skid_overshoot_counts() -> SkidOvershootCounts {
    let all_reports = reverie::skid_overshoot_count();
    let details = reverie::take_skid_overshoot_details();
    let detcore = detcore_store().clone();
    let counts = SkidOvershootCounts::count(all_reports, &details, &detcore);
    for detail in details {
        reverie::record_skid_overshoot_detail(detail);
    }
    counts
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Serializes the tests that use the process-global counts and details.
    static GLOBAL_SKID_OVERSHOOTS: Mutex<()> = Mutex::new(());

    pub(crate) fn lock_global_skid_overshoots() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_SKID_OVERSHOOTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn tid(raw: i32) -> Tid {
        Tid::from_raw(raw)
    }

    fn detail(raw_tid: i32, rcb_target: u64) -> SkidOvershootDetail {
        SkidOvershootDetail {
            tid: tid(raw_tid),
            kind: reverie::SkidOvershootKind::DeliveredLate,
            rcb_actual: rcb_target + 13,
            rcb_target,
            overflow_point: Some(rcb_target),
            overflow_recorded: Some(true),
            sample_age_ns: Some(400_000),
        }
    }

    fn detcore(pairs: &[(i32, u64)]) -> RcbOvershootReports {
        RcbOvershootReports {
            kept: pairs
                .iter()
                .map(|&(raw_tid, rcb_target)| RcbOvershootReport {
                    tid: tid(raw_tid),
                    rcb_target,
                })
                .collect(),
            unkept: 0,
        }
    }

    fn count(all_reports: u64, details: &[SkidOvershootDetail], ours: &[(i32, u64)]) -> (u64, u64) {
        let counts = SkidOvershootCounts::count(all_reports, details, &detcore(ours));
        (counts.events, counts.reports)
    }

    #[test]
    fn one_event_reported_by_both_sites_counts_once() {
        assert_eq!(count(2, &[detail(5, 1_000)], &[(5, 1_000)]), (1, 2));
    }

    #[test]
    fn a_detection_by_one_site_counts_once() {
        // Detcore's prehook alone: the safety net for an overshoot no Reverie
        // site counted.
        assert_eq!(count(1, &[], &[(5, 1_000)]), (1, 1));
        // Reverie alone.
        assert_eq!(count(1, &[detail(5, 1_000)], &[]), (1, 1));
    }

    #[test]
    fn two_events_count_twice() {
        let both = [detail(5, 1_000), detail(5, 2_000)];
        assert_eq!(count(4, &both, &[(5, 1_000), (5, 2_000)]), (2, 4));
        // The same target on two threads is two events.
        let threads = [detail(5, 1_000), detail(6, 1_000)];
        assert_eq!(count(4, &threads, &[(6, 1_000), (5, 1_000)]), (2, 4));
        // A Detcore report of another target beside a Reverie event.
        assert_eq!(count(2, &[detail(5, 1_000)], &[(5, 1_500)]), (2, 2));
        // No reports, no events.
        assert_eq!(count(0, &[], &[]), (0, 0));
    }

    #[test]
    fn reverie_reports_without_a_detail_take_up_unmatched_detcore_reports() {
        // The block-step guard counts without a detail.
        assert_eq!(count(2, &[], &[(5, 1_000)]), (1, 2));
        // Twenty events, each reported by both sites; Reverie kept details of
        // the first sixteen.
        let targets: Vec<u64> = (1..=20).map(|n| n * 1_000).collect();
        let details: Vec<_> = targets
            .iter()
            .take(reverie::SKID_OVERSHOOT_DETAILS_KEPT)
            .map(|&target| detail(5, target))
            .collect();
        let ours: Vec<_> = targets.iter().map(|&target| (5, target)).collect();
        assert_eq!(count(40, &details, &ours), (20, 40));
        // Detcore reports past those it keeps pair the same way.
        let mut capped = detcore(&ours[..16]);
        capped.unkept = 4;
        let counts = SkidOvershootCounts::count(40, &details, &capped);
        assert_eq!((counts.events, counts.reports), (20, 40));
    }

    #[test]
    fn inconsistent_inputs_never_lose_every_event() {
        // A detail with no report to go with it.
        assert_eq!(count(1, &[detail(5, 1_000)], &[(5, 1_000)]), (1, 1));
        // More Detcore reports than reports at all.
        assert_eq!(count(1, &[], &[(5, 1_000), (5, 2_000)]), (1, 1));
        // Never zero while reports are not.
        for all_reports in 1..6 {
            for matching in 0..3 {
                let details: Vec<_> = (0..matching).map(|n| detail(5, n)).collect();
                let ours: Vec<_> = (0..matching).map(|n| (5, n)).collect();
                assert!(count(all_reports, &details, &ours).0 > 0);
            }
        }
    }

    #[test]
    fn detcore_keeps_a_bounded_number_of_reports() {
        let almost_full = RcbOvershootReports {
            kept: vec![
                RcbOvershootReport {
                    tid: tid(5),
                    rcb_target: 1,
                };
                RCB_OVERSHOOT_REPORTS_KEPT - 1
            ],
            unkept: 0,
        };
        let _lock = lock_global_skid_overshoots();
        *detcore_store() = almost_full;
        record_rcb_overshoot_report(RcbOvershootReport {
            tid: tid(5),
            rcb_target: 2,
        });
        record_rcb_overshoot_report(RcbOvershootReport {
            tid: tid(5),
            rcb_target: 3,
        });
        let taken = take_rcb_overshoot_reports();
        assert_eq!(taken.kept.len(), RCB_OVERSHOOT_REPORTS_KEPT);
        assert_eq!(taken.kept.last().map(|report| report.rcb_target), Some(2));
        assert_eq!(taken.unkept, 1);
        assert_eq!(taken.total(), RCB_OVERSHOOT_REPORTS_KEPT as u64 + 1);
        assert_eq!(take_rcb_overshoot_reports(), RcbOvershootReports::default());
    }
}
