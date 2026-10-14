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
use std::sync::OnceLock;
use std::time::Duration;

use nix::fcntl::OFlag;
use reverie::syscalls::Errno;
use serde::Deserialize;
use serde::Serialize;

use crate::dirents::DirEntry;
use crate::dirents::DirectoryStream;
use crate::procfs::ProcfsFile;
use crate::procfs::ProcfsSnapshotContext;
use crate::procfs::TimerSlackReadPreview;
use crate::procfs_inode::DirectoryInodes;
use crate::resources::ResourceID;
use crate::stat::*;
use crate::tool_global::SnapshotMark;
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

/// How an active network trace mode classifies a socket's family and type
/// (see `crate::syscalls::network_trace`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum NetworkSocketKind {
    /// Not an IPv4 or IPv6 socket, or no network trace mode was active.
    #[default]
    NotInet,
    /// An IPv4 or IPv6 TCP stream socket. `connect` checks the address
    /// against the socket's family, as Linux does.
    InetStream {
        /// Whether the socket is `AF_INET6`.
        ipv6: bool,
    },
    /// Any other IPv4 or IPv6 socket, such as UDP or raw.
    InetOther,
}

/// Provenance of a terminal inherited at the container's first image.
///
/// The original open-file identity distinguishes aliases from separate opens
/// of the same tty. The raw identity is used only to validate an exec handoff;
/// it is never a guest-visible inode or a scheduler resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InheritedTerminal {
    pub(crate) origin: OpenFileId,
    pub(crate) dev: u64,
    pub(crate) inode: u64,
    pub(crate) rdev: u64,
    pub(crate) resource: Option<ResourceID>,
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
    open_file: Arc<Mutex<OpenFileState>>,
}

/// A socket's receive timeout (`SO_RCVTIMEO`, socket(7)), as the guest set it.
// TODO-HUMAN-REVIEW(PR-3908)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ReceiveTimeout {
    /// No timeout: a blocking call waits indefinitely. Also what a zero
    /// value sets.
    #[default]
    None,
    /// A negative second count, which Linux stores as a zero timeout, so a
    /// blocking call fails at once with `EAGAIN`. `getsockopt` reads it back
    /// as zero, the same as `None`.
    Immediate,
    /// Wait at most this long. Never so long that Linux stores no timeout.
    After(Duration),
    /// Set by a call whose value Detcore could not read.
    Unknown,
}

impl ReceiveTimeout {
    /// The timeout a successful `setsockopt(SO_RCVTIMEO)` with `value` sets.
    /// (Linux has already refused a microsecond count out of range.)
    ///
    /// Linux stores a second count too large to count in scheduler ticks, at
    /// least `LONG_MAX / HZ - 1`, as no timeout (`MAX_SCHEDULE_TIMEOUT`), so
    /// such an accept restarts after a signal like an untimed one. The model
    /// takes the bound for the largest `HZ`, 1000; on a kernel with a smaller
    /// `HZ`, record's check against the kernel refuses a value between the two.
    pub(crate) fn set_by(value: libc::timeval) -> Self {
        const UNBOUNDED_SECONDS: libc::time_t = libc::time_t::MAX / 1000 - 1;
        if value.tv_sec < 0 {
            Self::Immediate
        } else if (value.tv_sec == 0 && value.tv_usec == 0) || value.tv_sec >= UNBOUNDED_SECONDS {
            Self::None
        } else {
            Self::After(
                Duration::from_secs(value.tv_sec as u64)
                    + Duration::from_micros(value.tv_usec.max(0) as u64),
            )
        }
    }
}

/// Where the model of one open file description lives.
///
/// Every `DetFd` method reaches the model through `DetFd::with_description`,
/// so a handle's state can change from `Local` to `Shared` under every alias
/// at once, escaped clones included, because they all hold the same `Arc`.
// The state always lives inside the one `Arc` allocation its aliases share,
// and `Local` is every description today, so boxing it would only add a
// second allocation to every open.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Serialize, Deserialize)]
enum OpenFileState {
    /// Only this process can reach the open file description, so the model
    /// lives here. This is every description today.
    Local(OpenFileDescription),
    /// Several guest processes share the open file description, so the one
    /// canonical model lives in Detcore's global state and this handle holds
    /// none. Each access takes the model through the installed
    /// [`SharedOpenFileChannel`] and publishes it back. Nothing creates this
    /// state yet; the cross-process promotion that does is a later step of
    /// https://github.com/rrnewton/hermit/issues/3520.
    Shared(SharedOpenFile),
}

/// The process-local part of a shared open file description: its identity,
/// and the runtime lock that is never part of the model.
#[derive(Debug, Serialize, Deserialize)]
struct SharedOpenFile {
    id: OpenFileId,
    /// Serves `DetFd::directory_lock` for this process's aliases, as the
    /// description's own lock does for a local description.
    #[serde(skip)]
    directory_lock: Arc<futures::lock::Mutex<()>>,
}

/// The canonical model of an open file description that several guest
/// processes share. Detcore's global state stores it, and a
/// [`SharedOpenFileChannel`] carries it to and from the process that holds
/// the lease. Its contents are private to Detcore.
#[derive(Debug, Serialize, Deserialize)]
pub struct OpenFileModel(OpenFileDescription);

impl OpenFileModel {
    /// The open file description this model describes.
    pub(crate) fn id(&self) -> OpenFileId {
        self.0.id
    }

    #[cfg(test)]
    pub(crate) fn path_for_test(&self) -> Option<PathBuf> {
        self.0.path.clone()
    }
}

/// The canonical model of one shared open file description, taken by one
/// process for the length of one synchronous `DetFd` method call.
#[derive(Debug, Serialize, Deserialize)]
pub struct OpenFileLease {
    /// The open file description the lease covers.
    pub id: OpenFileId,
    /// Names this lease, so that the global state can refuse a late,
    /// duplicate or foreign publish.
    pub sequence: u64,
    /// The canonical model as of the take.
    pub model: OpenFileModel,
}

/// A failure to take or publish a shared open file description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedOpenFileError(pub String);

impl fmt::Display for SharedOpenFileError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SharedOpenFileError {}

/// Synchronous access to the canonical models of shared open file
/// descriptions.
///
/// A `DetFd` method on a shared handle calls `take`, runs its synchronous
/// body on the model, and calls `publish`, all while it holds the handle's
/// local lock and without awaiting. So a lease never outlives one method
/// call. Detcore names no backend here: the backend that runs Detcore inside
/// each guest process installs an implementation with
/// [`install_shared_open_file_channel`].
pub trait SharedOpenFileChannel: Send + Sync {
    /// Takes the canonical model of `id` and starts a lease on it.
    fn take(&self, id: OpenFileId) -> Result<OpenFileLease, SharedOpenFileError>;

    /// Stores `lease.model` as the canonical model of `lease.id` and ends the
    /// lease. A publish that does not name the current lease is refused.
    fn publish(&self, lease: OpenFileLease) -> Result<(), SharedOpenFileError>;
}

static SHARED_OPEN_FILE_CHANNEL: OnceLock<Box<dyn SharedOpenFileChannel>> = OnceLock::new();

/// Installs this process's channel to shared open file descriptions. Only the
/// first channel installed in a process is retained; a later one is returned.
pub fn install_shared_open_file_channel(
    channel: Box<dyn SharedOpenFileChannel>,
) -> Result<(), Box<dyn SharedOpenFileChannel>> {
    SHARED_OPEN_FILE_CHANNEL.set(channel)
}

