/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The user namespace each guest process lives in, relative to the
//! container's, as Detcore's record of how each process was made.
//!
//! Linux puts a new process in its creator's user namespace, or, for
//! `clone(CLONE_NEWUSER)` (and `clone3` with that flag), in a new namespace
//! whose parent is the creator's (`copy_creds`, `create_user_ns`). Linux has
//! two other ways into a user namespace, `unshare(CLONE_NEWUSER)` and
//! `setns`, and Detcore refuses both (EPERM,
//! `syscall_classification::is_mount_ns_admin_refused_syscall`), so the clone
//! is the only way in, and the record, kept in the creator's clone turn, is
//! complete. An exec does not change a process's namespace.
//!
//! A namespace is named by the number the record gives it when a clone makes
//! it, never reused; the container's namespace has none. A process's chain is
//! the namespaces it lives in, outermost first: its creator's chain, plus the
//! new namespace for `CLONE_NEWUSER`. The record keeps a process's chain until
//! a later process with the same number replaces it, so a process that has
//! exited and is not yet reaped still has its chain (Linux keeps its
//! credentials, and their namespace, until it is released).
//!
//! What it answers: whether a task lives in the reader's namespace, below it,
//! or elsewhere ([`UserNamespaceRelation`]), which Linux's
//! `ptrace_may_access` turns on for a reader without `CAP_SYS_PTRACE` in its
//! effective set (`Detcore::io_read_allowed_by_number`). Linux's rule also
//! asks who owns the namespace just below the reader's on the task's chain
//! (`cap_capable`'s owner rule, which asks about no other namespace): the
//! effective user ID of the task that made it. The record keeps, for each
//! namespace, whether its creator's effective user ID may not have been its
//! real one ([`UserNamespaces::note_owner_may_differ`]), as the creator
//! reported it before its clone, noted only once the clone has made the
//! namespace.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::types::DetPid;

/// Where a task's user namespace is, from a reader's.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize
)]
pub enum UserNamespaceRelation {
    /// The task lives in the reader's own namespace.
    Same,
    /// The task lives in a namespace below the reader's: the reader's is an
    /// ancestor of it.
    Below,
    /// Anywhere else: above the reader's, or in another branch.
    Elsewhere,
}

/// The record (module documentation).
#[derive(Debug, Default, Clone)]
pub struct UserNamespaces {
    /// Each process below the container's namespace: the namespaces it lives
    /// in, outermost first. A process with no entry lives in the container's.
    chains: BTreeMap<DetPid, Vec<u64>>,
    /// How many namespaces the record has named.
    named: u64,
    /// The namespaces a task made whose effective user ID was not its real
    /// user ID, read as the overflow ID, or could not be read: such a
    /// namespace's owner may not be the effective user ID of a reader whose
    /// effective ID is its real one.
    owners_may_differ: BTreeSet<u64>,
}

impl UserNamespaces {
    /// A new process `child` starts in its creator's namespace (`creator`;
    /// `None` for the run's first process, which lives in the container's).
    /// A process that reuses an earlier one's number replaces its chain.
    pub fn inherit(&mut self, child: DetPid, creator: Option<DetPid>) {
        let chain = creator
            .and_then(|creator| self.chains.get(&creator))
            .cloned()
            .unwrap_or_default();
        if chain.is_empty() {
            self.chains.remove(&child);
        } else {
            self.chains.insert(child, chain);
        }
    }

    /// `process`, which `clone(CLONE_NEWUSER)` just made, lives in a new
    /// namespace below the one it inherited ([`Self::inherit`]).
    pub fn enter_new(&mut self, process: DetPid) {
        self.named += 1;
        let named = self.named;
        self.chains.entry(process).or_default().push(named);
    }

    /// Where `task`'s namespace is from `reader`'s.
    pub fn relation(&self, reader: DetPid, task: DetPid) -> UserNamespaceRelation {
        let reader = self.chain(reader);
        let task = self.chain(task);
        if reader == task {
            UserNamespaceRelation::Same
        } else if task.starts_with(reader) {
            UserNamespaceRelation::Below
        } else {
            UserNamespaceRelation::Elsewhere
        }
    }

    /// Record that the namespace `process`'s clone just made (the last of its
    /// chain, [`Self::enter_new`]) was made by a task whose effective user ID
    /// was not its real one, read as the overflow ID, or could not be read:
    /// its owner may not be the real user ID every guest task has. Nothing
    /// for a process the record has below no namespace of its own.
    pub fn note_owner_may_differ(&mut self, process: DetPid) {
        if let Some(&newest) = self.chain(process).last() {
            self.owners_may_differ.insert(newest);
        }
    }

