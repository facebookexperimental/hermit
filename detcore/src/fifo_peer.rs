/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Does a guest hold the other end of a FIFO?
//!
//! A FIFO opened by path is a `Regular` descriptor, so a read or write that
//! must wait blocks in the kernel while the caller holds the scheduler turn.
//! When the other end belongs to a host process that is right: the host makes
//! progress on its own, and every guest waits at a fixed point. When it
//! belongs to another guest, that guest never runs, and the run deadlocks
//! (<https://github.com/rrnewton/hermit/issues/4200>).
//!
//! So the first blocking read or write on such a FIFO asks whether some guest
//! holds a descriptor for the same FIFO with the access the other end needs
//! (a writer for a read, a reader for a write). Only then does Detcore make
//! the FIFO an internally polled pipe; otherwise the call blocks as before.
//!
//! The answer is deterministic. The scan runs in the caller's turn, when every
//! other guest thread is stopped, and a guest's descriptor table changes only
//! in its own turns. A host process is never counted: the scan covers only the
//! processes of Hermit's private PID namespace, whose `/proc` lists nothing
//! else, and is skipped (answering no) when the tracer is not that
//! namespace's init.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use detcore_model::futex::MmId;

/// How many times a guest has opened each FIFO `(dev, ino)` for writing
/// (`O_WRONLY` or `O_RDWR`) and for reading (`O_RDONLY` or `O_RDWR`),
/// nonblocking opens included. A guest's blocking open completes when the
/// other end opens, even one that has closed again by the opener's next
/// polling turn, which the scan of open descriptors cannot see. Counted in
/// the opener's turn, so the count a waiting open reads is deterministic.
/// One count for the tracer, which is right where the scan is: only a tracer
/// that is the init of Hermit's PID namespace scans at all.
static GUEST_OPENS: Mutex<BTreeMap<(u64, u64), Ends>> = Mutex::new(BTreeMap::new());

/// The FIFOs `(dev, ino)` a guest made in this run (a successful `mknod` or
/// `mknodat` with `S_IFIFO`), until a guest removes the last name that
/// reaches one (`unlink`, `unlinkat`, or a `rename` over it, not onto another
/// link of the same FIFO). Only a blocking open of one
/// of these waits in polling turns for a guest peer
/// (<https://github.com/rrnewton/hermit/issues/2203>): its other end is a
/// guest's unless a host process opens a FIFO a guest just made. Every other
/// FIFO (a host's, one made before the run, a pipe reached by path) is opened
/// as before, in the kernel. Changed in the guest's turn.
///
/// A number the kernel reuses for a new FIFO after a guest removed the old
/// one's last name is not inherited: removing that name forgets it. A host that
/// removes a guest-made FIFO and makes another that reuses its inode number
/// within the run would inherit it; Detcore sees no host removal.
static GUEST_MADE: Mutex<BTreeSet<(u64, u64)>> = Mutex::new(BTreeSet::new());

/// Record that a guest made the FIFO `fifo`.
pub(crate) fn note_guest_made(fifo: (u64, u64)) {
    GUEST_MADE.lock().unwrap().insert(fifo);
    forget_fifo_channel(fifo);
}

/// `--pipe-wake-latency`'s names for FIFOs ([`fifo_channel`]): each FIFO
/// `(dev, ino)` that has had a channel end in this run, with its ordinal,
/// and the next ordinal.
type FifoChannelNames = (BTreeMap<(u64, u64), u64>, u64);
static FIFO_CHANNELS: Mutex<FifoChannelNames> = Mutex::new((BTreeMap::new(), 0));

/// The channel name of the FIFO `fifo` for `--pipe-wake-latency`
/// (<https://github.com/rrnewton/hermit/issues/4380>): an ordinal assigned at
/// its first use, which is in a guest's turn, so the name, which the
/// scheduler's polls log, is the same in every run; a host `(dev, ino)` is
/// not. A guest that makes a FIFO or removes the last name of one it made
/// drops the entry ([`note_guest_made`], [`forget_guest_made`]), so a number
/// the kernel reuses for a new FIFO gets a new ordinal. As for `GUEST_MADE`,
/// a host's removal is not seen.
pub(crate) fn fifo_channel(fifo: (u64, u64)) -> u64 {
    let mut channels = FIFO_CHANNELS.lock().unwrap();
    let (names, next) = &mut *channels;
    *names.entry(fifo).or_insert_with(|| {
        *next += 1;
        *next - 1
    })
}

fn forget_fifo_channel(fifo: (u64, u64)) {
    FIFO_CHANNELS.lock().unwrap().0.remove(&fifo);
}

/// Forget the guest-made FIFO `fifo`, whose name a guest removed.
pub(crate) fn forget_guest_made(fifo: (u64, u64)) {
    forget_fifo_channel(fifo);
    if GUEST_MADE.lock().unwrap().remove(&fifo) {
        // No host inode number: the line is part of the verified log.
        tracing::info!(
            "[detcore] a guest removed the name of the FIFO it made; its number is forgotten"
        );
    }
}

/// Whether a guest made the FIFO `fifo` in this run (see `GUEST_MADE`).
pub(crate) fn is_guest_made(fifo: (u64, u64)) -> bool {
    GUEST_MADE.lock().unwrap().contains(&fifo)
}

/// Whether a guest has made any FIFO in this run, so that a removal is worth
/// a look.
pub(crate) fn any_guest_made() -> bool {
    !GUEST_MADE.lock().unwrap().is_empty()
}

