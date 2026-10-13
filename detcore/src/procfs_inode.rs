/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Keys for numbering the procfs entries that the host renumbers.
//!
//! Linux numbers most procfs inodes when it builds their dentries: every
//! per-process entry (`/proc/<pid>`, `/proc/<pid>/fd/<n>`, ...) and every
//! `/proc/sys` entry takes the next value of one host-wide counter
//! (`get_next_ino`). An unused dentry can be evicted at any time by memory
//! pressure on the host, or dropped by a failed lookup, and the next lookup
//! or `readdir` builds it again under a new number. The inode pool numbers a
//! host `(device, inode)` it has not seen as a new file, so the guest saw a
//! number that depended on whether the host had rebuilt the entry since the
//! guest last saw it: a second `lsof` listing of `/proc/3/fd` after the host
//! rebuilt its entries got new numbers in one `--verify` run and the old ones
//! in the other.
//!
//! So a procfs sighting also names the entry ([`ProcfsName`]): the key of its
//! path within procfs ([`path_inode`]), which does not depend on where procfs
//! is mounted, and the task IDs that path names. The pool numbers an entry by
//! its host inode first: a host inode it already knows keeps its number, so
//! aliases (one entry listed in `/proc/<pid>/net` and reached as
//! `/proc/<pid>/task/<pid>/net/tcp`) share it. Only a host inode the pool has
//! never seen, reached by a path it has seen, is that path's entry rebuilt,
//! and takes the path's number. A path key only ever leads to a host key; it
//! numbers nothing itself. A task ID that a new thread reuses names a new
//! task, whose entries are new files ([`with_incarnation`]), and an entry a
//! directory snapshot lists names the tasks that held its IDs when the
//! snapshot was taken ([`ProcfsName::in_snapshot`]).
//!
//! Detcore names an entry only once the guest's mount table says its device
//! is a procfs mount ([`procfs_mounts`]), and reads its canonical path from a
//! magic link in the guest thread's `/proc/<tid>` ([`inspect_link`]): the
//! link of the descriptor or working directory the call described, or, for a
//! call that named a path, the link of an `O_PATH` descriptor that Detcore
//! opens with the guest's own path, directory and root, and closes again
//! ([`read_open`], [`read_close`]). The mount the object was reached through,
//! which `statx` names, gives the mount point to take off that path and the
//! directory of procfs it shows, once that mount point, resolved without
//! following a link, leads to the root of the same mount ([`path_within`],
//! [`mount_root`]). When it cannot name a procfs entry (the mount table cannot
//! be read, the guest has no descriptor left or no longer holds the path it
//! named, the call named a path while the guest's threads are not
//! sequentialized, an injected call had no effect in every attempt, the
//! object found is not the one the guest saw, or was reached through a mount
//! the table does not list at a mount point that holds it) it numbers
//! the entry by its host inode and records a determinism loss, so the run is
//! not compared.
//!
//! Each of these numbers the entry as Detcore did before:
//!
//! - An entry rebuilt before Detcore first saw its path gets a new number,
//!   which that path then leads to.
//! - A path lookup crosses every mount on the guest's path again, and the
//!   check of a mount point every mount above it, so a mount on a FUSE file
//!   system whose server is a guest Detcore holds could stall it.
//! - `get_next_ino` wraps at 2^32, so a directory other than a procfs root can
//!   have inode 1, and has its link count reported as 1.
//! - Under a backend whose guest thread IDs do not name host tasks, there is
//!   no `/proc/<tid>` to read, and procfs entries keep their host keys: KVM's
//!   guest threads are not host tasks, and DBT's thread IDs are its
//!   scheduler's virtual IDs, in no PID namespace that gives its host tasks
//!   those IDs.

use std::collections::HashMap;
use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use detcore_model::fd::RawInode;
use reverie::BackendCapabilities;
use reverie::syscalls::Errno;
use serde::Deserialize;
use serde::Serialize;

use crate::util::describe_io_error;

/// The inode number of every procfs root (`PROC_ROOT_INO` in Linux).
pub(crate) const PROC_ROOT_INO: u64 = 1;

/// The block size procfs reports (`s_blocksize` is 1024).
pub(crate) const PROC_BLOCK_SIZE: i64 = 1024;

/// The device every procfs file reports to the guest, on every backend:
/// Detcore's own number (major 0, minor 4095), never a host device. The
/// device pool maps the guest's procfs device to it and never mints it for
/// another device (<https://github.com/rrnewton/hermit/issues/4213>).
pub(crate) const VIRTUAL_PROC_DEVICE: u64 = 0x00f0_00ff;

/// The inode number a procfs file Detcore renders reports, on every backend:
/// a function of its path alone, so that no other observation of the run can
/// change it. It is in a range Detcore's inode pool, which counts up from a
/// few thousand, never reaches.
pub(crate) fn rendered_inode(path: &Path) -> u64 {
    const RENDERED_INODE_BASE: u64 = 0xf000_0000;
    RENDERED_INODE_BASE | (fnv1a(FNV_OFFSET_BASIS, path.as_os_str().as_bytes()) & 0x0fff_ffff)
}

/// The bit every path key sets, which no 32-bit procfs inode number has.
const PATH_KEY_BIT: u64 = 1 << 63;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// Continues a 64-bit FNV-1a hash from `hash` over `bytes`.
fn fnv1a(hash: u64, bytes: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(hash, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

/// Whether a stat result can describe a procfs file: procfs has an anonymous
/// device (major 0) and a 1024-byte block size. Other file systems match too
/// (devpts does), so a match only means the caller should ask the kernel.
pub(crate) fn may_be_procfs(device: u64, block_size: i64) -> bool {
    libc::major(device) == 0 && block_size == PROC_BLOCK_SIZE
}

/// Whether the thread IDs of a guest that `backend` runs name host tasks,
/// whose `/proc/<tid>` the tracer can read, for a guest thread whose host
/// thread ID is `physical_tid` when the backend supplies one beside its own.
/// Not under KVM, the backend that owns process signal control, whose guest
/// threads are not host tasks, nor under DBT, which supplies `physical_tid`
/// as its thread IDs are its scheduler's virtual IDs, in no PID namespace
/// that gives its host tasks those IDs: `/proc/<tid>` would describe an
/// unrelated host task, if any.
pub(crate) fn guest_tids_name_host_tasks(
    backend: &BackendCapabilities,
    physical_tid: Option<i32>,
) -> bool {
    !backend.provides_process_signal_control && physical_tid.is_none()
}

/// The key for the procfs entry at `path` within procfs on `device`: the
/// 64-bit FNV-1a hash of the path's bytes, with [`PATH_KEY_BIT`] set.
pub(crate) fn path_inode(device: u64, path: &Path) -> RawInode {
    RawInode::new(
        device,
        PATH_KEY_BIT | fnv1a(FNV_OFFSET_BASIS, path.as_os_str().as_bytes()),
    )
}

/// The path key `key` of an entry that names task ID `task`, for the
/// `incarnation`th thread created under that ID: the hash continued over
/// both, with [`PATH_KEY_BIT`] set.
pub(crate) fn with_incarnation(key: RawInode, task: i32, incarnation: u32) -> RawInode {
    let hash = fnv1a(
        fnv1a(key.ino, &task.to_le_bytes()),
        &incarnation.to_le_bytes(),
    );
    RawInode::new(key.dev, PATH_KEY_BIT | hash)
}

/// A procfs entry's name: the key of its path within procfs, and the task IDs
/// that path names, a process and then a thread in its `task` directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcfsName {
    pub path: RawInode,
    pub tasks: Vec<i32>,
    /// For an entry of a directory snapshot, how many guest threads Detcore
    /// had counted when the snapshot was taken (`InodePool::count_incarnation`):
    /// the entry names the tasks that held its task IDs then, though one may
    /// since have exited and a new thread taken its ID. `None` for an entry
    /// the guest reaches as it asks.
    pub threads_counted: Option<u64>,
}

impl ProcfsName {
    /// The name of the entry at `within`, its path within procfs on `device`.
    pub(crate) fn new(device: u64, within: &Path) -> Self {
        Self {
            path: path_inode(device, within),
            tasks: tasks_named(within),
            threads_counted: None,
        }
    }

    /// This name, for an entry of a directory snapshot taken when Detcore had
    /// counted `threads_counted` guest threads.
    pub(crate) fn in_snapshot(self, threads_counted: u64) -> Self {
        Self {
            threads_counted: Some(threads_counted),
            ..self
        }
    }
}

/// The task ID a path component names, if it is one.
fn task_id(component: Option<Component<'_>>) -> Option<i32> {
    match component {
        Some(Component::Normal(name)) => name
            .to_str()?
            .parse::<i32>()
            .ok()
            .filter(|id| *id > 0 && name.as_bytes()[0] != b'+'),
        _ => None,
    }
}

/// The task IDs a path within procfs names: the process of `/<pid>/...`, and
/// then the thread of `/<pid>/task/<tid>/...`.
fn tasks_named(within: &Path) -> Vec<i32> {
    let mut components = within
        .components()
        .skip_while(|component| *component == Component::RootDir);
    let Some(pid) = task_id(components.next()) else {
        return Vec::new();
    };
    let mut tasks = vec![pid];
    if components.next() == Some(Component::Normal(OsStr::new("task")))
        && let Some(tid) = task_id(components.next())
    {
        tasks.push(tid);
    }
    tasks
}

/// A procfs mount in a guest's mount table: its ID, the directory of procfs it
/// shows (`/` unless a subtree is bind-mounted) and where it is mounted, below
/// the path the guest's root reads as (see [`procfs_mounts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcfsMount {
    id: u64,
    root: PathBuf,
    mount_point: PathBuf,
}

