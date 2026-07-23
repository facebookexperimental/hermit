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
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use nix::fcntl::OFlag;
use serde::Deserialize;
use serde::Serialize;

use crate::procfs::ProcfsFile;
use crate::resources::ResourceID;
use crate::stat::*;
use crate::types::RawFd;
use crate::types::*;

/// file descriptor type
#[derive(
    PartialEq,
    Eq,
    Debug,
    Default,
    Clone,
    Copy,
    Hash,
    Serialize,
    Deserialize
)]
pub enum FdType {
    /// Regular fd, such as from openat
    #[default]
    Regular,
    /// signalfd
    Signalfd,
    /// eventfd
    Eventfd,
    /// timerfd
    Timerfd,
    /// inotify instance
    Inotify,
    /// epoll instance (from epoll_create/epoll_create1)
    Epoll,
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
    /// Random-number generator device
    Rng,
}

/// Deterministic file descriptor
///
/// Notice `statbuf` can be cached here, this is because
/// `stat` is valid as long as fd stays open.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetFd {
    /// underlying file descriptor
    pub(crate) fd: RawFd,
    /// Per-slot descriptor flags, currently only `O_CLOEXEC`.
    fd_flags: i32,
    /// State shared by every descriptor referring to the same Linux `struct file`.
    open_file: Arc<Mutex<OpenFileDescription>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenFileDescription {
    id: OpenFileId,
    /// fd type
    ty: FdType,
    /// File status flags shared by dup and fork aliases.
    status_flags: i32,
    /// File path associated with fd.
    /// This cannot be relied upon. Special devices won't have it, for example.
    path: Option<PathBuf>,
    /// Cached det/virtual inode.
    /// This cannot be relied upon. Special devices won't have it, for example.
    /// However if `ty` indicates a `Regular` file, then there should reliably be an inode.
    inode: Option<DetInode>,
    /// inode is dirty
    dirty: bool,

    /// Irrespective of whether the file descriptor is marked logically blocking by the
    /// user, this tracks whether Detcore has converted the fd to nonblocking for its own
    /// purposes.
    physically_nonblocking: bool,

    /// cached statbuf
    ///
    /// This is the RAW stat from the file system, NOT determinized.
    ///
    /// Some of these fields will change at runtime. But the following fields will
    /// be constant when `virtualize_metadata` is on, over the life of the DetFd:
    ///  - dev, rdev, blksize
    ///
    /// This should always be `Some` for regular files, as we eagerly populate it.
    stat: Option<DetStat>,
    /// resource
    resource: Option<ResourceID>,
    /// Deterministic snapshot state for selected procfs files.
    procfs: Option<ProcfsFile>,
}

impl PartialEq for DetFd {
    fn eq(&self, other: &Self) -> bool {
        self.fd == other.fd
    }
}