/// A set of guest-made FIFOs, as a removal consults it
/// (`Detcore::guest_made_fifos_removed_from`).
pub(crate) trait GuestMadeFifos {
    /// Whether the set has any FIFO ([`any_guest_made`]).
    fn any(&self) -> bool;
    /// Whether the set has `fifo` ([`is_guest_made`]).
    fn contains(&self, fifo: (u64, u64)) -> bool;
}

/// The run's guest-made FIFOs ([`GUEST_MADE`]).
pub(crate) struct GuestMadeRegistry;

impl GuestMadeFifos for GuestMadeRegistry {
    fn any(&self) -> bool {
        any_guest_made()
    }

    fn contains(&self, fifo: (u64, u64)) -> bool {
        is_guest_made(fifo)
    }
}

/// The guest blocking opens of each FIFO waiting, in polling turns, for the
/// other end ([`PendingOpen`]). On Linux a blocking open counts as a reader
/// or writer while it waits, so a waiting reader completes a writer's open
/// and the reverse; these are those waiting opens, which hold no descriptor
/// the scan could see. Changed in the opener's turn.
static PENDING_OPENS: Mutex<BTreeMap<(u64, u64), Ends>> = Mutex::new(BTreeMap::new());

/// A writer the tracer holds open for the waiting writers of a FIFO once a
/// reader's open has completed ([`stand_in_for_waiting_writers`]): on Linux a
/// waiting writer counts as a writer, so that reader's reads wait for it
/// rather than seeing end of file. Dropped when no writer waits any more.
static STAND_IN_WRITERS: Mutex<BTreeMap<(u64, u64), fs::File>> = Mutex::new(BTreeMap::new());

/// Called as a guest reader's open of the FIFO `fifo` (found by the tracer at
/// `path`) completes, while a reader is open: if a guest writer's blocking
/// open is waiting, the tracer holds the FIFO open for writing until none is,
/// so the reader does not read end of file before that writer has opened.
pub(crate) fn stand_in_for_waiting_writers(fifo: (u64, u64), path: &std::ffi::OsStr) {
    use std::os::unix::fs::OpenOptionsExt;
    if pending_opens(fifo).writers == 0 {
        return;
    }
    let mut stand_ins = STAND_IN_WRITERS.lock().unwrap();
    if stand_ins.contains_key(&fifo) {
        return;
    }
    // `path` may name another file by now (a rename while the open waited);
    // only the FIFO itself is held.
    if let Ok(file) = fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        && file
            .metadata()
            .is_ok_and(|meta| (meta.dev(), meta.ino()) == fifo)
    {
        stand_ins.insert(fifo, file);
    }
}

/// A count for each end of a FIFO.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ends {
    pub(crate) readers: u64,
    pub(crate) writers: u64,
}

impl Ends {
    fn add(&mut self, flags: i32, by: i64) {
        let access = flags & libc::O_ACCMODE;
        if access != libc::O_WRONLY {
            self.readers = self.readers.saturating_add_signed(by);
        }
        if access != libc::O_RDONLY {
            self.writers = self.writers.saturating_add_signed(by);
        }
    }

    /// The count of the end `peer` names.
    pub(crate) fn of(self, peer: FifoPeer) -> u64 {
        match peer {
            FifoPeer::Reader => self.readers,
            FifoPeer::Writer => self.writers,
        }
    }
}

/// Count a guest's successful open of the FIFO `fifo` with the file status
/// flags `flags`.
pub(crate) fn note_guest_open(fifo: (u64, u64), flags: i32) {
    GUEST_OPENS
        .lock()
        .unwrap()
        .entry(fifo)
        .or_default()
        .add(flags, 1);
}

/// How many times a guest has opened each end of the FIFO `fifo`.
pub(crate) fn guest_opens(fifo: (u64, u64)) -> Ends {
    GUEST_OPENS
        .lock()
        .unwrap()
        .get(&fifo)
        .copied()
        .unwrap_or_default()
}

/// How many guest blocking opens of each end of the FIFO `fifo` are waiting.
pub(crate) fn pending_opens(fifo: (u64, u64)) -> Ends {
    PENDING_OPENS
        .lock()
        .unwrap()
        .get(&fifo)
        .copied()
        .unwrap_or_default()
}

/// A guest blocking open of a FIFO while it waits in polling turns. It counts
/// in [`pending_opens`] until dropped, however the open ends. The tracer holds
/// the FIFO it waits on (`handle`, an `O_PATH` descriptor), so the open
/// completes on that FIFO, as Linux's does, even if a guest renames or
/// unlinks the name meanwhile. A waiting reader also holds the FIFO open for
/// reading in the tracer (`reader`), as Linux counts it a reader while it
/// waits: a writer's open, the guest's or a host's, then completes at once,
/// and what a writer writes before the guest's open completes stays in the
/// FIFO for it. The tracer's descriptors are in no guest's table, so no guest
/// can use them.
pub(crate) struct PendingOpen {
    fifo: (u64, u64),
    flags: i32,
    handle: Option<fs::File>,
    /// Held only to keep the FIFO open for reading while the open waits.
    _reader: Option<fs::File>,
}

