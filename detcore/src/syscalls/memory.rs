/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic policies for memory-management advice.

use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;

use crate::Detcore;
use crate::RecordOrReplay;

const PAGE_SIZE: usize = 4096;
// Added in Linux 6.13 and not yet exposed by the pinned libc crate.
const MADV_GUARD_INSTALL: i32 = 102;
const MADV_GUARD_REMOVE: i32 = 103;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MadviseAction {
    ForwardHint,
    ForwardSemantic,
    Ignore,
    Reject(Errno),
    Unknown,
}

const fn madvise_action(advice: i32) -> MadviseAction {
    match advice {
        // Pure access-pattern and prefetch hints have no required memory-content
        // side effect. Backends without native madvise support accept them as no-ops.
        libc::MADV_NORMAL | libc::MADV_RANDOM | libc::MADV_SEQUENTIAL | libc::MADV_WILLNEED => {
            MadviseAction::ForwardHint
        }

        // Operations with guest-visible memory, fork, backing-store, dump, or guard
        // semantics must reach a backend that implements native madvise behavior.
        libc::MADV_DONTNEED
        | libc::MADV_DONTFORK
        | libc::MADV_DOFORK
        | libc::MADV_DONTDUMP
        | libc::MADV_DODUMP
        | libc::MADV_WIPEONFORK
        | libc::MADV_KEEPONFORK
        | libc::MADV_DONTNEED_LOCKED
        | MADV_GUARD_INSTALL
        | MADV_GUARD_REMOVE => MadviseAction::ForwardSemantic,

        // These are optional reclaim or asynchronous VM-policy controls. Their host
        // effects depend on memory pressure, KSM, and THP activity. Hermit accepts
        // them as fixed no-ops after deterministic argument validation; it deliberately
        // does not reproduce each advice's host- and mapping-specific EINVAL cases.
        libc::MADV_FREE
        | libc::MADV_MERGEABLE
        | libc::MADV_UNMERGEABLE
        | libc::MADV_HUGEPAGE
        | libc::MADV_NOHUGEPAGE
        | libc::MADV_COLD
        | libc::MADV_PAGEOUT => MadviseAction::Ignore,

        // Hole punching mutates backing storage and every mapping alias. Refuse it
        // until Detcore can update file resources and replay all affected aliases.
        libc::MADV_REMOVE => MadviseAction::Reject(Errno::EINVAL),

        // Successful population and collapse promise synchronous, resource-
        // dependent work. Report the same deterministic error Linux uses when
        // an advice value is unsupported so callers can take their fallback.
        libc::MADV_POPULATE_READ | libc::MADV_POPULATE_WRITE | libc::MADV_COLLAPSE => {
            MadviseAction::Reject(Errno::EINVAL)
        }

        // Never let a guest inject host memory failures, even if the container
        // unexpectedly has enough privilege to make these operations succeed.
        libc::MADV_HWPOISON | libc::MADV_SOFT_OFFLINE => MadviseAction::Reject(Errno::EPERM),

        _ => MadviseAction::Unknown,
    }
}

