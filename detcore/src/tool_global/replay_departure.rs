/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Acknowledged findings within an already owned serial turn. This path must
//! precede ordinary RPC clock, grant, deadline and work-span accounting.

use reverie::Guest;
use serde::Deserialize;
use serde::Serialize;

use super::GlobalRequest as Q;
use super::GlobalResponse as R;
use super::GlobalState;
use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;
use crate::types::DetTid;
use crate::types::LogicalTime;

/// Why a finding could not be accepted. An originating faithful departure
/// cannot continue as a successful run after one of these refusals.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Refusal {
    /// Only a bare request or one nonzero counted envelope is canonical.
    Envelope,
    /// The sender has no actual live scheduler registration.
    Sender,
    /// The registered memory/incarnation does not match this request.
    Incarnation,
    /// The live sender does not own the current serial grant.
    UnownedTurn,
    /// This run is not a faithful preemption-history replay.
    Mode,
    /// The supplied change point advances rather than departing.
    Advancing,
    /// The bounded detection count cannot represent another observation.
    CountOverflow,
    /// The sender received a clock, wrapper or unrelated acknowledgment.
    Acknowledgment,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HERMIT_REPLAY_DEPARTURE: finding publication refused ({self:?}); refusing faithful replay completion"
        )
    }
}

pub(super) fn is_report(mut request: &Q) -> bool {
    loop {
        match request {
            Q::WithForwardedCount(_, inner) => request = inner,
            Q::ReplayDeparture { .. } => return true,
            _ => return false,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Acknowledgment {
    Accepted,
    Retired,
    ThreadExited,
    Refused(Refusal),
}

/// Only the dedicated neutral reply accepts publication. In particular a zero
/// clock or ordinary grant wrapper is not an observation-only acknowledgment.
pub(super) fn acknowledgment(
    response: <GlobalState as reverie::GlobalTool>::Response,
) -> Acknowledgment {
    match response {
        (None, R::ReplayDeparture(Ok(()))) => Acknowledgment::Accepted,
        (None, R::Retired) => Acknowledgment::Retired,
        (None, R::ThreadExited) => Acknowledgment::ThreadExited,
        (None, R::ReplayDeparture(Err(reason))) => Acknowledgment::Refused(reason),
        _ => Acknowledgment::Refused(Refusal::Acknowledgment),
    }
}

impl GlobalState {
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/4860)
    pub(super) fn accept_replay_departure(
        &self,
        from: reverie::Tid,
        request: <Self as reverie::GlobalTool>::Request,
    ) -> <Self as reverie::GlobalTool>::Response {
        let (_, mm, request) = request;
        let tid = DetTid::from_raw(from.as_raw());
        let mut scheduler = self.sched.lock().unwrap();
        // Terminal ownership wins even for malformed reports. No refusal latch,
        // forwarding consumption or ordinary RPC effect precedes this decision.
        if scheduler.backend_failed() || scheduler.is_unaccounted_sigkill_victim(tid) {
            return (None, R::Retired);
        }
        if scheduler.thread_is_logically_killed(tid) || !scheduler.rpc_incarnation_matches(tid, mm)
        {
            return (None, R::ThreadExited);
        }
        let accepted = (|| {
            let (produced, request) = match request {
                Q::WithForwardedCount(0, _) => return Err(Refusal::Envelope),
                Q::WithForwardedCount(count, request) => (Some(count), *request),
                request => (None, request),
            };
            let Q::ReplayDeparture {
                previous,
                observed,
                slice,
            } = request
            else {
                // A nested counted envelope never becomes valid authority.
                return Err(Refusal::Envelope);
            };
            if !scheduler.thread_was_registered(tid) || !scheduler.thread_is_live(tid) {
                return Err(Refusal::Sender);
            }
            // A fresh fork/clone has a registered thread_mm without an exec
            // transition entry. Registration + exact MM above/below, together
            // with the incarnation/tombstone check, authenticates that birth.
            if scheduler.thread_mm(tid) != Some(mm) {
                return Err(Refusal::Incarnation);
            }
            if !scheduler.holds_serial_grant(tid) {
                return Err(Refusal::UnownedTurn);
            }
            scheduler.validate_replay_departure(previous, observed)?;
            // Existing scheduler -> forwarded-source lock order. An ACK means
            // accepted finding, not a healthy forwarding interval. Bare reports
            // do not clear an interval; count mismatches retain their loss latch.
            crate::detlog::drain_forwarded_at_request(from.as_raw());
            if let Some(count) = produced {
                crate::detlog::check_forwarded_count(from.as_raw(), count);
            }
            scheduler.note_replay_departure(
                detcore_model::replay_departure::Source::ThreadFiling,
                tid,
                previous,
                observed,
                Some(slice),
            )
        })();
        if let Err(reason) = accepted {
            // A guest-side exit is not a library error. Retain the refusal at
            // its coordinator so cleanup refuses even without an accepted
            // finding; this does not count or mark the rejected payload.
            scheduler.refuse_replay_publication(tid, reason);
        }
        (None, R::ReplayDeparture(accepted))
    }
}

/// Publish from the actual faithful stale-filing branch before abandoning its
/// remaining rows. No later Resources request is needed to flush this finding.
pub(crate) async fn publish<G, T>(
    guest: &mut G,
    previous: LogicalTime,
    observed: LogicalTime,
    slice: u64,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    assert_eq!(
        DetTid::from_raw(guest.tid().as_raw()),
        guest.thread_state().dettid,
        "replay finding must come from the current image's thread"
    );
    let thread = guest.thread_state();
    let request = (
        thread.thread_logical_time.clone(),
        thread.mm_id,
        super::counted_request(Q::ReplayDeparture {
            previous,
            observed,
            slice,
        }),
    );
    let response = guest.send_rpc(request).await;
    match acknowledgment(response) {
        Acknowledgment::Accepted => {}
        Acknowledgment::Retired => std::future::pending().await,
        Acknowledgment::ThreadExited => {
            guest.thread_state().cancel_exit_group_filing();
            if !guest
                .thread_state()
                .wait_for_exit_group_robust_walk(crate::tool_local::EXIT_GROUP_ROBUST_WALK_WAIT)
            {
                panic!(
                    "EXIT_GROUP_ROBUST_WALK_TIMEOUT: replay-finding sender cannot retire before the robust-list walk finishes"
                );
            }
            guest.retire_current_thread().await
        }
        Acknowledgment::Refused(refusal) => crate::scheduler::exit_on_scheduler_refusal(&refusal),
    }
}
