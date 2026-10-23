/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `mmap` is a tricky case. When using `MAP_ANONYMOUS` with a file descriptor of
//! -1, we are effectively allocating memory. If ASLR is turned off, this should
//! be deterministic and the mmap can be let through in this case. However, when
//! mapping a file descriptor, things get tricky. Memory writes from one thread
//! can affect the memory reads from another thread. During recording, we should
//! record the entire contents of the buffer. During replay, we need some way to
//! allocate the same size buffer at the same address. We do this by injecting
//!
//! ```ignore
//! mmap(desired_addr, length, flags | MAP_ANONYMOUS, -1, 0)
//! ```
//!
//! in order to get a blank memory map of the right size. We can then fill this
//! with the previously recorded bytes.
//!
//! A `MAP_SHARED` mapping of a file cannot be a copy: Linux backs every shared
//! mapping of one file with the file's own pages, so a store through one is
//! read through all the others. Replay backs each recorded shared file with
//! one file of its own ([`SharedBacking`]) and maps every shared mapping of
//! the file as a real `MAP_SHARED` view of that file at the recorded offset.
//! Overlapping, adjacent, split, shrunk and moved mappings then share exactly
//! the pages Linux shares, because the kernel keeps them as mappings of one
//! file.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::fmt::Display;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use reverie::Errno;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Close;
use reverie::syscalls::Madvise;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Mmap;
use reverie::syscalls::Mprotect;
use reverie::syscalls::OFlag;
use reverie::syscalls::Openat;
use reverie::syscalls::PathPtr;
use reverie::syscalls::Prlimit64;
use reverie::syscalls::ProtFlags;
use reverie::syscalls::Recvmsg;
use reverie::syscalls::Socketpair;

use super::Replayer;
use crate::event::MadviseRefill;
use crate::event::SharedMapping;

const PAGE_SIZE: usize = 4096;

/// How replay refuses a `MAP_SHARED` file mapping it cannot make a view of
/// the recorded file's backing. A copy in its place would not share stores
/// with the file's other mappings, so the guest would read stale bytes where
/// it read fresh ones while recording, and replay would go on wrong.
pub(super) const SHARED_MAPPING_REFUSAL: &str = "replay refused: replay cannot make a \
    MAP_SHARED file mapping share the file's memory with its other mappings";

fn refuse(step: &str, error: impl Display) -> ! {
    panic!("{SHARED_MAPPING_REFUSAL}: {step}: {error}")
}

/// A file by device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileId {
    device: u64,
    inode: u64,
}

impl FileId {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

/// A recorded shared file in one replay: the replay's root directory, the
/// container's private root every guest process of the replay shares,
/// whatever PID namespace it is in, and the file's identity in the
/// recording. PID namespaces do not divide a file's pages: Linux keeps one
/// page cache per file, which every process maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SharedFile {
    root: FileId,
    recorded: FileId,
}

/// The memory behind every replay mapping of one recorded shared file.
///
/// Linux keeps one copy of a file's pages, which every shared mapping of it
/// maps, in every process. The backing is that copy: a file this tracer
/// holds, which each replayed shared mapping of the file maps at its recorded
/// offset, in whichever guest process makes it. A fork's child inherits its
/// parent's mappings as mappings of the same file, as Linux has the child map
/// the same file. The backing outlives its mappings until no guest task maps
/// it ([`prune`]), so a mapping made after the others went away starts from a
/// fresh one, filled from its own recorded bytes.
///
/// The backing is a file with a fresh name of its own directly beneath the
/// replay's root, made when the backing is and unlinked when it is dropped,
/// which the guest opens by that name ([`Replayer::open_view`]). The guest
/// learns the name from no call but that injected open: replay answers the
/// guest's directory reads, stats and opens from the recording, which never
/// held it. Where no file can be made there, the backing is a memfd, which
/// only a guest receive can install ([`Replayer::push_descriptor`]).
#[derive(Debug)]
struct SharedBacking {
    file: OwnedFd,
    /// The backing's identity as `stat` gives it.
    file_id: FileId,
    /// The backing's identity as `/proc/<pid>/maps` names a mapping of it,
    /// read from the first view replay makes of it ([`note_view`]): the
    /// device a file system shows there need not be the one `stat` gives (a
    /// btrfs subvolume's `stat` device is its own, its mappings show the file
    /// system's). `None` until it is read, and a backing whose listed
    /// identity is unknown is never pruned, since no listing could show it.
    view: Option<FileId>,
    /// The replay root, as a path descriptor. Held for the backing's life, so
    /// the root's inode cannot be given to another replay's root while the
    /// key names it.
    root: OwnedFd,
    /// The backing's name beneath the root, when it is a file there.
    name: Option<String>,
    /// The replay objects through which the guest writes the recorded file:
    /// the file a descriptor of a mapping named, when replay holds a regular
    /// file of its own there (a file it materialized, or a memfd the guest
    /// made in replay). Each is held open, so its inode cannot name another
    /// file while it is linked here.
    objects: Vec<(FileId, OwnedFd)>,
}

impl SharedBacking {
    /// A new backing: a file of a fresh name beneath the replay root `root`,
    /// or a memfd when none can be made there.
    fn new(root: OwnedFd) -> io::Result<Self> {
        let name = backing_name();
        let path = std::ffi::CString::new(name.as_str()).expect("a backing name holds no NUL");
        // SAFETY: the path is a valid C string; the result is checked.
        let fd = unsafe {
            libc::openat(
                root.as_raw_fd(),
                path.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        let (file, name) = if fd >= 0 {
            // SAFETY: openat returned a new descriptor that nothing else owns.
            (unsafe { OwnedFd::from_raw_fd(fd) }, Some(name))
        } else {
            tracing::debug!(
                error = %io::Error::last_os_error(),
                "cannot make a file beneath the replay root; backing a shared file with a memfd"
            );
            // SAFETY: the name is a valid C string; the result is checked.
            let fd = unsafe {
                libc::memfd_create(c"hermit-replay-shared-file".as_ptr(), libc::MFD_CLOEXEC)
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: memfd_create returned a new descriptor that nothing else owns.
            (unsafe { OwnedFd::from_raw_fd(fd) }, None)
        };
        let backing = Self {
            file_id: FileId::of(&File::from(file.try_clone()?).metadata()?),
            view: None,
            file,
            root,
            name,
            objects: Vec::new(),
        };
        Ok(backing)
    }
}

impl Drop for SharedBacking {
    fn drop(&mut self) {
        if let Some(name) = &self.name
            && let Err(error) = unlink_backing(self.root.as_fd(), name)
        {
            tracing::debug!(%error, name, "cannot unlink a shared file's backing");
        }
    }
}

type SharedBackings = HashMap<SharedFile, SharedBacking>;

fn shared_backings() -> MutexGuard<'static, SharedBackings> {
    static BACKINGS: OnceLock<Mutex<SharedBackings>> = OnceLock::new();
    BACKINGS
        .get_or_init(Default::default)
        .lock()
        .expect("should not be poisoned: no code panics while holding it")
}

/// Every guest task replay has started (`track_task`), whose mappings decide
/// which backings are still mapped ([`prune`]).
fn tracked_tasks() -> MutexGuard<'static, std::collections::BTreeSet<i32>> {
    static TASKS: OnceLock<Mutex<std::collections::BTreeSet<i32>>> = OnceLock::new();
    TASKS
        .get_or_init(Default::default)
        .lock()
        .expect("should not be poisoned: no code panics while holding it")
}

/// Remembers guest task `tid`, which replay has just started, as one whose
/// mappings keep backings alive.
pub(super) fn track_task(tid: i32) {
    tracked_tasks().insert(tid);
}

/// Adds the files a `/proc/<pid>/maps` listing maps to `mapped`.
fn add_mapped_files(maps: &str, mapped: &mut HashSet<FileId>) {
    for line in maps.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(_range), Some(_perms), Some(_offset), Some(device), Some(inode)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let Some((major, minor)) = device.split_once(':') else {
            continue;
        };
        let (Ok(major), Ok(minor), Ok(inode)) = (
            u32::from_str_radix(major, 16),
            u32::from_str_radix(minor, 16),
            inode.parse::<u64>(),
        ) else {
            continue;
        };
        if inode != 0 {
            mapped.insert(FileId {
                device: libc::makedev(major, minor),
                inode,
            });
        }
    }
}