/// Runs `f` on the canonical model of the shared open file description `id`.
///
/// A missing channel, a failed take or publish, or a lease for another
/// description is an infrastructure failure, not something the guest's
/// syscall can report, so it panics. The model is published even when `f`
/// returns an error value, so the lease always ends; `f` itself decides what
/// it changed. If `f` panics, the lease is never published and the process
/// is lost with it.
fn with_shared_description<R>(id: OpenFileId, f: impl FnOnce(&mut OpenFileDescription) -> R) -> R {
    let channel = SHARED_OPEN_FILE_CHANNEL.get().unwrap_or_else(|| {
        panic!(
            "open file description {id:?} is shared, but no shared open file channel is installed"
        )
    });
    let mut lease = channel.take(id).unwrap_or_else(|error| {
        panic!("taking shared open file description {id:?} failed: {error}")
    });
    assert_eq!(
        lease.id, id,
        "the shared open file channel returned a lease for another description"
    );
    assert_eq!(
        lease.model.0.id, id,
        "the shared open file channel returned the model of another description"
    );
    let result = f(&mut lease.model.0);
    channel.publish(lease).unwrap_or_else(|error| {
        panic!("publishing shared open file description {id:?} failed: {error}")
    });
    result
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenFileDescription {
    id: OpenFileId,
    /// fd type
    ty: FdType,
    /// Read provenance for a terminal inherited from the container launcher.
    /// Guest-created PTYs never acquire this fact merely by becoming stdio.
    #[serde(default)]
    inherited_terminal: Option<InheritedTerminal>,
    /// Process named by a pidfd created through `pidfd_open`.
    ///
    /// This is shared by descriptor aliases just like the kernel pidfd object.
    /// `None` means either that this is not a pidfd or that Detcore did not
    /// observe enough provenance to identify its target.
    #[serde(default)]
    pidfd_target: Option<DetPid>,
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

    /// A FIFO opened by path in `hermit run`, still `Regular` and physically
    /// blocking: its first blocking read or write that finds a guest holding
    /// the other end makes it an internally polled `Pipe`
    /// (`Detcore::fifo_polls_if_a_guest_holds_its_peer`).
    #[serde(default)]
    fifo_awaiting_guest_peer: bool,

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
    /// Signal phase 1's kernel account of the file, recorded at its open by a
    /// process that handles SIGALRM (`sigalrm_phase1::kernel_provenance`).
    #[serde(default)]
    sigalrm_phase1: Option<crate::sigalrm_phase1::Phase1Provenance>,
    /// Sorted directory stream, created by the first `getdents` call that
    /// succeeds on this open file description.
    #[serde(default)]
    directory: Option<DirectoryStream>,
    /// True while `getdents` on this open file reads the host directory one
    /// kernel buffer at a time, because no whole-directory snapshot can stand
    /// in for its position. See `use_host_directory_order`; an `lseek` back
    /// to 0 clears it.
    #[serde(default)]
    directory_host_order: bool,
    /// Held across a whole `getdents` or `lseek` on this open file
    /// description. Reading the host directory takes several injected
    /// syscalls on the shared kernel position, and the kernel's own per-file
    /// position lock covers only one of them; without this, two unsequentialized
    /// threads could interleave their reads and each keep part of the
    /// directory. Sequentialized threads never contend for it.
    ///
    /// This must stay a runtime-agnostic lock that uses no thread-locals.
    /// The DBT backend polls these handlers on DynamoRIO application
    /// threads, outside any Tokio runtime. Tokio's own locks charge each
    /// poll to a per-thread budget kept in a thread-local that has a
    /// destructor, and registering a thread-local destructor on a DynamoRIO
    /// application thread crashes DynamoRIO: the run exits with status 255
    /// and loses its evidence FINAL frames.
    #[serde(skip)]
    directory_lock: Arc<futures::lock::Mutex<()>>,
    /// Logical timestamp of the last packet delivered through this socket.
    socket_receive_timestamp: Option<LogicalTime>,
    /// True when this open file is an `AF_NETLINK`/`NETLINK_SOCK_DIAG` socket,
    /// whose binary dump replies carry host-assigned socket inode numbers that
    /// must be determinized (see `crate::sock_diag`).
    sock_diag: bool,
    /// Whether this open file is a `NETLINK_ROUTE` socket whose link-dump
    /// replies carry live interface counters that must be zeroed (see
    /// `crate::netlink_route`).
    netlink_route: bool,
    /// True when this socket connected to an IPv4 or IPv6 loopback peer.
    #[serde(default)]
    loopback_peer: bool,
    /// True when this socket is a channel of the external network trace, so
    /// its traffic is recorded or replayed (see `crate::syscalls::network_trace`).
    #[serde(default)]
    network_channel: bool,
    /// The socket's `SO_RCVLOWAT` while a network trace mode is active, or
    /// `None` for the default of one byte.
    #[serde(default)]
    network_lowat: Option<usize>,
    /// The socket's family and type, recorded at creation while a network
    /// trace mode is active.
    #[serde(default)]
    network_socket: NetworkSocketKind,
    /// True when this socket is one end of a `socketpair(2)`, whose two
    /// endpoints are therefore both container-internal. See
    /// `syscall_targets_internal_fd`.
    #[serde(default)]
    socketpair_endpoint: bool,
    /// True when `accept(2)` or `accept4(2)` returned this socket. Record and
    /// replay keep such a connection only in the log, and replay stands in an
    /// eventfd for it. See `is_accepted_connection`.
    #[serde(default)]
    accepted_connection: bool,
    /// True when record/replay runs a blocking `accept(2)` on this socket in
    /// the caller's turn, one nonblocking attempt at a time. `socket(2)` sets
    /// it for a stream socket the container created. See
    /// `is_accept_in_turn_listener`.
    // TODO-HUMAN-REVIEW(PR-3908)
    #[serde(default)]
    accept_in_turn_eligible: bool,
    /// True once this socket was named in an `SCM_RIGHTS` message, so a
    /// process outside Detcore's view may hold it. Permanent. See
    /// `is_exported`.
    #[serde(default)]
    exported: bool,
    /// The receive timeout the last successful `setsockopt(SO_RCVTIMEO)` on
    /// this socket set, which a blocking accept in turn waits by. See
    /// `receive_timeout`.
    #[serde(default)]
    receive_timeout: ReceiveTimeout,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// The `flock(2)` mode this open file description currently holds, as the
    /// bare `LOCK_SH`/`LOCK_EX` constant, or `None` when it holds no lock.
    ///
    /// `flock` locks are a property of the open file description, not of the
    /// descriptor slot and not of the inode, which is exactly the granularity
    /// this struct models: `dup`/`fork` aliases share one lock, two separate
    /// `open`s of the same file contend with each other.
    ///
    /// Detcore tracks this only so `handle_flock` can tell a first acquisition
    /// (where a failed `LOCK_NB` probe changes nothing) from a *conversion* of
    /// an already-held lock (where Linux drops the old lock before it can fail).
    /// It is a cache of what Detcore itself granted, never an authority: it is
    /// written only after the kernel reports success.
    #[serde(default)]
    flock_mode: Option<i32>,
    /// Whether `flock_mode` describes the kernel state. Descriptors created by
    /// an intercepted syscall start known-unlocked; descriptors discovered
    /// after entering the guest can already carry a lock.
    #[serde(default)]
    flock_mode_known: bool,
    /// Whether Detcore has EVER known this description's lock state.
    ///
    /// This separates two different unknowns that `flock_mode_known == false`
    /// otherwise collapses. A descriptor Detcore never observed -- stdin,
    /// stdout, stderr, a live-discovered fd -- has no cached claim at all, so
    /// there is nothing about it that can be STALE. A descriptor that was known
    /// and then invalidated by a process copy does have a claim that may now be
    /// wrong. Only the second is a reason to refuse `vfork`.
    #[serde(default)]
    flock_mode_ever_known: bool,
    /// A timerfd's expirations as a function of virtual time, once the guest
    /// has armed or disarmed it through Detcore (`VirtualTimerfd`). `None` for
    /// every other description, and for a timerfd Detcore leaves to the host
    /// (record/replay, or no virtual time).
    #[serde(default)]
    timerfd: Option<VirtualTimerfd>,
}

/// A timerfd's expirations counted on Detcore's virtual clock instead of the
/// host's (timerfd_create(2)).
///
/// The host timerfd behind the descriptor is never armed with the guest's
/// setting, so the host cannot make it readable at a host-timed moment.
/// Instead, when a thread of the process observes the descriptor after the
/// virtual deadline (`crate::syscalls::timerfd`), Detcore counts the
/// expirations here and makes the host descriptor readable before the guest's
/// probe runs; a `read` then returns the count kept here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VirtualTimerfd {
    /// The next expiration not yet counted, or `None` when disarmed.
    pub(crate) deadline: Option<LogicalTime>,
    /// The reload interval in nanoseconds, 0 for a one-shot timer.
    pub(crate) interval_ns: u64,
    /// Expirations counted but not yet read.
    pub(crate) unread: u64,
}

impl VirtualTimerfd {
    /// The state `timerfd_settime` leaves: armed at `deadline` (or disarmed
    /// with `None`), with no expiration pending, as Linux discards the count.
    /// A disarmed timer keeps `interval_ns`, which `timerfd_gettime` reports.
    pub(crate) fn set(deadline: Option<LogicalTime>, interval_ns: u64) -> Self {
        VirtualTimerfd {
            deadline,
            interval_ns,
            unread: 0,
        }
    }

    /// Counts every expiration at or before `now`, as Linux would have by
    /// then, and returns how many were added. A periodic timer moves to its
    /// first expiration after `now`; a one-shot timer disarms.
    pub(crate) fn expire_through(&mut self, now: LogicalTime) -> u64 {
        let Some(deadline) = self.deadline else {
            return 0;
        };
        if now < deadline {
            return 0;
        }
        let elapsed = now.as_nanos() - deadline.as_nanos();
        let added = match elapsed.checked_div(self.interval_ns) {
            // A one-shot timer expires once and disarms.
            None => {
                self.deadline = None;
                1
            }
            Some(periods) => {
                let added = 1 + periods;
                self.deadline = Some(LogicalTime::from_nanos(
                    deadline
                        .as_nanos()
                        .saturating_add(added.saturating_mul(self.interval_ns)),
                ));
                added
            }
        };
        self.unread = self.unread.saturating_add(added);
        added
    }