    /// Whether the namespace whose owner Linux's rule asks about for a
    /// `reader` and a `task` below it (the one just below the reader's on the
    /// task's chain) may be owned by an effective user ID other than the real
    /// user ID every guest task has ([`Self::note_owner_may_differ`]). False
    /// where the task is not below the reader, whose answer turns on no
    /// owner.
    pub fn owner_may_differ_below(&self, reader: DetPid, task: DetPid) -> bool {
        let reader = self.chain(reader);
        let task = self.chain(task);
        task.len() > reader.len()
            && task.starts_with(reader)
            && self.owners_may_differ.contains(&task[reader.len()])
    }

    fn chain(&self, process: DetPid) -> &[u64] {
        self.chains.get(&process).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(raw: i32) -> DetPid {
        DetPid::from_raw(raw)
    }

    #[test]
    fn a_process_lives_where_its_creator_does_unless_its_clone_made_a_namespace() {
        use UserNamespaceRelation::*;
        let mut record = UserNamespaces::default();
        record.inherit(pid(3), None);
        // A plain fork: the creator's namespace.
        record.inherit(pid(4), Some(pid(3)));
        assert_eq!(record.relation(pid(3), pid(4)), Same);
        // A clone(CLONE_NEWUSER): a namespace below the creator's.
        record.inherit(pid(5), Some(pid(3)));
        record.enter_new(pid(5));
        assert_eq!(record.relation(pid(3), pid(5)), Below);
        assert_eq!(record.relation(pid(4), pid(5)), Below);
        assert_eq!(record.relation(pid(5), pid(3)), Elsewhere);
        // Its child, forked plainly, shares it; one made with
        // CLONE_NEWUSER is below both.
        record.inherit(pid(6), Some(pid(5)));
        assert_eq!(record.relation(pid(5), pid(6)), Same);
        record.inherit(pid(7), Some(pid(6)));
        record.enter_new(pid(7));
        assert_eq!(record.relation(pid(3), pid(7)), Below);
        assert_eq!(record.relation(pid(6), pid(7)), Below);
        // Two namespaces made from the same one are apart.
        record.inherit(pid(8), Some(pid(3)));
        record.enter_new(pid(8));
        assert_eq!(record.relation(pid(5), pid(8)), Elsewhere);
        assert_eq!(record.relation(pid(8), pid(7)), Elsewhere);
        // A process the record never saw made lives in the container's.
        assert_eq!(record.relation(pid(3), pid(99)), Same);
        assert_eq!(record.relation(pid(99), pid(5)), Below);
    }

    #[test]
    fn a_reused_number_takes_its_new_creators_namespace() {
        use UserNamespaceRelation::*;
        let mut record = UserNamespaces::default();
        record.inherit(pid(3), None);
        record.inherit(pid(5), Some(pid(3)));
        record.enter_new(pid(5));
        // Process 5 is reaped and a plain fork of 3 takes its number.
        record.inherit(pid(5), Some(pid(3)));
        assert_eq!(record.relation(pid(3), pid(5)), Same);
        // A namespace made later gets a new name: it is not the old one.
        record.inherit(pid(6), Some(pid(3)));
        record.enter_new(pid(6));
        record.inherit(pid(9), Some(pid(3)));
        record.enter_new(pid(9));
        assert_eq!(record.relation(pid(6), pid(9)), Elsewhere);
    }

    #[test]
    fn an_owner_note_is_kept_for_the_one_namespace_linux_asks_about() {
        let mut record = UserNamespaces::default();
        record.inherit(pid(3), None);
        // 5 makes a namespace below the container's; its creator's owner is
        // noted as one that may differ.
        record.inherit(pid(5), Some(pid(3)));
        record.enter_new(pid(5));
        record.note_owner_may_differ(pid(5));
        // 7, below 5's, was made by a creator whose owner is not noted.
        record.inherit(pid(7), Some(pid(5)));
        record.enter_new(pid(7));
        // From 3, Linux asks about 5's namespace for both 5 and 7.
        assert!(record.owner_may_differ_below(pid(3), pid(5)));
        assert!(record.owner_may_differ_below(pid(3), pid(7)));
        // From 5, it asks about 7's, whose owner is not noted.
        assert!(!record.owner_may_differ_below(pid(5), pid(7)));
        // A namespace made elsewhere is not touched by 5's note.
        record.inherit(pid(8), Some(pid(3)));
        record.enter_new(pid(8));
        assert!(!record.owner_may_differ_below(pid(3), pid(8)));
        // Not below: no owner is asked about.
        assert!(!record.owner_may_differ_below(pid(5), pid(3)));
        assert!(!record.owner_may_differ_below(pid(5), pid(5)));
        // A process below no namespace of its own notes nothing.
        record.note_owner_may_differ(pid(3));
        assert!(!record.owner_may_differ_below(pid(3), pid(8)));
    }
}
