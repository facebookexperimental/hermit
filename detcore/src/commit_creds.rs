/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Linux's rule for when a credential change clears a task's parent-death
//! signal, shared by every path through which Detcore's model changes a
//! task's credentials: a credential setter
//! (`Detcore::reset_parent_death_on_credential_change`) and an exec
//! (`Detcore::handle_post_exec`). Linux applies it in `commit_creds`
//! (kernel/cred.c:406-413, v6.17), which both reach: a setter directly, and
//! an exec through `begin_new_exec` (fs/exec.c:1263).

use reverie::syscalls::Sysno;

/// The reason Detcore refuses an exec, by name, where it holds a
/// parent-death signal for the exec'ing task but cannot read the task's
/// permitted capability set: without the set it cannot apply the capability
/// part of the rule (`clears_parent_death`), and the signal could stay where
/// Linux clears it.
pub(crate) const UNREADABLE_PERMITTED_SET: &str = "Detcore holds a parent-death signal for this task \
     but cannot read its permitted capability set (capget refused, or no stack scratch below the \
     red zone), so it cannot apply Linux's rule that an exec growing that set clears the signal \
     (commit_creds)";

/// The parts of a task's credentials that `commit_creds` compares to decide
/// whether to clear the parent-death signal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommittedCredentials {
    pub euid: u32,
    pub egid: u32,
    pub fsuid: u32,
    pub fsgid: u32,
    /// The permitted capability set, its low 64 bits, which hold every
    /// capability Linux defines.
    pub permitted: u64,
}

impl CommittedCredentials {
    /// Detcore's fixed virtual root, every id 0, with permitted set
    /// `permitted`.
    pub(crate) fn virtual_root(permitted: u64) -> Self {
        Self {
            permitted,
            ..Self::default()
        }
    }

    /// These credentials after a successful credential setter `sysno` with
    /// `args` by a privileged caller, as Linux's setters compute them
    /// (kernel/sys.c, v6.17): `setuid` and `setgid` set every user or group
    /// id (lines 680 and 498); `setreuid`, `setregid`, `setresuid` and
    /// `setresgid` set the effective id where they give one (-1 keeps it),
    /// and the filesystem id follows the effective id (lines 617, 456, 765
    /// and 859); `setfsuid` and `setfsgid` set the filesystem id alone (lines
    /// 926 and 970). An invalid id (-1) changes nothing. The permitted set is
    /// kept: a setter can drop capabilities but never gain one. Any other
    /// call (`setgroups`, for one) changes none of these.
    pub(crate) fn after_setter(self, sysno: Sysno, args: [u64; 6]) -> Self {
        let id = |at: usize| Some(args[at] as u32).filter(|id| *id != u32::MAX);
        let mut new = self;
        match sysno {
            Sysno::setuid => {
                if let Some(uid) = id(0) {
                    new.euid = uid;
                    new.fsuid = uid;
                }
            }
            Sysno::setgid => {
                if let Some(gid) = id(0) {
                    new.egid = gid;
                    new.fsgid = gid;
                }
            }
            Sysno::setreuid | Sysno::setresuid => {
                if let Some(euid) = id(1) {
                    new.euid = euid;
                }
                new.fsuid = new.euid;
            }
            Sysno::setregid | Sysno::setresgid => {
                if let Some(egid) = id(1) {
                    new.egid = egid;
                }
                new.fsgid = new.egid;
            }
            Sysno::setfsuid => {
                if let Some(fsuid) = id(0) {
                    new.fsuid = fsuid;
                }
            }
            Sysno::setfsgid => {
                if let Some(fsgid) = id(0) {
                    new.fsgid = fsgid;
                }
            }
            _ => {}
        }
        new
    }
}

/// Whether Linux's `commit_creds` clears the parent-death signal when a
/// task's credentials move from `old` to `new` (kernel/cred.c:406-413,
/// v6.17): an effective or filesystem user or group id changes, or the
/// permitted set gains a capability (`!cred_cap_issubset`,
/// kernel/cred.c:353-375, for two credentials in one user namespace, as a
/// setter's and an exec's are). A capability dropped, or a real or saved id
/// moved alone, clears nothing.
pub(crate) fn clears_parent_death(old: &CommittedCredentials, new: &CommittedCredentials) -> bool {
    old.euid != new.euid
        || old.egid != new.egid
        || old.fsuid != new.fsuid
        || old.fsgid != new.fsgid
        || new.permitted & !old.permitted != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each id `commit_creds` compares clears the signal when it changes, and
    /// so does a capability the permitted set gains; a dropped capability or
    /// no change keeps it.
    #[test]
    fn commit_creds_clears_for_a_moved_id_or_a_gained_capability() {
        let old = CommittedCredentials::virtual_root(0b0110);
        for (new, clears) in [
            (old, false),
            (CommittedCredentials { euid: 1, ..old }, true),
            (CommittedCredentials { egid: 1, ..old }, true),
            (CommittedCredentials { fsuid: 1, ..old }, true),
            (CommittedCredentials { fsgid: 1, ..old }, true),
            (
                CommittedCredentials {
                    permitted: 0b0111,
                    ..old
                },
                true,
            ),
            (
                CommittedCredentials {
                    permitted: 0b0010,
                    ..old
                },
                false,
            ),
            (
                CommittedCredentials {
                    permitted: 0b1010,
                    ..old
                },
                true,
            ),
        ] {
            assert_eq!(clears_parent_death(&old, &new), clears, "{new:?}");
        }
    }

    /// A setter clears the parent-death signal of the virtual root exactly
    /// where it moves an effective or filesystem id away from 0: a real or
    /// saved id alone, -1, 0 and `setgroups` keep it.
    #[test]
    fn only_a_setter_that_moves_an_effective_or_filesystem_id_clears_the_signal() {
        let keep = u32::MAX as u64;
        let root = CommittedCredentials::virtual_root(u64::MAX >> 23);
        for (sysno, args, clears) in [
            (Sysno::setuid, [1000, 0, 0], true),
            (Sysno::setuid, [0, 0, 0], false),
            (Sysno::setuid, [keep, 0, 0], false),
            (Sysno::setgid, [1000, 0, 0], true),
            (Sysno::setfsuid, [1000, 0, 0], true),
            (Sysno::setfsuid, [keep, 0, 0], false),
            (Sysno::setfsgid, [1000, 0, 0], true),
            (Sysno::setreuid, [keep, 1000, 0], true),
            (Sysno::setreuid, [1000, keep, 0], false),
            (Sysno::setregid, [keep, 1000, 0], true),
            (Sysno::setregid, [1000, 0, 0], false),
            (Sysno::setresuid, [keep, 1000, keep], true),
            (Sysno::setresuid, [1000, keep, 1000], false),
            (Sysno::setresgid, [keep, 1000, keep], true),
            (Sysno::setresgid, [1000, keep, 1000], false),
            (Sysno::setgroups, [1, 0x1000, 0], false),
        ] {
            let args = [args[0], args[1], args[2], 0, 0, 0];
            let new = root.after_setter(sysno, args);
            assert_eq!(new.permitted, root.permitted, "{sysno} {args:?}");
            assert_eq!(clears_parent_death(&root, &new), clears, "{sysno} {args:?}");
        }
    }
}