/// The procfs mounts of `device` in the mount namespace of the guest thread
/// `tid`, by its `/proc/<tid>/mountinfo`, which Linux writes without touching
/// any mounted file system: `Ok(None)` when `device` is not procfs there, and
/// an error when the tracer cannot tell, or when the descriptor that read the
/// table did not close (see [`read_checking_close`]).
///
/// The table gives each mount point relative to the guest thread's root,
/// while a magic link reads as a path relative to the root of the thread
/// that reads it (the tracer's, or, under SaBRe, the guest's own) when it
/// leads below that root, and from the root of the mount namespace when it
/// does not. So each mount point is put below the path the guest thread's
/// root reads as, from its `/proc/<tid>/root`, which the same thread reads.
/// That puts a mount point and a link in one frame when the guest's root is
/// below the reader's, but not always when it is not, so [`path_within`]
/// uses a mount point only where the reader finds the mount.
///
/// Detcore refuses `mount`, `unshare` and `setns` once it starts, so a guest's
/// procfs mount stays one: the mounts of a device found to be procfs are
/// remembered with its mount namespace and the guest thread's root, which a
/// `chroot` changes, by its mount, device and inode, and the path it reads
/// as. Any other device is looked up again, as it may yet become one. A
/// rename of a directory above a mount point moves the mount and changes
/// none of these, so [`path_within`] asks for the table again (`reread`),
/// which reads it and remembers what it finds, or forgets the device when it
/// is no longer procfs there. A guest thread that changes the root while
/// Detcore reads for another thread that shares it, which thread
/// sequentialization rules out, can leave the two paths in different frames.
pub(crate) fn procfs_mounts(
    tid: i32,
    device: u64,
    reread: bool,
) -> io::Result<Option<Arc<[ProcfsMount]>>> {
    // The mount namespace, the root's mount, device, inode and path, and
    // the device.
    type Key = (u64, Option<u64>, u64, u64, PathBuf, u64);
    static PROCFS_DEVICES: OnceLock<Mutex<HashMap<Key, Arc<[ProcfsMount]>>>> = OnceLock::new();
    let namespace = std::fs::metadata(format!("/proc/{tid}/ns/mnt"))?;
    let root_link = PathBuf::from(format!("/proc/{tid}/root"));
    let root = identify(&root_link, true)?;
    let root_path = std::fs::read_link(&root_link)?;
    let key = (
        namespace.ino(),
        root.mount_id,
        root.device,
        root.inode,
        root_path.clone(),
        device,
    );
    let known = PROCFS_DEVICES.get_or_init(Default::default);
    if !reread && let Some(mounts) = known.lock().unwrap().get(&key) {
        return Ok(Some(mounts.clone()));
    }
    let mountinfo = read_checking_close(&format!("/proc/{tid}/mountinfo"))?;
    let mounts = procfs_mounts_in(&mountinfo, device, &root_path);
    if mounts.is_empty() {
        known.lock().unwrap().remove(&key);
        return Ok(None);
    }
    let mounts: Arc<[ProcfsMount]> = mounts.into();
    known.lock().unwrap().insert(key, mounts.clone());
    Ok(Some(mounts))
}

/// How many times Detcore makes a call of its own to read the guest's mount
/// table that was interrupted (`EINTR`) before it gives up. Under SaBRe the
/// call runs in the guest's process, under the guest's seccomp filters, and
/// a filter can return `EINTR` every time without running it.
const INTERRUPTED_ATTEMPTS: usize = 8;

/// The contents of the file at `path`, read through a descriptor that must
/// close. Under SaBRe, Detcore runs in the guest's process, so its
/// descriptors are in the guest's descriptor table. A `close` that a seccomp
/// filter refuses leaves the descriptor open there, where the guest sees it
/// and its later descriptors get other numbers, and `File` ignores that
/// error when it drops. So a failed close is an error, as it is for the
/// descriptors Detcore opens in the guest to name a path.
///
/// The open and each read are tried again when interrupted, but only as
/// [`retrying_interrupted`] allows, where `File::open` and `read_to_end`
/// would try again without end. The close is not: Linux releases the
/// descriptor before anything that can interrupt it.
fn read_checking_close(path: &str) -> io::Result<Vec<u8>> {
    let c_path = c_path(Path::new(path))?;
    let fd = retrying_interrupted("open", path, || {
        // SAFETY: `c_path` is NUL-terminated.
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(fd)
        }
    })?;
    let read = read_to_end(fd, path);
    // SAFETY: `fd` is the descriptor open returned, and nothing else uses it.
    if unsafe { libc::close(fd) } != 0 {
        return Err(io::Error::other(format!(
            "cannot close descriptor {fd} of {path}: {}",
            describe_io_error(&io::Error::last_os_error())
        )));
    }
    read
}

/// Everything left to read from the descriptor `fd` of `path`, one `read` at
/// a time, each tried again when interrupted only as [`retrying_interrupted`]
/// allows.
fn read_to_end(fd: RawFd, path: &str) -> io::Result<Vec<u8>> {
    let mut contents = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let count = retrying_interrupted("read", path, || {
            // SAFETY: `chunk` is writable for its whole length.
            let count = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            usize::try_from(count).map_err(|_| io::Error::last_os_error())
        })?;
        if count == 0 {
            return Ok(contents);
        }
        contents.extend_from_slice(&chunk[..count]);
    }
}

/// The result of `call`, the `name` call on `path`, which is made again
/// while it fails with `EINTR`, but at most [`INTERRUPTED_ATTEMPTS`] times.
/// Running out is an error of another kind, so that no caller tries again.
fn retrying_interrupted<T>(
    name: &str,
    path: &str,
    mut call: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    for _ in 0..INTERRUPTED_ATTEMPTS {
        match call() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            done => return done,
        }
    }
    Err(io::Error::other(format!(
        "the {name} of {path} was interrupted in each of {INTERRUPTED_ATTEMPTS} attempts"
    )))
}

/// The procfs mounts of `device` a `mountinfo` table lists: lines whose third
/// field is the device's `major:minor` and whose file system type, the field
/// after the `-` separator that ends the optional fields, is `proc`. Each
/// mount point, which the table gives relative to the root of the thread it
/// describes, is put below `guest_root`, the path that root reads as.
fn procfs_mounts_in(mountinfo: &[u8], device: u64, guest_root: &Path) -> Vec<ProcfsMount> {
    let wanted = format!("{}:{}", libc::major(device), libc::minor(device));
    mountinfo
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let fields: Vec<&[u8]> = line.split(|byte| *byte == b' ').collect();
            if fields.get(2).copied() != Some(wanted.as_bytes()) {
                return None;
            }
            let separator = 6 + fields.get(6..)?.iter().position(|field| *field == b"-")?;
            if fields.get(separator + 1).copied() != Some(b"proc".as_slice()) {
                return None;
            }
            let mount_point = unescape(fields[4]);
            Some(ProcfsMount {
                id: std::str::from_utf8(fields[0]).ok()?.parse().ok()?,
                root: unescape(fields[3]),
                mount_point: guest_root.join(mount_point.strip_prefix("/").unwrap_or(&mount_point)),
            })
        })
        .collect()
}

