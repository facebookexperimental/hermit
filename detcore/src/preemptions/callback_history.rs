/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Software callback expirations in an otherwise unchanged legacy event replay.
//! These coordinates do not program the PMU or select a runnable owner.

use std::collections::VecDeque;

use serde::Deserialize;
use serde::Serialize;

use crate::ThreadState;
use crate::slice_boundary::callbacks;
use crate::slice_boundary::callbacks::Callback;
use crate::types::LogicalTime;
use crate::types::Op;
use crate::types::SchedEvent;

/// Recording must track actual events before its first optional checkpoint.
/// Re-recording an old schedule does not manufacture replay authority.
pub(crate) fn recording_enabled(cfg: &crate::Config) -> bool {
    cfg.chaos
        && cfg.record_preemptions
        && !cfg.strict_schedule_boundaries
        && cfg.replay_schedule_from.is_none()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
/// The original callback location that ended the logical slice.
pub enum Site {
    /// Normal expiration before the handler's operation.
    Prehook,
    /// Normal expiration after the handler's operation.
    Posthook,
    /// Posthook expiration within one RCB of the physical maximum.
    PosthookPmuEpsilon,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Observed software-expiry coordinates, independent of the replay tape.
pub struct Origin {
    pub(crate) callback: Callback,
    pub(crate) site: Site,
    pub(crate) progress: u64,
    pub(crate) last_op: Option<Op>,
    pub(crate) time: LogicalTime,
    pub(crate) rcbs: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// One callback expiry attached to an existing priority-history row.
pub struct Checkpoint {
    pub(crate) history_index: usize,
    pub(crate) origin: Origin,
}

/// An owned handler identity independent of strict boundary observations.
/// Nesting keeps the outer callback intact; an early return drops only its token.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Token {
    callback: Option<Callback>,
    boundary: Option<callbacks::Token>,
    pre_cut: Option<Progress>,
    post_cut: Option<Progress>,
}

impl Token {
    /// Local phase evidence, sampled after the original event update/handoffs.
    /// It is not a coordinate in the tape: split PMU callbacks may enter with
    /// different progress before reaching the same positive-event endpoint.
    pub(crate) fn note_cut(&mut self, progress: Progress, site: Site) {
        match site {
            Site::Prehook => self.pre_cut = Some(progress),
            Site::Posthook => self.post_cut = Some(progress),
            Site::PosthookPmuEpsilon => {}
        }
    }

    fn proves_passed(&self, expected: &Origin, actual: &Origin) -> bool {
        if self.callback != Some(expected.callback) || actual.callback != expected.callback {
            return false;
        }
        let observed = match (expected.site, actual.site) {
            (Site::Prehook, Site::Posthook | Site::PosthookPmuEpsilon) => &self.pre_cut,
            (Site::Posthook, Site::PosthookPmuEpsilon) => &self.post_cut,
            _ => return false,
        };
        observed.as_ref().is_some_and(|progress| {
            progress.count == expected.progress
                && progress.last_op == expected.last_op
                && actual.time >= expected.time
                && actual.rcbs >= expected.rcbs
        })
    }
}

pub(crate) fn begin<T>(
    thread: &mut ThreadState<T>,
    callback: Option<Callback>,
) -> anyhow::Result<Option<Token>> {
    let boundary = callbacks::begin(thread, callback)?;
    Ok(Some(Token {
        callback,
        boundary,
        pre_cut: None,
        post_cut: None,
    }))
}

pub(crate) fn pre<T>(thread: &mut ThreadState<T>, token: &mut Option<Token>) -> anyhow::Result<()> {
    let mut boundary = token.as_mut().and_then(|token| token.boundary.take());
    let result = callbacks::pre(thread, &mut boundary);
    if let Some(token) = token {
        token.boundary = boundary;
    }
    result
}

pub(crate) fn post<T>(thread: &mut ThreadState<T>, token: &Option<Token>) -> anyhow::Result<()> {
    callbacks::post(
        thread,
        token.as_ref().and_then(|token| token.boundary.clone()),
    )
}

pub(crate) fn delivered_signal(
    token: &mut Option<Token>,
    signal: crate::types::SigWrapper,
    delivery: u64,
) -> anyhow::Result<()> {
    if let Some(token) = token {
        callbacks::delivered_signal(&mut token.boundary, signal, delivery)?;
    }
    Ok(())
}

/// Passive progress from actual positive-count events, never the expected tape.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub(crate) count: u64,
    pub(crate) last_op: Option<Op>,
}

impl Progress {
    pub(crate) fn observe_event(&mut self, event: &SchedEvent) {
        if event.count > 0 {
            self.count = self
                .count
                .checked_add(u64::from(event.count))
                .expect("callback event progress overflow");
            self.last_op = Some(event.op);
        }
    }
}

/// The existing startup grant restores actual progress after a Tool reload.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Startup {
    pub(crate) progress: Progress,
    pub(crate) checkpoints: Vec<Checkpoint>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct State {
    progress: Progress,
    checkpoints: VecDeque<Checkpoint>,
    active: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Decision {
    Wait,
    Act(Origin),
    Mismatch {
        checkpoint: Checkpoint,
        hard: bool,
        actual: Origin,
    },
}

/// The priority-history row whose end a thread's current slice waits for,
/// under `--replay-preemptions-from` (`ThreadState::active_priority_row`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActiveRow {
    /// No recorded row is installed yet.
    BeforeFirst,
    /// The slice ends recorded row `n`.
    Row(usize),
    /// The final slice, after the last recorded row.
    Exhausted,
}

/// Why priority-history replay refuses a recorded callback checkpoint. Each
/// names the run's policy exit (`exit_on_scheduler_refusal`); none is a
/// warning, and none falls back to the legacy RCB expiry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    /// A checkpoint of a row that already ended was never consumed.
    Skipped,
    /// The thread went past the endpoint: more progress or more RCBs than the
    /// recorded endpoint, or its own callback proves it left that site.
    Overshot,
    /// The endpoint's callback, site, progress and RCBs, but another time or
    /// last operation.
    Incompatible,
    /// Checkpoints remain where no recorded row can consume them.
    Incomplete,
}

