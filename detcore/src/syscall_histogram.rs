/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore's side of the per-run syscall histogram
//! (`detcore_model::syscall_histogram`).
//!
//! `ThreadStats::count_syscall` counts the entry. When the handler has its
//! final result, [`finish`] records one outcome:
//!
//! - **refused** when the guest gets `ENOSYS`, from Detcore or from the
//!   backend, or when one of Detcore's refusal policies answered the call and
//!   called [`mark_refused`]: a fixed-errno refusal arm of the dispatch, the
//!   signal-phase-1 `SIGALRM` refusal, a `/proc` level refusal, an
//!   unserviceable-operation refusal, or a backend's typed refusal of an
//!   injection that no handler consumed;
//! - otherwise **backend** when the backend counted at least one injection
//!   ([`reverie::Guest::injected_syscalls`]) between the entry and the result;
//! - otherwise **detcore**.
//!
//! A handler that ends in a non-errno error, or a backend that does not count
//! injections, records no outcome; the call stays counted as an entry. So do
//! calls that never return to the handler (`exit`, `exit_group`, a successful
//! `execve`).
//!
//! Nothing here reads guest memory or registers, injects anything, or talks to
//! the scheduler: the counts live in the thread's own state until it
//! deregisters.

use detcore_model::syscall_histogram::SyscallOutcome;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls::Errno;
use reverie::syscalls::Sysno;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;

/// Record that a Detcore refusal policy answered the current syscall.
pub(crate) fn mark_refused<T, G>(guest: &mut G)
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    guest.thread_state_mut().stats.syscall_refused = true;
}

/// The outcome of a completed call from its result, whether a refusal policy
/// answered it, and the backend's injection counts at its entry and now.
pub(crate) fn outcome(
    result: &Result<i64, Error>,
    refused: bool,
    injected_at_entry: Option<u64>,
    injected_now: Option<u64>,
) -> Option<SyscallOutcome> {
    match result {
        Err(Error::Errno(Errno::ENOSYS)) => return Some(SyscallOutcome::Refused),
        Ok(_) | Err(Error::Errno(_)) if refused => return Some(SyscallOutcome::Refused),
        Ok(_) | Err(Error::Errno(_)) => {}
        Err(_) => return None,
    }
    match (injected_at_entry, injected_now) {
        (Some(entry), Some(now)) if now > entry => Some(SyscallOutcome::Backend),
        (Some(_), Some(_)) => Some(SyscallOutcome::Detcore),
        _ => None,
    }
}

/// Record the outcome of the current call of `sysno` in the thread's
/// histogram.
pub(crate) fn finish<T, G>(
    guest: &mut G,
    sysno: Sysno,
    result: &Result<i64, Error>,
    injected_at_entry: Option<u64>,
) where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let injected_now = guest.injected_syscalls();
    let stats = &mut guest.thread_state_mut().stats;
    if let Some(outcome) = outcome(
        result,
        stats.syscall_refused,
        injected_at_entry,
        injected_now,
    ) {
        stats.syscall_histogram.finish(sysno, outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enosys_is_refused_wherever_it_came_from() {
        let enosys = Err(Error::Errno(Errno::ENOSYS));
        for (entry, now) in [(Some(0), Some(0)), (Some(0), Some(1)), (None, None)] {
            assert_eq!(
                outcome(&enosys, false, entry, now),
                Some(SyscallOutcome::Refused)
            );
        }
    }

    #[test]
    fn a_marked_refusal_is_refused_even_after_an_injection() {
        let eperm = Err(Error::Errno(Errno::EPERM));
        assert_eq!(
            outcome(&eperm, true, Some(3), Some(4)),
            Some(SyscallOutcome::Refused)
        );
        assert_eq!(
            outcome(&Ok(0), true, Some(3), Some(3)),
            Some(SyscallOutcome::Refused)
        );
    }

    #[test]
    fn an_injection_makes_it_a_backend_call() {
        assert_eq!(
            outcome(&Ok(5), false, Some(7), Some(9)),
            Some(SyscallOutcome::Backend)
        );
        // An errno the backend returned is still the backend's answer.
        assert_eq!(
            outcome(&Err(Error::Errno(Errno::EBADF)), false, Some(7), Some(8)),
            Some(SyscallOutcome::Backend)
        );
    }

    #[test]
    fn no_injection_makes_it_a_detcore_answer() {
        assert_eq!(
            outcome(&Ok(0), false, Some(7), Some(7)),
            Some(SyscallOutcome::Detcore)
        );
        assert_eq!(
            outcome(&Err(Error::Errno(Errno::EBADF)), false, Some(0), Some(0)),
            Some(SyscallOutcome::Detcore)
        );
    }

    #[test]
    fn unmeasured_backends_and_tool_failures_record_nothing() {
        assert_eq!(outcome(&Ok(0), false, None, None), None);
        assert_eq!(outcome(&Ok(0), false, Some(1), None), None);
        let failure = Err(Error::Tool(anyhow::anyhow!("replay failed")));
        assert_eq!(outcome(&failure, false, Some(0), Some(1)), None);
        assert_eq!(outcome(&failure, true, Some(0), Some(0)), None);
    }
}