/// A `mountinfo` path with its octal escapes (`\040` for a space) decoded.
fn unescape(field: &[u8]) -> PathBuf {
    let mut path = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        let octal = field
            .get(index + 1..index + 4)
            .filter(|digits| field[index] == b'\\' && digits.iter().all(u8::is_ascii_digit))
            .filter(|digits| digits.iter().all(|digit| *digit < b'8'));
        match octal {
            Some(digits) => {
                path.push(digits.iter().fold(0u8, |value, digit| {
                    value.wrapping_mul(8).wrapping_add(digit - b'0')
                }));
                index += 4;
            }
            None => {
                path.push(field[index]);
                index += 1;
            }
        }
    }
    PathBuf::from(OsString::from_vec(path))
}

/// The path within procfs of the object at the canonical path `canonical`,
/// reached through the mount `mount_id`: its path below that mount's mount
/// point, joined to the mount's root. The mount point must be where the
/// reader finds the root of that mount: `mount_root` gives the mount whose
/// root a path leads to, as the thread that read `canonical` resolves it
/// (see [`mount_root`]).
///
/// That check makes the name the object's, whichever table the mount point
/// came from. Along a path that follows no link, which `mount_root` resolves
/// the mount point as, the reader reaches the root of a mount only through
/// its one mount point, and `canonical` reads, from the same root, through
/// that mount point and then the object's path within the mount. So a prefix of
/// `canonical` where the reader finds the mount is that mount point, and the
/// rest is the path within. Without it, a mount point remembered from before
/// a rename of a directory above it, or one read in another frame than the
/// link (see [`procfs_mounts`]), can be another prefix of `canonical`, and
/// give the entry another name with no loss.
///
/// When the check fails with `mounts`, `reread` gives the guest's table read
/// again, which is tried once more. An error is the reason no name was found.
pub(crate) fn path_within(
    mounts: &[ProcfsMount],
    mount_id: u64,
    canonical: &Path,
    reread: impl FnOnce() -> io::Result<Option<Arc<[ProcfsMount]>>>,
    mount_root: impl Fn(&Path) -> io::Result<Option<u64>>,
) -> Result<PathBuf, String> {
    if let Ok(within) = within_table(mounts, mount_id, canonical, &mount_root) {
        return Ok(within);
    }
    match reread() {
        Ok(Some(mounts)) => within_table(&mounts, mount_id, canonical, &mount_root),
        Ok(None) => Err(format!(
            "{} was reached through mount {mount_id}, and the guest's mount table, read again, lists no procfs mount of its device",
            canonical.display()
        )),
        Err(error) => Err(format!(
            "{} was reached through mount {mount_id}, and the guest's mount table cannot be read again: {}",
            canonical.display(),
            describe_io_error(&error)
        )),
    }
}

/// [`path_within`] with the mounts of one table.
fn within_table(
    mounts: &[ProcfsMount],
    mount_id: u64,
    canonical: &Path,
    mount_root: &impl Fn(&Path) -> io::Result<Option<u64>>,
) -> Result<PathBuf, String> {
    let reached = format!(
        "{} was reached through mount {mount_id}",
        canonical.display()
    );
    let mount = mounts
        .iter()
        .find(|mount| mount.id == mount_id)
        .ok_or_else(|| {
            format!("{reached}, which the guest's mount table does not list as a procfs mount")
        })?;
    let point = mount.mount_point.display();
    let rest = canonical.strip_prefix(&mount.mount_point).map_err(|_| {
        format!("{reached}, which the guest's mount table lists at {point}, not above it")
    })?;
    match mount_root(&mount.mount_point) {
        Ok(Some(found)) if found == mount_id => {}
        Ok(Some(found)) => {
            return Err(format!(
                "{reached}, and {point}, where the guest's mount table lists it, is the root of mount {found}"
            ));
        }
        Ok(None) => {
            return Err(format!(
                "{reached}, and {point}, where the guest's mount table lists it, is the root of no mount"
            ));
        }
        Err(error) => {
            return Err(format!(
                "{reached}, and cannot tell whether {point}, where the guest's mount table lists it, holds it: {}",
                describe_io_error(&error)
            ));
        }
    }
    if rest.as_os_str().is_empty() {
        Ok(mount.root.clone())
    } else {
        Ok(mount.root.join(rest))
    }
}

/// The canonical path of the procfs object behind the magic link `link` (a
/// `/proc/<tid>/fd/<n>` or `/proc/<tid>/cwd`), which must be inode `inode` on
/// `device`, and the ID of the mount it was reached through, which places the
/// path in the guest's mount table (see [`path_within`]). Following such a
/// link touches only procfs and the object, which is on procfs.
///
/// Once a task is reaped, a stat through a link to one of its entries fails,
/// and the link reads as the entry's path with ` (deleted)` appended, as for
/// a removed file. The task can be reaped between the two, so such a path is
/// refused too (see [`refuse_deleted`]).
pub(crate) fn inspect_link(link: &Path, device: u64, inode: u64) -> Result<(PathBuf, u64), String> {
    let mount_id = check_link_object(link, device, inode)?
        .ok_or_else(|| format!("the kernel does not report the mount of {}", link.display()))?;
    let canonical = std::fs::read_link(link).map_err(|error| {
        format!(
            "cannot read the link {}: {}",
            link.display(),
            describe_io_error(&error)
        )
    })?;
    Ok((refuse_deleted(link, canonical)?, mount_id))
}

/// `canonical`, read from the magic link `link`, unless it ends in
/// ` (deleted)`, as the path of an entry of a reaped task does. No procfs
/// name ends that way, so such a path names nothing.
fn refuse_deleted(link: &Path, canonical: PathBuf) -> Result<PathBuf, String> {
    if canonical.as_os_str().as_bytes().ends_with(b" (deleted)") {
        return Err(format!(
            "{} leads to {}, an entry of a task that has exited",
            link.display(),
            canonical.display()
        ));
    }
    Ok(canonical)
}

/// Whether the object behind the magic link `link`, which must be inode
/// `inode` on `device`, is on a procfs filesystem, whatever mount it was
/// reached through.
pub(crate) fn is_on_procfs(link: &Path, device: u64, inode: u64) -> Result<bool, String> {
    check_link_object(link, device, inode)?;
    let path = CString::new(link.as_os_str().as_bytes())
        .map_err(|_| format!("{} contains a NUL byte", link.display()))?;
    let mut buf = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `buf` is large enough for statfs.
    if unsafe { libc::statfs(path.as_ptr(), buf.as_mut_ptr()) } != 0 {
        return Err(format!(
            "cannot statfs {}: {}",
            link.display(),
            describe_io_error(&io::Error::last_os_error())
        ));
    }
    // SAFETY: statfs succeeded, so it filled `buf`.
    Ok(unsafe { buf.assume_init() }.f_type == libc::PROC_SUPER_MAGIC)
}

/// Requires that the magic link `link` lead to inode `inode` on `device`, and
/// returns the ID of the mount the object was reached through, when the
/// kernel reports one.
fn check_link_object(link: &Path, device: u64, inode: u64) -> Result<Option<u64>, String> {
    let found = identify(link, true).map_err(|error| {
        format!(
            "cannot stat {}: {}",
            link.display(),
            describe_io_error(&error)
        )
    })?;
    if found.device != device || found.inode != inode {
        return Err(format!(
            "{} is not the object the guest saw",
            link.display()
        ));
    }
    Ok(found.mount_id)
}