impl Refusal {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Refusal::Skipped => "HERMIT_CALLBACK_CHECKPOINT_SKIPPED",
            Refusal::Overshot => "HERMIT_CALLBACK_CHECKPOINT_OVERSHOT",
            Refusal::Incompatible => "HERMIT_CALLBACK_CHECKPOINT_INCOMPATIBLE",
            Refusal::Incomplete => "HERMIT_CALLBACK_CHECKPOINT_INCOMPLETE",
        }
    }
}

/// What priority-history replay does at one actual callback check-in of a
/// thread whose record carries callback checkpoints.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RowDecision {
    /// The active row has no checkpoint: the legacy expiry decides.
    NoCheckpoint,
    /// The row's endpoint is still ahead: the slice does not end here.
    Pending,
    /// The actual endpoint of the row, popped: end the slice with it attached.
    Match(Checkpoint),
    /// Refuse, with the checkpoint popped from the thread's custody. `actual`
    /// is `None` at a check-in without a callback identity.
    Refuse {
        refusal: Refusal,
        checkpoint: Checkpoint,
        actual: Option<Origin>,
    },
}

impl State {
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn progress(&self) -> Progress {
        self.progress.clone()
    }
    pub fn observe_event(&mut self, event: &SchedEvent) {
        self.progress.observe_event(event);
    }

    pub fn install(&mut self, startup: Startup) {
        if self.active || self.progress != Progress::default() {
            assert_eq!(
                self.progress, startup.progress,
                "preserved Tool progress matches actual coordinator observations"
            );
            assert_eq!(
                self.checkpoints.iter().cloned().collect::<Vec<_>>(),
                startup.checkpoints,
                "preserved checkpoint custody matches coordinator"
            );
        }
        self.progress = startup.progress;
        self.checkpoints = startup.checkpoints.into();
        // Envelope presence is activation, even at zero progress with no
        // remaining checkpoints. Exhaustion and exec do not disable tracking.
        self.active = true;
    }

    pub fn origin<T>(
        &self,
        thread: &ThreadState<T>,
        token: &Option<Token>,
        site: Site,
    ) -> Option<Origin> {
        Some(Origin {
            callback: token.as_ref()?.callback?,
            site,
            progress: self.progress.count,
            last_op: self.progress.last_op,
            time: thread.thread_logical_time.as_nanos(),
            rcbs: thread.committed_clock_value,
        })
    }

    /// The actual coordinates of a check-in at `site` of the callback `token`
    /// owns, or `None` when the check-in has no callback identity.
    pub(crate) fn observed<T>(
        &self,
        thread: &ThreadState<T>,
        token: Option<&Token>,
        site: Site,
    ) -> Option<Origin> {
        Some(Origin {
            callback: token?.callback?,
            site,
            progress: self.progress.count,
            last_op: self.progress.last_op,
            time: thread.thread_logical_time.as_nanos(),
            rcbs: thread.committed_clock_value,
        })
    }

    pub fn refresh<T>(&self, thread: &ThreadState<T>, origin: &Origin) -> Origin {
        Origin {
            callback: origin.callback,
            site: origin.site,
            progress: self.progress.count,
            last_op: self.progress.last_op,
            time: thread.thread_logical_time.as_nanos(),
            rcbs: thread.committed_clock_value,
        }
    }

    /// Only an independently reached callback endpoint may actuate a checkpoint.
    /// Earlier/nested callbacks wait. Passing the endpoint reports desync; it
    /// never authorizes a late administrative operation.
    #[cfg(test)]
    pub fn check(&mut self, actual: Origin) -> Decision {
        self.check_at(actual, None)
    }

