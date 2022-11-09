/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic file descriptor

use std::fmt;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;

use nix::fcntl::OFlag;
use serde::Deserialize;
use serde::Serialize;

use crate::resources::ResourceID;
use crate::stat::*;
use crate::types::RawFd;
use crate::types::*;

/// file descriptor type
#[derive(PartialEq, Eq, Debug, Clone, Copy, Hash, Serialize, Deserialize)]
pub enum FdType {
    /// Regular fd, such as from openat
    Regular,
    /// signalfd
    Signalfd,
    /// eventfd
    Eventfd,
    /// timerfd
    Timerfd,
    /// socket fd
    Socket,
    /// pipe fd
    Pipe,
    /// memfd
    Memfd,
    /// pidfd
    Pidfd,
    /// userfaultfd
    Userfaultfd,
    ///
    Rng,
}

impl Default for FdType {
    fn default() -> Self {
        FdType::Regular
    }
}

/// Deterministic file descriptor
///
/// Notice `statbuf` can be cached here, this is because
/// `stat` is valid as long as fd stays open.
#[derive(Default, Debug, Eq, Clone, Serialize, Deserialize)]
pub struct DetFd {
    /// underlying file descriptor
    pub(crate) fd: RawFd,
    /// fd type
    pub(crate) ty: FdType,
    /// flags with fd
    pub(crate) flags: i32,
    /// File path associated with fd.
    /// This cannot be relied upon. Special devices won't have it, for example.
    pub(crate) path: Option<PathBuf>,
    /// Cached det/virtual inode.
    /// This cannot be relied upon. Special devices won't have it, for example.
    /// However if `ty` indicates a `Regular` file, then there should reliably be an inode.
    pub(crate) inode: Option<DetInode>,
    /// inode is dirty
    pub(crate) dirty: bool,

    /// Irrespective of whether the file descriptor is marked logically blocking by the
    /// user, this tracks whether Detcore has converted the fd to nonblocking for its own
    /// purposes.
    pub(crate) physically_nonblocking: bool,

    /// cached statbuf
    ///
    /// This is the RAW stat from the file system, NOT determinized.
    ///
    /// Some of these fields will change at runtime. But the following fields will
    /// be constant when `virtualize_metadata` is on, over the life of the DetFd:
    ///  - dev, rdev, blksize
    ///
    /// This should always be `Some` for regular files, as we eagerly populate it.
    pub(crate) stat: Option<DetStat>,
    /// resource
    pub(crate) resource: Option<ResourceID>,
}

impl PartialEq for DetFd {
    fn eq(&self, other: &Self) -> bool {
        self.fd == other.fd
    }
}

impl Hash for DetFd {
    // fd is owned by process and is unique per process/thread
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fd.hash(state);
    }
}

/// If the flags specify O_NONBLOCK.
fn oflags_nonblocking(flags: i32) -> bool {
    let o_nonblock = OFlag::O_NONBLOCK.bits();
    flags & o_nonblock == o_nonblock
}

impl DetFd {
    /// create a new detfd from rawfd
    pub fn new(fd: RawFd, flags: OFlag, ty: FdType) -> Self {
        let bits = flags.bits();
        DetFd {
            fd,
            ty,
            flags: bits,
            path: None,
            inode: None,
            dirty: false,
            stat: None,
            resource: None,
            // By default, we assume it matches the flags we were given:
            physically_nonblocking: oflags_nonblocking(bits),
        }
    }
    /// update fd
    pub fn with_fd(mut self, fd: RawFd) -> Self {
        self.fd = fd;
        self
    }
    /// change fd type
    pub fn with_type(mut self, ty: FdType) -> Self {
        self.ty = ty;
        self
    }
    /// change flags (OFlag)
    pub fn with_flags(mut self, flags: OFlag) -> Self {
        self.flags = flags.bits();
        self
    }
    /// set path associated with `fd`
    pub fn with_path<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.path = Some(PathBuf::from(path.as_ref()));
        self
    }
    /// set virtual inode
    pub fn with_inode(mut self, inode: DetInode) -> Self {
        self.inode = Some(inode);
        self
    }
    /// set dirty flag
    pub fn with_dirty(mut self, dirty: bool) -> Self {
        self.dirty = dirty;
        self
    }
    /// update statbuf
    pub fn with_stat<S: Into<Option<DetStat>>>(mut self, stat: S) -> Self {
        self.stat = stat.into();
        self
    }
    /// set resource id
    pub fn with_resource<S: Into<Option<ResourceID>>>(mut self, resource: S) -> Self {
        self.resource = resource.into();
        self
    }

    /// If fd is non blocking
    pub fn is_nonblocking(&self) -> bool {
        oflags_nonblocking(self.flags)
    }
}

impl fmt::Display for DetFd {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "DetFd({})", self.fd)
    }
}