/// The file a `/proc/<pid>/maps` listing shows mapped at `address`, the start
/// of a mapping.
fn mapping_at(maps: &str, address: usize) -> Option<FileId> {
    let line = maps.lines().find(|line| {
        line.split_once('-')
            .and_then(|(start, _)| usize::from_str_radix(start, 16).ok())
            == Some(address)
    })?;
    let mut mapped = HashSet::new();
    add_mapped_files(line, &mut mapped);
    mapped.into_iter().next()
}

/// Remembers how `/proc/<tid>/maps` names `file`'s backing, from the view of
/// it guest task `tid` has just mapped at `address`.
fn note_view(file: SharedFile, tid: Pid, address: usize) {
    let shown = std::fs::read_to_string(format!("/proc/{}/maps", tid.as_raw()))
        .ok()
        .and_then(|maps| mapping_at(&maps, address));
    match (shown, shared_backings().get_mut(&file)) {
        (Some(shown), Some(backing)) => backing.view = Some(shown),
        // Kept until a later view shows how it is listed (`SharedBacking::view`).
        (None, Some(backing)) => tracing::debug!(
            ?file,
            stat = ?backing.file_id,
            "cannot read how a backing's view is listed"
        ),
        (_, None) => {}
    }
}

/// The files the tasks `tids` map, read with `maps` (`/proc/<tid>/maps`),
/// and the tasks that are gone, whose listing does not exist. `None` when any
/// other listing could not be read: the result would be incomplete, and a
/// backing missing from it may still be mapped.
fn mapped_files(
    tids: impl IntoIterator<Item = i32>,
    maps: impl Fn(i32) -> io::Result<String>,
) -> (Option<HashSet<FileId>>, Vec<i32>) {
    let mut mapped = HashSet::new();
    let mut gone = Vec::new();
    for tid in tids {
        match maps(tid) {
            Ok(listing) => add_mapped_files(&listing, &mut mapped),
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                gone.push(tid)
            }
            Err(error) => {
                tracing::debug!(tid, %error, "cannot list a guest task's mappings");
                return (None, gone);
            }
        }
    }
    (Some(mapped), gone)
}

/// Drops the backings no tracked guest task maps any more. A later mapping of
/// the file starts from a new backing and its own recorded bytes, which are
/// the file's contents when it was made. Only a complete listing drops
/// anything: when any task's mappings cannot be read, every backing is kept,
/// since keeping an unmapped one until the tracer exits costs only memory and
/// dropping a mapped one would end its sharing.
fn prune(backings: &mut SharedBackings) {
    if backings.is_empty() {
        return;
    }
    let tids: Vec<i32> = tracked_tasks().iter().copied().collect();
    let (mapped, gone) = mapped_files(tids, |tid| {
        std::fs::read_to_string(format!("/proc/{tid}/maps"))
    });
    {
        let mut tasks = tracked_tasks();
        for tid in gone {
            tasks.remove(&tid);
        }
    }
    retain_mapped(backings, mapped.as_ref());
}

/// Keeps the backings `mapped` names, or every backing when the listing is
/// incomplete (`None`).
fn retain_mapped(backings: &mut SharedBackings, mapped: Option<&HashSet<FileId>>) {
    if let Some(mapped) = mapped {
        backings.retain(|_, backing| backing.view.is_none_or(|view| mapped.contains(&view)));
    }
}

/// How a guest process opens a backing for a view of it.
enum Opening {
    /// By its name beneath the replay root, which is the process's root.
    Name(String),
    /// By a link of that name into the process's own root, made for the open
    /// only: the process chrooted away from the replay root (`real_root`).
    Link { real_root: OwnedFd, name: String },
    /// By a receive: the backing is a memfd, which no name reaches.
    Receive,
}

/// A shared file's backing, at least `end` bytes long, made if replay holds
/// none: a duplicate of its file, and how guest process `pid` opens it. The
/// backing belongs to the replay, by the replay's real root (its
/// `TempChroot`, `super::replay_root_of`) whatever root the process chrooted
/// into, and is made beneath that real root; without a known replay root,
/// the process's own root stands for it.
fn backing_of(pid: Pid, recorded: FileId, end: u64) -> io::Result<(SharedFile, File, Opening)> {
    let own = FileId::of(&std::fs::metadata(format!("/proc/{}/root", pid.as_raw()))?);
    let (root, real_root) = match super::replay_root_of(pid) {
        Some(((device, inode), real_root)) => (FileId { device, inode }, Some(real_root)),
        None => (own, None),
    };
    let file = SharedFile { root, recorded };
    let (backing, opening) = {
        let mut backings = shared_backings();
        prune(&mut backings);
        let backing = match backings.entry(file) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(SharedBacking::new(match real_root {
                Some(real_root) => real_root,
                None => crate::record_replay_path::open_process_root(pid)?,
            })?),
        };
        let opening = match &backing.name {
            None => Opening::Receive,
            Some(name) if own == root => Opening::Name(name.clone()),
            Some(name) => Opening::Link {
                real_root: backing.root.try_clone()?,
                name: name.clone(),
            },
        };
        (backing.file.try_clone()?, opening)
    };
    let backing = File::from(backing);
    if backing.metadata()?.len() < end {
        backing.set_len(end)?;
    }
    Ok((file, backing, opening))
}

/// A link of a backing's name into a chrooted guest's root, for one open;
/// dropping it unlinks it.
struct BackingAlias {
    root: OwnedFd,
    name: String,
}

impl BackingAlias {
    /// Links `name` beneath `real_root` as a fresh name beneath `root`.
    fn link(real_root: BorrowedFd<'_>, name: &str, root: OwnedFd) -> io::Result<Self> {
        let alias = backing_name();
        let (source, target) = (
            std::ffi::CString::new(name).expect("a backing name holds no NUL"),
            std::ffi::CString::new(alias.as_str()).expect("a backing name holds no NUL"),
        );
        // SAFETY: both names are valid C strings and both descriptors are open.
        let linked = unsafe {
            libc::linkat(
                real_root.as_raw_fd(),
                source.as_ptr(),
                root.as_raw_fd(),
                target.as_ptr(),
                0,
            )
        };
        if linked != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { root, name: alias })
    }
}

impl Drop for BackingAlias {
    fn drop(&mut self) {
        if let Err(error) = unlink_backing(self.root.as_fd(), &self.name) {
            tracing::debug!(%error, name = self.name, "cannot unlink a backing's alias");
        }
    }
}