impl Eq for DetFd {}

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
    pub fn new(fd: RawFd, flags: OFlag, ty: FdType, id: OpenFileId) -> Self {
        let bits = flags.bits();
        DetFd {
            fd,
            fd_flags: bits & OFlag::O_CLOEXEC.bits(),
            open_file: Arc::new(Mutex::new(OpenFileDescription {
                id,
                ty,
                status_flags: bits & !OFlag::O_CLOEXEC.bits(),
                path: None,
                inode: None,
                dirty: false,
                stat: None,
                resource: None,
                procfs: None,
                // By default, we assume it matches the flags we were given:
                physically_nonblocking: oflags_nonblocking(bits),
            })),
        }
    }

    fn description(&self) -> MutexGuard<'_, OpenFileDescription> {
        self.open_file.lock().expect("open file mutex poisoned")
    }

    /// update fd
    pub fn with_fd(mut self, fd: RawFd) -> Self {
        self.fd = fd;
        self
    }
    /// change fd type
    pub fn with_type(self, ty: FdType) -> Self {
        self.description().ty = ty;
        self
    }
    /// Set per-slot descriptor flags on a newly duplicated fd.
    pub fn with_fd_flags(mut self, flags: OFlag) -> Self {
        self.fd_flags = flags.bits() & OFlag::O_CLOEXEC.bits();
        self
    }
    /// set path associated with `fd`
    pub fn with_path<P: AsRef<Path>>(self, path: P) -> Self {
        self.description().path = Some(PathBuf::from(path.as_ref()));
        self
    }
    /// set virtual inode
    pub fn with_inode(self, inode: DetInode) -> Self {
        self.description().inode = Some(inode);
        self
    }
    /// set dirty flag
    pub fn with_dirty(self, dirty: bool) -> Self {
        self.description().dirty = dirty;
        self
    }
    /// update statbuf
    pub fn with_stat<S: Into<Option<DetStat>>>(self, stat: S) -> Self {
        self.description().stat = stat.into();
        self
    }
    /// set resource id
    pub fn with_resource<S: Into<Option<ResourceID>>>(self, resource: S) -> Self {
        self.description().resource = resource.into();
        self
    }

    /// If fd is non blocking
    pub fn is_nonblocking(&self) -> bool {
        oflags_nonblocking(self.description().status_flags)
    }

    /// Whether close-on-exec is set for this descriptor slot.
    pub fn is_cloexec(&self) -> bool {
        self.fd_flags & OFlag::O_CLOEXEC.bits() != 0
    }

    /// Update close-on-exec for this descriptor slot only.
    pub fn set_cloexec(&mut self, enabled: bool) {
        self.fd_flags = if enabled { OFlag::O_CLOEXEC.bits() } else { 0 };
    }

    /// Update both the logical (guest-visible) and physical (scheduler)
    /// nonblocking status for every alias of this open file description. Use this
    /// only when the physical fd genuinely tracks the guest's request; when
    /// Detcore forces the fd physically nonblocking for the scheduler, update the
    /// logical view alone via [`Self::set_logical_nonblocking`].
    pub fn set_nonblocking(&self, enabled: bool) {
        let mut description = self.description();
        if enabled {
            description.status_flags |= OFlag::O_NONBLOCK.bits();
        } else {
            description.status_flags &= !OFlag::O_NONBLOCK.bits();
        }
        description.physically_nonblocking = enabled;
    }

    /// Update only the logical (guest-visible) O_NONBLOCK status flag, leaving
    /// the physical (scheduler) nonblocking state untouched. This lets a guest
    /// clear O_NONBLOCK while Detcore keeps the fd physically nonblocking, which
    /// the scheduler relies on for nonblockize-and-retry.
    pub fn set_logical_nonblocking(&self, enabled: bool) {
        let mut description = self.description();
        if enabled {
            description.status_flags |= OFlag::O_NONBLOCK.bits();
        } else {
            description.status_flags &= !OFlag::O_NONBLOCK.bits();
        }
    }

    /// Stable identity shared by dup and fork aliases.
    pub fn open_file_id(&self) -> OpenFileId {
        self.description().id
    }

    /// Number of modeled descriptor slots that retain this open file description.
    pub(crate) fn open_file_alias_count(&self) -> usize {
        Arc::strong_count(&self.open_file)
    }

    /// File type attached to the open file description.
    pub fn ty(&self) -> FdType {
        self.description().ty
    }

    /// Resource attached to the open file description.
    pub fn resource(&self) -> Option<ResourceID> {
        self.description().resource.clone()
    }

    /// Attach deterministic procfs snapshot state to this open file description.
    pub(crate) fn set_procfs(&self, procfs: ProcfsFile) {
        self.description().procfs = Some(procfs);
    }

    /// Whether this procfs open file description still needs its initial snapshot.
    pub(crate) fn procfs_needs_snapshot(&self) -> bool {
        self.description()
            .procfs
            .as_ref()
            .is_some_and(ProcfsFile::needs_snapshot)
    }

    /// Initialize the deterministic snapshot shared by all aliases.
    pub(crate) fn initialize_procfs(&self, contents: Vec<u8>) {
        self.description()
            .procfs
            .as_mut()
            .expect("procfs fd disappeared while taking its snapshot")
            .initialize(contents);
    }

    /// Read from the deterministic procfs snapshot at its shared offset.
    pub(crate) fn take_procfs(&self, maximum: usize) -> Option<Vec<u8>> {
        self.description()
            .procfs
            .as_mut()
            .and_then(|procfs| procfs.take(maximum))
    }

    /// Cached stat data attached to the backing object.
    pub fn stat(&self) -> Option<DetStat> {
        self.description().stat
    }

    /// Whether Detcore has made the open file description physically nonblocking.
    pub fn physically_nonblocking(&self) -> bool {
        self.description().physically_nonblocking
    }

    /// Mark every alias of this open file description physically nonblocking.
    pub fn set_physically_nonblocking(&self) {
        self.description().physically_nonblocking = true;
    }

    /// Update file status flags for every alias of this open file description.
    pub fn set_status_flags(&self, flags: i32) {
        let mut description = self.description();
        description.status_flags = flags & !OFlag::O_CLOEXEC.bits();
        description.physically_nonblocking = oflags_nonblocking(flags);
    }
}

