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

/// Re-exported so `procfs` can report the SAME ceiling that `F_SETPIPE_SZ`
/// enforces. Two constants that must agree are one constant.
pub(crate) use files::DETERMINISTIC_PIPE_CAPACITY_BYTES;
mod io;
mod memory;
mod misc;
mod namespace;
pub(crate) mod robust_list;
mod signal;
pub(crate) mod socket_timestamp_ioctl;
mod sysinfo;
mod threads;
pub(crate) mod time;

use crate::consts::DET_SPECIAL_INODE_OFFSET;
use crate::resources::Device;
use crate::resources::ResourceID;
use crate::types::DetInode;
use crate::types::RawFd;

/// Give inherited standard streams identities that do not depend on backend
/// loader activity observed before the guest reaches its entry point.
fn deterministic_stdio_inode(fd: RawFd) -> Option<DetInode> {
    (libc::STDIN_FILENO..=libc::STDERR_FILENO)
        .contains(&fd)
        .then_some(DetInode::mint(
            DET_SPECIAL_INODE_OFFSET.as_raw() + fd as u64,
        ))
}

/// Preserve the existing inherited-stdio slot identities, but do not assign one
/// to an ordinary file or pipe that has replaced that slot. Aliases above fd 2
/// retain their existing pooled identity; extending the fixed namespace to
/// aliases requires consistent path-stat behavior too.
pub(crate) fn deterministic_stdio_inode_for_resource(
    fd: RawFd,
    resource: Option<ResourceID>,
) -> Option<DetInode> {
    match resource {
        Some(ResourceID::Device(
            Device::ContainerStdin | Device::ContainerStdout | Device::ContainerStderr,
        )) => deterministic_stdio_inode(fd),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_inode_namespace_is_fixed() {
        assert_eq!(
            deterministic_stdio_inode(libc::STDIN_FILENO),
            Some(DetInode::mint(1000))
        );
        assert_eq!(
            deterministic_stdio_inode(libc::STDOUT_FILENO),
            Some(DetInode::mint(1001))
        );
        assert_eq!(
            deterministic_stdio_inode(libc::STDERR_FILENO),
            Some(DetInode::mint(1002))
        );
        assert_eq!(deterministic_stdio_inode(3), None);

        for resource in [
            ResourceID::Device(Device::ContainerStdin),
            ResourceID::Device(Device::ContainerStdout),
            ResourceID::Device(Device::ContainerStderr),
        ] {
            for fd in 0..=2 {
                assert_eq!(
                    deterministic_stdio_inode_for_resource(fd, Some(resource.clone())),
                    Some(DetInode::mint(1000 + fd as u64))
                );
            }
            assert_eq!(
                deterministic_stdio_inode_for_resource(3, Some(resource)),
                None
            );
        }
        for fd in 0..=2 {
            assert_eq!(deterministic_stdio_inode_for_resource(fd, None), None);
            assert_eq!(
                deterministic_stdio_inode_for_resource(
                    fd,
                    Some(ResourceID::FileContents(DetInode::mint(1000))),
                ),
                None
            );
        }
    }
}
