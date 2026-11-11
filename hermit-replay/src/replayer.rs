/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[macro_use]
mod macros;

mod fs;
mod mmap;
mod network;
mod random;
mod time;

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::io::Seek;
use std::io::SeekFrom;
use std::ops::Deref;
use std::ops::DerefMut;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
#[cfg(target_arch = "x86_64")]
use reverie::Rdtsc;
#[cfg(target_arch = "x86_64")]
use reverie::RdtscResult;
use reverie::Stack;
use reverie::Subscription;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Chdir;
use reverie::syscalls::Close;
use reverie::syscalls::Dup3;
use reverie::syscalls::EfdFlags;
use reverie::syscalls::Eventfd2;
use reverie::syscalls::Fcntl;
use reverie::syscalls::FcntlCmd;
use reverie::syscalls::Flock;
use reverie::syscalls::FromToRaw;
use reverie::syscalls::Lseek;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::OFlag;
use reverie::syscalls::Openat;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie::syscalls::Unlinkat;
use reverie::syscalls::Whence;
use serde::Deserialize;
use serde::Serialize;
fn capture_guest_fd(pid: Pid, fd: libc::c_int) -> (Option<std::os::fd::OwnedFd>, Option<String>) {
    match crate::fd::duplicate_guest_fd(pid, fd) {
        Ok(duplicate) => (Some(duplicate), None),
        Err(error) if error.raw_os_error() == Some(libc::EBADF) => (None, None),
        Err(error) => (None, Some(error.to_string())),
    }
}

fn same_guest_open_file_description(
    pid: Pid,
    left: libc::c_int,
    right: libc::c_int,
) -> io::Result<bool> {
    const KCMP_FILE: libc::c_int = 0;
    let comparison = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            pid.as_raw(),
            pid.as_raw(),
            KCMP_FILE,
            left,
            right,
        )
    };
    if comparison == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(comparison == 0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReplayFileIdentity {
    device: u64,
    inode: u64,
}

impl ReplayFileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

type MaterializedFiles = BTreeMap<ReplayFileIdentity, BTreeMap<ReplayFileIdentity, OwnedFd>>;

static MATERIALIZED_FILES: OnceLock<Mutex<MaterializedFiles>> = OnceLock::new();

fn materialized_files() -> &'static Mutex<MaterializedFiles> {
    MATERIALIZED_FILES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn identity_for_fd(fd: libc::c_int) -> io::Result<ReplayFileIdentity> {
    Ok(ReplayFileIdentity::from_metadata(&std::fs::metadata(
        format!("/proc/self/fd/{fd}"),
    )?))
}

fn replay_root_identity(pid: Pid) -> io::Result<ReplayFileIdentity> {
    Ok(ReplayFileIdentity::from_metadata(&std::fs::metadata(
        format!("/proc/{}/root", pid.as_raw()),
    )?))
}

pub struct ReplayMaterializationScope {
    identity: ReplayFileIdentity,
    _root: OwnedFd,
    pending_at_start: Vec<PendingPlaceholderKey>,
}

/// The identity under which one pending placeholder entry is remembered,
/// used to tell entries that predated a scope from entries it created.
type PendingPlaceholderKey = (
    u64,
    ReplayFileIdentity,
    libc::pid_t,
    libc::pid_t,
    libc::c_int,
    Option<String>,
);

fn pending_placeholder_key(pending: &PendingPlaceholderOpen) -> PendingPlaceholderKey {
    (
        pending.open_id,
        pending.root,
        pending.tgid,
        pending.opener_tid,
        pending.fd,
        pending.placeholder_id.clone(),
    )
}

impl ReplayMaterializationScope {
    pub fn new(root_fd: libc::c_int) -> io::Result<Self> {
        let root = duplicate_controller_fd(root_fd)?;
        let identity = identity_for_fd(root.as_raw_fd())?;
        replay_roots()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((identity, root.try_clone()?));
        // Pending flock entries are keyed by the root their recorded
        // open resolved under, and a guest that chroots mid-run opens
        // under a *different* root than this scope was created for.
        // Removing only this scope's identity below would leave those
        // entries spendable by a later run, so remember exactly which
        // entries predate the scope and remove every other one.
        let pending_at_start = PENDING_PLACEHOLDER_OPENS
            .get()
            .map_or_else(Vec::new, |registry| {
                registry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .map(pending_placeholder_key)
                    .collect()
            });
        Ok(Self {
            identity,
            _root: root,
            pending_at_start,
        })
    }
}

impl Drop for ReplayMaterializationScope {
    fn drop(&mut self) {
        materialized_files()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.identity);
        // Pending flock entries created during this scope die with it,
        // like its materialized files, so a later run reusing a descriptor
        // number and thread-group id cannot spend a stale entry. This
        // includes entries opened under a root a guest `chroot` moved it
        // to mid-run, which are not keyed by this scope's own identity.
        // The registry itself is declared with the pending-open type below.
        if let Some(registry) = PENDING_PLACEHOLDER_OPENS.get() {
            registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|pending| {
                    self.pending_at_start
                        .contains(&pending_placeholder_key(pending))
                });
        }
        replay_roots()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(identity, _)| *identity != self.identity);
    }
}

/// The real roots of the replays this process runs: each replay's own root
/// directory, its `TempChroot`, as `ReplayMaterializationScope` holds it,
/// whatever root a guest process later chroots into.
fn replay_roots() -> &'static Mutex<Vec<(ReplayFileIdentity, OwnedFd)>> {
    static ROOTS: OnceLock<Mutex<Vec<(ReplayFileIdentity, OwnedFd)>>> = OnceLock::new();
    ROOTS.get_or_init(Default::default)
}

/// The real root of the replay guest process `pid` belongs to, as its
/// device and inode and a path descriptor: the replay whose root is the
/// process's own, or else the only replay this process runs, which a guest
/// that chrooted belongs to. `None` when no replay root is known here, or
/// several are and none is the process's own.
pub(super) fn replay_root_of(pid: Pid) -> Option<((u64, u64), OwnedFd)> {
    let own = replay_root_identity(pid).ok()?;
    let roots = replay_roots()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (identity, root) = roots
        .iter()
        .find(|(identity, _)| *identity == own)
        .or_else(|| (roots.len() == 1).then(|| &roots[0]))?;
    Some(((identity.device, identity.inode), root.try_clone().ok()?))
}

fn duplicate_controller_fd(fd: libc::c_int) -> io::Result<OwnedFd> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned by this process.
        Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
    }
}

fn register_materialized(root: ReplayFileIdentity, object: OwnedFd) -> io::Result<()> {
    let identity = identity_for_fd(object.as_raw_fd())?;
    materialized_files()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(root)
        .or_default()
        .entry(identity)
        .or_insert(object);
    Ok(())
}

fn materialized_file_is_registered(pid: Pid, metadata: &std::fs::Metadata) -> bool {
    let Ok(root) = replay_root_identity(pid) else {
        return false;
    };
    materialized_files()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&root)
        .is_some_and(|files| files.contains_key(&ReplayFileIdentity::from_metadata(metadata)))
}

fn unexpected_pidfd_getfd_fd(
    recorded: &Result<i64, Errno>,
    actual: &Result<i64, Errno>,
) -> Option<libc::c_int> {
    if recorded == actual {
        None
    } else {
        actual.as_ref().ok().map(|fd| *fd as libc::c_int)
    }
}

/// How replay refuses a recording in which an accepted connection holds a
/// different descriptor slot than it did during recording. Record runs
/// `accept` in the background and does not log where it took its slot among
/// other threads' descriptor calls, so replay can find the recorded slot taken
/// when it reaches the accept, or find that the accept's stand-in has taken a
/// slot that a later call recorded for another descriptor.
pub(super) const ACCEPT_SLOT_REFUSAL: &str = "replay refused: an accepted connection's \
    descriptor slot differs from the recording, because record does not log where a \
    backgrounded accept took its slot (https://github.com/rrnewton/hermit/issues/3880)";

/// How replay refuses a recorded `flock` whose descriptor it cannot
/// materialize beneath the replay root (dh-gg5uv): the descriptor was
/// served a placeholder at its recorded open and nothing since put a
/// real file beneath the root for it, so taking the lock for real is
/// impossible and returning the recorded result would falsely claim a
/// lock was acquired. Named like [`ACCEPT_SLOT_REFUSAL`] so downstream
/// tooling can tell this typed refusal from a generic panic.
pub(super) const FLOCK_OUTSIDE_REPLAY_ROOT_REFUSAL: &str = "replay refused: cannot replay flock side effects for a descriptor outside the \
     replay root that replay could not materialize; returning the recorded result \
     would falsely claim a lock was acquired";

/// The initial count of the eventfd that stands in for an accepted
/// connection. Every other replay placeholder starts at 0, and replay serves
/// every operation on a stand-in from the log, so this count never changes and
/// marks exactly the live stand-ins and their duplicates, across `fork` and
/// `dup`. An eventfd id would not do: Linux reuses a freed id at once, so a
/// later placeholder would carry a closed stand-in's id. `eventfd2` takes a
/// 32-bit initial value; a guest eventfd created with this value, or counted
/// up to it, in a diverged slot would read as a stand-in.
const ACCEPT_STAND_IN_COUNT: u32 = 0x3880_acc7;
/// The initial count of the eventfd that stands in for a descriptor an
/// `SCM_RIGHTS` receive installed. Detcore replaces it with a live duplicate
/// when the recording found the received file already in the receiver's
/// table (`Detcore::install_live_alias`); one that remains names a file this
/// process held nowhere else, which replay does not have. Replay serves every
/// operation on it from the log. No recorded eventfd operation names one:
/// Detcore records a received eventfd only as an alias of a descriptor the
/// receiving table holds, which replay makes live, and leaves any other
/// received anonymous inode but an inotify instance unrecorded. Nothing reads
/// the count back; it is distinctive only so that a stand-in can be told
/// apart from a guest eventfd in a replay's `/proc/<pid>/fdinfo`.
pub(super) const RECEIVED_STAND_IN_COUNT: u32 = 0x5c3d_f0d5;

/// The `eventfd-id` line of an fdinfo file: the kernel-wide identity
/// of one eventfd open file description, shared by its duplicates.
fn parse_eventfd_id(fdinfo: &str) -> Option<String> {
    fdinfo
        .lines()
        .find_map(|line| Some(line.strip_prefix("eventfd-id:")?.trim().to_owned()))
}

/// The `eventfd-id` of descriptor `fd` in thread `tid`'s table.
fn placeholder_eventfd_id(tid: Pid, fd: libc::c_int) -> Option<String> {
    let fdinfo = std::fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", tid.as_raw())).ok()?;
    parse_eventfd_id(&fdinfo)
}

/// The `eventfd-count` line of an fdinfo file, present for eventfds. Linux
/// prints the count in hexadecimal.
fn parse_eventfd_count(fdinfo: &str) -> Option<u64> {
    let count = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("eventfd-count:"))?;
    u64::from_str_radix(count.trim(), 16).ok()
}

/// How many times a replayed `close` is injected while a signal stops it
/// before it runs.
const PHYSICAL_CLOSE_RESTART_ATTEMPTS: u32 = 16;

/// Injects the physical `close` a replayed close needs for its descriptor
/// namespace effect. A signal can stop the injection before the syscall
/// instruction runs, which a backend reports as ERESTARTSYS and Linux's close
/// never returns, so only that is retried: a completed close that reported
/// EINTR or EIO has released the descriptor, and repeating it could close a
/// descriptor a later call reused. A close stopped that way on every attempt
/// ends the replay, because the recorded result says the descriptor is
/// released while its physical placeholder would stay open.
async fn inject_physical_close<T: Tool, G: Guest<T>>(
    guest: &mut G,
    syscall: Syscall,
) -> Result<i64, Errno> {
    for _ in 0..PHYSICAL_CLOSE_RESTART_ATTEMPTS {
        match guest.inject(syscall).await {
            Err(Errno::ERESTARTSYS) => continue,
            result => return result,
        }
    }
    panic!(
        "replayed {syscall:?} was stopped before it ran on {PHYSICAL_CLOSE_RESTART_ATTEMPTS} attempts"
    );
}

/// Whether `fd` in the table of thread `tid` holds a placeholder installed
/// for an accepted connection. `/proc/<tid>` shows that thread's own table,
/// which is not the leader's for a thread cloned without `CLONE_FILES`.
fn holds_accept_stand_in(tid: Pid, fd: libc::c_int) -> bool {
    std::fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", tid.as_raw()))
        .ok()
        .and_then(|fdinfo| parse_eventfd_count(&fdinfo))
        == Some(u64::from(ACCEPT_STAND_IN_COUNT))
}

/// Why a placeholder for slot `fd` landed in `placeholder` instead: the
/// named #3880 refusal when an accepted connection's stand-in holds the slot,
/// and the generic divergence otherwise.
fn slot_divergence_message(tid: Pid, fd: libc::c_int, placeholder: i64) -> String {
    if holds_accept_stand_in(tid, fd) {
        format!(
            "{ACCEPT_SLOT_REFUSAL}: slot {fd}, recorded for another descriptor, holds an \
             accepted connection's stand-in; lowest free {placeholder}"
        )
    } else {
        format!(
            "replay FD namespace diverged: expected slot {fd}, placeholder returned {placeholder}"
        )
    }
}

/// Whether descriptor `fd` of thread `tid` names an eventfd, which is what
/// `reserve_replay_fd` installs as a placeholder.
fn fd_is_eventfd(tid: Pid, fd: libc::c_int) -> bool {
    std::fs::read_link(format!("/proc/{}/fd/{fd}", tid.as_raw()))
        .is_ok_and(|target| target == std::path::Path::new("anon_inode:[eventfd]"))
}

/// Whether descriptor `fd` of thread `tid` is an `AF_UNIX` socket: its socket
/// inode is listed in the `/proc/<tid>/net/unix` table of the thread's network
/// namespace.
fn fd_is_unix_socket(tid: Pid, fd: libc::c_int) -> io::Result<bool> {
    let target = std::fs::read_link(format!("/proc/{}/fd/{fd}", tid.as_raw()))?;
    let Some(inode) = socket_inode(&target) else {
        return Ok(false);
    };
    // The table lists every Unix socket in the network namespace, and a
    // socket's path can be any bytes, so it is not read as UTF-8.
    let table = std::fs::read(format!("/proc/{}/net/unix", tid.as_raw()))?;
    Ok(unix_table_lists_inode(&table, inode))
}

/// The inode of a `socket:[<inode>]` descriptor link.
fn socket_inode(target: &std::path::Path) -> Option<u64> {
    target
        .to_str()?
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// Whether a `/proc/net/unix` table has a row for `inode`, its seventh column.
fn unix_table_lists_inode(table: &[u8], inode: u64) -> bool {
    String::from_utf8_lossy(table).lines().skip(1).any(|row| {
        row.split_whitespace()
            .nth(6)
            .and_then(|column| column.parse::<u64>().ok())
            == Some(inode)
    })
}

/// Whether the regular file behind a guest descriptor lives beneath the
/// guest's replay root, judged by directory ancestry rather than path text
/// (the guest may live in a mount namespace the replayer cannot name).
///
/// Only a pathname descriptor qualifies: its procfs link is absolute. A
/// placeholder's link (`anon_inode:[eventfd]`, `pipe:[…]`, `socket:[…]`) is
/// slashless, its parent is the empty path, and the host path built below
/// would name the replay root itself, which is trivially beneath the root.
/// Without this guard, `handle_flock` admitted such a placeholder and
/// locked the shared `anon_inode` instead of the recorded object, so
/// distinct recorded objects falsely contended (dh-gg5uv review).
fn file_is_beneath_replay_root(pid: Pid, fd: libc::c_int) -> bool {
    let Ok(link) = std::fs::read_link(format!("/proc/{}/fd/{fd}", pid.as_raw())) else {
        return false;
    };
    let link = link.to_string_lossy().into_owned();
    let link = link.strip_suffix(" (deleted)").unwrap_or(&link);
    if !std::path::Path::new(link).is_absolute() {
        return false;
    }
    let Some(parent) = std::path::Path::new(link).parent() else {
        return false;
    };
    let host_parent = format!("/proc/{}/root{}", pid.as_raw(), parent.display());
    let (Ok(root), Ok(directory)) = (
        crate::record_replay_path::open_process_root(pid),
        std::fs::File::open(&host_parent).map(std::os::fd::OwnedFd::from),
    ) else {
        return false;
    };
    crate::record_replay_path::directory_is_beneath(&root, &directory).unwrap_or(false)
}

fn remember_materialized_file(pid: Pid, fd: libc::c_int) {
    let Ok(metadata) = std::fs::metadata(format!("/proc/{}/fd/{fd}", pid.as_raw())) else {
        return;
    };
    // FIFOs register beside regular files: the registry is keyed by object
    // identity (root, device, inode), which is exactly the identity a
    // later `flock`, `fchmod`, or `lseek` on any descriptor for the same
    // object must be judged by. A per-thread set of bare descriptor
    // numbers could not do that — it stayed stale across close/reuse and
    // disagreed across `CLONE_FILES` threads (dh-gg5uv review).
    if metadata.file_type().is_file() || metadata.file_type().is_fifo() {
        let Ok(root) = replay_root_identity(pid) else {
            return;
        };
        let Ok(object) = crate::record_replay_path::open_process_fd(pid, fd) else {
            return;
        };
        let _ = register_materialized(root, object);
    }
}

pub fn remember_materialized_object(root_fd: libc::c_int, fd: libc::c_int) -> io::Result<()> {
    let metadata = std::fs::metadata(format!("/proc/self/fd/{fd}"))?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "exec materialization did not resolve to a regular file",
        ));
    }
    register_materialized(identity_for_fd(root_fd)?, duplicate_controller_fd(fd)?)
}

/// How many materialized objects are registered under ROOT_FD. Test support
/// only, for the replay tests here and in the hermit library.
#[doc(hidden)]
pub fn registered_materialized_count(root_fd: libc::c_int) -> usize {
    let Ok(root) = identity_for_fd(root_fd) else {
        return 0;
    };
    materialized_files()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&root)
        .map_or(0, BTreeMap::len)
}

fn remember_materialized_path(
    root: &std::os::fd::OwnedFd,
    start: &std::os::fd::OwnedFd,
    path: &Path,
) -> io::Result<()> {
    let resolved = crate::record_replay_path::resolve_existing_path(root, start, path, false)?;
    remember_materialized_object(root.as_raw_fd(), resolved.object.as_raw_fd())
}

use crate::desync::DesyncError;
use crate::event::InterruptedPoll;
use crate::event::OpenMaterialization;
use crate::event_stream::ChildEventStreamIds;
use crate::event_stream::DebugEvent;
use crate::event_stream::EventReader;
use crate::event_stream::EventStreamId;
use crate::event_stream::normalize_unused_args;

#[derive(Serialize, Deserialize)]
pub struct ReplayerThreadState {
    events: EventReader,
    /// The poll the recording says a signal just ended in the kernel, until
    /// this thread's next syscall (`InterruptedPoll`).
    #[serde(default)]
    interrupted_poll: Option<InterruptedPoll>,
    stream_id: EventStreamId,
    child_stream_ids: ChildEventStreamIds,
    bootstrapped: bool,
    /// The root guest's captured output endpoints (`ReplayOutputs`), which
    /// every thread inherits from the thread that created it.
    #[serde(skip)]
    outputs: std::sync::Arc<ReplayOutputs>,
}

/// A placeholder descriptor reserved for a recorded open, with the
/// request that may still materialize it at a following `flock`.
///
/// Entries live in a process-wide registry, not in thread state:
/// descriptor tables are shared across `CLONE_FILES` threads and copied
/// at `fork`, so any thread naming the descriptor must invalidate the
/// entry, and the entry may be spent only once even where two tables
/// inherited the placeholder (dh-gg5uv review). `tgid` is the opening
/// thread group; a descendant that inherited the descriptor may spend
/// the entry (a shell handing its descriptor to a `flock` helper), but
/// an unrelated process reusing the same descriptor number may not.
/// `parent`/`object` pin what `(dirfd, path)` resolved to at recorded-
/// open time, by device and inode, so re-resolution at `flock` time
/// cannot quietly materialize a different object after a `chdir`,
/// rename, unlink, symlink swap, or `dirfd` reuse; a pin that could
/// not be taken refuses by name at materialize time where the check
/// needs it, rather than skipping the check. `open_id` is one
/// recorded open's identity across its dup aliases: spending any
/// alias's entry retires them all, because one recorded open has one
/// open file description and materializing two aliases separately
/// would invent two lock domains for it.
#[derive(Clone, Debug)]
struct PendingPlaceholderOpen {
    open_id: u64,
    root: ReplayFileIdentity,
    tgid: libc::pid_t,
    opener_tid: libc::pid_t,
    fd: libc::c_int,
    dirfd: libc::c_int,
    path: PathBuf,
    flags: OFlag,
    mode: Option<reverie::syscalls::Mode>,
    parent: Option<ReplayFileIdentity>,
    object: Option<ReplayFileIdentity>,
    /// The placeholder eventfd's own identity (`eventfd-id` from its
    /// fdinfo), pinned at reservation. All eventfds share one anon
    /// inode, so device/inode cannot tell two placeholders apart; the
    /// id can, and it is what keeps a split-table sibling (same
    /// thread group, private descriptor table) from spending this
    /// entry on its own placeholder at the same number. A freed id is
    /// reused at once, but the close/dup2 that frees a placeholder
    /// also retires its entry by descriptor number first.
    placeholder_id: Option<String>,
}