impl PendingOpen {
    /// Start waiting to open the FIFO `fifo`, found at the tracer path `path`,
    /// with the file status flags `flags`. Called in the turn of the guest's
    /// own nonblocking try, once it has succeeded (a reader) or found no
    /// reader (a writer), so a failing open opens nothing here.
    pub(crate) fn begin(fifo: (u64, u64), path: &std::ffi::OsStr, flags: i32) -> Self {
        use std::os::unix::fs::OpenOptionsExt;
        let handle = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)
            .ok()
            .filter(|file| {
                file.metadata()
                    .is_ok_and(|meta| (meta.dev(), meta.ino()) == fifo)
            });
        let mut pending = PendingOpen {
            fifo,
            flags,
            handle,
            _reader: None,
        };
        if flags & libc::O_ACCMODE == libc::O_RDONLY {
            pending._reader = pending.open_in_tracer(libc::O_RDONLY).ok();
        }
        PENDING_OPENS
            .lock()
            .unwrap()
            .entry(fifo)
            .or_default()
            .add(flags, 1);
        pending
    }

    /// The path at which a guest opens the FIFO this open waits on: the
    /// tracer's descriptor for it, through the tracer's `/proc` entry (the
    /// tracer and the guests share a PID namespace). `None` if the tracer
    /// could not hold it.
    pub(crate) fn guest_path(&self) -> Option<String> {
        use std::os::fd::AsRawFd;
        let handle = self.handle.as_ref()?;
        Some(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            handle.as_raw_fd()
        ))
    }

    /// [`stand_in_for_waiting_writers`] for the FIFO this open waited on, as
    /// a reader's open of it completes.
    pub(crate) fn stand_in_for_waiting_writers(&self) {
        if pending_opens(self.fifo).writers == 0 {
            return;
        }
        let mut stand_ins = STAND_IN_WRITERS.lock().unwrap();
        if !stand_ins.contains_key(&self.fifo)
            && let Ok(file) = self.open_in_tracer(libc::O_WRONLY)
        {
            stand_ins.insert(self.fifo, file);
        }
    }

    /// Open the FIFO this open waits on in the tracer, nonblocking, with the
    /// access `access` (`O_RDONLY` or `O_WRONLY`).
    pub(crate) fn open_in_tracer(&self, access: i32) -> std::io::Result<fs::File> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EBADF))?;
        fs::OpenOptions::new()
            .read(access == libc::O_RDONLY)
            .write(access == libc::O_WRONLY)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(format!("/proc/self/fd/{}", handle.as_raw_fd()))
    }
}

impl Drop for PendingOpen {
    fn drop(&mut self) {
        let writers_left = PENDING_OPENS
            .lock()
            .unwrap()
            .get_mut(&self.fifo)
            .map(|ends| {
                ends.add(self.flags, -1);
                ends.writers
            })
            .unwrap_or(0);
        if writers_left == 0 {
            STAND_IN_WRITERS.lock().unwrap().remove(&self.fifo);
        }
    }
}

/// The end of a FIFO a waiting call needs another holder of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FifoPeer {
    /// A descriptor open for writing (`O_WRONLY` or `O_RDWR`), which a read
    /// waits for.
    Writer,
    /// A descriptor open for reading (`O_RDONLY` or `O_RDWR`), which a write
    /// waits for.
    Reader,
}

impl FifoPeer {
    /// Whether a descriptor with these status flags is such an end. An
    /// `O_PATH` handle is neither: it reads and writes nothing, and its access
    /// bits are 0, which would otherwise read as `O_RDONLY`.
    fn accepts_flags(self, flags: i32) -> bool {
        flags & libc::O_PATH == 0 && self.accepts(flags & libc::O_ACCMODE)
    }

    fn accepts(self, access_mode: i32) -> bool {
        match self {
            FifoPeer::Writer => access_mode == libc::O_WRONLY || access_mode == libc::O_RDWR,
            FifoPeer::Reader => access_mode == libc::O_RDONLY || access_mode == libc::O_RDWR,
        }
    }
}

/// The file status flags of `<table>/fdinfo/<fd>`, as the kernel holds them
/// now, where `table` is a process's or a thread's `/proc` directory.
fn fdinfo_flags(table: &Path, fd: &str) -> Option<i32> {
    let info = fs::read_to_string(table.join("fdinfo").join(fd)).ok()?;
    let flags = info.lines().find_map(|line| line.strip_prefix("flags:"))?;
    i32::from_str_radix(flags.trim(), 8).ok()
}

/// Whether Detcore can see a guest's descriptors through `/proc/<tid>` on
/// `backend`, as every check here needs. Only where the tool is a separate
/// tracer, outside the guest's descriptor table (not DBT, whose tool runs in
/// the guest), and where a guest thread's ID is a host task's
/// ([`crate::procfs_inode::guest_tids_name_host_tasks`]; not KVM, where
/// `/proc/<tid>` is whatever host task has that ID). `physical_tid` is the
/// guest thread's host ID where the backend supplies one beside its own.
/// Elsewhere a FIFO keeps blocking, as before.
pub(crate) fn guest_descriptors_are_visible(
    backend: &reverie::BackendCapabilities,
    physical_tid: Option<i32>,
) -> bool {
    !backend.tool_shares_guest_descriptor_table
        && crate::procfs_inode::guest_tids_name_host_tasks(backend, physical_tid)
}

