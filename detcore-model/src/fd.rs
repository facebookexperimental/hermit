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
}