    /// `timerfd_gettime`'s answer at `now`, after `expire_through(now)`: the
    /// time left to the next expiration (0 when disarmed) and the interval,
    /// both in nanoseconds.
    pub(crate) fn remaining(&self, now: LogicalTime) -> (u64, u64) {
        let left = self.deadline.map_or(0, |deadline| {
            deadline.as_nanos().saturating_sub(now.as_nanos())
        });
        (left, self.interval_ns)
    }
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
            open_file: Arc::new(Mutex::new(OpenFileState::Local(OpenFileDescription {
                id,
                ty,
                pidfd_target: None,
                inherited_terminal: None,
                status_flags: bits & !OFlag::O_CLOEXEC.bits(),
                path: None,
                inode: None,
                dirty: false,
                fifo_awaiting_guest_peer: false,
                stat: None,
                random_device_offset: 0,
                resource: None,
                procfs: None,
                sigalrm_phase1: None,
                directory: None,
                directory_host_order: false,
                directory_lock: Default::default(),
                socket_receive_timestamp: None,
                sock_diag: false,
                netlink_route: false,
                loopback_peer: false,
                network_channel: false,
                network_lowat: None,
                network_socket: NetworkSocketKind::NotInet,
                socketpair_endpoint: false,
                accepted_connection: false,
                accept_in_turn_eligible: false,
                exported: false,
                receive_timeout: ReceiveTimeout::None,
                flock_mode: None,
                flock_mode_known: true,
                flock_mode_ever_known: true,
                timerfd: None,
                // By default, we assume it matches the flags we were given:
                physically_nonblocking: oflags_nonblocking(bits),
            }))),
        }
    }

    /// Runs `f` on this open file description's model while holding the lock
    /// every alias shares. One call is one atomic step on the model, whether
    /// the model is local or shared (see [`OpenFileState`]). `f` must not
    /// access this description through another `DetFd` method or alias, and
    /// must not await.
    fn with_description<R>(&self, f: impl FnOnce(&mut OpenFileDescription) -> R) -> R {
        let mut state = self.open_file.lock().expect("open file mutex poisoned");
        match &mut *state {
            OpenFileState::Local(description) => f(description),
            OpenFileState::Shared(shared) => with_shared_description(shared.id, f),
        }
    }

    /// update fd
    pub fn with_fd(mut self, fd: RawFd) -> Self {
        self.fd = fd;
        self
    }
    /// change fd type
    pub fn with_type(self, ty: FdType) -> Self {
        self.with_description(|d| d.ty = ty);
        self
    }
    /// Set per-slot descriptor flags on a newly duplicated fd.
    pub fn with_fd_flags(mut self, flags: OFlag) -> Self {
        self.fd_flags = flags.bits() & OFlag::O_CLOEXEC.bits();
        self
    }
    /// set path associated with `fd`
    pub fn with_path<P: AsRef<Path>>(self, path: P) -> Self {
        let path = Some(PathBuf::from(path.as_ref()));
        self.with_description(|d| d.path = path);
        self
    }
    /// set virtual inode
    pub fn with_inode(self, inode: DetInode) -> Self {
        self.with_description(|d| d.inode = Some(inode));
        self
    }
    /// set dirty flag
    pub fn with_dirty(self, dirty: bool) -> Self {
        self.with_description(|d| d.dirty = dirty);
        self
    }
    /// update statbuf
    pub fn with_stat<S: Into<Option<DetStat>>>(self, stat: S) -> Self {
        let stat = stat.into();
        self.with_description(|d| d.stat = stat);
        self
    }
    /// set resource id
    pub fn with_resource<S: Into<Option<ResourceID>>>(self, resource: S) -> Self {
        let resource = resource.into();
        self.with_description(|d| d.resource = resource);
        self
    }

    /// Update the scheduler resource shared by aliases of this open file.
    pub(crate) fn set_resource<S: Into<Option<ResourceID>>>(&self, resource: S) {
        let resource = resource.into();
        self.with_description(|d| d.resource = resource);
    }

    pub(crate) fn inherited_terminal(&self) -> Option<InheritedTerminal> {
        self.with_description(|d| d.inherited_terminal.clone())
    }

    pub(crate) fn set_inherited_terminal(&self, terminal: InheritedTerminal) {
        self.with_description(|d| d.inherited_terminal = Some(terminal));
    }

    /// If fd is non blocking
    pub fn is_nonblocking(&self) -> bool {
        self.with_description(|d| oflags_nonblocking(d.status_flags))
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
        self.with_description(|description| {
            if enabled {
                description.status_flags |= OFlag::O_NONBLOCK.bits();
            } else {
                description.status_flags &= !OFlag::O_NONBLOCK.bits();
            }
            description.physically_nonblocking = enabled;
        })
    }

    /// Update only the logical (guest-visible) O_NONBLOCK status flag, leaving
    /// the physical (scheduler) nonblocking state untouched. This lets a guest
    /// clear O_NONBLOCK while Detcore keeps the fd physically nonblocking, which
    /// the scheduler relies on for nonblockize-and-retry.
    pub fn set_logical_nonblocking(&self, enabled: bool) {
        self.with_description(|description| {
            if enabled {
                description.status_flags |= OFlag::O_NONBLOCK.bits();
            } else {
                description.status_flags &= !OFlag::O_NONBLOCK.bits();
            }
        })
    }

    /// Makes this descriptor refer to `other`'s open file description object.
    pub(crate) fn share_open_file_with(&mut self, other: &DetFd) {
        self.open_file = Arc::clone(&other.open_file);
    }

    /// Stable identity shared by dup and fork aliases. A shared handle keeps
    /// its identity locally, so this never takes the canonical model.
    pub fn open_file_id(&self) -> OpenFileId {
        match &*self.open_file.lock().expect("open file mutex poisoned") {
            OpenFileState::Local(description) => description.id,
            OpenFileState::Shared(shared) => shared.id,
        }
    }

    /// Number of modeled descriptor slots that retain this open file description.
    pub(crate) fn open_file_alias_count(&self) -> usize {
        Arc::strong_count(&self.open_file)
    }

    /// File type attached to the open file description.
    pub fn ty(&self) -> FdType {
        self.with_description(|d| d.ty)
    }

    /// This timerfd's virtual-time model, if Detcore keeps one.
    pub(crate) fn virtual_timerfd(&self) -> Option<VirtualTimerfd> {
        self.with_description(|d| d.timerfd)
    }

    /// Replace this timerfd's virtual-time model, under every alias.
    pub(crate) fn set_virtual_timerfd(&self, timerfd: Option<VirtualTimerfd>) {
        self.with_description(|d| {
            debug_assert_eq!(d.ty, FdType::Timerfd);
            d.timerfd = timerfd;
        })
    }

    /// Record the process identity carried by a newly created pidfd.
    pub(crate) fn set_pidfd_target(&self, target: DetPid) {
        self.with_description(|description| {
            debug_assert_eq!(description.ty, FdType::Pidfd);
            description.pidfd_target = Some(target);
        })
    }

    /// Return the process identity carried by this pidfd, when known.
    pub(crate) fn pidfd_target(&self) -> Option<DetPid> {
        self.with_description(|d| d.pidfd_target)
    }

    /// Resource attached to the open file description.
    pub fn resource(&self) -> Option<ResourceID> {
        self.with_description(|d| d.resource.clone())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1096): Review canonical random-device cursor sharing.
    /// Return the cursor shared by aliases of this random-device open file.
    #[cfg(test)]
    pub(crate) fn random_device_offset(&self) -> u64 {
        self.with_description(|d| d.random_device_offset)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1096): Review canonical random-device cursor sharing.
    /// Advance the cursor shared by aliases of this random-device open file.
    #[cfg(test)]
    pub(crate) fn advance_random_device_offset(&self, count: usize) {
        self.with_description(|description| {
            description.random_device_offset = description
                .random_device_offset
                .saturating_add(count as u64);
        })
    }

    /// Copy from the shared random-device cursor and commit the returned byte count.
    ///
    /// The description stays locked throughout the synchronous copy. The closure
    /// must not access this description through another `DetFd` method or alias.
    /// Clone the descriptor and drop its metadata guard before entering this
    /// transaction; the closure must not acquire Detcore metadata/description
    /// locks or await. This preserves the metadata-before-description lock order.
    /// A large copy can block relaxed-mode aliases for its bounded duration.
    /// An error leaves the cursor unchanged; it does not undo guest memory writes.
    pub(crate) fn with_random_device_stream<R>(
        &self,
        copy: impl FnOnce(u64) -> Result<usize, R>,
    ) -> Result<usize, R> {
        self.with_description(|description| {
            let copied = copy(description.random_device_offset)?;
            description.random_device_offset = description
                .random_device_offset
                .saturating_add(copied as u64);
            Ok(copied)
        })
    }

    /// Path used to open this file description, when it was observable.
    pub(crate) fn path(&self) -> Option<PathBuf> {
        self.with_description(|d| d.path.clone())
    }

    /// Record the resolved path used to open this file description.
    pub(crate) fn set_path<P: AsRef<Path>>(&self, path: P) {
        let path = Some(path.as_ref().to_path_buf());
        self.with_description(|d| d.path = path);
    }

    /// Attach deterministic procfs snapshot state to this open file description.
    pub(crate) fn set_procfs(&self, procfs: ProcfsFile) {
        self.with_description(|d| d.procfs = Some(procfs));
    }

    /// The procfs path of a file Detcore opened with no kernel object behind
    /// it (`ProcfsFile::is_rendered`): this descriptor is a placeholder.
    pub(crate) fn procfs_rendered_path(&self) -> Option<&'static Path> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .filter(|procfs| procfs.is_rendered())
                .and_then(ProcfsFile::rendered_path)
        })
    }

    /// The procfs path of a file whose every byte Detcore renders
    /// (`ProcfsFile::rendered_path`), whether Detcore opened it as a
    /// placeholder or the kernel opened it: through a symbolic link or `..`,
    /// after `chroot`, or under record and replay.
    pub(crate) fn procfs_renderable_path(&self) -> Option<&'static Path> {
        self.with_description(|d| d.procfs.as_ref().and_then(ProcfsFile::rendered_path))
    }

    /// Whether a short copy of a read of this procfs file is `EFAULT`
    /// (`ProcfsFile::faults_on_a_short_copy`).
    pub(crate) fn procfs_faults_on_a_short_copy(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::faults_on_a_short_copy)
        })
    }

    /// The bytes a rendered procfs file's snapshot starts from in place of a
    /// capture (`ProcfsFile::rendering_template`), if this is one.
    pub(crate) fn procfs_rendering_template(&self) -> Option<&'static [u8]> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .filter(|procfs| procfs.is_rendered())
                .map(ProcfsFile::rendering_template)
        })
    }

    /// Whether this procfs open file description still needs its initial snapshot.
    pub(crate) fn procfs_needs_snapshot(&self) -> bool {
        self.with_description(|d| d.procfs.as_ref().is_some_and(ProcfsFile::needs_snapshot))
    }

    /// Whether reads of this open file description are served from a procfs
    /// snapshot, already taken or still to be taken.
    pub(crate) fn procfs_serves_snapshot(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(|procfs| procfs.needs_snapshot() || procfs.position().1.is_some())
        })
    }

    /// Signal phase 1's kernel account of this file, if one was recorded.
    pub(crate) fn sigalrm_phase1_provenance(
        &self,
    ) -> Option<crate::sigalrm_phase1::Phase1Provenance> {
        self.with_description(|d| d.sigalrm_phase1)
    }

    /// Record signal phase 1's kernel account of this file.
    pub(crate) fn set_sigalrm_phase1_provenance(
        &self,
        provenance: crate::sigalrm_phase1::Phase1Provenance,
    ) {
        self.with_description(|d| d.sigalrm_phase1 = Some(provenance));
    }

    /// Whether this procfs file's snapshot cannot wait to be taken
    /// (`ProcfsFile::capture_cannot_wait`).
    pub(crate) fn procfs_capture_cannot_wait(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::capture_cannot_wait)
        })
    }

    /// Whether this procfs snapshot consumes deterministic random bytes.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-955): Review deterministic kernel UUID generation.
    /// Whether this snapshot has maps-format headers needing determinized
    /// device and inode columns.
    pub(crate) fn procfs_needs_mapping_identities(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::needs_mapping_identities)
        })
    }

    /// Whether this procfs file lists the guest's own address space, so
    /// its capture must not create a mapping that the listing would show.
    pub(crate) fn procfs_lists_address_space(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::lists_address_space)
        })
    }

    pub(crate) fn procfs_needs_mountinfo_identities(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::needs_mountinfo_identities)
        })
    }

    pub(crate) fn procfs_needs_random_uuid(&self) -> bool {
        self.with_description(|d| d.procfs.as_ref().is_some_and(ProcfsFile::needs_random_uuid))
    }

    pub(crate) fn procfs_is_cpuinfo(&self) -> bool {
        self.with_description(|d| d.procfs.as_ref().is_some_and(ProcfsFile::is_cpuinfo))
    }

    pub(crate) fn procfs_needs_boot_time(&self) -> bool {
        self.with_description(|d| d.procfs.as_ref().is_some_and(ProcfsFile::needs_boot_time))
    }

    /// Initialize the deterministic snapshot shared by all aliases.
    // TODO-HUMAN-REVIEW(PR-723): Review procfs snapshot identity parameters.
    // TODO-HUMAN-REVIEW(PR-955): Review deterministic UUID snapshot input.
    pub(crate) fn initialize_procfs(&self, contents: Vec<u8>, context: ProcfsSnapshotContext) {
        self.with_description(|d| {
            d.procfs
                .as_mut()
                .expect("procfs fd disappeared while taking its snapshot")
                .initialize(contents, context)
        });
    }

    /// Preview the snapshot bytes at its shared offset, with that offset,
    /// without consuming them.
    pub(crate) fn preview_procfs(&self, maximum: usize) -> Option<(usize, Vec<u8>)> {
        self.with_description(|description| {
            let procfs = description.procfs.as_ref()?;
            let (offset, _) = procfs.position();
            procfs.take_at(offset, maximum).map(|bytes| (offset, bytes))
        })
    }

    /// Advance the shared offset by the bytes a read copied to the guest.
    pub(crate) fn commit_procfs_read(&self, offset: usize, copied: usize) {
        self.with_description(|d| {
            d.procfs
                .as_mut()
                .expect("procfs fd disappeared while committing a read")
                .commit_read(offset, copied)
        });
    }

    /// Read a deterministic procfs snapshot without changing its shared cursor.
    pub(crate) fn take_procfs_at(&self, offset: usize, maximum: usize) -> Option<Vec<u8>> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .and_then(|procfs| procfs.take_at(offset, maximum))
        })
    }

    /// Namespace-visible task id and stable proc inode bound at open time.
    pub(crate) fn procfs_timer_slack_binding(&self) -> Option<(i32, u64, u64)> {
        self.with_description(|d| d.procfs.as_ref().and_then(ProcfsFile::timer_slack_binding))
    }

    /// Preview bytes without consuming the snapshot shared by dup/fork aliases.
    pub(crate) fn preview_procfs_timer_slack(
        &self,
        value: u64,
        maximum: usize,
    ) -> Option<TimerSlackReadPreview> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .and_then(|procfs| procfs.preview_timer_slack(value, maximum))
        })
    }

    /// Commit bytes only after their guest-memory copy succeeds.
    pub(crate) fn commit_procfs_timer_slack_read(
        &self,
        preview: &TimerSlackReadPreview,
        copied: usize,
    ) {
        self.with_description(|d| {
            d.procfs
                .as_mut()
                .expect("timer-slack procfs state disappeared")
                .commit_timer_slack_read(preview, copied)
        });
    }

    /// Read a fresh timer-slack value at an explicit offset.
    pub(crate) fn take_procfs_timer_slack_at(
        &self,
        value: u64,
        offset: usize,
        maximum: usize,
    ) -> Option<Vec<u8>> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .and_then(|procfs| procfs.take_timer_slack_at(value, offset, maximum))
        })
    }

    /// The lock serializing `getdents` and `lseek` on this open file
    /// description across every alias of it. For a shared description it
    /// covers only this process's aliases: the lock protects unsequentialized
    /// threads, and a description is shared only where threads are
    /// sequentialized.
    pub(crate) fn directory_lock(&self) -> Arc<futures::lock::Mutex<()>> {
        match &*self.open_file.lock().expect("open file mutex poisoned") {
            OpenFileState::Local(description) => Arc::clone(&description.directory_lock),
            OpenFileState::Shared(shared) => Arc::clone(&shared.directory_lock),
        }
    }

    /// Whether a `getdents` call has created a directory stream here. Only a
    /// successful read of the host directory creates one, so this also proves
    /// the open file is a directory.
    pub(crate) fn has_directory_stream(&self) -> bool {
        self.with_description(|d| d.directory.is_some())
    }

    /// Whether `getdents` on this open file reads the host directory one
    /// kernel buffer at a time instead of from a sorted stream.
    pub(crate) fn directory_in_host_order(&self) -> bool {
        self.with_description(|d| d.directory_host_order)
    }

    /// Read this open file's directory one kernel buffer at a time until it
    /// is seeked back to 0: its position was moved before the first
    /// `getdents`, or a read of the host directory after the first failed, so
    /// no snapshot of the whole directory could be taken. A guest buffer too
    /// small for the next entry does not land here: the stream answers it
    /// with `EINVAL`, as Linux does. Any stream is dropped, so `lseek`
    /// reaches the kernel again.
    pub(crate) fn use_host_directory_order(&self) {
        self.with_description(|description| {
            description.directory_host_order = true;
            description.directory = None;
        })
    }

    /// Serve this open file's directory as a sorted stream again, from its
    /// next `getdents`: its kernel position is back at 0.
    pub(crate) fn use_directory_stream(&self) {
        self.with_description(|d| d.directory_host_order = false);
    }

    /// Whether the next `getdents` must read the host directory.
    pub(crate) fn directory_needs_snapshot(&self) -> bool {
        self.with_description(|d| {
            d.directory
                .as_ref()
                .is_none_or(DirectoryStream::needs_snapshot)
        })
    }

    /// Install a snapshot freshly read in host order, creating the stream at
    /// position 0 if this is the open file's first `getdents`.
    ///
    /// `mark` is the inode pool's state read just before the host directory
    /// was (see `SnapshotMark`), and `inodes` how to key the inode numbers of
    /// its entries, decided then too.
    pub(crate) fn install_directory_snapshot(
        &self,
        entries: Vec<DirEntry>,
        mark: SnapshotMark,
        inodes: DirectoryInodes,
    ) {
        self.with_description(|d| {
            let stream = d.directory.get_or_insert_with(DirectoryStream::default);
            stream.install(entries);
            stream.note_snapshot_mark(mark);
            stream.note_snapshot_inodes(inodes);
        });
    }

    /// Run `f` on the directory stream shared by every alias of this open file.
    /// `EBADF` if there is none: under `--no-sequentialize-threads`, another
    /// thread can close or replace the descriptor after its caller found one.
    pub(crate) fn with_directory_stream<R>(
        &self,
        f: impl FnOnce(&mut DirectoryStream) -> R,
    ) -> Result<R, Errno> {
        self.with_description(|d| d.directory.as_mut().map(f).ok_or(Errno::EBADF))
    }

    /// Return the shared procfs cursor and initialized snapshot length.
    pub(crate) fn procfs_position(&self) -> Option<(usize, Option<usize>)> {
        self.with_description(|d| d.procfs.as_ref().map(ProcfsFile::position))
    }

    pub(crate) fn procfs_target_fd(&self) -> Option<i32> {
        self.with_description(|d| d.procfs.as_ref().and_then(ProcfsFile::target_fd))
    }

    /// Update the cursor shared by every alias of a procfs open file.
    pub(crate) fn set_procfs_offset(&self, offset: usize) {
        self.with_description(|d| {
            d.procfs
                .as_mut()
                .expect("procfs fd disappeared while updating its offset")
                .set_offset(offset)
        });
    }

    /// Cached stat data attached to the backing object.
    pub fn stat(&self) -> Option<DetStat> {
        self.with_description(|d| d.stat)
    }

    /// Whether Detcore has made the open file description physically nonblocking.
    pub fn physically_nonblocking(&self) -> bool {
        self.with_description(|d| d.physically_nonblocking)
    }

    pub(crate) fn status_flags(&self) -> i32 {
        self.with_description(|d| d.status_flags)
    }

    /// Mark every alias of this open file description physically nonblocking.
    pub fn set_physically_nonblocking(&self) {
        self.with_description(|d| d.physically_nonblocking = true);
    }

    /// Whether this is a FIFO that may still become an internally polled pipe
    /// (see `OpenFileDescription::fifo_awaiting_guest_peer`).
    pub(crate) fn fifo_awaiting_guest_peer(&self) -> bool {
        self.with_description(|d| d.fifo_awaiting_guest_peer)
    }

    /// Mark a `Regular` FIFO as one that may become an internally polled pipe.
    pub(crate) fn mark_fifo_awaiting_guest_peer(&self) {
        self.with_description(|d| {
            debug_assert_eq!(d.ty, FdType::Regular);
            d.fifo_awaiting_guest_peer = true;
        });
    }

    /// The FIFO is now physically nonblocking (Detcore set `O_NONBLOCK`, here
    /// or through another process sharing the open file description): treat
    /// it from now on as a scheduler-managed pipe, as `pipe2` creates.
    pub(crate) fn fifo_joins_internal_polling(&self) {
        self.with_description(|d| {
            debug_assert!(d.fifo_awaiting_guest_peer);
            d.fifo_awaiting_guest_peer = false;
            d.ty = FdType::Pipe;
            d.physically_nonblocking = true;
        });
    }

    /// Update file status flags for every alias of this open file description.
    pub fn set_status_flags(&self, flags: i32) {
        self.with_description(|description| {
            // F_SETFL cannot change how this description was opened. Access
            // checks must retain these bits even when an alias passes only
            // O_NONBLOCK (whose access-mode bits happen to spell O_RDONLY).
            let mutable = (OFlag::O_APPEND
                | OFlag::O_ASYNC
                | OFlag::O_DIRECT
                | OFlag::O_NOATIME
                | OFlag::O_NONBLOCK)
                .bits();
            description.status_flags = (description.status_flags & !mutable) | (flags & mutable);
            description.physically_nonblocking = oflags_nonblocking(description.status_flags);
        })
    }

    // TODO-HUMAN-REVIEW(PR-912): Review open-file sharing of socket receive timestamps.
    /// Record the logical time at which a socket delivered its most recent packet.
    pub(crate) fn set_socket_receive_timestamp(&self, timestamp: LogicalTime) {
        self.with_description(|d| d.socket_receive_timestamp = Some(timestamp));
    }

    /// Return the last receive timestamp shared by every alias of this socket.
    pub(crate) fn socket_receive_timestamp(&self) -> Option<LogicalTime> {
        self.with_description(|d| d.socket_receive_timestamp)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1064)
    /// Mark this open file as a `NETLINK_SOCK_DIAG` socket. Shared across every
    /// dup/fork alias of the same open file description.
    pub(crate) fn set_sock_diag(&self) {
        self.with_description(|d| d.sock_diag = true);
    }

    // TODO-HUMAN-REVIEW(PR-1064)
    /// Whether this open file is a `NETLINK_SOCK_DIAG` socket whose dump replies
    /// must have their socket inode numbers determinized.
    pub(crate) fn is_sock_diag(&self) -> bool {
        self.with_description(|d| d.sock_diag)
    }

    /// Mark this open file as a `NETLINK_ROUTE` socket. Like `set_sock_diag`
    /// this applies to the open file description, so a dup or fork alias of the
    /// same socket is covered too.
    pub(crate) fn set_netlink_route(&self) {
        self.with_description(|d| d.netlink_route = true);
    }

    /// Whether this open file is a `NETLINK_ROUTE` socket whose link-dump
    /// replies carry live interface counters.
    pub(crate) fn is_netlink_route(&self) -> bool {
        self.with_description(|d| d.netlink_route)
    }

    /// Update whether this open file is connected to a loopback peer.
    pub(crate) fn set_loopback_peer(&self, loopback_peer: bool) {
        self.with_description(|d| d.loopback_peer = loopback_peer);
    }

    /// Whether this open file is a socket connected to a loopback peer.
    pub(crate) fn is_loopback_peer(&self) -> bool {
        self.with_description(|d| d.loopback_peer)
    }

    /// Mark this open file as an external network trace channel.
    pub(crate) fn set_network_channel(&self) {
        self.with_description(|d| d.network_channel = true);
    }

    /// Whether this open file is an external network trace channel.
    pub(crate) fn is_network_channel(&self) -> bool {
        self.with_description(|d| d.network_channel)
    }

    /// Record the socket's `SO_RCVLOWAT`.
    pub(crate) fn set_network_lowat(&self, lowat: usize) {
        self.with_description(|d| d.network_lowat = Some(lowat));
    }

    /// The socket's `SO_RCVLOWAT`.
    pub(crate) fn network_lowat(&self) -> usize {
        self.with_description(|d| d.network_lowat.unwrap_or(1))
    }

    /// Record the socket's family and type for the network trace.
    pub(crate) fn set_network_socket(&self, kind: NetworkSocketKind) {
        self.with_description(|d| d.network_socket = kind);
    }

    /// The socket's family and type as the network trace classified it.
    pub(crate) fn network_socket(&self) -> NetworkSocketKind {
        self.with_description(|d| d.network_socket)
    }

    /// Mark this open file as one end of a `socketpair(2)`. Like
    /// `set_netlink_route` this applies to the open file description, so a dup
    /// or fork alias of the same endpoint is covered too -- which is the case
    /// that matters, since the usual way to use a socketpair is to fork and let
    /// each side inherit one end.
    pub(crate) fn set_socketpair_endpoint(&self) {
        self.with_description(|d| d.socketpair_endpoint = true);
    }

    /// Whether this open file is one end of a `socketpair(2)`, and therefore
    /// container-internal.
    pub(crate) fn is_socketpair_endpoint(&self) -> bool {
        self.with_description(|d| d.socketpair_endpoint)
    }

    /// Mark this open file as a connection returned by `accept(2)` or
    /// `accept4(2)`. It applies to the open file description, so dup and fork
    /// aliases are covered too.
    pub(crate) fn set_accepted_connection(&self) {
        self.with_description(|d| d.accepted_connection = true);
    }

    /// Whether `accept(2)` or `accept4(2)` returned this open file.
    pub(crate) fn is_accepted_connection(&self) -> bool {
        self.with_description(|d| d.accepted_connection)
    }

    /// Mark this open file as a listener whose blocking `accept(2)` runs in
    /// the caller's turn under record/replay. It applies to the open file
    /// description, so dup and fork aliases are covered too.
    pub(crate) fn set_accept_in_turn_eligible(&self) {
        self.with_description(|d| d.accept_in_turn_eligible = true);
    }

    /// Whether a blocking `accept(2)` on this open file runs in the caller's
    /// turn: it was marked eligible and has not been exported.
    pub(crate) fn is_accept_in_turn_listener(&self) -> bool {
        self.with_description(|d| d.accept_in_turn_eligible && !d.exported)
    }

    /// Whether this open file was marked eligible, exported or not.
    pub(crate) fn is_accept_in_turn_eligible(&self) -> bool {
        self.with_description(|d| d.accept_in_turn_eligible)
    }

    /// Mark this open file as passed over `SCM_RIGHTS`. Permanent.
    pub(crate) fn set_exported(&self) {
        self.with_description(|d| d.exported = true);
    }

    /// Whether this open file was passed over `SCM_RIGHTS`.
    pub(crate) fn is_exported(&self) -> bool {
        self.with_description(|d| d.exported)
    }

    /// Record the receive timeout a successful `setsockopt(SO_RCVTIMEO)` set.
    pub(crate) fn set_receive_timeout(&self, timeout: ReceiveTimeout) {
        self.with_description(|d| d.receive_timeout = timeout);
    }

    /// The receive timeout `setsockopt(SO_RCVTIMEO)` calls set through any
    /// descriptor Detcore tracks for this open file description, dup, fork and
    /// exec aliases included. A copy that reached a guest some other way, which
    /// Detcore tracks as a description of its own, does not update it; record
    /// checks the model against the kernel before a blocking accept relies on
    /// it.
    pub(crate) fn receive_timeout(&self) -> ReceiveTimeout {
        self.with_description(|d| d.receive_timeout)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// Return the known `flock(2)` state for this open file description.
    ///
    /// The outer `None` means Detcore did not observe the descriptor's history.
    /// `Some(None)` means known-unlocked; `Some(Some(mode))` means the kernel
    /// granted `LOCK_SH` or `LOCK_EX` through this handler.
    pub(crate) fn known_flock_mode(&self) -> Option<Option<i32>> {
        self.with_description(|description| {
            description
                .flock_mode_known
                .then_some(description.flock_mode)
        })
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// Record the `flock(2)` mode this open file description now holds. Every
    /// `dup`/`fork` alias observes the change, matching the kernel, where one
    /// `flock` lock belongs to the whole open file description.
    pub(crate) fn set_flock_mode(&self, mode: Option<i32>) {
        self.with_description(|description| {
            if description.flock_mode_known {
                description.flock_mode = mode;
            }
        })
    }

    /// Mark the kernel lock state unknown without changing the kernel lock.
    /// This is required for live-discovered and externally received file
    /// descriptions, whose history Detcore did not observe.
    pub(crate) fn forget_flock_mode(&self) {
        self.with_description(|description| {
            description.flock_mode = None;
            description.flock_mode_known = false;
        })
    }

    /// Record that Detcore never observed this description's lock history, as
    /// opposed to having known it and then invalidated it. Used for descriptors
    /// that existed before Detcore began observing the guest.
    pub(crate) fn mark_flock_mode_unobserved(&self) {
        self.with_description(|description| {
            description.flock_mode = None;
            description.flock_mode_known = false;
            description.flock_mode_ever_known = false;
        })
    }

    /// True when a cached lock claim existed and may now be wrong.
    pub(crate) fn flock_mode_may_be_stale(&self) -> bool {
        self.with_description(|description| {
            description.flock_mode_ever_known && !description.flock_mode_known
        })
    }
}

impl fmt::Display for DetFd {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "DetFd({})", self.fd)
    }
}