/// The identity `(dev, ino)` of the FIFO a guest thread's
/// `openat(dirfd, path)` would open, and the path at which the tracer finds it, read through the thread's own root, cwd
/// or `dirfd`; `None` when it is not a FIFO, cannot be read, or the guest's
/// descriptors are not visible ([`guest_descriptors_are_visible`]). (An
/// absolute symlink is resolved from the tracer's root, which only a chrooted
/// guest sees differently; a FIFO missed that way is opened as before.)
///
/// A path's leading `/proc/self` or `/proc/thread-self`, which the tracer's
/// lookup would resolve for the tracer, is spelled with the guest's own
/// numbers (`own`, its pid and tid as its `/proc` names them,
/// `Detcore::procfs_caller_ids`), which the lookup through the guest's root
/// finds in the guest's `/proc`: so the FIFO behind one of the guest's
/// descriptor links (`/proc/self/fd/N`, the spelling a name below a
/// descriptor directory takes, `syscalls::files::descriptor_directory`) is
/// found, and its open waits in polling turns rather than in the kernel.
pub(crate) fn fifo_at(
    backend: &reverie::BackendCapabilities,
    physical_tid: Option<i32>,
    tid: i32,
    own: Option<(i32, i32)>,
    dirfd: i32,
    path: &Path,
) -> Option<((u64, u64), std::ffi::OsString)> {
    if !guest_descriptors_are_visible(backend, physical_tid) {
        return None;
    }
    let respelled;
    let path = match (own, path.strip_prefix("/proc")) {
        (Some((pid, tid)), Ok(below)) => {
            let mut components = below.components();
            match components.next() {
                Some(std::path::Component::Normal(name)) if name == "self" => {
                    respelled = Path::new("/proc")
                        .join(pid.to_string())
                        .join(components.as_path());
                    respelled.as_path()
                }
                Some(std::path::Component::Normal(name)) if name == "thread-self" => {
                    respelled = Path::new("/proc")
                        .join(pid.to_string())
                        .join("task")
                        .join(tid.to_string())
                        .join(components.as_path());
                    respelled.as_path()
                }
                _ => path,
            }
        }
        _ => path,
    };
    let base = if path.is_absolute() {
        format!("/proc/{tid}/root")
    } else if dirfd == libc::AT_FDCWD {
        format!("/proc/{tid}/cwd/")
    } else {
        format!("/proc/{tid}/fd/{dirfd}/")
    };
    let mut full = std::ffi::OsString::from(base);
    full.push(path.as_os_str());
    let meta = fs::metadata(&full).ok()?;
    meta.file_type()
        .is_fifo()
        .then(|| ((meta.dev(), meta.ino()), full))
}

/// The identity `(dev, ino)` of the FIFO behind guest thread `tid`'s
/// descriptor `fd` (a named FIFO, or a pipe reached by path), or `None` when
/// it is not a FIFO or the guest's descriptors are not visible
/// ([`guest_descriptors_are_visible`]). Read through the thread, whose
/// descriptor table may not be its leader's (a thread cloned without
/// CLONE_FILES). Every later check here runs only on a FIFO this found.
pub(crate) fn fifo_of(
    backend: &reverie::BackendCapabilities,
    physical_tid: Option<i32>,
    tid: i32,
    fd: i32,
) -> Option<(u64, u64)> {
    if !guest_descriptors_are_visible(backend, physical_tid) {
        return None;
    }
    fifo_behind(tid, fd)
}

/// [`fifo_of`] without the visibility check, for the checks that run only on
/// a FIFO it found.
fn fifo_behind(tid: i32, fd: i32) -> Option<(u64, u64)> {
    let meta = fs::metadata(format!("/proc/{tid}/fd/{fd}")).ok()?;
    meta.file_type().is_fifo().then(|| (meta.dev(), meta.ino()))
}

/// Whether the kernel's open file description behind thread `tid`'s `fd` is
/// nonblocking now. `None` when procfs cannot say.
pub(crate) fn kernel_nonblocking(tid: i32, fd: i32) -> Option<bool> {
    fdinfo_flags(&Path::new("/proc").join(tid.to_string()), &fd.to_string())
        .map(|flags| flags & libc::O_NONBLOCK != 0)
}

/// Whether a guest holds a descriptor for the FIFO behind thread `tid`'s
/// `fd` with the access `peer` names, other than `fd`'s own open file
/// description, wherever that is held (a `dup`, or a child that inherited it).
/// A FIFO the guest opened read-write is its own writer to the kernel, but
/// whether data ever comes is up to the FIFO's other holders, which may be
/// host processes only (`exec 3<>fifo`). `fd` is read in `tid`'s own table.
pub(crate) fn guest_holds_peer(tid: i32, fd: i32, peer: FifoPeer) -> bool {
    // A guest's blocking open of the other end that is still waiting counts:
    // on Linux it already holds that end (a waiting writer is a writer).
    fifo_behind(tid, fd)
        .is_some_and(|fifo| pending_opens(fifo).of(peer) > 0 || scan(fifo, peer, Some((tid, fd))))
}

/// Whether a guest holds a descriptor for the FIFO `(dev, ino)` with the
/// access `peer` names. For an open, which has no descriptor yet.
pub(crate) fn guest_holds_peer_of(fifo: (u64, u64), peer: FifoPeer) -> bool {
    scan(fifo, peer, None)
}

/// The scan behind `guest_holds_peer` and `guest_holds_peer_of`, leaving out
/// the open file description of `own`, a `(tid, fd)` pair.
fn scan(fifo: (u64, u64), peer: FifoPeer, own: Option<(i32, i32)>) -> bool {
    // Outside a private PID namespace `/proc` lists host processes too.
    let tracer = std::process::id();
    if tracer != 1 {
        return false;
    }
    let proc_root = Path::new("/proc");
    let tracer = tracer.to_string();
    // Directories are listed without the C library's heap
    // (util::find_in_directory) throughout.
    crate::util::find_in_directory(proc_root, |name| {
        let name = name.to_str()?;
        (name.bytes().all(|b| b.is_ascii_digit())
            && name != tracer
            && descriptor_tables(&proc_root.join(name))
                .iter()
                .any(|(table, task)| holds_peer(table, *task, fifo, peer, own)))
        .then_some(())
    })
    .is_ok_and(|found| found.is_some())
}