static PENDING_PLACEHOLDER_OPENS: OnceLock<Mutex<Vec<PendingPlaceholderOpen>>> = OnceLock::new();

/// Sequence for `PendingPlaceholderOpen::open_id`: unique per recorded
/// open within this replay process, shared by its dup-alias clones.
static PENDING_OPEN_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn pending_placeholder_opens() -> &'static Mutex<Vec<PendingPlaceholderOpen>> {
    PENDING_PLACEHOLDER_OPENS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Remembers one placeholder-answered recorded open for a following
/// `flock`, replacing only an entry for the very same placeholder. Entries for other descriptors stay: a second
/// recorded open must not displace the first one's pending request.
fn remember_pending_placeholder_open(pending: PendingPlaceholderOpen) {
    let mut registry = pending_placeholder_opens()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Re-remembering the same placeholder replaces its entry; a
    // different placeholder at the same number in a split descriptor
    // table (same thread group, another thread's own table) must not
    // evict it, or that thread's flock could spend this entry on the
    // wrong file.
    registry.retain(|existing| {
        !(existing.root == pending.root
            && existing.opener_tid == pending.opener_tid
            && existing.fd == pending.fd
            && existing.placeholder_id == pending.placeholder_id)
    });
    registry.push(pending);
}

/// The thread group and parent process of a guest thread, from its
/// status file. `guest.pid()` is the group leader, whose descriptor
/// table is not the calling thread's when it was made without
/// `CLONE_FILES`, so pending entries are judged against the calling
/// thread (`guest.tid()`), not the leader.
fn guest_tgid_ppid(tid: Pid) -> Option<(libc::pid_t, libc::pid_t)> {
    let status = std::fs::read_to_string(format!("/proc/{}/status", tid.as_raw())).ok()?;
    let (mut tgid, mut ppid) = (None, None);
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Tgid:") {
            tgid = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("PPid:") {
            ppid = rest.trim().parse().ok();
        }
    }
    Some((tgid?, ppid?))
}

/// Whether thread group `ancestor` is the thread group of `tid` or one
/// of its process ancestors: the groups whose descriptor this thread
/// could have inherited the pending placeholder from.
fn tgid_is_self_or_ancestor(ancestor: libc::pid_t, tid: Pid) -> bool {
    let Some((tgid, mut ppid)) = guest_tgid_ppid(tid) else {
        return false;
    };
    if tgid == ancestor {
        return true;
    }
    // A ppid names the parent's thread-group leader. The walk is
    // bounded so a cyclic or vanished ancestry just fails closed.
    for _ in 0..16 {
        if ppid == ancestor {
            return true;
        }
        if ppid <= 1 {
            return false;
        }
        let Some((_, next)) = guest_tgid_ppid(Pid::from_raw(ppid)) else {
            return false;
        };
        ppid = next;
    }
    false
}

/// Takes the pending entry for `fd` that thread `tid` may spend: the
/// opening thread's own first, then its thread group's, else an
/// ancestor's it inherited. The opening-thread preference matters
/// where a thread group has split descriptor tables (a thread made
/// without `CLONE_FILES` holds its own file at the same number):
/// exact `tgid` alone would let such a thread spend a sibling's entry
/// and materialize over its own real descriptor. Taking removes the
/// whole family — every dup-alias entry of the same recorded open —
/// so a fork sibling or alias descriptor cannot materialize a
/// second, divergent object for the one recorded open; its recorded
/// `flock` then refuses by name instead of locking a different open
/// file description than the recording's.
fn take_pending_placeholder_open(tid: Pid, fd: libc::c_int) -> Option<PendingPlaceholderOpen> {
    let root = replay_root_identity(tid).ok()?;
    let (tgid, _) = guest_tgid_ppid(tid)?;
    // Cross-table tiers (thread group, ancestor) must prove this
    // descriptor is the very placeholder the entry was remembered
    // for, by its eventfd id: a split-table sibling's own placeholder
    // at the same number has a different id, and spending its entry
    // here would lock the sibling's file. The opening thread's own
    // entry may spend when no id could be pinned (its table is the
    // one the reservation landed in).
    let current_id = placeholder_eventfd_id(tid, fd);
    let id_matches = |pending: &PendingPlaceholderOpen| {
        pending.placeholder_id.is_some() && pending.placeholder_id == current_id
    };
    let mut registry = pending_placeholder_opens()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = registry
        .iter()
        .position(|pending| {
            pending.root == root
                && pending.opener_tid == tid.as_raw()
                && pending.fd == fd
                && (pending.placeholder_id.is_none() || pending.placeholder_id == current_id)
        })
        .or_else(|| {
            registry.iter().position(|pending| {
                pending.root == root
                    && pending.tgid == tgid
                    && pending.fd == fd
                    && id_matches(pending)
            })
        })
        .or_else(|| {
            registry.iter().position(|pending| {
                pending.root == root
                    && pending.fd == fd
                    && id_matches(pending)
                    && tgid_is_self_or_ancestor(pending.tgid, tid)
            })
        })?;
    let taken = registry.remove(index);
    registry.retain(|pending| pending.open_id != taken.open_id);
    Some(taken)
}

/// Whether descriptor `fd` of thread `tid` is a plain replay
/// placeholder: an eventfd still at the initial count `reserve_replay_fd`
/// installs (0), as opposed to an accepted connection's or received
/// descriptor's stand-in, which carry their own counts. Only a plain
/// placeholder may be swapped for a pending entry's real object;
/// anything else at that number is a real descriptor (or another
/// stand-in) that a pending spend must not clobber.
fn fd_is_plain_placeholder(tid: Pid, fd: libc::c_int) -> bool {
    if !fd_is_eventfd(tid, fd) {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", tid.as_raw())) {
        Ok(fdinfo) => parse_eventfd_count(&fdinfo) == Some(0),
        // Unreadable fdinfo proves nothing about this descriptor, and
        // this guard exists to keep a spend from clobbering a real
        // descriptor: fail closed.
        Err(_) => false,
    }
}

/// Clones the pending entry for `oldfd` onto `newfd` after a recorded
/// dup produced `newfd`: a shell opens its lock file onto the lowest
/// free descriptor, then dups it onto the high descriptor it hands to
/// `flock` and closes the original, so the pending open must travel
/// with the placeholder or that shape could never materialize. The
/// original entry stays while its descriptor stays open (aliases of
/// one placeholder), and `take_pending_placeholder_open` still spends
/// an entry exactly once. Only entries the dup'ing thread could itself
/// spend are cloned, under the new descriptor's close-on-exec bit.
fn follow_pending_through_dup(tid: Pid, oldfd: libc::c_int, newfd: libc::c_int, cloexec: bool) {
    if oldfd == newfd {
        return;
    }
    let Ok(root) = replay_root_identity(tid) else {
        return;
    };
    let clones: Vec<PendingPlaceholderOpen> = {
        let registry = pending_placeholder_opens()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry
            .iter()
            .filter(|pending| {
                pending.root == root
                    && pending.fd == oldfd
                    && tgid_is_self_or_ancestor(pending.tgid, tid)
            })
            .map(|pending| {
                let mut clone = pending.clone();
                clone.fd = newfd;
                clone.flags.set(OFlag::O_CLOEXEC, cloexec);
                clone
            })
            .collect()
    };
    for clone in clones {
        remember_pending_placeholder_open(clone);
    }
}

/// What `(dirfd, path)` resolves to for thread `tid` right now, as
/// identities: the containing directory and, when it exists, the object
/// itself. Resolution is descriptor-based without procfs symlinks
/// (`resolve_existing_path_without_procfs_symlinks`), starting from the
/// calling thread's own root/cwd/dirfd descriptors, so a path crossing a
/// procfs magic link cannot pin an in-root name to an out-of-root
/// object, and a thread with its own descriptor table pins what *it*
/// sees, not its leader's view.
struct PendingOpenPins {
    parent: Option<ReplayFileIdentity>,
    object: Option<ReplayFileIdentity>,
    object_is_fifo: bool,
}

fn pin_pending_open_object(tid: Pid, dirfd: libc::c_int, path: &Path) -> PendingOpenPins {
    let missing = PendingOpenPins {
        parent: None,
        object: None,
        object_is_fifo: false,
    };
    let Ok(root) = crate::record_replay_path::open_process_root(tid) else {
        return missing;
    };
    let start = if path.is_absolute() {
        crate::record_replay_path::open_process_root(tid)
    } else if dirfd == libc::AT_FDCWD {
        crate::record_replay_path::open_process_cwd(tid)
    } else {
        crate::record_replay_path::open_process_directory_fd(tid, dirfd)
    };
    let Ok(start) = start else {
        return missing;
    };
    let parent = match path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => crate::record_replay_path::resolve_existing_path_without_procfs_symlinks(
            &root, &start, parent,
        )
        .ok()
        .and_then(|resolved| identity_for_fd(resolved.object.as_raw_fd()).ok()),
        None => identity_for_fd(start.as_raw_fd()).ok(),
    };
    let (object, object_is_fifo) =
        match crate::record_replay_path::resolve_existing_path_without_procfs_symlinks(
            &root, &start, path,
        ) {
            Ok(resolved) => {
                let is_fifo =
                    std::fs::metadata(format!("/proc/self/fd/{}", resolved.object.as_raw_fd()))
                        .is_ok_and(|metadata| metadata.file_type().is_fifo());
                (identity_for_fd(resolved.object.as_raw_fd()).ok(), is_fifo)
            }
            Err(_) => (None, false),
        };
    PendingOpenPins {
        parent,
        object,
        object_is_fifo,
    }
}

/// The absolute guest path a recorded relative open named for the
/// opening thread, right now: the recorded path joined onto the
/// guest-visible name of its base directory (the thread's cwd for
/// `AT_FDCWD`, the directory behind `dirfd` otherwise). A pending
/// entry must remember this form, not the raw `(dirfd, path)` pair:
/// by the recorded `flock`, the guest may have restored another cwd
/// (the JVM's secure-open shape does exactly that after its
/// `openat(AT_FDCWD, "9")` inside the hsperf directory and before
/// the flock) or closed the dirfd, and re-resolving the raw pair then
/// names the wrong directory - the dh-gg5uv landing-validation
/// refusal. Resolving the joined absolute path from the replay
/// root at materialization names the same object the recorded open
/// named. `None` when the base directory cannot be named in guest
/// terms (deleted, or outside the thread's root); the caller keeps
/// the raw pair then and accepts today's behaviour for that open.
fn guest_absolute_open_path(tid: Pid, dirfd: libc::c_int, path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return None;
    }
    let base = if dirfd == libc::AT_FDCWD {
        crate::record_replay_path::process_cwd_path(tid).ok()?
    } else {
        crate::record_replay_path::process_directory_fd_path(tid, dirfd).ok()?
    };
    Some(base.join(path))
}

/// Builds the registry entry for one placeholder-answered recorded
/// open, pinning `(dirfd, path)` as it resolves for the opening thread.
/// Returns `None` when the opening thread's root or thread group
/// cannot be identified; the recorded `flock` then refuses by name
/// rather than materializing on a guess.
fn pending_placeholder_open(
    tid: Pid,
    fd: libc::c_int,
    dirfd: libc::c_int,
    path: PathBuf,
    flags: OFlag,
    mode: Option<reverie::syscalls::Mode>,
) -> Option<PendingPlaceholderOpen> {
    // Remember the recorded open under the absolute guest path it
    // named for this thread, so a later cwd restore or dirfd close
    // cannot move its target. The pins are taken against that same
    // stored form, matching what materialization will resolve.
    let (dirfd, path) = match guest_absolute_open_path(tid, dirfd, &path) {
        Some(absolute) => (libc::AT_FDCWD, absolute),
        None => (dirfd, path),
    };
    let pins = pin_pending_open_object(tid, dirfd, &path);
    Some(PendingPlaceholderOpen {
        open_id: PENDING_OPEN_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        root: replay_root_identity(tid).ok()?,
        tgid: guest_tgid_ppid(tid)?.0,
        opener_tid: tid.as_raw(),
        fd,
        dirfd,
        path,
        flags,
        mode,
        parent: pins.parent,
        object: pins.object,
        placeholder_id: placeholder_eventfd_id(tid, fd),
    })
}

/// Whether `syscall` may have operated on descriptor `fd`, retiring a
/// pending placeholder for it: after such a call the placeholder may
/// have served as the descriptor, so its recorded open may no longer
/// be materialized late at a `flock`.
///
/// This is deliberately inverted from a "names the descriptor" list
/// (dh-gg5uv review): a syscall keeps the pending entry only when it
/// is proven not to touch `fd`. Syscalls whose descriptor arguments
/// are decoded below (including the second descriptor of the
/// splice family, `epoll_ctl`'s target, `pidfd_getfd`'s target, and
/// the `dirfd` arguments of the `*at` family) clear only on a match;
/// syscalls that take descriptors from guest memory (`poll`/`select`
/// arrays, io_uring submission entries, AIO control blocks,
/// `SCM_RIGHTS` in the message calls, `bpf` unions) clear always, as
/// do numbers this function does not classify. Erring wide only loses
/// a materialization, which the named refusal then states; erring
/// narrow would lock a placeholder or an object whose recorded I/O
/// was served virtually. Path-only calls keep the entry: what they
/// change is caught by the identity pins when the `flock` arrives.
/// `flock` itself never clears here, so `handle_flock` can spend the
/// entry its own descriptor named.
fn syscall_clears_pending(syscall: Syscall, fd: libc::c_int) -> bool {
    // Pure queries leave no state on the open file description for a
    // late materialization to miss: replay answers them from the
    // recording, and the object the descriptor names is unchanged.
    // (A shell, for one, fstats its freshly opened lock file before
    // dup'ing it onto the descriptor it hands to `flock`.)
    match syscall {
        Syscall::Fstat(_)
        | Syscall::Fstatfs(_)
        | Syscall::Fgetxattr(_)
        | Syscall::Flistxattr(_) => {
            return false;
        }
        Syscall::Fcntl(call) => match call.cmd() {
            FcntlCmd::F_GETFD | FcntlCmd::F_GETFL => return false,
            // A dup's source keeps its entry; the dup handlers clone
            // it onto the descriptor the recording actually produced.
            FcntlCmd::F_DUPFD(_) | FcntlCmd::F_DUPFD_CLOEXEC(_) => return false,
            _ => return call.fd() == fd,
        },
        _ => {}
    }
    if let Syscall::Mmap(call) = syscall {
        return call.fd() == fd;
    }
    // A dup2/dup3 retires only an entry for the descriptor it
    // replaces; the source entry is cloned onto the new descriptor by
    // the dup handlers once the recorded dup is known to have
    // succeeded, so a shell's open-then-dup-to-9 shape reaches its
    // flock with the pending open still attached to descriptor 9.
    // x86_64 only: aarch64 has no dup2 (dup3 below serves it).
    #[cfg(target_arch = "x86_64")]
    if let Syscall::Dup2(call) = syscall {
        return call.newfd() == fd && call.oldfd() != call.newfd();
    }
    if let Syscall::Dup3(call) = syscall {
        return call.newfd() == fd && call.oldfd() != call.newfd();
    }
    let (number, args) = syscall.into_parts();
    let arg = |value: usize| value as libc::c_int == fd;
    if number == Sysno::close_range {
        // close_range takes unsigned bounds: ~0U is the usual "to the
        // end" last bound, whose cast to c_int used to make the range
        // empty and leave closed descriptors pending (dh-gg5uv review).
        let in_range = fd >= 0 && (args.arg0 as u32..=args.arg1 as u32).contains(&(fd as u32));
        // CLOSE_RANGE_CLOEXEC only flags the descriptors, but retiring
        // the entry is the conservative reading: an exec between here
        // and the flock would close it, and this function does not
        // track the flag onto the entry.
        return in_range;
    }
    // The flock this pending entry exists for; handle_flock spends it.
    if number == Sysno::flock {
        return false;
    }
    // Descriptors decoded from the arguments (first listed position is
    // the one the kernel reads the descriptor from).
    match number {
        // One descriptor argument, in arg0: plain I/O and per-file
        // operations, socket calls on one socket, and the *at family
        // whose arg0 is the base directory (using the placeholder as a
        // directory base is operating on it).
        Sysno::read
        | Sysno::write
        | Sysno::close
        | Sysno::lseek
        | Sysno::pread64
        | Sysno::pwrite64
        | Sysno::readv
        | Sysno::writev
        | Sysno::preadv
        | Sysno::pwritev
        | Sysno::preadv2
        | Sysno::pwritev2
        | Sysno::fcntl
        | Sysno::ftruncate
        | Sysno::fsync
        | Sysno::fdatasync
        | Sysno::readahead
        | Sysno::fallocate
        | Sysno::fadvise64
        | Sysno::fstat
        | Sysno::fstatfs
        | Sysno::ioctl
        | Sysno::getdents64
        | Sysno::fchmod
        | Sysno::fchown
        | Sysno::fchdir
        | Sysno::fsetxattr
        | Sysno::fremovexattr
        | Sysno::syncfs
        | Sysno::epoll_pwait
        | Sysno::epoll_pwait2
        | Sysno::accept
        | Sysno::accept4
        | Sysno::connect
        | Sysno::bind
        | Sysno::listen
        | Sysno::getsockname
        | Sysno::getpeername
        | Sysno::setsockopt
        | Sysno::getsockopt
        | Sysno::shutdown
        | Sysno::sendto
        | Sysno::recvfrom
        | Sysno::vmsplice
        | Sysno::inotify_add_watch
        | Sysno::inotify_rm_watch
        | Sysno::signalfd4
        | Sysno::timerfd_settime
        | Sysno::timerfd_gettime
        | Sysno::pidfd_send_signal
        | Sysno::openat
        | Sysno::openat2
        | Sysno::unlinkat
        | Sysno::mkdirat
        | Sysno::mknodat
        | Sysno::fchownat
        | Sysno::fchmodat
        | Sysno::fchmodat2
        | Sysno::readlinkat
        | Sysno::faccessat
        | Sysno::faccessat2
        | Sysno::utimensat
        | Sysno::execveat
        | Sysno::statx
        | Sysno::name_to_handle_at
        | Sysno::open_by_handle_at => arg(args.arg0),
        // x86_64 only: aarch64 lacks these numbers entirely.
        #[cfg(target_arch = "x86_64")]
        Sysno::sync_file_range | Sysno::getdents | Sysno::epoll_wait | Sysno::signalfd => {
            arg(args.arg0)
        }
        // aarch64 number 84 is sys_sync_file_range (descriptor in arg0).
        #[cfg(target_arch = "aarch64")]
        Sysno::sync_file_range2 => arg(args.arg0),
        #[cfg(target_arch = "x86_64")]
        Sysno::newfstatat | Sysno::futimesat => arg(args.arg0),
        #[cfg(target_arch = "aarch64")]
        Sysno::fstatat => arg(args.arg0),
        // Two descriptor arguments.
        Sysno::sendfile => arg(args.arg0) || arg(args.arg1),
        Sysno::splice | Sysno::copy_file_range => arg(args.arg0) || arg(args.arg2),
        Sysno::tee => arg(args.arg0) || arg(args.arg1),
        Sysno::epoll_ctl => arg(args.arg0) || arg(args.arg2),
        Sysno::renameat | Sysno::renameat2 | Sysno::linkat => arg(args.arg0) || arg(args.arg2),
        Sysno::symlinkat => arg(args.arg1),
        Sysno::pidfd_getfd => arg(args.arg0) || arg(args.arg1),
        Sysno::fanotify_mark => arg(args.arg0) || arg(args.arg3),
        Sysno::perf_event_open => arg(args.arg3),
        Sysno::kcmp => arg(args.arg3) || arg(args.arg4),
        // Descriptor lists or ancillary data in guest memory, or a ring
        // whose entries name descriptors: undecodable here, always clear.
        Sysno::ppoll
        | Sysno::pselect6
        | Sysno::io_uring_enter
        | Sysno::io_uring_register
        | Sysno::io_submit
        | Sysno::sendmsg
        | Sysno::recvmsg
        | Sysno::sendmmsg
        | Sysno::recvmmsg
        | Sysno::bpf => true,
        // x86_64 only: aarch64 has ppoll/pselect6 alone.
        #[cfg(target_arch = "x86_64")]
        Sysno::poll | Sysno::select => true,
        // Proven to take no descriptor argument: identity/time/signal/
        // memory bookkeeping, process lifecycle (a fork and exec between
        // the open and the flock is the shell-to-helper shape; the
        // close-on-exec retirement runs separately in the dispatch
        // prologue), pathname-only calls, and descriptor *creators*,
        // which cannot take the pending slot while its placeholder
        // occupies it.
        Sysno::getpid
        | Sysno::gettid
        | Sysno::getppid
        | Sysno::getuid
        | Sysno::geteuid
        | Sysno::getgid
        | Sysno::getegid
        | Sysno::getsid
        | Sysno::getpgid
        | Sysno::setpgid
        | Sysno::setsid
        | Sysno::getgroups
        | Sysno::setgroups
        | Sysno::setuid
        | Sysno::setgid
        | Sysno::setreuid
        | Sysno::setregid
        | Sysno::setresuid
        | Sysno::setresgid
        | Sysno::getresuid
        | Sysno::getresgid
        | Sysno::times
        | Sysno::clock_gettime
        | Sysno::clock_getres
        | Sysno::clock_settime
        | Sysno::gettimeofday
        | Sysno::settimeofday
        | Sysno::sysinfo
        | Sysno::uname
        | Sysno::getrandom
        | Sysno::brk
        | Sysno::munmap
        | Sysno::mprotect
        | Sysno::mremap
        | Sysno::msync
        | Sysno::mincore
        | Sysno::madvise
        | Sysno::mlock
        | Sysno::mlock2
        | Sysno::mlockall
        | Sysno::munlock
        | Sysno::munlockall
        | Sysno::set_tid_address
        | Sysno::set_robust_list
        | Sysno::get_robust_list
        | Sysno::rseq
        | Sysno::futex
        | Sysno::nanosleep
        | Sysno::clock_nanosleep
        | Sysno::rt_sigaction
        | Sysno::rt_sigprocmask
        | Sysno::rt_sigpending
        | Sysno::rt_sigreturn
        | Sysno::rt_sigsuspend
        | Sysno::rt_sigtimedwait
        | Sysno::sigaltstack
        | Sysno::sched_yield
        | Sysno::sched_getaffinity
        | Sysno::sched_setaffinity
        | Sysno::sched_getparam
        | Sysno::sched_setparam
        | Sysno::sched_getscheduler
        | Sysno::sched_setscheduler
        | Sysno::sched_rr_get_interval
        | Sysno::sched_getattr
        | Sysno::sched_setattr
        | Sysno::getcpu
        | Sysno::getrlimit
        | Sysno::setrlimit
        | Sysno::prlimit64
        | Sysno::getrusage
        | Sysno::membarrier
        | Sysno::clone
        | Sysno::clone3
        | Sysno::execve
        | Sysno::wait4
        | Sysno::waitid
        | Sysno::exit
        | Sysno::exit_group
        | Sysno::kill
        | Sysno::tkill
        | Sysno::tgkill
        | Sysno::getcwd
        | Sysno::chdir
        | Sysno::umask
        | Sysno::statfs
        | Sysno::truncate
        | Sysno::setxattr
        | Sysno::lsetxattr
        | Sysno::getxattr
        | Sysno::lgetxattr
        | Sysno::listxattr
        | Sysno::llistxattr
        | Sysno::removexattr
        | Sysno::lremovexattr
        | Sysno::dup
        | Sysno::socket
        | Sysno::socketpair
        | Sysno::pipe2
        | Sysno::eventfd2
        | Sysno::epoll_create1
        | Sysno::timerfd_create
        | Sysno::inotify_init1
        | Sysno::io_uring_setup
        | Sysno::memfd_create
        | Sysno::userfaultfd
        | Sysno::pidfd_open => false,
        // x86_64 only: aarch64 lacks these numbers (its libc uses the
        // *at / newer forms, which are classified above).
        #[cfg(target_arch = "x86_64")]
        Sysno::getpgrp
        | Sysno::time
        | Sysno::arch_prctl
        | Sysno::pause
        | Sysno::fork
        | Sysno::vfork
        | Sysno::open
        | Sysno::creat
        | Sysno::stat
        | Sysno::lstat
        | Sysno::access
        | Sysno::readlink
        | Sysno::mkdir
        | Sysno::rmdir
        | Sysno::unlink
        | Sysno::rename
        | Sysno::link
        | Sysno::symlink
        | Sysno::chmod
        | Sysno::chown
        | Sysno::lchown
        | Sysno::mknod
        | Sysno::utime
        | Sysno::utimes
        | Sysno::pipe
        | Sysno::eventfd
        | Sysno::epoll_create
        | Sysno::inotify_init => false,
        // Anything unclassified: not proven descriptor-free.
        _ => true,
    }
}

