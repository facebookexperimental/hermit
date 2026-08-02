/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! This module just aggregates submodules.

mod files;
pub mod helpers;
mod io;
mod memory;
mod misc;
mod namespace;
mod signal;
pub(crate) mod socket_timestamp_ioctl;
mod sysinfo;
mod threads;
mod time;

use crate::consts::DET_SPECIAL_INODE_OFFSET;
use crate::types::DetInode;
use crate::types::RawFd;

/// Give inherited standard streams identities that do not depend on backend
/// loader activity observed before the guest reaches its entry point.
fn deterministic_stdio_inode(fd: RawFd) -> Option<DetInode> {
    (libc::STDIN_FILENO..=libc::STDERR_FILENO)
        .contains(&fd)
        .then_some(DET_SPECIAL_INODE_OFFSET + fd as DetInode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_inode_namespace_is_fixed() {
        assert_eq!(deterministic_stdio_inode(libc::STDIN_FILENO), Some(1000));
        assert_eq!(deterministic_stdio_inode(libc::STDOUT_FILENO), Some(1001));
        assert_eq!(deterministic_stdio_inode(libc::STDERR_FILENO), Some(1002));
        assert_eq!(deterministic_stdio_inode(3), None);
    }
}