/// Whether the descriptor table at `table` (a task's `/proc` directory, of
/// task `task`) holds a descriptor for the FIFO `(dev, ino)` with the access
/// `peer` names, other than the open file description of `own`, a
/// `(tid, fd)` pair.
fn holds_peer(
    table: &Path,
    task: i32,
    (dev, ino): (u64, u64),
    peer: FifoPeer,
    own: Option<(i32, i32)>,
) -> bool {
    let descriptors = table.join("fd");
    crate::util::find_in_directory(&descriptors, |name| {
        let fd = name.to_str()?.parse::<i32>().ok()?;
        (fs::metadata(descriptors.join(name))
            .is_ok_and(|meta| meta.file_type().is_fifo() && meta.dev() == dev && meta.ino() == ino)
            && fdinfo_flags(table, &fd.to_string()).is_some_and(|flags| peer.accepts_flags(flags))
            && !own.is_some_and(|(tid, own_fd)| same_description(tid, own_fd, task, fd)))
        .then_some(())
    })
    .is_ok_and(|found| found.is_some())
}

/// Each distinct descriptor table of a process, as a task's `/proc` directory
/// and that task's id. Its threads usually share one table, but a thread
/// cloned without CLONE_FILES has its own, and the leader's `fd` directory is
/// empty once the leader has exited (`pthread_exit`). Threads with the same
/// table are listed once (`kcmp(KCMP_FILES)`); when the kernel cannot say,
/// each is listed, which costs only a second look.
///
/// The leader-exit case is untested: on ptrace any leader `pthread_exit`
/// beside a live thread hangs before a FIFO is read
/// (<https://github.com/rrnewton/hermit/issues/4100>).
fn descriptor_tables(process: &Path) -> Vec<(std::path::PathBuf, i32)> {
    let task_root = process.join("task");
    let mut tasks: Vec<(std::path::PathBuf, i32)> = Vec::new();
    // A listing error keeps the tasks listed before it, as std's read_dir
    // iteration did.
    let _ = crate::util::find_in_directory(&task_root, |name| {
        if let Some(id) = name.to_str().and_then(|id| id.parse::<i32>().ok()) {
            tasks.push((task_root.join(name), id));
        }
        None::<()>
    });
    tasks.sort_by_key(|(_, id)| *id);
    let mut tables: Vec<(std::path::PathBuf, i32)> = Vec::new();
    for (dir, id) in tasks {
        // Close to std's `read_dir(..).next().is_some()`: a listing error
        // still counts where the directory exists, which costs only a
        // second look (holds_peer then finds nothing there).
        let fd_directory = dir.join("fd");
        let listed = match crate::util::find_in_directory(&fd_directory, |_| Some(())) {
            Ok(found) => found.is_some(),
            Err(_) => crate::util::raw_stat(&fd_directory).is_ok(),
        };
        if listed && !tables.iter().any(|(_, seen)| same_table(*seen, id)) {
            tables.push((dir, id));
        }
    }
    tables
}

/// Whether tasks `a` and `b` share one descriptor table (`kcmp(KCMP_FILES)`).
fn same_table(a: i32, b: i32) -> bool {
    const KCMP_FILES: libc::c_int = 2;
    // SAFETY: kcmp reads no memory; it compares two tasks' tables.
    let order = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            a as libc::pid_t,
            b as libc::pid_t,
            KCMP_FILES,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    order == 0
}

/// Whether `fd1` of task `pid1` and `fd2` of task `pid2` are one open file
/// description (`kcmp(KCMP_FILE)`). When the kernel cannot say, they are
/// taken to be the same, so the descriptor is not counted as a peer and the
/// FIFO keeps blocking as before.
fn same_description(pid1: i32, fd1: i32, pid2: i32, fd2: i32) -> bool {
    const KCMP_FILE: libc::c_int = 0;
    // SAFETY: kcmp reads no memory; it compares two tasks' descriptors.
    let order = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            pid1 as libc::pid_t,
            pid2 as libc::pid_t,
            KCMP_FILE,
            fd1 as libc::c_ulong,
            fd2 as libc::c_ulong,
        )
    };
    !(1..=3).contains(&order)
}

/// An address at which a FIFO open can map its staging page in thread
/// `tid`'s address space, read from `/proc/<tid>/maps`
/// ([`pick_staging_address`]). Chosen before the page is mapped, so that the
/// page has an owner ([`StagedPage`]) from before its `mmap` runs: a task
/// killed during that `mmap` leaves an address its address space's next task
/// can check (Codex re-check of
/// <https://github.com/rrnewton/hermit/pull/4329> at revision 6, P2).
pub(crate) fn free_staging_address(tid: i32) -> Option<usize> {
    pick_staging_address(&fs::read_to_string(format!("/proc/{tid}/maps")).ok()?)
}

/// Two pages below the start of the highest mapping that lies below the
/// stack, has three free pages below it, and is not the stack itself (which
/// grows down into the gap below it), as the kernel's top-down allocator
/// would place a page; `None` if `maps` has no such gap.
fn pick_staging_address(maps: &str) -> Option<usize> {
    pick_staging_address_with_page(maps, crate::page::page_size() as usize)
}

