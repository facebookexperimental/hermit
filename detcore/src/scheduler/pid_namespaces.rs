/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The pid every guest task has in each nested PID namespace it belongs to
//! (<https://github.com/rrnewton/hermit/issues/4155>,
//! <https://github.com/rrnewton/hermit/issues/4090>).
//!
//! Detcore names a task by its pid in the container's namespace (its DetTid).
//! A task created below a `clone(CLONE_NEWPID)` also has a pid in every
//! namespace between the container's and its own, fixed by the kernel at its
//! creation and never changed for its life. Detcore records that list once,
//! in the creator's clone turn, from the child's `NSpid` (design: dev-hermit
//! `ai_docs/hermit-4155-4090-pid-namespace-translation-design-20261012.md`).
//! Every later translation is a lookup here, taken in the caller's turn:
//!
//! - [`PidNamespaces::resolve`]: a pid the guest names, in its own namespace,
//!   to the DetTid it names there, or `None` (Linux's ESRCH).
//! - [`PidNamespaces::localize`]: a DetTid to its pid in a namespace, or
//!   `None` when the task is not visible there.
//!
//! A namespace is named by the DetPid of its init. The container's namespace
//! is `None`, where every DetTid is its own pid, so both translations are the
//! identity for a caller at depth 0.

use std::collections::BTreeMap;

use crate::types::DetPid;
use crate::types::DetTid;

/// A PID namespace: the DetPid of its init, or `None` for the container's.
pub type PidNamespace = Option<DetPid>;

/// Why a task's nested pids could not be recorded. Each is a determinism
/// loss reported by name: the task cannot be translated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PidNamespaceRecordError {
    /// The kernel gave the task a pid at or below the highest pid that
    /// namespace had handed out, so its allocator has wrapped, and which pids
    /// were free then depends on when the host reaped earlier tasks.
    Wrapped {
        namespace: DetPid,
        pid: i32,
        highest: i32,
    },
    /// The task's `NSpid` does not have one entry per level of its namespace
    /// chain, or its first entry is not its DetTid.
    Mismatch(String),
}

impl std::fmt::Display for PidNamespaceRecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PidNamespaceRecordError::Wrapped {
                namespace,
                pid,
                highest,
            } => write!(
                f,
                "the PID namespace of init {namespace} handed out pid {pid} after {highest}: its allocator wrapped, and which pids were free depends on host reap timing"
            ),
            PidNamespaceRecordError::Mismatch(detail) => write!(f, "{detail}"),
        }
    }
}

/// The record (module documentation).
#[derive(Debug, Default, Clone)]
pub struct PidNamespaces {
    /// For each task below the container's namespace, its pid in each nested
    /// namespace, outermost first: `(namespace's init, pid there)`. The last
    /// entry is the task's own namespace.
    levels: BTreeMap<DetTid, Vec<(DetPid, i32)>>,
    /// `(namespace's init, pid there)` to the task.
    tasks: BTreeMap<(DetPid, i32), DetTid>,
    /// The highest pid each namespace has handed out.
    highest: BTreeMap<DetPid, i32>,
    /// Nested tasks whose pids could not be recorded, and why. A pid such a
    /// task names, or its own pid, cannot be translated: the call is refused
    /// by name.
    untranslatable: BTreeMap<DetTid, String>,
}

/// What a pid named by a guest in a nested namespace names.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GuestPidAnswer {
    /// This task. `own_init_caught` is set when the task belongs to the
    /// caller's own namespace's init, to the signals that init has a handler
    /// for (`SigCgt`): Linux drops any other signal sent to its init from
    /// inside the namespace (`SIGNAL_UNKILLABLE`, `sig_task_ignored`), and
    /// SIGKILL and SIGSTOP always.
    Task {
        task: DetTid,
        own_init_caught: Option<u64>,
    },
    /// No task of the caller's namespace has that pid: Linux answers ESRCH.
    NoSuchPid,
    /// The caller's pids could not be recorded (the reason).
    Untranslatable(String),
}

impl PidNamespaces {
    /// The namespace `task` lives in: its innermost one.
    pub fn namespace_of(&self, task: DetTid) -> PidNamespace {
        self.levels
            .get(&task)
            .and_then(|levels| levels.last())
            .map(|(init, _)| *init)
    }

    /// The chain of nested namespaces `task` belongs to, outermost first.
    pub fn chain_of(&self, task: DetTid) -> Vec<DetPid> {
        self.levels
            .get(&task)
            .map(|levels| levels.iter().map(|(init, _)| *init).collect())
            .unwrap_or_default()
    }