impl fmt::Display for DetFd {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "DetFd({})", self.fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dup_shares_open_file_state_but_not_slot_flags() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(
            3,
            OFlag::O_NONBLOCK,
            FdType::Socket,
            OpenFileId::new(owner, 0),
        );
        let duplicate = original.clone().with_fd(4).with_fd_flags(OFlag::O_CLOEXEC);

        assert_eq!(original.open_file_id(), duplicate.open_file_id());
        assert!(
            !original.is_cloexec(),
            "dup must not alter the source fd flags"
        );
        assert!(
            duplicate.is_cloexec(),
            "dup3(O_CLOEXEC) applies to the new slot"
        );
        assert!(
            duplicate.is_nonblocking(),
            "dup must preserve shared status flags"
        );

        duplicate.set_status_flags(OFlag::empty().bits());
        assert!(
            !original.is_nonblocking(),
            "status flag changes through one alias must be visible through every alias"
        );
    }

    #[test]
    fn clearing_logical_nonblocking_preserves_physical() {
        // Models a FIONBIO(0) clear on an fd that Detcore forced physically
        // nonblocking for the scheduler: the guest-visible flag clears, but the
        // physical (scheduler) state must survive.
        let owner = DetTid::from_raw(10);
        let fd = DetFd::new(3, OFlag::empty(), FdType::Socket, OpenFileId::new(owner, 0));
        fd.set_physically_nonblocking();
        fd.set_logical_nonblocking(true);
        assert!(fd.is_nonblocking());
        assert!(fd.physically_nonblocking());

        fd.set_logical_nonblocking(false);
        assert!(
            !fd.is_nonblocking(),
            "the guest must observe O_NONBLOCK cleared"
        );
        assert!(
            fd.physically_nonblocking(),
            "the scheduler's physical nonblocking state must be preserved"
        );

        // The both-flags setter still tracks physical alongside logical.
        fd.set_nonblocking(false);
        assert!(!fd.is_nonblocking());
        assert!(!fd.physically_nonblocking());
    }

    #[test]
    fn separate_opens_have_distinct_identity() {
        let owner = DetTid::from_raw(10);
        let first = DetFd::new(
            3,
            OFlag::empty(),
            FdType::Regular,
            OpenFileId::new(owner, 0),
        );
        let second = DetFd::new(
            4,
            OFlag::empty(),
            FdType::Regular,
            OpenFileId::new(owner, 1),
        );

        assert_ne!(first.open_file_id(), second.open_file_id());
    }
}