/// [`pick_staging_address`] on a host whose pages are `page` bytes; a mapping
/// address must be a multiple of it.
fn pick_staging_address_with_page(maps: &str, page: usize) -> Option<usize> {
    let mut ranges: Vec<(usize, usize, bool)> = maps
        .lines()
        .filter_map(|line| {
            let (range, rest) = line.split_once(' ')?;
            let (start, end) = range.split_once('-')?;
            Some((
                usize::from_str_radix(start, 16).ok()?,
                usize::from_str_radix(end, 16).ok()?,
                rest.trim_end().ends_with("[stack]"),
            ))
        })
        .collect();
    ranges.sort_unstable();
    let stack = ranges
        .iter()
        .find(|(_, _, is_stack)| *is_stack)
        .map(|(start, _, _)| *start)?;
    ranges
        .windows(2)
        .rev()
        .find(|pair| {
            let ((_, below_end, _), (start, _, is_stack)) = (pair[0], pair[1]);
            start <= stack && !is_stack && start.saturating_sub(below_end) >= 3 * page
        })
        .map(|pair| pair[1].0 - 2 * page)
}

/// Pages a completing FIFO open mapped in a guest address space to stage a
/// path ([`StagedPage`]), and could not unmap: the task went away first (a
/// task killed while its injected open waits in the kernel drops the open),
/// or the unmap injection did not run. The address space can outlive the task
/// (a `CLONE_VM` process, a `vfork` or `posix_spawn` parent, or a thread
/// group's other threads), so the next task of that address space to reach
/// Detcore unmaps them ([`take_orphaned_pages`]) before it runs guest code
/// again (Codex re-check of <https://github.com/rrnewton/hermit/pull/4329> at
/// revision 5, P2; Claude's at revision 6, MEDIUM-G).
static ORPHANED_PAGES: Mutex<Vec<OrphanedPage>> = Mutex::new(Vec::new());

/// Whether [`ORPHANED_PAGES`] may hold a page, so that the check each event
/// makes costs one load while it is empty.
static ANY_ORPHANED_PAGE: AtomicBool = AtomicBool::new(false);

/// The bytes a staging page holds: the path, NUL-padded.
pub(crate) const STAGED_PAGE_LENGTH: usize = 64;

/// A staging page left mapped in the address space `mm` at `address`,
/// holding `contents`, which tell it from whatever the guest mapped there
/// since (Claude re-check at revision 6, LOW-I).
#[derive(Clone, Copy)]
struct OrphanedPage {
    mm: MmId,
    address: usize,
    contents: [u8; STAGED_PAGE_LENGTH],
}

/// A page mapped in the address space `mm` at `address`, holding
/// `contents`, which the caller unmaps and then reports with
/// [`StagedPage::unmapped`]. Dropped without that report, it is left to the
/// address space's next task ([`ORPHANED_PAGES`]): so too when the caller is
/// cancelled, because its task exits while it waits (for the opener, and for
/// a task that reclaims the page, Codex re-check at revision 7, P2).
pub(crate) struct StagedPage {
    page: OrphanedPage,
    unmapped: bool,
}

impl StagedPage {
    pub(crate) fn new(mm: MmId, address: usize, contents: [u8; STAGED_PAGE_LENGTH]) -> Self {
        StagedPage {
            page: OrphanedPage {
                mm,
                address,
                contents,
            },
            unmapped: false,
        }
    }

    /// The caller's unmap ran, or the page is no longer there to unmap.
    pub(crate) fn unmapped(mut self) {
        self.unmapped = true;
    }

    pub(crate) fn address(&self) -> usize {
        self.page.address
    }

    pub(crate) fn contents(&self) -> &[u8; STAGED_PAGE_LENGTH] {
        &self.page.contents
    }
}

impl Drop for StagedPage {
    fn drop(&mut self) {
        if !self.unmapped {
            leave_orphaned_page(self.page);
        }
    }
}

/// Leave `page` to the next task of its address space.
fn leave_orphaned_page(page: OrphanedPage) {
    ORPHANED_PAGES.lock().unwrap().push(page);
    ANY_ORPHANED_PAGE.store(true, Ordering::SeqCst);
}

/// Whether a page may be left ([`ANY_ORPHANED_PAGE`]): the load
/// [`take_orphaned_pages`] makes first, which answers no page while it is
/// false.
pub(crate) fn any_orphaned_page() -> bool {
    ANY_ORPHANED_PAGE.load(Ordering::SeqCst)
}

/// The pages ([`ORPHANED_PAGES`]) left in the address space `mm`, each
/// owned again by a [`StagedPage`], which the caller unmaps now; any it does
/// not report unmapped, because its unmap did not run or the caller was
/// cancelled, are left again.
pub(crate) fn take_orphaned_pages(mm: MmId) -> Vec<StagedPage> {
    if !ANY_ORPHANED_PAGE.load(Ordering::SeqCst) {
        return Vec::new();
    }
    let mut orphaned = ORPHANED_PAGES.lock().unwrap();
    let (taken, kept): (Vec<_>, Vec<_>) = orphaned.drain(..).partition(|page| page.mm == mm);
    *orphaned = kept;
    if orphaned.is_empty() {
        ANY_ORPHANED_PAGE.store(false, Ordering::SeqCst);
    }
    taken
        .into_iter()
        .map(|page| StagedPage {
            page,
            unmapped: false,
        })
        .collect()
}

/// Whether thread `tid`'s address space has, at `address`, a standalone
/// one-page anonymous `rw-p` mapping, as a staging page is (the address
/// picker leaves a free page on each side, so it never merges): a page of
/// zeros there is then the staging page of an opener killed before it wrote
/// the path, not a sibling's fresh mapping that covers the address (Claude
/// re-check at revision 7, LOW-J).
pub(crate) fn is_lone_staging_mapping(tid: i32, address: usize) -> bool {
    fs::read_to_string(format!("/proc/{tid}/maps"))
        .is_ok_and(|maps| maps_have_lone_staging_mapping(&maps, address))
}