    pub(crate) fn check_at(&mut self, actual: Origin, token: Option<&Token>) -> Decision {
        let Some(expected) = self.checkpoints.front() else {
            return Decision::Wait;
        };
        if actual.progress < expected.origin.progress {
            return Decision::Wait;
        }
        if actual.progress == expected.origin.progress {
            if (actual.callback != expected.origin.callback || actual.site != expected.origin.site)
                && !token.is_some_and(|token| token.proves_passed(&expected.origin, &actual))
            {
                return Decision::Wait;
            }
            if actual.time < expected.origin.time || actual.rcbs < expected.origin.rcbs {
                return Decision::Wait;
            }
        }
        let expected = self.checkpoints.pop_front().unwrap();
        if actual == expected.origin {
            return Decision::Act(actual);
        }
        let hard = actual.progress != expected.origin.progress
            || actual.callback != expected.origin.callback
            || actual.site != expected.origin.site
            || actual.last_op != expected.origin.last_op;
        Decision::Mismatch {
            checkpoint: expected,
            hard,
            actual,
        }
    }

    /// Decide one actual check-in of priority-history replay
    /// (`--replay-preemptions-from` with callback checkpoints). Unlike
    /// `check_at`, the global schedule replay's rule, this is row-aware and
    /// strict:
    ///
    /// 1. The raw front first: a front for an earlier row than `active` was
    ///    skipped; a front for a later row leaves `active` unannotated, as is
    ///    any row before the first is installed. After the last row, any
    ///    remaining checkpoint is incomplete.
    /// 2. Passed evidence: more progress or more RCBs than the endpoint, or
    ///    the thread's own callback proving it left the endpoint's site.
    /// 3. At the endpoint's progress and RCBs, the same callback and site end
    ///    the row when its time and last operation agree too, and refuse when
    ///    they do not; another callback or site waits (a timer stop or a
    ///    prehook before a posthook endpoint, or a nested callback).
    /// 4. Below the endpoint, wait.
    ///
    /// A match or a refusal pops the front: the caller acknowledges it to the
    /// scheduler, whose own copy pops in the same order. A check-in without a
    /// callback identity (`actual` `None`) is never an endpoint: after step 1
    /// it applies step 2's progress and RCB tests to the thread's own
    /// progress and its committed RCBs `rcbs`, refusing as overshot what is
    /// past the endpoint, and otherwise waits. Without an identity there is no
    /// callback to prove a site was left. `rcbs` is read only then; an
    /// identified check-in carries its RCBs in `actual`.
    pub(crate) fn decide_row(
        &mut self,
        active: ActiveRow,
        actual: Option<Origin>,
        token: Option<&Token>,
        rcbs: u64,
    ) -> RowDecision {
        let Some(front) = self.checkpoints.front() else {
            return RowDecision::NoCheckpoint;
        };
        let row = match active {
            ActiveRow::BeforeFirst => return RowDecision::NoCheckpoint,
            ActiveRow::Exhausted => return self.refuse_front(Refusal::Incomplete, actual),
            ActiveRow::Row(row) => row,
        };
        if front.history_index < row {
            return self.refuse_front(Refusal::Skipped, actual);
        }
        if front.history_index > row {
            return RowDecision::NoCheckpoint;
        }
        let expected = front.origin.clone();
        let Some(actual) = actual else {
            if self.progress.count > expected.progress || rcbs > expected.rcbs {
                return self.refuse_front(Refusal::Overshot, None);
            }
            return RowDecision::Pending;
        };
        let passed = actual.progress > expected.progress
            || actual.rcbs > expected.rcbs
            || (actual.progress == expected.progress
                && token.is_some_and(|token| token.proves_passed(&expected, &actual)));
        if passed {
            return self.refuse_front(Refusal::Overshot, Some(actual));
        }
        if actual.progress < expected.progress || actual.rcbs < expected.rcbs {
            return RowDecision::Pending;
        }
        if actual.callback != expected.callback || actual.site != expected.site {
            return RowDecision::Pending;
        }
        if actual.time != expected.time || actual.last_op != expected.last_op {
            return self.refuse_front(Refusal::Incompatible, Some(actual));
        }
        RowDecision::Match(self.checkpoints.pop_front().unwrap())
    }

    fn refuse_front(&mut self, refusal: Refusal, actual: Option<Origin>) -> RowDecision {
        RowDecision::Refuse {
            refusal,
            checkpoint: self.checkpoints.pop_front().unwrap(),
            actual,
        }
    }

    /// The checkpoints this thread has not consumed, in order.
    pub(crate) fn remaining(&self) -> impl Iterator<Item = &Checkpoint> {
        self.checkpoints.iter()
    }