/// The descriptor of `backing` a receive hands the guest for a mapping of a
/// descriptor opened with `access`: one reopened read-only for a read-only
/// one, so the guest never holds the backing open for writing where the
/// recording did not, and the backing itself otherwise.
fn handed_descriptor(backing: &File, access: i32) -> io::Result<OwnedFd> {
    if access & libc::O_ACCMODE != libc::O_RDONLY {
        return Ok(backing.try_clone()?.into());
    }
    let path = std::ffi::CString::new(format!("/proc/self/fd/{}", backing.as_raw_fd()))
        .expect("a descriptor path holds no NUL");
    // SAFETY: the path is a valid C string; the result is checked.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The replay object descriptor `fd` of thread `tid` names, if it is a
/// regular file of its own: an eventfd or other anonymous inode that stands
/// in for a file shares its inode with every other one, so it names nothing.
fn replay_object(tid: Pid, fd: RawFd) -> Option<FileId> {
    let path = format!("/proc/{}/fd/{fd}", tid.as_raw());
    let metadata = std::fs::metadata(&path).ok()?;
    let anonymous = std::fs::read_link(&path).is_ok_and(|target| {
        target
            .as_os_str()
            .as_encoded_bytes()
            .starts_with(b"anon_inode:")
    });
    (metadata.file_type().is_file() && !anonymous).then(|| FileId::of(&metadata))
}

/// Links the replay object descriptor `fd` of thread `tid` in process `pid`
/// names to `file`'s backing, so that replayed writes through it reach the
/// backing too.
fn link_object(file: SharedFile, pid: Pid, tid: Pid, fd: RawFd) {
    let Some(object) = replay_object(tid, fd) else {
        return;
    };
    let mut backings = shared_backings();
    if backings.iter().any(|(other, backing)| {
        *other != file && backing.objects.iter().any(|(id, _)| *id == object)
    }) {
        // One replay object names one recorded file. Two names would mean
        // replay lost track of a descriptor; writes through it reach neither.
        tracing::warn!(
            ?file,
            ?object,
            "replay object already backs another shared file"
        );
        return;
    }
    let Some(backing) = backings.get_mut(&file) else {
        return;
    };
    if backing.objects.iter().any(|(id, _)| *id == object) {
        return;
    }
    match crate::fd::duplicate_guest_thread_fd(pid, tid, fd) {
        Ok(pin) => backing.objects.push((object, pin)),
        Err(error) => tracing::debug!(%error, fd, "cannot hold a shared file's replay object"),
    }
}

/// The backing that descriptor `fd` of thread `tid` writes, if it names a
/// replay object linked to one. `tid`'s own descriptor table is the one
/// read: a thread made without `CLONE_FILES` has a table of its own.
fn object_backing(tid: Pid, fd: RawFd) -> Option<File> {
    if shared_backings().is_empty() {
        return None;
    }
    let object = replay_object(tid, fd)?;
    let backings = shared_backings();
    let backing = backings
        .values()
        .find(|backing| backing.objects.iter().any(|(id, _)| *id == object))?;
    Some(File::from(backing.file.try_clone().ok()?))
}

/// Whether replayed writes through descriptor `fd` of thread `tid` reach a
/// shared file's backing.
pub(super) fn writes_shared_backing(tid: Pid, fd: RawFd) -> bool {
    object_backing(tid, fd).is_some()
}

/// Applies a replayed write of `bytes` at `offset` through descriptor `fd` of
/// thread `tid` to the backing of the shared file it names, as Linux applies
/// a write to the pages every mapping of the file maps.
pub(super) fn write_shared_backing(
    tid: Pid,
    fd: RawFd,
    bytes: &[u8],
    offset: u64,
) -> io::Result<()> {
    match object_backing(tid, fd) {
        Some(backing) => backing.write_all_at(bytes, offset),
        None => Ok(()),
    }
}

/// Applies a replayed `ftruncate` to `length` through descriptor `fd` of
/// thread `tid` to the backing of the shared file it names: the bytes past
/// `length` read as zeros again, as Linux drops a truncated file's pages.
/// The backing keeps its size, so a mapping past the end reads zeros where
/// Linux would raise SIGBUS, as replay's copies always did.
pub(super) fn truncate_shared_backing(tid: Pid, fd: RawFd, length: u64) -> io::Result<()> {
    let Some(backing) = object_backing(tid, fd) else {
        return Ok(());
    };
    let size = backing.metadata()?.len();
    if length >= size {
        return Ok(());
    }
    let (start, len) = (
        libc::off_t::try_from(length).map_err(io::Error::other)?,
        libc::off_t::try_from(size - length).map_err(io::Error::other)?,
    );
    // SAFETY: backing is an open file; the range is checked above.
    let result = unsafe {
        libc::fallocate(
            backing.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            start,
            len,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::EOPNOTSUPP) {
        return Err(error);
    }
    // A file system that punches no holes: the same zeros, written.
    let zeros = vec![0u8; 64 * 1024];
    let mut offset = length;
    while offset < size {
        let chunk =
            usize::try_from(size - offset).map_or(zeros.len(), |left| left.min(zeros.len()));
        backing.write_all_at(&zeros[..chunk], offset)?;
        offset += chunk as u64;
    }
    Ok(())
}

/// The value of `field` (such as `Seccomp_filters`) in a `/proc/<pid>/status`
/// listing.
fn status_field(status: &str, field: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        line.strip_prefix(field)?
            .strip_prefix(':')?
            .trim()
            .parse()
            .ok()
    })
}

/// Whether the calls replay adds to a guest to make a view (`openat`, `mmap`
/// and `close`; `prlimit64` at a full table; `socketpair` and `recvmsg` on the
/// fallback) can meet no seccomp filter but Reverie's own, from the tracer's
/// own status listing `tracer`, or why not. The guest cannot install a filter
/// (Detcore refuses `seccomp(2)` and `PR_SET_SECCOMP`), so its filters are
/// Reverie's, which admits Reverie's injections, and those the tracer
/// inherited from whatever launched it. Such a filter is unknown: whatever it
/// allowed of the recorded calls, it may kill or trap a call replay adds, at
/// Reverie's injection address, before any named refusal. The command line
/// refuses to start under one unless `--unsafe-ignore-host-seccomp` admits
/// it, and a library caller of replay is not checked there.
fn injected_calls_admitted(tracer: &str) -> Result<(), String> {
    match status_field(tracer, "Seccomp") {
        None | Some(0) => Ok(()),
        Some(mode) => Err(format!(
            "the replay runs under a seccomp filter it inherited from whatever launched it \
             (Seccomp: {mode}, Seccomp_filters: {}), which is not known to allow the calls \
             replay would add to the guest to make the view, and could kill or trap one",
            status_field(tracer, "Seccomp_filters").map_or("unknown".to_owned(), |n| n.to_string())
        )),
    }
}

/// `injected_calls_admitted` for this tracer thread.
fn tracer_admits_injected_calls() -> Result<(), String> {
    let path = "/proc/thread-self/status";
    injected_calls_admitted(
        &std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?,
    )
}