fn maps_have_lone_staging_mapping(maps: &str, address: usize) -> bool {
    lone_staging_mapping_with_page(maps, address, crate::page::page_size() as usize)
}

/// [`maps_have_lone_staging_mapping`] on a host whose pages are `page` bytes.
fn lone_staging_mapping_with_page(maps: &str, address: usize, page: usize) -> bool {
    let wanted = format!(
        "{:08x}-{:08x} rw-p 00000000 00:00 0",
        address,
        address + page
    );
    maps.lines().any(|line| line.trim_end() == wanted)
}

#[cfg(test)]
mod tests {
    use super::lone_staging_mapping_with_page;
    use super::pick_staging_address_with_page;

    /// The page size the fixtures below are written for.
    const PAGE: usize = 4096;

    /// A page of zeros is taken for a staging page only where the maps show
    /// a standalone one-page anonymous `rw-p` mapping at its address.
    #[test]
    fn only_a_lone_anonymous_page_is_taken_for_a_staging_page() {
        let lone = "7ffff7fbe000-7ffff7fbf000 rw-p 00000000 00:00 0 \n";
        assert!(lone_staging_mapping_with_page(lone, 0x7ffff7fbe000, PAGE));
        let larger = "7ffff7fbe000-7ffff7fc0000 rw-p 00000000 00:00 0\n";
        assert!(!lone_staging_mapping_with_page(
            larger,
            0x7ffff7fbe000,
            PAGE
        ));
        let covering = "7ffff7fbd000-7ffff7fbf000 rw-p 00000000 00:00 0\n";
        assert!(!lone_staging_mapping_with_page(
            covering,
            0x7ffff7fbe000,
            PAGE
        ));
        let file = "7ffff7fbe000-7ffff7fbf000 rw-p 00000000 00:20 7 /tmp/f\n";
        assert!(!lone_staging_mapping_with_page(file, 0x7ffff7fbe000, PAGE));
        let read_only = "7ffff7fbe000-7ffff7fbf000 r--p 00000000 00:00 0\n";
        assert!(!lone_staging_mapping_with_page(
            read_only,
            0x7ffff7fbe000,
            PAGE
        ));
    }

    /// The staging page goes two pages below the highest mapping under the
    /// stack that has three free pages below it, never into the gap below
    /// the stack.
    #[test]
    fn a_staging_page_goes_below_the_highest_mapping_with_room() {
        let maps = "\
00400000-00401000 r--p 00000000 00:20 1 /bin/guest
7ffff7fc0000-7ffff7fc4000 r--p 00000000 00:00 0 [vvar]
7ffff7fc4000-7ffff7fc6000 r-xp 00000000 00:00 0 [vdso]
7ffff7fc6000-7ffff7fc8000 r--p 00000000 00:20 2 /lib/ld.so
7ffffffde000-7ffffffff000 rw-p 00000000 00:00 0 [stack]
ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0 [vsyscall]
";
        assert_eq!(
            pick_staging_address_with_page(maps, PAGE),
            Some(0x7ffff7fbe000)
        );
        // Too little room below the vvar page: the next gap down.
        let tight = maps.replace(
            "00400000-00401000 r--p 00000000 00:20 1 /bin/guest",
            "00400000-00401000 r--p 00000000 00:20 1 /bin/guest\n7ffff7fbe000-7ffff7fc0000 rw-p 00000000 00:00 0",
        );
        assert_eq!(
            pick_staging_address_with_page(&tight, PAGE),
            Some(0x7ffff7fbc000)
        );
        assert_eq!(
            pick_staging_address_with_page("00400000-00401000 r--p 0 00:20 1 /bin/guest\n", PAGE),
            None
        );
    }

    /// With 64 KiB pages the staging page and its gaps are 64 KiB pages, so
    /// its address stays a multiple of the page size.
    #[test]
    fn a_staging_page_on_a_host_with_large_pages_is_page_aligned() {
        const LARGE: usize = 0x10000;
        let maps = "\
aaaaaaaa0000-aaaaaaab0000 r--p 00000000 00:20 1 /bin/guest
ffffb7fc0000-ffffb7fd0000 r--p 00000000 00:00 0 [vvar]
ffffb7fd0000-ffffb7fe0000 r-xp 00000000 00:00 0 [vdso]
fffffffd0000-1000000000000 rw-p 00000000 00:00 0 [stack]
";
        let address = pick_staging_address_with_page(maps, LARGE).unwrap();
        assert_eq!(address, 0xffffb7fa0000);
        assert_eq!(address % LARGE, 0);
        let lone = "ffffb7fa0000-ffffb7fb0000 rw-p 00000000 00:00 0\n";
        assert!(lone_staging_mapping_with_page(lone, address, LARGE));
        assert!(!lone_staging_mapping_with_page(lone, address, PAGE));
    }

    use std::fs::File;
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    use super::*;