    /// Record `task`'s pids. `chain` is its namespace chain, outermost first
    /// (the creator's chain, plus the task itself when it is a new
    /// namespace's init); `nspid` is its `NSpid` line, container's pid first.
    pub fn record(
        &mut self,
        task: DetTid,
        chain: &[DetPid],
        nspid: &[i32],
    ) -> Result<(), PidNamespaceRecordError> {
        if nspid.len() != chain.len() + 1 || nspid.first() != Some(&task.as_raw()) {
            return Err(PidNamespaceRecordError::Mismatch(format!(
                "task {} has NSpid {:?} for namespace chain {:?}",
                task, nspid, chain
            )));
        }
        self.forget(task);
        if chain.is_empty() {
            return Ok(());
        }
        let mut levels = Vec::with_capacity(chain.len());
        for (init, pid) in chain.iter().copied().zip(nspid[1..].iter().copied()) {
            // A namespace's init is its first task, pid 1; after that the
            // kernel hands out increasing pids until its allocator wraps.
            let fresh = init == task && pid == 1;
            if let Some(&highest) = self.highest.get(&init)
                && !fresh
                && pid <= highest
            {
                return Err(PidNamespaceRecordError::Wrapped {
                    namespace: init,
                    pid,
                    highest,
                });
            }
            levels.push((init, pid));
        }
        for &(init, pid) in &levels {
            self.highest.insert(init, pid);
            self.tasks.insert((init, pid), task);
        }
        self.levels.insert(task, levels);
        Ok(())
    }

    /// `task`, a nested task, could not be recorded (`untranslatable`).
    pub fn mark_untranslatable(&mut self, task: DetTid, reason: String) {
        self.forget(task);
        self.untranslatable.insert(task, reason);
    }

    /// Why `task`'s pids are unknown, if they are.
    pub fn untranslatable(&self, task: DetTid) -> Option<&str> {
        self.untranslatable.get(&task).map(String::as_str)
    }

    /// What `pid`, named by `caller`, names in `caller`'s namespace.
    pub fn resolve_for(&self, caller: DetTid, pid: i32) -> GuestPidAnswer {
        if let Some(reason) = self.untranslatable(caller) {
            return GuestPidAnswer::Untranslatable(reason.to_owned());
        }
        match self.resolve(self.namespace_of(caller), pid) {
            Some(task) => GuestPidAnswer::Task {
                task,
                own_init_caught: None,
            },
            None => GuestPidAnswer::NoSuchPid,
        }
    }

    /// `task`'s pid as `caller` sees it, or `Ok(None)` when it is not visible
    /// in `caller`'s namespace.
    pub fn localize_for(&self, caller: DetTid, task: DetTid) -> Result<Option<i32>, String> {
        if let Some(reason) = self.untranslatable(caller) {
            return Err(reason.to_owned());
        }
        if self.namespace_of(caller).is_some()
            && let Some(reason) = self.untranslatable(task)
        {
            return Err(reason.to_owned());
        }
        Ok(self.localize(self.namespace_of(caller), task))
    }

    /// Drop `task`'s pids: its DetTid now names a different task. A reaped
    /// task's entries are left in place until then: a pid that names it
    /// resolves to a task Detcore knows has exited, and a new task of that
    /// namespace cannot take its pid without a wrap, which is refused.
    pub fn forget(&mut self, task: DetTid) {
        self.untranslatable.remove(&task);
        if let Some(levels) = self.levels.remove(&task) {
            for key in levels {
                if self.tasks.get(&key) == Some(&task) {
                    self.tasks.remove(&key);
                }
            }
        }
    }

    /// Every recorded task of `init`'s namespace or one below it.
    pub fn members(&self, init: DetPid) -> Vec<DetTid> {
        self.levels
            .iter()
            .filter(|(_, levels)| levels.iter().any(|(level, _)| *level == init))
            .map(|(task, _)| *task)
            .collect()
    }

    /// The task a guest in `namespace` names by `pid`, or `None` when no task
    /// of that namespace has that pid (Linux answers ESRCH). `pid` must be
    /// positive.
    pub fn resolve(&self, namespace: PidNamespace, pid: i32) -> Option<DetTid> {
        match namespace {
            None => Some(DetTid::from_raw(pid)),
            Some(init) => self.tasks.get(&(init, pid)).copied(),
        }
    }