/// A name for a backing beneath the replay root: one no guest file is likely
/// to have, and never the same twice in this tracer.
fn backing_name() -> String {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        ".hermit-shared-file-{}-{nanos:x}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Unlinks `name` beneath the directory `root`.
fn unlink_backing(root: BorrowedFd<'_>, name: &str) -> io::Result<()> {
    let name = std::ffi::CString::new(name).expect("a view name holds no NUL");
    // SAFETY: the path is a valid C string and the descriptor is open.
    if unsafe { libc::unlinkat(root.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sends `fd` over the datagram socket `socket` in one `SCM_RIGHTS` message.
fn send_descriptor(socket: BorrowedFd<'_>, fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: all-zero is a valid `msghdr`.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = space;
    // SAFETY: the control buffer has room for one header and one descriptor,
    // so CMSG_FIRSTHDR names a header inside it.
    unsafe {
        let message = libc::CMSG_FIRSTHDR(&header);
        (*message).cmsg_level = libc::SOL_SOCKET;
        (*message).cmsg_type = libc::SCM_RIGHTS;
        (*message).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(message).cast::<RawFd>(), fd.as_raw_fd());
    }
    // SAFETY: header and the buffers it names live until the call returns.
    let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &header, libc::MSG_NOSIGNAL) };
    match sent {
        1 => Ok(()),
        -1 => Err(io::Error::last_os_error()),
        sent => Err(io::Error::other(format!(
            "sendmsg sent {sent} bytes, not 1"
        ))),
    }
}

/// The `msghdr` of a one-byte receive: one `iovec` at guest address `iov`
/// and a control buffer of `control_len` bytes at guest address `control`.
fn receive_header(iov: usize, control: usize, control_len: usize) -> libc::msghdr {
    // SAFETY: all-zero is a valid `msghdr`.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = iov as *mut libc::iovec;
    header.msg_iovlen = 1;
    header.msg_control = control as *mut libc::c_void;
    header.msg_controllen = control_len;
    header
}

/// The one descriptor a receive delivered, from the bytes of its `msghdr`
/// and its control buffer after the call, or why there is none.
fn received_descriptor(header: &[u8], control: &[u8]) -> Result<RawFd, String> {
    assert_eq!(header.len(), size_of::<libc::msghdr>());
    // SAFETY: the bytes are a `msghdr`, read without alignment; its pointers
    // are guest addresses and are not followed.
    let header: libc::msghdr = unsafe { std::ptr::read_unaligned(header.as_ptr().cast()) };
    if header.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err("the descriptor did not fit the receiver's table (MSG_CTRUNC)".into());
    }
    // SAFETY: CMSG_LEN only computes a size.
    let one = unsafe { libc::CMSG_LEN(size_of::<RawFd>() as u32) } as usize;
    // SAFETY: as above.
    let data = unsafe { libc::CMSG_LEN(0) } as usize;
    if header.msg_controllen < one || control.len() < one {
        return Err(format!(
            "{} control bytes name no descriptor",
            header.msg_controllen
        ));
    }
    // SAFETY: the buffer holds at least one `cmsghdr`, read without alignment.
    let message: libc::cmsghdr = unsafe { std::ptr::read_unaligned(control.as_ptr().cast()) };
    if message.cmsg_level != libc::SOL_SOCKET
        || message.cmsg_type != libc::SCM_RIGHTS
        || message.cmsg_len != one
    {
        return Err(format!("unexpected control message {message:?}"));
    }
    Ok(RawFd::from_ne_bytes(
        control[data..data + size_of::<RawFd>()]
            .try_into()
            .expect("a descriptor is four bytes"),
    ))
}

/// The guest's own descriptor of a backing, made by
/// [`Replayer::push_descriptor`], and the descriptor limit to put back once
/// it is closed, if the push had to raise it.
struct GuestDescriptor {
    fd: RawFd,
    restore: Option<libc::rlimit64>,
}