    /// The descriptor checks run only under a separate tracer whose guest
    /// thread IDs are host tasks: ptrace and in-guest LiteInst (whose tool is
    /// still a separate tracer), not DBT or SaBRe (their tool shares the
    /// guest's table) nor KVM (`/proc/<tid>` there is an unrelated host task;
    /// on one host it measured as a `kvm-nx-lpage-recovery` worker thread of
    /// Hermit's own process), nor a thread whose host ID differs from its own.
    #[test]
    fn guest_descriptors_are_visible_only_under_a_host_task_tracer() {
        use reverie::BackendCapabilities;
        assert!(guest_descriptors_are_visible(
            &BackendCapabilities::PTRACE,
            None
        ));
        assert!(guest_descriptors_are_visible(
            &BackendCapabilities::LITEINST_IN_GUEST,
            None
        ));
        assert!(!guest_descriptors_are_visible(
            &BackendCapabilities::KVM,
            None
        ));
        assert!(!guest_descriptors_are_visible(
            &BackendCapabilities::DBT,
            Some(4_321)
        ));
        assert!(!guest_descriptors_are_visible(
            &BackendCapabilities::SABRE,
            None
        ));
        assert!(!guest_descriptors_are_visible(
            &BackendCapabilities::PTRACE,
            Some(4_321)
        ));
    }

    /// A real FIFO, held by this test process: it is a FIFO only where the
    /// backend's guest descriptors are visible, so no caller can read another
    /// task's table on KVM.
    #[test]
    fn a_fifo_is_seen_only_where_guest_descriptors_are_visible() {
        use reverie::BackendCapabilities;
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let end = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&end);
        let tid = nix::unistd::gettid().as_raw();
        assert!(fifo_of(&BackendCapabilities::PTRACE, None, tid, fd).is_some());
        assert!(fifo_of(&BackendCapabilities::KVM, None, tid, fd).is_none());
        assert!(fifo_of(&BackendCapabilities::DBT, Some(tid), tid, fd).is_none());
    }

    fn fifo_in(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("fifo");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
        path
    }

    fn identity(file: &File) -> (u64, u64) {
        let meta = file.metadata().unwrap();
        (meta.dev(), meta.ino())
    }

    #[test]
    fn a_peer_is_a_descriptor_for_the_same_fifo_with_the_other_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let me = std::process::id().to_string();
        let proc_root = Path::new("/proc");

        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        let fifo = identity(&reader);
        assert!(holds_peer(
            &proc_root.join(&me),
            me.parse().unwrap(),
            fifo,
            FifoPeer::Reader,
            None
        ));
        assert!(
            !holds_peer(
                &proc_root.join(&me),
                me.parse().unwrap(),
                fifo,
                FifoPeer::Writer,
                None
            ),
            "a reader is not the writer a read waits for"
        );

        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(holds_peer(
            &proc_root.join(&me),
            me.parse().unwrap(),
            fifo,
            FifoPeer::Writer,
            None
        ));

        // Another FIFO's writer is not a peer.
        let other_dir = tempfile::tempdir().unwrap();
        let other = fifo_in(other_dir.path());
        let _other_reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&other)
            .unwrap();
        let _other_writer = OpenOptions::new().write(true).open(&other).unwrap();
        drop(writer);
        assert!(
            !holds_peer(
                &proc_root.join(&me),
                me.parse().unwrap(),
                fifo,
                FifoPeer::Writer,
                None
            ),
            "the other FIFO's writer must not count"
        );
    }

    #[test]
    fn a_read_write_descriptor_is_both_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let me = std::process::id().to_string();
        let both = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let fifo = identity(&both);
        assert!(holds_peer(
            &Path::new("/proc").join(&me),
            me.parse().unwrap(),
            fifo,
            FifoPeer::Writer,
            None
        ));
        assert!(holds_peer(
            &Path::new("/proc").join(&me),
            me.parse().unwrap(),
            fifo,
            FifoPeer::Reader,
            None
        ));
    }

    #[test]
    fn the_callers_own_description_is_not_its_peer() {
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let me = std::process::id() as i32;
        let both = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&both);
        let fifo = identity(&both);
        let own = Some((me, fd));
        let proc_root = Path::new("/proc");
        assert!(
            !holds_peer(
                &proc_root.join(me.to_string()),
                me,
                fifo,
                FifoPeer::Writer,
                own
            ),
            "a read-write description must not be its own writer"
        );
        let alias = both.try_clone().unwrap();
        assert!(
            !holds_peer(
                &proc_root.join(me.to_string()),
                me,
                fifo,
                FifoPeer::Writer,
                own
            ),
            "a dup of the caller's description must not count either"
        );
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(
            holds_peer(
                &proc_root.join(me.to_string()),
                me,
                fifo,
                FifoPeer::Writer,
                own
            ),
            "another description open for writing is a peer"
        );
        drop((alias, writer));
    }

    #[test]
    fn an_o_path_handle_is_neither_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let me = std::process::id().to_string();
        let table = Path::new("/proc").join(&me);
        let task: i32 = me.parse().unwrap();
        let observer = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH)
            .open(&path)
            .unwrap();
        let fifo = identity(&observer);
        assert!(
            !holds_peer(&table, task, fifo, FifoPeer::Reader, None),
            "an O_PATH handle must not count as a reader"
        );
        assert!(!holds_peer(&table, task, fifo, FifoPeer::Writer, None));
        assert!(FifoPeer::Reader.accepts_flags(libc::O_RDONLY));
        assert!(!FifoPeer::Reader.accepts_flags(libc::O_PATH | libc::O_CLOEXEC));
    }

    #[test]
    fn fdinfo_flags_are_the_kernel_status_flags() {
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&reader);
        let pid = std::process::id() as i32;
        assert!(fifo_of(&reverie::BackendCapabilities::PTRACE, None, pid, fd).is_some());
        assert_eq!(kernel_nonblocking(pid, fd), Some(true));
        // SAFETY: plain fcntl on a descriptor this test owns.
        unsafe { libc::fcntl(fd, libc::F_SETFL, 0) };
        assert_eq!(kernel_nonblocking(pid, fd), Some(false));
    }
}