/// Duplicates of the stdout and stderr the root guest inherited, the only
/// endpoints whose writes the recorder captures as output (`ContainerOutputs`
/// in recorder.rs), with the locks that order replayed writes to each.
///
/// Every process's `Replayer::new` duplicates that process's own descriptors 1
/// and 2 when it is created, which in a child forked after its parent
/// redirected its stdout are a pipe inside the container. Replay used to emit a
/// captured write into the writing process's own duplicate, so a child that
/// wrote to the container's stdout through a descriptor its parent saved had
/// its bytes written into that pipe instead, whose reader replay serves from
/// the recording; once the pipe was full, the replayed write waited forever
/// (<https://github.com/rrnewton/hermit/issues/4006>). Only the root thread's
/// copy is used now, and every other thread shares it.
#[derive(Default)]
pub struct ReplayOutputs {
    pub stdout: Option<std::os::fd::OwnedFd>,
    pub stderr: Option<std::os::fd::OwnedFd>,
    /// Preserve replayed write ordering independently for each captured stream.
    pub stdout_output_lock: tokio::sync::Mutex<()>,
    pub stderr_output_lock: tokio::sync::Mutex<()>,
    pub stdout_error: Option<String>,
    pub stderr_error: Option<String>,
}

impl ReplayOutputs {
    fn capture(pid: Pid) -> Self {
        let (stdout, stdout_error) = capture_guest_fd(pid, libc::STDOUT_FILENO);
        let (stderr, stderr_error) = capture_guest_fd(pid, libc::STDERR_FILENO);
        Self {
            stdout,
            stderr,
            stdout_output_lock: tokio::sync::Mutex::new(()),
            stderr_output_lock: tokio::sync::Mutex::new(()),
            stdout_error,
            stderr_error,
        }
    }
}

impl Deref for ReplayerThreadState {
    type Target = EventReader;

    fn deref(&self) -> &Self::Target {
        &self.events
    }
}

impl DerefMut for ReplayerThreadState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.events
    }
}

impl Default for ReplayerThreadState {
    fn default() -> Self {
        panic!("thread state should be explicitly initialized in init_thread_state")
    }
}

/// A Reverie tool that replays syscalls. Note that only syscalls that cannot be
/// made deterministic are forwarded to this tool.
#[derive(Serialize, Deserialize)]
pub struct Replayer {
    // Keep track of the data directory. Each thread uses this path to open its
    // event stream.
    data: PathBuf,
    /// Duplicates of this process's descriptors 1 and 2 when it was created.
    /// Only the root guest's are the captured output endpoints, so only the
    /// root thread takes them (`ReplayerThreadState::outputs`).
    #[serde(skip)]
    outputs: std::sync::Arc<ReplayOutputs>,
}

impl Default for Replayer {
    fn default() -> Self {
        Self {
            data: PathBuf::new(),
            outputs: Default::default(),
        }
    }
}

#[reverie::tool]
impl Tool for Replayer {
    fn stream_role() -> reverie::ToolStreamRole {
        reverie::ToolStreamRole::BinaryReplay
    }

    type GlobalState = detcore::GlobalState;
    type ThreadState = ReplayerThreadState;

    fn new(pid: Pid, cfg: &<Self::GlobalState as GlobalTool>::Config) -> Self {
        Self {
            data: cfg.replay_data.as_ref().unwrap().clone(),
            outputs: std::sync::Arc::new(ReplayOutputs::capture(pid)),
        }
    }