    /// Whether the next unconsumed checkpoint is that of `row`.
    pub(crate) fn annotates(&self, row: usize) -> bool {
        self.checkpoints
            .front()
            .is_some_and(|front| front.history_index == row)
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::schedule::SyscallPhase;
    use reverie::syscalls::Sysno;

    use super::*;
    use crate::config::Config;
    use crate::types::DetTid;

    fn thread() -> ThreadState<()> {
        ThreadState::new(DetTid::from_raw(3), &Config::default(), ())
    }
    fn origin() -> Origin {
        Origin {
            callback: Callback::Syscall(Sysno::close),
            site: Site::Posthook,
            progress: 2,
            last_op: Some(Op::Syscall(Sysno::close, SyscallPhase::Posthook)),
            time: LogicalTime::from_nanos(100),
            rcbs: 10,
        }
    }
    fn replay(origin: Origin) -> State {
        let mut state = State::default();
        state.install(Startup {
            checkpoints: vec![Checkpoint {
                history_index: 0,
                origin,
            }],
            ..Startup::default()
        });
        state
    }

    #[test]
    fn callback_checkpoint_requires_phase_progress_and_both_clocks() {
        let expected = origin();
        let mut state = replay(expected.clone());
        for actual in [
            Origin {
                site: Site::Prehook,
                ..expected.clone()
            },
            Origin {
                time: LogicalTime::from_nanos(75),
                ..expected.clone()
            },
            Origin {
                rcbs: 9,
                ..expected.clone()
            },
            Origin {
                progress: 1,
                ..expected.clone()
            },
            Origin {
                callback: Callback::Cpuid,
                ..expected.clone()
            },
        ] {
            assert_eq!(state.check(actual), Decision::Wait);
            assert_eq!(state.checkpoints.len(), 1);
        }
        assert_eq!(
            state.check(expected.clone()),
            Decision::Act(expected.clone())
        );
        assert_eq!(state.check(expected), Decision::Wait);
    }

    #[test]
    fn callback_checkpoint_missed_endpoint_is_hard_but_clock_drift_is_soft() {
        let expected = origin();
        for (actual, hard) in [
            (
                Origin {
                    progress: 3,
                    ..expected.clone()
                },
                true,
            ),
            (
                Origin {
                    last_op: Some(Op::OtherInstructions),
                    ..expected.clone()
                },
                true,
            ),
            (
                Origin {
                    time: LogicalTime::from_nanos(101),
                    ..expected.clone()
                },
                false,
            ),
            (
                Origin {
                    rcbs: 11,
                    ..expected.clone()
                },
                false,
            ),
        ] {
            let mut state = replay(expected.clone());
            assert!(
                matches!(state.check(actual), Decision::Mismatch { hard: actual_hard, .. } if actual_hard == hard)
            );
            assert!(state.checkpoints.is_empty());
        }
    }

    #[test]
    fn callback_checkpoint_counts_positive_events_and_preserves_nested_handler_identity() {
        let mut whole = State::default();
        let mut split = State::default();
        let tid = DetTid::from_raw(3);
        whole.observe_event(&SchedEvent::branches(tid, 10));
        for count in [0, 3, 0, 7, 0] {
            split.observe_event(&SchedEvent::branches(tid, count));
        }
        assert_eq!(whole, split);
        let mut thread = thread();
        let mut outer = begin(&mut thread, Some(Callback::Syscall(Sysno::read))).unwrap();
        pre(&mut thread, &mut outer).unwrap();
        let mut inner = begin(&mut thread, Some(Callback::Syscall(Sysno::getpid))).unwrap();
        pre(&mut thread, &mut inner).unwrap();
        post(&mut thread, &inner).unwrap();
        assert!(thread.slice_boundary.is_none());
        assert_eq!(
            whole
                .origin(&thread, &outer, Site::Posthook)
                .unwrap()
                .callback,
            Callback::Syscall(Sysno::read)
        );
        post(&mut thread, &outer).unwrap();
        let _abandoned = begin(&mut thread, Some(Callback::Cpuid)).unwrap();
        assert_eq!(
            whole
                .origin(&thread, &inner, Site::PosthookPmuEpsilon)
                .unwrap()
                .callback,
            Callback::Syscall(Sysno::getpid)
        );
    }

    #[test]
    fn callback_checkpoint_passed_owned_phase_is_hard_without_fragment_authority() {
        let expected = Origin {
            site: Site::Prehook,
            progress: 10,
            last_op: Some(Op::Branch),
            ..origin()
        };
        let tid = DetTid::from_raw(3);
        let mut whole_thread = thread();
        let mut split_thread = thread();
        split_thread
            .callback_history
            .observe_event(&SchedEvent::branches(tid, 3));
        let mut whole = begin(&mut whole_thread, Some(expected.callback))
            .unwrap()
            .unwrap();
        let mut split = begin(&mut split_thread, Some(expected.callback))
            .unwrap()
            .unwrap();
        whole_thread
            .callback_history
            .observe_event(&SchedEvent::branches(tid, 10));
        split_thread
            .callback_history
            .observe_event(&SchedEvent::branches(tid, 7));
        whole.note_cut(whole_thread.callback_history.progress(), Site::Prehook);
        split.note_cut(split_thread.callback_history.progress(), Site::Prehook);
        assert_eq!(
            whole, split,
            "only the actual cut endpoint is retained locally"
        );
        let later = Origin {
            site: Site::Posthook,
            ..expected.clone()
        };
        let nested = begin(&mut split_thread, Some(expected.callback))
            .unwrap()
            .unwrap();
        let mut state = replay(expected.clone());
        assert_eq!(state.check_at(later.clone(), Some(&nested)), Decision::Wait);
        assert_eq!(
            state.check_at(
                Origin {
                    time: LogicalTime::from_nanos(99),
                    ..later.clone()
                },
                Some(&split)
            ),
            Decision::Wait
        );
        assert!(matches!(
            state.check_at(later, Some(&split)),
            Decision::Mismatch { hard: true, .. }
        ));
        assert!(state.checkpoints.is_empty());
        let expected = Origin {
            site: Site::Posthook,
            ..expected
        };
        whole.note_cut(whole_thread.callback_history.progress(), Site::Posthook);
        let mut state = replay(expected.clone());
        assert!(matches!(
            state.check_at(
                Origin {
                    site: Site::PosthookPmuEpsilon,
                    ..expected
                },
                Some(&whole)
            ),
            Decision::Mismatch { hard: true, .. }
        ));
    }

    #[test]
    fn callback_checkpoint_wire_retains_full_positional_payload() {
        let point = Checkpoint {
            history_index: 7,
            origin: origin(),
        };
        let response = crate::tool_global::GlobalResponse::WithCallbackHistory(
            Startup {
                checkpoints: vec![point.clone()],
                ..Startup::default()
            },
            Box::new(crate::tool_global::GlobalResponse::StartNewThread(None)),
        );
        let bytes = bincode::serde::encode_to_vec(&response, bincode::config::standard()).unwrap();
        let (decoded, used): (crate::tool_global::GlobalResponse, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded, response);
        assert_eq!(used, bytes.len());
        let mut resources = crate::resources::Resources::new(DetTid::from_raw(3));
        // Boxing the rare request payload must not change either wire format.
        for inline in [None, Some(point.origin.clone())] {
            let boxed = inline.clone().map(Box::new);
            assert_eq!(
                bincode::serde::encode_to_vec(&inline, bincode::config::standard()).unwrap(),
                bincode::serde::encode_to_vec(&boxed, bincode::config::standard()).unwrap(),
            );
            assert_eq!(
                serde_json::to_vec(&inline).unwrap(),
                serde_json::to_vec(&boxed).unwrap(),
            );
        }
        resources.callback_checkpoint = Some(Box::new(point.origin));
        let bytes = bincode::serde::encode_to_vec(&resources, bincode::config::standard()).unwrap();
        let (decoded, used): (crate::resources::Resources, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded, resources);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn callback_history_activation_survives_empty_custody_and_refuses_disagreement() {
        let expected = Origin {
            progress: 0,
            last_op: None,
            ..origin()
        };
        let mut state = replay(expected.clone());
        assert_eq!(state.check(expected.clone()), Decision::Act(expected));
        assert!(state.checkpoints.is_empty());
        assert!(state.is_active());
        state.observe_event(&SchedEvent::branches(DetTid::from_raw(3), 7));
        let startup = Startup {
            progress: state.progress(),
            checkpoints: Vec::new(),
        };
        let mut reloaded = State::default();
        reloaded.install(startup.clone());
        assert_eq!(reloaded, state);
        reloaded.install(startup);
        assert_eq!(reloaded, state);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                reloaded.install(Startup::default());
            }))
            .is_err()
        );

        // An active empty envelope is not an inactive startup. A preserved
        // zero-progress Tool must refuse new, conflicting checkpoint custody.
        let mut empty = State::default();
        empty.install(Startup::default());
        assert!(empty.is_active());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                empty.install(Startup {
                    checkpoints: vec![Checkpoint {
                        history_index: 0,
                        origin: origin(),
                    }],
                    ..Startup::default()
                });
            }))
            .is_err()
        );
        for original in [State::default(), state] {
            let bytes =
                bincode::serde::encode_to_vec(&original, bincode::config::legacy()).unwrap();
            let (decoded, used): (State, usize) =
                bincode::serde::decode_from_slice(&bytes, bincode::config::legacy()).unwrap();
            assert_eq!(decoded, original);
            assert_eq!(used, bytes.len());
        }
    }

    /// Custody of priority-history checkpoints, each for its recorded row.
    fn rows(points: &[(usize, Origin)]) -> State {
        let mut state = State::default();
        state.install(Startup {
            checkpoints: points
                .iter()
                .map(|(history_index, origin)| Checkpoint {
                    history_index: *history_index,
                    origin: origin.clone(),
                })
                .collect(),
            ..Startup::default()
        });
        state
    }

    /// Priority-history replay ends an annotated row only at its recorded
    /// endpoint: not before the first row is installed, not for a row the
    /// front does not name (a legal unannotated row), not below the
    /// endpoint's progress or RCBs, not at another callback or site at the
    /// endpoint's coordinates (a timer stop or a prehook before a posthook
    /// endpoint), and not at a check-in without a callback identity. Only a
    /// match consumes the checkpoint.
    #[test]
    fn a_priority_row_ends_only_at_its_recorded_endpoint() {
        let expected = origin();
        let mut state = rows(&[(1, expected.clone())]);
        let endpoint_rcbs = expected.rcbs;
        let decide = move |state: &mut State, active, actual: Option<Origin>| {
            let rcbs = actual.as_ref().map_or(endpoint_rcbs, |actual| actual.rcbs);
            state.decide_row(active, actual, None, rcbs)
        };
        assert_eq!(
            decide(&mut state, ActiveRow::BeforeFirst, Some(expected.clone())),
            RowDecision::NoCheckpoint
        );
        assert_eq!(
            decide(&mut state, ActiveRow::Row(0), Some(expected.clone())),
            RowDecision::NoCheckpoint,
            "row 0 has no checkpoint: the legacy expiry decides it"
        );
        for actual in [
            Origin {
                progress: 1,
                ..expected.clone()
            },
            Origin {
                rcbs: 9,
                ..expected.clone()
            },
            Origin {
                site: Site::Prehook,
                ..expected.clone()
            },
            Origin {
                callback: Callback::PmuTimer,
                site: Site::Prehook,
                ..expected.clone()
            },
        ] {
            assert_eq!(
                decide(&mut state, ActiveRow::Row(1), Some(actual.clone())),
                RowDecision::Pending,
                "{actual:?}"
            );
        }
        assert_eq!(
            decide(&mut state, ActiveRow::Row(1), None),
            RowDecision::Pending
        );
        assert!(state.annotates(1), "nothing was consumed");
        assert_eq!(
            decide(&mut state, ActiveRow::Row(1), Some(expected.clone())),
            RowDecision::Match(Checkpoint {
                history_index: 1,
                origin: expected.clone(),
            })
        );
        assert_eq!(state.remaining().count(), 0);
        assert_eq!(
            decide(&mut state, ActiveRow::Exhausted, Some(expected)),
            RowDecision::NoCheckpoint,
            "a consumed history ends in the legacy final slice"
        );
    }

    /// The exact row of a retained Sapphire Rapids failure
    /// (hermit_run_chaosreplay__rs_print_nanosleep_race, F1 063d4181, CI job
    /// 45035997061922517; SPR-DIAGNOSTIC-REVIEW.md). The recording ended dtid
    /// 7's row 1 at the `write` prehook, progress 10759483, raw RCBs
    /// 10759329. The unrepaired replay ended it one event earlier, at the PMU
    /// timer stop that landed exactly on those RCBs with the same time
    /// (progress 10759482, last operation a branch), before `write` and with
    /// no conditional branch between them. That stop must leave the row
    /// pending; only the recorded `write` prehook ends it.
    #[test]
    fn the_retained_sapphire_rapids_row_waits_past_its_timer_stop_for_the_write_prehook() {
        let time = LogicalTime::from_nanos(1_792_852_217_684_087_955);
        let recorded = Origin {
            callback: Callback::Syscall(Sysno::write),
            site: Site::Prehook,
            progress: 10_759_483,
            last_op: Some(Op::OtherInstructions),
            time,
            rcbs: 10_759_329,
        };
        let timer_stop = Origin {
            callback: Callback::PmuTimer,
            site: Site::Prehook,
            progress: 10_759_482,
            last_op: Some(Op::Branch),
            time,
            rcbs: 10_759_329,
        };
        let row0_endpoint = Origin {
            callback: Callback::PmuTimer,
            site: Site::Prehook,
            progress: 2_459_633,
            last_op: Some(Op::Branch),
            time: LogicalTime::from_nanos(1_792_852_217_600_760_455),
            rcbs: 2_459_579,
        };
        let mut state = rows(&[(0, row0_endpoint.clone()), (1, recorded.clone())]);
        let RowDecision::Match(row0) =
            state.decide_row(ActiveRow::Row(0), Some(row0_endpoint), None, 2_459_579)
        else {
            panic!("row 0 ends at its recorded timer stop");
        };
        assert_eq!(row0.history_index, 0);
        assert_eq!(
            state.decide_row(ActiveRow::Row(1), Some(timer_stop), None, 10_759_329),
            RowDecision::Pending,
            "the timer stop at the endpoint's RCBs, one event early, must not end row 1"
        );
        assert!(state.annotates(1), "row 1's checkpoint was not consumed");
        assert_eq!(
            state.decide_row(
                ActiveRow::Row(1),
                Some(recorded.clone()),
                None,
                recorded.rcbs
            ),
            RowDecision::Match(Checkpoint {
                history_index: 1,
                origin: recorded,
            })
        );
        assert_eq!(state.remaining().count(), 0);
    }

    /// Every way past or beside the recorded endpoint is a named refusal that
    /// consumes the checkpoint, never a wait or a legacy fallback: more
    /// progress or more RCBs (overshot), the endpoint's coordinates with
    /// another time or last operation (incompatible), a front for a row that
    /// already ended (skipped), and checkpoints left after the last row
    /// (incomplete).
    #[test]
    fn a_priority_row_refuses_every_endpoint_it_cannot_honor() {
        let expected = origin();
        for (active, actual, refusal) in [
            (
                ActiveRow::Row(1),
                Origin {
                    progress: 3,
                    ..expected.clone()
                },
                Refusal::Overshot,
            ),
            (
                ActiveRow::Row(1),
                Origin {
                    rcbs: 11,
                    site: Site::Prehook,
                    ..expected.clone()
                },
                Refusal::Overshot,
            ),
            (
                ActiveRow::Row(1),
                Origin {
                    time: LogicalTime::from_nanos(101),
                    ..expected.clone()
                },
                Refusal::Incompatible,
            ),
            (
                ActiveRow::Row(1),
                Origin {
                    last_op: Some(Op::Branch),
                    ..expected.clone()
                },
                Refusal::Incompatible,
            ),
            (ActiveRow::Row(2), expected.clone(), Refusal::Skipped),
            (ActiveRow::Exhausted, expected.clone(), Refusal::Incomplete),
        ] {
            let mut state = rows(&[(1, expected.clone()), (3, expected.clone())]);
            assert_eq!(
                state.decide_row(active, Some(actual.clone()), None, actual.rcbs),
                RowDecision::Refuse {
                    refusal,
                    checkpoint: Checkpoint {
                        history_index: 1,
                        origin: expected.clone(),
                    },
                    actual: Some(actual.clone()),
                },
                "{active:?} {actual:?}"
            );
            assert!(
                state.annotates(3),
                "only the refused checkpoint is consumed"
            );
        }
        // A skipped front is refused even at a check-in without an identity.
        let mut state = rows(&[(1, expected.clone())]);
        assert!(matches!(
            state.decide_row(ActiveRow::Row(2), None, None, 0),
            RowDecision::Refuse {
                refusal: Refusal::Skipped,
                actual: None,
                ..
            }
        ));
        assert_eq!(
            Refusal::Skipped.name(),
            "HERMIT_CALLBACK_CHECKPOINT_SKIPPED"
        );
        assert_eq!(
            Refusal::Overshot.name(),
            "HERMIT_CALLBACK_CHECKPOINT_OVERSHOT"
        );
        assert_eq!(
            Refusal::Incompatible.name(),
            "HERMIT_CALLBACK_CHECKPOINT_INCOMPATIBLE"
        );
        assert_eq!(
            Refusal::Incomplete.name(),
            "HERMIT_CALLBACK_CHECKPOINT_INCOMPLETE"
        );
    }

    /// A priority-history check-in without a callback identity (no token, or a
    /// token without a callback) is refused as overshot by name when the
    /// thread's own progress or committed RCBs are past the active row's
    /// endpoint, instead of waiting until the timer arm's assertion; at the
    /// endpoint it still waits, and the raw front is still judged first.
    #[test]
    fn a_check_in_without_identity_past_the_endpoint_is_refused_as_overshot() {
        let expected = origin();
        // A check-in without a callback identity is never the endpoint, but
        // the thread's own progress and committed RCBs still prove when it
        // went past one: that is refused as overshot by name, with only the
        // local custody consumed. Exactly at the endpoint's progress and RCBs
        // it waits; the boundary is the identified check-in's.
        let tid = DetTid::from_raw(3);
        let at_endpoint = |progress: u32| {
            let mut state = rows(&[(1, expected.clone()), (3, expected.clone())]);
            state.observe_event(&SchedEvent::branches(tid, progress));
            state
        };
        let mut state = at_endpoint(2);
        assert_eq!(
            state.decide_row(ActiveRow::Row(1), None, None, expected.rcbs),
            RowDecision::Pending,
            "at the endpoint's progress and RCBs"
        );
        assert!(state.annotates(1), "a pending check-in consumes nothing");
        for (progress, rcbs, why) in [
            (2, expected.rcbs + 1, "one RCB past the endpoint"),
            (3, expected.rcbs, "one progress unit past the endpoint"),
        ] {
            let mut state = at_endpoint(progress);
            assert_eq!(
                state.decide_row(ActiveRow::Row(1), None, None, rcbs),
                RowDecision::Refuse {
                    refusal: Refusal::Overshot,
                    checkpoint: Checkpoint {
                        history_index: 1,
                        origin: expected.clone(),
                    },
                    actual: None,
                },
                "{why}"
            );
            assert!(
                state.annotates(3),
                "{why}: only the refused checkpoint is consumed"
            );
        }
        // The raw front is still judged first: past the endpoint, a skipped
        // front is refused as skipped and an exhausted one as incomplete.
        for (active, refusal) in [
            (ActiveRow::Row(2), Refusal::Skipped),
            (ActiveRow::Exhausted, Refusal::Incomplete),
        ] {
            let mut state = at_endpoint(3);
            assert!(
                matches!(
                    state.decide_row(active, None, None, expected.rcbs + 1),
                    RowDecision::Refuse { refusal: r, actual: None, .. } if r == refusal
                ),
                "{active:?}"
            );
        }
    }

    /// At the endpoint's progress, the thread's own callback proves it left a
    /// prehook endpoint for its posthook: overshot, not a wait. Without that
    /// evidence (another callback's token, or no cut at the endpoint) the
    /// later site waits.
    #[test]
    fn a_priority_row_refuses_a_prehook_endpoint_its_own_callback_left() {
        let tid = DetTid::from_raw(3);
        let expected = Origin {
            site: Site::Prehook,
            progress: 2,
            last_op: Some(Op::Branch),
            ..origin()
        };
        let later = Origin {
            site: Site::Posthook,
            ..expected.clone()
        };
        let mut thread = thread();
        let mut token = begin(&mut thread, Some(expected.callback))
            .unwrap()
            .unwrap();
        let unrelated = begin(&mut thread, Some(Callback::Cpuid)).unwrap().unwrap();
        let uncut = token.clone();
        thread
            .callback_history
            .observe_event(&SchedEvent::branches(tid, 2));
        token.note_cut(thread.callback_history.progress(), Site::Prehook);
        for witness in [&unrelated, &uncut] {
            let mut state = rows(&[(0, expected.clone())]);
            assert_eq!(
                state.decide_row(
                    ActiveRow::Row(0),
                    Some(later.clone()),
                    Some(witness),
                    later.rcbs
                ),
                RowDecision::Pending
            );
        }
        let mut state = rows(&[(0, expected.clone())]);
        assert!(matches!(
            state.decide_row(
                ActiveRow::Row(0),
                Some(later.clone()),
                Some(&token),
                later.rcbs
            ),
            RowDecision::Refuse {
                refusal: Refusal::Overshot,
                ..
            }
        ));
    }

    /// A thread's active row follows its own history cursor: before the first
    /// row is installed, each installed row in turn, then the final slice. Its
    /// pending checkpoint is the front checkpoint only while that names the
    /// active row.
    #[test]
    fn the_active_priority_row_follows_the_history_cursor() {
        use crate::preemptions::ThreadHistory;
        let mut thread = thread();
        assert_eq!(thread.active_priority_row(), None, "no history replayed");
        thread.preemption_points = Some(
            ThreadHistory::new()
                .with_prio_changes(vec![
                    (LogicalTime::from_nanos(10), 7),
                    (LogicalTime::from_nanos(20), 8),
                ])
                .with_preemption_rcbs(vec![10, 20])
                .into_iter(),
        );
        thread.callback_history = rows(&[(1, origin())]);
        assert_eq!(thread.active_priority_row(), Some(ActiveRow::BeforeFirst));
        assert!(thread.pending_checkpoint().is_none());
        let history = thread.preemption_points.as_mut().unwrap();
        history.next_with_rcbs().unwrap();
        assert_eq!(thread.active_priority_row(), Some(ActiveRow::Row(0)));
        assert!(
            thread.pending_checkpoint().is_none(),
            "row 0 is unannotated"
        );
        thread
            .preemption_points
            .as_mut()
            .unwrap()
            .next_with_rcbs()
            .unwrap();
        assert_eq!(thread.active_priority_row(), Some(ActiveRow::Row(1)));
        assert_eq!(thread.pending_checkpoint().unwrap().origin, origin());
        assert!(
            thread
                .preemption_points
                .as_mut()
                .unwrap()
                .next_with_rcbs()
                .is_none()
        );
        thread.stats.last_recorded_slice = Some(thread.stats.timeslice_count);
        assert_eq!(thread.active_priority_row(), Some(ActiveRow::Exhausted));
        assert!(thread.pending_checkpoint().is_none());
    }

    /// The observed origin of a check-in carries its callback and site with
    /// the thread's progress, time and RCBs now; none without an identity.
    #[test]
    fn an_observed_priority_checkin_reads_actual_coordinates() {
        let tid = DetTid::from_raw(3);
        let mut thread = thread();
        thread.committed_clock_value = 41;
        thread
            .callback_history
            .observe_event(&SchedEvent::branches(tid, 5));
        let token = begin(&mut thread, Some(Callback::Rdtsc)).unwrap().unwrap();
        let observed = thread
            .callback_history
            .observed(&thread, Some(&token), Site::Prehook)
            .unwrap();
        assert_eq!(observed.callback, Callback::Rdtsc);
        assert_eq!(observed.site, Site::Prehook);
        assert_eq!(observed.progress, 5);
        assert_eq!(observed.last_op, Some(Op::Branch));
        assert_eq!(observed.rcbs, 41);
        assert_eq!(observed.time, thread.thread_logical_time.as_nanos());
        assert_eq!(
            thread
                .callback_history
                .observed(&thread, None, Site::Prehook),
            None
        );
        let anonymous = begin(&mut thread, None).unwrap().unwrap();
        assert_eq!(
            thread
                .callback_history
                .observed(&thread, Some(&anonymous), Site::Prehook),
            None
        );
    }
}