/// Restores descriptor aliasing after deserialization: every descriptor with
/// the same `OpenFileId` refers to one open file description object again.
///
/// Serialization writes an `Arc`'s contents, not its identity, so two
/// descriptors that shared one description (after `dup`) come back as two
/// independent copies, and a read through one would no longer move the
/// other's cursor. Within one table an `OpenFileId` names exactly one open
/// file description, so the copies with one id are identical, and the first
/// (lowest descriptor number) is kept. Descriptor flags (`FD_CLOEXEC`) stay
/// per descriptor.
pub(crate) fn intern_open_files(handles: &mut std::collections::HashMap<RawFd, DetFd>) {
    let mut fds: Vec<RawFd> = handles.keys().copied().collect();
    fds.sort_unstable();
    let mut first: std::collections::HashMap<OpenFileId, DetFd> = std::collections::HashMap::new();
    for fd in fds {
        let detfd = handles.get_mut(&fd).expect("listed descriptor");
        match first.get(&detfd.open_file_id()) {
            Some(kept) => detfd.share_open_file_with(kept),
            None => {
                first.insert(detfd.open_file_id(), detfd.clone());
            }
        }
    }
}

impl DetFd {
    /// A copy of this local description's model, for tests of the global
    /// store.
    #[cfg(test)]
    pub(crate) fn model_for_test(&self) -> OpenFileModel {
        self.with_description(|d| {
            OpenFileModel(serde_json::from_value(serde_json::to_value(&*d).unwrap()).unwrap())
        })
    }