    fn init_thread_state(
        &self,
        child: Tid,
        parent: Option<(Tid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        let stream_id = match parent {
            None => EventStreamId::root(),
            Some((_, state)) => state.child_stream_ids.next(&state.stream_id),
        };
        // Its mappings keep shared files' backings alive.
        mmap::track_task(child.as_raw());
        // We have to unwrap because there is no way to handle errors here.
        ReplayerThreadState {
            events: EventReader::open(&self.data, &stream_id).unwrap_or_else(|err| {
                panic!(
                    "Failed to open {:?} for replay thread {} (physical {}): {}",
                    self.data, stream_id, child, err
                )
            }),
            stream_id,
            child_stream_ids: ChildEventStreamIds::default(),
            interrupted_poll: None,
            bootstrapped: parent.is_some_and(|(_, state)| state.bootstrapped),
            outputs: match parent {
                None => self.outputs.clone(),
                Some((_, state)) => state.outputs.clone(),
            },
        }
    }

    fn subscriptions(_config: &<Self::GlobalState as GlobalTool>::Config) -> Subscription {
        // Subscribe to the exact same events as the recorder does.
        hermit_record_format::subscriptions::record_and_replay()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        self.expect_syscall(guest, syscall);

        // A placeholder open is materializable only by a following
        // flock, and only until some other syscall may have operated
        // on its descriptor (which replay may have served virtually
        // on the placeholder). The registry is process-wide, so a
        // sibling thread sharing the descriptor table (CLONE_FILES)
        // or a fork sibling retires the entry exactly as the opening
        // thread would; clearing is keyed by descriptor number across
        // entries, which at worst loses another process's
        // materialization to the named refusal, never locks wrongly.
        // Proven descriptor-free syscalls — including a fork and exec
        // between the open and the flock, as in a shell handing its
        // descriptor to a lock helper — leave entries pending. An
        // exec's close-on-exec retirement runs in `handle_exec`, only
        // once the recorded exec is known to have succeeded: a failed
        // exec keeps its descriptors, so retiring them here, before
        // the result is known, would refuse a flock the recording
        // took on the surviving descriptor.
        {
            let mut registry = pending_placeholder_opens()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.retain(|pending| {
                let clears = syscall_clears_pending(syscall, pending.fd);
                if clears {
                    tracing::debug!(?syscall, pending.fd, "pending flock open retired");
                }
                !clears
            });
        }

        // As the recorder does: the kernel's restart of a poll a signal ended
        // there is served from the poll's recorded outputs.
        if let Some(poll) = guest.thread_state_mut().interrupted_poll.take()
            && poll.restarted_by(&syscall)
        {
            return Ok(self.handle_restarted_poll(guest, poll).await?);
        }

        // NOTE: This match statement should be identical to the one in the
        // recorder. Otherwise, our recorder and replayer will disagree about
        // how syscalls should be handled.
        //
        // FIXME: Figure out a way to avoid duplicate code. (Merge record/replay
        // into a single tool?)
        Ok(match syscall {
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Execve(_) | Syscall::Execveat(_) => self.handle_exec(guest, syscall).await,
            Syscall::Brk(_) => self.let_through(guest, syscall).await,
            Syscall::Mprotect(_) => self.let_through(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::ArchPrctl(_) => {
                // To properly handle arch_prctl, we should prevent calls from
                // using ARCH_SET_CPUID since we already do that for the
                // tracees. However, it is rare for programs to use
                // ARCH_SET_CPUID. For all other arch_prctl subfunctions, we
                // should let it through.
                self.let_through(guest, syscall).await
            }
            Syscall::Read(syscall) => Ok(self.handle_read(guest, syscall).await?),
            Syscall::Pread64(syscall) => self.handle_pread64(guest, syscall).await,
            Syscall::Readv(syscall) => Ok(self
                .handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.len(),
                    syscall.into(),
                )
                .await?),
            Syscall::Preadv(syscall) => Ok(self
                .handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len(),
                    syscall.into(),
                )
                .await?),
            Syscall::Preadv2(syscall) => Ok(self
                .handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len() as usize,
                    syscall.into(),
                )
                .await?),
            Syscall::Recvfrom(syscall) => self.handle_recvfrom(guest, syscall).await,
            Syscall::Recvmsg(syscall) => self.handle_recvmsg(guest, syscall).await,
            Syscall::Write(syscall) => Ok(self.handle_write_family(guest, syscall.into()).await?),
            Syscall::Pwrite64(syscall) => {
                Ok(self.handle_write_family(guest, syscall.into()).await?)
            }
            Syscall::Writev(syscall) => Ok(self.handle_write_family(guest, syscall.into()).await?),
            Syscall::Pwritev(syscall) => {
                Ok(self.handle_write_family(guest, syscall.into()).await?)
            }
            Syscall::Pwritev2(syscall) => {
                Ok(self.handle_write_family(guest, syscall.into()).await?)
            }
            Syscall::Sendfile(syscall) => Ok(self.handle_sendfile(guest, syscall).await?),
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#3598)
            Syscall::Faccessat(_) | Syscall::Other(Sysno::faccessat2, _) => {
                self.handle_simple(guest, syscall).await
            }
            #[cfg(target_arch = "x86_64")]
            Syscall::Access(_) => self.handle_simple(guest, syscall).await,
            Syscall::Lseek(_) => self.handle_optional_fd_position(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Stat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Fstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Lstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Newfstatat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            #[cfg(target_arch = "aarch64")]
            Syscall::Fstatat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Statfs(syscall) => self.handle_statfs(guest, syscall.buf()).await,
            Syscall::Fstatfs(syscall) => self.handle_statfs(guest, syscall.buf()).await,
            Syscall::Statx(syscall) => self.handle_statx(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Getdents(syscall) => self.handle_getdents(guest, syscall).await,
            Syscall::Getdents64(syscall) => self.handle_getdents64(guest, syscall).await,
            Syscall::Mmap(syscall) => self.handle_mmap(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Madvise(syscall) => self.handle_madvise(guest, syscall).await,
            Syscall::Munmap(_) => self.let_through(guest, syscall).await,
            Syscall::Openat(_) => self.handle_virtual_fd_create(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Open(_) => self.handle_virtual_fd_create(guest, syscall).await,
            Syscall::Openat2(call) => self.handle_openat2(guest, call).await,
            Syscall::Close(_) => self.handle_close(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#3598)
            Syscall::Chdir(_) | Syscall::Fchdir(_) => {
                self.handle_working_directory_change(guest, syscall).await
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Getcwd(call) => self.handle_getcwd(guest, call).await,
            Syscall::Fadvise64(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-3601): the descriptor may be a placeholder,
            // so the recorded result is returned without running the call.
            Syscall::Fsync(_) | Syscall::Fdatasync(_) | Syscall::Syncfs(_) => {
                self.handle_simple(guest, syscall).await
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Fchmod(_) | Syscall::Fchown(_) => {
                self.handle_descriptor_metadata_change(guest, syscall).await
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Fsetxattr(_)
            | Syscall::Fremovexattr(_)
            | Syscall::Setxattr(_)
            | Syscall::Lsetxattr(_)
            | Syscall::Removexattr(_)
            | Syscall::Lremovexattr(_) => self.handle_xattr_change(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-3601)
            Syscall::Getxattr(_)
            | Syscall::Lgetxattr(_)
            | Syscall::Fgetxattr(_)
            | Syscall::Listxattr(_)
            | Syscall::Llistxattr(_)
            | Syscall::Flistxattr(_) => self.handle_xattr_query(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#2373)
            Syscall::Flock(call) => self.handle_flock(guest, call).await,
            Syscall::Ftruncate(syscall) => Ok(self.handle_ftruncate(guest, syscall).await?),
            Syscall::Dup(_) => self.handle_dup_family(guest, syscall, "dup").await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Dup2(_) => self.handle_dup2(guest, syscall).await,
            Syscall::Dup3(_) => self.handle_dup_family(guest, syscall, "dup3").await,
            Syscall::Ioctl(syscall) => self.handle_ioctl(guest, syscall).await,
            Syscall::Socket(_) => {
                self.handle_replayed_side_effect(guest, syscall, "socket")
                    .await
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-979): recreate and validate the pidfd on
            // replay. pidfd_open returns a real kernel fd that later modeled
            // descriptor ops (fcntl/poll/waitid/close) act on, so we re-inject
            // it and assert the fd matches the recorded value, catching
            // fd-allocation or target-lifetime drift rather than blindly
            // substituting the recorded number.
            Syscall::PidfdOpen(_) => {
                self.handle_replayed_side_effect(guest, syscall, "pidfd_open")
                    .await
            }
            Syscall::ClockGettime(syscall) => self.handle_clock_gettime(guest, syscall).await,
            Syscall::ClockGetres(syscall) => self.handle_clock_getres(guest, syscall).await,
            Syscall::Gettimeofday(syscall) => self.handle_gettimeofday(guest, syscall).await,
            Syscall::Settimeofday(_) => self.handle_simple(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Time(syscall) => self.handle_time(guest, syscall).await,
            Syscall::Setsockopt(_) => self.handle_setsockopt(guest, syscall).await,
            Syscall::Fcntl(call)
                if matches!(
                    call.cmd(),
                    FcntlCmd::F_DUPFD(_) | FcntlCmd::F_DUPFD_CLOEXEC(_)
                ) =>
            {
                self.handle_dup_family(guest, syscall, "fcntl_dupfd").await
            }
            Syscall::Fcntl(call) if matches!(call.cmd(), FcntlCmd::F_SETFD(_)) => {
                self.handle_replayed_side_effect(guest, syscall, "fcntl_setfd")
                    .await
            }
            Syscall::Fcntl(call) if crate::event::fcntl_output(&call).is_some() => {
                self.handle_fcntl_output(guest, call).await
            }
            Syscall::Fcntl(_) => self.handle_simple(guest, syscall).await,
            // A regular file the guest opened but did not create is usually an
            // eventfd placeholder here, which epoll accepts where the recording
            // got EPERM, so the recorded answer stands
            // (https://github.com/rrnewton/hermit/issues/31).
            Syscall::EpollCtl(_) => self.handle_epoll_ctl(guest, syscall).await,
            Syscall::Connect(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-3871): Replay does not recreate a connection's
            // peer, so the calls that set up and tear down a connection are
            // served from the log like `connect`.
            Syscall::Bind(_) | Syscall::Listen(_) => self.handle_simple(guest, syscall).await,
            Syscall::Shutdown(_) => self.handle_shutdown(guest, syscall).await,
            Syscall::Accept(call) => self.handle_accept4(guest, call.into()).await,
            Syscall::Accept4(call) => self.handle_accept4(guest, call).await,
            Syscall::Sendto(_) => self.handle_simple(guest, syscall).await,
            Syscall::Sendmsg(_) => self.handle_simple(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Poll(syscall) => self.handle_poll(guest, syscall).await,
            Syscall::Ppoll(syscall) => self.handle_ppoll(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Select(syscall) => self.handle_select(guest, syscall).await,
            Syscall::Pselect6(syscall) => self.handle_pselect6(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::EpollWait(syscall) => self.handle_epoll_wait(guest, syscall).await,
            Syscall::EpollPwait(syscall) => self.handle_epoll_pwait(guest, syscall).await,
            Syscall::Other(Sysno::epoll_pwait2, _) => {
                self.handle_epoll_pwait2(guest, syscall).await
            }
            Syscall::Getsockopt(syscall) => self.handle_sockopt_family(guest, syscall.into()).await,
            Syscall::Getpeername(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getsockname(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getrandom(syscall) => self.handle_getrandom(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Readlink(syscall) => self.handle_readlink(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            #[cfg(target_arch = "x86_64")]
            Syscall::Mkdir(_) => self.handle_mkdir(guest, syscall, false).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Unlink(_) => self.handle_optional_path_removal(guest, syscall).await,
            Syscall::Unlinkat(call) => self.handle_unlinkat(guest, call).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Mkdirat(_) => self.handle_mkdir(guest, syscall, true).await,
            Syscall::Mknodat(_)
            | Syscall::Fchownat(_)
            | Syscall::Linkat(_)
            | Syscall::Renameat(_)
            | Syscall::Renameat2(_)
            | Syscall::Symlinkat(_)
            | Syscall::Fchmodat(_)
            | Syscall::Utimensat(_) => self.handle_confined_path_mutation(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            #[cfg(target_arch = "x86_64")]
            Syscall::Rename(_)
            | Syscall::Link(_)
            | Syscall::Symlink(_)
            | Syscall::Chmod(_)
            | Syscall::Chown(_)
            | Syscall::Lchown(_)
            | Syscall::Mknod(_) => self.handle_confined_path_mutation(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#3598): rmdir shares unlink's optional removal.
            #[cfg(target_arch = "x86_64")]
            Syscall::Rmdir(_) => self.handle_optional_path_removal(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Other(Sysno::close_range, _) => self.handle_close_range(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#2407): pidfd_getfd is raw in the pinned
            // Reverie. Replay must materialize its real OFD alias and validate
            // both successful fd allocation and failure errno.
            Syscall::Other(Sysno::pidfd_getfd, _) => self.handle_pidfd_getfd(guest, syscall).await,
            unsupported => return Ok(guest.inject_with_retry(unsupported).await?),
        }?)
    }

    // TODO-HUMAN-REVIEW(#2370)
    async fn handle_post_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        guest.thread_state_mut().bootstrapped = true;
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    async fn handle_rdtsc_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        _request: Rdtsc,
    ) -> Result<RdtscResult, Errno> {
        next_event!(guest, Rdtsc)
    }
}

impl Replayer {
    // TODO-HUMAN-REVIEW(#2370)
    async fn handle_exec<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let event = match next_event!(guest, Exec) {
            Ok(event) => event,
            Err(error) => return Err(error),
        };
        // The recorded exec succeeded (this handler runs only for a
        // recorded Exec event): pending entries of the exec'ing
        // thread group whose recorded descriptors were close-on-exec
        // die with it, because its placeholders are physically closed.
        // An ancestor's entry stays: its own table still holds the
        // descriptor, and a descendant can no longer spend it anyway
        // once its inherited placeholder is closed (the eventfd-id
        // check in `take_pending_placeholder_open`).
        if let Some((exec_tgid, _)) = guest_tgid_ppid(guest.tid()) {
            pending_placeholder_opens()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|pending| {
                    !(pending.flags.contains(OFlag::O_CLOEXEC) && pending.tgid == exec_tgid)
                });
        }
        let actual_request = self
            .read_exec_request(guest, syscall)
            .unwrap_or_else(|error| {
                panic!("could not read replay exec request after recorded success: {error}")
            });
        assert_eq!(
            actual_request.dirfd, event.request.dirfd,
            "replay exec directory context differs from recording"
        );
        assert_eq!(
            actual_request.flags, event.request.flags,
            "replay exec flags differ from recording"
        );
        let bootstrap_path = if actual_request.path == event.request.path {
            None
        } else {
            let valid_bootstrap_substitution = guest.is_root_thread()
                && !guest.thread_state().bootstrapped
                && matches!(
                    &event.target,
                    crate::event::ExecTarget::Materialize(materialization)
                        if matches!(
                            materialization.base,
                            crate::event::ExecMaterializationBase::Root
                        ) && materialization.path == event.request.path
                );
            assert!(
                valid_bootstrap_substitution,
                "replay exec pathname differs from recording outside the root bootstrap"
            );
            assert!(
                actual_request.path.len() >= event.request.path.len(),
                "private replay bootstrap pathname is shorter than the recorded pathname"
            );
            Some(event.request.path.clone())
        };

        let root = crate::record_replay_path::open_process_root(guest.pid())
            .unwrap_or_else(|error| panic!("could not pin replay root for exec: {error}"));
        for dependency in &event.dependencies {
            let path = PathBuf::from(std::ffi::OsString::from_vec(dependency.path.clone()));
            let start = match dependency.base {
                crate::event::ExecMaterializationBase::Root => {
                    crate::record_replay_path::open_process_root(guest.pid())
                }
                crate::event::ExecMaterializationBase::Cwd => {
                    crate::record_replay_path::open_process_cwd(guest.pid())
                }
                crate::event::ExecMaterializationBase::DirectoryFd(fd) => {
                    crate::record_replay_path::open_process_directory_fd(guest.pid(), fd)
                }
            }
            .unwrap_or_else(|error| panic!("could not pin replay exec dependency base: {error}"));
            let mut snapshot = self
                .open_exec_snapshot(&dependency.image)
                .unwrap_or_else(|error| {
                    panic!("could not open recorded exec dependency snapshot: {error}")
                });
            crate::record_replay_path::materialize_regular_file(
                &root,
                &start,
                &path,
                &dependency.symlinks,
                &mut snapshot,
                dependency.image.digest,
                dependency.image.mode,
            )
            .unwrap_or_else(|error| {
                panic!("could not materialize recorded exec dependency {path:?}: {error}")
            });
            remember_materialized_path(&root, &start, &path).unwrap_or_else(|error| {
                panic!("could not register recorded exec dependency {path:?}: {error}")
            });
        }

        if let crate::event::ExecTarget::Materialize(materialization) = &event.target {
            let path = PathBuf::from(std::ffi::OsString::from_vec(materialization.path.clone()));
            let start = match materialization.base {
                crate::event::ExecMaterializationBase::Root => {
                    crate::record_replay_path::open_process_root(guest.pid())
                }
                crate::event::ExecMaterializationBase::Cwd => {
                    crate::record_replay_path::open_process_cwd(guest.pid())
                }
                crate::event::ExecMaterializationBase::DirectoryFd(fd) => {
                    crate::record_replay_path::open_process_directory_fd(guest.pid(), fd)
                }
            }
            .unwrap_or_else(|error| {
                panic!("could not pin replay exec materialization base: {error}")
            });
            let mut snapshot = self
                .open_exec_snapshot(&event.executable)
                .unwrap_or_else(|error| panic!("could not open recorded exec snapshot: {error}"));
            crate::record_replay_path::materialize_regular_file(
                &root,
                &start,
                &path,
                &materialization.symlinks,
                &mut snapshot,
                event.executable.digest,
                event.executable.mode,
            )
            .and_then(|()| remember_materialized_path(&root, &start, &path))
            .unwrap_or_else(|error| {
                panic!("could not materialize recorded exec target {path:?}: {error}")
            });
        }

        if let crate::event::ExecTarget::RestoreDescriptor(descriptor) = &event.target {
            self.restore_exec_descriptor(guest, descriptor, &event.executable)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "could not restore recorded executable descriptor {}: {error}",
                        descriptor.target_fd
                    )
                });
        }

        self.verify_live_exec_target(guest, syscall, &event.executable)
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "replay exec target {:?} (dirfd {}, flags {:#x}) does not match recorded image: {error}",
                    std::ffi::OsString::from_vec(event.request.path.clone()),
                    event.request.dirfd,
                    event.request.flags,
                )
            });

        if let Some(path) = bootstrap_path {
            return self
                .inject_successful_exec_at_path(guest, syscall, &path)
                .await;
        }

        match guest.inject(syscall).await {
            Err(error) => panic!(
                "replayed exec returned {error} after recording successfully replaced the image"
            ),
            Ok(value) => panic!(
                "replayed exec returned {value} after recording successfully replaced the image"
            ),
        }
    }

    async fn inject_successful_exec_at_path<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        path: &[u8],
    ) -> Result<i64, Errno> {
        let address = match syscall {
            Syscall::Execve(call) => <Option<PathPtr<'_>> as FromToRaw>::into_raw(call.path()),
            Syscall::Execveat(call) => <Option<PathPtr<'_>> as FromToRaw>::into_raw(call.path()),
            _ => unreachable!("bootstrap path rewrite requested for {syscall:?}"),
        };
        assert_ne!(
            address, 0,
            "successful bootstrap exec has a non-null pathname"
        );
        let mut replacement = path.to_vec();
        replacement.push(0);
        guest.memory().write_exact(
            AddrMut::<u8>::from_raw(address).expect("bootstrap pathname address is non-null"),
            &replacement,
        )?;
        match guest.inject(syscall).await {
            Err(error) => panic!(
                "replayed bootstrap exec returned {error} after recording successfully replaced the image"
            ),
            Ok(value) => panic!(
                "replayed bootstrap exec returned {value} after recording successfully replaced the image"
            ),
        }
    }

    // TODO-HUMAN-REVIEW(#2370)
    async fn preserve_materialized_exec_descriptor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        descriptor: &crate::event::ExecDescriptor,
        image: &crate::event::ExecImage,
    ) -> io::Result<bool> {
        let target_path = format!("/proc/{}/fd/{}", guest.pid().as_raw(), descriptor.target_fd);
        let target_metadata = match std::fs::metadata(&target_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !target_metadata.file_type().is_file()
            || !materialized_file_is_registered(guest.pid(), &target_metadata)
        {
            return Ok(false);
        }

        let mut actual_status = guest
            .inject_with_retry(
                Fcntl::new()
                    .with_fd(descriptor.target_fd)
                    .with_cmd(FcntlCmd::F_GETFL),
            )
            .await
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
            as libc::c_int;
        const SETTABLE_STATUS_FLAGS: libc::c_int =
            libc::O_APPEND | libc::O_ASYNC | libc::O_DIRECT | libc::O_NOATIME | libc::O_NONBLOCK;
        if actual_status & !SETTABLE_STATUS_FLAGS
            != descriptor.status_flags & !SETTABLE_STATUS_FLAGS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "materialized exec descriptor immutable status flags differ: expected {:#x}, found {actual_status:#x}",
                    descriptor.status_flags
                ),
            ));
        }
        if actual_status != descriptor.status_flags {
            let normalized = (actual_status & !SETTABLE_STATUS_FLAGS)
                | (descriptor.status_flags & SETTABLE_STATUS_FLAGS);
            let result = guest
                .inject_with_retry(
                    Fcntl::new()
                        .with_fd(descriptor.target_fd)
                        .with_cmd(FcntlCmd::F_SETFL(normalized)),
                )
                .await;
            if result != Ok(0) {
                return Err(io::Error::other(format!(
                    "could not restore materialized exec descriptor status flags: {result:?}"
                )));
            }
            actual_status = guest
                .inject_with_retry(
                    Fcntl::new()
                        .with_fd(descriptor.target_fd)
                        .with_cmd(FcntlCmd::F_GETFL),
                )
                .await
                .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
                as libc::c_int;
            if actual_status != descriptor.status_flags {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "materialized exec descriptor status flags differ after F_SETFL: expected {:#x}, found {actual_status:#x}",
                        descriptor.status_flags
                    ),
                ));
            }
        }

        let actual_offset = guest
            .inject_with_retry(
                Lseek::new()
                    .with_fd(descriptor.target_fd)
                    .with_offset(0)
                    .with_whence(Whence::SEEK_CUR),
            )
            .await;
        match (descriptor.offset, actual_offset) {
            (Some(expected), Ok(actual)) if actual == expected => {}
            (Some(expected), Ok(actual)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "materialized exec descriptor offset differs: expected {expected}, found {actual}"
                    ),
                ));
            }
            (Some(_), Err(error)) => {
                return Err(io::Error::from_raw_os_error(error.into_raw()));
            }
            (None, Err(Errno::EBADF | Errno::ESPIPE)) => {}
            (None, Err(error)) => {
                return Err(io::Error::from_raw_os_error(error.into_raw()));
            }
            (None, Ok(actual)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "materialized exec descriptor unexpectedly has seekable offset {actual}"
                    ),
                ));
            }
        }

        let pinned = crate::record_replay_path::open_process_fd(guest.pid(), descriptor.target_fd)?;
        let identity = crate::record_replay_path::file_identity(pinned.as_raw_fd())?;
        if !crate::record_replay_path::is_regular_file(identity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "materialized exec descriptor is not a regular file",
            ));
        }
        if identity.mode & 0o7777 != image.mode {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "materialized exec descriptor mode differs: expected {:o}, found {:o}",
                    image.mode,
                    identity.mode & 0o7777
                ),
            ));
        }
        let actual_digest = detcore::Digest::digest_reader(
            crate::record_replay_path::open_readable_fd(pinned.as_raw_fd())?,
        )?;
        if actual_digest != image.digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "materialized exec descriptor digest differs: expected {}, found {}",
                    image.digest, actual_digest
                ),
            ));
        }

        let mut expected_aliases = descriptor
            .aliases
            .iter()
            .map(|alias| (alias.fd, alias.descriptor_flags))
            .collect::<Vec<_>>();
        expected_aliases.sort_unstable();
        let original_count = expected_aliases.len();
        expected_aliases.dedup_by_key(|alias| alias.0);
        if expected_aliases.len() != original_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recorded exec descriptor contains duplicate aliases",
            ));
        }

        let target_identity = ReplayFileIdentity::from_metadata(&target_metadata);
        let mut actual_aliases = Vec::new();
        for entry in std::fs::read_dir(format!("/proc/{}/fd", guest.pid().as_raw()))? {
            let entry = entry?;
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::c_int>().ok())
            else {
                continue;
            };
            let metadata = match std::fs::metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if ReplayFileIdentity::from_metadata(&metadata) != target_identity {
                continue;
            }
            if same_guest_open_file_description(guest.pid(), descriptor.target_fd, fd)? {
                let descriptor_flags = guest
                    .inject_with_retry(Fcntl::new().with_fd(fd).with_cmd(FcntlCmd::F_GETFD))
                    .await
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
                    as libc::c_int;
                actual_aliases.push((fd, descriptor_flags));
            }
        }
        actual_aliases.sort_unstable();
        if actual_aliases != expected_aliases {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "materialized exec descriptor aliases differ: expected {expected_aliases:?}, found {actual_aliases:?}"
                ),
            ));
        }

        Ok(true)
    }

    // TODO-HUMAN-REVIEW(#2370)
    async fn restore_exec_descriptor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        descriptor: &crate::event::ExecDescriptor,
        image: &crate::event::ExecImage,
    ) -> io::Result<()> {
        if descriptor.aliases.is_empty()
            || !descriptor
                .aliases
                .iter()
                .any(|alias| alias.fd == descriptor.target_fd)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recorded exec descriptor has no target alias",
            ));
        }
        for alias in &descriptor.aliases {
            let actual = guest
                .inject_with_retry(Fcntl::new().with_fd(alias.fd).with_cmd(FcntlCmd::F_GETFD))
                .await
                .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
                as libc::c_int;
            if actual != alias.descriptor_flags {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "replay descriptor {} flags differ before exec restore: expected {:#x}, found {actual:#x}",
                        alias.fd, alias.descriptor_flags
                    ),
                ));
            }
        }
        if self
            .preserve_materialized_exec_descriptor(guest, descriptor, image)
            .await?
        {
            return Ok(());
        }

        let root = crate::record_replay_path::open_process_root(guest.pid())?;
        let mut snapshot = self.open_exec_snapshot(image)?;
        let private_name = crate::record_replay_path::create_root_temporary_regular_file(
            &root,
            &mut snapshot,
            image.digest,
            image.mode,
        )?;
        let private_path = Path::new("/").join(&private_name);
        let path_bytes = private_path.as_os_str().as_bytes();
        if path_bytes.len() + 1 > 128 {
            let _ = crate::record_replay_path::unlink_root_entry(&root, &private_name);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "temporary executable pathname is too long",
            ));
        }
        let mut path = [0_u8; 128];
        path[..path_bytes.len()].copy_from_slice(path_bytes);
        let mut stack = guest.stack().await;
        let path_addr = stack.push(path);
        let path_ptr =
            PathPtr::from_ptr(unsafe { path_addr.cast::<u8>().as_ptr().cast::<libc::c_char>() })
                .expect("guest stack address is non-null");
        let _path_guard = stack
            .commit()
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
        let reopen_flags = OFlag::from_bits_truncate(descriptor.status_flags) | OFlag::O_CLOEXEC;
        let source_fd = match guest
            .inject_with_retry(
                Openat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(Some(path_ptr))
                    .with_flags(reopen_flags),
            )
            .await
        {
            Ok(fd) => fd as libc::c_int,
            Err(error) => {
                let _ = crate::record_replay_path::unlink_root_entry(&root, &private_name);
                return Err(io::Error::from_raw_os_error(error.into_raw()));
            }
        };
        drop(_path_guard);
        crate::record_replay_path::unlink_root_entry(&root, &private_name)?;

        let result = async {
            let actual_status = guest
                .inject_with_retry(Fcntl::new().with_fd(source_fd).with_cmd(FcntlCmd::F_GETFL))
                .await
                .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
                as libc::c_int;
            if actual_status != descriptor.status_flags {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "restored exec descriptor status flags differ: expected {:#x}, found {actual_status:#x}",
                        descriptor.status_flags
                    ),
                ));
            }
            if let Some(offset) = descriptor.offset {
                let actual = guest
                    .inject_with_retry(
                        Lseek::new()
                            .with_fd(source_fd)
                            .with_offset(offset)
                            .with_whence(Whence::SEEK_SET),
                    )
                    .await
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
                if actual != offset {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "restored exec descriptor offset differs: expected {offset}, found {actual}"
                        ),
                    ));
                }
            }

            for alias in &descriptor.aliases {
                let flags = if alias.descriptor_flags & libc::FD_CLOEXEC != 0 {
                    OFlag::O_CLOEXEC
                } else {
                    OFlag::empty()
                };
                let actual = guest
                    .inject_with_retry(
                        Dup3::new()
                            .with_oldfd(source_fd)
                            .with_newfd(alias.fd)
                            .with_flags(flags),
                    )
                    .await
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
                if actual != i64::from(alias.fd) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "restored exec descriptor alias returned {actual}, expected {}",
                            alias.fd
                        ),
                    ));
                }
                remember_materialized_file(guest.pid(), alias.fd);
            }
            Ok(())
        }
        .await;
        let close_source = guest
            .inject_with_retry(Close::new().with_fd(source_fd))
            .await;
        if close_source != Ok(0) && result.is_ok() {
            return Err(io::Error::other(format!(
                "could not close executable staging descriptor: {close_source:?}"
            )));
        }
        result
    }

    fn read_exec_request<G: Guest<Self>>(
        &self,
        guest: &G,
        syscall: Syscall,
    ) -> io::Result<crate::event::ExecRequest> {
        let call = match syscall {
            Syscall::Execve(call) => call.into(),
            Syscall::Execveat(call) => call,
            _ => unreachable!("exec request read for {syscall:?}"),
        };
        let path = call
            .path()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?
            .read(&guest.memory())
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
        Ok(crate::event::ExecRequest {
            dirfd: call.dirfd(),
            path: path.as_os_str().as_bytes().to_vec(),
            flags: call.flags(),
        })
    }

    fn open_exec_snapshot(&self, image: &crate::event::ExecImage) -> io::Result<File> {
        let path = self
            .data
            .join(crate::consts::EXEC_FILES_NAME)
            .join(image.digest.to_string());
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("recorded exec snapshot is not a regular file: {path:?}"),
            ));
        }
        let mut file = File::open(&path)?;
        let actual = detcore::Digest::digest_reader(&mut file)?;
        if actual != image.digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "recorded exec snapshot digest mismatch at {path:?}: expected {}, found {}",
                    image.digest, actual
                ),
            ));
        }
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    }

    async fn verify_live_exec_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        expected: &crate::event::ExecImage,
    ) -> io::Result<()> {
        let call = match syscall {
            Syscall::Execve(call) => call.into(),
            Syscall::Execveat(call) => call,
            _ => unreachable!("exec target verification for {syscall:?}"),
        };
        let path_ptr = call
            .path()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?;
        let path = path_ptr
            .read(&guest.memory())
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
        let pinned = if path.as_os_str().is_empty() {
            if call.flags() & libc::AT_EMPTY_PATH == 0 {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            crate::record_replay_path::open_process_fd(guest.pid(), call.dirfd())?
        } else {
            let mut flags = OFlag::O_PATH | OFlag::O_CLOEXEC;
            if call.flags() & libc::AT_SYMLINK_NOFOLLOW != 0 {
                flags |= OFlag::O_NOFOLLOW;
            }
            let temporary = guest
                .inject_with_retry(
                    reverie::syscalls::Openat::new()
                        .with_dirfd(call.dirfd())
                        .with_path(Some(path_ptr))
                        .with_flags(flags),
                )
                .await
                .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
            let pinned =
                crate::record_replay_path::open_process_fd(guest.pid(), temporary as libc::c_int);
            let close = guest
                .inject_with_retry(Close::new().with_fd(temporary as libc::c_int))
                .await;
            if close != Ok(0) {
                return Err(io::Error::other(format!(
                    "could not close temporary replay exec descriptor: {close:?}"
                )));
            }
            pinned?
        };
        let identity = crate::record_replay_path::file_identity(pinned.as_raw_fd())?;
        if !crate::record_replay_path::is_regular_file(identity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "replay exec target is not a regular file",
            ));
        }
        if identity.mode & 0o7777 != expected.mode {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "replay exec mode differs: expected {:o}, found {:o}",
                    expected.mode,
                    identity.mode & 0o7777
                ),
            ));
        }
        let actual = detcore::Digest::digest_reader(crate::record_replay_path::open_readable_fd(
            pinned.as_raw_fd(),
        )?)?;
        if actual != expected.digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "replay exec digest differs: expected {}, found {}",
                    expected.digest, actual
                ),
            ));
        }
        Ok(())
    }

    pub(super) async fn reserve_replay_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        cloexec: bool,
    ) {
        if let Err(placeholder) = self.try_reserve_replay_fd(guest, fd, cloexec, 0).await {
            panic!("{}", slot_divergence_message(guest.tid(), fd, placeholder));
        }
    }

    /// Installs the stand-in for a descriptor an `SCM_RIGHTS` receive put in
    /// slot `fd` (`RECEIVED_STAND_IN_COUNT`), which must be the lowest free
    /// descriptor.
    pub(super) async fn reserve_received_stand_in<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        cloexec: bool,
    ) {
        if let Err(placeholder) = self
            .try_reserve_replay_fd(guest, fd, cloexec, RECEIVED_STAND_IN_COUNT)
            .await
        {
            panic!("{}", slot_divergence_message(guest.tid(), fd, placeholder));
        }
    }

    /// Installs the placeholder for an accepted connection in slot `fd`,
    /// refusing by name if that slot is not the lowest free descriptor.
    pub(super) async fn reserve_accept_stand_in<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        cloexec: bool,
    ) {
        if let Err(placeholder) = self
            .try_reserve_replay_fd(guest, fd, cloexec, ACCEPT_STAND_IN_COUNT)
            .await
        {
            panic!("{ACCEPT_SLOT_REFUSAL}: accept recorded {fd}, lowest free {placeholder}");
        }
    }

    /// Installs an eventfd placeholder with initial `count` in slot `fd`,
    /// which must be the lowest free descriptor. If Linux places it elsewhere,
    /// closes it and returns the slot it landed in, so the caller can name the
    /// cause.
    async fn try_reserve_replay_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        cloexec: bool,
        count: u32,
    ) -> Result<(), i64> {
        let flags = if cloexec {
            EfdFlags::EFD_CLOEXEC
        } else {
            EfdFlags::empty()
        };
        let placeholder = guest
            .inject_with_retry(Eventfd2::new().with_count(count).with_flags(flags))
            .await
            .unwrap_or_else(|error| {
                panic!("could not reserve replay FD {fd} with an eventfd: {error}")
            });
        if placeholder != i64::from(fd) {
            let _ = guest.inject(Close::new().with_fd(placeholder as i32)).await;
            return Err(placeholder);
        }
        Ok(())
    }
    pub(super) fn fd_is_in_replay_root(&self, pid: Pid, fd: libc::c_int) -> bool {
        let path = format!("/proc/{}/fd/{fd}", pid.as_raw());
        let Ok(metadata) = std::fs::metadata(&path) else {
            return false;
        };
        if metadata.file_type().is_file() || metadata.file_type().is_fifo() {
            return materialized_file_is_registered(pid, &metadata);
        }
        if !metadata.file_type().is_dir() {
            return false;
        }
        // Compare directory objects, not procfs link text. The guest may live in
        // a mount namespace the replayer cannot name, such as a private tmpfs at
        // `/test`, where the two link texts never share a prefix.
        let (Ok(root), Ok(directory)) = (
            crate::record_replay_path::open_process_root(pid),
            crate::record_replay_path::open_process_directory_fd(pid, fd),
        ) else {
            return false;
        };
        crate::record_replay_path::directory_is_beneath(&root, &directory).unwrap_or(false)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#662): Audit replay path confinement.
    fn open_request_with_mode<G: Guest<Self>>(
        &self,
        guest: &G,
        syscall: Syscall,
    ) -> io::Result<(
        (libc::c_int, PathBuf, OFlag),
        Option<reverie::syscalls::Mode>,
    )> {
        let request = match syscall {
            #[cfg(target_arch = "x86_64")]
            Syscall::Open(call) => (
                (
                    libc::AT_FDCWD,
                    call.path()
                        .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?
                        .read(&guest.memory())
                        .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?,
                    call.flags(),
                ),
                call.mode(),
            ),
            Syscall::Openat(call) => (
                (
                    call.dirfd(),
                    call.path()
                        .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?
                        .read(&guest.memory())
                        .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?,
                    call.flags(),
                ),
                call.mode(),
            ),
            _ => unreachable!("open replay path requested for {syscall:?}"),
        };
        Ok(request)
    }

    /// Decides whether a recorded regular-file open names a file replay may
    /// open for real: a file replay itself materialized, a file the open creates,
    /// or, for `O_TMPFILE`, a directory. Resolution starts from descriptors for
    /// the guest's root and base directory, so it sees the guest's mounts and
    /// symlinks rather than the replayer's. An `O_CREAT` open first creates the
    /// missing parent directories inside the replay root.
    ///
    /// A path that crosses a procfs symlink (`/proc/<pid>/root`,
    /// `/proc/self/fd/<n>`, or `/dev/fd/<n>` through `/proc/self/fd`) is
    /// refused before any directory is created. Its text names a path inside
    /// the replay root, but the guest's kernel follows the link to the linked
    /// object, which may be a host file outside it.
    ///
    /// Returns why a recorded regular-file open may not be opened for real, or
    /// `None` when it may. Keeping the refusal as a named reason, not a
    /// bare `false`, is what lets a later flock refusal say which gate
    /// fired (dh-gg5uv: the landing validation of the recorded-base
    /// fix still refused `/tmp/hsperfdata_root/9` on its first attempt,
    /// and a boolean gate could not say whether the object was
    /// missing, its parent was missing, or it resolved to the wrong
    /// kind of object in the fresh replay root).
    fn open_materialization_refusal(
        &self,
        pid: Pid,
        dirfd: libc::c_int,
        path: &Path,
        flags: OFlag,
    ) -> Option<String> {
        // The recorded open's cwd and dirfd live in the calling thread's
        // state, not its leader's: a JVM service thread enters its perf
        // directory itself, and resolving through the leader looks in the
        // leader's (different) cwd and descriptor table.
        let root = match crate::record_replay_path::open_process_root(pid) {
            Ok(root) => root,
            Err(error) => return Some(format!("could not open the replay root: {error}")),
        };
        let start = if path.is_absolute() {
            crate::record_replay_path::open_process_root(pid)
        } else if dirfd == libc::AT_FDCWD {
            crate::record_replay_path::open_process_cwd(pid)
        } else {
            crate::record_replay_path::open_process_directory_fd(pid, dirfd)
        };
        let start = match start {
            Ok(start) => start,
            Err(error) => {
                return Some(format!(
                    "could not open the resolution base for {path:?}: {error}"
                ));
            }
        };
        match crate::record_replay_path::directory_is_beneath(&root, &start) {
            Ok(true) => {}
            Ok(false) => {
                return Some(format!(
                    "the resolution base for {path:?} is not beneath the replay root"
                ));
            }
            Err(error) => {
                return Some(format!(
                    "could not check the resolution base for {path:?} against the replay root: {error}"
                ));
            }
        }
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if flags.contains(OFlag::O_CREAT)
            && let Some(parent) = parent
            && let Err(error) =
                crate::record_replay_path::ensure_directory_path_follow_final_without_procfs_symlinks(
                    &root, &start, parent,
                )
            && error.raw_os_error() == Some(libc::ELOOP)
        {
            return Some(format!(
                "creating the parent of {path:?} would cross a procfs symlink: {error}"
            ));
        }
        let metadata =
            |object: &OwnedFd| std::fs::metadata(format!("/proc/self/fd/{}", object.as_raw_fd()));
        match crate::record_replay_path::resolve_existing_path_without_procfs_symlinks(
            &root, &start, path,
        ) {
            Ok(resolved) => match metadata(&resolved.object) {
                Ok(found) if flags.contains(OFlag::O_TMPFILE) => {
                    if found.is_dir() {
                        None
                    } else {
                        Some(format!(
                            "{path:?} resolves to a non-directory for an O_TMPFILE open"
                        ))
                    }
                }
                Ok(found) if found.file_type().is_file() => {
                    // A regular file already beneath the replay root is a
                    // real lockable object whether or not an earlier replay
                    // step registered it. Registration can lag the guest's
                    // own creation of the file (its creating open may have
                    // been served virtually), and requiring it here made a
                    // later flock on the descriptor panic (issue 5007).
                    // Register it now and materialize the open for real.
                    if !materialized_file_is_registered(pid, &found)
                        && let Ok(root_identity) = replay_root_identity(pid)
                        && let Ok(object) = duplicate_controller_fd(resolved.object.as_raw_fd())
                    {
                        let _ = register_materialized(root_identity, object);
                    }
                    None
                }
                Ok(found) => Some(format!(
                    "{path:?} resolves to a {:?}, not a regular file",
                    found.file_type()
                )),
                Err(error) => Some(format!(
                    "could not stat the object {path:?} resolves to: {error}"
                )),
            },
            Err(error)
                if error.raw_os_error() == Some(libc::ENOENT)
                    && flags.contains(OFlag::O_CREAT)
                    && !flags.contains(OFlag::O_TMPFILE) =>
            {
                // Only the final component may be missing.
                match parent {
                    None => None,
                    Some(parent) => {
                        match crate::record_replay_path::resolve_existing_path_without_procfs_symlinks(
                            &root, &start, parent,
                        )
                        .and_then(|resolved| metadata(&resolved.object))
                        {
                            Ok(found) if found.is_dir() => None,
                            Ok(_) => Some(format!(
                                "the parent of {path:?} does not resolve to a directory"
                            )),
                            Err(parent_error) => Some(format!(
                                "{path:?} does not exist and its parent cannot be resolved: {parent_error}"
                            )),
                        }
                    }
                }
            }
            Err(error) => Some(format!("{path:?} cannot be resolved: {error}")),
        }
    }

    fn materialize_recorded_directory<G: Guest<Self>>(
        &self,
        guest: &G,
        syscall: Syscall,
        dirfd: libc::c_int,
        path: &Path,
        flags: OFlag,
    ) -> io::Result<bool> {
        let root = crate::record_replay_path::open_process_root(guest.tid())?;
        let start = if path.is_absolute() {
            crate::record_replay_path::open_process_root(guest.tid())?
        } else if dirfd == libc::AT_FDCWD {
            crate::record_replay_path::open_process_cwd(guest.tid())?
        } else {
            let start =
                match crate::record_replay_path::open_process_directory_fd(guest.tid(), dirfd) {
                    Ok(start) => start,
                    Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
                        tracing::debug!(
                            ?syscall,
                            "recorded open directory base is a virtual replay descriptor"
                        );
                        return Ok(false);
                    }
                    Err(error) => return Err(error),
                };
            match crate::record_replay_path::directory_is_beneath(&root, &start)? {
                true => {}
                false => {
                    tracing::debug!(
                        ?syscall,
                        "recorded open directory base is not confined to replay root"
                    );
                    return Ok(false);
                }
            }
            start
        };
        if flags.contains(OFlag::O_NOFOLLOW) {
            crate::record_replay_path::ensure_directory_path(&root, &start, path)?;
        } else {
            crate::record_replay_path::ensure_directory_path_follow_final(&root, &start, path)?;
        }
        Ok(true)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#662): Audit replay-root file and directory materialization.
    async fn handle_virtual_fd_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Open)?;
        let recorded = event.result;
        if let Ok(fd) = recorded {
            let ((dirfd, path, flags), mode) = self
                .open_request_with_mode(guest, syscall)
                .unwrap_or_else(|error| {
                    panic!("could not decode successful recorded open: {error}")
                });

            // Decide materialization against the absolute guest path the
            // recorded open named for this thread, for the same reason a
            // pending entry remembers that form: the raw relative pair's
            // base (this thread's cwd or dirfd) is the right base only at
            // this moment, and resolving it through any other state -
            // including the leader's - can name the wrong directory.
            let (materialization_dirfd, materialization_path) =
                match guest_absolute_open_path(guest.tid(), dirfd, &path) {
                    Some(absolute) => (libc::AT_FDCWD, absolute),
                    None => (dirfd, path.clone()),
                };
            let materialize = match event.materialize {
                OpenMaterialization::Directory => self
                    .materialize_recorded_directory(
                        guest,
                        syscall,
                        materialization_dirfd,
                        &materialization_path,
                        flags,
                    )
                    .unwrap_or_else(|error| {
                        panic!("failed to materialize recorded open directory: {error}")
                    }),
                OpenMaterialization::RegularFile => {
                    match self.open_materialization_refusal(
                        guest.tid(),
                        materialization_dirfd,
                        &materialization_path,
                        flags,
                    ) {
                        None => true,
                        Some(reason) => {
                            // A regular file that replay answers with a
                            // placeholder here is exactly the descriptor a
                            // later flock must materialize or refuse; say
                            // which gate made it a placeholder.
                            tracing::debug!(
                                ?syscall,
                                reason,
                                "recorded regular-file open stays virtual"
                            );
                            false
                        }
                    }
                }
                OpenMaterialization::None => false,
            };
            if materialize {
                match guest.inject_with_retry(syscall).await {
                    Ok(actual) => {
                        assert_eq!(
                            actual, fd,
                            "replay materialized open returned a different descriptor"
                        );
                        remember_materialized_file(guest.tid(), actual as libc::c_int);
                        if flags.contains(OFlag::O_TRUNC) {
                            mmap::truncate_shared_backing(guest.tid(), actual as libc::c_int, 0)
                                .unwrap_or_else(|error| {
                                    mmap::refuse("truncate the shared backing after open", error)
                                });
                        }
                    }
                    Err(error) => {
                        tracing::debug!(
                            ?syscall,
                            %error,
                            "replay path unavailable; reserving a virtual descriptor"
                        );
                        self.reserve_replay_fd(guest, fd as i32, flags.contains(OFlag::O_CLOEXEC))
                            .await;
                        if let Some(pending) = pending_placeholder_open(
                            guest.tid(),
                            fd as libc::c_int,
                            dirfd,
                            path.clone(),
                            flags,
                            mode,
                        ) {
                            remember_pending_placeholder_open(pending);
                        }
                    }
                }
            } else if self
                .try_materialize_fifo_open(
                    guest,
                    syscall,
                    materialization_dirfd,
                    &materialization_path,
                    flags,
                    fd,
                )
                .await
            {
                // The recorded open now names the real fifo in this
                // descriptor table, registered by identity inside.
            } else {
                self.reserve_replay_fd(guest, fd as i32, flags.contains(OFlag::O_CLOEXEC))
                    .await;
                if let Some(pending) = pending_placeholder_open(
                    guest.tid(),
                    fd as libc::c_int,
                    dirfd,
                    path.clone(),
                    flags,
                    mode,
                ) {
                    remember_pending_placeholder_open(pending);
                }
            }
        }
        recorded
    }

    /// Materializes a recorded fifo open for real, in the opening
    /// thread's own table, when the fifo resolves beneath the replay
    /// root. Doing it here rather than lazily at a descendant's
    /// `flock` is what keeps the lock's lifetime right: a lock taken
    /// through an inherited descriptor is held by the shared open
    /// file description, so it must survive the locking child's exit
    /// for exactly as long as the opener keeps the descriptor — a
    /// descriptor materialized only in the child's table takes its
    /// lock with it when the child exits, and a later contender the
    /// recording turned away would win in replay (the dh-gg5uv
    /// review's fork/shared-description split, in lock-lifetime
    /// form). The recorder still marks fifo opens unmaterialized, so
    /// this is tried only where the regular-file decision said no.
    ///
    /// A single-access-mode fifo open is re-issued with `O_NONBLOCK`
    /// (and its recorded status flags restored after) so replay
    /// cannot stall on a rendezvous whose peer has not been replayed
    /// yet; a writer open that then fails `ENXIO` falls back to the
    /// placeholder plus pending entry, exactly as before.
    async fn try_materialize_fifo_open<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        dirfd: libc::c_int,
        path: &Path,
        flags: OFlag,
        fd: i64,
    ) -> bool {
        if !self.fifo_resolves_in_replay_root(guest, dirfd, path) {
            return false;
        }
        let access = flags & OFlag::O_ACCMODE;
        let nonblocking_reissue = access != OFlag::O_RDWR
            && !flags.contains(OFlag::O_NONBLOCK)
            && !flags.contains(OFlag::O_PATH);
        let inject = if nonblocking_reissue {
            match syscall {
                #[cfg(target_arch = "x86_64")]
                Syscall::Open(call) => Syscall::Open(call.with_flags(flags | OFlag::O_NONBLOCK)),
                Syscall::Openat(call) => {
                    Syscall::Openat(call.with_flags(flags | OFlag::O_NONBLOCK))
                }
                _ => return false,
            }
        } else {
            syscall
        };
        let actual = match guest.inject_with_retry(inject).await {
            Ok(actual) => actual,
            Err(error) => {
                tracing::debug!(
                    ?syscall,
                    %error,
                    "replay fifo path unavailable; reserving a virtual descriptor"
                );
                return false;
            }
        };
        assert_eq!(
            actual, fd,
            "replay materialized fifo open returned a different descriptor"
        );
        if nonblocking_reissue {
            const SETTABLE: libc::c_int = libc::O_APPEND
                | libc::O_ASYNC
                | libc::O_DIRECT
                | libc::O_NOATIME
                | libc::O_NONBLOCK;
            guest
                .inject_with_retry(
                    Fcntl::new()
                        .with_fd(fd as libc::c_int)
                        .with_cmd(FcntlCmd::F_SETFL(flags.bits() & SETTABLE)),
                )
                .await
                .unwrap_or_else(|error| {
                    panic!("could not restore materialized fifo open status flags: {error}")
                });
        }
        remember_materialized_file(guest.tid(), fd as libc::c_int);
        true
    }

    fn mkdir_request<G: Guest<Self>>(
        &self,
        guest: &G,
        syscall: Syscall,
    ) -> io::Result<(libc::c_int, PathBuf)> {
        let (dirfd, path) = match syscall {
            #[cfg(target_arch = "x86_64")]
            Syscall::Mkdir(call) => (
                libc::AT_FDCWD,
                call.path()
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?
                    .read(&guest.memory())
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?,
            ),
            Syscall::Mkdirat(call) => (
                call.dirfd(),
                call.path()
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?
                    .read(&guest.memory())
                    .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?,
            ),
            _ => unreachable!("mkdir replay path requested for {syscall:?}"),
        };
        Ok((dirfd, path))
    }

    /// Creates the recorded mkdir's target directory, or with `parent_only`
    /// just its parent chain, beneath the replay root. Returns false when the
    /// directory base is a virtual replay descriptor or lies outside the root.
    fn materialize_recorded_mkdir<G: Guest<Self>>(
        &self,
        guest: &G,
        syscall: Syscall,
        parent_only: bool,
    ) -> io::Result<bool> {
        let (dirfd, path) = self.mkdir_request(guest, syscall)?;
        let path = if parent_only {
            match path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                Some(parent) => parent.to_path_buf(),
                None => return Ok(true),
            }
        } else {
            path
        };
        // Linux follows a symlink in every component except the final one of
        // the mkdir path, so a parent's own final component is followed. As for
        // an O_CREAT open's parent, a procfs symlink is refused rather than
        // followed to a host path.
        let ensure = if parent_only {
            crate::record_replay_path::ensure_directory_path_follow_final_without_procfs_symlinks
        } else {
            crate::record_replay_path::ensure_directory_path
        };
        let root = crate::record_replay_path::open_process_root(guest.pid())?;
        if path.is_absolute() {
            ensure(&root, &root, &path)?;
            return Ok(true);
        }

        let start = if dirfd == libc::AT_FDCWD {
            crate::record_replay_path::open_process_cwd(guest.pid())?
        } else {
            match crate::record_replay_path::open_process_directory_fd(guest.pid(), dirfd) {
                Ok(start) => start,
                Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
                    tracing::debug!(
                        ?syscall,
                        "recorded mkdir directory base is a virtual replay descriptor"
                    );
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
        };
        match crate::record_replay_path::directory_is_beneath(&root, &start)? {
            true => {}
            false => {
                tracing::debug!(
                    ?syscall,
                    "recorded mkdir directory base is not confined to replay root"
                );
                return Ok(false);
            }
        }
        ensure(&root, &start, &path)?;
        Ok(true)
    }

    // TODO-HUMAN-REVIEW(#2370)
    /// Replays successful mkdir side effects. A successful mkdir proves that
    /// every parent directory existed at record time, so replay first creates
    /// that parent chain in the fresh chroot; a standalone replay otherwise
    /// lacks any host directory the guest only mkdir'd beneath. For a recorded
    /// `EEXIST` caused by a directory, it reconstructs that directory in the
    /// fresh chroot while still returning the recorded error to the guest.
    async fn handle_mkdir<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        confined_dirfd: bool,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Mkdir)?;
        if event.result == Err(Errno::EEXIST) && event.existing_directory {
            self.materialize_recorded_mkdir(guest, syscall, false)
                .unwrap_or_else(|error| {
                    panic!("failed to materialize recorded mkdir directory: {error}")
                });
        } else if let Ok(expected) = event.result {
            if (!confined_dirfd || self.path_mutation_dirfds_are_confined(guest.pid(), syscall))
                && let Err(error) = self.materialize_recorded_mkdir(guest, syscall, true)
            {
                // The injected mkdir below still decides: it must reproduce the
                // recorded success (or, for a confined mkdirat, stay virtual).
                tracing::debug!(?syscall, %error, "replay could not create the mkdir parent");
            }
            if !confined_dirfd {
                let actual = guest.inject_with_retry(syscall).await;
                assert_eq!(actual, Ok(expected), "mkdir side effects diverged");
            } else if self.path_mutation_dirfds_are_confined(guest.pid(), syscall) {
                match guest.inject_with_retry(syscall).await {
                    Ok(actual) => assert_eq!(
                        actual, expected,
                        "replayed mkdirat returned a different result"
                    ),
                    Err(error @ (Errno::ENOENT | Errno::ENOTDIR | Errno::EROFS)) => {
                        tracing::debug!(?syscall, %error, "replay mkdirat kept virtual");
                    }
                    Err(error) => {
                        panic!(
                            "replayed mkdirat {syscall:?} failed after recording returned {expected}: {error}"
                        );
                    }
                }
            }
        }
        event.result
    }

    fn dirfd_is_confined(&self, pid: Pid, fd: libc::c_int) -> bool {
        fd == libc::AT_FDCWD || self.fd_is_in_replay_root(pid, fd)
    }

    fn path_mutation_dirfds_are_confined(&self, pid: Pid, syscall: Syscall) -> bool {
        match syscall {
            Syscall::Mkdirat(call) => self.dirfd_is_confined(pid, call.dirfd()),
            Syscall::Mknodat(call) => self.dirfd_is_confined(pid, call.dirfd()),
            Syscall::Fchownat(call) => self.dirfd_is_confined(pid, call.dirfd()),
            Syscall::Fchmodat(call) => self.dirfd_is_confined(pid, call.dirfd()),
            // TODO-HUMAN-REVIEW(#3598): the legacy forms resolve relative
            // paths against the working directory, like AT_FDCWD.
            #[cfg(target_arch = "x86_64")]
            Syscall::Rename(_)
            | Syscall::Link(_)
            | Syscall::Symlink(_)
            | Syscall::Chmod(_)
            | Syscall::Chown(_)
            | Syscall::Lchown(_)
            | Syscall::Mknod(_) => true,
            Syscall::Utimensat(call) => self.dirfd_is_confined(pid, call.dirfd()),
            Syscall::Symlinkat(call) => self.dirfd_is_confined(pid, call.newdirfd()),
            Syscall::Linkat(call) => {
                self.dirfd_is_confined(pid, call.olddirfd())
                    && self.dirfd_is_confined(pid, call.newdirfd())
            }
            Syscall::Renameat(call) => {
                self.dirfd_is_confined(pid, call.olddirfd())
                    && self.dirfd_is_confined(pid, call.newdirfd())
            }
            Syscall::Renameat2(call) => {
                self.dirfd_is_confined(pid, call.olddirfd())
                    && self.dirfd_is_confined(pid, call.newdirfd())
            }
            _ => false,
        }
    }

    async fn handle_openat2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: reverie::syscalls::Openat2,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if let Ok(fd) = recorded {
            // Remember a placeholder reserve below for a following
            // flock, unless the resolve flags change what an openat
            // re-issue would mean, or the recorded flags/mode are wider
            // than the plain openat the materialization re-issues:
            // truncating them would re-issue a different open than the
            // recorded one (dh-gg5uv review), so they must fit exactly.
            let how = syscall.how().and_then(|how| how.read(&guest.memory()).ok());
            let remember = |guest: &mut G| {
                let (Some(how), Some(path)) = (how, syscall.path()) else {
                    return;
                };
                if how.resolve != 0 {
                    return;
                }
                let (Ok(flags), Ok(path)) = (
                    i32::try_from(how.flags)
                        .ok()
                        .and_then(OFlag::from_bits)
                        .ok_or(()),
                    path.read(&guest.memory()),
                ) else {
                    return;
                };
                if u32::try_from(how.mode).is_err() {
                    return;
                }
                let mode = reverie::syscalls::Mode::from_bits(how.mode as u32);
                if mode.is_none() && how.mode != 0 {
                    return;
                }
                if let Some(pending) = pending_placeholder_open(
                    guest.tid(),
                    fd as libc::c_int,
                    syscall.dirfd(),
                    path,
                    flags,
                    mode,
                ) {
                    remember_pending_placeholder_open(pending);
                }
            };
            if self.dirfd_is_confined(guest.tid(), syscall.dirfd()) {
                match guest.inject_with_retry(syscall).await {
                    Ok(actual) => {
                        assert_eq!(actual, fd, "replayed openat2 returned a different fd")
                    }
                    Err(error) => {
                        tracing::debug!(%error, "replay openat2 path unavailable");
                        self.reserve_replay_fd(
                            guest,
                            fd as i32,
                            how.is_some_and(|how| {
                                how.flags & u64::from(libc::O_CLOEXEC as u32) != 0
                            }),
                        )
                        .await;
                        remember(guest);
                    }
                }
            } else if let Some(inject) = self.openat2_fifo_inject(&syscall, how)
                && self
                    .try_materialize_fifo_open(
                        guest,
                        inject,
                        syscall.dirfd(),
                        &syscall
                            .path()
                            .and_then(|path| path.read(&guest.memory()).ok())
                            .unwrap_or_default(),
                        i32::try_from(how.map(|how| how.flags).unwrap_or(0))
                            .ok()
                            .and_then(OFlag::from_bits)
                            .unwrap_or_else(OFlag::empty),
                        fd,
                    )
                    .await
            {
                // Real fifo descriptor in the recorded slot, as with
                // the open/openat eager path.
            } else {
                self.reserve_replay_fd(
                    guest,
                    fd as i32,
                    how.is_some_and(|how| how.flags & u64::from(libc::O_CLOEXEC as u32) != 0),
                )
                .await;
                remember(guest);
            }
        }
        recorded
    }

    /// The plain-`openat` form of a recorded `openat2`, for eager fifo
    /// materialization on its unconfined path (where the open would
    /// otherwise become a placeholder): `Some` only when the resolve
    /// flags are zero and the recorded flags/mode fit `openat`
    /// exactly, the same conditions under which the pending registry
    /// may re-issue the open at a later flock. Confined openat2 opens
    /// are already injected for real by `handle_openat2` itself.
    fn openat2_fifo_inject(
        &self,
        syscall: &reverie::syscalls::Openat2,
        how: Option<reverie::syscalls::OpenHow>,
    ) -> Option<Syscall> {
        let how = how?;
        if how.resolve != 0 {
            return None;
        }
        let flags = i32::try_from(how.flags).ok().and_then(OFlag::from_bits)?;
        let mode = reverie::syscalls::Mode::from_bits(u32::try_from(how.mode).ok()?);
        if mode.is_none() && how.mode != 0 {
            return None;
        }
        Some(Syscall::Openat(
            reverie::syscalls::Openat::new()
                .with_dirfd(syscall.dirfd())
                .with_path(syscall.path())
                .with_flags(flags)
                .with_mode(mode),
        ))
    }

    async fn handle_confined_path_mutation<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if let Ok(expected) = recorded
            && self.path_mutation_dirfds_are_confined(guest.pid(), syscall)
        {
            match guest.inject_with_retry(syscall).await {
                Ok(actual) => assert_eq!(
                    actual, expected,
                    "replayed path mutation returned a different result"
                ),
                Err(error @ (Errno::ENOENT | Errno::ENOTDIR | Errno::EROFS)) => {
                    tracing::debug!(?syscall, %error, "replay path mutation kept virtual");
                }
                Err(error) => {
                    panic!(
                        "replayed path mutation {syscall:?} failed after recording returned {expected}: {error}"
                    );
                }
            }
        }
        recorded
    }

    // TODO-HUMAN-REVIEW(PR-3601)
    /// Replays a metadata change made through a descriptor: fchmod or fchown.
    /// A placeholder descriptor has no metadata to change, so the recorded
    /// result stands alone. A file in the replay root gets the change
    /// reapplied, because later path calls such as chmod run there and must
    /// find what the recording found.
    async fn handle_descriptor_metadata_change<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let fd = match syscall {
            Syscall::Fchmod(call) => call.fd(),
            Syscall::Fchown(call) => call.fd(),
            _ => unreachable!("descriptor metadata handler received {syscall:?}"),
        };
        let recorded = next_event!(guest, Return);
        if let Ok(expected) = recorded
            && self.fd_is_in_replay_root(guest.pid(), fd)
        {
            match guest.inject_with_retry(syscall).await {
                Ok(actual) => assert_eq!(
                    actual, expected,
                    "replayed descriptor metadata change returned a different result"
                ),
                Err(error) => {
                    panic!(
                        "replayed descriptor metadata change {syscall:?} failed after recording returned {expected}: {error}"
                    );
                }
            }
        }
        recorded
    }

    // TODO-HUMAN-REVIEW(PR-3601)
    /// Replays an xattr change from its recorded result. Every xattr query
    /// also replays from the recording, so the guest never reads the replay
    /// root's attributes directly. The kernel and filesystems do, for every
    /// namespace but `user.`: `system.` holds POSIX and NFSv4 ACLs, which
    /// govern access and later creations; `security.` holds file capabilities
    /// and the labels security modules (SELinux, Smack, IMA, EVM) consult on
    /// access and exec; `trusted.` is read by filesystems such as overlayfs.
    /// Those govern calls replay runs live, so a change to one of them that
    /// the recording saw succeed on a file in the replay root must establish
    /// its end state there, even a read-only one (EROFS), or replay refuses to
    /// continue. A `user.` change is carried over where the replay root can
    /// take it and otherwise kept virtual: nothing that replay runs live can
    /// observe it.
    ///
    /// The replay root is built without the host's attributes, so its starting
    /// state may differ from the recording's. A set therefore runs without
    /// XATTR_CREATE or XATTR_REPLACE: whatever was there, the attribute ends up
    /// holding the recorded value. A removal that finds no attribute (ENODATA)
    /// ends in the recorded state too.
    ///
    /// A file the replay root lacks (a placeholder descriptor, ENOENT or
    /// ENOTDIR) keeps the change virtual, whatever the name, as other path
    /// mutations do. No live call can reach such a file:
    /// tools that copy a file's ACL to their output, such as strip, and
    /// installs that set a capability on a staged binary both change files
    /// outside the recorded tree.
    async fn handle_xattr_change<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let pid = guest.pid();
        // The path calls take no dirfd; a relative path resolves against the
        // working directory, which replay keeps inside the root.
        let (in_replay_root, name, live_call, is_removal) = {
            let memory = guest.memory();
            let read = |name: Option<reverie::syscalls::CStrPtr<'_>>| {
                name.ok_or(Errno::EFAULT)
                    .and_then(|name| name.read(&memory))
            };
            match syscall {
                Syscall::Fsetxattr(call) => (
                    self.fd_is_in_replay_root(pid, call.fd()),
                    read(call.name()),
                    Syscall::Fsetxattr(call.with_flags(0)),
                    false,
                ),
                Syscall::Setxattr(call) => (
                    true,
                    read(call.name()),
                    Syscall::Setxattr(call.with_flags(0)),
                    false,
                ),
                Syscall::Lsetxattr(call) => (
                    true,
                    read(call.name()),
                    Syscall::Lsetxattr(call.with_flags(0)),
                    false,
                ),
                Syscall::Fremovexattr(call) => (
                    self.fd_is_in_replay_root(pid, call.fd()),
                    read(call.name()),
                    syscall,
                    true,
                ),
                Syscall::Removexattr(call) => (true, read(call.name()), syscall, true),
                Syscall::Lremovexattr(call) => (true, read(call.name()), syscall, true),
                _ => unreachable!("xattr change handler received {syscall:?}"),
            }
        };
        let recorded = next_event!(guest, Return)?;
        let name = name.unwrap_or_else(|error| {
            panic!("could not read the name of recorded xattr change {syscall:?}: {error}")
        });
        let governs_live_calls = !name.to_bytes().starts_with(b"user.");
        // A placeholder descriptor names a file the replay root lacks.
        let outcome = if in_replay_root {
            guest.inject_with_retry(live_call).await.map(drop)
        } else {
            Err(Errno::ENOENT)
        };
        match outcome {
            Ok(()) => {}
            Err(Errno::ENODATA) if is_removal => {}
            Err(error @ (Errno::ENOENT | Errno::ENOTDIR)) => {
                tracing::debug!(?syscall, %error, "replay xattr change kept virtual");
            }
            Err(error) if !governs_live_calls => {
                tracing::debug!(?syscall, %error, "replay xattr change kept virtual");
            }
            Err(error) => panic!(
                "replay root could not take recorded change to {name:?} {syscall:?}: {error}"
            ),
        }
        Ok(recorded)
    }

    // TODO-HUMAN-REVIEW(#3598)
    /// Replay `chdir(2)` or `fchdir(2)` from its recorded result and move the
    /// replay process's working directory with it. A successful recording
    /// carries the directory the call actually entered; that directory is
    /// created inside the replay root, as for a recorded `O_DIRECTORY` open,
    /// and entered by that resolved name. Neither the spelled path nor the
    /// descriptor is reused: the path may cross a host symlink or `..` that the
    /// replay root lacks, and the descriptor may be a replay placeholder. Every
    /// later `AT_FDCWD`-relative or legacy relative-path replay mutation trusts
    /// `/proc/<pid>/cwd`, so a stale or wrong directory would land them
    /// elsewhere; the resulting cwd is checked against the recording.
    async fn handle_working_directory_change<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let entered = PathBuf::from(std::ffi::OsString::from_vec(next_event!(guest, Bytes)?));
        assert!(
            entered.is_absolute(),
            "recorded {syscall:?} entered a relative directory {entered:?}"
        );
        self.materialize_recorded_directory(
            guest,
            syscall,
            libc::AT_FDCWD,
            &entered,
            OFlag::empty(),
        )
        .unwrap_or_else(|error| {
            panic!("failed to materialize recorded working directory {entered:?}: {error}")
        });
        // The injection stack holds well under PATH_MAX, so enter the directory
        // in pieces: the root first, then runs of components, each of which
        // fits the buffer. A single component is at most NAME_MAX bytes.
        let mut path = [0_u8; 512];
        let mut pieces = vec![b"/".to_vec()];
        for component in entered.components().skip(1) {
            let component = component.as_os_str().as_bytes();
            let last = pieces.last_mut().expect("pieces starts non-empty");
            let separator = usize::from(!last.ends_with(b"/"));
            if last.len() + separator + component.len() < path.len() {
                if separator == 1 {
                    last.push(b'/');
                }
                last.extend_from_slice(component);
            } else {
                pieces.push(component.to_vec());
            }
        }
        for piece in pieces {
            assert!(
                piece.len() < path.len(),
                "path component of {entered:?} exceeds NAME_MAX"
            );
            path.fill(0);
            path[..piece.len()].copy_from_slice(&piece);
            let mut stack = guest.stack().await;
            let path_addr = stack.push(path);
            let path_ptr = PathPtr::from_ptr(unsafe {
                path_addr.cast::<u8>().as_ptr().cast::<libc::c_char>()
            })
            .expect("guest stack address is non-null");
            let _path_guard = stack.commit()?;
            if let Err(error) = guest
                .inject_with_retry(Chdir::new().with_path(Some(path_ptr)))
                .await
            {
                panic!("replayed {syscall:?} into recorded {entered:?} failed: {error}");
            }
        }
        let actual = crate::record_replay_path::process_cwd_path(guest.tid())
            .unwrap_or_else(|error| panic!("could not read replayed working directory: {error}"));
        assert_eq!(
            actual, entered,
            "replayed {syscall:?} entered a different directory than the recording"
        );
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// Replay `flock(2)` by taking the lock again, not by consuming its recorded
    /// return value.
    ///
    /// `handle_simple` is only valid when nothing beyond the return value depends
    /// on a call. `flock` is the opposite case: its entire product is kernel state
    /// that a contender observes -- another process, or any other open file
    /// description in the same lock domain. Consuming the recorded `Return`
    /// without re-issuing the call makes a replayed run a program that merely
    /// *claims* to hold locks, so replaying a lockfile-based workload excludes
    /// nothing and a concurrent writer can corrupt what the recording protected.
    ///
    /// Only a descriptor that materialized in the replay root is a real kernel
    /// descriptor; the others are eventfd placeholders standing in for opens that
    /// replay serves from the log (see `handle_virtual_fd_create`). Locking a
    /// placeholder would lock the wrong object. A real file the guest created
    /// itself beneath the replay root is registered on demand here (and at
    /// its open), so the lock is really taken (issue 5007); a descriptor
    /// outside the root still fails closed rather than claim a lock.
    ///
    /// Unlike `handle_replayed_side_effect`, this re-issues the call even when
    /// the recording FAILED, and that is the load-bearing part. A recorded
    /// `EWOULDBLOCK` is the observable that proves the conflicting lock is
    /// really held; skipping the attempt on a failed record would leave replay
    /// unable to notice that no lock exists at all, because the only calls it
    /// checked would be the ones that succeed whether or not the lock domain is
    /// real. Re-issuing both outcomes makes the guest's own contention pattern
    /// the check: if a replayed run had faked its locks, the contended probe
    /// would come back `Ok` where the recording said `EWOULDBLOCK`.
    ///
    /// Consequence, stated rather than hidden: within the materialized set,
    /// replay now depends on real lock state the same way recording does. If a
    /// conflicting lock exists during replay but not during recording (or the
    /// reverse), the re-issued call disagrees with the recording and this
    /// reports the divergence instead of silently continuing. That is the
    /// intended failure direction: replay is not faithful under a contender the
    /// recording did not have, and saying so beats pretending. The exposure is
    /// narrow in practice, because a pre-existing host file is exactly what
    /// replay does NOT materialize -- the materialized set is dominated by
    /// files the guest itself created inside the replay root.
    /// Materializes the placeholder a recorded open reserved, so a
    /// following `flock` locks a real object: injects the recorded
    /// open for real beneath the replay root, dups it onto the
    /// placeholder's descriptor, and registers it. The pending entry
    /// is retired by any syscall that may have operated on the
    /// descriptor (`syscall_clears_pending`), so the placeholder
    /// cannot have served I/O the real descriptor would miss.
    ///
    /// Late is not the recorded moment, and the differences that
    /// matter refuse by name through `reason` rather than lock a
    /// substitute (dh-gg5uv review): `(dirfd, path)` must still
    /// resolve to the parent and object pinned at recorded-open time,
    /// and an `O_TRUNC` against an object that already existed then
    /// cannot be applied after I/O the recording did first. A fifo
    /// open in a single access mode waited for its peer at the
    /// recorded moment (a `flock` helper opens its lock fifo exactly
    /// that way); re-issuing it blocking could wait forever on a
    /// rendezvous that already happened, so it is re-issued with
    /// `O_NONBLOCK` and the recorded status flags are restored with
    /// `F_SETFL` once the real descriptor is in its slot. For a
    /// writer open that fails `ENXIO` because no reader is real in
    /// replay either, the failed open becomes the refusal reason.
    async fn materialize_pending_open_for_flock<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pending: &PendingPlaceholderOpen,
    ) -> Result<(), String> {
        if let Some(reason) = self.open_materialization_refusal(
            guest.tid(),
            pending.dirfd,
            &pending.path,
            pending.flags,
        ) && !self.fifo_resolves_in_replay_root(guest, pending.dirfd, &pending.path)
        {
            return Err(format!(
                "recorded open of {:?} does not resolve to a lockable object \
                 beneath the replay root ({reason})",
                pending.path
            ));
        }
        let now = pin_pending_open_object(guest.tid(), pending.dirfd, &pending.path);
        if pending.parent.is_none() {
            return Err(format!(
                "recorded open of {:?} could not pin its directory at open                  time, so re-resolution cannot be verified",
                pending.path
            ));
        }
        if pending.object.is_none() && !pending.flags.contains(OFlag::O_CREAT) {
            return Err(format!(
                "recorded open of {:?} did not resolve to an object beneath                  the replay root at open time, so there is no pinned object                  to materialize",
                pending.path
            ));
        }
        if pending.parent.is_some() && now.parent != pending.parent {
            return Err(format!(
                "recorded open of {:?} resolved under a different directory \
                 at flock time than at open time",
                pending.path
            ));
        }
        if pending.object.is_some() && now.object != pending.object {
            return Err(format!(
                "recorded open of {:?} named a different object at flock \
                 time than at open time",
                pending.path
            ));
        }
        if pending.flags.contains(OFlag::O_TRUNC)
            && (pending.object.is_some() || now.object.is_some())
        {
            return Err(format!(
                "recorded open of {:?} truncated an existing object at open \
                 time, which a late open cannot reproduce",
                pending.path
            ));
        }
        let access = pending.flags & OFlag::O_ACCMODE;
        let restore_status_flags = now.object_is_fifo
            && access != OFlag::O_RDWR
            && !pending.flags.contains(OFlag::O_NONBLOCK);
        let inject_flags = if restore_status_flags {
            pending.flags | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK
        } else {
            pending.flags | OFlag::O_CLOEXEC
        };
        let path_bytes = pending.path.as_os_str().as_bytes();
        // The path is staged on the guest stack in a fixed-size buffer
        // (`Stack::push` copies one sized value), and that stack is
        // far smaller than PATH_MAX — pushing a 4096-byte buffer for
        // every path panics on it. Stage the smallest tier that holds
        // the path, and refuse by name when no tier fits the stack
        // this guest actually has.
        async fn inject_open<const N: usize, G: Guest<Replayer>>(
            guest: &mut G,
            pending: &PendingPlaceholderOpen,
            path_bytes: &[u8],
            inject_flags: OFlag,
        ) -> Result<i64, String> {
            let mut path = [0_u8; N];
            path[..path_bytes.len()].copy_from_slice(path_bytes);
            let mut stack = guest.stack().await;
            if N > stack.capacity() {
                return Err(format!(
                    "recorded open path {:?} ({} bytes) does not fit the \
                     guest stack staging area ({} bytes)",
                    pending.path,
                    path_bytes.len(),
                    stack.capacity()
                ));
            }
            let path_addr = stack.push(path);
            let path_ptr = PathPtr::from_ptr(unsafe {
                path_addr.cast::<u8>().as_ptr().cast::<libc::c_char>()
            })
            .expect("guest stack address is non-null");
            let _path_guard = stack
                .commit()
                .map_err(|error| format!("could not stage the open path: {error}"))?;
            guest
                .inject_with_retry(
                    Openat::new()
                        .with_dirfd(pending.dirfd)
                        .with_path(Some(path_ptr))
                        .with_flags(inject_flags)
                        .with_mode(pending.mode),
                )
                .await
                .map_err(|error| format!("re-issued open of {:?} failed: {error}", pending.path))
        }
        let opened = match path_bytes.len() + 1 {
            0..=256 => inject_open::<256, _>(guest, pending, path_bytes, inject_flags).await,
            257..=512 => inject_open::<512, _>(guest, pending, path_bytes, inject_flags).await,
            513..=768 => inject_open::<768, _>(guest, pending, path_bytes, inject_flags).await,
            769..=896 => inject_open::<896, _>(guest, pending, path_bytes, inject_flags).await,
            897..=2048 => inject_open::<2048, _>(guest, pending, path_bytes, inject_flags).await,
            2049..=4096 => inject_open::<4096, _>(guest, pending, path_bytes, inject_flags).await,
            _ => Err(format!(
                "recorded open path {:?} is too long to re-issue",
                pending.path
            )),
        }?;
        let opened = opened as libc::c_int;
        // Note: the transient descriptor above needs one free slot
        // beyond the recorded table. A guest exactly at its
        // RLIMIT_NOFILE fails the re-issued open with EMFILE instead,
        // which surfaces as this function's refusal reason rather than
        // as a wrong lock.
        if opened != pending.fd {
            let dup_flags = if pending.flags.contains(OFlag::O_CLOEXEC) {
                OFlag::O_CLOEXEC
            } else {
                OFlag::empty()
            };
            if let Err(error) = guest
                .inject_with_retry(
                    Dup3::new()
                        .with_oldfd(opened)
                        .with_newfd(pending.fd)
                        .with_flags(dup_flags),
                )
                .await
            {
                // The swap failed; do not leak the materialized
                // descriptor into a replay that is refusing anyway.
                let _ = guest.inject_with_retry(Close::new().with_fd(opened)).await;
                return Err(format!("dup onto placeholder failed: {error}"));
            }
            // The swap is complete: the recorded descriptor now names
            // the real object, so a failed close of the transient
            // duplicate must not discard the materialization (and must
            // not be retried as a fresh close of a possibly reused
            // number). It leaks one unnamed descriptor, stated here.
            if let Err(error) = guest.inject_with_retry(Close::new().with_fd(opened)).await {
                tracing::warn!(
                    opened,
                    %error,
                    "close of materialized open's transient descriptor failed; leaking it"
                );
            }
        } else if !pending.flags.contains(OFlag::O_CLOEXEC) {
            // The re-issued open landed in the recorded slot itself
            // (the placeholder was replaced between reservation and
            // now), carrying the O_CLOEXEC the injection forced but
            // the recorded open did not request: undo it, so a later
            // recorded exec keeps the descriptor as recorded.
            guest
                .inject_with_retry(
                    Fcntl::new()
                        .with_fd(pending.fd)
                        .with_cmd(FcntlCmd::F_SETFD(0)),
                )
                .await
                .map_err(|error| {
                    format!("clearing forced O_CLOEXEC on materialized open failed: {error}")
                })?;
        }
        if restore_status_flags {
            // Undo the O_NONBLOCK added for the re-issued fifo open:
            // the recorded descriptor was blocking, and a later
            // recorded read or write must meet the status flags the
            // recording had. Only the F_SETFL-settable bits are
            // anyone's to restore.
            const SETTABLE: libc::c_int = libc::O_APPEND
                | libc::O_ASYNC
                | libc::O_DIRECT
                | libc::O_NOATIME
                | libc::O_NONBLOCK;
            guest
                .inject_with_retry(
                    Fcntl::new()
                        .with_fd(pending.fd)
                        .with_cmd(FcntlCmd::F_SETFL(pending.flags.bits() & SETTABLE)),
                )
                .await
                .map_err(|error| {
                    format!("restoring status flags on materialized open failed: {error}")
                })?;
        }
        // The pins were checked against a resolution the controller
        // made; the injected open re-resolves in the guest kernel.
        // Close that gap: the descriptor now in the slot must be the
        // object the flock-time resolution named, or this refuses
        // rather than locking whatever the name landed on.
        if let Some(expected) = now.object {
            let actual =
                std::fs::metadata(format!("/proc/{}/fd/{}", guest.tid().as_raw(), pending.fd))
                    .map(|metadata| ReplayFileIdentity::from_metadata(&metadata))
                    .ok();
            if actual != Some(expected) {
                return Err(format!(
                    "re-issued open of {:?} landed on a different object than                      the one pinned at flock time",
                    pending.path
                ));
            }
        }
        remember_materialized_file(guest.tid(), pending.fd);
        Ok(())
    }

    /// Whether `path` resolves to a fifo beneath the replay root. The
    /// regular-file materialization decision above deliberately does
    /// not cover fifos, but a fifo is lockable in place once the
    /// recorded `mkfifo` has been replayed into the root.
    fn fifo_resolves_in_replay_root<G: Guest<Self>>(
        &self,
        guest: &G,
        dirfd: libc::c_int,
        path: &Path,
    ) -> bool {
        // The calling thread's own descriptors, not the leader's: a
        // thread made without CLONE_FILES resolves `dirfd` in its own
        // table. Resolution refuses procfs symlinks, as the pins do,
        // so a magic link cannot name an out-of-root fifo as beneath
        // the root (dh-gg5uv review).
        let pid = guest.tid();
        let Ok(root) = crate::record_replay_path::open_process_root(pid) else {
            return false;
        };
        let start = if path.is_absolute() {
            crate::record_replay_path::open_process_root(pid)
        } else if dirfd == libc::AT_FDCWD {
            crate::record_replay_path::open_process_cwd(pid)
        } else {
            crate::record_replay_path::open_process_directory_fd(pid, dirfd)
        };
        let Ok(start) = start else {
            return false;
        };
        if !crate::record_replay_path::directory_is_beneath(&root, &start).unwrap_or(false) {
            return false;
        }
        crate::record_replay_path::resolve_existing_path_without_procfs_symlinks(
            &root, &start, path,
        )
        .map(|resolved| {
            std::fs::metadata(format!("/proc/self/fd/{}", resolved.object.as_raw_fd()))
                .is_ok_and(|metadata| metadata.file_type().is_fifo())
        })
        .unwrap_or(false)
    }

    async fn handle_flock<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Flock,
    ) -> Result<i64, Errno> {
        // The calling thread's descriptor table, not the leader's
        // (`guest.pid()`): a thread made without CLONE_FILES holds a
        // different file at this number in the leader's table.
        let tid = guest.tid();
        // A recorded open answered with a placeholder is materialized
        // FIRST, before any beneath-root admission of the descriptor
        // as it stands: the placeholder's `anon_inode:[eventfd]` link
        // must never be admitted as a lockable beneath-root object,
        // or the lock below would land on the shared anon inode and
        // every placeholder flock would contend with every other
        // (dh-gg5uv review). Taking the entry spends its whole
        // dup-alias family process-wide, so neither an alias nor an
        // inherited table copy can materialize a second open file
        // description for the one recorded open. The spend happens
        // only where this thread's descriptor really is the plain
        // placeholder, so a thread with a split descriptor table
        // cannot spend its sibling's entry over its own real file.
        if fd_is_plain_placeholder(tid, syscall.fd())
            && let Some(pending) = take_pending_placeholder_open(tid, syscall.fd())
            && let Err(reason) = self
                .materialize_pending_open_for_flock(guest, &pending)
                .await
        {
            panic!(
                "{FLOCK_OUTSIDE_REPLAY_ROOT_REFUSAL}: descriptor {} ({reason})",
                syscall.fd()
            );
        }
        // Never panic on an unregistered descriptor (issue 5007): a
        // file the guest created itself can reach flock before replay
        // registered it. Register it now when the descriptor is a
        // real pathname object beneath the replay root; the registry
        // names fifos beside regular files, by object identity, so a
        // later lock or unlock through any descriptor for the same
        // object is judged the same way.
        if !self.fd_is_in_replay_root(tid, syscall.fd())
            && file_is_beneath_replay_root(tid, syscall.fd())
        {
            remember_materialized_file(tid, syscall.fd());
        }
        if !self.fd_is_in_replay_root(tid, syscall.fd()) {
            panic!(
                "{FLOCK_OUTSIDE_REPLAY_ROOT_REFUSAL}: descriptor {}",
                syscall.fd()
            );
        }
        let recorded = next_event!(guest, Return);
        let call = Syscall::from(syscall);
        let actual = guest.inject_with_retry(call).await;
        assert_eq!(
            actual, recorded,
            "flock side effects diverged: replay observed {actual:?} where the recording \
             observed {recorded:?} for {call:?}"
        );
        recorded
    }

    async fn handle_unlinkat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Unlinkat,
    ) -> Result<i64, Errno> {
        if syscall.dirfd() == libc::AT_FDCWD
            || self.fd_is_in_replay_root(guest.pid(), syscall.dirfd())
        {
            self.handle_optional_path_removal(guest, syscall.into())
                .await
        } else {
            self.handle_simple(guest, syscall.into()).await
        }
    }

    async fn handle_optional_path_removal<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if let Ok(expected) = recorded {
            match guest.inject_with_retry(syscall).await {
                Ok(actual) => assert_eq!(
                    actual, expected,
                    "replayed path removal returned a different result"
                ),
                Err(error @ (Errno::ENOENT | Errno::ENOTDIR)) => {
                    tracing::debug!(
                        ?syscall,
                        %error,
                        "replay path unavailable; keeping path removal virtual"
                    );
                }
                Err(error) => {
                    panic!(
                        "replayed path removal {syscall:?} failed after recording returned {expected}: {error}"
                    );
                }
            }
        }
        recorded
    }

    async fn handle_optional_fd_position<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let Syscall::Lseek(call) = syscall else {
            unreachable!("descriptor-position handler received {syscall:?}");
        };
        // The calling thread's descriptor: a thread made without CLONE_FILES
        // has a table of its own, where the leader's (`guest.pid()`) can hold
        // another file at the same number (Codex review of
        // https://github.com/rrnewton/hermit/pull/4406).
        let thread = guest.tid();
        if !self.fd_is_in_replay_root(thread, call.fd()) {
            return self.handle_simple(guest, syscall).await;
        }
        // A directory's position is its file system's: SEEK_END is its size
        // on btrfs and a hash cookie on ext4, and the positions telldir gives
        // differ between file systems, so the materialized directory need not
        // answer as the recorded one did, and asserting that it does stopped
        // the replay (https://github.com/rrnewton/hermit/issues/4373). Replay
        // serves getdents from the recording (`handle_getdents64`), which no
        // physical position feeds, so the recorded result is the whole answer.
        let link = format!("/proc/{}/fd/{}", thread.as_raw(), call.fd());
        if std::fs::metadata(&link).is_ok_and(|metadata| metadata.is_dir()) {
            return next_event!(guest, Return);
        }
        if matches!(call.whence(), Whence::SEEK_DATA | Whence::SEEK_HOLE) {
            let recorded = next_event!(guest, Return);
            if let Ok(expected) = recorded {
                let duplicate =
                    crate::fd::duplicate_guest_thread_fd(guest.pid(), thread, call.fd())
                        .unwrap_or_else(|error| {
                            panic!("failed to duplicate replay file for extent seek: {error}")
                        });
                // SAFETY: duplicate owns the same open-file description and the
                // recorded extent seek returned this nonnegative offset.
                let actual =
                    unsafe { libc::lseek(duplicate.as_raw_fd(), expected, libc::SEEK_SET) };
                assert_eq!(
                    actual, expected,
                    "failed to restore replay position after extent seek"
                );
            }
            return recorded;
        }
        let recorded = next_event!(guest, Return);
        if let Ok(expected) = recorded {
            match guest.inject_with_retry(syscall).await {
                Ok(actual) => assert_eq!(
                    actual, expected,
                    "replayed descriptor position returned a different offset"
                ),
                Err(error @ (Errno::EBADF | Errno::ESPIPE)) => {
                    tracing::debug!(
                        ?syscall,
                        %error,
                        "replay descriptor is virtual; keeping position change virtual"
                    );
                }
                Err(error) => {
                    panic!(
                        "replayed descriptor position {syscall:?} failed after recording returned {expected}: {error}"
                    );
                }
            }
        }
        recorded
    }

    /// Replays the recorded result of `close` while preserving its physical FD
    /// namespace effect.
    async fn handle_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);

        // Linux releases a descriptor even when close reports EINTR, EIO,
        // ENOSPC, or EDQUOT. EBADF leaves the namespace unchanged, while
        // ERESTARTSYS means Reverie must restart the injection first.
        if !matches!(recorded, Err(Errno::EBADF | Errno::ERESTARTSYS))
            && let Err(error) = inject_physical_close(guest, syscall).await
        {
            if error == Errno::EBADF {
                // Some replayed descriptor sources, notably SCM_RIGHTS,
                // currently have no physical peer.
                tracing::debug!(?syscall, "replayed close had no physical descriptor");
            } else {
                tracing::warn!(
                    ?error,
                    "physical close during replay differed from the recorded result"
                );
            }
        }

        recorded
    }

    // Check if we received the expected syscall or not.
    fn expect_syscall<G: Guest<Self>>(&self, guest: &mut G, syscall: Syscall) {
        let thread = guest.tid();
        let next_count = guest.thread_state().count + 1;
        let debug_event = guest
            .thread_state_mut()
            .next_debug_event()
            .unwrap_or_else(|source| {
                panic!(
                    "Replay syscall stream ended unexpectedly for recording {} on thread {} at event {} while the guest executed {:?}: {}",
                    self.data.display(),
                    thread,
                    next_count,
                    syscall,
                    source,
                )
            });

        let actual_event = DebugEvent::new(syscall, &guest.memory());
        let exec_syscall = matches!(syscall, Syscall::Execve(_) | Syscall::Execveat(_));

        // Compare only the argument registers the syscall actually uses. Reverie
        // keeps all six raw registers in every typed syscall and derives
        // `PartialEq` over them, so unused registers (which hold arbitrary
        // leftover guest values) would otherwise produce false desyncs for any
        // syscall with fewer than six arguments.
        if normalize_unused_args(debug_event.syscall()) == normalize_unused_args(syscall)
            && debug_event.exec_request_matches(&actual_event)
        {
            return;
        }

        if guest.is_root_thread() && !guest.thread_state().bootstrapped && exec_syscall {
            // The controller-visible replay bootstrap pathname is deliberately
            // substituted before injection and therefore differs here.
            return;
        }

        let error = DesyncError {
            thread,
            stream_id: guest.thread_state().stream_id.clone(),
            count: guest.thread_state().count,
            actual: actual_event,
            expected: debug_event,
        };
        let summary = error.summary(&self.data, 16, 4).to_string();
        let report = match error.generate_report(&self.data) {
            Ok(report) => format!("Full desynchronization report: {}", report.display()),
            Err(report_error) => {
                format!("Could not write the full desynchronization report: {report_error}")
            }
        };

        panic!(
            "Replay diverged from recording {} on thread {} at syscall event {}. Re-record the workload with the same Hermit build after diagnosing the mismatch.\n{}\n{}",
            self.data.display(),
            thread,
            error.count,
            summary,
            report,
        );
    }

    /// Called for syscalls to explicitly let through. This should only be called
    /// for syscalls that cannot be recorded and are necessary for the program to
    /// function correctly. Examples of syscalls that fall into this category are
    /// ones that help with memory management (e.g., `brk`, `mprotect`, or
    /// `munmap`) or process management (e.g., `fork`, `vfork`, `clone`).
    ///
    /// For these syscalls, we just care about detecting dsynchronization and
    /// simply inject them to let them through.
    async fn let_through<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        // NOTE: Must use `inject_with_retry` here. Otherwise, we may end up
        // introducing non-determinism into the replay and popping multiple
        // syscall events.
        guest.inject_with_retry(syscall).await
    }

    /// Handles a syscall whose only value we care about is the return value
    /// (i.e., simple syscalls).
    ///
    /// For recording, this means we only record the return value of the syscall.
    /// For replay, this means we substitute the return value in lieu of actually
    /// performing the injection.
    ///
    /// The syscall must have two properties satisfied for this to be called:
    ///  1. The syscall must only have "input" arguments. That is, all arguments
    ///     must either be values or const pointers.
    ///  2. The execution of the program must not depend on anything else other
    ///     than the return value of the syscall. For example, `mmap` would violate
    ///     this rule since it affects later memory access.
    ///
    /// There are many syscalls who satisfy these two requirements.
    /// Replays an `fcntl` that writes its result through its argument
    /// (`crate::event::fcntl_output`): the recorded status, and on success the
    /// recorded bytes, written back to the guest's buffer
    /// (https://github.com/rrnewton/hermit/issues/2289).
    async fn handle_fcntl_output<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Fcntl,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let (buf, len) = crate::event::fcntl_output(&call)
                .unwrap_or_else(|| unreachable!("fcntl output replay called for {call:?}"));
            let bytes = next_event!(guest, Bytes)?;
            assert_eq!(bytes.len(), len, "recorded fcntl output has the wrong size");
            guest
                .memory()
                .write_exact(buf.ok_or(Errno::EFAULT)?, &bytes)?;
        }
        recorded
    }

    async fn handle_simple<G: Guest<Self>>(
        &self,
        guest: &mut G,
        _syscall: Syscall,
    ) -> Result<i64, Errno> {
        next_event!(guest, Return)
    }

    /// Replays epoll_ctl's recorded answer. A recorded success is reapplied
    /// in the kernel, so the replay's epoll set holds what the recording's
    /// held for the epoll_wait calls that replay in the kernel
    /// (`replay_kernel_side_effect`); a recorded failure changed nothing and
    /// is not reissued. A reapplied success can still fail where the
    /// descriptor is another kind of object during replay: a standard stream
    /// that `hermit replay` inherited from a file instead of a terminal, or an
    /// epoll descriptor received over `SCM_RIGHTS`, which is a placeholder
    /// here. The recorded answer is returned all the same, with a warning, so
    /// the guest sees what it saw while recording. A set holding a terminal,
    /// pipe or socket is never one whose waits replay in the kernel
    /// (`epoll_requires_replay_kernel_side_effect`).
    async fn handle_epoll_ctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let actual = guest.inject_with_retry(syscall).await;
            if actual != recorded {
                tracing::warn!(
                    ?syscall,
                    ?actual,
                    ?recorded,
                    "replay could not reapply a recorded epoll_ctl; returning the recorded answer"
                );
            }
        }
        recorded
    }

    #[cfg(target_arch = "x86_64")]
    async fn handle_dup2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let actual = guest.inject_with_retry(syscall).await;
            // Some source descriptors are virtual: open-family syscalls replay
            // their recorded return value without creating a live kernel fd.
            // Preserve that behavior when there is nothing to duplicate in the
            // replay process.
            if actual != Err(Errno::EBADF) {
                assert_eq!(actual, recorded, "dup2 fd-table mutation diverged");
                if let Syscall::Dup2(call) = syscall {
                    follow_pending_through_dup(guest.tid(), call.oldfd(), call.newfd(), false);
                }
            }
        }
        recorded
    }

    /// A recorded dup-family call (`dup`, `dup3`, `fcntl(F_DUPFD*)`):
    /// replayed as a side effect, then any pending flock open on the
    /// source descriptor is cloned onto the descriptor the recording
    /// produced, with that descriptor's close-on-exec bit.
    async fn handle_dup_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        operation: &str,
    ) -> Result<i64, Errno> {
        let (oldfd, cloexec) = match syscall {
            Syscall::Dup(call) => (call.oldfd(), false),
            Syscall::Dup3(call) => (call.oldfd(), call.flags().contains(OFlag::O_CLOEXEC)),
            Syscall::Fcntl(call) => (
                call.fd(),
                matches!(call.cmd(), FcntlCmd::F_DUPFD_CLOEXEC(_)),
            ),
            _ => unreachable!("dup-family handler received {syscall:?}"),
        };
        let recorded = self
            .handle_replayed_side_effect(guest, syscall, operation)
            .await;
        if let Ok(newfd) = recorded {
            follow_pending_through_dup(guest.tid(), oldfd, newfd as libc::c_int, cloexec);
        }
        recorded
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    async fn handle_replayed_side_effect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        operation: &str,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let actual = guest.inject_with_retry(syscall).await;
            assert_eq!(actual, recorded, "{operation} side effects diverged");
        }
        recorded
    }

    /// `setsockopt` changes kernel socket state that later live calls can
    /// observe, so a recorded success is reapplied and must succeed again.
    ///
    /// The exception is a replay placeholder. An accepted connection or a
    /// descriptor received over `SCM_RIGHTS` is an eventfd during replay, and
    /// every recorded operation on it is served from the log. A recorded success
    /// means the recorded descriptor was a socket, since `setsockopt` on an
    /// eventfd fails with `ENOTSOCK`, so an eventfd in its slot can only be such
    /// a placeholder and the option is not reapplied.
    async fn handle_setsockopt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let Syscall::Setsockopt(call) = syscall else {
            unreachable!("handle_setsockopt called for {syscall:?}");
        };
        if fd_is_eventfd(guest.tid(), call.fd()) {
            return next_event!(guest, Return);
        }
        self.handle_replayed_side_effect(guest, syscall, "setsockopt")
            .await
    }

    /// `shutdown` changes kernel socket state that later live calls observe on
    /// a socket replay recreated, such as a `socketpair` endpoint whose reads
    /// run live, so a recorded success is reapplied.
    ///
    /// Two kinds of socket hold their connection only in the log. A replay
    /// placeholder (an eventfd standing in for an accepted connection or a
    /// descriptor received over `SCM_RIGHTS`) serves the recorded result, as in
    /// [`Self::handle_setsockopt`]. A live socket whose `connect`, `bind` and
    /// `listen` were served from the log is unconnected during replay, so
    /// Linux reports `ENOTCONN` while still marking it shut down; every
    /// recorded operation on such a socket is served from the log as well.
    /// Linux's `AF_UNIX` shutdown never reports `ENOTCONN`, so on a Unix
    /// socket, such as a recreated socketpair endpoint, it is a divergence.
    // TODO-HUMAN-REVIEW(PR-3871): Reapply shutdown on live sockets during replay.
    async fn handle_shutdown<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let Syscall::Shutdown(call) = syscall else {
            unreachable!("handle_shutdown called for {syscall:?}");
        };
        if fd_is_eventfd(guest.tid(), call.fd()) {
            return next_event!(guest, Return);
        }
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let actual = guest.inject_with_retry(syscall).await;
            let unconnected = actual == Err(Errno::ENOTCONN)
                && !fd_is_unix_socket(guest.tid(), call.fd()).unwrap_or_else(|error| {
                    panic!(
                        "cannot tell whether shutdown's fd {} is a Unix socket: {error}",
                        call.fd()
                    )
                });
            assert!(
                actual == recorded || unconnected,
                "shutdown side effects diverged: replay observed {actual:?}, recording observed {recorded:?}"
            );
        }
        recorded
    }

    async fn handle_pidfd_getfd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        let actual = guest.inject_with_retry(syscall).await;
        if let Some(fd) = unexpected_pidfd_getfd_fd(&recorded, &actual) {
            let cleanup = guest.inject_with_retry(Close::new().with_fd(fd)).await;
            assert_eq!(
                cleanup,
                Ok(0),
                "pidfd_getfd replay diverged and cleanup of unexpected fd {fd} failed"
            );
        }
        assert_eq!(
            actual, recorded,
            "pidfd_getfd side effects diverged: replay observed {actual:?}, recording observed {recorded:?}"
        );
        recorded
    }

    // TODO-HUMAN-REVIEW(#557): Audit close_range fd-table replay semantics.
    async fn handle_close_range<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let actual = guest.inject_with_retry(syscall).await;
            assert_eq!(actual, recorded, "close_range side effects diverged");
        }
        recorded
    }
}

