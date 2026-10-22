/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! What a syscall interrupted for a caught signal gives the guest, by Linux's
//! x86 table (A5 design 16.6 and D40, D43).
//!
//! A wait that Detcore ends because a caught signal is pending ends with one
//! of the kernel's private restart codes. On ptrace the kernel applies the
//! table itself when it builds the handler's frame. Where Detcore ends the
//! wait and the kernel never sees the code (an in-guest backend under its
//! delivery gate, `Config::in_guest_signal_delivery`), Detcore applies the
//! same table, with the action of the first caught delivery at that return,
//! and the backend only writes the registers. Ignored instances ahead of it
//! are discarded without touching the registers, and a later frame at the
//! same return sees the result the first one left (`-EINTR`, or the call's
//! number to run again), which is no longer a restart code.

/// The kernel's private restart codes, as negative results
/// (include/linux/errno.h v6.12 lines 14-18,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/include/linux/errno.h?h=v6.12#n14>).
pub const ERESTARTSYS: i64 = -512;
/// See [`ERESTARTSYS`].
pub const ERESTARTNOINTR: i64 = -513;
/// See [`ERESTARTSYS`].
pub const ERESTARTNOHAND: i64 = -514;
/// See [`ERESTARTSYS`].
pub const ERESTART_RESTARTBLOCK: i64 = -516;

/// What the guest's syscall does once the caught signal's frame is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaughtSignalReturn {
    /// The call returns this raw result to the guest: `-EINTR` in place of a
    /// restart code, or the call's own result, which is not one.
    Returns(i64),
    /// The call runs again once the handler returns: the result register
    /// holds the syscall's number again and the instruction pointer moves
    /// back over the `syscall` instruction, so the guest executes it again.
    Restarts,
}

/// The syscall's outcome when a caught signal whose action has `sa_flags` is
/// delivered at its return with raw result `result` (an errno as its
/// negative): Linux's `handle_signal`, arch/x86/kernel/signal.c v6.12 lines
/// 264-282,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/arch/x86/kernel/signal.c?h=v6.12#n264>:
/// - `-ERESTART_RESTARTBLOCK` and `-ERESTARTNOHAND` return `-EINTR`;
/// - `-ERESTARTSYS` returns `-EINTR` without `SA_RESTART`, and restarts
///   with it;
/// - `-ERESTARTNOINTR` restarts, whatever the flags;
/// - any other result is returned as it is.
///
/// Only for the first caught delivery at the return. With no handler at all,
/// the kernel restarts instead (lines 343-357): `-ERESTARTNOHAND`,
/// `-ERESTARTSYS` and `-ERESTARTNOINTR` run the same call again, and
/// `-ERESTART_RESTARTBLOCK` runs `restart_syscall` (lines 353-356), not the
/// same call. Neither that case nor a default-fatal delivery, which ends the
/// process, comes here.
pub fn caught_signal_return(result: i64, sa_flags: u64) -> CaughtSignalReturn {
    const EINTR: i64 = -(libc::EINTR as i64);
    let sa_restart = libc::SA_RESTART as u32 as u64;
    match result {
        ERESTART_RESTARTBLOCK | ERESTARTNOHAND => CaughtSignalReturn::Returns(EINTR),
        ERESTARTSYS if sa_flags & sa_restart == 0 => CaughtSignalReturn::Returns(EINTR),
        ERESTARTSYS | ERESTARTNOINTR => CaughtSignalReturn::Restarts,
        other => CaughtSignalReturn::Returns(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EINTR: i64 = -(libc::EINTR as i64);
    const SA_RESTART: u64 = libc::SA_RESTART as u32 as u64;
    /// Flags beside `SA_RESTART`, which change no row.
    const OTHER_FLAGS: u64 = (libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_NODEFER) as u64;

    #[test]
    fn erestart_restartblock_returns_eintr_with_or_without_sa_restart() {
        for flags in [0, SA_RESTART, OTHER_FLAGS | SA_RESTART] {
            assert_eq!(
                caught_signal_return(ERESTART_RESTARTBLOCK, flags),
                CaughtSignalReturn::Returns(EINTR)
            );
        }
    }

    #[test]
    fn erestartnohand_returns_eintr_with_or_without_sa_restart() {
        for flags in [0, SA_RESTART, OTHER_FLAGS | SA_RESTART] {
            assert_eq!(
                caught_signal_return(ERESTARTNOHAND, flags),
                CaughtSignalReturn::Returns(EINTR)
            );
        }
    }

    #[test]
    fn erestartsys_without_sa_restart_returns_eintr() {
        for flags in [0, OTHER_FLAGS] {
            assert_eq!(
                caught_signal_return(ERESTARTSYS, flags),
                CaughtSignalReturn::Returns(EINTR)
            );
        }
    }

    #[test]
    fn erestartsys_with_sa_restart_restarts() {
        for flags in [SA_RESTART, OTHER_FLAGS | SA_RESTART] {
            assert_eq!(
                caught_signal_return(ERESTARTSYS, flags),
                CaughtSignalReturn::Restarts
            );
        }
    }

    #[test]
    fn erestartnointr_restarts_with_or_without_sa_restart() {
        for flags in [0, SA_RESTART, OTHER_FLAGS] {
            assert_eq!(
                caught_signal_return(ERESTARTNOINTR, flags),
                CaughtSignalReturn::Restarts
            );
        }
    }

    /// Results that are not restart codes, including the neighbours of the
    /// codes (515 is `ENOIOCTLCMD`, 517 `EPROBE_DEFER`, both kernel-private
    /// too) and `EINTR` itself, are returned as they are.
    #[test]
    fn any_other_result_is_returned_unchanged() {
        for result in [0, 1, 4096, EINTR, -(libc::EAGAIN as i64), -511, -515, -517] {
            for flags in [0, SA_RESTART] {
                assert_eq!(
                    caught_signal_return(result, flags),
                    CaughtSignalReturn::Returns(result)
                );
            }
        }
    }
}