    /// Moves this open file description's model out to a test's canonical
    /// store and makes every alias a shared handle, keeping this process's
    /// directory lock. The real promotion is a later step.
    #[cfg(test)]
    fn share_for_test(&self) -> OpenFileModel {
        let mut state = self.open_file.lock().expect("open file mutex poisoned");
        let OpenFileState::Local(description) = &*state else {
            panic!("open file description is already shared");
        };
        let shared = SharedOpenFile {
            id: description.id,
            directory_lock: Arc::clone(&description.directory_lock),
        };
        let OpenFileState::Local(description) =
            std::mem::replace(&mut *state, OpenFileState::Shared(shared))
        else {
            unreachable!("checked above");
        };
        OpenFileModel(description)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_access_modes_survive_status_updates_through_aliases() {
        for mode in [
            OFlag::O_RDONLY,
            OFlag::O_WRONLY,
            OFlag::O_RDWR,
            OFlag::O_ACCMODE,
            OFlag::O_PATH,
        ] {
            let original = DetFd::new(
                3,
                mode,
                FdType::Rng,
                OpenFileId::new(DetTid::from_raw(1), 0),
            );
            let alias = original.clone().with_fd(4);
            for update in [
                OFlag::O_NONBLOCK,
                OFlag::O_RDWR,
                OFlag::O_WRONLY,
                OFlag::O_PATH | OFlag::O_NONBLOCK,
            ] {
                alias.set_status_flags(update.bits());
                assert_eq!(
                    original.status_flags() & (OFlag::O_ACCMODE | OFlag::O_PATH).bits(),
                    mode.bits()
                );
                assert_eq!(
                    original.is_nonblocking(),
                    update.contains(OFlag::O_NONBLOCK)
                );
            }
        }
    }

    #[test]
    fn random_status_updates_preserve_immutable_flags_and_replace_mutable_flags() {
        let immutable = OFlag::O_RDWR | OFlag::O_SYNC;
        let original = DetFd::new(
            3,
            immutable | OFlag::O_APPEND | OFlag::O_NONBLOCK,
            FdType::Rng,
            OpenFileId::new(DetTid::from_raw(1), 0),
        );
        let alias = original.clone().with_fd(4);
        alias.set_status_flags((OFlag::O_WRONLY | OFlag::O_ASYNC | OFlag::O_NOATIME).bits());
        assert_eq!(
            original.status_flags(),
            (immutable | OFlag::O_ASYNC | OFlag::O_NOATIME).bits()
        );
        assert!(!original.physically_nonblocking());
        alias.set_status_flags(OFlag::O_NONBLOCK.bits());
        assert_eq!(
            original.status_flags(),
            (immutable | OFlag::O_NONBLOCK).bits()
        );
        assert!(original.physically_nonblocking());
    }

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

        assert!(!original.is_loopback_peer());
        duplicate.set_loopback_peer(true);
        assert!(
            original.is_loopback_peer(),
            "loopback-peer marking through one alias must be visible through every alias"
        );
        original.set_loopback_peer(false);
        assert!(
            !duplicate.is_loopback_peer(),
            "reconnects through one alias must clear loopback state for every alias"
        );
    }