fn validate_common_args(call: syscalls::Madvise) -> Result<(), Error> {
    let start = call.addr().map(AddrMut::as_raw).unwrap_or(0);
    if !start.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL.into());
    }
    if call.len() == 0 {
        return Ok(());
    }

    let end = start.checked_add(call.len()).ok_or(Errno::EINVAL)?;
    end.checked_add(PAGE_SIZE - 1).ok_or(Errno::EINVAL)?;
    Ok(())
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Apply a deterministic policy to madvise(2).
    ///
    /// Ptrace/DBI forward hints and supported advice with guest-visible semantics.
    /// Record/replay accepts pure hints as no-ops and rejects guest-semantic advice
    /// because replay replaces file mappings with anonymous mappings. Reclaim and
    /// asynchronous VM-policy
    /// advice receives fixed success without exposing host memory pressure. Resource-
    /// dependent, backing-store, and hardware-failure operations receive fixed errors.
    /// KVM accepts pure hints as no-ops and reports ENOSYS for guest-visible semantics
    /// its executor cannot provide.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#548): Recheck advice policy and record/replay boundaries.
    pub async fn handle_madvise<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Madvise,
    ) -> Result<i64, Error> {
        let advice = call.advice();
        let action = madvise_action(advice);
        validate_common_args(call)?;

        if call.len() == 0 {
            return match action {
                MadviseAction::Unknown => Err(Errno::EINVAL.into()),
                _ => Ok(0),
            };
        }
        if self.cfg.recordreplay_modes {
            match action {
                MadviseAction::ForwardHint => {
                    crate::detlog!(
                        "[dtid {}] madvise hint {} accepted as record/replay no-op",
                        guest.thread_state().dettid,
                        advice,
                    );
                    return Ok(0);
                }
                MadviseAction::ForwardSemantic => {
                    crate::detlog!(
                        "[dtid {}] madvise advice {} unsupported in record/replay",
                        guest.thread_state().dettid,
                        advice,
                    );
                    return Err(Errno::ENOSYS.into());
                }
                MadviseAction::Ignore | MadviseAction::Reject(_) | MadviseAction::Unknown => {}
            }
        }

        match action {
            MadviseAction::ForwardHint if self.cfg.backend_supports_madvise => {
                Ok(self.record_or_replay(guest, call).await?)
            }
            MadviseAction::ForwardHint => {
                crate::detlog!(
                    "[dtid {}] madvise hint {} accepted as backend no-op",
                    guest.thread_state().dettid,
                    advice,
                );
                Ok(0)
            }
            MadviseAction::ForwardSemantic if self.cfg.backend_supports_madvise => {
                Ok(self.record_or_replay(guest, call).await?)
            }
            MadviseAction::ForwardSemantic => {
                crate::detlog!(
                    "[dtid {}] madvise advice {} is unsupported by this backend",
                    guest.thread_state().dettid,
                    advice,
                );
                Err(Errno::ENOSYS.into())
            }
            MadviseAction::Ignore => {
                crate::detlog!(
                    "[dtid {}] madvise advice {} accepted as deterministic no-op",
                    guest.thread_state().dettid,
                    advice,
                );
                Ok(0)
            }
            MadviseAction::Reject(errno) => {
                crate::detlog!(
                    "[dtid {}] madvise advice {} rejected with {}",
                    guest.thread_state().dettid,
                    advice,
                    errno,
                );
                Err(errno.into())
            }
            MadviseAction::Unknown => {
                crate::detlog!(
                    "[dtid {}] unknown madvise advice {} rejected with EINVAL",
                    guest.thread_state().dettid,
                    advice,
                );
                Err(Errno::EINVAL.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn madvise_known_linux_advice_has_an_explicit_policy() {
        for advice in [
            libc::MADV_NORMAL,
            libc::MADV_RANDOM,
            libc::MADV_SEQUENTIAL,
            libc::MADV_WILLNEED,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::ForwardHint);
        }

        for advice in [
            libc::MADV_DONTNEED,
            libc::MADV_DONTFORK,
            libc::MADV_DOFORK,
            libc::MADV_DONTDUMP,
            libc::MADV_DODUMP,
            libc::MADV_WIPEONFORK,
            libc::MADV_KEEPONFORK,
            libc::MADV_DONTNEED_LOCKED,
            MADV_GUARD_INSTALL,
            MADV_GUARD_REMOVE,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::ForwardSemantic);
        }

        for advice in [
            libc::MADV_FREE,
            libc::MADV_MERGEABLE,
            libc::MADV_UNMERGEABLE,
            libc::MADV_HUGEPAGE,
            libc::MADV_NOHUGEPAGE,
            libc::MADV_COLD,
            libc::MADV_PAGEOUT,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::Ignore);
        }

        for advice in [
            libc::MADV_REMOVE,
            libc::MADV_POPULATE_READ,
            libc::MADV_POPULATE_WRITE,
            libc::MADV_COLLAPSE,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::Reject(Errno::EINVAL));
        }
        for advice in [libc::MADV_HWPOISON, libc::MADV_SOFT_OFFLINE] {
            assert_eq!(madvise_action(advice), MadviseAction::Reject(Errno::EPERM));
        }
        assert_eq!(madvise_action(i32::MAX), MadviseAction::Unknown);
    }

    #[test]
    fn common_argument_validation_is_host_independent() {
        let aligned = unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(0x1000) };
        assert!(
            validate_common_args(
                syscalls::Madvise::new()
                    .with_addr(Some(aligned))
                    .with_len(0)
                    .with_advice(libc::MADV_FREE),
            )
            .is_ok()
        );

        let unaligned = unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(0x1001) };
        assert!(
            validate_common_args(
                syscalls::Madvise::new()
                    .with_addr(Some(unaligned))
                    .with_len(0)
                    .with_advice(libc::MADV_FREE),
            )
            .is_err()
        );

        let near_end =
            unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(usize::MAX & !(PAGE_SIZE - 1)) };
        assert!(
            validate_common_args(
                syscalls::Madvise::new()
                    .with_addr(Some(near_end))
                    .with_len(PAGE_SIZE)
                    .with_advice(libc::MADV_FREE),
            )
            .is_err()
        );
    }
}
