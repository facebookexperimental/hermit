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
}