impl Replayer {
    pub(super) async fn handle_mmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Mmap,
    ) -> Result<i64, Errno> {
        let flags = syscall.flags();

        // Let anonymous mappings through. This should already be deterministic
        // because ASLR is disabled.
        if flags.contains(MapFlags::MAP_ANONYMOUS) {
            return guest.inject_with_retry(syscall).await;
        }

        // Get the event. We need to know what pointer to use for the map.
        let event = next_event!(guest, Mmap)?;

        // This is safe since we only record non-NULL pointers.
        let addr = unsafe { AddrMut::<u8>::from_raw_unchecked(event.addr) };

        let len = syscall.len();
        let prot = syscall.prot();

        if let Some(shared) = event.shared
            && flags.contains(MapFlags::MAP_SHARED)
        {
            return Ok(self
                .map_shared_view(guest, syscall, event.addr, &event.buf, shared)
                .await);
        }

        // On replay, we can't actually map the original file because it may not
        // exist. Instead, change this to be an anonymous mapping and write the
        // bytes that we recorded to it. This has the same effect, but without
        // requiring to map a real file.
        //
        // Write permission is also needed to be able to write the recorded
        // bytes to the mapping. After the data has been written, it can be
        // reset to the original protection value with a call to `mprotect`.
        let ptr = guest
            .inject_with_retry(
                syscall
                    .with_addr(Some(addr.cast::<libc::c_void>().into()))
                    .with_prot(prot | ProtFlags::PROT_WRITE)
                    .with_flags(flags | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            )
            .await?;

        // Make sure we got the pointer we wanted.
        assert_eq!(
            ptr as usize, event.addr,
            "Failed to inject mmap at desired address"
        );

        // Fill in the memory map.
        guest.memory().write_exact(addr.cast(), &event.buf).unwrap();

        // Reset the page protection to the original value (if it didn't already
        // have PROT_WRITE) so that we still correctly mimic the page protection
        // of the original mapping.
        if !prot.contains(ProtFlags::PROT_WRITE) {
            guest
                .inject_with_retry(
                    Mprotect::new()
                        .with_addr(Some(addr.cast()))
                        .with_len(len)
                        .with_protection(prot),
                )
                .await?;
        }

        Ok(ptr)
    }

    /// Replays a `MAP_SHARED` mapping of a recorded file as a view of the
    /// file's backing at the recorded offset, at the recorded address.
    ///
    /// The recorded bytes are what the mapping showed when it was made, which
    /// Linux showed through every other mapping of those pages then, so they
    /// are written to the backing first. That also brings the file's older
    /// mappings up to date with a write replay could not apply to the backing.
    /// Bytes past the recorded ones are left as the backing holds them: they
    /// are past the file's end, or pages the recording could not read, and a
    /// new backing holds zeros there, as a replay copy did.
    async fn map_shared_view<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Mmap,
        address: usize,
        recorded: &[u8],
        shared: SharedMapping,
    ) -> i64 {
        let (pid, tid) = (guest.pid(), guest.tid());
        assert_eq!(
            u64::try_from(syscall.offset()).ok(),
            Some(shared.offset),
            "Replayed shared mmap offset differs from the recording"
        );
        // Before any call replay adds to the guest.
        if let Err(reason) = tracer_admits_injected_calls() {
            refuse("admit the calls that make the view", reason);
        }
        let recorded_file = FileId {
            device: shared.device,
            inode: shared.inode,
        };
        let end = u64::try_from(syscall.len().next_multiple_of(PAGE_SIZE))
            .ok()
            .and_then(|len| shared.offset.checked_add(len))
            .unwrap_or_else(|| refuse("size the backing", "offset and length overflow"));
        let (file, backing, opening) = backing_of(pid, recorded_file, end)
            .unwrap_or_else(|error| refuse("make the backing", error));
        backing
            .write_all_at(recorded, shared.offset)
            .unwrap_or_else(|error| refuse("write the recorded bytes", error));
        link_object(file, pid, tid, syscall.fd());

        let opened = match &opening {
            Opening::Name(name) => Some(self.open_view(guest, name, shared.access).await),
            Opening::Link { real_root, name } => {
                self.open_linked_view(guest, real_root.as_fd(), name, shared.access)
                    .await
            }
            Opening::Receive => None,
        };
        let view = match opened {
            Some(view) => view,
            None => {
                let handed = handed_descriptor(&backing, shared.access)
                    .unwrap_or_else(|error| refuse("reopen the backing read-only", error));
                self.push_descriptor(guest, handed.as_fd()).await
            }
        };
        // This is safe since we only record non-NULL pointers.
        let addr = unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(address) };
        let mapped = guest
            .inject_with_retry(syscall.with_addr(Some(addr.into())).with_fd(view.fd))
            .await;
        self.close_guest_descriptor(guest, view.fd).await;
        if let Some(limit) = view.restore {
            self.set_descriptor_limit(guest, limit)
                .await
                .unwrap_or_else(|errno| refuse("restore the descriptor limit", errno));
        }
        let ptr = mapped.unwrap_or_else(|errno| refuse("map the backing", errno));
        assert_eq!(
            ptr as usize, address,
            "Failed to inject mmap at desired address"
        );
        note_view(file, tid, address);
        ptr
    }

    /// Gives the guest a descriptor of its own for the backing named `name`
    /// beneath the replay root, using only calls a guest that maps a file
    /// makes itself: the guest opens `/<name>` with the mapped descriptor's
    /// recorded access mode (`access`), before it runs again. The caller maps
    /// and closes the descriptor. A table the guest has filled up to its
    /// descriptor limit gets one more slot for the duration
    /// (`GuestDescriptor::restore`).
    async fn open_view<G: Guest<Self>>(
        &self,
        guest: &mut G,
        name: &str,
        access: i32,
    ) -> GuestDescriptor {
        let path = format!("/{name}");
        let mut bytes = [0_u8; 128];
        assert!(
            path.len() < bytes.len(),
            "a backing's name fits the guest's path buffer"
        );
        bytes[..path.len()].copy_from_slice(path.as_bytes());
        let flags = OFlag::from_bits_truncate((access & libc::O_ACCMODE) | libc::O_CLOEXEC);
        let mut restore = None;
        let opened = match self.inject_open(guest, bytes, flags).await {
            Err(Errno::EMFILE) => {
                restore = Some(self.raise_descriptor_limit(guest, 1).await);
                self.inject_open(guest, bytes, flags).await
            }
            opened => opened,
        };
        let fd = opened.unwrap_or_else(|errno| refuse("open the backing", errno));
        GuestDescriptor { fd, restore }
    }

    /// As `open_view`, for a guest process that chrooted away from the replay
    /// root `real_root`: the backing's `name` there is linked into the
    /// process's own root under a fresh name for the open, and unlinked again
    /// before the guest runs. `None` when the link cannot be made (another
    /// file system, `EXDEV`); nothing was injected then.
    async fn open_linked_view<G: Guest<Self>>(
        &self,
        guest: &mut G,
        real_root: BorrowedFd<'_>,
        name: &str,
        access: i32,
    ) -> Option<GuestDescriptor> {
        let alias = crate::record_replay_path::open_process_root(guest.pid())
            .and_then(|root| BackingAlias::link(real_root, name, root))
            .map_err(|error| {
                tracing::debug!(%error, "cannot link a backing into a chrooted guest's root")
            })
            .ok()?;
        let view = self.open_view(guest, &alias.name, access).await;
        drop(alias);
        Some(view)
    }

    async fn inject_open<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: [u8; 128],
        flags: OFlag,
    ) -> Result<RawFd, Errno> {
        let mut stack = guest.stack().await;
        let path = stack.push(path);
        // SAFETY: the stack address is non-null and holds the NUL-terminated path.
        let path = PathPtr::from_ptr(unsafe { path.cast::<u8>().as_ptr().cast::<libc::c_char>() })
            .expect("guest stack address is non-null");
        let guard = stack.commit()?;
        let opened = guest
            .inject_with_retry(
                Openat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(Some(path))
                    .with_flags(flags),
            )
            .await;
        drop(guard);
        Ok(opened? as RawFd)
    }

    /// Gives the guest a descriptor of its own for `backing` where the replay
    /// root cannot name it: it injects a socketpair, takes one end, sends
    /// `backing` through it and has the guest receive it. The socketpair is
    /// closed again, so the guest's table gains only the returned descriptor,
    /// which the caller closes before the guest runs. A table the guest has
    /// filled up to its descriptor limit gets two more slots for the duration
    /// (`GuestDescriptor::restore`). The caller has checked that no seccomp
    /// filter but Reverie's own and the tracer's sees these calls.
    async fn push_descriptor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        backing: BorrowedFd<'_>,
    ) -> GuestDescriptor {
        let (pid, tid) = (guest.pid(), guest.tid());
        let mut restore = None;
        let pair = match self.inject_socketpair(guest).await {
            Ok(pair) => pair,
            Err(Errno::EMFILE) => {
                restore = Some(self.raise_descriptor_limit(guest, 2).await);
                self.inject_socketpair(guest).await.unwrap_or_else(|errno| {
                    refuse("make a socketpair at the raised descriptor limit", errno)
                })
            }
            Err(errno) => refuse("make a socketpair", errno),
        };
        let sender = crate::fd::duplicate_guest_thread_fd(pid, tid, pair[0])
            .unwrap_or_else(|error| refuse("take the socketpair's sending end", error));
        self.close_guest_descriptor(guest, pair[0]).await;
        send_descriptor(sender.as_fd(), backing)
            .unwrap_or_else(|error| refuse("send the backing", error));
        drop(sender);
        let received = self.inject_receive(guest, pair[1]).await;
        self.close_guest_descriptor(guest, pair[1]).await;
        let fd = received.unwrap_or_else(|error| refuse("receive the backing", error));
        GuestDescriptor { fd, restore }
    }

    async fn inject_socketpair<G: Guest<Self>>(&self, guest: &mut G) -> Result<[RawFd; 2], Errno> {
        let mut stack = guest.stack().await;
        let pair = stack.reserve::<[RawFd; 2]>();
        let guard = stack.commit()?;
        guest
            .inject_with_retry(
                Socketpair::new()
                    .with_family(libc::AF_UNIX)
                    .with_type(libc::SOCK_DGRAM | libc::SOCK_CLOEXEC)
                    .with_protocol(0)
                    .with_usockvec(Some(pair)),
            )
            .await?;
        let fds = guest.memory().read_value(pair)?;
        drop(guard);
        Ok(fds)
    }

    /// Receives one descriptor on the guest's datagram socket `socket`, into
    /// the guest's table, close-on-exec.
    async fn inject_receive<G: Guest<Self>>(
        &self,
        guest: &mut G,
        socket: RawFd,
    ) -> Result<RawFd, String> {
        const CONTROL: usize = 64;
        const IOVEC: usize = size_of::<libc::iovec>();
        const HEADER: usize = size_of::<libc::msghdr>();
        // `msghdr` and `iovec` hold raw pointers and are not `Send`, so
        // neither a value of them nor an address typed by them may live
        // across an `.await` here: the slots are named by their bytes.
        let mut stack = guest.stack().await;
        let byte = stack.reserve::<[u8; 8]>();
        let iov = stack.reserve::<[u8; IOVEC]>();
        let control = stack.reserve::<[u8; CONTROL]>();
        let header = stack.reserve::<[u8; HEADER]>();
        let guard = stack.commit().map_err(|errno| errno.to_string())?;
        guest
            .memory()
            .write_value(
                iov.cast::<libc::iovec>(),
                &libc::iovec {
                    iov_base: byte.as_raw() as *mut libc::c_void,
                    iov_len: 1,
                },
            )
            .map_err(|errno| errno.to_string())?;
        guest
            .memory()
            .write_value(
                header.cast::<libc::msghdr>(),
                &receive_header(iov.as_raw(), control.as_raw(), CONTROL),
            )
            .map_err(|errno| errno.to_string())?;
        let received = guest
            .inject_with_retry(
                Recvmsg::new()
                    .with_sockfd(socket)
                    .with_msg(Some(header.cast()))
                    .with_flags(libc::MSG_CMSG_CLOEXEC),
            )
            .await;
        let header_bytes = guest
            .memory()
            .read_value(header)
            .map_err(|errno| errno.to_string())?;
        let control_bytes = guest
            .memory()
            .read_value(control)
            .map_err(|errno| errno.to_string())?;
        drop(guard);
        match received {
            Ok(1) => received_descriptor(&header_bytes, &control_bytes),
            Ok(count) => Err(format!("received {count} bytes, not 1")),
            Err(errno) => Err(errno.to_string()),
        }
    }

    async fn close_guest_descriptor<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) {
        if let Err(errno) = guest.inject_with_retry(Close::new().with_fd(fd)).await {
            refuse("close a descriptor replay made", errno);
        }
    }

    /// Raises the guest's soft descriptor limit by `extra`, within its hard
    /// limit, and returns the limit to restore.
    async fn raise_descriptor_limit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        extra: u64,
    ) -> libc::rlimit64 {
        let mut stack = guest.stack().await;
        let old = stack.reserve::<libc::rlimit64>();
        let guard = stack
            .commit()
            .unwrap_or_else(|errno| refuse("read the descriptor limit", errno));
        guest
            .inject_with_retry(
                Prlimit64::new()
                    .with_pid(0)
                    .with_resource(libc::RLIMIT_NOFILE)
                    .with_old_rlim(Some(old)),
            )
            .await
            .unwrap_or_else(|errno| refuse("read the descriptor limit", errno));
        let limit = guest
            .memory()
            .read_value(old)
            .unwrap_or_else(|errno| refuse("read the descriptor limit", errno));
        drop(guard);
        let raised = limit
            .rlim_cur
            .checked_add(extra)
            .filter(|raised| *raised <= limit.rlim_max)
            .unwrap_or_else(|| {
                refuse(
                    "raise the descriptor limit",
                    format!(
                        "the guest's descriptor table is full at its hard limit {}",
                        limit.rlim_max
                    ),
                )
            });
        self.set_descriptor_limit(
            guest,
            libc::rlimit64 {
                rlim_cur: raised,
                rlim_max: limit.rlim_max,
            },
        )
        .await
        .unwrap_or_else(|errno| refuse("raise the descriptor limit", errno));
        limit
    }

    async fn set_descriptor_limit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        limit: libc::rlimit64,
    ) -> Result<(), Errno> {
        let mut stack = guest.stack().await;
        let new = stack.push(limit);
        let guard = stack.commit()?;
        guest
            .inject_with_retry(
                Prlimit64::new()
                    .with_pid(0)
                    .with_resource(libc::RLIMIT_NOFILE)
                    .with_new_rlim(Some(new)),
            )
            .await?;
        drop(guard);
        Ok(())
    }

    /// Replays a guest-semantic `madvise` recorded by the recorder's
    /// `handle_madvise`: applies the advice live to the recorded prefix, then
    /// restores the file-backed contents the recording observed.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3589): Audit the madvise refill and WIPEONFORK prefix.
    pub(super) async fn handle_madvise<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Madvise,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Madvise)?;
        let len = syscall.len();
        assert!(
            event.live_len <= len,
            "Recorded madvise prefix {} exceeds the guest length {}",
            event.live_len,
            len
        );

        if event.live_len == len {
            // Some recorded outcomes depend on the mapping type, which replay's
            // anonymous stand-ins do not preserve: MADV_GUARD_INSTALL on a
            // private file mapping fails with EINVAL before Linux 6.15, and
            // advice on device or hugetlbfs mappings can fail where anonymous
            // memory accepts it. Those fail the assertion below loudly.
            let result = guest.inject_with_retry(syscall).await;
            assert_eq!(
                result,
                event.result,
                "Replayed madvise({:?}, {}, {}) diverged from the recording; replay \
                 cannot reproduce this outcome",
                syscall.addr(),
                len,
                syscall.advice()
            );
        } else if event.live_len > 0 {
            // The recording failed at a file mapping after this prefix, so the
            // recorded error is the result. Before reaching it, Linux may have
            // passed holes (ENOMEM) or a shared mapping (EINVAL); anything else
            // means the prefix was not advised as it was when recording.
            let result = guest
                .inject_with_retry(syscall.with_len(event.live_len))
                .await;
            assert!(
                matches!(result, Ok(0) | Err(Errno::ENOMEM) | Err(Errno::EINVAL)),
                "Replayed madvise prefix ({:?}, {}, {}) returned {:?}; replay cannot \
                 reproduce the recorded {:?}",
                syscall.addr(),
                event.live_len,
                syscall.advice(),
                result,
                event.result
            );
        }

        if !event.refills.is_empty() {
            let mem_path = format!("/proc/{}/mem", guest.tid().as_raw());
            let mem = std::fs::File::open(&mem_path)
                .unwrap_or_else(|error| panic!("Cannot replay madvise: {mem_path}: {error}"));
            for refill in &event.refills {
                // Only write pages that differ: the range may include mappings
                // replay maps from the real file, such as executables, where a
                // write would create private copies the recording did not have.
                let mut current = vec![0u8; refill.bytes.len()];
                let readable = read_prefix(&mem, refill.addr, &mut current);
                for (offset, run) in differing_pages(&current[..readable], &refill.bytes) {
                    self.write_refill(guest, refill, offset, run).await;
                }
            }
        }

        event.result
    }
}