/// The device, inode and mount of a file, by `statx`.
struct FileIdentity {
    device: u64,
    inode: u64,
    /// The ID of the mount the file was reached through, the first field of
    /// its line in `mountinfo`, when the kernel reports it (`STATX_MNT_ID`,
    /// from Linux 5.8).
    mount_id: Option<u64>,
    /// Whether the file is the root of that mount, when the kernel reports it
    /// (`STATX_ATTR_MOUNT_ROOT`, from Linux 5.8).
    mount_root: Option<bool>,
}

/// The identity of the file at `path`, following a final link when `follow`
/// is set, as a magic link is followed: to the object it leads to, through
/// the mount it was reached through.
fn identify(path: &Path, follow: bool) -> io::Result<FileIdentity> {
    // `AT_NO_AUTOMOUNT`, which `stat` implies, so that looking does not
    // trigger an automount at the path's last component.
    let mut flags = libc::AT_NO_AUTOMOUNT;
    if !follow {
        flags |= libc::AT_SYMLINK_NOFOLLOW;
    }
    identify_at(libc::AT_FDCWD, &c_path(path)?, flags)
}

/// `path` as a C string.
fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other(format!("{} contains a NUL byte", path.display())))
}

/// The identity of the file at `path`, looked up from the directory `dirfd`
/// with the statx flags `flags`.
fn identify_at(dirfd: RawFd, path: &CStr, flags: i32) -> io::Result<FileIdentity> {
    let mut buf = std::mem::MaybeUninit::<libc::statx>::zeroed();
    let mask = libc::STATX_INO | libc::STATX_MNT_ID;
    // SAFETY: `path` is NUL-terminated and `buf` is large enough for statx.
    if unsafe { libc::statx(dirfd, path.as_ptr(), flags, mask, buf.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statx succeeded, so it filled `buf`.
    let found = unsafe { buf.assume_init() };
    if found.stx_mask & libc::STATX_INO == 0 {
        return Err(io::Error::other("statx reported no inode number"));
    }
    let mount_root = libc::STATX_ATTR_MOUNT_ROOT as u64;
    Ok(FileIdentity {
        device: libc::makedev(found.stx_dev_major, found.stx_dev_minor),
        inode: found.stx_ino,
        mount_id: (found.stx_mask & libc::STATX_MNT_ID != 0).then_some(found.stx_mnt_id),
        mount_root: (found.stx_attributes_mask & mount_root != 0)
            .then_some(found.stx_attributes & mount_root != 0),
    })
}

/// The ID of the mount whose root the path `path` leads to, as the calling
/// thread resolves it, without following a link anywhere in the path: `None`
/// when it leads to no mount's root, and an error when the path holds a link,
/// or the kernel does not report mount roots.
///
/// The path is resolved with `openat2`'s `RESOLVE_NO_SYMLINKS`, which refuses
/// every link, a magic link included. A path that follows no link and holds
/// no `..` reaches the root of a mount only through that mount's one mount
/// point, which [`path_within`] relies on; a link can lead to the same
/// directory along another path. The descriptor is opened `O_PATH`, which,
/// like `stat`, does not trigger an automount at the path's last component
/// (the lookup of an earlier component can, as any lookup's can), and must
/// close, as for [`read_checking_close`].
pub(crate) fn mount_root(path: &Path) -> io::Result<Option<u64>> {
    let path = c_path(path)?;
    // SAFETY: `open_how` holds only integers, and zero is the value the
    // kernel requires of every field this call does not set.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (libc::O_PATH | libc::O_CLOEXEC) as u64;
    how.resolve = libc::RESOLVE_NO_SYMLINKS;
    // SAFETY: `path` is NUL-terminated, and `how` is an `open_how` of the
    // size passed. glibc has no wrapper for openat2.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how as *const libc::open_how,
            std::mem::size_of::<libc::open_how>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = RawFd::try_from(fd)
        .map_err(|_| io::Error::other(format!("openat2 returned {fd}, which is no descriptor")))?;
    let found = identify_at(fd, c"", libc::AT_EMPTY_PATH | libc::AT_NO_AUTOMOUNT);
    // SAFETY: `fd` is the descriptor openat2 opened, and nothing else uses it.
    if unsafe { libc::close(fd) } != 0 {
        return Err(io::Error::other(format!(
            "cannot close descriptor {fd}: {}",
            describe_io_error(&io::Error::last_os_error())
        )));
    }
    let found = found?;
    match (found.mount_root, found.mount_id) {
        (Some(false), _) => Ok(None),
        (Some(true), Some(id)) => Ok(Some(id)),
        _ => Err(io::Error::other(
            "the kernel does not report the root of a mount",
        )),
    }
}

/// What a system call Detcore injected into a guest did, read from its
/// result: it ran, and returned this; it had no effect, as the result
/// reports (`NotRun`), and may be injected again; or it failed, for this
/// reason.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Injected<T> {
    Ran(T),
    NotRun(String),
    Failed(String),
}

/// What an `openat` Detcore injected into the guest to open an `O_PATH`
/// descriptor did, from its result.
///
/// Reverie reports an injection that a signal stopped before it ran as a
/// restart (`ERESTARTSYS`). An open that returns a restart or `EINTR` opened
/// no descriptor, whether it ran or not (a signal can interrupt a lookup, as
/// on FUSE), so it is tried again. Any other result is the call's own: a
/// descriptor, or the error that refused the open.
pub(crate) fn read_open(result: Result<i64, Errno>) -> Injected<RawFd> {
    match result {
        Ok(fd) => RawFd::try_from(fd).map_or_else(
            |_| Injected::Failed(format!("openat returned {fd}, which is no descriptor")),
            Injected::Ran,
        ),
        Err(
            errno @ (Errno::EINTR
            | Errno::ERESTARTSYS
            | Errno::ERESTARTNOINTR
            | Errno::ERESTARTNOHAND
            | Errno::ERESTART_RESTARTBLOCK),
        ) => Injected::NotRun(errno.to_string()),
        Err(errno) => Injected::Failed(format!("cannot open the guest's path again: {errno}")),
    }
}

/// What a `close` Detcore injected into the guest to close the `O_PATH`
/// descriptor it opened did, from its result.
///
/// Linux's close releases the descriptor before anything that can fail, and
/// an `O_PATH` descriptor has nothing to flush, so a close that runs returns
/// 0, and the descriptor is gone. A restart means that the close did not run,
/// as for [`read_open`]. Any other error, `EINTR` and `EBADF` among them, is
/// a seccomp filter the guest inherited refusing the close (or a descriptor
/// that was not open), which leaves the descriptor open.
pub(crate) fn read_close(result: Result<i64, Errno>) -> Injected<()> {
    match result {
        Ok(0) => Injected::Ran(()),
        Ok(other) => Injected::Failed(format!(
            "close of the guest's O_PATH descriptor returned {other}, which close never returns"
        )),
        Err(
            errno @ (Errno::ERESTARTSYS
            | Errno::ERESTARTNOINTR
            | Errno::ERESTARTNOHAND
            | Errno::ERESTART_RESTARTBLOCK),
        ) => Injected::NotRun(errno.to_string()),
        Err(errno) => Injected::Failed(format!(
            "close of the guest's O_PATH descriptor returned {errno:?}"
        )),
    }
}

/// A procfs directory being listed, by its path within procfs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcfsDirectory {
    path: PathBuf,
}

impl ProcfsDirectory {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The path within procfs of the entry named `name` in this directory.
    /// The `..` of procfs's root is the root itself.
    fn entry_path(&self, name: &[u8]) -> PathBuf {
        match name {
            b"." => self.path.clone(),
            b".." => self.path.parent().unwrap_or(&self.path).to_path_buf(),
            name => self.path.join(OsStr::from_bytes(name)),
        }
    }
}

/// How to key the inode numbers of the entries of one directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DirectoryInodes {
    /// The host device of the directory, which every entry shares.
    device: u64,
    /// The directory, when it is on procfs and its entries name their paths.
    procfs: Option<ProcfsDirectory>,
}

impl DirectoryInodes {
    pub(crate) fn new(device: u64, procfs: Option<ProcfsDirectory>) -> Self {
        Self { device, procfs }
    }

    /// The host device of the directory, which every entry shares.
    pub(crate) fn device(&self) -> u64 {
        self.device
    }

