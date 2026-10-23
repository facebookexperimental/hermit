/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Actual callback identity. Expectations cannot create tokens. A syscall's
//! token lives in its handler future, so nested callbacks cannot overwrite the
//! outer syscall's occurrence. An early-returned handler simply drops its
//! token; there is no pending-map entry to leak or reuse.

use anyhow::bail;
use detcore_model::schedule::SigWrapper;
use detcore_model::schedule::boundary::Entry;
use detcore_model::schedule::boundary::Owner;
use detcore_model::schedule::boundary::Phase;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use super::Observations;
use crate::ThreadState;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Callback {
    Syscall(Sysno),
    Cpuid,
    Rdtsc,
    PmuTimer,
    NoRcbTimer,
    ThreadStart,
    Exec,
    Signal { signal: SigWrapper, announced: bool },
}

/// An owned token, deliberately not Copy. Only `begin` creates one; prehook
/// publication mutates it and posthook publication consumes it. Serialization
/// is for the enclosing thread state, whose native-cut path already refuses
/// active boundary observations. It is not a public observation input.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Token {
    owner: Owner,
    entry: Entry,
    prehook_seen: bool,
}

impl Observations {
    fn begin_callback<T>(
        &mut self,
        thread: &ThreadState<T>,
        callback: Callback,
    ) -> anyhow::Result<Token> {
        self.require_owner(thread)?;
        let entry = match callback {
            Callback::Syscall(syscall) => {
                let occurrence = self.syscall_occurrence.checked_add(1).ok_or_else(|| {
                    anyhow::anyhow!("slice-boundary protocol refusal: syscall occurrence overflow")
                })?;
                self.syscall_occurrence = occurrence;
                Entry::Syscall {
                    syscall,
                    occurrence,
                }
            }
            Callback::Cpuid => Entry::Cpuid,
            Callback::Rdtsc => Entry::Rdtsc,
            Callback::PmuTimer => Entry::PmuTimer,
            Callback::NoRcbTimer => Entry::NoRcbTimer,
            Callback::ThreadStart => Entry::ThreadStart,
            Callback::Exec => Entry::Exec,
            Callback::Signal { signal, announced } => {
                if !(1..=64).contains(&signal.raw()) {
                    bail!("slice-boundary protocol refusal: invalid actual signal entry");
                }
                let occurrence = self.signal_entry_occurrence.checked_add(1).ok_or_else(|| {
                    anyhow::anyhow!(
                        "slice-boundary protocol refusal: signal entry occurrence overflow"
                    )
                })?;
                self.signal_entry_occurrence = occurrence;
                Entry::SignalEntry {
                    signal,
                    occurrence,
                    announced,
                }
            }
        };
        Ok(Token {
            owner: self.owner,
            entry,
            prehook_seen: false,
        })
    }

    fn publish_callback<T>(
        &mut self,
        thread: &ThreadState<T>,
        token: &mut Token,
        phase: Phase,
    ) -> anyhow::Result<()> {
        if token.owner != self.owner {
            bail!("slice-boundary protocol refusal: callback token belongs to another owner");
        }
        if token.prehook_seen != (phase == Phase::Posthook) {
            bail!("slice-boundary protocol refusal: callback phase is repeated or out of order");
        }
        if phase == Phase::Posthook && matches!(token.entry, Entry::SignalEntry { .. }) {
            bail!("slice-boundary protocol refusal: signal posthook precedes actual delivery");
        }
        self.publish(thread, token.entry, phase)?;
        token.prehook_seen = true;
        Ok(())
    }
}

fn with_observations<T, R>(
    thread: &mut ThreadState<T>,
    action: impl FnOnce(&mut Observations, &ThreadState<T>) -> anyhow::Result<R>,
) -> anyhow::Result<Option<R>> {
    let Some(mut observed) = thread.slice_boundary.take() else {
        return Ok(None);
    };
    let answer = action(&mut observed, thread);
    thread.slice_boundary = Some(observed);
    answer.map(Some)
}