#[cfg(test)]
mod pending_flock_tests {
    use super::*;

    /// The pending registry is process-wide, so tests that remember or
    /// spend entries (or drop a materialization scope over them) must not
    /// run concurrently with each other.
    fn registry_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn entry(
        fd: libc::c_int,
        open_id: u64,
        placeholder_id: Option<&str>,
    ) -> PendingPlaceholderOpen {
        let pid = Pid::from_raw(std::process::id() as i32);
        PendingPlaceholderOpen {
            open_id,
            root: replay_root_identity(pid).unwrap(),
            tgid: guest_tgid_ppid(pid).unwrap().0,
            opener_tid: pid.as_raw(),
            fd,
            dirfd: libc::AT_FDCWD,
            path: PathBuf::from("pending-flock-test"),
            flags: OFlag::O_RDWR,
            mode: None,
            parent: None,
            object: None,
            placeholder_id: placeholder_id.map(str::to_owned),
        }
    }

    #[test]
    fn clearing_predicate_decodes_descriptors_and_keeps_queries() {
        use reverie::syscalls::*;
        let read9 = Syscall::Read(Read::new().with_fd(9));
        assert!(syscall_clears_pending(read9, 9));
        assert!(!syscall_clears_pending(read9, 8));
        // A pure query leaves no open-file-description state behind.
        assert!(!syscall_clears_pending(
            Syscall::Fstat(Fstat::new().with_fd(9)),
            9
        ));
        // The flock the entry exists for must not retire it first.
        assert!(!syscall_clears_pending(
            Syscall::Flock(Flock::new().with_fd(9)),
            9
        ));
        // Memory-borne descriptor lists clear for every descriptor.
        assert!(syscall_clears_pending(
            Syscall::Poll(Poll::new().with_fds(AddrMut::from_raw(0x1000))),
            9
        ));
        // close_range bounds are unsigned: ~0U covers descriptor 9.
        // (Untyped in Reverie's list, so it arrives by number.)
        let (_, mut args) = Syscall::Getpid(Getpid::new()).into_parts();
        args.arg0 = 3;
        args.arg1 = u32::MAX as usize;
        assert!(syscall_clears_pending(
            Syscall::from_raw(Sysno::close_range, args),
            9
        ));
        // A proven descriptor-free call keeps the entry.
        assert!(!syscall_clears_pending(Syscall::Getpid(Getpid::new()), 9));
    }

