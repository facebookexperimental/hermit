/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use serde::Deserialize;
use serde::Serialize;

use crate::pid::DetTid;

/// For now we use the definiton of `RawFd` from `std::os`.
// (Workaround: reexporting this type directly triggers a rust-anlazer glitch.)
pub type RawFd = std::os::unix::io::RawFd;

/// Nondeterministic "physical" inode
pub type RawInode = u64;

/// Deterministic "virtual" inode.
pub type DetInode = RawInode;

/// Identity of a Linux descriptor table (`files_struct`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FilesId {
    creator: DetTid,
    generation: u64,
}

impl FilesId {
    /// Create the first descriptor table owned by a task.
    pub const fn initial(creator: DetTid) -> Self {
        Self {
            creator,
            generation: 0,
        }
    }

    /// Create a copied descriptor table for a newly created task.
    pub const fn forked(creator: DetTid) -> Self {
        Self::initial(creator)
    }

    /// Create the replacement table installed by exec.
    pub fn for_exec(self, creator: DetTid) -> Self {
        let generation = if self.creator == creator {
            self.generation + 1
        } else {
            0
        };
        Self {
            creator,
            generation,
        }
    }
}

/// Identity of one numeric descriptor slot within a descriptor table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FdSlot {
    /// Descriptor table containing the slot.
    pub files: FilesId,
    /// Numeric descriptor within the table.
    pub fd: RawFd,
}

/// Identity of a Linux open file description (`struct file`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize
)]
pub struct OpenFileId {
    creator: DetTid,
    sequence: u64,
}

impl OpenFileId {
    /// Create an identity from the task that observed the open and its local sequence.
    pub const fn new(creator: DetTid, sequence: u64) -> Self {
        Self { creator, sequence }
    }

    // TODO-HUMAN-REVIEW(PR-886): Review stable socket-cookie identity encoding.
    /// Encode the ordinary per-task open sequence as a deterministic socket cookie.
    ///
    /// Linux promises that live socket cookies are unique and that descriptor aliases
    /// for one open file description share a cookie. Detcore's virtual task IDs and
    /// open-file sequences provide those same properties for realistic descriptor
    /// counts while avoiding the kernel's host-global cookie allocator.
    pub fn deterministic_socket_cookie(self) -> u64 {
        let creator = self.creator.as_raw() as u32 as u64;
        (creator << 32) | (self.sequence & u32::MAX as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_socket_cookies_track_open_file_identity() {
        let first = OpenFileId::new(DetTid::from_raw(3), 7);
        let alias = first;
        let next = OpenFileId::new(DetTid::from_raw(3), 8);
        let other_task = OpenFileId::new(DetTid::from_raw(4), 7);

        assert_ne!(first.deterministic_socket_cookie(), 0);
        assert_eq!(
            first.deterministic_socket_cookie(),
            alias.deterministic_socket_cookie()
        );
        assert_ne!(
            first.deterministic_socket_cookie(),
            next.deterministic_socket_cookie()
        );
        assert_ne!(
            first.deterministic_socket_cookie(),
            other_task.deterministic_socket_cookie()
        );
    }
}