pub(crate) fn begin<T>(
    thread: &mut ThreadState<T>,
    callback: Option<Callback>,
) -> anyhow::Result<Option<Token>> {
    with_observations(thread, |observed, thread| {
        let callback = callback.ok_or_else(|| {
            anyhow::anyhow!(
                "slice-boundary protocol refusal: callback has no actual semantic identity"
            )
        })?;
        observed.begin_callback(thread, callback)
    })
}

pub(crate) fn pre<T>(thread: &mut ThreadState<T>, token: &mut Option<Token>) -> anyhow::Result<()> {
    with_observations(thread, |observed, thread| {
        let token = token.as_mut().ok_or_else(|| {
            anyhow::anyhow!("slice-boundary protocol refusal: prehook lost its callback token")
        })?;
        observed.publish_callback(thread, token, Phase::Prehook)
    })?;
    Ok(())
}

pub(crate) fn post<T>(thread: &mut ThreadState<T>, mut token: Option<Token>) -> anyhow::Result<()> {
    with_observations(thread, |observed, thread| {
        let token = token.as_mut().ok_or_else(|| {
            anyhow::anyhow!("slice-boundary protocol refusal: posthook lost its callback token")
        })?;
        observed.publish_callback(thread, token, Phase::Posthook)
    })?;
    Ok(())
}