impl Replayer {
    /// Writes `refill.bytes[offset..offset + len]`, lifting write protection
    /// for the duration of the write.
    ///
    /// The advice has already been applied, so a failure here panics rather
    /// than return an error the recording never saw.
    async fn write_refill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        refill: &MadviseRefill,
        offset: usize,
        len: usize,
    ) {
        let start = refill.addr + offset;
        let prot = ProtFlags::from_bits_truncate(refill.prot);
        if !prot.contains(ProtFlags::PROT_WRITE) {
            guest
                .inject_with_retry(protection(start, len, prot | ProtFlags::PROT_WRITE))
                .await
                .unwrap_or_else(|err| {
                    panic!("Cannot unprotect madvise refill at {start:#x}: {err}")
                });
        }
        // This is safe since the recorder only records mapped addresses.
        let addr = unsafe { AddrMut::<u8>::from_raw_unchecked(start) };
        guest
            .memory()
            .write_exact(addr, &refill.bytes[offset..offset + len])
            .unwrap();
        if !prot.contains(ProtFlags::PROT_WRITE) {
            guest
                .inject_with_retry(protection(start, len, prot))
                .await
                .unwrap_or_else(|err| {
                    panic!("Cannot reprotect madvise refill at {start:#x}: {err}")
                });
        }
    }
}

fn protection(start: usize, len: usize, protection: ProtFlags) -> Mprotect {
    // This is safe since the recorder only records mapped addresses.
    let addr = unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(start) };
    Mprotect::new()
        .with_addr(Some(addr))
        .with_len(len)
        .with_protection(protection)
}