    #[test]
    fn spending_one_alias_retires_the_whole_open_family() {
        let _registry_guard = registry_test_lock();
        let pid = Pid::from_raw(std::process::id() as i32);
        // One recorded open (open_id line unique to this test in the
        // process-wide registry) under two descriptor aliases.
        let open_id = 0xf10c_0001;
        remember_pending_placeholder_open(entry(1_000_001, open_id, None));
        remember_pending_placeholder_open(entry(1_000_002, open_id, None));
        assert!(take_pending_placeholder_open(pid, 1_000_001).is_some());
        assert!(
            take_pending_placeholder_open(pid, 1_000_002).is_none(),
            "the dup alias of a spent recorded open must not materialize again"
        );
    }

    #[test]
    fn pending_open_preserves_its_recorded_base_directory() {
        // The JVM hsperf shape: the recorded open is relative to a
        // directory descriptor that is closed (or a cwd that is
        // restored) before the recorded flock. The pending entry must
        // remember the absolute guest path the open named, not the raw
        // (dirfd, "9") pair whose meaning dies with that state.
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("perf");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("9"), b"perf").unwrap();
        let dir_file = File::open(&nested).unwrap();
        let pid = Pid::from_raw(std::process::id() as i32);
        let pending = pending_placeholder_open(
            pid,
            1_000_004,
            dir_file.as_raw_fd(),
            PathBuf::from("9"),
            OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            None,
        )
        .unwrap();
        assert_eq!(pending.dirfd, libc::AT_FDCWD);
        assert_eq!(pending.path, nested.join("9"));
        assert_eq!(
            pending.parent,
            Some(identity_for_fd(dir_file.as_raw_fd()).unwrap())
        );
        assert_eq!(
            pending.object,
            Some(identity_for_fd(File::open(nested.join("9")).unwrap().as_raw_fd()).unwrap())
        );
        // The AT_FDCWD form names the same transformation through the
        // opening thread's cwd at record time.
        assert_eq!(
            guest_absolute_open_path(pid, libc::AT_FDCWD, Path::new("9")),
            crate::record_replay_path::process_cwd_path(pid)
                .ok()
                .map(|cwd| cwd.join("9"))
        );
    }

    #[test]
    fn materialization_refusal_names_its_gate() {
        // The flock refusal is only diagnosable if the gate that made
        // a recorded open virtual names itself: an existing regular
        // file and a creatable missing file pass, while a missing
        // file, a non-directory parent, and a directory target each
        // refuse for a different stated reason.
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("9");
        std::fs::write(&file, b"perf").unwrap();
        let pid = Pid::from_raw(std::process::id() as i32);
        let replayer = Replayer::default();
        let refusal = |path: &Path, flags: OFlag| {
            replayer.open_materialization_refusal(pid, libc::AT_FDCWD, path, flags)
        };
        assert_eq!(refusal(&file, OFlag::O_RDONLY), None);
        let missing = directory.path().join("missing");
        let reason = refusal(&missing, OFlag::O_RDONLY).unwrap();
        assert!(
            reason.contains("cannot be resolved"),
            "missing-file reason: {reason}"
        );
        assert_eq!(
            refusal(&missing, OFlag::O_RDWR | OFlag::O_CREAT),
            None,
            "a missing final component under an existing parent is creatable"
        );
        assert!(
            !missing.exists(),
            "the decision alone must not create the file"
        );
        let through_file = file.join("9");
        let reason = refusal(&through_file, OFlag::O_RDWR | OFlag::O_CREAT).unwrap();
        assert!(
            reason.contains("cannot be resolved"),
            "non-directory-parent reason: {reason}"
        );
        let reason = refusal(directory.path(), OFlag::O_RDONLY).unwrap();
        assert!(
            reason.contains("not a regular file"),
            "directory-target reason: {reason}"
        );
    }

    #[test]
    fn scope_drop_removes_pending_entries_keyed_by_a_later_root() {
        let _registry_guard = registry_test_lock();
        // A guest chroot mid-run opens under a different root identity
        // than the scope was created with; entries remembered under
        // that later root must still die with the scope.
        let scope_root = tempfile::tempdir().unwrap();
        let scope_fd = crate::record_replay_path::open_directory_path(scope_root.path()).unwrap();
        let scope = ReplayMaterializationScope::new(scope_fd.as_raw_fd()).unwrap();
        let later_root = tempfile::tempdir().unwrap();
        let later_fd = crate::record_replay_path::open_directory_path(later_root.path()).unwrap();
        let mut pending = entry(1_000_005, 0xf10c_0004, None);
        pending.root = identity_for_fd(later_fd.as_raw_fd()).unwrap();
        remember_pending_placeholder_open(pending);
        assert!(
            pending_placeholder_opens()
                .lock()
                .unwrap()
                .iter()
                .any(|pending| pending.open_id == 0xf10c_0004)
        );
        drop(scope);
        assert!(
            !pending_placeholder_opens()
                .lock()
                .unwrap()
                .iter()
                .any(|pending| pending.open_id == 0xf10c_0004),
            "pending entry keyed by the post-chroot root survived scope drop"
        );
    }

    #[test]
    fn distinct_placeholders_at_one_number_do_not_evict_each_other() {
        let _registry_guard = registry_test_lock();
        // Two entries for the same thread and descriptor number but
        // different placeholder eventfds (the split-table shape):
        // remembering the second must not destroy the first, and the
        // spend for a descriptor whose live eventfd-id matches
        // neither (this process has no such descriptor) takes only
        // an unpinned opener entry — pinned ones stay for their own
        // tables.
        remember_pending_placeholder_open(entry(1_000_003, 0xf10c_0002, Some("111")));
        remember_pending_placeholder_open(entry(1_000_003, 0xf10c_0003, Some("222")));
        let remaining = pending_placeholder_opens()
            .lock()
            .unwrap()
            .iter()
            .filter(|pending| pending.fd == 1_000_003)
            .count();
        assert_eq!(remaining, 2);
        pending_placeholder_opens()
            .lock()
            .unwrap()
            .retain(|pending| pending.fd != 1_000_003);
    }
}