/// Link this already-entered callback to the actual delivery ordinal chosen
/// by the unchanged signal code. No entry/check-in/turn is allocated here.
/// An announcement withdrawn before delivery simply drops its retained token;
/// its earlier semantic check-in and any completed boundary remain observed.
pub(crate) fn delivered_signal(
    token: &mut Option<Token>,
    signal: SigWrapper,
    zero_based_delivery: u64,
) -> anyhow::Result<()> {
    let Some(token) = token else { return Ok(()) };
    let Entry::SignalEntry {
        signal: entered,
        occurrence,
        announced,
    } = token.entry
    else {
        bail!("slice-boundary protocol refusal: delivery has no unsettled signal entry");
    };
    if !token.prehook_seen || entered != signal {
        bail!("slice-boundary protocol refusal: delivery disagrees with its actual entry");
    }
    let delivery = zero_based_delivery.checked_add(1).ok_or_else(|| {
        anyhow::anyhow!("slice-boundary protocol refusal: signal delivery ordinal overflow")
    })?;
    token.entry = Entry::Signal {
        signal,
        occurrence,
        announced,
        delivery,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use detcore_model::pid::DetTid;

    use super::*;
    use crate::config::Config;

    fn thread() -> ThreadState<()> {
        let mut thread = ThreadState::new(DetTid::from_raw(3), &Config::default(), ());
        thread.slice_boundary = Some(Observations::new(&thread, 0));
        thread
    }

    #[test]
    fn nested_syscall_tokens_keep_the_outer_actual_occurrence() {
        let mut thread = thread();
        let mut outer = begin(&mut thread, Some(Callback::Syscall(Sysno::read))).unwrap();
        pre(&mut thread, &mut outer).unwrap();
        let mut inner = begin(&mut thread, Some(Callback::Syscall(Sysno::getpid))).unwrap();
        pre(&mut thread, &mut inner).unwrap();
        post(&mut thread, inner).unwrap();
        post(&mut thread, outer).unwrap();
        let observed = thread.slice_boundary.as_ref().unwrap();
        assert_eq!(
            observed.current.unwrap().0,
            Entry::Syscall {
                syscall: Sysno::read,
                occurrence: 1
            }
        );
        assert_eq!(observed.checkin, 4);
        assert_eq!(observed.syscall_occurrence, 2);
        // An early return does not prevent the next same-site invocation.
        let _abandoned = begin(&mut thread, Some(Callback::Syscall(Sysno::read))).unwrap();
        let next = begin(&mut thread, Some(Callback::Syscall(Sysno::read)))
            .unwrap()
            .unwrap();
        assert_eq!(
            next.entry,
            Entry::Syscall {
                syscall: Sysno::read,
                occurrence: 4
            }
        );
    }

    #[test]
    fn wrong_phase_missing_identity_stale_owner_and_overflow_refuse_without_publish() {
        let mut thread = thread();
        assert!(begin(&mut thread, None).is_err());
        let mut token = begin(&mut thread, Some(Callback::Cpuid)).unwrap();
        assert!(post(&mut thread, token.clone()).is_err());
        pre(&mut thread, &mut token).unwrap();
        let before = thread.slice_boundary.clone();
        assert!(pre(&mut thread, &mut token).is_err());
        assert_eq!(thread.slice_boundary, before);
        thread.mm_id = thread.mm_id.for_exec(thread.dettid);
        assert!(post(&mut thread, token).is_err());
        assert_eq!(thread.slice_boundary, before);
        thread.slice_boundary = Some(Observations::new(&thread, 0));
        thread.slice_boundary.as_mut().unwrap().syscall_occurrence = u64::MAX;
        let before = thread.slice_boundary.clone();
        assert!(begin(&mut thread, Some(Callback::Syscall(Sysno::getpid))).is_err());
        assert_eq!(thread.slice_boundary, before);
    }

    #[test]
    fn all_actual_callback_kinds_have_distinct_identity_and_legacy_stays_inert() {
        let mut thread = thread();
        for (callback, expected) in [
            (Callback::Cpuid, Entry::Cpuid),
            (Callback::Rdtsc, Entry::Rdtsc),
            (Callback::PmuTimer, Entry::PmuTimer),
            (Callback::NoRcbTimer, Entry::NoRcbTimer),
            (Callback::ThreadStart, Entry::ThreadStart),
            (Callback::Exec, Entry::Exec),
        ] {
            let mut token = begin(&mut thread, Some(callback)).unwrap();
            pre(&mut thread, &mut token).unwrap();
            assert_eq!(
                thread.slice_boundary.as_ref().unwrap().current.unwrap().0,
                expected
            );
            post(&mut thread, token).unwrap();
        }
        thread.slice_boundary = None;
        assert!(begin(&mut thread, None).unwrap().is_none());
        pre(&mut thread, &mut None).unwrap();
        post(&mut thread, None).unwrap();
    }
    #[test]
    fn actual_signal_entry_linkage_refuses_guessing_repetition_and_overflow() {
        let mut thread = thread();
        let signal = SigWrapper(libc::SIGUSR1);
        let mut token = begin(
            &mut thread,
            Some(Callback::Signal {
                signal,
                announced: true,
            }),
        )
        .unwrap();
        assert!(delivered_signal(&mut token, signal, 0).is_err());
        pre(&mut thread, &mut token).unwrap();
        assert!(post(&mut thread, token.clone()).is_err());
        let before = token.clone();
        assert!(delivered_signal(&mut token, SigWrapper(libc::SIGUSR2), 0).is_err());
        assert!(delivered_signal(&mut token, signal, u64::MAX).is_err());
        assert_eq!(token, before);
        let state = thread.slice_boundary.clone();
        delivered_signal(&mut token, signal, 7).unwrap();
        assert_eq!(
            thread.slice_boundary, state,
            "linking does not observe a second entry"
        );
        assert!(delivered_signal(&mut token, signal, 7).is_err());
        post(&mut thread, token).unwrap();
        assert_eq!(
            thread.slice_boundary.as_ref().unwrap().current.unwrap().0,
            Entry::Signal {
                signal,
                occurrence: 1,
                announced: true,
                delivery: 8
            }
        );
        thread
            .slice_boundary
            .as_mut()
            .unwrap()
            .signal_entry_occurrence = u64::MAX;
        let before = thread.slice_boundary.clone();
        assert!(
            begin(
                &mut thread,
                Some(Callback::Signal {
                    signal,
                    announced: false
                })
            )
            .is_err()
        );
        assert_eq!(thread.slice_boundary, before);
    }
}