    /// `task`'s pid in `namespace`, or `None` when it is not visible there
    /// (it lives in an outer namespace, or a sibling one).
    pub fn localize(&self, namespace: PidNamespace, task: DetTid) -> Option<i32> {
        match namespace {
            None => Some(task.as_raw()),
            Some(init) => self
                .levels
                .get(&task)?
                .iter()
                .find(|(level, _)| *level == init)
                .map(|(_, pid)| *pid),
        }
    }
}

/// The `NSpid` line of task `tid` of process `pid`, as Hermit's `/proc` shows
/// it: the task's pid in the container's namespace first, then one per nested
/// level.
pub fn read_nspid(pid: DetPid, tid: DetTid) -> Result<Vec<i32>, String> {
    let path = format!("/proc/{}/task/{}/status", pid, tid);
    let status = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .ok_or_else(|| format!("{path} has no NSpid line"))?;
    line.split_whitespace()
        .map(|field| {
            field
                .parse::<i32>()
                .map_err(|e| format!("{path}: NSpid field {field:?}: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: i32) -> DetTid {
        DetTid::from_raw(n)
    }

    /// An init at container pid 4 with children 5 and 6 (local 2 and 3), and
    /// a grandchild namespace whose init is 6.
    fn family() -> PidNamespaces {
        let mut ns = PidNamespaces::default();
        ns.record(t(4), &[t(4)], &[4, 1]).unwrap();
        ns.record(t(5), &[t(4)], &[5, 2]).unwrap();
        ns.record(t(6), &[t(4), t(6)], &[6, 3, 1]).unwrap();
        ns.record(t(7), &[t(4), t(6)], &[7, 4, 2]).unwrap();
        ns
    }

    #[test]
    fn a_nested_pid_resolves_to_its_own_namespaces_task() {
        let ns = family();
        assert_eq!(ns.resolve(Some(t(4)), 1), Some(t(4)));
        assert_eq!(ns.resolve(Some(t(4)), 3), Some(t(6)));
        assert_eq!(ns.resolve(Some(t(6)), 2), Some(t(7)));
        // A number with no task in that namespace names nothing, even where
        // a container process has it.
        assert_eq!(ns.resolve(Some(t(4)), 9), None);
        assert_eq!(ns.resolve(Some(t(6)), 3), None);
        assert_eq!(ns.resolve(None, 3), Some(t(3)));
    }

    #[test]
    fn a_task_is_localized_only_where_it_is_visible() {
        let ns = family();
        assert_eq!(ns.localize(Some(t(4)), t(7)), Some(4));
        assert_eq!(ns.localize(Some(t(6)), t(7)), Some(2));
        assert_eq!(ns.localize(Some(t(6)), t(5)), None);
        assert_eq!(ns.localize(Some(t(4)), t(3)), None);
        assert_eq!(ns.localize(None, t(7)), Some(7));
        assert_eq!(ns.namespace_of(t(7)), Some(t(6)));
        assert_eq!(ns.namespace_of(t(3)), None);
        assert_eq!(ns.chain_of(t(7)), vec![t(4), t(6)]);
        assert_eq!(ns.members(t(4)), vec![t(4), t(5), t(6), t(7)]);
        assert_eq!(ns.members(t(6)), vec![t(6), t(7)]);
    }

    #[test]
    fn a_reaped_task_names_nothing() {
        let mut ns = family();
        ns.forget(t(5));
        assert_eq!(ns.resolve(Some(t(4)), 2), None);
        assert_eq!(ns.localize(Some(t(4)), t(5)), None);
    }

    #[test]
    fn a_pid_at_or_below_the_namespaces_highest_is_a_wrap() {
        let mut ns = family();
        ns.forget(t(5));
        assert_eq!(
            ns.record(t(8), &[t(4)], &[8, 2]),
            Err(PidNamespaceRecordError::Wrapped {
                namespace: t(4),
                pid: 2,
                highest: 4
            })
        );
        assert!(ns.record(t(8), &[t(4)], &[8, 5]).is_ok());
    }

    #[test]
    fn an_nspid_that_does_not_fit_the_chain_is_refused() {
        let mut ns = PidNamespaces::default();
        assert!(matches!(
            ns.record(t(4), &[t(4)], &[4]),
            Err(PidNamespaceRecordError::Mismatch(_))
        ));
        assert!(matches!(
            ns.record(t(4), &[t(4)], &[5, 1]),
            Err(PidNamespaceRecordError::Mismatch(_))
        ));
        assert!(ns.record(t(3), &[], &[3]).is_ok());
    }
}