    /// `flock(2)` locks belong to the open file description, so every `dup`
    /// alias must see one lock, while a separate `open` of the same file is a
    /// distinct contender with its own state.
    #[test]
    fn flock_mode_is_shared_by_dup_aliases_and_private_to_separate_opens() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(
            3,
            OFlag::empty(),
            FdType::Regular,
            OpenFileId::new(owner, 0),
        );
        let duplicate = original.clone().with_fd(4);
        let separate_open = DetFd::new(
            5,
            OFlag::empty(),
            FdType::Regular,
            OpenFileId::new(owner, 1),
        );

        assert_eq!(original.known_flock_mode(), Some(None));
        original.set_flock_mode(Some(libc::LOCK_SH));
        assert_eq!(
            duplicate.known_flock_mode(),
            Some(Some(libc::LOCK_SH)),
            "a dup alias shares the open file description's flock lock"
        );
        assert_eq!(
            separate_open.known_flock_mode(),
            Some(None),
            "a separate open of the same file is an independent flock contender"
        );

        duplicate.set_flock_mode(Some(libc::LOCK_EX));
        assert_eq!(original.known_flock_mode(), Some(Some(libc::LOCK_EX)));
        duplicate.set_flock_mode(None);
        assert_eq!(
            original.known_flock_mode(),
            Some(None),
            "LOCK_UN through one alias releases the whole open file description"
        );