/// Reads guest memory at `addr` through `/proc/<tid>/mem`, which also reads
/// mappings without `PROT_READ`, and returns how many leading bytes it read.
fn read_prefix(mem: &std::fs::File, addr: usize, buf: &mut [u8]) -> usize {
    let mut done = 0;
    while done < buf.len() {
        match mem.read_at(&mut buf[done..], (addr + done) as u64) {
            Ok(n) if n > 0 => done += n,
            _ => break,
        }
    }
    done
}

/// Page-aligned `(offset, len)` runs where `recorded` differs from `current`.
/// Bytes past the end of `current` (unreadable in replay) always differ.
fn differing_pages(current: &[u8], recorded: &[u8]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for offset in (0..recorded.len()).step_by(PAGE_SIZE) {
        let end = (offset + PAGE_SIZE).min(recorded.len());
        if current.get(offset..end) == Some(&recorded[offset..end]) {
            continue;
        }
        match runs.last_mut() {
            Some((start, len)) if *start + *len == offset => *len = end - *start,
            _ => runs.push((offset, end - offset)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::OpenOptionsExt;

    use super::*;

    /// `/proc/<pid>/maps` names a file by its device, in hexadecimal
    /// `major:minor`, and its inode; anonymous memory has inode 0.
    #[test]
    fn mapped_files_are_read_by_device_and_inode() {
        let maps = "7f0000000000-7f0000002000 rw-s 00000000 00:01 4242 /memfd:x (deleted)\n\
                    7f0000002000-7f0000003000 rw-p 00000000 00:00 0\n\
                    7f0000003000-7f0000004000 r--p 00001000 fd:1a 77 /usr/lib/libc.so.6\n";
        let mut mapped = HashSet::new();
        add_mapped_files(maps, &mut mapped);
        assert_eq!(
            mapped,
            HashSet::from([
                FileId {
                    device: libc::makedev(0, 1),
                    inode: 4242
                },
                FileId {
                    device: libc::makedev(0xfd, 0x1a),
                    inode: 77
                },
            ])
        );
    }

    /// The descriptor a backing is pushed with crosses a datagram socketpair
    /// as one `SCM_RIGHTS` message, and the receiver's header and control
    /// bytes name it: here sent and received within this process, through
    /// the same header layout the guest receives with.
    /// A directory as the path descriptor a replay root is held by.
    fn path_descriptor(directory: &std::path::Path) -> OwnedFd {
        std::fs::File::options()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(directory)
            .unwrap()
            .into()
    }

    /// The mapping a listing shows at an address is found by its start, with
    /// the device and inode the listing gives it.
    #[test]
    fn the_mapping_at_an_address_is_read_from_the_listing() {
        let maps = "7f0000000000-7f0000002000 rw-s 00000000 00:1e 4242 /r/#4242 (deleted)\n\
                    7f0000002000-7f0000003000 rw-p 00000000 00:00 0\n";
        assert_eq!(
            mapping_at(maps, 0x7f0000000000),
            Some(FileId {
                device: libc::makedev(0, 0x1e),
                inode: 4242
            })
        );
        assert_eq!(mapping_at(maps, 0x7f0000001000), None);
        assert_eq!(mapping_at(maps, 0x7f0000002000), None);
    }

    /// Only a complete listing of the tracked tasks' mappings drops a
    /// backing: a task whose listing is gone (ENOENT, ESRCH) has exited and
    /// is forgotten, but any other read error (here EMFILE, as a tracer
    /// holding many descriptors can get) makes the listing incomplete, and
    /// then every backing is kept.
    #[test]
    fn an_incomplete_mapping_listing_keeps_every_backing() {
        let maps = "7f0000000000-7f0000002000 rw-s 00000000 00:01 4242 /x (deleted)\n";
        let read = |failing: Option<i32>| {
            move |tid: i32| match tid {
                1 => Err(io::Error::from_raw_os_error(libc::ENOENT)),
                2 => Err(io::Error::from_raw_os_error(libc::ESRCH)),
                tid if Some(tid) == failing => Err(io::Error::from_raw_os_error(libc::EMFILE)),
                _ => Ok(maps.to_owned()),
            }
        };
        let mapped = FileId {
            device: libc::makedev(0, 1),
            inode: 4242,
        };
        assert_eq!(
            mapped_files([1, 2, 3, 4], read(None)),
            (Some(HashSet::from([mapped])), vec![1, 2])
        );
        assert_eq!(mapped_files([1, 2, 3, 4], read(Some(3))).0, None);

        let directory = tempfile::tempdir().unwrap();
        let backing = || SharedBacking::new(path_descriptor(directory.path())).unwrap();
        let key = |inode| SharedFile {
            root: mapped,
            recorded: FileId { device: 1, inode },
        };
        let mut backings = SharedBackings::from([(key(1), backing()), (key(2), backing())]);
        retain_mapped(&mut backings, None);
        assert_eq!(backings.len(), 2, "an incomplete listing dropped a backing");
        // A backing whose listed identity is not known yet is kept by any
        // listing: here one that shows its `stat` identity, which a btrfs
        // listing would not.
        let stat = backings[&key(1)].file_id;
        retain_mapped(&mut backings, Some(&HashSet::from([stat])));
        assert_eq!(
            backings.len(),
            2,
            "a backing with no listed identity was dropped"
        );
        let listed = |inode| FileId {
            device: libc::makedev(0, 0x1e),
            inode,
        };
        backings.get_mut(&key(1)).unwrap().view = Some(listed(1));
        backings.get_mut(&key(2)).unwrap().view = Some(listed(2));
        retain_mapped(&mut backings, Some(&HashSet::from([listed(1), stat])));
        assert_eq!(backings.keys().collect::<Vec<_>>(), vec![&key(1)]);
        retain_mapped(&mut backings, Some(&HashSet::new()));
        assert!(backings.is_empty());
    }

    /// A backing is a file of a fresh name beneath the replay root, which
    /// opens as the backing itself, and is unlinked when the backing is
    /// dropped.
    #[test]
    fn a_backing_opens_by_its_name_and_is_unlinked_when_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let backing = SharedBacking::new(path_descriptor(directory.path())).unwrap();
        let other = SharedBacking::new(path_descriptor(directory.path())).unwrap();
        let name = backing
            .name
            .clone()
            .expect("a file is made in the directory");
        assert_ne!(
            Some(&name),
            other.name.as_ref(),
            "two backings were given one name"
        );
        let opened = File::open(directory.path().join(&name)).unwrap();
        assert_eq!(FileId::of(&opened.metadata().unwrap()), backing.file_id);
        drop((backing, other));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    /// A read-only mapping's hand-off by receive is a read-only descriptor of
    /// the backing, through which no mapping can be made writable; a
    /// read-write one's is the backing's own.
    #[test]
    fn a_read_only_hand_off_cannot_be_made_writable() {
        // A root no file can be made in (procfs) forces the memfd fallback.
        let fallback = SharedBacking::new(path_descriptor(std::path::Path::new("/proc"))).unwrap();
        assert_eq!(fallback.name, None, "a file was made in /proc");
        let backing = File::from(fallback.file.try_clone().unwrap());
        backing.set_len(PAGE_SIZE as u64).unwrap();
        let access = |fd: &OwnedFd| {
            // SAFETY: fcntl F_GETFL only reads the descriptor's flags.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
            flags & libc::O_ACCMODE
        };
        let read_write = handed_descriptor(&backing, libc::O_RDWR).unwrap();
        assert_eq!(access(&read_write), libc::O_RDWR);
        let read_only = handed_descriptor(&backing, libc::O_RDONLY).unwrap();
        assert_eq!(access(&read_only), libc::O_RDONLY);
        assert_eq!(
            FileId::of(
                &File::from(read_only.try_clone().unwrap())
                    .metadata()
                    .unwrap()
            ),
            FileId::of(&backing.metadata().unwrap())
        );
        // SAFETY: a fresh shared read-only mapping of the descriptor, unmapped below.
        let view = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGE_SIZE,
                libc::PROT_READ,
                libc::MAP_SHARED,
                read_only.as_raw_fd(),
                0,
            )
        };
        assert_ne!(view, libc::MAP_FAILED);
        // SAFETY: `view` is the mapping made above.
        let made_writable =
            unsafe { libc::mprotect(view, PAGE_SIZE, libc::PROT_READ | libc::PROT_WRITE) };
        let error = io::Error::last_os_error();
        // SAFETY: as above.
        unsafe { libc::munmap(view, PAGE_SIZE) };
        assert_eq!(
            (made_writable, error.raw_os_error()),
            (-1, Some(libc::EACCES))
        );
    }

    /// A chrooted guest's root gets a link of the backing's name for one open:
    /// the link opens the backing itself and is gone once dropped.
    #[test]
    fn a_backing_is_linked_into_a_chrooted_root_for_one_open() {
        let real = tempfile::tempdir().unwrap();
        let chrooted = real.path().join("chrooted");
        std::fs::create_dir(&chrooted).unwrap();
        let backing = SharedBacking::new(path_descriptor(real.path())).unwrap();
        let name = backing
            .name
            .clone()
            .expect("a file is made beneath the root");
        let alias =
            BackingAlias::link(backing.root.as_fd(), &name, path_descriptor(&chrooted)).unwrap();
        let opened = File::open(chrooted.join(&alias.name)).unwrap();
        assert_eq!(FileId::of(&opened.metadata().unwrap()), backing.file_id);
        drop(alias);
        assert_eq!(std::fs::read_dir(&chrooted).unwrap().count(), 0);
    }

    /// The calls replay adds are admitted only when the tracer runs under no
    /// seccomp filter: any inherited one, whatever its count, refuses.
    #[test]
    fn only_a_tracer_under_no_seccomp_filter_admits_injected_calls() {
        assert_eq!(
            injected_calls_admitted("Name:\tx\nSeccomp:\t0\nSeccomp_filters:\t0\n"),
            Ok(())
        );
        // A kernel without seccomp reports neither field.
        assert_eq!(injected_calls_admitted("Name:\tx\n"), Ok(()));
        let error = injected_calls_admitted("Seccomp:\t2\nSeccomp_filters:\t1\n").unwrap_err();
        assert!(error.contains("Seccomp: 2, Seccomp_filters: 1"), "{error}");
        assert!(injected_calls_admitted("Seccomp:\t2\n").is_err());
        assert!(injected_calls_admitted("Seccomp:\t1\n").is_err());
    }

    #[test]
    fn a_pushed_backing_arrives_as_the_same_file() {
        let directory = tempfile::tempdir().unwrap();
        let backing = SharedBacking::new(path_descriptor(directory.path())).unwrap();
        let mut pair = [0; 2];
        // SAFETY: pair has room for the two descriptors.
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        // SAFETY: socketpair returned two new descriptors nothing else owns.
        let (sender, receiver) =
            unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
        send_descriptor(sender.as_fd(), backing.file.as_fd()).unwrap();

        let mut byte = [0u8; 8];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut control = [0u8; 64];
        let mut header = receive_header(
            (&raw mut iov) as usize,
            control.as_mut_ptr() as usize,
            control.len(),
        );
        // SAFETY: header names live buffers of the lengths it states.
        let received =
            unsafe { libc::recvmsg(receiver.as_raw_fd(), &mut header, libc::MSG_CMSG_CLOEXEC) };
        assert_eq!(received, 1);
        // SAFETY: header is a plain `msghdr`; its bytes are read as such.
        let header_bytes = unsafe {
            std::slice::from_raw_parts((&raw const header).cast::<u8>(), size_of::<libc::msghdr>())
        };
        let fd = received_descriptor(header_bytes, &control).unwrap();
        // SAFETY: the receive installed this descriptor, which nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        assert_eq!(
            FileId::of(&File::from(fd).metadata().unwrap()),
            backing.file_id
        );
    }

    /// A receive that dropped the descriptor (MSG_CTRUNC), or delivered a
    /// control message of another kind, names none.
    #[test]
    fn a_receive_without_one_descriptor_names_none() {
        // SAFETY: CMSG_LEN only computes a size.
        let one = unsafe { libc::CMSG_LEN(size_of::<RawFd>() as u32) } as usize;
        let message = |level: i32, kind: i32| {
            let mut control = vec![0u8; 64];
            // SAFETY: all-zero is a valid `cmsghdr`.
            let mut header: libc::cmsghdr = unsafe { std::mem::zeroed() };
            header.cmsg_len = one;
            header.cmsg_level = level;
            header.cmsg_type = kind;
            // SAFETY: the buffer holds a `cmsghdr`, written without alignment.
            unsafe { std::ptr::write_unaligned(control.as_mut_ptr().cast(), header) };
            // SAFETY: as above, for the descriptor after the header.
            let data = unsafe { libc::CMSG_LEN(0) } as usize;
            control[data..data + 4].copy_from_slice(&7i32.to_ne_bytes());
            control
        };
        let header = |flags: i32, length: usize| {
            let mut header = receive_header(0, 0, length);
            header.msg_flags = flags;
            // SAFETY: header is a plain `msghdr`; its bytes are copied out.
            unsafe {
                std::slice::from_raw_parts(
                    (&raw const header).cast::<u8>(),
                    size_of::<libc::msghdr>(),
                )
            }
            .to_vec()
        };
        let rights = message(libc::SOL_SOCKET, libc::SCM_RIGHTS);
        assert_eq!(received_descriptor(&header(0, one), &rights), Ok(7));
        assert!(received_descriptor(&header(libc::MSG_CTRUNC, one), &rights).is_err());
        assert!(received_descriptor(&header(0, 0), &rights).is_err());
        let credentials = message(libc::SOL_SOCKET, libc::SCM_CREDENTIALS);
        assert!(received_descriptor(&header(0, one), &credentials).is_err());
    }

    #[test]
    fn differing_pages_merges_adjacent_changed_pages() {
        let recorded = vec![1u8; 4 * PAGE_SIZE];
        let mut current = recorded.clone();
        current[PAGE_SIZE + 7] = 0;
        current[2 * PAGE_SIZE] = 0;
        assert_eq!(
            differing_pages(&current, &recorded),
            vec![(PAGE_SIZE, 2 * PAGE_SIZE)]
        );
        assert!(differing_pages(&recorded, &recorded).is_empty());
    }

    #[test]
    fn differing_pages_treats_unread_bytes_as_changed() {
        let recorded = vec![1u8; 3 * PAGE_SIZE + 10];
        let current = recorded[..PAGE_SIZE].to_vec();
        assert_eq!(
            differing_pages(&current, &recorded),
            vec![(PAGE_SIZE, 2 * PAGE_SIZE + 10)]
        );
    }
}