#[cfg(test)]
mod exec_snapshot_tests {
    use super::*;

    fn replayer_with_snapshot(
        contents: &[u8],
    ) -> (tempfile::TempDir, Replayer, crate::event::ExecImage) {
        let directory = tempfile::tempdir().unwrap();
        let digest = detcore::Digest::new(contents);
        let snapshots = directory.path().join(crate::consts::EXEC_FILES_NAME);
        std::fs::create_dir(&snapshots).unwrap();
        std::fs::write(snapshots.join(digest.to_string()), contents).unwrap();
        let replayer = Replayer {
            data: directory.path().to_path_buf(),
            ..Default::default()
        };
        let image = crate::event::ExecImage {
            digest,
            mode: 0o755,
        };
        (directory, replayer, image)
    }

    #[test]
    fn recorded_exec_snapshot_digest_is_verified() {
        let (directory, replayer, image) = replayer_with_snapshot(b"recorded image");
        std::fs::write(
            directory
                .path()
                .join(crate::consts::EXEC_FILES_NAME)
                .join(image.digest.to_string()),
            b"corrupt image",
        )
        .unwrap();

        let error = replayer.open_exec_snapshot(&image).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("digest mismatch"));
    }

    #[test]
    fn missing_recorded_exec_snapshot_fails_closed() {
        let (directory, replayer, image) = replayer_with_snapshot(b"recorded image");
        std::fs::remove_file(
            directory
                .path()
                .join(crate::consts::EXEC_FILES_NAME)
                .join(image.digest.to_string()),
        )
        .unwrap();

        assert_eq!(
            replayer.open_exec_snapshot(&image).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn materialized_object_pin_survives_unlink_until_scope_drop() {
        let directory = tempfile::tempdir().unwrap();
        let root = crate::record_replay_path::open_directory_path(directory.path()).unwrap();
        let scope = ReplayMaterializationScope::new(root.as_raw_fd()).unwrap();
        let root_identity = identity_for_fd(root.as_raw_fd()).unwrap();
        let path = directory.path().join("image");
        std::fs::write(&path, b"recorded").unwrap();
        let object: OwnedFd = File::open(&path).unwrap().into();
        let identity = identity_for_fd(object.as_raw_fd()).unwrap();
        register_materialized(root_identity, object).unwrap();
        std::fs::remove_file(&path).unwrap();

        let registry = materialized_files()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let pin = registry
            .get(&root_identity)
            .and_then(|files| files.get(&identity))
            .expect("registered object remains pinned after unlink");
        let pinned = std::fs::metadata(format!("/proc/self/fd/{}", pin.as_raw_fd())).unwrap();
        assert_eq!(
            (pinned.dev(), pinned.ino()),
            (identity.device, identity.inode)
        );
        drop(registry);

        std::fs::write(&path, b"replacement").unwrap();
        let replacement = std::fs::metadata(&path).unwrap();
        assert_ne!(
            (replacement.dev(), replacement.ino()),
            (identity.device, identity.inode)
        );
        assert_eq!(registered_materialized_count(root.as_raw_fd()), 1);

        drop(scope);
        assert_eq!(registered_materialized_count(root.as_raw_fd()), 0);
    }
}

#[cfg(test)]
mod pidfd_getfd_replay_tests {
    use super::*;

    #[test]
    fn exact_success_and_errno_need_no_cleanup() {
        assert_eq!(unexpected_pidfd_getfd_fd(&Ok(7), &Ok(7)), None);
        assert_eq!(
            unexpected_pidfd_getfd_fd(&Err(Errno::EBADF), &Err(Errno::EBADF)),
            None
        );
    }

    #[test]
    fn unexpected_success_is_owned_for_cleanup_before_refusal() {
        assert_eq!(
            unexpected_pidfd_getfd_fd(&Err(Errno::EBADF), &Ok(9)),
            Some(9)
        );
        assert_eq!(unexpected_pidfd_getfd_fd(&Ok(7), &Ok(9)), Some(9));
        assert_eq!(unexpected_pidfd_getfd_fd(&Ok(7), &Err(Errno::EPERM)), None);
    }
}

#[cfg(test)]
mod accept_stand_in_tests {
    use std::os::fd::AsRawFd;

    use super::*;

    fn eventfd(count: u32) -> OwnedFd {
        // SAFETY: eventfd(2) returns a new descriptor that only this test owns.
        unsafe {
            let fd = libc::eventfd(count, libc::EFD_CLOEXEC);
            assert!(fd >= 0, "eventfd: {}", io::Error::last_os_error());
            OwnedFd::from_raw_fd(fd)
        }
    }

    fn pid() -> Pid {
        Pid::from_raw(std::process::id() as i32)
    }

    fn is_generic(message: &str) -> bool {
        message.starts_with("replay FD namespace diverged:") && !message.contains("3880")
    }

    #[test]
    fn eventfd_count_is_read_from_its_own_fdinfo_line() {
        let fdinfo = "pos:\t0\nflags:\t02000002\nmnt_id:\t18\nino:\t5138\n\
                      eventfd-count:         3880acc7\neventfd-id: 8606\neventfd-semaphore: 0\n";
        assert_eq!(parse_eventfd_count(fdinfo), Some(0x3880_acc7));
        let fdinfo = fdinfo.replace("3880acc7", "       0");
        assert_eq!(parse_eventfd_count(&fdinfo), Some(0));
        assert_eq!(parse_eventfd_count("pos:\t0\nflags:\t02\nino:\t1\n"), None);
    }

    #[test]
    fn a_stand_in_and_its_duplicates_name_the_accept_refusal() {
        let stand_in = eventfd(ACCEPT_STAND_IN_COUNT);
        let duplicate = stand_in.try_clone().unwrap();
        for fd in [stand_in.as_raw_fd(), duplicate.as_raw_fd()] {
            assert!(holds_accept_stand_in(pid(), fd));
            assert!(slot_divergence_message(pid(), fd, 9).starts_with(ACCEPT_SLOT_REFUSAL));
        }
    }

    #[test]
    fn an_ordinary_placeholder_gives_the_generic_divergence() {
        let placeholder = eventfd(0);
        let (reader, _writer) = nix::unistd::pipe().unwrap();
        for fd in [placeholder.as_raw_fd(), reader.as_raw_fd(), -1] {
            assert!(!holds_accept_stand_in(pid(), fd));
            assert!(is_generic(&slot_divergence_message(pid(), fd, 9)));
        }
    }

    #[test]
    fn a_placeholder_reusing_a_closed_stand_ins_id_gives_the_generic_divergence() {
        let stand_in = eventfd(ACCEPT_STAND_IN_COUNT);
        let stand_in_id = eventfd_id(stand_in.as_raw_fd());
        drop(stand_in);
        let placeholder = eventfd(0);
        // Linux usually hands the freed id straight back out. Whether or not
        // it did here, the new placeholder must not read as a stand-in.
        let _reused = eventfd_id(placeholder.as_raw_fd()) == stand_in_id;
        assert!(!holds_accept_stand_in(pid(), placeholder.as_raw_fd()));
        assert!(is_generic(&slot_divergence_message(
            pid(),
            placeholder.as_raw_fd(),
            9
        )));
    }

    fn eventfd_id(fd: libc::c_int) -> Option<String> {
        std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
            .ok()?
            .lines()
            .find_map(|line| Some(line.strip_prefix("eventfd-id:")?.trim().to_owned()))
    }
}

#[cfg(test)]
mod shutdown_replay_tests {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    use super::*;

    #[test]
    fn unix_socket_lookup_tells_a_socketpair_from_tcp_and_a_pipe() {
        let pid = Pid::from_raw(std::process::id() as i32);
        let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
        // SAFETY: socket(2) returns a new descriptor that only this test owns.
        let tcp = unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            assert!(fd >= 0, "socket: {}", io::Error::last_os_error());
            OwnedFd::from_raw_fd(fd)
        };
        let (reader, _writer) = nix::unistd::pipe().unwrap();

        assert!(fd_is_unix_socket(pid, left.as_raw_fd()).unwrap());
        assert!(fd_is_unix_socket(pid, right.as_raw_fd()).unwrap());
        assert!(!fd_is_unix_socket(pid, tcp.as_raw_fd()).unwrap());
        assert!(!fd_is_unix_socket(pid, reader.as_raw_fd()).unwrap());
        assert!(fd_is_unix_socket(pid, -1).is_err());
    }

    #[test]
    fn unix_table_rows_match_only_their_inode_column() {
        // A path need not be UTF-8 (the second row's).
        let table = b"Num       RefCount Protocol Flags    Type St Inode Path\n\
                      0000000000000000: 00000002 00000000 00010000 0001 01 23456 /run/a\n\
                      0000000000000000: 00000002 00000000 00010000 0001 01 45678 @\xff\x01\n\
                      0000000000000000: 00000003 00000000 00000000 0001 03 34567\n";
        assert!(unix_table_lists_inode(table, 23456));
        assert!(unix_table_lists_inode(table, 34567));
        assert!(unix_table_lists_inode(table, 45678));
        assert!(!unix_table_lists_inode(table, 2));
        assert!(!unix_table_lists_inode(table, 10000));
        assert_eq!(
            socket_inode(std::path::Path::new("socket:[34567]")),
            Some(34567)
        );
        assert_eq!(socket_inode(std::path::Path::new("pipe:[34567]")), None);
    }
}