        duplicate.forget_flock_mode();
        assert_eq!(
            original.known_flock_mode(),
            None,
            "unknown state must be shared by every alias of the open file description"
        );
        original.set_flock_mode(Some(libc::LOCK_SH));
        assert_eq!(
            duplicate.known_flock_mode(),
            None,
            "a later call cannot make externally mutable lock state reliable again"
        );
    }

    #[test]
    fn pidfd_target_is_shared_by_descriptor_aliases() {
        let target = DetPid::from_raw(41);
        let original = DetFd::new(
            3,
            OFlag::O_CLOEXEC,
            FdType::Pidfd,
            OpenFileId::new(DetTid::from_raw(7), 0),
        );
        let duplicate = original.clone().with_fd(4);

        assert_eq!(original.pidfd_target(), None);
        original.set_pidfd_target(target);
        assert_eq!(original.pidfd_target(), Some(target));
        assert_eq!(duplicate.pidfd_target(), Some(target));
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

        assert_eq!(original.preview_procfs(2).unwrap(), (0, b"0\t".to_vec()));
        assert_eq!(duplicate.procfs_position().unwrap().0, 0);
        original.commit_procfs_read(0, 2);
        assert_eq!(duplicate.procfs_position().unwrap().0, 2);
        assert_eq!(duplicate.take_procfs_at(4, 1).unwrap(), b"9");
        assert_eq!(original.procfs_position().unwrap().0, 2);

        // A read that copied nothing leaves the shared cursor in place.
        let (offset, _) = duplicate.preview_procfs(4).unwrap();
        duplicate.commit_procfs_read(offset, 0);
        assert_eq!(original.procfs_position().unwrap().0, 2);

        duplicate.set_procfs_offset(0);
        assert_eq!(
            original.preview_procfs(128).unwrap(),
            (0, b"0\t0\t9223372036854775807\n".to_vec())
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
    fn random_device_stream_commits_returned_prefix_for_dup_aliases() {
        let owner = DetTid::from_raw(10);
        let id = OpenFileId::new(owner, 0);
        let original = DetFd::new(3, OFlag::empty(), FdType::Rng, id);
        let duplicate = original.clone().with_fd(4);
        original.advance_random_device_offset(7);

        assert_eq!(
            duplicate.with_random_device_stream(|offset| {
                assert_eq!(offset, 7);
                // A copy requesting eight bytes completed only this prefix.
                Ok::<_, ()>(3)
            }),
            Ok(3)
        );
        assert_eq!(original.random_device_offset(), 10);
        assert_eq!(duplicate.random_device_offset(), 10);
        assert_eq!(
            original.with_random_device_stream(|offset| {
                assert_eq!(offset, 10);
                Ok::<_, ()>(2)
            }),
            Ok(2)
        );
        assert_eq!(original.random_device_offset(), 12);
        assert_eq!(duplicate.random_device_offset(), 12);
        assert_eq!(original.open_file_id(), id);
        assert_eq!(duplicate.open_file_id(), id);
        assert!(Arc::ptr_eq(&original.open_file, &duplicate.open_file));
    }

    #[test]
    fn random_device_stream_preserves_error_identity_and_cursor() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(3, OFlag::empty(), FdType::Rng, OpenFileId::new(owner, 0));
        let duplicate = original.clone().with_fd(4);
        original.advance_random_device_offset(11);
        let failure = Arc::new(String::from("copy failure"));

        let error = duplicate
            .with_random_device_stream(|offset| {
                assert_eq!(offset, 11);
                Err(Arc::clone(&failure))
            })
            .unwrap_err();
        assert!(Arc::ptr_eq(&error, &failure));
        assert_eq!(original.random_device_offset(), 11);
        assert_eq!(duplicate.random_device_offset(), 11);
        assert_eq!(original.open_file_id(), duplicate.open_file_id());
    }

    #[test]
    fn random_device_stream_saturates_independently_of_partitioning() {
        let owner = DetTid::from_raw(10);
        for initial in [u64::MAX - 2, u64::MAX] {
            let whole = DetFd::new(3, OFlag::empty(), FdType::Rng, OpenFileId::new(owner, 0));
            let split = DetFd::new(4, OFlag::empty(), FdType::Rng, OpenFileId::new(owner, 1));
            whole.with_description(|d| d.random_device_offset = initial);
            split.with_description(|d| d.random_device_offset = initial);

            assert_eq!(
                whole.with_random_device_stream(|offset| {
                    assert_eq!(offset, initial);
                    Ok::<_, ()>(4)
                }),
                Ok(4)
            );
            assert_eq!(whole.random_device_offset(), u64::MAX);
            assert_eq!(
                split.with_random_device_stream(|offset| {
                    assert_eq!(offset, initial);
                    Ok::<_, ()>(1)
                }),
                Ok(1)
            );
            assert_eq!(split.random_device_offset(), initial.saturating_add(1));
            assert_eq!(
                split.with_random_device_stream(|offset| {
                    assert_eq!(offset, initial.saturating_add(1));
                    Ok::<_, ()>(3)
                }),
                Ok(3)
            );
            assert_eq!(split.random_device_offset(), u64::MAX);
        }
    }

    #[test]
    fn random_device_stream_holds_alias_lock_while_copying() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(3, OFlag::empty(), FdType::Rng, OpenFileId::new(owner, 0));
        let duplicate = original.clone().with_fd(4);

        assert_eq!(
            original.with_random_device_stream(|offset| {
                assert_eq!(offset, 0);
                // A nonblocking attempt cannot deadlock the copy, and observes
                // whether another thread could access the shared cursor here.
                assert!(
                    std::thread::spawn(move || {
                        matches!(
                            duplicate.open_file.try_lock(),
                            Err(std::sync::TryLockError::WouldBlock)
                        )
                    })
                    .join()
                    .unwrap()
                );
                Ok::<_, ()>(5)
            }),
            Ok(5)
        );
        assert_eq!(original.random_device_offset(), 5);
    }

    /// A path argument whose conversion checks that no alias's description
    /// lock is held while it converts.
    struct PathProbe {
        alias: DetFd,
    }

    impl AsRef<Path> for PathProbe {
        fn as_ref(&self) -> &Path {
            assert!(
                self.alias.open_file.try_lock().is_ok(),
                "an argument conversion ran under the description lock"
            );
            Path::new("/probe")
        }
    }

    #[test]
    fn argument_conversions_run_before_the_description_lock() {
        let original = DetFd::new(
            3,
            OFlag::O_RDONLY,
            FdType::Regular,
            OpenFileId::new(DetTid::from_raw(11), 0),
        );
        let alias = original.clone().with_fd(4);
        original.set_path(PathProbe {
            alias: alias.clone(),
        });
        let original = original.with_path(PathProbe {
            alias: alias.clone(),
        });
        assert_eq!(alias.path(), Some(PathBuf::from("/probe")));
        drop(original);
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

    /// The canonical store of the tests' shared open file descriptions, and
    /// the channel installed for this test process. Each test uses its own
    /// creator task, so tests running in parallel never share a description.
    #[derive(Default)]
    struct MemoryChannel {
        store: Mutex<std::collections::HashMap<OpenFileId, Canonical>>,
    }

    struct Canonical {
        /// `None` while a lease holds the model.
        model: Option<OpenFileModel>,
        sequence: u64,
        takes: usize,
    }

    struct InstalledMemoryChannel(&'static MemoryChannel);

    impl SharedOpenFileChannel for InstalledMemoryChannel {
        fn take(&self, id: OpenFileId) -> Result<OpenFileLease, SharedOpenFileError> {
            let mut store = self.0.store.lock().unwrap();
            let canonical = store
                .get_mut(&id)
                .ok_or_else(|| SharedOpenFileError(format!("{id:?} is not shared")))?;
            let model = canonical
                .model
                .take()
                .ok_or_else(|| SharedOpenFileError(format!("{id:?} is already leased")))?;
            canonical.sequence += 1;
            canonical.takes += 1;
            Ok(OpenFileLease {
                id,
                sequence: canonical.sequence,
                model,
            })
        }

        fn publish(&self, lease: OpenFileLease) -> Result<(), SharedOpenFileError> {
            let mut store = self.0.store.lock().unwrap();
            let canonical = store
                .get_mut(&lease.id)
                .ok_or_else(|| SharedOpenFileError(format!("{:?} is not shared", lease.id)))?;
            if canonical.model.is_some() || canonical.sequence != lease.sequence {
                return Err(SharedOpenFileError(format!(
                    "{:?}: publish names no current lease",
                    lease.id
                )));
            }
            canonical.model = Some(lease.model);
            Ok(())
        }
    }

    impl MemoryChannel {
        fn installed() -> &'static MemoryChannel {
            static CHANNEL: OnceLock<MemoryChannel> = OnceLock::new();
            let channel = CHANNEL.get_or_init(MemoryChannel::default);
            // Only the first installation is kept, and it is this channel.
            let _ = install_shared_open_file_channel(Box::new(InstalledMemoryChannel(channel)));
            channel
        }

        fn share(&self, detfd: &DetFd) {
            let model = detfd.share_for_test();
            let previous = self.store.lock().unwrap().insert(
                model.0.id,
                Canonical {
                    model: Some(model),
                    sequence: 0,
                    takes: 0,
                },
            );
            assert!(previous.is_none());
        }

        fn takes(&self, id: OpenFileId) -> usize {
            self.store.lock().unwrap()[&id].takes
        }

        /// Runs `f` on the canonical model, which must not be leased.
        fn canonical<R>(&self, id: OpenFileId, f: impl FnOnce(&OpenFileDescription) -> R) -> R {
            let store = self.store.lock().unwrap();
            f(&store[&id]
                .model
                .as_ref()
                .expect("a lease outlived its method call")
                .0)
        }
    }

    #[test]
    fn a_shared_handle_reaches_the_canonical_model_through_every_alias() {
        let channel = MemoryChannel::installed();
        let id = OpenFileId::new(DetTid::from_raw(9001), 0);
        let original = DetFd::new(3, OFlag::O_RDWR, FdType::Regular, id);
        let alias = original.clone().with_fd(4);
        // A clone that escaped the descriptor table before the promotion.
        let escaped = original.clone();
        channel.share(&original);

        alias.set_status_flags(OFlag::O_NONBLOCK.bits());
        assert_eq!(channel.takes(id), 1);
        assert!(channel.canonical(id, |model| oflags_nonblocking(model.status_flags)));
        assert!(original.is_nonblocking());
        assert!(escaped.is_nonblocking());
        assert_eq!(
            escaped.status_flags() & OFlag::O_ACCMODE.bits(),
            OFlag::O_RDWR.bits()
        );
        assert_eq!(channel.takes(id), 4);
        assert_eq!(original.open_file_alias_count(), 3);
    }

    #[test]
    fn identity_and_directory_lock_never_take_the_shared_model() {
        let channel = MemoryChannel::installed();
        let id = OpenFileId::new(DetTid::from_raw(9002), 0);
        let original = DetFd::new(3, OFlag::O_RDONLY, FdType::Regular, id);
        let local_lock = original.directory_lock();
        channel.share(&original);

        assert_eq!(original.open_file_id(), id);
        assert!(Arc::ptr_eq(&original.directory_lock(), &local_lock));
        assert!(Arc::ptr_eq(
            &original.clone().with_fd(4).directory_lock(),
            &local_lock
        ));
        assert_eq!(channel.takes(id), 0);
    }

    #[test]
    fn a_shared_handle_serializes_only_its_identity() {
        let channel = MemoryChannel::installed();
        let id = OpenFileId::new(DetTid::from_raw(9003), 0);
        let original = DetFd::new(3, OFlag::O_RDONLY, FdType::Regular, id).with_path("/a");
        channel.share(&original);

        let encoded = serde_json::to_string(&original).unwrap();
        assert!(encoded.contains("Shared"), "{encoded}");
        assert!(!encoded.contains("status_flags"), "{encoded}");
        assert!(!encoded.contains("/a"), "{encoded}");
        assert_eq!(channel.takes(id), 0);

        // A restored table interns its shared handles by identity, as it
        // does local ones, without taking the model.
        let mut handles = std::collections::HashMap::new();
        for fd in [3, 4] {
            let restored: DetFd = serde_json::from_str(&encoded).unwrap();
            handles.insert(fd, restored.with_fd(fd));
        }
        intern_open_files(&mut handles);
        assert_eq!(handles[&4].open_file_alias_count(), 2);
        assert_eq!(channel.takes(id), 0);
        assert_eq!(handles[&4].path(), Some(PathBuf::from("/a")));
        assert_eq!(channel.takes(id), 1);
    }

    #[test]
    fn a_failed_random_device_copy_still_ends_the_lease() {
        let channel = MemoryChannel::installed();
        let id = OpenFileId::new(DetTid::from_raw(9004), 0);
        let original = DetFd::new(3, OFlag::empty(), FdType::Rng, id);
        channel.share(&original);

        assert_eq!(
            original.with_random_device_stream(|_| Err::<usize, _>(())),
            Err(())
        );
        assert_eq!(channel.canonical(id, |model| model.random_device_offset), 0);
        assert_eq!(
            original.with_random_device_stream(|offset| {
                assert_eq!(offset, 0);
                Ok::<_, ()>(5)
            }),
            Ok(5)
        );
        assert_eq!(channel.canonical(id, |model| model.random_device_offset), 5);
        assert_eq!(channel.takes(id), 2);
    }

    fn at(ns: u64) -> LogicalTime {
        LogicalTime::from_nanos(ns)
    }

    /// A one-shot timerfd counts one expiration once virtual time reaches its
    /// deadline, then disarms; before the deadline it counts none.
    #[test]
    fn a_one_shot_timerfd_expires_once_at_its_virtual_deadline() {
        let mut timer = VirtualTimerfd::set(Some(at(1_000)), 0);
        assert_eq!(timer.expire_through(at(999)), 0);
        assert_eq!(timer.remaining(at(999)), (1, 0));
        assert_eq!(timer.expire_through(at(1_000)), 1);
        assert_eq!(
            timer,
            VirtualTimerfd {
                deadline: None,
                interval_ns: 0,
                unread: 1
            }
        );
        assert_eq!(timer.expire_through(at(5_000)), 0);
        assert_eq!(timer.remaining(at(5_000)), (0, 0));
    }

    /// A periodic timerfd observed late counts every expiration it missed,
    /// exactly, and its next deadline stays on the original grid: the count
    /// is a function of virtual time, not of how often it was observed.
    #[test]
    fn a_periodic_timerfd_counts_every_missed_expiration_on_its_grid() {
        let mut once = VirtualTimerfd::set(Some(at(1_000)), 300);
        assert_eq!(once.expire_through(at(1_950)), 4);
        assert_eq!(once.deadline, Some(at(2_200)));
        assert_eq!(once.remaining(at(1_950)), (250, 300));

        let mut often = VirtualTimerfd::set(Some(at(1_000)), 300);
        let mut added = 0;
        for now in [1_000, 1_100, 1_300, 1_599, 1_950] {
            added += often.expire_through(at(now));
        }
        assert_eq!(added, 4);
        assert_eq!(often, once);
    }

    /// A timer disarmed with a zero value keeps its interval for
    /// `timerfd_gettime`, as Linux does, and never expires.
    #[test]
    fn a_disarmed_timerfd_keeps_its_interval_and_never_expires() {
        let mut timer = VirtualTimerfd::set(None, 300);
        assert_eq!(timer.expire_through(at(u64::MAX / 2)), 0);
        assert_eq!(timer.remaining(at(5)), (0, 300));
        assert_eq!(timer.unread, 0);
    }

    /// Setting a timerfd discards its unread expirations, as Linux does.
    #[test]
    fn setting_a_timerfd_discards_unread_expirations() {
        let mut timer = VirtualTimerfd::set(Some(at(10)), 0);
        timer.expire_through(at(20));
        assert_eq!(timer.unread, 1);
        assert_eq!(VirtualTimerfd::set(None, 0).unread, 0);
    }
}
