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
use crate::procfs::ProcfsSnapshotContext;
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
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1096): Review canonical random-device cursor sharing.
    /// Cursor into Hermit's backend-independent random-device byte stream.
    #[serde(default)]
    random_device_offset: u64,
    /// resource
    resource: Option<ResourceID>,
    /// Deterministic snapshot state for selected procfs files.
    procfs: Option<ProcfsFile>,
    /// Logical timestamp of the last packet delivered through this socket.
    socket_receive_timestamp: Option<LogicalTime>,
    /// True when this open file is an `AF_NETLINK`/`NETLINK_SOCK_DIAG` socket,
    /// whose binary dump replies carry host-assigned socket inode numbers that
    /// must be determinized (see `crate::sock_diag`).
    sock_diag: bool,
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
                random_device_offset: 0,
                resource: None,
                procfs: None,
                socket_receive_timestamp: None,
                sock_diag: false,
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

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1096): Review canonical random-device cursor sharing.
    /// Return the cursor shared by aliases of this random-device open file.
    pub(crate) fn random_device_offset(&self) -> u64 {
        self.description().random_device_offset
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1096): Review canonical random-device cursor sharing.
    /// Advance the cursor shared by aliases of this random-device open file.
    pub(crate) fn advance_random_device_offset(&self, count: usize) {
        let mut description = self.description();
        description.random_device_offset = description
            .random_device_offset
            .saturating_add(count as u64);
    }

    /// Path used to open this file description, when it was observable.
    pub(crate) fn path(&self) -> Option<PathBuf> {
        self.description().path.clone()
    }

    /// Record the resolved path used to open this file description.
    pub(crate) fn set_path<P: AsRef<Path>>(&self, path: P) {
        self.description().path = Some(path.as_ref().to_path_buf());
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

    /// Whether this procfs snapshot consumes deterministic random bytes.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-955): Review deterministic kernel UUID generation.
    pub(crate) fn procfs_needs_random_uuid(&self) -> bool {
        self.description()
            .procfs
            .as_ref()
            .is_some_and(ProcfsFile::needs_random_uuid)
    }

    /// Initialize the deterministic snapshot shared by all aliases.
    // TODO-HUMAN-REVIEW(PR-723): Review procfs snapshot identity parameters.
    // TODO-HUMAN-REVIEW(PR-955): Review deterministic UUID snapshot input.
    pub(crate) fn initialize_procfs(&self, contents: Vec<u8>, context: ProcfsSnapshotContext) {
        self.description()
            .procfs
            .as_mut()
            .expect("procfs fd disappeared while taking its snapshot")
            .initialize(contents, context);
    }

    /// Read from the deterministic procfs snapshot at its shared offset.
    pub(crate) fn take_procfs(&self, maximum: usize) -> Option<Vec<u8>> {
        self.description()
            .procfs
            .as_mut()
            .and_then(|procfs| procfs.take(maximum))
    }

    /// Read a deterministic procfs snapshot without changing its shared cursor.
    pub(crate) fn take_procfs_at(&self, offset: usize, maximum: usize) -> Option<Vec<u8>> {
        self.description()
            .procfs
            .as_ref()
            .and_then(|procfs| procfs.take_at(offset, maximum))
    }

    /// Return the shared procfs cursor and initialized snapshot length.
    pub(crate) fn procfs_position(&self) -> Option<(usize, Option<usize>)> {
        self.description().procfs.as_ref().map(ProcfsFile::position)
    }

    pub(crate) fn procfs_target_fd(&self) -> Option<i32> {
        self.description()
            .procfs
            .as_ref()
            .and_then(ProcfsFile::target_fd)
    }

    /// Update the cursor shared by every alias of a procfs open file.
    pub(crate) fn set_procfs_offset(&self, offset: usize) {
        self.description()
            .procfs
            .as_mut()
            .expect("procfs fd disappeared while updating its offset")
            .set_offset(offset);
    }

    /// Cached stat data attached to the backing object.
    pub fn stat(&self) -> Option<DetStat> {
        self.description().stat
    }

    /// Whether Detcore has made the open file description physically nonblocking.
    pub fn physically_nonblocking(&self) -> bool {
        self.description().physically_nonblocking
    }

    pub(crate) fn status_flags(&self) -> i32 {
        self.description().status_flags
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

    // TODO-HUMAN-REVIEW(PR-912): Review open-file sharing of socket receive timestamps.
    /// Record the logical time at which a socket delivered its most recent packet.
    pub(crate) fn set_socket_receive_timestamp(&self, timestamp: LogicalTime) {
        self.description().socket_receive_timestamp = Some(timestamp);
    }

    /// Return the last receive timestamp shared by every alias of this socket.
    pub(crate) fn socket_receive_timestamp(&self) -> Option<LogicalTime> {
        self.description().socket_receive_timestamp
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1064)
    /// Mark this open file as a `NETLINK_SOCK_DIAG` socket. Shared across every
    /// dup/fork alias of the same open file description.
    pub(crate) fn set_sock_diag(&self) {
        self.description().sock_diag = true;
    }

    // TODO-HUMAN-REVIEW(PR-1064)
    /// Whether this open file is a `NETLINK_SOCK_DIAG` socket whose dump replies
    /// must have their socket inode numbers determinized.
    pub(crate) fn is_sock_diag(&self) -> bool {
        self.description().sock_diag
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

        let timestamp = LogicalTime::from_nanos(2_345_678_901);
        original.set_socket_receive_timestamp(timestamp);
        assert_eq!(duplicate.socket_receive_timestamp(), Some(timestamp));

        assert!(!original.is_sock_diag());
        original.set_sock_diag();
        assert!(
            duplicate.is_sock_diag(),
            "sock_diag marking through one alias must be visible through every alias"
        );
    }

    #[test]
    fn toggling_logical_nonblocking_preserves_physical() {
        // Models FIONBIO on an fd that Detcore forced physically nonblocking for
        // the scheduler: the guest-visible flag changes, but the physical state
        // must survive.
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

        fd.set_logical_nonblocking(true);
        assert!(fd.is_nonblocking());
        assert!(
            fd.physically_nonblocking(),
            "setting the logical flag must not discard physical tracking"
        );

        // The both-flags setter still tracks physical alongside logical.
        fd.set_nonblocking(false);
        assert!(!fd.is_nonblocking());
        assert!(!fd.physically_nonblocking());
    }

    #[test]
    fn procfs_offsets_are_shared_by_dup_aliases() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(
            3,
            OFlag::empty(),
            FdType::Regular,
            OpenFileId::new(owner, 0),
        );
        original.set_procfs(ProcfsFile::from_path(Path::new("/proc/sys/fs/file-nr")).unwrap());
        original.initialize_procfs(
            b"15\t0\t1000\n".to_vec(),
            ProcfsSnapshotContext {
                virtual_pid: 1,
                ..ProcfsSnapshotContext::default()
            },
        );
        let duplicate = original.clone().with_fd(4);

        assert_eq!(original.take_procfs(2).unwrap(), b"0\t");
        assert_eq!(duplicate.procfs_position().unwrap().0, 2);
        assert_eq!(duplicate.take_procfs_at(4, 1).unwrap(), b"9");
        assert_eq!(original.procfs_position().unwrap().0, 2);

        duplicate.set_procfs_offset(0);
        assert_eq!(
            original.take_procfs(128).unwrap(),
            b"0\t0\t9223372036854775807\n"
        );
    }

    #[test]
    fn random_device_offsets_are_shared_by_dup_aliases() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(3, OFlag::empty(), FdType::Rng, OpenFileId::new(owner, 0));
        let duplicate = original.clone().with_fd(4);

        assert_eq!(original.random_device_offset(), 0);
        duplicate.advance_random_device_offset(50);
        assert_eq!(original.random_device_offset(), 50);
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