    /// The inode number of the entry named `name` if it is a procfs file
    /// Detcore renders, which names it by its path alone ([`rendered_inode`]).
    pub(crate) fn rendered_entry_inode(&self, name: &[u8]) -> Option<u64> {
        let within = self.procfs.as_ref()?.entry_path(name);
        let relative = within.strip_prefix("/").unwrap_or(&within);
        let path = Path::new("/proc").join(relative);
        crate::procfs::ProcfsFile::from_path(&path)?
            .rendered_path()
            .map(rendered_inode)
    }

    /// The keys for the entry named `name`, which the host listed with inode
    /// number `host_ino`: its host key, and its name when the directory is on
    /// procfs.
    pub(crate) fn entry_inode(&self, name: &[u8], host_ino: u64) -> (RawInode, Option<ProcfsName>) {
        let procfs = self
            .procfs
            .as_ref()
            .map(|directory| ProcfsName::new(self.device, &directory.entry_path(name)));
        (RawInode::new(self.device, host_ino), procfs)
    }
}

/// Runs `work`, and then puts back the calling thread's `errno` as it was.
/// Under SaBRe, Detcore shares libc, and so `errno`, with the guest thread
/// whose call it handles, so a host call of its own that fails, such as a
/// descriptor lookup that finds nothing, would otherwise change the `errno`
/// the guest reads after its call returns.
pub(crate) async fn preserving_errno<F: std::future::Future>(work: F) -> F::Output {
    let saved = nix::errno::Errno::last_raw();
    let output = work.await;
    nix::errno::Errno::set_raw(saved);
    output
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::fd::AsRawFd;

    use super::*;

    /// A host call that fails inside `preserving_errno` leaves the caller's
    /// `errno`, which under SaBRe is the guest's, as it was.
    #[test]
    fn a_failed_host_call_leaves_the_callers_errno() {
        const SENTINEL: i32 = 4242;
        nix::errno::Errno::set_raw(SENTINEL);
        let inside = futures::executor::block_on(preserving_errno(async {
            assert!(std::fs::symlink_metadata("/proc/self/fd/-1").is_err());
            nix::errno::Errno::last_raw()
        }));
        assert_eq!(inside, libc::ENOENT, "the failed call set errno");
        assert_eq!(nix::errno::Errno::last_raw(), SENTINEL);
    }

    /// Make every call of the calling thread to one of `syscalls` fail with
    /// `errno` without running, as a seccomp filter can. The filter binds this
    /// thread and what it starts afterwards, and no other thread.
    #[cfg(target_arch = "x86_64")]
    fn deny_in_this_thread(syscalls: &[i64], errno: i32) {
        const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
        let load = |offset: u32| libc::sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            jt: 0,
            jf: 0,
            k: offset,
        };
        let jump_if = |value: u32, jt: u8, jf: u8| libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            jt,
            jf,
            k: value,
        };
        let ret = |action: u32| libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: action,
        };
        // seccomp_data: nr at 0, arch at 4. Each match jumps over the
        // matches after it and the allow, to the denial.
        let mut program = vec![
            load(4),
            jump_if(AUDIT_ARCH_X86_64, 1, 0),
            ret(libc::SECCOMP_RET_ALLOW),
            load(0),
        ];
        for (index, syscall) in syscalls.iter().enumerate() {
            let over = u8::try_from(syscalls.len() - index).unwrap();
            program.push(jump_if(u32::try_from(*syscall).unwrap(), over, 0));
        }
        program.push(ret(libc::SECCOMP_RET_ALLOW));
        program.push(ret(libc::SECCOMP_RET_ERRNO | errno as u32));
        let fprog = libc::sock_fprog {
            len: program.len() as u16,
            filter: program.as_mut_ptr(),
        };
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
            0
        );
        let installed = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                0,
                &fprog as *const libc::sock_fprog,
            )
        };
        assert_eq!(installed, 0, "seccomp: {}", io::Error::last_os_error());
    }

    /// The error of a read of this thread's mount table, for the device of
    /// `/`, with `syscalls` denied as [`deny_in_this_thread`] denies them.
    #[cfg(target_arch = "x86_64")]
    fn mount_table_error_with_denied(syscalls: &'static [i64], errno: i32) -> String {
        let root = std::fs::metadata("/").unwrap().dev();
        std::thread::spawn(move || {
            deny_in_this_thread(syscalls, errno);
            let tid = unsafe { libc::gettid() };
            let error = procfs_mounts(tid, root, false).unwrap_err();
            format!("{tid} {}", describe_io_error(&error))
        })
        .join()
        .unwrap()
    }

    /// Under SaBRe, Detcore reads a guest's mount table through a descriptor
    /// in the guest's own descriptor table. It reads the table again for any
    /// device it has not found to be procfs, such as that of `/`, and when a
    /// filter refuses that descriptor's close, the read is an error, which
    /// records a determinism loss. The descriptor is still open, as the
    /// guest would see it. The error names the errno without the C library's
    /// message, which can allocate on the guest's heap (see
    /// [`describe_io_error`]).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_mount_table_read_whose_close_is_refused_is_an_error() {
        let error = mount_table_error_with_denied(&[libc::SYS_close], libc::EPERM);
        let (_, error) = error.split_once(' ').unwrap();
        let fd: i32 = error
            .strip_prefix("cannot close descriptor ")
            .and_then(|rest| rest.split_once(' '))
            .and_then(|(fd, _)| fd.parse().ok())
            .unwrap_or_else(|| panic!("not a refused close: {error}"));
        assert!(error.ends_with(": PermissionDenied (errno 1)"), "{error}");
        assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0, "{error}");
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }

    /// A filter that interrupts every read of the mount table ends the read
    /// with an error after a bounded number of attempts, which records a
    /// determinism loss, where `read_to_end` would try again without end.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_mount_table_read_that_is_always_interrupted_is_an_error() {
        let error = mount_table_error_with_denied(&[libc::SYS_read], libc::EINTR);
        let (tid, error) = error.split_once(' ').unwrap();
        assert_eq!(
            error,
            format!(
                "the read of /proc/{tid}/mountinfo was interrupted in each of {INTERRUPTED_ATTEMPTS} attempts"
            )
        );
    }

    /// The same for a filter that interrupts every open of the mount table,
    /// where `File::open` would try again without end.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_mount_table_open_that_is_always_interrupted_is_an_error() {
        let error = mount_table_error_with_denied(&[libc::SYS_open, libc::SYS_openat], libc::EINTR);
        let (tid, error) = error.split_once(' ').unwrap();
        assert_eq!(
            error,
            format!(
                "the open of /proc/{tid}/mountinfo was interrupted in each of {INTERRUPTED_ATTEMPTS} attempts"
            )
        );
    }

    /// An interrupted call is made again, up to [`INTERRUPTED_ATTEMPTS`]
    /// times in all; any other result ends the attempts.
    #[test]
    fn an_interrupted_call_is_made_again_a_bounded_number_of_times() {
        let interrupted = || io::Error::from_raw_os_error(libc::EINTR);
        let mut calls = 0;
        let error = retrying_interrupted("read", "/x", || -> io::Result<()> {
            calls += 1;
            Err(interrupted())
        })
        .unwrap_err();
        assert_eq!(calls, INTERRUPTED_ATTEMPTS);
        assert_ne!(error.kind(), io::ErrorKind::Interrupted);
        let mut calls = 0;
        let read = retrying_interrupted("read", "/x", || {
            calls += 1;
            if calls < 3 { Err(interrupted()) } else { Ok(7) }
        });
        assert_eq!((read.unwrap(), calls), (7, 3));
        let mut calls = 0;
        let error = retrying_interrupted("open", "/x", || -> io::Result<()> {
            calls += 1;
            Err(io::Error::from_raw_os_error(libc::EACCES))
        })
        .unwrap_err();
        assert_eq!((error.raw_os_error(), calls), (Some(libc::EACCES), 1));
    }

    /// The whole of a file longer than one read is read.
    #[test]
    fn a_file_longer_than_one_read_is_read_whole() {
        let path = std::env::current_exe().unwrap();
        let path = path.to_str().unwrap();
        let expected = std::fs::read(path).unwrap();
        assert!(expected.len() > 2 * 4096, "too short to need several reads");
        assert!(read_checking_close(path).unwrap() == expected);
    }

    const PROC_DEVICE: u64 = libc::makedev(0, 23);

    fn proc_fd_listing() -> DirectoryInodes {
        DirectoryInodes::new(
            PROC_DEVICE,
            Some(ProcfsDirectory::new(PathBuf::from("/3/fd"))),
        )
    }

    fn name(path: &str) -> Option<ProcfsName> {
        Some(ProcfsName::new(PROC_DEVICE, Path::new(path)))
    }

    #[test]
    fn a_procfs_entry_names_its_path_whatever_its_host_inode() {
        let listing = proc_fd_listing();
        let (host, before) = listing.entry_inode(b"0", 4_123_456);
        let (rebuilt_host, rebuilt) = listing.entry_inode(b"0", 4_987_654);
        assert_eq!(host, RawInode::new(PROC_DEVICE, 4_123_456));
        assert_eq!(rebuilt_host, RawInode::new(PROC_DEVICE, 4_987_654));
        assert_eq!(before, rebuilt);
        // `stat` names the same entry by the same path.
        assert_eq!(before, name("/3/fd/0"));
        assert_eq!(before.unwrap().tasks, vec![3]);
        assert_ne!(listing.entry_inode(b"1", 4_123_456).1, name("/3/fd/0"));
    }

    #[test]
    fn other_directories_keep_host_keys() {
        let device = libc::makedev(8, 1);
        let listing = DirectoryInodes::new(device, None);
        assert_eq!(
            listing.entry_inode(b"a", 77),
            (RawInode::new(device, 77), None)
        );
        assert_ne!(listing.entry_inode(b"a", 77), listing.entry_inode(b"a", 78));
    }

    #[test]
    fn dot_entries_name_the_directory_and_its_parent() {
        let listing = proc_fd_listing();
        assert_eq!(listing.entry_inode(b".", 9).1, name("/3/fd"));
        assert_eq!(listing.entry_inode(b"..", 9).1, name("/3"));
        let root =
            DirectoryInodes::new(PROC_DEVICE, Some(ProcfsDirectory::new(PathBuf::from("/"))));
        assert_eq!(root.entry_inode(b".", PROC_ROOT_INO).1, name("/"));
        assert_eq!(root.entry_inode(b"..", PROC_ROOT_INO).1, name("/"));
        assert_eq!(root.entry_inode(b"self", 4_026_531_841).1, name("/self"));
        assert_eq!(
            root.entry_inode(b"self", 4_026_531_841).1.unwrap().tasks,
            Vec::<i32>::new()
        );
    }

    #[test]
    fn path_keys_cannot_equal_procfs_inodes() {
        let paths = ["/", "/1", "/3/fd/0", "/sys/kernel/pid_max"];
        let mut keys: Vec<RawInode> = paths
            .iter()
            .map(|path| path_inode(PROC_DEVICE, Path::new(path)))
            .collect();
        keys.push(with_incarnation(keys[1], 1, 2));
        keys.push(with_incarnation(keys[1], 1, 3));
        for key in &keys {
            assert_eq!(key.dev, PROC_DEVICE);
            assert!(key.ino > u64::from(u32::MAX), "{key:?}");
        }
        for (index, key) in keys.iter().enumerate() {
            assert!(!keys[index + 1..].contains(key), "{key:?}");
        }
        let other_mount = libc::makedev(0, 24);
        assert_ne!(
            path_inode(PROC_DEVICE, Path::new("/1")),
            path_inode(other_mount, Path::new("/1"))
        );
    }

    #[test]
    fn a_path_names_its_process_and_thread() {
        let tasks = |path: &str| tasks_named(Path::new(path));
        assert_eq!(tasks("/3"), vec![3]);
        assert_eq!(tasks("/3/fd/0"), vec![3]);
        assert_eq!(tasks("/3/task/7/fd/0"), vec![3, 7]);
        assert_eq!(tasks("/3/task"), vec![3]);
        assert_eq!(tasks("/3/task/self"), vec![3]);
        for path in [
            "/",
            "/self",
            "/sys/kernel/pid_max",
            "/0",
            "/-3",
            "/+3",
            "/3 (deleted)",
        ] {
            assert_eq!(tasks(path), Vec::<i32>::new(), "{path}");
        }
    }

    #[test]
    fn only_anonymous_devices_with_1024_byte_blocks_may_be_procfs() {
        assert!(may_be_procfs(PROC_DEVICE, 1024));
        assert!(!may_be_procfs(libc::makedev(8, 1), 1024));
        assert!(!may_be_procfs(PROC_DEVICE, 4096));
    }

    const MOUNTINFO: &[u8] = b"\
22 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
23 1 0:22 / /dev/pts rw,nosuid,noexec,relatime shared:2 - devpts devpts rw,mode=620
24 1 0:23 / /mnt/a\\040b rw,relatime - fuse.squashfuse_ll squashfuse_ll rw,user_id=0
25 22 0:24 / /proc/sys/fs/binfmt_misc rw,relatime shared:7 - autofs systemd-1 rw
26 1 0:25 / /srv/proc rw - proc proc rw
27 1 0:21 /sys /srv/my\\040sys rw master:3 - proc proc rw
";

    /// Detcore names procfs entries only where the guest's thread IDs are the
    /// host tasks' IDs. Under KVM and DBT they are not, so procfs entries keep
    /// their host keys there, rather than being named from whatever host task
    /// has the guest thread's ID.
    #[test]
    fn procfs_entries_are_named_only_where_thread_ids_are_host_ids() {
        assert!(guest_tids_name_host_tasks(
            &BackendCapabilities::PTRACE,
            None
        ));
        assert!(guest_tids_name_host_tasks(
            &BackendCapabilities::SABRE,
            None
        ));
        assert!(guest_tids_name_host_tasks(
            &BackendCapabilities::LITEINST_IN_GUEST,
            None
        ));
        assert!(!guest_tids_name_host_tasks(&BackendCapabilities::KVM, None));
        // DBT supplies each thread's host ID beside its virtual one
        // (detcore-dbt's `init_dbt_thread_state`), as would any backend whose
        // thread IDs are virtual.
        assert!(!guest_tids_name_host_tasks(
            &BackendCapabilities::DBT,
            Some(4_321)
        ));
        assert!(!guest_tids_name_host_tasks(
            &BackendCapabilities::PTRACE,
            Some(4_321)
        ));
    }

    /// Once its task has exited, an entry is not named: following the link
    /// fails, and the link's path, read in the moment before, names nothing.
    /// While the task runs, the same link names the entry.
    #[test]
    fn an_entry_of_a_task_that_has_exited_is_not_named() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let (finish, finished) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            sender.send(unsafe { libc::gettid() }).unwrap();
            finished.recv().unwrap();
        });
        let tid = receiver.recv().unwrap();
        let fds = File::open(format!("/proc/self/task/{tid}/fd")).unwrap();
        let metadata = fds.metadata().unwrap();
        let link = PathBuf::from(format!("/proc/self/fd/{}", fds.as_raw_fd()));
        let inspect = || inspect_link(&link, metadata.dev(), metadata.ino()).map(|(path, _)| path);
        let expected = format!("/proc/{}/task/{tid}/fd", std::process::id());
        assert_eq!(inspect(), Ok(PathBuf::from(expected)));
        finish.send(()).unwrap();
        thread.join().unwrap();
        // The join returns when the thread clears its ID, before Linux reaps
        // it.
        let start = std::time::Instant::now();
        let refused = loop {
            match inspect() {
                Err(reason) => break reason,
                Ok(path) => assert!(
                    start.elapsed() < std::time::Duration::from_secs(10),
                    "{} still names {} after its task exited",
                    link.display(),
                    path.display()
                ),
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert!(
            refused.starts_with("cannot stat ")
                || refused.ends_with("an entry of a task that has exited"),
            "{refused}"
        );
    }

    /// Linux spells the path of an entry of a reaped task with ` (deleted)`
    /// appended, which is no procfs name.
    #[test]
    fn a_path_linux_marks_deleted_names_nothing() {
        let link = Path::new("/proc/self/fd/3");
        let live = PathBuf::from("/proc/12/task/13/fd");
        assert_eq!(refuse_deleted(link, live.clone()), Ok(live));
        let deleted = PathBuf::from("/proc/12/task/13/fd (deleted)");
        assert_eq!(
            refuse_deleted(link, deleted),
            Err(
                "/proc/self/fd/3 leads to /proc/12/task/13/fd (deleted), an entry of a task \
                 that has exited"
                    .to_string()
            )
        );
    }

    /// The filesystem type is read from the object itself, so a procfs file
    /// is told from others wherever it is mounted, and only for the object the
    /// guest saw.
    #[test]
    fn a_procfs_object_is_told_by_its_filesystem_type() {
        let stat = File::open("/proc/self/stat").unwrap();
        let null = File::open("/dev/null").unwrap();
        let link = |file: &File| PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let (stat_metadata, null_metadata) = (stat.metadata().unwrap(), null.metadata().unwrap());
        assert_eq!(
            is_on_procfs(&link(&stat), stat_metadata.dev(), stat_metadata.ino()),
            Ok(true)
        );
        assert_eq!(
            is_on_procfs(&link(&null), null_metadata.dev(), null_metadata.ino()),
            Ok(false)
        );
        let other = is_on_procfs(&link(&null), stat_metadata.dev(), stat_metadata.ino());
        assert!(
            other
                .as_ref()
                .is_err_and(|reason| reason.ends_with("is not the object the guest saw")),
            "{other:?}"
        );
    }

    #[test]
    fn mountinfo_names_procfs_mounts_by_type() {
        let mounts = |minor| procfs_mounts_in(MOUNTINFO, libc::makedev(0, minor), Path::new("/"));
        let mount = |id: u64, root: &str, mount_point: &str| ProcfsMount {
            id,
            root: PathBuf::from(root),
            mount_point: PathBuf::from(mount_point),
        };
        assert_eq!(
            mounts(21),
            vec![mount(22, "/", "/proc"), mount(27, "/sys", "/srv/my sys")]
        );
        assert_eq!(
            mounts(25),
            vec![mount(26, "/", "/srv/proc")],
            "a mount with no optional fields"
        );
        for minor in [22, 23, 24, 26] {
            assert!(mounts(minor).is_empty(), "0:{minor}");
        }
        assert!(procfs_mounts_in(MOUNTINFO, libc::makedev(8, 21), Path::new("/")).is_empty());
    }

    /// The table gives mount points relative to the guest's root, so they
    /// are put below the path that root reads as.
    #[test]
    fn mount_points_are_read_in_the_frame_of_the_guests_root() {
        let mounts = procfs_mounts_in(MOUNTINFO, libc::makedev(0, 21), Path::new("/box"));
        let points: Vec<&Path> = mounts
            .iter()
            .map(|mount| mount.mount_point.as_path())
            .collect();
        assert_eq!(
            points,
            [Path::new("/box/proc"), Path::new("/box/srv/my sys")]
        );
    }

    /// A `mount_root` for a tree whose only mount root is that of mount `id`
    /// at `point`.
    fn only_root(point: &'static str, id: u64) -> impl Fn(&Path) -> io::Result<Option<u64>> + Copy {
        move |path| Ok((path == Path::new(point)).then_some(id))
    }

    /// [`path_within`] in a tree that does not change, whose mount table is
    /// `mounts`, read again too, and where each mount is found at the mount
    /// point the table lists.
    fn named_in(mounts: &Arc<[ProcfsMount]>, mount_id: u64, canonical: &str) -> Option<PathBuf> {
        path_within(
            mounts,
            mount_id,
            Path::new(canonical),
            || Ok(Some(mounts.clone())),
            |path| {
                let mount = mounts.iter().find(|mount| mount.mount_point == path);
                Ok(mount.map(|mount| mount.id))
            },
        )
        .ok()
    }

    #[test]
    fn a_path_within_procfs_does_not_depend_on_the_mount() {
        let mounts: Arc<[ProcfsMount]> =
            procfs_mounts_in(MOUNTINFO, libc::makedev(0, 21), Path::new("/")).into();
        let within = |id: u64, path: &str| named_in(&mounts, id, path);
        let named = |path: &str| Some(PathBuf::from(path));
        assert_eq!(within(22, "/proc"), named("/"));
        assert_eq!(within(22, "/proc/3/fd/0"), named("/3/fd/0"));
        assert_eq!(within(27, "/srv/my sys/kernel"), named("/sys/kernel"));
        assert_eq!(within(22, "/proc/sys/kernel"), named("/sys/kernel"));
        // A path that the mount it was reached through does not hold, or a
        // mount the table does not list for the device, names nothing.
        assert_eq!(within(22, "/procfs/3"), None);
        assert_eq!(within(22, "/elsewhere/3"), None);
        assert_eq!(within(27, "/proc/3"), None);
        assert_eq!(within(26, "/srv/proc/3"), None);
    }

    /// A table read before the guest changed its root lists mount points in
    /// the old frame. The path a link reads as in the new frame can lie under
    /// another mount there, here one that shows all of procfs where the guest
    /// reached a bind mount of `/proc/sys`. Matching the mount the guest
    /// reached the file through names nothing with that table, rather than
    /// another entry, and the table read again in the new frame names the
    /// entry.
    #[test]
    fn a_stale_table_names_nothing_rather_than_another_entry() {
        let device = libc::makedev(0, 21);
        let before: Arc<[ProcfsMount]> = procfs_mounts_in(
            b"\
22 1 0:21 / /proc rw - proc proc rw
30 1 0:21 /sys /box/proc rw - proc proc rw
",
            device,
            Path::new("/"),
        )
        .into();
        let sysctl = Some(PathBuf::from("/sys/kernel"));
        assert_eq!(named_in(&before, 30, "/box/proc/kernel"), sysctl);
        // After chroot("/box"), a guest that reads its own links, as under
        // SaBRe, reads this one as /proc/kernel, and finds mount 30 at /proc.
        let after = b"\
30 1 0:21 /sys /proc rw - proc proc rw
";
        let own: Arc<[ProcfsMount]> = procfs_mounts_in(after, device, Path::new("/")).into();
        let in_box = only_root("/proc", 30);
        let kernel = Path::new("/proc/kernel");
        assert_eq!(
            path_within(&before, 30, kernel, || Ok(Some(before.clone())), in_box),
            Err("/proc/kernel was reached through mount 30, which the guest's mount table lists at /box/proc, not above it".into())
        );
        assert_eq!(
            path_within(&before, 30, kernel, || Ok(Some(own.clone())), in_box).ok(),
            sysctl
        );
        // The tracer, whose root is still /, reads the guest's root as /box.
        let traced: Arc<[ProcfsMount]> = procfs_mounts_in(after, device, Path::new("/box")).into();
        assert_eq!(named_in(&traced, 30, "/box/proc/kernel"), sysctl);
    }

    /// A rename of a directory above procfs's mount point moves the mount,
    /// and changes nothing a table is remembered by. Here the guest renames
    /// `/a` to `/b`, makes a new `/a/proc`, and renames `/b` to
    /// `/a/proc/moved`, so that the mount point the table remembered from
    /// before lists is still a prefix of the path the entry reads as. The
    /// reader does not find the mount there, so the table is read again,
    /// which names the entry as before the renames, rather than
    /// `/moved/proc/1/stat`.
    #[test]
    fn a_mount_moved_by_a_rename_is_found_where_it_moved() {
        let device = libc::makedev(0, 21);
        let table = |mount_point: &str| -> Arc<[ProcfsMount]> {
            let line = format!("30 1 0:21 / {mount_point} rw - proc proc rw\n");
            procfs_mounts_in(line.as_bytes(), device, Path::new("/")).into()
        };
        let remembered = table("/a/proc");
        let moved = table("/a/proc/moved/proc");
        let found = only_root("/a/proc/moved/proc", 30);
        let stat = Path::new("/a/proc/moved/proc/1/stat");
        let named = Ok(PathBuf::from("/1/stat"));
        assert_eq!(
            path_within(&remembered, 30, stat, || Ok(Some(moved.clone())), found),
            named
        );
        assert_eq!(
            path_within(&moved, 30, stat, || panic!("read again"), found),
            named
        );
        // A table that still lists the mount where the reader finds no mount,
        // or another one, names nothing.
        let reached = "/a/proc/moved/proc/1/stat was reached through mount 30, and /a/proc";
        assert_eq!(
            path_within(
                &remembered,
                30,
                stat,
                || Ok(Some(remembered.clone())),
                found
            ),
            Err(format!(
                "{reached}, where the guest's mount table lists it, is the root of no mount"
            ))
        );
        let covered = only_root("/a/proc", 31);
        assert_eq!(
            path_within(
                &remembered,
                30,
                stat,
                || Ok(Some(remembered.clone())),
                covered
            ),
            Err(format!(
                "{reached}, where the guest's mount table lists it, is the root of mount 31"
            ))
        );
        let unknown = |_: &Path| Err(io::Error::other("no statx"));
        assert_eq!(
            path_within(
                &remembered,
                30,
                stat,
                || Ok(Some(remembered.clone())),
                unknown
            ),
            Err(
                "/a/proc/moved/proc/1/stat was reached through mount 30, and cannot tell whether /a/proc, where the guest's mount table lists it, holds it: no statx"
                    .to_string()
            )
        );
        // Nor does a table that cannot be read again, or lists no procfs
        // mount of the device.
        assert_eq!(
            path_within(&remembered, 30, stat, || Ok(None), found),
            Err("/a/proc/moved/proc/1/stat was reached through mount 30, and the guest's mount table, read again, lists no procfs mount of its device".into())
        );
        assert_eq!(
            path_within(&remembered, 30, stat, || Err(io::Error::other("gone")), found),
            Err("/a/proc/moved/proc/1/stat was reached through mount 30, and the guest's mount table cannot be read again: gone".into())
        );
    }

    /// The table gives mount points relative to the guest's root, which reads
    /// from the root of the mount namespace when the reader cannot reach it,
    /// while a link to a file the reader can reach reads from the reader's
    /// root. Here the tracer's root is `/sys`, the guest's is `/`, and
    /// procfs is mounted at `/sys/sys`. The tracer reads procfs's
    /// `/sys/kernel/hostname` as `/sys/sys/kernel/hostname`, and the mount
    /// point as `/sys`, while the table lists it at `/sys/sys`, a directory
    /// of procfs there. The entry goes unnamed, rather than named
    /// `/kernel/hostname`.
    #[test]
    fn a_mount_point_read_in_another_frame_names_nothing() {
        let table: Arc<[ProcfsMount]> = procfs_mounts_in(
            b"22 1 0:21 / /sys/sys rw - proc proc rw\n",
            libc::makedev(0, 21),
            Path::new("/"),
        )
        .into();
        let tracer = only_root("/sys", 22);
        assert_eq!(
            path_within(
                &table,
                22,
                Path::new("/sys/sys/kernel/hostname"),
                || Ok(Some(table.clone())),
                tracer
            ),
            Err("/sys/sys/kernel/hostname was reached through mount 22, and /sys/sys, where the guest's mount table lists it, is the root of no mount".into())
        );
    }

    /// A descriptor is the open's, whatever its number, `openat`'s own
    /// system call number included; a restart, or `EINTR`, is an open that
    /// opened nothing; and any other error refused the open.
    #[test]
    fn an_open_ran_unless_it_reports_a_restart() {
        let own = libc::SYS_openat;
        for fd in [0, 3, own - 1, own, own + 1] {
            assert_eq!(read_open(Ok(fd)), Injected::Ran(fd as RawFd));
        }
        assert_eq!(
            read_open(Ok(1 << 40)),
            Injected::Failed(format!(
                "openat returned {}, which is no descriptor",
                1i64 << 40
            ))
        );
        for errno in [
            Errno::EINTR,
            Errno::ERESTARTSYS,
            Errno::ERESTARTNOINTR,
            Errno::ERESTARTNOHAND,
            Errno::ERESTART_RESTARTBLOCK,
        ] {
            assert_eq!(read_open(Err(errno)), Injected::NotRun(errno.to_string()));
        }
        assert_eq!(
            read_open(Err(Errno::EPERM)),
            Injected::Failed(format!(
                "cannot open the guest's path again: {}",
                Errno::EPERM
            ))
        );
    }

    /// A close that runs returns 0, and a restart reports one that did not
    /// run. Any other number, `close`'s own system call number included, is
    /// one close never returns.
    #[test]
    fn only_a_close_returning_0_ran() {
        assert_eq!(read_close(Ok(0)), Injected::Ran(()));
        assert_eq!(
            read_close(Ok(libc::SYS_close)),
            Injected::Failed(format!(
                "close of the guest's O_PATH descriptor returned {}, which close never returns",
                libc::SYS_close
            ))
        );
        assert_eq!(
            read_close(Ok(5)),
            Injected::Failed(
                "close of the guest's O_PATH descriptor returned 5, which close never returns"
                    .into()
            )
        );
        assert_eq!(
            read_close(Err(Errno::ERESTARTSYS)),
            Injected::NotRun(Errno::ERESTARTSYS.to_string())
        );
        for (errno, name) in [(Errno::EPERM, "EPERM"), (Errno::EINTR, "EINTR")] {
            assert_eq!(
                read_close(Err(errno)),
                Injected::Failed(format!(
                    "close of the guest's O_PATH descriptor returned {name}"
                ))
            );
        }
    }

    #[test]
    fn this_process_reads_its_own_procfs_entries() {
        let pid = std::process::id() as i32;
        let proc = std::fs::metadata("/proc").unwrap();
        let mounts = procfs_mounts(pid, proc.dev(), false).unwrap().unwrap();
        assert!(
            procfs_mounts(pid, proc.dev(), false).unwrap().is_some(),
            "once remembered"
        );
        assert!(
            procfs_mounts(pid, proc.dev(), true).unwrap().is_some(),
            "read again"
        );
        assert!(
            procfs_mounts(pid, std::fs::metadata("/").unwrap().dev(), false)
                .unwrap()
                .is_none()
        );
        assert!(
            procfs_mounts(-1, proc.dev(), false).is_err(),
            "no such thread"
        );

        let stat = std::fs::metadata(format!("/proc/{pid}/stat")).unwrap();
        let file = File::open("/proc/self/stat").unwrap();
        let link = PathBuf::from(format!("/proc/{pid}/fd/{}", file.as_raw_fd()));
        let (canonical, mount_id) = inspect_link(&link, stat.dev(), stat.ino()).unwrap();
        assert_eq!(
            path_within(
                &mounts,
                mount_id,
                &canonical,
                || panic!("read again"),
                mount_root
            ),
            Ok(PathBuf::from(format!("/{pid}/stat")))
        );
        let mount = mounts.iter().find(|mount| mount.id == mount_id).unwrap();
        assert_eq!(mount_root(&mount.mount_point).unwrap(), Some(mount_id));
        let directory = mount.mount_point.join(pid.to_string());
        assert_eq!(mount_root(&directory).unwrap(), None);
        // A path that holds a link: a final one, and a magic link to this
        // process's root, which leads to the same mount's root along another
        // path. A lookup that follows it finds that root there.
        let refused = |path: &Path| mount_root(path).map_err(|error| error.raw_os_error());
        assert_eq!(
            refused(&mount.mount_point.join("self")),
            Err(Some(libc::ELOOP))
        );
        let through_root = mount
            .mount_point
            .join(format!("{pid}/root"))
            .join(mount.mount_point.strip_prefix("/").unwrap());
        let followed = identify(&through_root, false).unwrap();
        assert_eq!(
            (followed.mount_root, followed.mount_id),
            (Some(true), Some(mount_id))
        );
        assert_eq!(refused(&through_root), Err(Some(libc::ELOOP)));
        assert!(inspect_link(&link, stat.dev(), stat.ino() + 1).is_err());
        assert!(inspect_link(&link, stat.dev() + 1, stat.ino()).is_err());
        let cwd = PathBuf::from(format!("/proc/{pid}/cwd"));
        let here = std::fs::metadata(".").unwrap();
        assert_eq!(
            inspect_link(&cwd, here.dev(), here.ino()).unwrap().0,
            std::env::current_dir().unwrap()
        );
    }
}
