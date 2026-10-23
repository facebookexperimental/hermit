/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Procfs files that Detcore renders without the backend opening them
//! (<https://github.com/rrnewton/hermit/issues/4213>).
//!
//! For a file whose every byte comes from Detcore's own state
//! (`ProcfsFile::rendered_path`), Detcore decides on every backend that it
//! exists, what its metadata is and what it holds. (`/proc/cpuinfo` is one
//! only where Detcore answers the guest's CPUID instruction from its own
//! table, `procfs::RenderedFiles`; elsewhere it describes the host's CPU
//! and the kernel opens it.) The backend opens no
//! procfs file. The guest's descriptor is a placeholder: a memfd named
//! `PLACEHOLDER_NAME_PREFIX` and the file's path, reopened read-only with
//! the guest's own flags, a kernel object every backend has. Reopened that
//! way, the kernel itself refuses writes, write locks and size changes, and
//! reports the guest's status flags and a read-only descriptor link, as it
//! does for the procfs file. Detcore answers the rest as Linux answers for a
//! procfs file only global root may write (0444, or 0644 for the fixed
//! tunables of `procfs::fixed`), to a process that is not global root. A
//! per-process file (`/proc/<pid>/...`, `procfs::ProcfsTask`) belongs instead
//! to its process's owner, the guest's own identity, so the guest owns it and
//! its capabilities decide write access (`FileAccess`); a placeholder it opens
//! for writing is reopened writable, and Detcore answers every write through
//! it. Either way Detcore answers:
//!
//! - opening it, including the flag errors Linux reports;
//! - reads, from the procfs snapshot, as for every procfs file;
//! - `stat` of the path and `fstat` of the descriptor, with Detcore's procfs
//!   device (`procfs_inode::VIRTUAL_PROC_DEVICE`) and an inode that its path
//!   alone names (`procfs_inode::rendered_inode`), the same on every backend
//!   and for every spelling that reaches the file;
//! - the descriptor's link in any process's `/proc/*/fd`, and reopening
//!   through it;
//! - its `fdinfo`, `statfs`, and the calls such a procfs file refuses:
//!   `mmap`, `fsync`, `fchmod`, extended attributes, file `ioctl`s and seals.
//!
//! Not under record and replay, whose Replayer stands its own placeholder in
//! for every open, and not after the process changed its root with `chroot`,
//! where the kernel decides whether the file exists. `epoll` refuses the
//! placeholder (`EPERM`), which has no poll method, where it accepts the
//! procfs file: a known limitation, see
//! <https://github.com/rrnewton/hermit/issues/4263>.
// TODO-HUMAN-REVIEW(#4213): Review Detcore-rendered procfs files, a new
// determinization strategy: Detcore serves a file the kernel never opened.

use std::ffi::OsStr;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use nix::fcntl::OFlag;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::AtFlags;
use reverie::syscalls::Errno;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ProtFlags;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::StatxMask;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie::syscalls::Timespec;
use tracing::error;
use tracing::info;
use tracing::warn;

use super::StatTarget;
use crate::fd::FdType;
use crate::procfs::FixedProcfsFile;
use crate::procfs::ProcfsFile;
use crate::procfs::RenderedFiles;
use crate::procfs::placeholder_link_path;
use crate::procfs::placeholder_name;
use crate::procfs_inode;
use crate::record_or_replay::RecordOrReplay;
use crate::stat::DetStat;
use crate::stat::StatxAttributes;
use crate::syscalls::namespace::proc_fd_target;
use crate::syscalls::namespace::proc_fd_target_with_leading_zeros;
use crate::tool_local::Detcore;
use crate::util::raw_result;
use crate::util::raw_syscall;

/// The largest extended attribute value Linux accepts (`XATTR_SIZE_MAX`).
const XATTR_SIZE_MAX: usize = 65536;

/// Linux's longest extended-attribute name.
const XATTR_NAME_MAX: usize = 255;

/// The rendered path a path within a procfs mount names, if it names one the
/// session renders (`files`). Detcore names procfs files by their path within
/// the mount (`ProcfsEntry::At`), as `/sys/kernel/random/uuid`.
pub(crate) fn rendered_path_within(within: &Path, files: RenderedFiles) -> Option<&'static Path> {
    let relative = within.strip_prefix("/").unwrap_or(within);
    ProcfsFile::from_path(&Path::new("/proc").join(relative))?.rendered_path(files)
}

/// The seals a placeholder carries where the backend supports seals: nothing
/// can change the empty file, or its seals. (`MFD_NOEXEC_SEAL`, where the
/// kernel knows it, adds `F_SEAL_EXEC`.)
pub(crate) const PLACEHOLDER_SEALS: libc::c_int =
    libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;

/// The bytes of the buffer a placeholder's memfd name, with its NUL, is built
/// in: the most a memfd name holds (`NAME_MAX` less `memfd:`), and its NUL.
const PLACEHOLDER_NAME_CAPACITY: usize = 250;

/// How many low bits of a carried uptime the nanoseconds of a placeholder's
/// modification time hold ([`carried_uptime_mtime`]): fewer than 10^9 values,
/// as `utimensat` requires.
const CARRIED_UPTIME_NSEC_BITS: u32 = 29;

/// The modification time that makes a placeholder carry `uptime_seconds`, the
/// uptime of the first read of a file whose snapshot that read takes
/// (`ProcfsFile::snapshot_at_first_read`): a time before 1970, the uptime's
/// high bits counting down from -1 seconds and its low bits in the
/// nanoseconds. Every `u64` has one. A memfd's own modification time is the
/// host's time at its creation, which Linux never lets be before 1970
/// (`settimeofday` refuses a negative time), so no placeholder carries an
/// uptime it was not given.
fn carried_uptime_mtime(uptime_seconds: u64) -> Timespec {
    Timespec {
        tv_sec: -1 - (uptime_seconds >> CARRIED_UPTIME_NSEC_BITS) as i64,
        tv_nsec: (uptime_seconds & ((1 << CARRIED_UPTIME_NSEC_BITS) - 1)) as i64,
    }
}

/// Whether nothing can change what a descriptor number names between two of
/// Detcore's injected calls: threads are sequentialized, so Detcore holds the
/// scheduler turn across both, and no guest code runs inside a Tool handler
/// (`BackendCapabilities::runs_guest_code_between_injections`: SaBRe's and
/// in-guest LiteInst's accepted native signal handlers do).
fn descriptor_stable_across_injections(sequentialized: bool, guest_code_between: bool) -> bool {
    sequentialized && !guest_code_between
}

/// Whether a first read may read and write the uptime carried on the
/// placeholder behind its descriptor (`Detcore::first_read_uptime`): only
/// where the descriptor number keeps naming the same file between Detcore's
/// look and its write ([`descriptor_stable_across_injections`]), and only if
/// the descriptor is, by its kernel identity, the placeholder of `path` this
/// tool process registered (`placeholder_identity`), so that neither another
/// file's time is read as a carried uptime nor the carry written to another
/// file.
fn may_carry_uptime(stable: bool, identity: Option<&Path>, path: &Path) -> bool {
    stable && identity == Some(path)
}

/// The uptime a placeholder whose modification time is `seconds` and
/// `nanoseconds` carries ([`carried_uptime_mtime`]), if it carries one.
fn carried_uptime(seconds: i64, nanoseconds: i64) -> Option<u64> {
    if seconds >= 0 || !(0..1 << CARRIED_UPTIME_NSEC_BITS).contains(&nanoseconds) {
        return None;
    }
    let high = (-1 - seconds) as u64;
    (high < 1 << (64 - CARRIED_UPTIME_NSEC_BITS))
        .then_some((high << CARRIED_UPTIME_NSEC_BITS) | nanoseconds as u64)
}

/// The placeholders this tool process made, by their memfd's kernel identity
/// `(dev, ino)`, and the rendered file each stands in for. A kernel object
/// with one of these identities is Detcore's placeholder however the guest
/// reached it, which a memfd's name alone cannot show. Under backends whose
/// tool runs in each guest process, a process knows the placeholders it made
/// or inherited at `fork`, and those it rediscovers after `exec`.
static PLACEHOLDERS: std::sync::Mutex<std::collections::BTreeMap<(u64, u64), &'static Path>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Record that the memfd `(dev, ino)` is a placeholder for `path`.
pub(crate) fn register_placeholder(dev: u64, ino: u64, path: &'static Path) {
    PLACEHOLDERS
        .lock()
        .expect("placeholder registry poisoned")
        .insert((dev, ino), path);
}

/// The open file description each placeholder this tool process opened
/// belongs to, and the snapshot its name carries, by the placeholder's kernel
/// identity: every rendered open makes its own placeholder, so one identity
/// is one open file description (`Detcore::adopt_received_placeholders`).
/// One small entry per rendered open of the run, as `PLACEHOLDERS` keeps one;
/// the handle keeps no description alive.
static PLACEHOLDER_OPENS: std::sync::Mutex<
    std::collections::BTreeMap<(u64, u64), PlaceholderOpen>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// The open file description a placeholder belongs to, and the snapshot its
/// name carries (`PLACEHOLDER_OPENS`).
type PlaceholderOpen = (crate::fd::OpenFileHandle, Vec<u8>);

/// Record that the placeholder `identity` belongs to the open file
/// description `handle`, its name carrying `snapshot`.
pub(crate) fn register_placeholder_open(
    identity: (u64, u64),
    handle: crate::fd::OpenFileHandle,
    snapshot: Vec<u8>,
) {
    PLACEHOLDER_OPENS
        .lock()
        .expect("placeholder open registry poisoned")
        .insert(identity, (handle, snapshot));
}

/// The open file description the placeholder `identity` belongs to, and the
/// snapshot its name carries, if this tool process opened it.
fn placeholder_open(identity: (u64, u64)) -> Option<PlaceholderOpen> {
    PLACEHOLDER_OPENS
        .lock()
        .expect("placeholder open registry poisoned")
        .get(&identity)
        .cloned()
}

/// Whether this tool process knows any placeholder.
pub(crate) fn placeholders_exist() -> bool {
    !PLACEHOLDERS
        .lock()
        .expect("placeholder registry poisoned")
        .is_empty()
}

/// The rendered file that the kernel object `(dev, ino)` is Detcore's
/// placeholder for, if it is one.
pub(crate) fn placeholder_identity(dev: u64, ino: u64) -> Option<&'static Path> {
    PLACEHOLDERS
        .lock()
        .expect("placeholder registry poisoned")
        .get(&(dev, ino))
        .copied()
}

/// The procfs files Detcore renders from its run state, as opposed to the
/// fixed files of `procfs::fixed`.
const DYNAMIC_PATHS: [&std::ffi::CStr; 4] = [
    c"/proc/sys/kernel/random/uuid",
    c"/proc/sys/kernel/pty/nr",
    c"/proc/uptime",
    c"/proc/stat",
];

/// The paths of every procfs file a session renders (`files`):
/// `/proc/cpuinfo` too where it renders that.
pub(crate) fn rendered_paths(
    files: RenderedFiles,
) -> impl Iterator<Item = &'static std::ffi::CStr> {
    DYNAMIC_PATHS
        .into_iter()
        .chain(files.cpuinfo.then_some(c"/proc/cpuinfo"))
        .chain(
            FixedProcfsFile::ALL
                .into_iter()
                .map(FixedProcfsFile::c_path),
        )
}

/// The last component of a rendered file's path.
fn rendered_name(path: &std::ffi::CStr) -> &[u8] {
    let bytes = path.to_bytes();
    bytes.rsplit(|byte| *byte == b'/').next().unwrap_or(bytes)
}

/// The rendered file that the kernel's file with metadata `stat` is, if it
/// is one: the guest reached it through a spelling Detcore did not resolve,
/// as a symbolic link or a directory listing, under a backend whose procfs
/// files Detcore does not name by path
/// (`procfs_inode::guest_tids_name_host_tasks`).
///
/// Each candidate costs a stat, so only a regular file with one link and
/// permissions 0444 or 0644, as every rendered file is in the kernel's
/// procfs, is checked, and an identity found not to be a rendered file is
/// remembered (`NOT_RENDERED`): the checks of one procfs file cost their
/// stats once per tool process. The search covers every file Detcore can
/// render, so that what it remembers holds for any session; the answer is
/// then the file only if this session renders it (`files`).
pub(crate) fn rendered_path_of_raw(stat: &DetStat, files: RenderedFiles) -> Option<&'static Path> {
    let has = |field| stat.mask.contains(field);
    if (has(StatxMask::STATX_TYPE) && stat.mode & libc::S_IFMT != libc::S_IFREG)
        || (has(StatxMask::STATX_NLINK) && stat.nlink != 1)
        || (has(StatxMask::STATX_MODE) && !matches!(stat.mode & 0o7777, 0o444 | 0o644))
    {
        return None;
    }
    let identity = (stat.dev, stat.inode);
    if NOT_RENDERED
        .lock()
        .expect("raw identity cache poisoned")
        .contains(&identity)
    {
        return None;
    }
    let found = rendered_path_among(stat.dev, stat.inode, rendered_paths(RenderedFiles::ALL));
    if found.is_none() && proc_device() == Some(stat.dev) {
        NOT_RENDERED
            .lock()
            .expect("raw identity cache poisoned")
            .insert(identity);
    }
    found
        .and_then(ProcfsFile::from_path)
        .and_then(|file| file.rendered_path(files))
}

/// The kernel procfs identities `(dev, ino)` this tool process found not to be
/// a rendered file. procfs does not reuse an inode number during a run (sysctl
/// and per-process inodes take `get_next_ino`; `/proc/version`'s and
/// `/proc/cmdline`'s are fixed), so an identity that was not a rendered file
/// never becomes one. A rendered file's inode that the kernel drops and makes
/// again gets a new number, which is not here.
static NOT_RENDERED: std::sync::Mutex<std::collections::BTreeSet<(u64, u64)>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// The device of the guest's procfs mount, read once.
pub(super) fn proc_device() -> Option<u64> {
    static PROC_DEVICE: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *PROC_DEVICE.get_or_init(|| GuestProcfs::stat(c"/proc").ok().map(|stat| stat.st_dev))
}

/// The rendered file among `paths` that the kernel's procfs inode
/// `(dev, ino)` is, if it is one.
fn rendered_path_among(
    dev: u64,
    ino: u64,
    mut paths: impl Iterator<Item = &'static std::ffi::CStr>,
) -> Option<&'static Path> {
    if proc_device() != Some(dev) {
        return None;
    }
    paths.find_map(|path| {
        let stat = GuestProcfs::stat(path).ok()?;
        (stat.st_dev == dev && stat.st_ino == ino)
            .then(|| Path::new(OsStr::from_bytes(path.to_bytes())))
            .and_then(|path| ProcfsFile::from_path(path)?.rendered_path(RenderedFiles::ALL))
    })
}

/// Facts about the guest's procfs mount that a rendered file shares with the
/// kernel's procfs files. The tool runs in the guest's mount and user
/// namespaces on every backend outside record and replay, so its `/proc` is
/// the guest's. Read with raw system calls: under in-guest backends the tool
/// shares the guest's C library.
///
/// Each fact is read once, when `/proc` is first reachable, and kept: the
/// container's procfs mount does not change during the run, while the tool's
/// own view of it can disappear, as when an in-guest tool runs in a process
/// that later changed its root. A fact that cannot be read is the errno the
/// guest's call gets, never a failure of the run.
struct GuestProcfs;

impl GuestProcfs {
    fn stat(path: &std::ffi::CStr) -> Result<libc::stat, Errno> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: the path is NUL-terminated and `stat` is writable.
        let result = unsafe {
            raw_syscall(
                libc::SYS_newfstatat,
                [
                    libc::AT_FDCWD as u64,
                    path.as_ptr() as u64,
                    stat.as_mut_ptr() as u64,
                    0,
                    0,
                    0,
                ],
            )
        };
        raw_errno(result)?;
        // SAFETY: the call succeeded and initialized the structure.
        Ok(unsafe { stat.assume_init() })
    }

    /// The owner Linux reports for the guest's procfs sysctl files, and for
    /// the other system-wide procfs files (`/proc/version`, `/proc/cmdline`):
    /// global root as the guest's user namespace sees it.
    fn sysctl_owner() -> Result<(u32, u32), Errno> {
        static OWNER: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
        if let Some(owner) = OWNER.get() {
            return Ok(*owner);
        }
        let stat = Self::stat(c"/proc/sys/kernel")?;
        Ok(*OWNER.get_or_init(|| (stat.st_uid, stat.st_gid)))
    }

    /// The guest's procfs statistics, and its mount flags (`f_flags`, which
    /// the C library's structure keeps private).
    fn statfs() -> Result<(libc::statfs, u64), Errno> {
        // The kernel's `struct statfs` on x86_64: fifteen 64-bit words, of
        // which the eleventh is `f_flags`.
        const WORDS: usize = 15;
        const _: () = assert!(std::mem::size_of::<libc::statfs>() == WORDS * 8);
        static WORDS_READ: std::sync::OnceLock<[u64; WORDS]> = std::sync::OnceLock::new();
        let words = match WORDS_READ.get() {
            Some(words) => *words,
            None => {
                let mut words = [0_u64; WORDS];
                // SAFETY: the path is NUL-terminated and `words` is writable
                // and as large as the kernel's structure.
                let result = unsafe {
                    raw_syscall(
                        libc::SYS_statfs,
                        [
                            c"/proc".as_ptr() as u64,
                            words.as_mut_ptr() as u64,
                            0,
                            0,
                            0,
                            0,
                        ],
                    )
                };
                raw_errno(result)?;
                *WORDS_READ.get_or_init(|| words)
            }
        };
        let flags = words[10];
        // SAFETY: the two have the same size and layout, of plain integers.
        let statfs = unsafe { std::mem::transmute::<[u64; WORDS], libc::statfs>(words) };
        Ok((statfs, flags))
    }

    /// The raw mount ID of the guest's procfs mount, as Linux reports it in
    /// the fdinfo of a procfs file.
    fn mount_id() -> Result<u64, Error> {
        static MOUNT_ID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        if let Some(mount_id) = MOUNT_ID.get() {
            return Ok(*mount_id);
        }
        let mut statx = std::mem::MaybeUninit::<reverie::syscalls::linux_abi::Statx>::zeroed();
        // SAFETY: the path is NUL-terminated and `statx` is writable.
        let result = unsafe {
            raw_syscall(
                libc::SYS_statx,
                [
                    libc::AT_FDCWD as u64,
                    c"/proc".as_ptr() as u64,
                    0,
                    u64::from(reverie::syscalls::linux_abi::STATX_MNT_ID),
                    statx.as_mut_ptr() as u64,
                    0,
                ],
            )
        };
        raw_errno(result)?;
        // SAFETY: the call succeeded and initialized the structure.
        let statx = unsafe { statx.assume_init() };
        if statx.stx_mask & reverie::syscalls::linux_abi::STATX_MNT_ID == 0 {
            // A kernel too old to report it: the guest's read fails with
            // the errno a missing feature gives, not the run.
            return Err(Errno::EOPNOTSUPP.into());
        }
        Ok(*MOUNT_ID.get_or_init(|| statx.stx_mnt_id))
    }

    /// Whether the guest's user namespace maps user ID `id` (`gid` false) or
    /// group ID `id` (`gid` true). -1 never names an ID.
    fn maps_id(gid: bool, id: u32) -> bool {
        static UID_MAP: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        static GID_MAP: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        if id == u32::MAX {
            return false;
        }
        let (cell, path) = if gid {
            (&GID_MAP, c"/proc/self/gid_map")
        } else {
            (&UID_MAP, c"/proc/self/uid_map")
        };
        let map = match cell.get() {
            Some(map) => map,
            None => match Self::read_small(path) {
                Some(map) => cell.get_or_init(|| map),
                None => return false,
            },
        };
        String::from_utf8_lossy(map).lines().any(|line| {
            let fields: Vec<u64> = line
                .split_whitespace()
                .filter_map(|field| field.parse().ok())
                .collect();
            matches!(fields.as_slice(), [inside, _, count]
                if (*inside..inside.saturating_add(*count)).contains(&u64::from(id)))
        })
    }

    /// The first 4 KiB of the small procfs file at `path`, read with raw
    /// system calls.
    fn read_small(path: &std::ffi::CStr) -> Option<Vec<u8>> {
        // SAFETY: the path is NUL-terminated.
        let fd = unsafe {
            raw_syscall(
                libc::SYS_openat,
                [
                    libc::AT_FDCWD as u64,
                    path.as_ptr() as u64,
                    (libc::O_RDONLY | libc::O_CLOEXEC) as u64,
                    0,
                    0,
                    0,
                ],
            )
        };
        let fd = raw_errno(fd).ok()?;
        let mut buffer = vec![0_u8; 4096];
        // SAFETY: `buffer` is writable for its length.
        let length = unsafe {
            raw_syscall(
                libc::SYS_read,
                [fd, buffer.as_mut_ptr() as u64, buffer.len() as u64, 0, 0, 0],
            )
        };
        // SAFETY: closing the descriptor opened above.
        unsafe { raw_syscall(libc::SYS_close, [fd, 0, 0, 0, 0, 0]) };
        buffer.truncate(raw_errno(length).ok()? as usize);
        Some(buffer)
    }

    /// Read every fact while `/proc` is reachable.
    fn remember() {
        let _ = Self::sysctl_owner();
        let _ = Self::statfs();
        let _ = Self::mount_id();
        let _ = Self::maps_id(false, 0);
        let _ = Self::maps_id(true, 0);
    }
}

/// A raw system call's result as an errno.
fn raw_errno(result: i64) -> Result<u64, Errno> {
    raw_result(result)
        .map_err(|error| Errno::new(error.raw_os_error().expect("a raw result is an errno")))
}

/// The raw mount ID of the guest's procfs mount, for a rendered file's
/// `fdinfo`.
pub(super) fn guest_procfs_mount_id() -> Result<u64, Error> {
    GuestProcfs::mount_id()
}

/// The metadata Linux gives the procfs file at `path`, owned by `owner`,
/// with Detcore's procfs device, the inode its path names, and every time at
/// `epoch`, on every backend.
fn rendered_stat(path: &Path, epoch: Timespec, owner: (u32, u32)) -> DetStat {
    let permissions = path
        .to_str()
        .and_then(FixedProcfsFile::from_path)
        .map_or(0o444, FixedProcfsFile::permissions);
    let permissions = if rendered_path_is_owner_only(path) {
        0o400
    } else {
        permissions
    };
    DetStat {
        // What a procfs file's `statx` reports.
        mask: StatxMask::STATX_BASIC_STATS | StatxMask::STATX_MNT_ID,
        mode: libc::S_IFREG | permissions,
        uid: owner.0,
        gid: owner.1,
        attributes: StatxAttributes::empty(),
        // What `vfs_getattr_nosec` and `vfs_statx` report as supported.
        attributes_mask: StatxAttributes::STATX_ATTR_AUTOMOUNT
            | StatxAttributes::STATX_ATTR_MOUNT_ROOT
            | StatxAttributes::STATX_ATTR_DAX,
        dev: procfs_inode::VIRTUAL_PROC_DEVICE,
        rdev: 0,
        inode: procfs_inode::rendered_inode(path),
        nlink: 1,
        size: 0,
        blksize: procfs_inode::PROC_BLOCK_SIZE,
        blocks: 0,
        atime: epoch,
        btime: epoch,
        ctime: epoch,
        mtime: epoch,
    }
}

/// Whether the rendered file `path` is readable by its owner only (0400): a
/// process's `auxv` (`proc_pid_auxv`'s `S_IRUSR`).
pub(super) fn rendered_path_is_owner_only(path: &Path) -> bool {
    crate::procfs::per_process_name(path).is_some_and(|name| name.filename == "auxv")
}

/// What decides another process's open of a rendered `/proc/<pid>/auxv`
/// ([`auxv_access_answer`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AuxvAccess {
    /// The open's answer, as the kernel beneath gave it.
    Answer(Result<(), Errno>),
    /// The kernel beneath serves no `/proc` here: `CAP_SYS_PTRACE` in the
    /// caller's own effective set decides.
    CallerCapability,
}

/// What decides another process's open of a rendered `/proc/<pid>/auxv`
/// ([`Detcore::ptrace_read_allowed`]), from the caller's own opens of the
/// kernel's files: `target`, its open of the kernel's `/proc/<pid>/auxv`, and
/// `serves`, its open of its own `/proc/thread-self/stat` (mode 0444, which
/// no credential or dumpability refuses on a real kernel).
/// - The target's opens: allowed.
/// - It is refused while `serves` opens: the kernel beneath serves this
///   caller's `/proc` and applied Linux's whole rule; its errno is the answer.
/// - `serves` is refused with EACCES, the provider's backstop for every
///   `/proc` file (KVM): the caller's `CAP_SYS_PTRACE` decides.
/// - Any other errno of `serves` (EMFILE, ENFILE, ENOMEM, ...): the open's
///   answer, as Linux's own open would fail with it.
pub(super) fn auxv_access_answer(
    target: Result<(), Errno>,
    serves: Result<(), Errno>,
) -> AuxvAccess {
    match (target, serves) {
        (Ok(()), _) => AuxvAccess::Answer(Ok(())),
        (Err(refused), Ok(())) => AuxvAccess::Answer(Err(refused)),
        (Err(_), Err(Errno::EACCES)) => AuxvAccess::CallerCapability,
        (Err(_), Err(errno)) => AuxvAccess::Answer(Err(errno)),
    }
}

/// The owner Linux reports for the rendered file `path`: a per-process file
/// belongs to its process's owner (`task_dump_owner`), which for every guest
/// process is Detcore's virtual root, 0 (the credential calls are emulated to
/// it); a system-wide file to global root as the guest's user namespace sees
/// it (`GuestProcfs::sysctl_owner`).
fn rendered_owner(path: &Path) -> Result<(u32, u32), Errno> {
    if crate::procfs::rendered_path_is_per_process(path) {
        Ok((0, 0))
    } else {
        GuestProcfs::sysctl_owner()
    }
}

/// What decides Linux's answer to a call that asks for write access to a
/// rendered file, or for its owner: whether the caller owns it, and the
/// capabilities that override its permissions.
///
/// A system-wide file belongs to global root, whom the guest's user
/// namespace does not map, so no capability of the guest overrides its
/// permissions and the guest is not its owner ([`FileAccess::GLOBAL_ROOT`]).
/// A per-process file belongs to its process's owner, Detcore's virtual root,
/// which is every guest's own identity, so the guest owns it, and
/// `CAP_DAC_OVERRIDE` overrides its 0444 mode; `CAP_SYS_ADMIN` in procfs's
/// user namespace decides a `security.` attribute. Both come from the
/// guest's own capability sets, as `capget` reports them
/// (`Detcore::rendered_file_access`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FileAccess {
    /// The caller owns the file.
    owner: bool,
    /// The caller may read the file: every rendered file's mode lets anyone
    /// read it (0444) but a process's `auxv`'s (0400), which only its owner
    /// may ([`rendered_path_is_owner_only`]).
    readable: bool,
    /// `CAP_DAC_OVERRIDE`: write access despite the mode.
    dac_override: bool,
    /// `CAP_SYS_ADMIN`: a `security.` attribute may be changed.
    sys_admin: bool,
}

impl FileAccess {
    /// A system-wide rendered file, owned by global root.
    pub(super) const GLOBAL_ROOT: Self = Self {
        owner: false,
        readable: true,
        dac_override: false,
        sys_admin: false,
    };

    /// A per-process file, which the caller owns, with the capabilities
    /// `capabilities` (a `capget` set) of the caller.
    fn owned_with(capabilities: u64) -> Self {
        Self {
            owner: true,
            readable: true,
            dac_override: capabilities & (1 << CAP_DAC_OVERRIDE) != 0,
            sys_admin: capabilities & (1 << CAP_SYS_ADMIN) != 0,
        }
    }

    /// Whether the caller may write the file: every rendered file's mode
    /// grants no one write access (0444; a fixed tunable's 0644 grants only
    /// global root), so only `CAP_DAC_OVERRIDE` over an owner the caller's
    /// namespace maps does.
    fn may_write(&self) -> bool {
        self.dac_override
    }
}

/// `CAP_DAC_OVERRIDE`'s bit in a capability set.
const CAP_DAC_OVERRIDE: u32 = 1;

/// `CAP_SYS_ADMIN`'s bit in a capability set.
const CAP_SYS_ADMIN: u32 = 21;

/// `CAP_SYS_PTRACE`'s bit in a capability set.
const CAP_SYS_PTRACE: u32 = 19;

/// `_LINUX_CAPABILITY_VERSION_3`, whose `capget` reports two 32-bit words of
/// each set.
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// Which of the caller's capability sets a check uses: the effective set, or,
/// for `access` and `faccessat` without `AT_EACCESS`, the set Linux checks
/// with the real IDs (`access_override_creds`): for a real user ID of 0 in
/// the caller's namespace, which every guest's is, the permitted set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CapabilitySet {
    Effective,
    Permitted,
}

/// The configured epoch as a stat time.
pub(super) fn epoch_time(epoch: chrono::DateTime<chrono::Utc>) -> Timespec {
    Timespec {
        tv_sec: epoch.timestamp(),
        tv_nsec: i64::from(epoch.timestamp_subsec_nanos()),
    }
}

/// Whether the final component of `path` is followed by a slash, which only a
/// directory accepts.
fn names_a_directory(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    bytes.ends_with(b"/") || bytes.ends_with(b"/.")
}

/// Linux's answer to opening an existing rendered procfs file (writable by
/// no one but global root, `access`) with `flags`, from a process that is not
/// global root, before it opens anything: `build_open_flags`, then the
/// lookup's `O_EXCL` and type checks, then `may_open` and `do_dentry_open`.
/// On success, the flags the placeholder is reopened with: read-only, or with
/// the guest's access mode where it may write (a per-process file under
/// `CAP_DAC_OVERRIDE`, whose writes Detcore then answers,
/// `answer_rendered_procfs_operation`), never with `O_TRUNC`, which truncates
/// nothing in procfs.
fn rendered_open_flags(flags: OFlag, access: FileAccess) -> Result<OFlag, Errno> {
    if let Some(errno) = open_flags_refused(flags) {
        return Err(errno);
    }
    // `O_PATH` keeps only its own flags (`O_TMPFILE` includes `O_DIRECTORY`).
    if flags.contains(OFlag::O_PATH) {
        if flags.contains(OFlag::O_DIRECTORY) {
            return Err(Errno::ENOTDIR);
        }
        return Ok(OFlag::O_PATH | (flags & OFlag::O_CLOEXEC));
    }
    // A temporary file is made in a directory, which a file is not.
    if flags.contains(OFlag::O_TMPFILE) {
        return Err(Errno::ENOTDIR);
    }
    if flags.contains(OFlag::O_CREAT | OFlag::O_EXCL) {
        return Err(Errno::EEXIST);
    }
    if flags.contains(OFlag::O_DIRECTORY) {
        return Err(Errno::ENOTDIR);
    }
    // No mode bit allows writing; `O_TRUNC` asks for write access. Reading
    // an owner-only file (`auxv`) needs its owner (`FileAccess::readable`).
    let accmode = flags & OFlag::O_ACCMODE;
    let reads =
        accmode == OFlag::O_RDONLY || accmode == OFlag::O_RDWR || accmode == OFlag::O_ACCMODE;
    let writes = accmode != OFlag::O_RDONLY || flags.contains(OFlag::O_TRUNC);
    if (writes && !access.may_write()) || (reads && !access.readable) {
        return Err(Errno::EACCES);
    }
    // `O_NOATIME` needs the file's owner.
    if flags.contains(OFlag::O_NOATIME) && !access.owner {
        return Err(Errno::EPERM);
    }
    // Procfs files cannot do direct I/O.
    if flags.contains(OFlag::O_DIRECT) {
        return Err(Errno::EINVAL);
    }
    let kept = OFlag::O_APPEND
        | OFlag::O_ASYNC
        | OFlag::O_CLOEXEC
        | OFlag::O_DSYNC
        | OFlag::O_LARGEFILE
        | OFlag::O_NOATIME
        | OFlag::O_NONBLOCK
        | OFlag::O_SYNC;
    let mode = flags & OFlag::O_ACCMODE;
    Ok(mode | (flags & kept))
}

/// What `build_open_flags` refuses with EINVAL before any path is looked up,
/// once `build_open_how` has kept only `O_PATH`'s own flags: a malformed
/// `__O_TMPFILE` (it needs `O_DIRECTORY` and no `O_CREAT`), a temporary file
/// without write access, and `O_CREAT` with `O_DIRECTORY`.
pub(super) fn open_flags_refused(flags: OFlag) -> Option<Errno> {
    if flags.contains(OFlag::O_PATH) {
        return None;
    }
    let tmpfile = OFlag::O_TMPFILE & !OFlag::O_DIRECTORY;
    if flags.intersects(tmpfile)
        && (flags & (OFlag::O_TMPFILE | OFlag::O_CREAT) != OFlag::O_TMPFILE
            || flags & OFlag::O_ACCMODE == OFlag::O_RDONLY)
    {
        return Some(Errno::EINVAL);
    }
    flags
        .contains(OFlag::O_CREAT | OFlag::O_DIRECTORY)
        .then_some(Errno::EINVAL)
}

/// The checks `do_mmap` makes of a mapping of a procfs file before it looks
/// for a mapping the range overlaps (`MAP_FIXED_NOREPLACE`'s `EEXIST`), in
/// its order: the arguments and the address. Returns the page-aligned length.
fn rendered_mmap_address_checks(
    addr: usize,
    len: usize,
    offset: libc::off_t,
    flags: MapFlags,
) -> Result<usize, Errno> {
    // `ksys_mmap_pgoff` refuses MAP_HUGETLB for a file that is not on
    // hugetlbfs before `do_mmap` looks at anything.
    if flags.contains(MapFlags::MAP_HUGETLB) || len == 0 || offset & 4095 != 0 {
        return Err(Errno::EINVAL);
    }
    // A length that cannot fit the address space fails before the file is
    // looked at (`PAGE_ALIGN` overflowing, then `__get_unmapped_area`).
    // The 47-bit user address space of x86_64 without 5-level paging, which
    // Hermit assumes: on a host with LA57, Linux would take a higher fixed
    // address and answer ENODEV where this answers ENOMEM.
    const TASK_SIZE: usize = 0x7fff_ffff_f000;
    let Some(len) = len.checked_add(4095).map(|length| length & !4095) else {
        return Err(Errno::ENOMEM);
    };
    if len > TASK_SIZE {
        return Err(Errno::ENOMEM);
    }
    // A fixed address is taken as given; then it must leave room for the
    // mapping below TASK_SIZE, and be page aligned (measured: EINVAL before
    // ENODEV).
    if flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE) {
        if addr > TASK_SIZE - len {
            return Err(Errno::ENOMEM);
        }
        if addr & 4095 != 0 {
            return Err(Errno::EINVAL);
        }
        // `security_mmap_addr`: below `vm.mmap_min_addr` (the machine's, which
        // does not depend on the configuration) needs CAP_SYS_RAWIO, which a
        // guest lacks.
        let mmap_min_addr = crate::machine::MachineDescription::default()
            .limits
            .mmap_min_addr;
        if (addr as u64) < mmap_min_addr {
            return Err(Errno::EPERM);
        }
    }
    Ok(len)
}

/// The checks `do_mmap` and `mmap_region` make of a mapping of a procfs file
/// Detcore renders after the overlap check, in their order, and the error the
/// mapping ends with: a sysctl file has no `mmap` method (`ENODEV`); a
/// system-wide `seq_file` has `proc_reg_mmap` (`procfs::rendered_path_is_seq_file`),
/// so a growing-down mapping is refused (`EINVAL`) and otherwise the method
/// finds nothing behind it (`EIO`); a per-process file's operations have no
/// `mmap` (`ENODEV`, `procfs::rendered_path_is_per_process`). `len` is page
/// aligned. `noexec` says whether procfs is mounted `noexec`. `readable` and
/// `writable` are the descriptor's access mode: only a per-process file's can
/// be writable, or not readable.
fn rendered_mmap_file_checks(
    len: usize,
    offset: libc::off_t,
    prot: ProtFlags,
    flags: MapFlags,
    noexec: bool,
    seq_file: bool,
    (readable, writable): (bool, bool),
) -> Errno {
    // `file_mmap_ok`: the mapping must end within the largest offset a
    // regular file can have (`MAX_LFS_FILESIZE`), the offset taken unsigned.
    const MAX_LFS_FILESIZE: u64 = i64::MAX as u64;
    if offset as u64 / 4096 > (MAX_LFS_FILESIZE - len as u64) >> 12 {
        return Errno::EOVERFLOW;
    }
    let map_type = flags.bits() & libc::MAP_TYPE;
    if map_type != libc::MAP_SHARED
        && map_type != libc::MAP_PRIVATE
        && map_type != libc::MAP_SHARED_VALIDATE
    {
        return Errno::EINVAL;
    }
    // MAP_SHARED_VALIDATE refuses every flag outside `LEGACY_MAP_MASK`, as
    // procfs supports none (`mmap_supported_flags`): MAP_SYNC and
    // MAP_FIXED_NOREPLACE among them.
    if map_type == libc::MAP_SHARED_VALIDATE && flags.bits() & !LEGACY_MAP_MASK != 0 {
        return Errno::EOPNOTSUPP;
    }
    if map_type != libc::MAP_PRIVATE && prot.contains(ProtFlags::PROT_WRITE) && !writable {
        return Errno::EACCES;
    }
    // Every mapping needs a readable descriptor (`MAP_SHARED` falls through
    // to `MAP_PRIVATE`'s check).
    if !readable {
        return Errno::EACCES;
    }
    if noexec && prot.contains(ProtFlags::PROT_EXEC) {
        return Errno::EPERM;
    }
    if !seq_file {
        return Errno::ENODEV;
    }
    if flags.contains(MapFlags::MAP_GROWSDOWN) {
        return Errno::EINVAL;
    }
    Errno::EIO
}

/// `include/linux/mman.h`'s `LEGACY_MAP_MASK`: the flags every file accepts
/// under MAP_SHARED_VALIDATE, with the type bits.
const LEGACY_MAP_MASK: libc::c_int = libc::MAP_SHARED
    | libc::MAP_PRIVATE
    | libc::MAP_FIXED
    | libc::MAP_ANONYMOUS
    | libc::MAP_DENYWRITE
    | libc::MAP_EXECUTABLE
    | 0x400_0000 // MAP_UNINITIALIZED
    | libc::MAP_GROWSDOWN
    | libc::MAP_LOCKED
    | libc::MAP_NORESERVE
    | libc::MAP_POPULATE
    | libc::MAP_NONBLOCK
    | libc::MAP_STACK
    | libc::MAP_HUGETLB
    | libc::MAP_32BIT
    | 0x80 // MAP_ABOVE4G
    | (21 << 26) // MAP_HUGE_2MB
    | (30 << 26); // MAP_HUGE_1GB

/// Linux's answer to mapping a procfs file Detcore renders, where the range
/// overlaps no mapping (`MAP_FIXED_NOREPLACE` then has nothing to refuse):
/// [`rendered_mmap_address_checks`], then [`rendered_mmap_file_checks`]. The
/// unit tests' view of `answer_rendered_procfs_mapping`.
#[cfg(test)]
fn rendered_mmap_answer(
    addr: usize,
    len: usize,
    offset: libc::off_t,
    prot: ProtFlags,
    flags: MapFlags,
    noexec: bool,
    seq_file: bool,
) -> Result<i64, Errno> {
    let len = rendered_mmap_address_checks(addr, len, offset, flags)?;
    Err(rendered_mmap_file_checks(
        len,
        offset,
        prot,
        flags,
        noexec,
        seq_file,
        (true, false),
    ))
}

/// Linux's answer to a positioned write of a rendered `seq_file`, which is
/// opened without `FMODE_PWRITE` (`seq_open`): a negative position is
/// `EINVAL`, then `ESPIPE`, before the descriptor's access mode is looked at.
/// `None` for `pwritev2` at position -1, which writes at the file position
/// as `writev` does and is refused by the read-only placeholder as Linux
/// refuses it (`EBADF`).
fn rendered_seq_file_pwrite_answer(sysno: Sysno, position: i64) -> Option<Result<i64, Errno>> {
    if sysno == Sysno::pwritev2 && position == -1 {
        return None;
    }
    Some(Err(if position < 0 {
        Errno::EINVAL
    } else {
        Errno::ESPIPE
    }))
}

/// Linux's answer to setting the extended attribute `name` of a procfs file,
/// which has no attribute handlers, to `value` (`size` bytes, `None` if the
/// guest's buffer cannot be read), or to removing it (`size` is `None`), in
/// the kernel's order: the name, the size, the value, then the namespace.
/// Measured against /proc/sys/kernel/random/uuid as a user its user namespace
/// does not own ([`FileAccess::GLOBAL_ROOT`]), and against /proc/self/cgroup
/// as its owner with and without the capabilities `access` names.
fn xattr_change_answer(
    name: &[u8],
    size: Option<usize>,
    value: Option<&[u8]>,
    access: FileAccess,
) -> Result<i64, Errno> {
    if name.is_empty() || name.len() > 255 {
        return Err(Errno::ERANGE);
    }
    if size.is_some_and(|size| size > XATTR_SIZE_MAX) {
        return Err(Errno::E2BIG);
    }
    if size.is_some_and(|size| size > 0) && value.is_none() {
        return Err(Errno::EFAULT);
    }
    let setting = size.is_some();
    if name.starts_with(b"trusted.") {
        Err(Errno::EPERM)
    } else if name == b"system.posix_acl_access" || name == b"system.posix_acl_default" {
        // A list is parsed first; procfs then has no way to keep it.
        if setting && posix_acl_parser_refuses(value.unwrap_or_default()) {
            Err(Errno::EINVAL)
        } else {
            Err(Errno::EOPNOTSUPP)
        }
    } else if name == b"security.capability" && setting {
        // The capability module parses the value first.
        Err(Errno::EINVAL)
    } else if name.starts_with(b"security.") {
        // Changing a security attribute needs CAP_SYS_ADMIN; procfs then has
        // no way to keep it.
        Err(if access.sys_admin {
            Errno::EOPNOTSUPP
        } else {
            Errno::EPERM
        })
    } else if name.starts_with(b"system.") {
        Err(Errno::EOPNOTSUPP)
    } else {
        // `user.` and unknown namespaces need write permission first.
        Err(if access.may_write() {
            Errno::EOPNOTSUPP
        } else {
            Errno::EACCES
        })
    }
}

/// Whether the kernel's parser (`posix_acl_from_xattr`) refuses `value` as a
/// POSIX access control list: a value too short for its header, a size that
/// is not a whole number of entries, an unknown tag, or a named user or group
/// the caller's user namespace does not map. A bad version, an empty value or
/// an empty list are not refused here; procfs then has no way to keep any
/// list (EOPNOTSUPP). The checks of a list's order, permissions and counts
/// (`posix_acl_valid`) come after that, so a procfs file never reaches them.
/// Measured on Linux 7.1, as a user and as root in a user namespace.
fn posix_acl_parser_refuses(value: &[u8]) -> bool {
    if value.is_empty() {
        return false;
    }
    let Some((header, entries)) = value.split_first_chunk::<4>() else {
        return true;
    };
    if u32::from_le_bytes(*header) != 2 {
        return false;
    }
    if entries.len() % 8 != 0 {
        return true;
    }
    entries.as_chunks::<8>().0.iter().any(|entry| {
        let tag = u16::from_le_bytes([entry[0], entry[1]]);
        let id = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
        match tag {
            // ACL_USER_OBJ, ACL_GROUP_OBJ, ACL_MASK, ACL_OTHER.
            0x01 | 0x04 | 0x10 | 0x20 => false,
            0x02 => !GuestProcfs::maps_id(false, id),
            0x08 => !GuestProcfs::maps_id(true, id),
            _ => true,
        }
    })
}

/// Linux's answer to reading the extended attribute `name` of a procfs file,
/// which has no attribute handlers: a `trusted.` attribute is invisible
/// without the global capability; a `security.` or `system.` one is left to
/// the file system; any other name first needs read permission on the file
/// (`xattr_permission`), which a `readable` of false (a zombie's owner-only
/// file) refuses. Measured natively against a zombie's `/proc/<pid>/auxv`.
fn xattr_get_answer(name: &[u8], readable: bool) -> Result<i64, Errno> {
    if name.is_empty() || name.len() > 255 {
        Err(Errno::ERANGE)
    } else if name.starts_with(b"trusted.") {
        Err(Errno::ENODATA)
    } else if !readable && !name.starts_with(b"security.") && !name.starts_with(b"system.") {
        Err(Errno::EACCES)
    } else {
        Err(Errno::EOPNOTSUPP)
    }
}

/// A NUL-terminated string in a fixed buffer, to push on the guest's stack.
pub(super) fn c_string<const N: usize>(text: &[u8]) -> [u8; N] {
    assert!(text.len() < N, "{} bytes do not fit in {N}", text.len());
    let mut buffer = [0; N];
    buffer[..text.len()].copy_from_slice(text);
    buffer
}

/// Write `link`, truncated to `buffer_len`, as `readlink` writes a link.
fn write_link<M: MemoryAccess>(
    memory: &mut M,
    link: &Path,
    buffer: Option<AddrMut<'_, libc::c_char>>,
    buffer_len: usize,
) -> Result<i64, Error> {
    let link = link.as_os_str().as_bytes();
    let copied = link.len().min(buffer_len);
    let buffer = buffer.ok_or(Errno::EFAULT)?;
    memory.write_exact(buffer.cast::<u8>(), &link[..copied])?;
    Ok(copied as i64)
}

/// A rendered procfs file that a path names, and how.
pub(super) struct RenderedTarget {
    pub(super) procfs: ProcfsFile,
    /// Through a descriptor link, `/proc/*/fd/N` or `/dev/fd/N`.
    pub(super) via_link: bool,
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Whether Detcore renders procfs files for this guest thread now.
    pub(super) fn renders_procfs<G: Guest<Self>>(&self, guest: &G) -> bool {
        !self.cfg.recordreplay_modes
            && !guest
                .thread_state()
                .changed_root
                .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// What Detcore's process table says now of the task the rendered
    /// per-process path `path` names, and whether that is still the task the
    /// path was bound to (it exists and its numbers have the incarnations
    /// they had: no other task has taken them since). `None` for a path that
    /// names no per-process file.
    pub(super) async fn rendered_task_state<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
    ) -> Option<(crate::procfs::listing::ProcfsTaskAnswer, bool)> {
        let name = crate::procfs::per_process_name(path)?;
        let answer = crate::tool_global::procfs_task(guest, name.task).await;
        let current = name
            .incarnation
            .is_some_and(|bound| answer.still_names(bound));
        Some((answer, current))
    }

    /// Linux's answer to taking a new snapshot of the rendered file open as
    /// `fd`, where it is a per-process file: its `show` method finds its task
    /// again (`get_proc_task`), and a task that is gone, or whose pid another
    /// task now holds, is ESRCH. Linux takes a snapshot at a read with no
    /// buffered bytes from the start, and at a read or seek to a position the
    /// buffer does not hold (`traverse`). Only a file whose `show` runs at a
    /// read (`ProcfsFile::shows_at_read`): a process's `auxv` reads the
    /// memory map its open pinned, whatever became of the task since.
    pub(crate) async fn require_rendered_task<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<(), Errno> {
        let Some(path) = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_rendered_path())
            .ok()
            .flatten()
        else {
            return Ok(());
        };
        if !ProcfsFile::from_rendered_path(path).is_some_and(|file| file.shows_at_read()) {
            return Ok(());
        }
        match self.rendered_task_state(guest, path).await {
            Some((_, false)) => Err(Errno::ESRCH),
            _ => Ok(()),
        }
    }

    /// The owner Linux reports for the rendered file `path`
    /// ([`rendered_owner`]), for a per-process file from its task's lifecycle
    /// in Detcore's process table: Detcore's virtual root while the task has
    /// its memory map, and global root once it has exited (a zombie, whose
    /// files Linux gives to global root) or is gone.
    pub(super) async fn rendered_file_owner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
    ) -> Result<(u32, u32), Errno> {
        match self.rendered_task_state(guest, path).await {
            Some((answer, true)) if !answer.without_mm => Ok((0, 0)),
            Some(_) => GuestProcfs::sysctl_owner(),
            None => rendered_owner(path),
        }
    }

    /// The metadata of the rendered file `path` ([`rendered_stat`]), with its
    /// owner ([`Self::rendered_file_owner`]) and, for a per-process file, the
    /// modification time its owner set, which Detcore keeps in its global
    /// tool for every alias of the file (`RenderedFileState`).
    pub(super) async fn rendered_metadata<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
    ) -> Result<DetStat, Error> {
        let owner = self.rendered_file_owner(guest, path).await?;
        let epoch = epoch_time(guest.config().epoch);
        let mut stat = rendered_stat(path, epoch, owner);
        if crate::procfs::per_process_name(path).is_some() {
            let state = crate::tool_global::rendered_file_state(
                guest,
                stat.inode,
                crate::procfs::listing::RenderedFileChange::Read,
            )
            .await;
            if let Some((tv_sec, tv_nsec)) = state.mtime {
                stat.mtime = Timespec { tv_sec, tv_nsec };
            }
        }
        Ok(stat)
    }

    /// The procfs file Detcore renders that `path`, absolute in the guest's
    /// view, names: directly, or as this process's `/proc/*/fd/N` or
    /// `/dev/fd/N` link to a descriptor Detcore rendered. A spelling that
    /// asks for a directory fails as Linux fails it.
    pub(super) async fn rendered_procfs_at<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
    ) -> Result<Option<RenderedTarget>, Error> {
        // A `..` component is the kernel's to resolve: it walks through the
        // components before it, which need not exist or be directories, while
        // Detcore's names drop them. (A valid `..` spelling of a rendered file
        // so reaches the kernel's own: a known limitation, see
        // <https://github.com/rrnewton/hermit/issues/4263>.)
        if !self.renders_procfs(guest)
            || path
                .components()
                .any(|component| component == std::path::Component::ParentDir)
        {
            return Ok(None);
        }
        if let Some(mut procfs) = ProcfsFile::from_path(path) {
            // A per-process file belongs to the task its path names, which
            // Detcore's process table decides exists, as Linux's lookup of
            // `/proc/<pid>` and `task/<tid>` does: a task that is not there
            // has no file (ENOENT), before any other check. In a nested PID
            // namespace, whose numbers are its own, the kernel decides.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#4213): Review per-process rendered files: the
            // task a spelling names, its existence and identity from Detcore's
            // process table, and its owner's access (FileAccess).
            if procfs.per_process_name().is_some() {
                let Some((pid, tid)) = self.procfs_caller_ids(guest) else {
                    return Ok(None);
                };
                procfs.bind_task(pid, tid);
                let Some(task) = procfs.bound_task() else {
                    return Err(Errno::ENOENT.into());
                };
                let answer = crate::tool_global::procfs_task(guest, task).await;
                if !answer.exists {
                    return Err(Errno::ENOENT.into());
                }
                let without_mm = answer.without_mm;
                self.bind_task_identity(guest, &mut procfs, answer).await?;
                if self.kept_by_kernel(guest, &procfs, without_mm).await {
                    return Ok(None);
                }
                // A process's `mountinfo` or `mounts` shows the mounts below
                // its own root: where that process (a child that chrooted)
                // changed its root, the kernel serves its view, and the open
                // pins it (`Detcore::mount_view_is_kernel`). The caller's own
                // root is checked above (`renders_procfs`).
                if procfs.mount_table_rendering().is_some()
                    && crate::tool_global::root_changed(
                        guest,
                        detcore_model::pid::DetPid::from_raw(task.thread.unwrap_or(task.process)),
                    )
                    .await
                {
                    return Ok(None);
                }
            }
            if procfs.rendered_path(self.rendered_files(guest)).is_none() {
                return Ok(None);
            }
            if names_a_directory(path) {
                return Err(Errno::ENOTDIR.into());
            }
            // `/proc/mounts` is a symbolic link to the caller's `self/mounts`:
            // its own metadata, `readlink` and `O_NOFOLLOW` are the link's.
            let via_link = crate::procfs::is_proc_mounts_link(path);
            return Ok(Some(RenderedTarget { procfs, via_link }));
        }
        // procfs names descriptors without leading zeros, so `fd/03` does not
        // exist even where descriptor 3 is a placeholder. Detcore answers so
        // on every backend (KVM's own procfs resolves the spelling).
        if let Some((subject, fd)) = proc_fd_target_with_leading_zeros(path) {
            let own = match subject {
                Some(subject) => self.guest_visible_own_pid(guest).await? == i64::from(subject),
                None => true,
            };
            let rendered = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.procfs_rendered_path().is_some())
                .unwrap_or(false);
            if own && rendered {
                return Err(Errno::ENOENT.into());
            }
            return Ok(None);
        }
        let Some((subject, fd)) = proc_fd_target(path) else {
            return Ok(None);
        };
        if let Some(subject) = subject
            && self.guest_visible_own_pid(guest).await? != i64::from(subject)
        {
            return Ok(None);
        }
        // The descriptor is a placeholder, so the file it reopens is one
        // Detcore opened as rendered: the target keeps that state, whatever
        // the session renders by path.
        let target = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_rendered_path())
            .ok()
            .flatten()
            .and_then(ProcfsFile::from_rendered_path)
            .map(|procfs| RenderedTarget {
                procfs: procfs.into_rendered(),
                via_link: true,
            });
        // The link reaches a file, which a trailing slash or `/.` refuses.
        if target.is_some() && names_a_directory(path) {
            return Err(Errno::ENOTDIR.into());
        }
        Ok(target)
    }

    /// The rendered procfs file that descriptor `fd`, which the kernel just
    /// opened, is a placeholder for: one the guest reached through a link
    /// Detcore does not resolve itself, such as another process's
    /// `/proc/<pid>/fd/N` or `/proc/*/task/*/fd/N`. A placeholder this tool
    /// process made is known by its kernel identity. One it did not (another
    /// process's, under a backend whose tool runs in each guest process) is
    /// known by its memfd's name and seals, which a placeholder carries where
    /// the backend supports seals.
    pub(super) async fn rendered_procfs_behind<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<&'static Path>, Error> {
        if !self.renders_procfs(guest) {
            return Ok(None);
        }
        if let Ok(stat) = self.inject_fstat(guest, fd).await
            && let Some(path) = placeholder_identity(stat.st_dev, stat.st_ino)
        {
            return Ok(Some(path));
        }
        // Where the tool runs in one process for every guest, it made every
        // placeholder, and its identity is the whole answer. Elsewhere a name
        // and seals a guest could imitate decide: a known limitation, see
        // <https://github.com/rrnewton/hermit/issues/4263>.
        if !self.cfg.backend.tool_shares_guest_descriptor_table {
            return Ok(None);
        }
        let named = self.placeholder_named(guest, fd).await?;
        if named.is_none() {
            return Ok(None);
        }
        let seals = guest
            .inject(
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(syscalls::FcntlCmd::F_GET_SEALS),
            )
            .await;
        // `MFD_NOEXEC_SEAL` adds `F_SEAL_EXEC` to the seals Detcore sets. An
        // `O_PATH` descriptor cannot show its seals (EBADF); its name decides.
        let wanted = i64::from(PLACEHOLDER_SEALS);
        Ok(named.filter(|_| match seals {
            Ok(seals) => seals & wanted == wanted,
            Err(errno) => errno == Errno::EBADF,
        }))
    }

    /// The rendered procfs file that descriptor `fd`'s memfd is named for,
    /// read from its link.
    async fn placeholder_named<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<&'static Path>, Error> {
        const TARGET_CAPACITY: usize = crate::procfs::PLACEHOLDER_LINK_CAPACITY;
        let mut stack = guest.stack().await;
        // The calling thread's descriptor, by its own table: `/proc/self/fd`
        // is the thread group leader's, another table once a thread made
        // without CLONE_FILES has its own.
        let link = stack.push(c_string::<32>(
            format!("/proc/thread-self/fd/{fd}").as_bytes(),
        ));
        let target = stack.reserve::<[u8; TARGET_CAPACITY]>();
        let stack_guard = stack.commit()?;
        let length = guest
            .inject(
                syscalls::Readlinkat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(link.as_raw() as *const libc::c_char))
                    .with_buf(Some(target.cast::<libc::c_char>()))
                    .with_buf_len(TARGET_CAPACITY),
            )
            .await;
        let path = match length {
            Ok(length) => {
                let mut bytes = vec![0; (length as usize).min(TARGET_CAPACITY)];
                guest.memory().read_exact(target.cast::<u8>(), &mut bytes)?;
                placeholder_link_path(&bytes, self.rendered_files(guest))
            }
            Err(_) => None,
        };
        drop(stack_guard);
        Ok(path)
    }

    /// The rendered file whose placeholder the guest's path at `path` (guest
    /// address, relative to `dirfd`) reaches through a spelling Detcore does
    /// not resolve itself, such as a symbolic link to a descriptor link or
    /// `/dev/stdin`: by the kernel identity of what it reaches. Only while
    /// this process holds a rendered descriptor, so a process that holds none
    /// pays nothing.
    pub(super) async fn placeholder_reached<G: Guest<Self>>(
        &self,
        guest: &mut G,
        dirfd: RawFd,
        path: Option<PathPtr<'_>>,
        follow: bool,
    ) -> Option<&'static Path> {
        let pointer = path?;
        // No placeholder known to this tool process, which serves every
        // guest process: nothing can reach one. (Checked first: it costs
        // nothing, while the next looks at every descriptor.) Where the tool
        // runs in each guest process, another process's may.
        let in_guest = self.cfg.backend.tool_shares_guest_descriptor_table;
        if (!placeholders_exist() && !in_guest) || !self.renders_procfs(guest) {
            return None;
        }
        // Another process's descriptor link (or `/dev/std*`) may reach a
        // placeholder this process does not hold. Where the tool runs in each
        // guest process, any spelling may reach one this image inherited and
        // has not yet looked at (descriptors are discovered when used).
        if !in_guest && !guest.thread_state().holds_a_rendered_procfs_file() {
            let text: std::path::PathBuf = pointer.read(&guest.memory()).ok()?;
            let names_a_link = super::names_descriptor_link(&text)
                || text.starts_with("/dev/std")
                || text.starts_with("/dev/fd/");
            if !names_a_link {
                return None;
            }
        }
        let path = pointer;
        let flags = if follow {
            AtFlags::empty()
        } else {
            AtFlags::AT_SYMLINK_NOFOLLOW
        };
        let stat = Self::inject_statat(guest, dirfd, path, [Some(path), None], flags)
            .await
            .ok()?;
        let target = StatTarget::Path {
            dirfd,
            path: Some(path),
            follow,
        };
        self.recognize_placeholder(guest, &DetStat::from(stat), target)
            .await
    }

    /// The rendered file whose placeholder the kernel object with metadata
    /// `stat`, reached through `target`, is. By its kernel identity, where this
    /// tool process made or learned the placeholder. Where the tool runs in
    /// each guest process, a placeholder another process made, or an earlier
    /// image of this one (inherited across `fork` or `exec`), is learned here:
    /// an empty regular file with no link, as a placeholder is, is checked by
    /// its memfd's name and seals, and then known by its identity. A path is
    /// opened `O_PATH` for that, which no file notices; an `O_PATH`
    /// descriptor shows no seals, so for a path, or an `O_PATH` descriptor,
    /// the name alone decides. (A guest could imitate the name and seals: a known
    /// limitation, see <https://github.com/rrnewton/hermit/issues/4263>.)
    pub(super) async fn recognize_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        stat: &DetStat,
        target: StatTarget<'_>,
    ) -> Option<&'static Path> {
        if let Some(path) = placeholder_identity(stat.dev, stat.inode) {
            return Some(path);
        }
        let has = |field| stat.mask.contains(field);
        let may_be_placeholder = (!has(StatxMask::STATX_TYPE)
            || stat.mode & libc::S_IFMT == libc::S_IFREG)
            && (!has(StatxMask::STATX_NLINK) || stat.nlink == 0)
            && (!has(StatxMask::STATX_SIZE) || stat.size == 0);
        if !self.cfg.backend.tool_shares_guest_descriptor_table
            || self.cfg.recordreplay_modes
            || !may_be_placeholder
        {
            return None;
        }
        let (fd, opened) = match target {
            StatTarget::Descriptor(fd) => (fd, false),
            StatTarget::Path {
                dirfd,
                path: Some(path),
                follow,
            } => {
                let flags = OFlag::O_PATH
                    | OFlag::O_CLOEXEC
                    | if follow {
                        OFlag::empty()
                    } else {
                        OFlag::O_NOFOLLOW
                    };
                let fd = guest
                    .inject(
                        syscalls::Openat::new()
                            .with_dirfd(dirfd)
                            .with_path(Some(path))
                            .with_flags(flags),
                    )
                    .await
                    .ok()?;
                (fd as RawFd, true)
            }
            _ => return None,
        };
        let path = self.rendered_procfs_behind(guest, fd).await.ok().flatten();
        if opened {
            let _ = guest.inject(syscalls::Close::new().with_fd(fd)).await;
        }
        if let Some(path) = path {
            register_placeholder(stat.dev, stat.inode, path);
        }
        path
    }

    /// The rendered file whose placeholder descriptor `fd` is, though Detcore
    /// did not record it as one (an alias the kernel opened while metadata is
    /// not virtualized). Where one tool process serves every guest, only while
    /// this process holds a rendered descriptor; where the tool runs in each
    /// guest process, for any descriptor (an `fstat` per call, only without
    /// virtualized metadata).
    async fn placeholder_descriptor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Option<&'static Path> {
        let in_guest = self.cfg.backend.tool_shares_guest_descriptor_table;
        if !in_guest
            && (!placeholders_exist() || !guest.thread_state().holds_a_rendered_procfs_file())
        {
            return None;
        }
        let stat = self.inject_fstat(guest, fd).await.ok()?;
        self.recognize_placeholder(guest, &DetStat::from(stat), StatTarget::Descriptor(fd))
            .await
    }

    /// Who owns the rendered file `path` and what the caller may override
    /// ([`FileAccess`]). A per-process file whose task has exited or is gone
    /// belongs to global root. For a live task's file, the caller's capability set
    /// `set` decides the overrides, asked of the kernel (or the provider that
    /// answers `capget`, as it answers the guest's own) only where `needed`,
    /// a call that asks for write access or a `security.` attribute: every
    /// other call's answer depends on ownership alone, so a read pays nothing.
    /// A set that cannot be read overrides nothing.
    pub(super) async fn rendered_file_access<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
        needed: bool,
        set: CapabilitySet,
    ) -> Result<FileAccess, Error> {
        if !crate::procfs::rendered_path_is_per_process(path) {
            return Ok(FileAccess::GLOBAL_ROOT);
        }
        // A task that has exited (a zombie), or is gone, has no memory map,
        // and Linux gives its files to global root (`task_dump_owner`): then
        // the caller owns nothing and no capability of its namespace applies,
        // so it reads an owner-only file (`auxv`, 0400) no more than it
        // writes any (EACCES).
        match self.rendered_task_state(guest, path).await {
            Some((answer, true)) if !answer.without_mm => {}
            _ => {
                return Ok(FileAccess {
                    readable: !rendered_path_is_owner_only(path),
                    ..FileAccess::GLOBAL_ROOT
                });
            }
        }
        // A set that cannot be read overrides nothing; scratch that cannot be
        // put back is the failure policy's (`inject_capget`).
        let capabilities = if needed {
            Self::inject_capget(guest, set).await?.unwrap_or(0)
        } else {
            0
        };
        Ok(FileAccess::owned_with(capabilities))
    }

    /// The caller's capability set `set` (its low 64 bits), as `capget`
    /// reports it for the calling thread. `Ok(None)` where it cannot be read
    /// with nothing changed: no stack scratch to borrow, or `capget` refused.
    /// `Err` where the borrowed scratch could not be put back; the failure
    /// policy has then been applied (`scratch_failure`), so the caller
    /// returns it as it is, never as an ordinary answer.
    pub(super) async fn inject_capget<G: Guest<Self>>(
        guest: &mut G,
        set: CapabilitySet,
    ) -> Result<Option<u64>, Error> {
        // The header (8 bytes) and the data (two 12-byte words) in borrowed
        // stack scratch, whose original bytes go back on every path.
        let (scratch, original, stack_guard) = match Self::borrow_scratch::<G, 32>(guest).await {
            Ok(Some(borrowed)) => borrowed,
            Ok(None) => return Ok(None),
            Err(reason) => {
                return Err(
                    Self::scratch_failure(guest, reverie::syscalls::Sysno::capget, reason).await,
                );
            }
        };
        // Moved into the block by value: a borrowed address would make the
        // future not `Send`.
        let caller = &mut *guest;
        let words = async move {
            let guest = caller;
            let header = scratch;
            let data = AddrMut::<u8>::from_raw(scratch.as_raw() + 8)?;
            let mut bytes = [0_u8; 32];
            bytes[..4].copy_from_slice(&LINUX_CAPABILITY_VERSION_3.to_ne_bytes());
            guest.memory().write_exact(header, &bytes).ok()?;
            guest
                .inject(
                    syscalls::Capget::new()
                        .with_header(AddrMut::from_raw(header.as_raw()))
                        .with_data(AddrMut::from_raw(data.as_raw())),
                )
                .await
                .ok()?;
            let mut read = [0_u8; 24];
            guest.memory().read_exact(data, &mut read).ok()?;
            Some(read)
        }
        .await;
        let restored = guest.memory().write_exact(scratch, &original);
        drop(stack_guard);
        if let Err(errno) = restored {
            let reason = format!(
                "the guest stack scratch under an injected capget could not be restored: {errno}"
            );
            return Err(
                Self::scratch_failure(guest, reverie::syscalls::Sysno::capget, reason).await,
            );
        }
        // Each 32-bit word holds `effective`, `permitted`, `inheritable`.
        let Some(words) = words else {
            return Ok(None);
        };
        let word =
            |index: usize| u32::from_ne_bytes(words[index * 4..index * 4 + 4].try_into().unwrap());
        let index = match set {
            CapabilitySet::Effective => 0,
            CapabilitySet::Permitted => 1,
        };
        Ok(Some(
            u64::from(word(index)) | (u64::from(word(index + 3)) << 32),
        ))
    }

    /// `N` bytes of the guest's stack scratch (below the red zone), borrowed
    /// for an injected call: the scratch address, the bytes it held before,
    /// which the borrower must write back on every path, error paths
    /// included, and the stack guard, which the borrower keeps until it has.
    /// `Ok(None)` where the scratch cannot be used and nothing is left
    /// changed: it cannot be read and written through the guest's own
    /// protections (a stack pointer just above a guard page), or its commit
    /// failed and what it wrote was put back. `Err` where a failed commit's
    /// partial write could not be put back.
    pub(super) async fn borrow_scratch<G: Guest<Self>, const N: usize>(
        guest: &mut G,
    ) -> Result<
        Option<(
            AddrMut<'static, u8>,
            [u8; N],
            <G::Stack as Stack>::StackGuard,
        )>,
        String,
    > {
        let mut stack = guest.stack().await;
        let scratch = stack.reserve::<[u8; N]>().cast::<u8>();
        let mut original = [0_u8; N];
        // `commit` writes the reserved bytes, so save them before it.
        if !super::save_procfs_scratch(&mut guest.memory(), scratch, &mut original) {
            return Ok(None);
        }
        match stack.commit() {
            Ok(guard) => Ok(Some((scratch, original, guard))),
            // A commit that failed part way may have written some of them.
            Err(_) => match guest.memory().write_exact(scratch, &original) {
                Ok(()) => Ok(None),
                Err(errno) => Err(format!(
                    "a failed stack commit's partial write to the guest stack scratch could not \
                     be put back: {errno}"
                )),
            },
        }
    }

    /// Apply the failure policy to an injected call Detcore could not finish
    /// cleanly (`reason`): guest stack scratch it could not put back, or a
    /// probe descriptor it could not close. Logged by name; a fail-closed run
    /// stops with the policy's exit, and a run with
    /// `--allow-unsupported-syscalls` gives the guest's call EOPNOTSUPP.
    /// Never an ordinary answer such as EACCES.
    async fn scratch_failure<G: Guest<Self>>(
        guest: &mut G,
        sysno: reverie::syscalls::Sysno,
        reason: String,
    ) -> Error {
        error!("Detcore refuses {sysno}: {reason}");
        let panic_on_unsupported = guest.config().panic_on_unsupported_syscalls;
        match crate::syscalls::helpers::refuse_unserviceable_operation_with(
            guest,
            panic_on_unsupported,
            sysno,
            Errno::EOPNOTSUPP,
            Some(&reason),
        )
        .await
        {
            Ok(_) => Errno::EOPNOTSUPP.into(),
            Err(error) => error,
        }
    }

    /// Open the rendered procfs file `target` names with the guest's `flags`.
    pub(super) async fn open_rendered_procfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        flags: OFlag,
        target: RenderedTarget,
    ) -> Result<i64, Error> {
        // A descriptor link is a symbolic link: `O_NOFOLLOW` refuses it, once
        // the flags themselves passed. (With `O_PATH`, the caller let the
        // kernel open the link itself.)
        if let Some(errno) = open_flags_refused(flags) {
            return Err(errno.into());
        }
        // `O_CREAT|O_EXCL` follows no final link: the link exists (EEXIST).
        if target.via_link
            && flags.contains(OFlag::O_CREAT | OFlag::O_EXCL)
            && !flags.contains(OFlag::O_PATH)
        {
            return Err(Errno::EEXIST.into());
        }
        if target.via_link && flags.contains(OFlag::O_NOFOLLOW) {
            return Err(Errno::ELOOP.into());
        }
        let procfs = target.procfs.into_rendered();
        let path = procfs
            .opened_path()
            .expect("a file opened as rendered has a path");
        let writes = flags & OFlag::O_ACCMODE != OFlag::O_RDONLY || flags.contains(OFlag::O_TRUNC);
        let access = self
            .rendered_file_access(guest, path, writes, CapabilitySet::Effective)
            .await?;
        let reopen = rendered_open_flags(flags, access)?;
        // A process's `mountinfo` or `mounts` takes the task's mount namespace
        // at its open (`mounts_open_common`), which a task that has exited no
        // longer has: EINVAL, after the permission check.
        if procfs.mount_table_rendering().is_some()
            && !flags.contains(OFlag::O_PATH)
            && let Some((answer, current)) = self.rendered_task_state(guest, path).await
            && (!current || answer.without_mm)
        {
            return Err(Errno::EINVAL.into());
        }
        // A process's `auxv`: its snapshot is the process's auxiliary vector
        // now, which the open pins (`open_process_auxv`).
        // Boxed: the access check's injected calls would otherwise grow every
        // open's future, which some backends poll on a small stack.
        let opened = if procfs.is_process_auxv() && !flags.contains(OFlag::O_PATH) {
            Some(Box::pin(self.open_process_auxv(guest, &procfs)).await?)
        } else {
            None
        };
        GuestProcfs::remember();
        // Read before the placeholder exists, so that a failure leaves the
        // guest nothing open.
        let metadata = self.rendered_metadata(guest, path).await?;
        // The snapshot this open takes, which its placeholder's name carries,
        // so that every backend has it after `exec`; a file whose every
        // snapshot is the same is rendered again there
        // (`ProcfsFile::snapshot_is_rerendered`). A file Linux renders when it
        // is read takes none until its first read
        // (`ProcfsFile::snapshot_at_first_read`).
        let snapshot = match opened {
            Some(bytes) => Some(bytes),
            None if procfs.is_process_auxv() || procfs.is_process_cmdline() => None,
            None => {
                (!procfs.snapshot_at_first_read()).then(|| self.render_snapshot(guest, &procfs))
            }
        };
        // A process's `auxv`'s snapshot does not fit a memfd name: it stays
        // with the open file description (and `PLACEHOLDER_OPENS`). A
        // `cmdline` takes none: its open reads no memory
        // (`proc_pid_cmdline_ops` has no `open`), and every read reads the
        // memory of the process it names (`Self::read_rendered_cmdline`).
        let carried: Vec<u8> = match &snapshot {
            Some(snapshot)
                if !procfs.snapshot_is_rerendered()
                    && !procfs.is_process_auxv()
                    && !procfs.is_process_cmdline() =>
            {
                snapshot.clone()
            }
            _ => Vec::new(),
        };
        let (fd, identity) = self
            .inject_placeholder(guest, path, &carried, reopen)
            .await?;
        // What a descriptor rebuilt from the placeholder's identity reads
        // (`adopt_received_placeholder`): a process's `auxv` keeps its whole
        // snapshot there, which its name cannot carry.
        let registered = match &snapshot {
            Some(snapshot) if procfs.is_process_auxv() => snapshot.clone(),
            _ => carried,
        };
        // The status flags Linux keeps, which include `O_NOFOLLOW`: the
        // placeholder cannot be reopened with it through its link.
        guest.thread_state().add_fd(
            fd,
            reopen | (flags & OFlag::O_NOFOLLOW),
            FdType::Regular,
            Some(metadata),
        )?;
        let procfs = match snapshot {
            Some(snapshot) => procfs.with_rendered_snapshot(snapshot),
            None => procfs,
        };
        let handle = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_path(path);
            detfd.set_procfs(procfs.clone());
            detfd.open_file_handle()
        })?;
        register_placeholder_open(identity, handle, registered);
        Ok(fd as i64)
    }

    /// What Linux's `auxv_open` does for the rendered `/proc/<pid>/auxv`
    /// `procfs` names, after the open's permission check: it looks for the
    /// task's memory map (`proc_mem_open`, `mm_access`) and pins it, so the
    /// file reads that map's `saved_auxv` for as long as it is open, through
    /// any later exec or the task's exit. Another thread group's map needs
    /// `ptrace_may_access` ([`Self::ptrace_read_allowed`]). Returns the
    /// auxiliary vector the open snapshots.
    ///
    /// A zombie's map is gone (ESRCH in Linux), but its file is global
    /// root's, which no guest in Hermit's user namespace can read
    /// (`rendered_file_access`), so that open has already failed with EACCES.
    async fn open_process_auxv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        procfs: &ProcfsFile,
    ) -> Result<Vec<u8>, Error> {
        let task = procfs.bound_task().ok_or(Errno::ENOENT)?;
        let target = detcore_model::pid::DetPid::from_raw(task.process);
        let entry = crate::tool_global::process_auxv(guest, target)
            .await
            .ok_or(Errno::ESRCH)?;
        // The caller's own thread group needs no permission
        // (`__ptrace_may_access`'s `same_thread_group`).
        if self.own_detpid(guest) != entry.process {
            Self::ptrace_read_allowed(guest, task).await??;
        }
        Ok(entry.bytes)
    }

    /// Linux's `ptrace_may_access(task, PTRACE_MODE_READ_FSCREDS)` for the
    /// caller and `task`, the task a `/proc/<pid>/auxv` path names (the
    /// thread below `task/`, else the number at the top of `/proc`), in
    /// another thread group: `Ok(Ok(()))` where the caller may open the file,
    /// `Ok(Err(errno))` where the open fails with `errno`.
    ///
    /// Where the kernel beneath serves the caller's `/proc` (ptrace), the
    /// kernel decides, by the caller's own open of the kernel's
    /// `/proc/<pid>/auxv` ([`Self::kernel_procfs_open_answer`], by the
    /// kernel's number, `kernel_procfs_number`): Linux's whole rule,
    /// credentials, user namespaces (a guest may make one with
    /// `clone(CLONE_NEWUSER)`), the memory map's namespace and its
    /// dumpability, applied by Linux itself. Whether the kernel beneath
    /// serves this caller's `/proc` is a fact the kernel reports, not a
    /// backend switch: a refused probe is followed by the caller's open of
    /// its own `/proc/thread-self/stat` (mode 0444, which no credential or
    /// dumpability refuses). Where that is refused with EACCES too (KVM's
    /// provider refuses every `/proc` file with EACCES, its backstop), the
    /// answer is `CAP_SYS_PTRACE` in the caller's own effective set
    /// (`capget`), else EACCES: KVM's named difference from Linux, which also
    /// asks about the target's dumpability and capabilities. Any other errno
    /// from either probe (EMFILE, ENFILE, ENOMEM, ...) is the open's answer,
    /// as Linux's own open would fail with it ([`auxv_access_answer`]).
    ///
    /// With sequentialized threads every call runs in the caller's turn, so
    /// the answer is the guest's own history's. Without them
    /// (`--no-sequentialize-threads`, a declared relaxation) a target's
    /// thread may change its credentials, dumpability or namespace
    /// concurrently, and the answer is host-timed; and the probe's
    /// descriptor briefly takes the lowest free number, so a concurrent open
    /// by another guest thread may be given a higher number than in another
    /// run.
    ///
    /// `Err` is a probe Detcore could not finish cleanly (no stack scratch
    /// for its path, scratch it could not put back, a descriptor it could
    /// not close, the caller's capabilities unreadable), after the failure
    /// policy (`scratch_failure`): never an access answer.
    // TODO-HUMAN-REVIEW(#4213): Review the access check of another process's
    // open of its rendered /proc/<pid>/auxv by the kernel beneath's answer.
    pub(crate) async fn ptrace_read_allowed<G: Guest<Self>>(
        guest: &mut G,
        task: crate::procfs::BoundTask,
    ) -> Result<Result<(), Errno>, Error> {
        let named = task.thread.unwrap_or(task.process);
        let target = match Self::kernel_procfs_number(guest, named).await {
            Some(kernel) => Self::probe(guest, format!("/proc/{kernel}/auxv").as_bytes()).await?,
            None => Err(Errno::ESRCH),
        };
        if target.is_ok() {
            return Ok(Ok(()));
        }
        let serves = Self::probe(guest, b"/proc/thread-self/stat").await?;
        match auxv_access_answer(target, serves) {
            AuxvAccess::Answer(answer) => Ok(answer),
            AuxvAccess::CallerCapability => {
                match Self::inject_capget(guest, CapabilitySet::Effective).await? {
                    Some(effective) if effective & (1 << CAP_SYS_PTRACE) != 0 => Ok(Ok(())),
                    Some(_) => Ok(Err(Errno::EACCES)),
                    None => Err(Self::scratch_failure(
                        guest,
                        reverie::syscalls::Sysno::openat,
                        "the caller's capabilities, which decide another process's \
                         /proc/<pid>/auxv where the kernel beneath serves no /proc, could \
                         not be read"
                            .to_string(),
                    )
                    .await),
                }
            }
        }
    }

    /// [`Self::kernel_procfs_open_answer`], with the failure policy applied
    /// to a probe Detcore could not finish cleanly.
    async fn probe<G: Guest<Self>>(guest: &mut G, path: &[u8]) -> Result<Result<(), Errno>, Error> {
        match Self::kernel_procfs_open_answer(guest, path).await {
            Ok(answer) => Ok(answer),
            Err(reason) => {
                Err(Self::scratch_failure(guest, reverie::syscalls::Sysno::openat, reason).await)
            }
        }
    }

    /// The kernel beneath's answer to the caller opening `path` (a kernel
    /// `/proc` file) for reading: `Ok(Ok(()))` where it opens, `Ok(Err(errno))`
    /// where it fails with `errno`. An open Reverie reports did not run
    /// (ERESTARTSYS) is retried, so that errno never reaches the guest. The
    /// probe's descriptor is closed: a close Reverie reports did not run
    /// (ERESTARTSYS) is retried; EINTR is not, since Linux has released the
    /// descriptor before close returns it, and a retry could close a number
    /// another thread was given. The path lives in borrowed stack scratch
    /// whose original bytes go back on every path ([`Self::borrow_scratch`]).
    /// `Err` (no scratch for the path, scratch not put back, any other close
    /// error) is a probe that could not be finished cleanly.
    pub(super) async fn kernel_procfs_open_answer<G: Guest<Self>>(
        guest: &mut G,
        path: &[u8],
    ) -> Result<Result<(), Errno>, String> {
        const PATH_CAPACITY: usize = 32;
        let (scratch, original, stack_guard) =
            match Self::borrow_scratch::<G, PATH_CAPACITY>(guest).await? {
                Some(borrowed) => borrowed,
                None => {
                    return Err(format!(
                        "no guest stack scratch can hold the path of the access probe of {}",
                        String::from_utf8_lossy(path)
                    ));
                }
            };
        // Moved into the block by value: a borrowed address would make the
        // future not `Send`.
        let probe = &mut *guest;
        let answer: Result<Result<(), Errno>, String> = async move {
            let guest = probe;
            let bytes = c_string::<PATH_CAPACITY>(path);
            if let Err(errno) = guest.memory().write_exact(scratch, &bytes) {
                return Err(format!(
                    "the path of the access probe of {} could not be written to the guest \
                     stack scratch: {errno}",
                    String::from_utf8_lossy(path)
                ));
            }
            let opened = loop {
                match guest
                    .inject(
                        syscalls::Openat::new()
                            .with_dirfd(libc::AT_FDCWD)
                            .with_path(PathPtr::from_ptr(scratch.as_raw() as *const libc::c_char))
                            .with_flags(OFlag::O_RDONLY | OFlag::O_CLOEXEC),
                    )
                    .await
                {
                    Err(Errno::ERESTARTSYS) => continue,
                    opened => break opened,
                }
            };
            let fd = match opened {
                Ok(fd) => fd as RawFd,
                Err(errno) => return Ok(Err(errno)),
            };
            loop {
                match guest.inject(syscalls::Close::new().with_fd(fd)).await {
                    Ok(_) | Err(Errno::EINTR) => return Ok(Ok(())),
                    Err(Errno::ERESTARTSYS) => continue,
                    Err(errno) => {
                        return Err(format!(
                            "the descriptor {fd} of the access probe of {} could not be closed: \
                             {errno}",
                            String::from_utf8_lossy(path)
                        ));
                    }
                }
            }
        }
        .await;
        let restored = guest.memory().write_exact(scratch, &original);
        drop(stack_guard);
        if let Err(errno) = restored {
            return Err(format!(
                "the guest stack scratch under the access probe of {} could not be restored: \
                 {errno}",
                String::from_utf8_lossy(path)
            ));
        }
        answer
    }

    /// The snapshot of the rendered file `procfs` an open takes, from
    /// Detcore's state, as its first read used to: a UUID from the thread's
    /// generator, Detcore's count of open ptys, the virtual CPU's table
    /// (`Detcore::virtual_cpuid`, which answers the guest's CPUID
    /// instruction), or a fact of the machine description.
    fn render_snapshot<G: Guest<Self>>(&self, guest: &mut G, procfs: &ProcfsFile) -> Vec<u8> {
        use rand::RngExt as _;
        let mut file = procfs.clone();
        let random_uuid = file
            .needs_random_uuid()
            .then(|| guest.thread_state_mut().thread_prng().random::<[u8; 16]>());
        let virtual_pty_count = guest
            .thread_state()
            .count_open_files_at_paths(&[Path::new("/dev/ptmx"), Path::new("/dev/pts/ptmx")]);
        let virtual_cpuinfo = file.is_cpuinfo().then(|| self.virtual_cpuid().cpuinfo());
        let template = file.rendering_template().to_vec();
        file.initialize(
            template,
            crate::procfs::ProcfsSnapshotContext {
                random_uuid,
                virtual_pty_count,
                virtual_cpuinfo,
                machine: crate::machine::MachineDescription::of(guest.config()),
                ..Default::default()
            },
        );
        file.rendered_snapshot()
            .expect("a rendered file has its snapshot once initialized")
            .to_vec()
    }

    /// The uptime, in seconds, that the first read in this process of the
    /// rendered file `path` behind `fd`, one whose snapshot its first read
    /// takes (`ProcfsFile::snapshot_at_first_read`), renders: `uptime_seconds`,
    /// the uptime now, unless an earlier read of the same open file already
    /// took the snapshot and carried its uptime on the placeholder.
    ///
    /// Linux keeps that snapshot in the open file, for every process sharing
    /// it and across `exec`. Detcore keeps it in its state of the open file,
    /// which ptrace and KVM keep across `fork` and `exec` too; where the tool
    /// runs in each guest process (DBT, SaBRe, in-guest LiteInst), that state
    /// is the process's own, and a process that shares the open file, or the
    /// image after `exec`, starts without it. Every other byte of the
    /// snapshot is fixed for the run, so the uptime is all it needs, and the
    /// placeholder, one kernel object per open file, carries it in its
    /// modification time ([`carried_uptime_mtime`]). The guest never sees
    /// that time (every stat of a rendered file is Detcore's
    /// `rendered_stat`) and cannot set it (Detcore answers the guest's
    /// `utimensat` of a rendered file, by descriptor too).
    ///
    /// The carry is read and written only where nothing can change what the
    /// descriptor number names between Detcore's look and its write
    /// ([`may_carry_uptime`]): with threads sequentialized, Detcore holds the
    /// scheduler turn across both injected calls, no guest code runs inside
    /// the handler, and the descriptor must be the placeholder of `path` this
    /// tool process registered, by its kernel identity. Under
    /// `--no-sequentialize-threads` another thread could replace the number
    /// in between, and on SaBRe and in-guest LiteInst an accepted native
    /// signal handler could, so the write could land on another file: there
    /// is no carry there, and a process without Detcore's state of the open
    /// file (after `exec` or `fork` on SaBRe) takes its own snapshot at its
    /// next read, as before.
    ///
    /// If the write fails (the process no longer owns the placeholder after
    /// changing its user), the snapshot is still taken, and is not carried.
    pub(super) async fn first_read_uptime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        path: &'static Path,
        uptime_seconds: u64,
    ) -> u64 {
        let stable = descriptor_stable_across_injections(
            guest.config().sequentialize_threads,
            guest.config().backend.runs_guest_code_between_injections,
        );
        if !stable {
            return uptime_seconds;
        }
        let Ok(stat) = self.inject_fstat(guest, fd).await else {
            return uptime_seconds;
        };
        if !may_carry_uptime(stable, placeholder_identity(stat.st_dev, stat.st_ino), path) {
            return uptime_seconds;
        }
        if let Some(carried) = carried_uptime(stat.st_mtime, stat.st_mtime_nsec) {
            return carried;
        }
        let times = [
            Timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            carried_uptime_mtime(uptime_seconds),
        ];
        if let Err(errno) = Self::inject_futimens(guest, fd, times).await {
            warn!(
                "The first read of {} on fd {fd} could not carry its uptime on the \
                 placeholder ({errno}); a process without Detcore's state of the open \
                 file takes its own snapshot.",
                path.display()
            );
        }
        uptime_seconds
    }

    /// `utimensat(fd, NULL, times, 0)`, its times in guest stack scratch, or in
    /// a private anonymous page mapped for this call only where the stack
    /// cannot hold them (as `inject_fstat` falls back,
    /// https://github.com/rrnewton/hermit/issues/3328).
    async fn inject_futimens<G: Guest<Self>>(
        guest: &mut G,
        fd: RawFd,
        times: [Timespec; 2],
    ) -> Result<(), Errno> {
        let call = |times| {
            syscalls::Utimensat::new()
                .with_dirfd(fd)
                .with_path(None)
                .with_times(Some(times))
                .with_flags(0)
        };
        let mut stack = guest.stack().await;
        let staged = stack.push(times);
        // Keep the guard until the call has read the times: on backends whose
        // scratch is a Tool-owned arena (DBT, SaBRe), dropping it frees them.
        if let Ok(stack_guard) = stack.commit() {
            let result = guest.inject(call(staged)).await;
            drop(stack_guard);
            if result != Err(Errno::EFAULT) {
                return result.map(drop);
            }
        }
        let len = std::mem::size_of::<[Timespec; 2]>();
        let mapped = guest
            .inject_with_retry(Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(len)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await?;
        let page = usize::try_from(mapped).map_err(|_| Errno::EFAULT)?;
        let staged = Addr::<[Timespec; 2]>::from_raw(page).ok_or(Errno::EFAULT)?;
        let written = AddrMut::<[Timespec; 2]>::from_raw(page)
            .ok_or(Errno::EFAULT)
            .and_then(|address| guest.memory().write_value(address, &times));
        let result = match written {
            Ok(()) => guest.inject(call(staged)).await.map(drop),
            Err(errno) => Err(errno),
        };
        let _ = guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Some(staged.cast()))
                    .with_len(len),
            ))
            .await;
        result
    }

    /// Inject a placeholder for `path` open with `flags`, at the lowest free
    /// descriptor number, as `open` would place the file.
    async fn inject_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &'static Path,
        snapshot: &[u8],
        flags: OFlag,
    ) -> Result<(RawFd, (u64, u64)), Error> {
        let name = placeholder_name(path, snapshot);
        let mut stack = guest.stack().await;
        let name = stack.push(c_string::<PLACEHOLDER_NAME_CAPACITY>(name.as_bytes()));
        let link = stack.reserve::<[u8; 32]>();
        let stack_guard = stack.commit()?;
        let name = name.as_raw();
        let create = move |flags| {
            syscalls::MemfdCreate::new()
                .with_name(PathPtr::from_ptr(name as *const libc::c_char))
                .with_flags(flags)
        };
        // Not executable, which a hardened `vm.memfd_noexec` requires and
        // kernels before 6.3 do not know; sealed, so that a spelling Detcore
        // does not recognise cannot change the empty file.
        let sealable = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
        let memfd = match guest.inject(create(sealable | libc::MFD_NOEXEC_SEAL)).await {
            Err(Errno::EINVAL) => guest.inject(create(sealable)).await?,
            created => created?,
        } as RawFd;
        // A backend without seals (KVM refuses `F_ADD_SEALS`) leaves the
        // placeholder unsealed; Detcore still answers every call it knows.
        let _ = guest
            .inject(
                syscalls::Fcntl::new()
                    .with_fd(memfd)
                    .with_cmd(syscalls::FcntlCmd::F_ADD_SEALS(PLACEHOLDER_SEALS)),
            )
            .await;
        // By the calling thread's own table (`/proc/thread-self/fd`): the
        // leader's (`/proc/self/fd`) lacks the memfd in a thread made without
        // CLONE_FILES, and the reopen then failed with ENOENT.
        guest.memory().write_exact(
            link.cast::<u8>(),
            &c_string::<32>(format!("/proc/thread-self/fd/{memfd}").as_bytes()),
        )?;
        // Reopening through the descriptor link gives a description with the
        // guest's flags, which then takes the memfd's number.
        let reopened = guest
            .inject(
                syscalls::Openat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(link.as_raw() as *const libc::c_char))
                    .with_flags(flags | OFlag::O_CLOEXEC),
            )
            .await;
        let moved = match reopened {
            Ok(reopened) => {
                let moved = guest
                    .inject(
                        syscalls::Dup3::new()
                            .with_oldfd(reopened as i32)
                            .with_newfd(memfd)
                            .with_flags(flags & OFlag::O_CLOEXEC),
                    )
                    .await;
                let _ = guest
                    .inject(syscalls::Close::new().with_fd(reopened as i32))
                    .await;
                moved
            }
            Err(errno) => Err(errno),
        };
        drop(stack_guard);
        // The guest gets the errno that kept the file from opening, as an open
        // that runs out of descriptors or memory reports it; nothing is left
        // open.
        if let Err(errno) = moved {
            let _ = guest.inject(syscalls::Close::new().with_fd(memfd)).await;
            return Err(errno.into());
        }
        let identity = match self.inject_fstat(guest, memfd).await {
            Ok(stat) => stat,
            Err(errno) => {
                let _ = guest.inject(syscalls::Close::new().with_fd(memfd)).await;
                return Err(errno.into());
            }
        };
        register_placeholder(identity.st_dev, identity.st_ino, path);
        Ok((memfd, (identity.st_dev, identity.st_ino)))
    }

    /// The inode number of the directory entry `name`, which the host listed
    /// with inode `host_ino`, if it is a procfs file Detcore renders. A
    /// per-process file is numbered for the task holding its numbers now (the
    /// process table's incarnation, `Detcore::rendered_task_state`), as a stat
    /// of it would be, and only where Detcore renders procfs files (not after
    /// `chroot`, where the kernel's file is the file).
    pub(super) async fn rendered_listed_inode<G: Guest<Self>>(
        &self,
        guest: &mut G,
        inodes: &procfs_inode::DirectoryInodes,
        name: &[u8],
        host_ino: u64,
        files: RenderedFiles,
    ) -> Result<Option<u64>, Error> {
        if self.cfg.recordreplay_modes {
            return Ok(None);
        }
        if let Some(path) = inodes.entry_procfs_path(name)
            && let Some(mut file) = ProcfsFile::from_path(&path)
            && file.per_process_name().is_some()
        {
            if !self.renders_procfs(guest) {
                return Ok(None);
            }
            let Some(task) = file.bound_task() else {
                return Ok(None);
            };
            let answer = crate::tool_global::procfs_task(guest, task).await;
            if !answer.exists {
                return Ok(None);
            }
            let without_mm = answer.without_mm;
            self.bind_task_identity(guest, &mut file, answer).await?;
            // A process whose `auxv` the kernel serves lists the kernel's
            // entry, as a path to it opens the kernel's file; so does another
            // process's `cmdline`.
            if self.kept_by_kernel(guest, &file, without_mm).await {
                return Ok(None);
            }
            return Ok(file.rendered_path(files).map(procfs_inode::rendered_inode));
        }
        // By kernel identity only for an entry named as a rendered file is,
        // which costs a stat for each rendered file of that name (at most
        // two: `version`), and only then.
        Ok(inodes.rendered_entry_inode(name, files).or_else(|| {
            let named = rendered_paths(files).filter(|path| rendered_name(path) == name);
            rendered_path_among(inodes.device(), host_ino, named).map(procfs_inode::rendered_inode)
        }))
    }

    /// Whether `file` is a process's `auxv` that the kernel serves: Detcore
    /// recorded no auxiliary vector for the process's image (the backend
    /// reports no initial stack pointer, or the vector there is implausible;
    /// `Detcore::record_process_auxv`), so every spelling of it, and its
    /// listing, is the kernel's file.
    ///
    /// Never a zombie's (`without_mm`): its file is served on the rendered
    /// path, whose access check refuses it (EACCES, global root's), whether
    /// or not its vector is still kept, so the host-timed moment the global
    /// tool forgets an exited process's vector changes nothing the guest
    /// sees.
    ///
    /// And whether `file` is a process's `cmdline` that the kernel serves:
    /// another process's, whose memory Detcore cannot read (Linux reads the
    /// target's memory at each read), or the caller's own where Detcore
    /// recorded no argument area for its image (the same backends and stacks
    /// as for `auxv`).
    async fn kept_by_kernel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        file: &ProcfsFile,
        without_mm: bool,
    ) -> bool {
        let Some(task) = file.bound_task() else {
            return false;
        };
        let process = detcore_model::pid::DetPid::from_raw(task.process);
        if file.is_process_cmdline() {
            let (process, area) = crate::tool_global::process_cmdline(guest, process).await;
            return process != self.own_detpid(guest) || area.is_none();
        }
        if !file.is_process_auxv() || without_mm {
            return false;
        }
        crate::tool_global::process_auxv(guest, process)
            .await
            .is_none()
    }

    /// One plain read of the rendered `/proc/<pid>/cmdline` open as `fd`, of
    /// `count` bytes at position `pos` into the caller's buffer at
    /// `destination`, as Linux's `proc_pid_cmdline_read` reads it
    /// (fs/proc/base.c:393-408): it finds the descriptor's task again (ESRCH
    /// where it is gone, or another task now holds its numbers), reads
    /// nothing where the task has no memory map (a zombie, or the top-level
    /// file of a thread group whose leader has exited: `get_task_mm`), and
    /// otherwise reads the process's memory now, at the read's position,
    /// page by page, copying out after each page (`procfs::read_cmdline`).
    /// Returns the bytes copied.
    ///
    /// Detcore reads only the calling process's memory. Where the file is
    /// another process's (a descriptor its owner opened that reached the
    /// caller across `fork` or `SCM_RIGHTS`), the read goes to the kernel
    /// beneath's file of that task ([`Self::kernel_cmdline_read`]), as before
    /// Detcore rendered it, and never to a copy.
    pub(crate) async fn read_rendered_cmdline<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        destination: usize,
        count: usize,
        pos: u64,
    ) -> Result<usize, Error> {
        let path = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_rendered_path())?
            .ok_or(Errno::EBADF)?;
        let task = ProcfsFile::from_rendered_path(path)
            .and_then(|file| file.bound_task())
            .ok_or(Errno::ESRCH)?;
        match self.rendered_task_state(guest, path).await {
            Some((_, false)) | None => return Err(Errno::ESRCH.into()),
            Some((answer, true)) if answer.without_mm => return Ok(0),
            Some(_) => {}
        }
        let (process, area) = crate::tool_global::process_cmdline(
            guest,
            detcore_model::pid::DetPid::from_raw(task.process),
        )
        .await;
        // A read of zero bytes has found its task, which is all Linux's
        // does before get_mm_cmdline returns 0 (base.c:325-328): no memory,
        // and no injection.
        if count == 0 {
            return Ok(0);
        }
        // Another process's file, or the caller's own where Detcore recorded
        // no argument area for its current image (a stack layout
        // initial_stack_cmdline_area refused, or one it could not read, on a
        // backend that reports the initial stack pointer; a descriptor kept
        // across an exec into such an image), is the kernel beneath's.
        let Some(area) = area.filter(|_| process == self.own_detpid(guest)) else {
            return Box::pin(self.kernel_cmdline_read(guest, task, area, destination, count, pos))
                .await;
        };
        let mut memory = guest.memory();
        let mut adapter = GuestCmdlineMemory {
            memory: &mut memory,
            destination,
            failure: None,
        };
        let read = crate::procfs::read_cmdline(&area, pos, count, &mut adapter);
        if let Some(failure) = adapter.failure {
            return Err(failure.into());
        }
        Ok(read?)
    }

    /// One read of `count` bytes at `pos` of the kernel beneath's
    /// `/proc/<pid>/cmdline` (or `/proc/<pid>/task/<tid>/cmdline`) of `task`,
    /// into the caller's buffer at `destination`: the caller's own open of
    /// the kernel's file by the kernel's numbers
    /// (`Self::kernel_procfs_number`), a `pread64` into the caller's buffer,
    /// and the close ([`Self::inject_cmdline_pread`]). On ptrace the kernel
    /// reads the task's memory as Linux does; where the kernel beneath serves
    /// no `/proc` for the caller (KVM), its answer to the open is the read's
    /// (https://github.com/rrnewton/hermit/issues/4564). The path lives in
    /// borrowed stack scratch whose bytes go back on every path, or, where
    /// that scratch would overlap the caller's buffer (whose bytes its
    /// restoration would erase) or the target's recorded argument and
    /// environment area `source` (which the caller shares when the target
    /// is a `CLONE_VM` relative, and whose bytes the kernel would read with
    /// the path written over them), in a transient private page; where that
    /// page overlaps either too, or a step Detcore cannot finish cleanly, the
    /// read goes to the failure policy.
    ///
    /// This reopens a pathname in the caller's own root, mount namespace,
    /// descriptor table and seccomp filter, where Linux's read of the open
    /// descriptor needs none of them: a caller that changed its root or mount
    /// namespace so that `/proc` is not the kernel's procfs reads ENOENT or
    /// another file, a full descriptor table gives EMFILE, and a seccomp
    /// filter that refuses the open gives its answer
    /// (https://github.com/rrnewton/hermit/issues/4568).
    async fn kernel_cmdline_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        task: crate::procfs::BoundTask,
        source: Option<crate::procfs::CmdlineArea>,
        destination: usize,
        count: usize,
        pos: u64,
    ) -> Result<usize, Error> {
        const PATH_CAPACITY: usize = 64;
        let process = Self::kernel_procfs_number(guest, task.process)
            .await
            .ok_or(Errno::ESRCH)?;
        let path = match task.thread {
            Some(thread) => {
                let thread = Self::kernel_procfs_number(guest, thread)
                    .await
                    .ok_or(Errno::ESRCH)?;
                format!("/proc/{process}/task/{thread}/cmdline")
            }
            None => format!("/proc/{process}/cmdline"),
        };
        let borrowed = match Self::borrow_scratch::<G, PATH_CAPACITY>(guest).await {
            Ok(Some(borrowed)) => borrowed,
            Ok(None) => {
                return Err(Self::scratch_failure(
                    guest,
                    Sysno::read,
                    format!("no guest stack scratch can hold the path {path}"),
                )
                .await);
            }
            Err(reason) => return Err(Self::scratch_failure(guest, Sysno::read, reason).await),
        };
        let (scratch, original, stack_guard) = borrowed;
        let overlaps = |at: usize, length: usize| {
            cmdline_path_overlaps(at, length, destination, count, source)
        };
        if !overlaps(scratch.as_raw(), PATH_CAPACITY) {
            let answer =
                Self::inject_cmdline_pread(guest, scratch, &path, destination, count, pos).await;
            let restored = guest.memory().write_exact(scratch, &original);
            drop(stack_guard);
            if let Err(errno) = restored {
                return Err(Self::scratch_failure(
                    guest,
                    Sysno::read,
                    format!(
                        "the guest stack scratch under a kernel cmdline read could not be \
                         restored: {errno}"
                    ),
                )
                .await);
            }
            return match answer {
                Ok(read) => Ok(read?),
                Err(reason) => Err(Self::scratch_failure(guest, Sysno::read, reason).await),
            };
        }
        // The stack scratch overlaps the caller's buffer (a buffer below the
        // stack pointer), whose bytes the scratch's restoration would erase,
        // or the target's argument area (a CLONE_VM child whose stack pointer
        // lies in the parent's arguments), whose bytes the path would replace
        // while the kernel reads them. Put the stack back untouched and hold
        // the path in a transient private page instead, a new mapping that no
        // mapped buffer or area can lie in.
        let restored = guest.memory().write_exact(scratch, &original);
        drop(stack_guard);
        if let Err(errno) = restored {
            return Err(Self::scratch_failure(
                guest,
                Sysno::read,
                format!("the guest stack scratch could not be put back unused: {errno}"),
            )
            .await);
        }
        const PAGE: usize = 4096;
        let Some(page) = super::procfs_levels::map_transient(guest, PAGE).await else {
            return Err(Self::scratch_failure(
                guest,
                Sysno::read,
                format!("no transient page can hold the path {path}"),
            )
            .await);
        };
        if overlaps(page.as_raw(), PAGE) {
            super::procfs_levels::unmap_transient(guest, page, PAGE).await;
            return Err(Self::scratch_failure(
                guest,
                Sysno::read,
                format!(
                    "the transient page for the path {path} overlaps the caller's buffer or \
                     the target's argument area"
                ),
            )
            .await);
        }
        let answer = Self::inject_cmdline_pread(guest, page, &path, destination, count, pos).await;
        super::procfs_levels::unmap_transient(guest, page, PAGE).await;
        match answer {
            Ok(read) => Ok(read?),
            Err(reason) => Err(Self::scratch_failure(guest, Sysno::read, reason).await),
        }
    }

    /// The caller's open of the kernel's `path`, written to `scratch`, a
    /// `pread64` of `count` bytes at `pos` into the caller's buffer at
    /// `destination`, and the close ([`Self::kernel_cmdline_read`]). An
    /// injected call Reverie reports did not run (ERESTARTSYS) is retried.
    /// `Ok(Err(errno))` is the kernel's answer to the open or the read;
    /// `Err` is a step Detcore could not finish cleanly.
    async fn inject_cmdline_pread<G: Guest<Self>>(
        guest: &mut G,
        scratch: AddrMut<'static, u8>,
        path: &str,
        destination: usize,
        count: usize,
        pos: u64,
    ) -> Result<Result<usize, Errno>, String> {
        const PATH_CAPACITY: usize = 64;
        if let Err(errno) = guest
            .memory()
            .write_exact(scratch, &c_string::<PATH_CAPACITY>(path.as_bytes()))
        {
            return Err(format!(
                "the path {path} could not be written to the guest scratch: {errno}"
            ));
        }
        let opened = loop {
            match guest
                .inject(
                    syscalls::Openat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(PathPtr::from_ptr(scratch.as_raw() as *const libc::c_char))
                        .with_flags(OFlag::O_RDONLY | OFlag::O_CLOEXEC),
                )
                .await
            {
                Err(Errno::ERESTARTSYS) => continue,
                opened => break opened,
            }
        };
        let fd = match opened {
            Ok(fd) => fd as RawFd,
            Err(errno) => return Ok(Err(errno)),
        };
        let read = loop {
            match guest
                .inject(
                    syscalls::Pread64::new()
                        .with_fd(fd)
                        .with_buf(AddrMut::from_raw(destination))
                        .with_len(count)
                        .with_offset(pos as libc::loff_t),
                )
                .await
            {
                Err(Errno::ERESTARTSYS) => continue,
                read => break read.map(|copied| copied as usize),
            }
        };
        loop {
            match guest.inject(syscalls::Close::new().with_fd(fd)).await {
                Ok(_) | Err(Errno::EINTR) => return Ok(read),
                Err(Errno::ERESTARTSYS) => continue,
                Err(errno) => {
                    return Err(format!(
                        "the descriptor {fd} of {path} could not be closed: {errno}"
                    ));
                }
            }
        }
    }

    /// Bind the per-process file `file` to the identity of the task the
    /// process table's `answer` describes (`ProcfsFile::set_task_incarnation`),
    /// which numbers its path and inode. A task generation beyond what an
    /// inode number tells apart (`procfs_inode::MAX_INCARNATION`: the 256th
    /// task to hold one number in a run) is refused by name through the
    /// unsupported-operation policy: a fail-closed run stops with the
    /// policy's exit and the reason, and a run started with
    /// `--allow-unsupported-syscalls` gets EOVERFLOW. Never another task's
    /// number, and never a panic.
    pub(super) async fn bind_task_identity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        file: &mut ProcfsFile,
        answer: crate::procfs::listing::ProcfsTaskAnswer,
    ) -> Result<(), Error> {
        let incarnation = answer.incarnation();
        if procfs_inode::incarnation_fits(incarnation) {
            file.set_task_incarnation(incarnation);
            return Ok(());
        }
        let task = file.bound_task();
        let reason = format!(
            "the per-process /proc file of task {task:?} would belong to its numbers' \
             incarnation {incarnation:?}, beyond the {} task generations an inode number \
             of such a file tells apart (procfs_inode::MAX_INCARNATION)",
            procfs_inode::MAX_INCARNATION
        );
        let sysno = reverie::syscalls::Sysno::new(guest.regs().await.orig_rax as usize)
            .unwrap_or(reverie::syscalls::Sysno::openat);
        error!("Detcore refuses {sysno}: {reason}");
        crate::syscalls::helpers::refuse_unserviceable_operation_with(
            guest,
            self.cfg.panic_on_unsupported_syscalls,
            sysno,
            Errno::EOVERFLOW,
            Some(&reason),
        )
        .await
        .map(|_| ())
    }

    /// Where the tool runs in each guest process, take a placeholder's cursor
    /// from its kernel position, before a read or seek uses it. Processes
    /// that share the open file across `fork` share only that position, which
    /// Detcore keeps at each one's cursor after every read and seek
    /// (`sync_rendered_procfs_position`). Where one tool process serves every
    /// guest, the cursor itself is shared.
    pub(super) async fn adopt_rendered_procfs_position<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<(), Error> {
        if !self.cfg.backend.tool_shares_guest_descriptor_table {
            return Ok(());
        }
        let kept = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_keeps_kernel_position())
            .unwrap_or(false);
        if !kept {
            return Ok(());
        }
        // An `O_PATH` descriptor has no position (EBADF): nothing to adopt.
        if let Ok(offset) = guest
            .inject(
                syscalls::Lseek::new()
                    .with_fd(fd)
                    .with_offset(0)
                    .with_whence(syscalls::Whence::SEEK_CUR),
            )
            .await
        {
            guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.set_procfs_offset(offset as usize))?;
        }
        Ok(())
    }

    /// Keep a placeholder's kernel position at Detcore's cursor, which a
    /// backend that rediscovers descriptors after `exec`, or another process
    /// sharing the open file, reads back (`adopt_rendered_procfs_position`).
    pub(super) async fn sync_rendered_procfs_position<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<(), Error> {
        let (offset, placeholder) = guest.thread_state().with_detfd(fd, |detfd| {
            let offset = detfd
                .procfs_keeps_kernel_position()
                .then(|| detfd.procfs_position().map(|(offset, _)| offset))
                .flatten();
            (offset, detfd.procfs_rendered_path().is_some())
        })?;
        if let Some(offset) = offset {
            let moved = guest
                .inject(
                    syscalls::Lseek::new()
                        .with_fd(fd)
                        .with_offset(offset as libc::off_t)
                        .with_whence(syscalls::Whence::SEEK_SET),
                )
                .await;
            // A placeholder takes any position. The kernel's own file may
            // refuse one past its contents, which differ from Detcore's;
            // Detcore's cursor still answers the guest.
            if placeholder {
                moved?;
            }
        }
        Ok(())
    }

    /// The metadata of the rendered procfs file `target` names, if it names
    /// one.
    pub(super) async fn rendered_procfs_stat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target: StatTarget<'_>,
    ) -> Result<Option<DetStat>, Error> {
        let path = match target {
            StatTarget::Descriptor(fd) => {
                match guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.procfs_rendered_path())
                    .ok()
                    .flatten()
                {
                    Some(path) => Some(path),
                    // With metadata virtualized, `determinize_stat` already
                    // knows a placeholder by its identity.
                    None if !guest.config().virtualize_metadata => {
                        self.placeholder_descriptor(guest, fd).await
                    }
                    None => None,
                }
            }
            StatTarget::Path {
                dirfd,
                path: Some(pointer),
                follow,
            } => {
                let path = pointer.read(&guest.memory())?;
                // Without `AT_EMPTY_PATH` (which names a descriptor), an
                // empty path names nothing: the kernel's ENOENT.
                if path.as_os_str().is_empty() {
                    return Ok(None);
                }
                let observed = self.procfs_spelling_at(guest, dirfd, &path).await?;
                match self.rendered_procfs_at(guest, &observed).await? {
                    // A link's own metadata is not its target's: `lstat` of
                    // `/proc/self/fd/N` describes the link, which the kernel has.
                    Some(target) if follow || !target.via_link => {
                        target.procfs.rendered_path(self.rendered_files(guest))
                    }
                    Some(_) => None,
                    None if !guest.config().virtualize_metadata => {
                        self.placeholder_reached(guest, dirfd, Some(pointer), follow)
                            .await
                    }
                    None => None,
                }
            }
            StatTarget::Path { path: None, .. } | StatTarget::Cwd | StatTarget::Overwritten => None,
        };
        let Some(path) = path else {
            return Ok(None);
        };
        Ok(Some(self.rendered_metadata(guest, path).await?))
    }

    /// Write the guest's procfs statistics, as `statfs` of a procfs file
    /// Detcore renders reports them, to `buffer`.
    pub(super) fn write_procfs_statfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        buffer: Option<AddrMut<'_, libc::statfs>>,
    ) -> Result<(), Error> {
        let (statfs, _) = GuestProcfs::statfs()?;
        let buffer = buffer.ok_or(Errno::EFAULT)?;
        guest.memory().write_value(buffer, &statfs)?;
        self.canonicalize_statfs_buf(guest, Some(buffer))
    }

    /// Answer `readlink` of `path` if it names a procfs file Detcore renders:
    /// the file itself is no symbolic link, and this process's descriptor
    /// link to it names its path, as Linux's names the file's.
    pub(crate) async fn readlink_rendered_procfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
    ) -> Result<Option<i64>, Error> {
        let Some(target) = self.rendered_procfs_at(guest, path).await? else {
            return Ok(None);
        };
        let size = crate::syscalls::namespace::readlink_size(buffer_len)?;
        if !target.via_link {
            return Err(Errno::EINVAL.into());
        }
        // `/proc/mounts`, a link the kernel's procfs has: its target is
        // `self/mounts`, which the kernel reads.
        let Some(link) = target.procfs.opened_path() else {
            return Ok(None);
        };
        write_link(&mut guest.memory(), link, buffer, size).map(Some)
    }

    /// Rewrite a descriptor link that the kernel wrote to `buffer`, `result`
    /// bytes, if it names a placeholder: another process's link to a
    /// rendered file names the file's path, as Linux's does. (A buffer too
    /// small for the placeholder's link keeps the kernel's truncated bytes.)
    pub(crate) async fn rewrite_placeholder_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        link: (RawFd, Option<PathPtr<'_>>),
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
        result: i64,
        saved: Option<&[u8]>,
    ) -> Result<i64, Error> {
        let (Some(address), Ok(length)) = (buffer, usize::try_from(result)) else {
            return Ok(result);
        };
        // Nothing to rewrite where no placeholder can be reached: one tool
        // process serves every guest and knows none.
        if self.cfg.recordreplay_modes
            || length == 0
            || (!placeholders_exist() && !self.cfg.backend.tool_shares_guest_descriptor_table)
        {
            return Ok(result);
        }
        let mut bytes = vec![0; length.min(buffer_len)];
        guest
            .memory()
            .read_exact(address.cast::<u8>(), &mut bytes)?;
        match placeholder_link_path(&bytes, self.rendered_files(guest)) {
            Some(path) if self.placeholder_link_confirmed(guest, link).await => {
                let written = write_link(&mut guest.memory(), path, buffer, buffer_len)?;
                crate::syscalls::namespace::restore_link_tail(
                    guest,
                    address,
                    written as usize,
                    bytes.len(),
                    saved,
                )?;
                Ok(written)
            }
            _ => Ok(result),
        }
    }

    /// Linux's answer to cloning descriptor `source`'s data into a rendered
    /// file: no such descriptor, or one on procfs itself (where the
    /// read-only destination is checked next), is EBADF; another file system
    /// is EXDEV.
    fn clone_ioctl_answer<G: Guest<Self>>(guest: &mut G, source: RawFd) -> Result<i64, Errno> {
        let on_procfs = guest
            .thread_state()
            .with_detfd(source, |detfd| {
                detfd.procfs_rendered_path().is_some()
                    || detfd.path().is_some_and(|path| path.starts_with("/proc"))
            })
            .map_err(|_| Errno::EBADF)?;
        Err(if on_procfs {
            Errno::EBADF
        } else {
            Errno::EXDEV
        })
    }

    /// Whether the descriptor link the guest named at `link` (its `dirfd` and
    /// path), whose target text names a placeholder, reaches one Detcore
    /// made: by the kernel identity of what it reaches. Where the tool runs in
    /// each guest process and so may not have made it (another process's
    /// placeholder), its name is taken as it stands.
    pub(crate) async fn placeholder_link_confirmed<G: Guest<Self>>(
        &self,
        guest: &mut G,
        link: (RawFd, Option<PathPtr<'_>>),
    ) -> bool {
        if self.cfg.backend.tool_shares_guest_descriptor_table {
            return true;
        }
        let (dirfd, Some(path)) = link else {
            return false;
        };
        match Self::inject_statat(guest, dirfd, path, [Some(path), None], AtFlags::empty()).await {
            Ok(stat) => placeholder_identity(stat.st_dev, stat.st_ino).is_some(),
            Err(_) => false,
        }
    }

    /// Linux's answer to a mapping of a procfs file Detcore renders, which the
    /// placeholder standing in for it (a memfd) would accept. In `do_mmap`'s
    /// order: the arguments and the address; `MAP_FIXED_NOREPLACE` over a
    /// mapping is `EEXIST`, found by asking the kernel for the same range
    /// with nothing behind it; then the file's checks
    /// (`rendered_mmap_file_checks`). A `seq_file`'s mapping reaches
    /// `mmap_region`, which with `MAP_FIXED` unmaps whatever the range held
    /// before `proc_reg_mmap` fails, so the range is unmapped here too.
    ///
    /// The probe and the unmap are injected while the thread holds its turn,
    /// so with threads sequentialized (the default) no sibling runs between
    /// them. Under `--no-sequentialize-threads` a sibling could map into the
    /// probed range between the probe and its unmap and lose its mapping,
    /// where Linux makes the whole call under the mm's lock; there neither is
    /// made, so an overlap is not found and the range is not unmapped.
    pub(crate) async fn answer_rendered_procfs_mapping<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Option<Result<i64, Error>> {
        let Syscall::Mmap(c) = call else {
            return None;
        };
        if c.flags().contains(MapFlags::MAP_ANONYMOUS) {
            return None;
        }
        let (path, status_flags) = guest
            .thread_state()
            .with_detfd(c.fd(), |detfd| {
                (detfd.status_flags() & libc::O_PATH == 0)
                    .then(|| detfd.procfs_rendered_path())
                    .flatten()
                    .map(|path| (path, detfd.status_flags()))
            })
            .ok()
            .flatten()?;
        // A per-process file has no `mmap` method, as a sysctl file has none.
        let seq_file = crate::procfs::rendered_path_is_seq_file(path)
            && !crate::procfs::rendered_path_is_per_process(path);
        let access_mode = status_flags & libc::O_ACCMODE;
        // Exactly: access mode 3 can neither read nor write.
        let mode = (
            access_mode == libc::O_RDONLY || access_mode == libc::O_RDWR,
            access_mode == libc::O_WRONLY || access_mode == libc::O_RDWR,
        );
        let noexec = match GuestProcfs::statfs() {
            Ok((_, flags)) => flags & libc::ST_NOEXEC != 0,
            Err(errno) => return Some(Err(errno.into())),
        };
        let addr = c.addr().map_or(0, |addr| addr.as_raw());
        let flags = c.flags();
        let len = match rendered_mmap_address_checks(addr, c.len(), c.offset(), flags) {
            Ok(len) => len,
            Err(errno) => return Some(Err(errno.into())),
        };
        let sequentialized = guest.config().sequentialize_threads;
        if sequentialized && flags.contains(MapFlags::MAP_FIXED_NOREPLACE) {
            let probe = guest
                .inject_with_retry(Syscall::Mmap(
                    syscalls::Mmap::new()
                        .with_addr(c.addr())
                        .with_len(len)
                        .with_prot(ProtFlags::PROT_NONE)
                        .with_flags(
                            MapFlags::MAP_PRIVATE
                                | MapFlags::MAP_ANONYMOUS
                                | MapFlags::MAP_FIXED_NOREPLACE,
                        )
                        .with_fd(-1)
                        .with_offset(0),
                ))
                .await;
            match probe {
                // The backend refused the range itself: EEXIST for an overlap,
                // or EPERM where DBT answers an overlap so for every mapping
                // (https://github.com/rrnewton/hermit/issues/4429; an anonymous
                // PROT_NONE probe meets no other EPERM, the address having
                // passed `security_mmap_addr` above). That is its answer for
                // this range too.
                Err(errno @ (Errno::EEXIST | Errno::EPERM)) => return Some(Err(errno.into())),
                // A probe refused for another reason says nothing of an
                // overlap: its limits (`map_count` before the overlap check,
                // `RLIMIT_AS` after the file's) are not this mapping's order.
                // Nothing is unmapped for MAP_FIXED_NOREPLACE, so going on to
                // the file's checks cannot remove a mapping.
                Err(_) => {}
                // Unmap the probe where it was placed. A backend that takes
                // MAP_FIXED_NOREPLACE as a hint (KVM's executor) places it
                // elsewhere when the range is occupied: that is the overlap.
                // If that unmap fails, the PROT_NONE probe stays and the guest
                // sees the unmap's error, which Linux never gives here; the
                // unmap of a range the kernel just mapped does not fail in
                // practice.
                Ok(placed) => {
                    let placed_addr = Addr::<libc::c_void>::from_raw(placed as usize);
                    if let Err(errno) = guest
                        .inject_with_retry(Syscall::Munmap(
                            syscalls::Munmap::new().with_addr(placed_addr).with_len(len),
                        ))
                        .await
                    {
                        return Some(Err(errno.into()));
                    }
                    if placed as usize != addr {
                        return Some(Err(Errno::EEXIST.into()));
                    }
                }
            }
        }
        let errno =
            rendered_mmap_file_checks(len, c.offset(), c.prot(), flags, noexec, seq_file, mode);
        // MAP_FIXED_NOREPLACE implies MAP_FIXED in Linux, but its range is
        // vacant here, so there is nothing to unmap.
        let unmaps = sequentialized
            && errno == Errno::EIO
            && flags.contains(MapFlags::MAP_FIXED)
            && !flags.contains(MapFlags::MAP_FIXED_NOREPLACE);
        if unmaps
            && let Err(unmapped) = guest
                .inject_with_retry(Syscall::Munmap(
                    syscalls::Munmap::new().with_addr(c.addr()).with_len(len),
                ))
                .await
        {
            return Some(Err(unmapped.into()));
        }
        if unmaps {
            // As `handle_munmap` does, so Detcore's record of shared
            // mappings (futex keys, the heap) forgets the range.
            guest.thread_state().unmap_memory(addr, len);
        }
        Some(Err(errno.into()))
    }

    /// Linux's answer to a call that writes through the descriptor it names
    /// (`write`, `writev`, `pwritev2` at the file position, `ftruncate`,
    /// `fallocate`, and `sendfile` or `splice` into it) when that descriptor
    /// is a placeholder opened for writing, or with access mode 3, which only
    /// a per-process file under `CAP_DAC_OVERRIDE` is (`rendered_open_flags`).
    /// Detcore answers every such call, so none reaches the memfd. Linux opens such a
    /// file without `FMODE_CAN_WRITE` (it has no write method), so a write is
    /// `EINVAL` after the access mode is checked; `proc_setattr` accepts any
    /// size and keeps none; and it has no `fallocate` method. A positioned
    /// write is the `seq_file`'s `ESPIPE` (`answer_rendered_procfs_operation`).
    /// Measured on Linux 7.1 against `/proc/self/cgroup` as its owner with
    /// `CAP_DAC_OVERRIDE`. `sendfile` and `splice` into it answer in Linux's
    /// order, which looks at the source descriptor and the offsets first
    /// ([`Self::answer_rendered_procfs_transfer`]).
    async fn answer_rendered_procfs_write<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        args: &reverie::syscalls::SyscallArgs,
    ) -> Option<Result<i64, Errno>> {
        let fd = match sysno {
            Sysno::write | Sysno::writev | Sysno::ftruncate | Sysno::fallocate => args.arg0,
            Sysno::pwritev2 if args.arg3 as i64 == -1 => args.arg0,
            Sysno::sendfile => args.arg0,
            Sysno::splice => args.arg2,
            _ => return None,
        } as RawFd;
        let status = guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                (detfd.procfs_rendered_path().is_some() && detfd.status_flags() & libc::O_PATH == 0)
                    .then(|| detfd.status_flags())
            })
            .ok()
            .flatten();
        let writable = match status.map(|status| status & libc::O_ACCMODE) {
            None | Some(libc::O_RDONLY) => return None,
            Some(mode) => mode == libc::O_WRONLY || mode == libc::O_RDWR,
        };
        if matches!(sysno, Sysno::sendfile | Sysno::splice) {
            // `O_APPEND`, which `F_SETFL` changes, is the kernel's answer for
            // the description, not Detcore's copy, which another process
            // sharing it (a native fork's child under DBT) may have changed.
            // The access mode is fixed at the open.
            let kernel = guest
                .inject(
                    syscalls::Fcntl::new()
                        .with_fd(fd)
                        .with_cmd(syscalls::FcntlCmd::F_GETFL),
                )
                .await
                .ok()
                .map(|status| status as i32);
            let appends = kernel
                .or(status)
                .is_some_and(|status| status & libc::O_APPEND != 0);
            return Some(
                self.answer_rendered_procfs_transfer(guest, sysno, args, writable, appends)
                    .await,
            );
        }
        // Access mode 3 (O_ACCMODE) asks for both permissions at the open and
        // gives the description neither: it can neither read nor write
        // (`FMODE_READ` and `FMODE_WRITE` unset), so a write is EBADF, and
        // `ftruncate` the EINVAL of a description not open for writing.
        if !writable {
            return Some(match sysno {
                Sysno::ftruncate => Err(Errno::EINVAL),
                _ => Err(Errno::EBADF),
            });
        }
        Some(match sysno {
            Sysno::ftruncate if (args.arg1 as i64) < 0 => Err(Errno::EINVAL),
            Sysno::ftruncate => Ok(0),
            Sysno::fallocate if (args.arg2 as i64) < 0 || (args.arg3 as i64) <= 0 => {
                Err(Errno::EINVAL)
            }
            Sysno::fallocate => Err(Errno::EOPNOTSUPP),
            _ => Err(Errno::EINVAL),
        })
    }

    /// Linux's answer to `sendfile` or `splice` into a placeholder opened
    /// for writing (`writable`) or with access mode 3, in the order Linux
    /// checks (measured on Linux 7.1 as root against `/proc/self/cgroup`
    /// opened `O_RDWR`). The source descriptor's state is the kernel's
    /// answer (`F_GETFL`, `fstat`), not Detcore's record of it, which a
    /// received descriptor need not have.
    ///
    /// `sendfile(out, in, offset, count)`: an unreadable `offset` is EFAULT;
    /// a source that is not open, is `O_PATH`, or cannot read is EBADF; an
    /// offset for a pipe or socket source is ESPIPE; a negative offset is
    /// EINVAL; then a destination that cannot write is EBADF and an
    /// `O_APPEND` one EINVAL; a count of 0 transfers nothing (0); anything
    /// else is the EINVAL of a file with no `splice_write`.
    ///
    /// `splice(in, off_in, out, off_out, len, flags)`: a length of 0 is 0
    /// before anything is looked at; unknown flags are EINVAL; a source that
    /// is not open or is `O_PATH` is EBADF; an `off_in` for a pipe source is
    /// ESPIPE; an unreadable `off_in` or `off_out` is EFAULT; a source that
    /// cannot read, or a destination that cannot write, is EBADF; anything
    /// else is EINVAL (no `splice_write`, or no pipe on either side).
    async fn answer_rendered_procfs_transfer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        args: &reverie::syscalls::SyscallArgs,
        writable: bool,
        appends: bool,
    ) -> Result<i64, Errno> {
        const SPLICE_F_ALL: usize = 0xf;
        fn offset_at<M: MemoryAccess>(memory: &M, address: usize) -> Result<i64, Errno> {
            let address = reverie::syscalls::Addr::<i64>::from_raw(address).ok_or(Errno::EFAULT)?;
            memory.read_value(address).map_err(|_| Errno::EFAULT)
        }
        let (source, length) = match sysno {
            Sysno::sendfile => (args.arg1 as RawFd, args.arg3),
            _ => (args.arg0 as RawFd, args.arg4),
        };
        if sysno == Sysno::splice {
            if length == 0 {
                return Ok(0);
            }
            if args.arg5 & !SPLICE_F_ALL != 0 {
                return Err(Errno::EINVAL);
            }
        }
        let offset = if sysno == Sysno::sendfile && args.arg2 != 0 {
            Some(offset_at(&guest.memory(), args.arg2)?)
        } else {
            None
        };
        let source_status = guest
            .inject(
                syscalls::Fcntl::new()
                    .with_fd(source)
                    .with_cmd(syscalls::FcntlCmd::F_GETFL),
            )
            .await
            .map_err(|_| Errno::EBADF)? as i32;
        if source_status & libc::O_PATH != 0 {
            return Err(Errno::EBADF);
        }
        let source_reads = matches!(
            source_status & libc::O_ACCMODE,
            libc::O_RDONLY | libc::O_RDWR
        );
        // The source's file type, which decides an offset's ESPIPE: a pipe
        // or FIFO has no position for either call, and a socket none for
        // `sendfile` (no `FMODE_PREAD`).
        let source_type =
            |stat: Result<libc::stat, Errno>| stat.map_or(0, |stat| stat.st_mode & libc::S_IFMT);
        if sysno == Sysno::sendfile {
            if !source_reads {
                return Err(Errno::EBADF);
            }
            if let Some(offset) = offset {
                let kind = source_type(self.inject_fstat(guest, source).await);
                if matches!(kind, libc::S_IFIFO | libc::S_IFSOCK) {
                    return Err(Errno::ESPIPE);
                }
                if offset < 0 {
                    return Err(Errno::EINVAL);
                }
            }
            if !writable {
                return Err(Errno::EBADF);
            }
            if appends {
                return Err(Errno::EINVAL);
            }
            return if length == 0 {
                Ok(0)
            } else {
                Err(Errno::EINVAL)
            };
        }
        // `splice`.
        let (off_in, off_out) = (args.arg1, args.arg3);
        if off_in != 0 && source_type(self.inject_fstat(guest, source).await) == libc::S_IFIFO {
            return Err(Errno::ESPIPE);
        }
        if off_in != 0 {
            offset_at(&guest.memory(), off_in)?;
        }
        if off_out != 0 {
            offset_at(&guest.memory(), off_out)?;
        }
        if !source_reads || !writable {
            return Err(Errno::EBADF);
        }
        Err(Errno::EINVAL)
    }

    /// Record the auxiliary vector the calling process's new image finds on
    /// its initial stack, before its first instruction, for its
    /// `/proc/<pid>/auxv` (`ProcessAuxvTable`): the pairs past `argc`, the
    /// argument and environment pointers, read through guest memory at the
    /// stack pointer the image starts with, as the backend reports it
    /// (`Guest::initial_stack_pointer`; `procfs::initial_stack_auxv`, which
    /// refuses an implausible vector). They are what the guest's own
    /// `getauxval` reads: a backend that gives the guest the canonical values
    /// of the CPU and kernel entries (ptrace's `canonicalize_new_image`, KVM's
    /// loader) has written them there before this callback, and Detcore takes
    /// them as they are, never from the backend's `Guest::auxv`, which is
    /// sorted and, on ptrace, the kernel's own file.
    ///
    /// A backend that reports no initial stack pointer (DBT, SaBRe, LiteInst),
    /// or a vector that fails the check, records nothing and drops the
    /// process's stale vector: the image's file is left to the kernel beneath
    /// (the host kernel's file on ptrace and DBT, the provider's EACCES
    /// backstop on KVM), and the run warns of each reason once, by name
    /// (`tool_global::auxv_from_kernel`).
    ///
    /// The same stack gives the image's argument and environment areas
    /// (`procfs::initial_stack_cmdline_area`), which the process's rendered
    /// `/proc/<pid>/cmdline` reads from its memory
    /// ([`Self::read_rendered_cmdline`]); without them, the same backends
    /// leave that file to the kernel beneath too. Both go to the global tool
    /// in the one request this callback already makes.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#4213): Review the auxiliary vector Detcore takes from
    // each new image's initial stack, which its rendered /proc/<pid>/auxv
    // serves in place of the kernel's saved_auxv.
    pub(crate) async fn record_process_auxv<G: Guest<Self>>(&self, guest: &mut G) {
        use crate::tool_global::AuxvFromKernel;
        // Only a stack pointer the backend observed at the image's first
        // instruction: post-exec registers need not retain it on every
        // backend. DBT's first-entry admission still does not supply this
        // image-owned hook. Without one, or with an implausible vector there,
        // the file is left to the kernel beneath:
        // the host kernel's file on ptrace and DBT, the provider's refusal on
        // KVM.
        let (found, cmdline) = match guest.initial_stack_pointer() {
            Some(stack) => {
                let memory = guest.memory();
                let read_word = |at: u64| {
                    let address =
                        reverie::syscalls::Addr::<u64>::from_raw(usize::try_from(at).ok()?)?;
                    memory.read_value(address).ok()
                };
                (
                    crate::procfs::initial_stack_auxv(stack, read_word)
                        .ok_or(AuxvFromKernel::Implausible),
                    crate::procfs::initial_stack_cmdline_area(stack, read_word, |at, buffer| {
                        read_prefix(&memory, at, buffer)
                    }),
                )
            }
            None => (Err(AuxvFromKernel::NoInitialStackPointer), None),
        };
        let bytes = match found {
            Ok(bytes) => bytes,
            Err(why) => {
                // The process's vector from before the exec (its own earlier
                // image's, or its creator's) is stale: the global tool drops
                // it (`ProcessAuxvTable::clear`). The run says once, by name,
                // what the guest sees instead; the global tool's per-run flag
                // decides the first, so a second run in one process warns too.
                if crate::tool_global::auxv_from_kernel(guest, why, cmdline).await {
                    match why {
                        AuxvFromKernel::NoInitialStackPointer => warn!(
                            "auxv: backend gives no initial stack pointer; guest auxv is the \
                             kernel beneath's"
                        ),
                        AuxvFromKernel::Implausible => warn!(
                            "auxv: the initial stack did not hold a plausible auxiliary vector; \
                             guest auxv is the kernel beneath's"
                        ),
                    }
                }
                return;
            }
        };
        crate::tool_global::record_process_auxv(guest, bytes, cmdline).await;
    }

    /// Adopt, before `call` runs, each descriptor it names that Detcore has
    /// no record of and that is a placeholder this tool process made: a
    /// rendered procfs descriptor that reached this table by a route Detcore
    /// does not model, `SCM_RIGHTS` (where the tool does not discover
    /// descriptors as they are used: ptrace and KVM; an in-guest backend
    /// discovers it first, `discover_fd_from_current_process`). Then every
    /// call through it (reads, the cursor, a mapping, the write family, the
    /// task's reap) is answered as for a descriptor Detcore opened, and none
    /// reaches the memfd.
    ///
    /// The descriptor shares the sender's `struct file`, so it is an alias of
    /// the sender's open file description while Detcore still models that
    /// description (`PLACEHOLDER_OPENS`): one cursor, one snapshot, one set
    /// of status flags, exactly as Linux shares them. Once no modeled
    /// descriptor refers to it (the sender closed its own), the description
    /// is rebuilt from what the kernel keeps of it, as an in-guest backend
    /// rebuilds a placeholder it rediscovers (`adopt_placeholder`): the file
    /// and task its identity names, the status flags (`F_GETFL`), the cursor
    /// Detcore keeps at the kernel position (`sync_rendered_procfs_position`),
    /// and the snapshot its name carries; a file whose snapshot is taken at
    /// its first read takes it then (`initialize_procfs_snapshot`).
    ///
    /// Only once this tool process has made a placeholder, and only for a
    /// descriptor Detcore has no record of: one injected `fstat`, for each
    /// such descriptor a listed call names, every time (no cache: a
    /// descriptor number's identity can only be learned by asking).
    // TODO-HUMAN-REVIEW(#4213): Review the adoption of rendered descriptors
    // received over SCM_RIGHTS: the alias of a modeled description, the
    // rebuild of one no longer modeled, and the refusal without
    // sequentialized threads.
    pub(crate) async fn adopt_received_placeholders<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Result<(), Error> {
        if !placeholders_exist() || !self.renders_procfs(guest) {
            return Ok(());
        }
        let (sysno, args) = call.into_parts();
        for fd in descriptor_arguments(&guest.memory(), sysno, &args) {
            if fd < 0 || guest.thread_state().with_detfd(fd, |_| ()).is_ok() {
                continue;
            }
            info!(
                "Descriptor {fd}, which Detcore has no record of, is checked for a rendered \
                 procfs placeholder."
            );
            let Ok(stat) = self.inject_fstat(guest, fd).await else {
                continue;
            };
            // A placeholder is an empty regular file with no link, as the
            // in-guest discovery requires too (`discover_fd_from_current_process`).
            // Its identity is its memfd's (dev, ino) on the kernel's internal
            // shmem mount, which numbers inodes from the superblock's 64-bit
            // `next_ino` in per-CPU batches, without the 32-bit wrap of a
            // user-visible tmpfs (`shmem_reserve_inode` for an
            // `SB_KERNMOUNT`, Linux 5.9 and later): ASSUMED here, a number
            // that named a placeholder this run is not given to another
            // memfd, so the registry, which is never pruned, names no other
            // file.
            let may_be_placeholder = stat.st_mode & libc::S_IFMT == libc::S_IFREG
                && stat.st_nlink == 0
                && stat.st_size == 0;
            if !may_be_placeholder {
                continue;
            }
            let identity = (stat.st_dev, stat.st_ino);
            let Some(path) = placeholder_identity(identity.0, identity.1) else {
                continue;
            };
            // Without sequentialized threads (a declared relaxation) another
            // thread may close the descriptor, or `dup2` another file over
            // it, between the `fstat` above and the adoption, which would
            // then install a stale description. Refused by name instead,
            // through the unsupported-operation policy: a fail-closed run
            // stops with the reason, and --allow-unsupported-syscalls gives
            // the call EOPNOTSUPP. Never a silent memfd effect.
            if !guest.config().sequentialize_threads {
                let reason = format!(
                    "descriptor {fd}, a rendered procfs file Detcore has no record of \
                     (received over SCM_RIGHTS), cannot be adopted without sequentialized \
                     threads (--no-sequentialize-threads)"
                );
                // Named where the run's own output shows it: a backend whose
                // tool runs in Hermit's process prints no stored reason.
                error!("Detcore refuses {sysno}: {reason}");
                return crate::syscalls::helpers::refuse_unserviceable_operation_with(
                    guest,
                    self.cfg.panic_on_unsupported_syscalls,
                    sysno,
                    Errno::EOPNOTSUPP,
                    Some(&reason),
                )
                .await
                .map(|_| ());
            }
            self.adopt_received_placeholder(guest, fd, path, identity)
                .await?;
        }
        Ok(())
    }

    /// Make descriptor `fd`, which Detcore has no record of, the placeholder
    /// `identity` for the rendered file `path` it is
    /// ([`Self::adopt_received_placeholders`]).
    async fn adopt_received_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        path: &'static Path,
        identity: (u64, u64),
    ) -> Result<(), Error> {
        let descriptor_flags = guest
            .inject(
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(syscalls::FcntlCmd::F_GETFD),
            )
            .await?;
        let cloexec = descriptor_flags & i64::from(libc::FD_CLOEXEC) != 0;
        let registered = placeholder_open(identity);
        if let Some(alias) = registered
            .as_ref()
            .and_then(|(description, _)| description.descriptor(fd, cloexec))
        {
            guest.thread_state().install_alias(alias);
            return Ok(());
        }
        let status = guest
            .inject(
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(syscalls::FcntlCmd::F_GETFL),
            )
            .await? as i32;
        // An `O_PATH` descriptor has no position (EBADF).
        let offset = guest
            .inject(
                syscalls::Lseek::new()
                    .with_fd(fd)
                    .with_offset(0)
                    .with_whence(syscalls::Whence::SEEK_CUR),
            )
            .await
            .map_or(0, |offset| offset as usize);
        // OBLIGATION for every rendered file added after `cgroup`: this
        // rebuilt description has the snapshot the placeholder's name
        // carries, and no other. A file whose snapshot its first read takes
        // takes it again at its next read here; at position 0 that is
        // Linux's `show` running again, exact, but at a position above 0
        // Linux reads on from the buffer the first read filled in another
        // process. That is exact only while every snapshot of the file is
        // the same, as `cgroup`'s template is. A per-process file whose bytes
        // change (`status`, `stat`) must carry its first read's snapshot on
        // its placeholder (as `/proc/uptime` carries its first read's uptime,
        // `first_read_uptime`) before it is rendered, or its rebuilt
        // descriptor reads other bytes than the sender's buffer held.
        let snapshot = registered.map(|(_, snapshot)| snapshot).unwrap_or_default();
        let metadata = self.rendered_metadata(guest, path).await?;
        let mut flags = OFlag::from_bits_truncate(status);
        flags.set(OFlag::O_CLOEXEC, cloexec);
        guest
            .thread_state()
            .add_fd(fd, flags, FdType::Regular, Some(metadata))?;
        let procfs = ProcfsFile::from_rendered_path(path)
            .expect("a placeholder's path is a rendered procfs file")
            .into_rendered();
        let procfs = if snapshot.is_empty() {
            procfs
        } else {
            procfs.with_rendered_snapshot(snapshot.clone())
        };
        let handle = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_path(path);
            detfd.set_procfs(procfs.clone());
            detfd.set_procfs_offset(offset);
            detfd.open_file_handle()
        })?;
        register_placeholder_open(identity, handle, snapshot);
        Ok(())
    }

    /// Linux's answer to `splice` from a rendered `/proc/<pid>/cmdline`
    /// (`fd_in`, `args.arg0`), which must not read the empty placeholder
    /// standing in for it, in the order Linux 6.17 checks (fs/splice.c): a
    /// length of 0 is 0 and unknown flags are EINVAL (`SYSCALL_DEFINE6(splice)`,
    /// splice.c:1622-1626); a source or destination that is not open or is
    /// `O_PATH` is EBADF (splice.c:1628-1634, `fdget` skips `O_PATH`); a
    /// destination pipe that splice can use (not a notification pipe:
    /// [`Self::splice_pipe`]) with an `off_out` is ESPIPE (`__do_splice`,
    /// splice.c:1416-1418); an unreadable `off_out` or `off_in` is EFAULT
    /// (splice.c:1422-1431); a source that cannot read or a destination that
    /// cannot write is EBADF (`do_splice`, splice.c:1310-1312); and then,
    /// because `proc_pid_cmdline_ops` has no `splice_read`, EINVAL: at
    /// splice.c:1383 for a destination that is not a pipe, and from
    /// `do_splice_read` through `splice_file_to_pipe` (splice.c:1376) for a
    /// pipe. The destination's state is the kernel's answer (`F_GETFL`,
    /// `fstat`, the watch-queue probe), as for a transfer into a
    /// placeholder.
    ///
    /// Not modelled, declared: before that last EINVAL a destination pipe
    /// with no reader gives SIGPIPE and EPIPE, and a full one EAGAIN when
    /// nonblocking or a wait for space (`wait_for_space`, splice.c:1261). By
    /// default Detcore refuses `splice` with EINVAL for every other file; this
    /// answer stands with and without `--allow-unsupported-syscalls`. `None`
    /// for any other source.
    async fn answer_cmdline_splice<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: &reverie::syscalls::SyscallArgs,
    ) -> Option<Result<i64, Error>> {
        let status = guest
            .thread_state()
            .with_detfd(args.arg0 as RawFd, |detfd| {
                detfd
                    .procfs_reads_memory_at_every_read()
                    .then(|| detfd.status_flags())
            })
            .ok()
            .flatten()?;
        Some(self.cmdline_splice_answer(guest, args, status).await)
    }

    /// [`Self::answer_cmdline_splice`]'s answer for a source whose status
    /// flags are `status`. The destination's state comes from the kernel by
    /// injected probes, each retried while Reverie reports it did not run
    /// (ERESTARTSYS) or it was interrupted (EINTR): EBADF only where the
    /// kernel says the descriptor is not open, and any other probe failure
    /// the named refusal of the failure policy, never an errno Detcore made
    /// up.
    async fn cmdline_splice_answer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: &reverie::syscalls::SyscallArgs,
        status: i32,
    ) -> Result<i64, Error> {
        const SPLICE_F_ALL: usize = 0xf;
        fn readable_offset<M: MemoryAccess>(memory: &M, address: usize) -> Result<(), Errno> {
            let address = reverie::syscalls::Addr::<i64>::from_raw(address).ok_or(Errno::EFAULT)?;
            memory
                .read_value(address)
                .map(|_| ())
                .map_err(|_| Errno::EFAULT)
        }
        let (off_in, out, off_out) = (args.arg1, args.arg2 as RawFd, args.arg3);
        if args.arg4 == 0 {
            return Ok(0);
        }
        if args.arg5 & !SPLICE_F_ALL != 0 {
            return Err(Errno::EINVAL.into());
        }
        if status & libc::O_PATH != 0 {
            return Err(Errno::EBADF.into());
        }
        let out_status = loop {
            match guest
                .inject(
                    syscalls::Fcntl::new()
                        .with_fd(out)
                        .with_cmd(syscalls::FcntlCmd::F_GETFL),
                )
                .await
            {
                Err(Errno::ERESTARTSYS | Errno::EINTR) => continue,
                Ok(flags) => break flags as i32,
                Err(Errno::EBADF) => return Err(Errno::EBADF.into()),
                Err(errno) => {
                    return Err(Self::scratch_failure(
                        guest,
                        Sysno::splice,
                        format!(
                            "the status flags of splice's destination {out} could not be read: \
                             {errno}"
                        ),
                    )
                    .await);
                }
            }
        };
        if out_status & libc::O_PATH != 0 {
            return Err(Errno::EBADF.into());
        }
        if off_out != 0 && self.splice_pipe(guest, out).await? {
            return Err(Errno::ESPIPE.into());
        }
        if off_out != 0 {
            readable_offset(&guest.memory(), off_out)?;
        }
        if off_in != 0 {
            readable_offset(&guest.memory(), off_in)?;
        }
        let reads = matches!(status & libc::O_ACCMODE, libc::O_RDONLY | libc::O_RDWR);
        let writes = matches!(out_status & libc::O_ACCMODE, libc::O_WRONLY | libc::O_RDWR);
        if !reads || !writes {
            return Err(Errno::EBADF.into());
        }
        Err(Errno::EINVAL.into())
    }

    /// Whether the open descriptor `fd` is a pipe that splice can use,
    /// Linux's `get_pipe_info(file, true)` (fs/pipe.c:1413-1424): a pipe or
    /// FIFO (the kernel's `fstat`), but not a notification pipe
    /// (`pipe2(O_NOTIFICATION_PIPE)`, which has a watch queue). The watch
    /// queue is told by `ioctl(IOC_WATCH_QUEUE_SET_SIZE, 0)`, which changes
    /// nothing whatever it answers (`watch_queue_set_size`,
    /// kernel/watch_queue.c:237-252): ENODEV for a pipe without one, EBUSY or
    /// EINVAL for one with one, and ENOTTY from a kernel without watch
    /// queues, where no pipe has one. Each probe is retried as in
    /// [`Self::cmdline_splice_answer`]; any other failure, which leaves the
    /// pipe unclassified, is the named refusal of the failure policy.
    async fn splice_pipe<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) -> Result<bool, Error> {
        const IOC_WATCH_QUEUE_SET_SIZE: usize = 0x5760; // _IO('W', 0x60)
        let stat = loop {
            match self.inject_fstat(guest, fd).await {
                Err(Errno::ERESTARTSYS | Errno::EINTR) => continue,
                stat => break stat,
            }
        };
        let stat = match stat {
            Ok(stat) => stat,
            Err(errno) => {
                return Err(Self::scratch_failure(
                    guest,
                    Sysno::splice,
                    format!(
                        "the file type of splice's destination {fd} could not be read: {errno}"
                    ),
                )
                .await);
            }
        };
        if stat.st_mode & libc::S_IFMT != libc::S_IFIFO {
            return Ok(false);
        }
        let probe = loop {
            let call = Syscall::from_raw(
                Sysno::ioctl,
                reverie::syscalls::SyscallArgs::new(
                    fd as usize,
                    IOC_WATCH_QUEUE_SET_SIZE,
                    0,
                    0,
                    0,
                    0,
                ),
            );
            match guest.inject(call).await {
                Err(Errno::ERESTARTSYS | Errno::EINTR) => continue,
                probe => break probe,
            }
        };
        match splice_pipe_from_watch_queue_probe(probe) {
            Some(usable) => Ok(usable),
            None => Err(Self::scratch_failure(
                guest,
                Sysno::splice,
                format!(
                    "splice's destination pipe {fd} could not be classified as a notification \
                     pipe or not: the watch-queue probe answered {probe:?}"
                ),
            )
            .await),
        }
    }

    /// Answer `call` if it must not reach the placeholder behind a rendered
    /// procfs descriptor: what a procfs file refuses, in the order Linux
    /// checks. A read-only placeholder's description already makes the kernel
    /// refuse writes and size changes, and an `O_PATH` one every call but
    /// `fstat`, which Detcore answers. A per-process file opened for writing
    /// (`rendered_open_flags`) has a writable placeholder, so Detcore answers
    /// every call that would write it ([`Self::answer_rendered_procfs_write`]):
    /// none reaches the memfd, which KVM, without seals, would let change.
    pub(crate) async fn answer_rendered_procfs_operation<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Option<Result<i64, Error>> {
        let (sysno, args) = call.into_parts();
        if let Some(answer) = self.answer_rendered_procfs_write(guest, sysno, &args).await {
            return Some(answer.map_err(Error::from));
        }
        if sysno == Sysno::splice
            && let Some(answer) = self.answer_cmdline_splice(guest, &args).await
        {
            return Some(answer);
        }
        let fd = match sysno {
            Sysno::pwrite64
            | Sysno::pwritev
            | Sysno::pwritev2
            | Sysno::fsync
            | Sysno::fdatasync
            | Sysno::fchmod
            | Sysno::fgetxattr
            | Sysno::flistxattr
            | Sysno::fsetxattr
            | Sysno::fremovexattr
            | Sysno::ioctl
            | Sysno::fcntl => args.arg0 as i32,
            _ => return None,
        };
        let (path, status_flags) = guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                detfd
                    .procfs_rendered_path()
                    .map(|path| (path, detfd.status_flags()))
            })
            .ok()
            .flatten()?;
        if status_flags & libc::O_PATH != 0 {
            return None;
        }
        let seq_file = crate::procfs::rendered_path_is_seq_file(path);
        let writable = matches!(
            status_flags & libc::O_ACCMODE,
            libc::O_WRONLY | libc::O_RDWR
        );
        let per_process = crate::procfs::rendered_path_is_per_process(path);
        // Who owns the file matters only to the calls below that ask for the
        // owner or for write access, and only an attribute change asks for
        // the caller's capabilities; no other call pays for either.
        let command = args.arg1 as libc::c_int;
        let needs_access = matches!(sysno, Sysno::fsetxattr | Sysno::fremovexattr)
            || (sysno == Sysno::fcntl
                && (matches!(command, libc::F_SETLEASE | 1036)
                    || (command == libc::F_SETFL
                        && args.arg2 as libc::c_int & libc::O_NOATIME != 0)));
        let access = if needs_access {
            match self
                .rendered_file_access(
                    guest,
                    path,
                    matches!(sysno, Sysno::fsetxattr | Sysno::fremovexattr),
                    CapabilitySet::Effective,
                )
                .await
            {
                Ok(access) => access,
                Err(error) => return Some(Err(error)),
            }
        } else {
            FileAccess::GLOBAL_ROOT
        };
        let answer = match call {
            // A sysctl file's placeholder refuses a positioned write as the
            // sysctl does: it is read-only (EBADF), or the position is
            // negative (EINVAL).
            // A process's `auxv` keeps its positioned writes (`FMODE_PWRITE`):
            // a negative position is EINVAL first (`pwritev2` at -1, which
            // writes at the file position, is `writev`'s EBADF on a
            // read-only description; a writable one is answered by
            // `answer_rendered_procfs_write`); then a description that cannot
            // write is EBADF, and one that can the EINVAL of a file with no
            // write method. Answered here, as its placeholder may be writable.
            Syscall::Pwrite64(_) | Syscall::Pwritev(_) | Syscall::Pwritev2(_)
                if !seq_file && per_process =>
            {
                let position = args.arg3 as i64;
                if sysno == Sysno::pwritev2 && position == -1 {
                    Err(Errno::EBADF)
                } else if position < 0 || writable {
                    Err(Errno::EINVAL)
                } else {
                    Err(Errno::EBADF)
                }
            }
            Syscall::Pwrite64(_) | Syscall::Pwritev(_) | Syscall::Pwritev2(_) if !seq_file => {
                return None;
            }
            Syscall::Pwrite64(_) | Syscall::Pwritev(_) | Syscall::Pwritev2(_) => {
                return rendered_seq_file_pwrite_answer(sysno, args.arg3 as i64)
                    .map(|answer| answer.map_err(Error::from));
            }
            Syscall::Fsync(_) | Syscall::Fdatasync(_) => Err(Errno::EINVAL),
            Syscall::Fchmod(_) => Err(Errno::EPERM),
            Syscall::Fgetxattr(c) => match c.name().map(|name| name.read(&guest.memory())) {
                // The description was opened by a caller that could read the
                // file, so read permission holds.
                Some(Ok(name)) => xattr_get_answer(name.as_bytes(), true),
                Some(Err(errno)) => Err(errno),
                None => Err(Errno::EFAULT),
            },
            Syscall::Flistxattr(_) => Ok(0),
            Syscall::Fsetxattr(c)
                if c.flags() & !(libc::XATTR_CREATE | libc::XATTR_REPLACE) != 0 =>
            {
                Err(Errno::EINVAL)
            }
            Syscall::Fsetxattr(c) => match c.name().map(|name| name.read(&guest.memory())) {
                Some(Ok(name)) => xattr_change_answer(
                    name.as_bytes(),
                    Some(c.size()),
                    read_value(&guest.memory(), args.arg2 as u64, c.size()).as_deref(),
                    access,
                ),
                Some(Err(errno)) => Err(errno),
                None => Err(Errno::EFAULT),
            },
            Syscall::Fremovexattr(c) => match c.name().map(|name| name.read(&guest.memory())) {
                Some(Ok(name)) => xattr_change_answer(name.as_bytes(), None, None, access),
                Some(Err(errno)) => Err(errno),
                None => Err(Errno::EFAULT),
            },
            Syscall::Ioctl(_) => match args.arg1 as u32 as usize {
                // The generic file ioctls Linux answers before the file's own:
                // FIBMAP and FIFREEZE/FITHAW need capabilities, FS_IOC_FIEMAP
                // a file system that maps extents, and FICLONE/FICLONERANGE a
                // source on the same file system.
                1 | 0xc004_5877 | 0xc004_5878 => Err(Errno::EPERM),
                0xc020_660b => Err(Errno::EOPNOTSUPP),
                0x4004_9409 => Self::clone_ioctl_answer(guest, args.arg2 as i32),
                0x4020_940d => {
                    // The whole `struct file_clone_range` is read first.
                    match reverie::syscalls::Addr::<[u64; 4]>::from_raw(args.arg2)
                        .map(|address| guest.memory().read_value(address))
                    {
                        Some(Ok(range)) => Self::clone_ioctl_answer(guest, range[0] as i32),
                        _ => Err(Errno::EFAULT),
                    }
                }
                // FIONCLEX, FIOCLEX, FIOASYNC and FIONBIO, which the kernel
                // answers alike for both files.
                0x5450..=0x5452 | 0x5421 => return None,
                // FIONREAD: the file's size, 0, less the position, as an int.
                // Where the tool runs in each guest process, the kernel answers
                // from the empty placeholder's position, which Detcore keeps
                // current and which processes sharing the open file share.
                // Elsewhere Detcore answers from its own cursor, shared there
                // too (KVM's executor has no answer for a memfd).
                0x541b if self.cfg.backend.tool_shares_guest_descriptor_table => return None,
                0x541b => match AddrMut::<libc::c_int>::from_raw(args.arg2) {
                    Some(address) => {
                        let position = guest
                            .thread_state()
                            .with_detfd(args.arg0 as RawFd, |detfd| {
                                detfd.procfs_position().map_or(0, |(offset, _)| offset)
                            })
                            .unwrap_or(0);
                        let unread = 0_i64.wrapping_sub(position as i64) as libc::c_int;
                        guest.memory().write_value(address, &unread).map(|()| 0)
                    }
                    None => Err(Errno::EFAULT),
                },
                // FIOQSIZE: the bytes the file uses, none for procfs.
                0x5460 => match AddrMut::<libc::loff_t>::from_raw(args.arg2) {
                    Some(address) => guest.memory().write_value(address, &0).map(|()| 0),
                    None => Err(Errno::EFAULT),
                },
                // FIGETBSZ: procfs's block size.
                2 => match AddrMut::<libc::c_int>::from_raw(args.arg2) {
                    Some(address) => guest
                        .memory()
                        .write_value(address, &(procfs_inode::PROC_BLOCK_SIZE as libc::c_int))
                        .map(|()| 0),
                    None => Err(Errno::EFAULT),
                },
                _ => Err(Errno::ENOTTY),
            },
            Syscall::Fcntl(_) => match args.arg1 as libc::c_int {
                // Adding seals needs a writable descriptor, before Linux
                // looks for seals the file cannot have.
                libc::F_ADD_SEALS if !writable => Err(Errno::EPERM),
                libc::F_ADD_SEALS | libc::F_GET_SEALS => Err(Errno::EINVAL),
                // A lease needs the file's owner, so a system-wide file never
                // has one; its owner may ask, and procfs takes none.
                libc::F_SETLEASE if access.owner => Err(Errno::EINVAL),
                libc::F_SETLEASE => Err(Errno::EACCES),
                // F_SET_RW_HINT needs the file's owner, then reads and checks
                // the hint (`rw_hint_valid`), which procfs keeps on the inode.
                // Detcore keeps it, for every descriptor and reopening of a
                // per-process file (`RenderedFileState`).
                1036 if access.owner => {
                    match Addr::<u64>::from_raw(args.arg2)
                        .map(|hint| guest.memory().read_value(hint))
                    {
                        Some(Ok(hint)) if hint <= 5 => {
                            crate::tool_global::rendered_file_state(
                                guest,
                                procfs_inode::rendered_inode(path),
                                crate::procfs::listing::RenderedFileChange::WriteHint(hint),
                            )
                            .await;
                            Ok(0)
                        }
                        Some(Ok(_)) => Err(Errno::EINVAL),
                        _ => Err(Errno::EFAULT),
                    }
                }
                1036 => Err(Errno::EPERM),
                // F_GET_RW_HINT: the hint the inode keeps, which any
                // descriptor reads. A system-wide file's is never set, and its
                // placeholder answers 0 as the file does.
                1035 if per_process => {
                    let state = crate::tool_global::rendered_file_state(
                        guest,
                        procfs_inode::rendered_inode(path),
                        crate::procfs::listing::RenderedFileChange::Read,
                    )
                    .await;
                    match AddrMut::<u64>::from_raw(args.arg2) {
                        Some(address) => guest
                            .memory()
                            .write_value(address, &state.write_hint)
                            .map(|()| 0),
                        None => Err(Errno::EFAULT),
                    }
                }
                libc::F_GETLEASE => Ok(i64::from(libc::F_UNLCK)),
                // `O_NOATIME` needs the file's owner (the owner's reaches the
                // placeholder, which the guest owns too), and procfs files
                // cannot do direct I/O.
                libc::F_SETFL
                    if args.arg2 as libc::c_int & libc::O_NOATIME != 0 && !access.owner =>
                {
                    Err(Errno::EPERM)
                }
                libc::F_SETFL if args.arg2 as libc::c_int & libc::O_DIRECT != 0 => {
                    Err(Errno::EINVAL)
                }
                _ => return None,
            },
            _ => return None,
        };
        Some(answer.map_err(Into::into))
    }
}

/// What a path call asks of the file it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathRequest {
    /// `access`: `mode`, with `faccessat2`'s `flags`.
    Access {
        mode: u64,
        flags: u64,
    },
    /// `getxattr`, whose attribute name the answer reads.
    GetXattr,
    ListXattr,
    /// `setxattr` with the value's size, the value at guest address `value`,
    /// and its flags; or `removexattr`.
    ChangeXattr {
        size: Option<usize>,
        value: u64,
        flags: u64,
    },
    Chmod {
        flags: u64,
    },
    Truncate {
        length: i64,
    },
    /// Setting times: `None` for the current time, as when `times` is NULL.
    Times {
        explicit: Option<bool>,
    },
    Chdir,
    /// Arguments Linux refuses before it looks at the file.
    Invalid,
    /// Arguments Linux cannot read.
    Fault,
}

/// Linux's answer to `request` for a rendered procfs file (0444, or 0644 for
/// a fixed tunable: only global root may write either), owned and overridden
/// as `access` says, given the attribute `name` when it changes one.
fn path_request_answer(
    request: PathRequest,
    name: Option<&[u8]>,
    value: Option<&[u8]>,
    access: FileAccess,
) -> Result<i64, Errno> {
    match request {
        PathRequest::Access { mode, flags } => {
            if access_arguments_invalid(mode, flags) {
                Err(Errno::EINVAL)
            } else if mode & libc::X_OK as u64 != 0
                || (mode & libc::W_OK as u64 != 0 && !access.may_write())
                || (mode & libc::R_OK as u64 != 0 && !access.readable)
            {
                // No execute bit, which even CAP_DAC_OVERRIDE needs one of.
                Err(Errno::EACCES)
            } else {
                Ok(0)
            }
        }
        PathRequest::GetXattr => xattr_get_answer(name.ok_or(Errno::EFAULT)?, access.readable),
        PathRequest::ListXattr => Ok(0),
        PathRequest::ChangeXattr { size, flags, .. } => {
            if flags & !(libc::XATTR_CREATE | libc::XATTR_REPLACE) as u64 != 0 {
                return Err(Errno::EINVAL);
            }
            xattr_change_answer(name.ok_or(Errno::EFAULT)?, size, value, access)
        }
        PathRequest::Chmod { flags } => {
            if flags & !(libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) as u64 != 0 {
                Err(Errno::EINVAL)
            } else {
                Err(Errno::EPERM)
            }
        }
        PathRequest::Truncate { length } if length < 0 => Err(Errno::EINVAL),
        // procfs's `proc_setattr` accepts a size and keeps none.
        PathRequest::Truncate { .. } if access.may_write() => Ok(0),
        PathRequest::Truncate { .. } => Err(Errno::EACCES),
        // Leaving both times alone changes nothing; the current time needs
        // write permission or the owner, and any other time the owner.
        // (`proc_setattr` keeps the owner's times on the inode until Linux
        // drops it; Detcore's metadata of the file keeps the epoch.)
        PathRequest::Times { explicit: None } if access.owner || access.may_write() => Ok(0),
        PathRequest::Times { explicit: None } => Err(Errno::EACCES),
        PathRequest::Times {
            explicit: Some(false),
        } => Ok(0),
        PathRequest::Times {
            explicit: Some(true),
        } if access.owner => Ok(0),
        PathRequest::Times {
            explicit: Some(true),
        } => Err(Errno::EPERM),
        PathRequest::Chdir => Err(Errno::ENOTDIR),
        PathRequest::Invalid => Err(Errno::EINVAL),
        PathRequest::Fault => Err(Errno::EFAULT),
    }
}

/// How `utimensat`'s `times` asks to change a file's times: `None` for the
/// current time (NULL, or both `UTIME_NOW`), `Some(false)` to change nothing
/// (both `UTIME_OMIT`), `Some(true)` for any other time, including one
/// `UTIME_NOW` with one `UTIME_OMIT`, which needs the owner.
fn utimens_request(times: Option<[libc::timespec; 2]>) -> Option<bool> {
    let times = times?;
    if times.iter().all(|time| time.tv_nsec == libc::UTIME_OMIT) {
        Some(false)
    } else if times.iter().all(|time| time.tv_nsec == libc::UTIME_NOW) {
        None
    } else {
        Some(true)
    }
}

/// Whether Linux refuses an access probe's mode or flags. It checks them
/// before it looks up the path (`do_faccessat`), with both already narrowed
/// to `int`.
fn access_arguments_invalid(mode: u64, flags: u64) -> bool {
    flags & !(libc::AT_EACCESS | libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) as u64 != 0
        || mode & !7 != 0
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Answer a path call (or an `AT_EMPTY_PATH`/NULL-path call on a
    /// descriptor) if it names a procfs file Detcore renders, as Linux
    /// answers for the procfs file: the backend has no such file, and the
    /// placeholder must not be changed or shown. A spelling Detcore does not
    /// resolve lexically that may be a descriptor link is resolved by the
    /// object it reaches.
    pub(crate) async fn answer_rendered_procfs_path_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Result<Option<i64>, Error> {
        // A rendered descriptor stays the file after a chroot, so only the
        // path spellings below depend on the root.
        if self.cfg.recordreplay_modes {
            return Ok(None);
        }
        let (sysno, args) = call.into_parts();
        let [a0, a1, a2, a3, a4] =
            [args.arg0, args.arg1, args.arg2, args.arg3, args.arg4].map(|arg| arg as u64);
        let at = libc::AT_FDCWD as u64;
        // (dirfd, path, flags, request, attribute name pointer)
        // For `futimens` on an `O_PATH` descriptor: whether the call is done
        // before the descriptor is looked up (both times omitted), or refused
        // first (unknown flags).
        let mut decided_before_descriptor = false;
        let (dirfd, path, flags, request, name) = match sysno {
            // Linux declares mode and flags `int`: it ignores the upper bits
            // of the raw registers.
            Sysno::access => (
                at,
                a0,
                0,
                PathRequest::Access {
                    mode: u64::from(a1 as u32),
                    flags: 0,
                },
                0,
            ),
            Sysno::faccessat => (
                a0,
                a1,
                0,
                PathRequest::Access {
                    mode: u64::from(a2 as u32),
                    flags: 0,
                },
                0,
            ),
            Sysno::faccessat2 => (
                a0,
                a1,
                u64::from(a3 as u32),
                PathRequest::Access {
                    mode: u64::from(a2 as u32),
                    flags: u64::from(a3 as u32),
                },
                0,
            ),
            Sysno::getxattr => (at, a0, 0, PathRequest::GetXattr, a1),
            Sysno::lgetxattr => (at, a0, nofollow(), PathRequest::GetXattr, a1),
            Sysno::listxattr => (at, a0, 0, PathRequest::ListXattr, 0),
            Sysno::llistxattr => (at, a0, nofollow(), PathRequest::ListXattr, 0),
            Sysno::setxattr | Sysno::lsetxattr => (
                at,
                a0,
                if sysno == Sysno::lsetxattr {
                    nofollow()
                } else {
                    0
                },
                PathRequest::ChangeXattr {
                    size: Some(a3 as usize),
                    value: a2,
                    // Linux declares the flags `int`.
                    flags: u64::from(a4 as u32),
                },
                a1,
            ),
            Sysno::removexattr | Sysno::lremovexattr => (
                at,
                a0,
                if sysno == Sysno::lremovexattr {
                    nofollow()
                } else {
                    0
                },
                PathRequest::ChangeXattr {
                    size: None,
                    value: 0,
                    flags: 0,
                },
                a1,
            ),
            Sysno::chmod => (at, a0, 0, PathRequest::Chmod { flags: 0 }, 0),
            Sysno::fchmodat => (a0, a1, 0, PathRequest::Chmod { flags: 0 }, 0),
            Sysno::fchmodat2 => (a0, a1, a3, PathRequest::Chmod { flags: a3 }, 0),
            // Ownership changes keep Detcore's policy for every file
            // (`handle_ownership_change_noop`).
            Sysno::truncate => (at, a0, 0, PathRequest::Truncate { length: a1 as i64 }, 0),
            Sysno::utimensat => {
                // Unreadable times are the kernel's EFAULT to report.
                let Ok(times) = read_pair::<libc::timespec, _>(&guest.memory(), a2) else {
                    return Ok(None);
                };
                // Both times omitted is success before anything is looked at;
                // unknown flags, or any flag with a NULL path, are refused
                // before the file is looked up; out-of-range times after it
                // (measured on Linux 7.1). Detcore answers the first two for
                // every file where it renders procfs files: Linux's answer
                // does not depend on the file.
                let omitted = times
                    .is_some_and(|times| times.iter().all(|time| time.tv_nsec == libc::UTIME_OMIT));
                // A NULL path with flags is refused only for a descriptor:
                // with AT_FDCWD, Linux looks the NULL path up (EFAULT).
                let flags_valid = a3 & !((libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) as u64)
                    == 0
                    && (a1 != 0 || a3 == 0 || a0 as i32 == libc::AT_FDCWD);
                if self.renders_procfs(guest) {
                    if omitted {
                        return Ok(Some(0));
                    }
                    if !flags_valid {
                        return Err(Errno::EINVAL.into());
                    }
                    // A NULL path with AT_FDCWD is looked up, and faults (KVM's
                    // executor answers otherwise).
                    if a1 == 0 && a0 as i32 == libc::AT_FDCWD {
                        return Err(Errno::EFAULT.into());
                    }
                }
                decided_before_descriptor = omitted || !flags_valid;
                let valid = omitted
                    || (flags_valid
                        && times.is_none_or(|times| times.iter().all(valid_utime_nsec)));
                (
                    a0,
                    a1,
                    a3,
                    if valid {
                        PathRequest::Times {
                            explicit: utimens_request(times),
                        }
                    } else {
                        PathRequest::Invalid
                    },
                    0,
                )
            }
            Sysno::utime => (
                at,
                a0,
                0,
                legacy_times_request(&guest.memory(), a1, false),
                0,
            ),
            Sysno::utimes => (
                at,
                a0,
                0,
                legacy_times_request(&guest.memory(), a1, true),
                0,
            ),
            // `futimesat` is ENOSYS under Detcore for every file.
            Sysno::chdir => (at, a0, 0, PathRequest::Chdir, 0),
            _ => return Ok(None),
        };
        // Linux refuses invalid access arguments before it looks up the path
        // or the descriptor, for every file; leave that order to the backend.
        if let PathRequest::Access { mode, flags } = request
            && access_arguments_invalid(mode, flags)
        {
            return Ok(None);
        }
        // Likewise invalid setxattr flags, before the name, the value or the
        // path, for every file (`setxattr_copy`). KVM's executor reads the
        // name first, so answer here rather than leave it to the backend.
        if let PathRequest::ChangeXattr { flags, .. } = request
            && flags & !(libc::XATTR_CREATE | libc::XATTR_REPLACE) as u64 != 0
            && self.renders_procfs(guest)
        {
            return Err(Errno::EINVAL.into());
        }
        let dirfd = dirfd as i32;
        let empty_path_allowed = flags & libc::AT_EMPTY_PATH as u64 != 0;
        // An unreadable path is the kernel's EFAULT to report.
        let path_text = match PathPtr::from_ptr(path as *const libc::c_char) {
            Some(path) => match path.read(&guest.memory()) {
                Ok(path) => Some(path),
                Err(_) => return Ok(None),
            },
            None => None,
        };
        // A descriptor named by an empty path (`AT_EMPTY_PATH`), or by none
        // (`futimens`).
        let by_descriptor = match &path_text {
            None => sysno == Sysno::utimensat,
            Some(path) => path.as_os_str().is_empty() && empty_path_allowed,
        };
        // Linux then copies and checks the attribute name, the size and the
        // value, still before it looks up the path (`setxattr_copy`,
        // `import_xattr_name`), so a path error cannot hide them. Detcore
        // checks them first where it may answer: a spelling that could name
        // a rendered file. Any other path goes to the kernel untouched.
        let session_files = self.rendered_files(guest);
        let xattr_arguments = if self.renders_procfs(guest)
            && (by_descriptor
                || path_text
                    .as_deref()
                    .is_some_and(|text| could_name_rendered(text, session_files)))
        {
            checked_xattr_arguments(&guest.memory(), request, name)?
        } else {
            None
        };
        let rendered = if by_descriptor {
            let known = guest
                .thread_state()
                .with_detfd(dirfd, |detfd| {
                    detfd
                        .procfs_rendered_path()
                        .map(|path| (path, detfd.status_flags() & libc::O_PATH != 0))
                })
                .ok()
                .flatten();
            // A placeholder Detcore did not record as one (an alias the kernel
            // opened) is known by its kernel identity, as a stat of it is: a
            // time set on it would change no file of the guest's and could be
            // read as an uptime a first read carried (`first_read_uptime`).
            let rendered = match known {
                Some(known) => Some(known),
                None if self.renders_procfs(guest) => {
                    match self.placeholder_descriptor(guest, dirfd).await {
                        Some(path) => {
                            let status = guest
                                .inject(
                                    syscalls::Fcntl::new()
                                        .with_fd(dirfd)
                                        .with_cmd(syscalls::FcntlCmd::F_GETFL),
                                )
                                .await
                                .unwrap_or(0);
                            Some((path, status & i64::from(libc::O_PATH) != 0))
                        }
                        None => None,
                    }
                }
                None => None,
            };
            // `futimens` needs a descriptor opened for I/O: an `O_PATH` one
            // is EBADF, after the flags are checked and before the times are
            // (measured on Linux 7.1).
            if let Some((_, true)) = rendered
                && path_text.is_none()
                && !decided_before_descriptor
            {
                return Err(Errno::EBADF.into());
            }
            rendered.map(|(path, _)| path)
        } else {
            let Some(text) = path_text else {
                return Ok(None);
            };
            if text.as_os_str().is_empty() || !self.renders_procfs(guest) {
                return Ok(None);
            }
            // The spellings Detcore resolves itself, which end in a rendered
            // file's name or a descriptor number.
            let files = self.rendered_files(guest);
            let lexical = if could_name_rendered(&text, files) {
                let observed = self.procfs_spelling_at(guest, dirfd, &text).await?;
                match self.rendered_procfs_at(guest, &observed).await? {
                    // A call that does not follow a descriptor link acts on
                    // the link itself, which the kernel has.
                    Some(target) if target.via_link && flags & nofollow() != 0 => return Ok(None),
                    Some(target) => target.procfs.rendered_path(files),
                    None => None,
                }
            } else {
                None
            };
            match lexical {
                Some(rendered) => Some(rendered),
                // Any other spelling (a symbolic link to a descriptor link,
                // `/dev/stdin`): what the kernel reaches, by its identity.
                // Only while this process holds a rendered descriptor, so a
                // process that holds none pays nothing.
                None => {
                    self.placeholder_reached(
                        guest,
                        dirfd,
                        PathPtr::from_ptr(path as *const libc::c_char),
                        flags & nofollow() == 0,
                    )
                    .await
                }
            }
        };
        if rendered.is_none() {
            return Ok(None);
        }
        // A spelling that reached a rendered file another way (a link) has
        // its attribute arguments checked here, after the path.
        let xattr_arguments = match xattr_arguments {
            Some(arguments) => Some(arguments),
            None => checked_xattr_arguments(&guest.memory(), request, name)?,
        };
        let (name, value) = match xattr_arguments {
            Some((name, value)) => (Some(name), value),
            None => (None, None),
        };
        let rendered = rendered.expect("checked above");
        // A request for write access (an access probe for it, a truncation,
        // an attribute change) asks for the caller's capabilities; `access`
        // and `faccessat` without `AT_EACCESS` check the real IDs' set.
        let needs_capabilities = match request {
            PathRequest::Access { mode, .. } => mode & libc::W_OK as u64 != 0,
            PathRequest::Truncate { .. } | PathRequest::ChangeXattr { .. } => true,
            _ => false,
        };
        let set = match request {
            PathRequest::Access { flags, .. } if flags & libc::AT_EACCESS as u64 == 0 => {
                CapabilitySet::Permitted
            }
            _ => CapabilitySet::Effective,
        };
        let access = self
            .rendered_file_access(guest, rendered, needs_capabilities, set)
            .await?;
        let answer = path_request_answer(request, name.as_deref(), value.as_deref(), access);
        // A time set on a per-process file stays on its inode (`proc_setattr`
        // keeps what `setattr_copy` copies), which every alias of the file
        // shows: Detcore keeps the modification time in its global tool. The
        // access and change times are the epoch for every rendered file, as
        // Detcore's metadata shows them for every file it virtualizes.
        if answer == Ok(0)
            && matches!(request, PathRequest::Times { .. })
            && access.owner
            && crate::procfs::per_process_name(rendered).is_some()
            && let Some((seconds, nanoseconds)) =
                self.requested_mtime(guest, sysno, [a0, a1, a2]).await
        {
            crate::tool_global::rendered_file_state(
                guest,
                procfs_inode::rendered_inode(rendered),
                crate::procfs::listing::RenderedFileChange::Mtime(seconds, nanoseconds),
            )
            .await;
        }
        answer.map(Some).map_err(Into::into)
    }

    /// The modification time a successful `utimensat`, `utime` or `utimes`
    /// with arguments `args` sets: the given time, or the virtual time now for
    /// a NULL `times` or `UTIME_NOW`; `None` where it leaves it alone
    /// (`UTIME_OMIT`) or its times cannot be read.
    async fn requested_mtime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        [_, a1, a2]: [u64; 3],
    ) -> Option<(i64, i64)> {
        let times = if sysno == Sysno::utimensat { a2 } else { a1 };
        let explicit = if times == 0 {
            None
        } else if sysno == Sysno::utimensat {
            let [_, mtime] = read_pair::<libc::timespec, _>(&guest.memory(), times)
                .ok()
                .flatten()?;
            match mtime.tv_nsec {
                libc::UTIME_OMIT => return None,
                libc::UTIME_NOW => None,
                _ => Some((mtime.tv_sec, mtime.tv_nsec)),
            }
        } else if sysno == Sysno::utime {
            let [_, modtime] = read_pair::<libc::time_t, _>(&guest.memory(), times)
                .ok()
                .flatten()?;
            Some((modtime, 0))
        } else {
            let [_, mtime] = read_pair::<libc::timeval, _>(&guest.memory(), times)
                .ok()
                .flatten()?;
            Some((mtime.tv_sec, mtime.tv_usec * 1000))
        };
        match explicit {
            Some(time) => Some(time),
            None => {
                let now: Timespec = crate::tool_global::thread_observe_time(guest).await.into();
                Some((now.tv_sec, now.tv_nsec))
            }
        }
    }
}

/// The descriptors `sysno` with arguments `args` names, which a placeholder
/// may be (`Detcore::adopt_received_placeholders`): the descriptor of every
/// call that reads, writes, seeks, maps, describes, changes, duplicates or
/// transfers an open file, and the directory descriptor of a path call whose
/// path is empty or NULL, which names that descriptor itself. Not `close`,
/// which needs no model of what it closes.
fn descriptor_arguments<M: MemoryAccess>(
    memory: &M,
    sysno: Sysno,
    args: &reverie::syscalls::SyscallArgs,
) -> Vec<RawFd> {
    let names_itself = |path: usize| -> bool {
        match PathPtr::from_ptr(path as *const libc::c_char) {
            None => true,
            Some(pointer) => pointer
                .read(memory)
                .is_ok_and(|path: std::path::PathBuf| path.as_os_str().is_empty()),
        }
    };
    let fd = |raw: usize| raw as i32 as RawFd;
    match sysno {
        Sysno::read
        | Sysno::readv
        | Sysno::pread64
        | Sysno::preadv
        | Sysno::preadv2
        | Sysno::write
        | Sysno::writev
        | Sysno::pwrite64
        | Sysno::pwritev
        | Sysno::pwritev2
        | Sysno::lseek
        | Sysno::fstat
        | Sysno::fstatfs
        | Sysno::fcntl
        | Sysno::ioctl
        | Sysno::ftruncate
        | Sysno::fallocate
        | Sysno::fsync
        | Sysno::fdatasync
        | Sysno::fadvise64
        | Sysno::readahead
        | Sysno::sync_file_range
        | Sysno::fchmod
        | Sysno::fchown
        | Sysno::fgetxattr
        | Sysno::fsetxattr
        | Sysno::flistxattr
        | Sysno::fremovexattr
        | Sysno::flock
        | Sysno::dup
        | Sysno::dup2
        | Sysno::dup3
        | Sysno::getdents
        | Sysno::getdents64
        | Sysno::fchdir => vec![fd(args.arg0)],
        Sysno::mmap => vec![fd(args.arg4)],
        Sysno::sendfile | Sysno::tee => vec![fd(args.arg0), fd(args.arg1)],
        Sysno::splice | Sysno::copy_file_range => vec![fd(args.arg0), fd(args.arg2)],
        Sysno::epoll_ctl => vec![fd(args.arg2)],
        Sysno::newfstatat
        | Sysno::statx
        | Sysno::utimensat
        | Sysno::faccessat2
        | Sysno::fchownat
        | Sysno::fchmodat
        | Sysno::readlinkat
            if names_itself(args.arg1) =>
        {
            vec![fd(args.arg0)]
        }
        _ => Vec::new(),
    }
}

/// An extended attribute's name, and the value a change sets.
type XattrArguments = (Vec<u8>, Option<Vec<u8>>);

/// An extended-attribute call's name and value, checked as Linux checks them
/// before the path (`setxattr_copy`, `import_xattr_name`): the name (EFAULT,
/// or ERANGE when empty or longer than `XATTR_NAME_MAX`), then the size
/// (E2BIG), then the value (EFAULT). None for any other request.
fn checked_xattr_arguments<M: MemoryAccess>(
    memory: &M,
    request: PathRequest,
    name: u64,
) -> Result<Option<XattrArguments>, Errno> {
    if !matches!(
        request,
        PathRequest::GetXattr | PathRequest::ChangeXattr { .. }
    ) {
        return Ok(None);
    }
    let name = read_xattr_name(memory, name)?;
    if name.is_empty() || name.len() > XATTR_NAME_MAX {
        return Err(Errno::ERANGE);
    }
    let value = match request {
        PathRequest::ChangeXattr {
            size: Some(size),
            value,
            ..
        } => {
            if size > XATTR_SIZE_MAX {
                return Err(Errno::E2BIG);
            }
            let read = read_value(memory, value, size);
            if size > 0 && read.is_none() {
                return Err(Errno::EFAULT);
            }
            read
        }
        _ => None,
    };
    Ok(Some((name, value)))
}

/// An extended attribute's name at guest address `address`, imported as
/// Linux does (`import_xattr_name`): at most `XATTR_NAME_MAX + 1` bytes are
/// read, so a name with no NUL within them is too long (ERANGE) even when
/// the bytes after them cannot be read, and only a fault within them is
/// EFAULT. The answer checks the length.
///
/// Each read stays within one page and uses the guest's own access rights
/// (`read_exact_with_user_access`): an exact read that faults only when a
/// needed byte does, as the kernel's copy faults.
fn read_xattr_name<M: MemoryAccess>(memory: &M, address: u64) -> Result<Vec<u8>, Errno> {
    const PAGE: u64 = 4096;
    let mut name = Vec::new();
    let limit = XATTR_NAME_MAX + 1;
    while name.len() < limit {
        let start = address
            .checked_add(name.len() as u64)
            .ok_or(Errno::EFAULT)?;
        let page_left = (PAGE - start % PAGE) as usize;
        let mut chunk = vec![0u8; page_left.min(limit - name.len())];
        let chunk_address =
            reverie::syscalls::Addr::<u8>::from_raw(start as usize).ok_or(Errno::EFAULT)?;
        memory
            .read_exact_with_user_access(chunk_address, &mut chunk)
            .map_err(|_| Errno::EFAULT)?;
        match chunk.iter().position(|byte| *byte == 0) {
            Some(end) => {
                name.extend_from_slice(&chunk[..end]);
                return Ok(name);
            }
            None => name.extend_from_slice(&chunk),
        }
    }
    Ok(name)
}

/// `AT_SYMLINK_NOFOLLOW`, as the path-call table records `l*` calls.
const fn nofollow() -> u64 {
    libc::AT_SYMLINK_NOFOLLOW as u64
}

/// Whether `path` could be a spelling Detcore resolves to a rendered file
/// itself: its last component is a rendered file's name or a descriptor
/// number.
fn could_name_rendered(path: &Path, files: RenderedFiles) -> bool {
    let Some(std::path::Component::Normal(last)) = path.components().next_back() else {
        return false;
    };
    let last = last.as_bytes();
    rendered_paths(files).any(|rendered| rendered_name(rendered) == last)
        || ProcfsFile::PER_PROCESS_NAMES
            .iter()
            .any(|name| name.as_bytes() == last)
        || (!last.is_empty() && last.iter().all(u8::is_ascii_digit))
}

/// The guest's bytes at `at`, up to the first one it cannot read, through
/// the vectored, protection-respecting copy (`super::read_guest_prefix`):
/// how many it read. Any failure other than a fault reads nothing.
fn read_prefix<M: MemoryAccess>(memory: &M, at: u64, buffer: &mut [u8]) -> usize {
    usize::try_from(at)
        .ok()
        .and_then(AddrMut::<u8>::from_raw)
        .map_or(0, |address| {
            super::read_guest_prefix(memory, address, buffer).unwrap_or(0)
        })
}

/// The calling process's memory as a read of its own rendered
/// `/proc/<pid>/cmdline` reads and copies it (`procfs::CmdlineMemory`):
/// reads through the vectored, protection-respecting copy, as Linux's
/// `access_remote_vm` without `FOLL_FORCE` (never `PTRACE_PEEKDATA`, which
/// reaches pages the guest made unreadable and reads whole words), and
/// copies to the caller's buffer at `destination` through the user-access
/// write, up to its first byte the guest cannot write. A failure other than
/// a fault (the backend's) is kept in `failure` and ends the read.
struct GuestCmdlineMemory<'m, M> {
    memory: &'m mut M,
    destination: usize,
    failure: Option<Errno>,
}

impl<M: MemoryAccess> crate::procfs::CmdlineMemory for GuestCmdlineMemory<'_, M> {
    fn read(&mut self, at: u64, buffer: &mut [u8]) -> usize {
        if self.failure.is_some() {
            return 0;
        }
        let Some(address) = usize::try_from(at).ok().and_then(AddrMut::<u8>::from_raw) else {
            return 0;
        };
        match super::read_guest_prefix(&*self.memory, address, buffer) {
            Ok(read) => read,
            Err(errno) => {
                self.failure = Some(errno);
                0
            }
        }
    }

    fn copy_out(&mut self, offset: usize, bytes: &[u8]) -> usize {
        let mut copied = 0;
        while copied < bytes.len() && self.failure.is_none() {
            let Some(address) = self
                .destination
                .checked_add(offset + copied)
                .and_then(AddrMut::<u8>::from_raw)
            else {
                break;
            };
            match self
                .memory
                .write_with_user_access(address, &bytes[copied..])
            {
                Ok(0) | Err(Errno::EFAULT) => break,
                Ok(written) => copied += written,
                Err(errno) => self.failure = Some(errno),
            }
        }
        copied
    }
}

/// An attribute value of `size` bytes at guest address `address` (at most
/// `XATTR_SIZE_MAX`, which the answer checks first), if the guest can read it.
fn read_value<M: MemoryAccess>(memory: &M, address: u64, size: usize) -> Option<Vec<u8>> {
    let size = size.min(XATTR_SIZE_MAX);
    if size == 0 {
        return Some(Vec::new());
    }
    let address = reverie::syscalls::Addr::<u8>::from_raw(address as usize)?;
    let mut value = vec![0; size];
    // Exact, with the guest's own access rights, as the kernel copies it.
    memory
        .read_exact_with_user_access(address, &mut value)
        .ok()?;
    Some(value)
}

/// What `utime` (a `struct utimbuf`) or `utimes`/`futimesat` (two `struct
/// timeval`, when `timevals`) asks of a file's times at guest address
/// `times`: NULL is the current time; the times are read, and for `timeval`s
/// their microseconds checked, before the file is.
fn legacy_times_request<M: MemoryAccess>(memory: &M, times: u64, timevals: bool) -> PathRequest {
    if times == 0 {
        return PathRequest::Times { explicit: None };
    }
    if !timevals {
        // A `struct utimbuf`: two `time_t`, with nothing to check.
        return match read_pair::<libc::time_t, M>(memory, times) {
            Err(_) => PathRequest::Fault,
            Ok(_) => PathRequest::Times {
                explicit: Some(true),
            },
        };
    }
    match read_pair::<libc::timeval, M>(memory, times) {
        Err(_) => PathRequest::Fault,
        Ok(Some(values))
            if values
                .iter()
                .any(|value| !(0..1_000_000).contains(&value.tv_usec)) =>
        {
            PathRequest::Invalid
        }
        Ok(_) => PathRequest::Times {
            explicit: Some(true),
        },
    }
}

/// Whether a `utimensat` time's nanoseconds are in range or name a special
/// value.
fn valid_utime_nsec(time: &libc::timespec) -> bool {
    (0..1_000_000_000).contains(&time.tv_nsec)
        || time.tv_nsec == libc::UTIME_NOW
        || time.tv_nsec == libc::UTIME_OMIT
}

/// Two values of `U` at guest address `address`, or `None` for NULL.
fn read_pair<U: Copy, M: MemoryAccess>(memory: &M, address: u64) -> Result<Option<[U; 2]>, Error> {
    match reverie::syscalls::Addr::<[U; 2]>::from_raw(address as usize) {
        Some(address) => Ok(Some(memory.read_value(address)?)),
        None => Ok(None),
    }
}

/// What `ioctl(fd, IOC_WATCH_QUEUE_SET_SIZE, 0)` on a pipe or FIFO says about
/// whether splice can use it (`Detcore::splice_pipe`): `Some(true)` for a pipe
/// without a watch queue (ENODEV, or ENOTTY from a kernel without watch
/// queues), `Some(false)` for a notification pipe (EBUSY once sized, EINVAL
/// for the size 0 before), which `get_pipe_info(file, true)` refuses, and
/// `None` for any other answer, which classifies nothing.
fn splice_pipe_from_watch_queue_probe(probe: Result<i64, Errno>) -> Option<bool> {
    match probe {
        Err(Errno::ENODEV | Errno::ENOTTY) => Some(true),
        Err(Errno::EBUSY | Errno::EINVAL) => Some(false),
        _ => None,
    }
}

/// Whether a path written at `at`, `length` bytes, for a kernel read of
/// another process's `/proc/<pid>/cmdline` (`Detcore::kernel_cmdline_read`)
/// overlaps a range it must not lie in: the caller's buffer, `count` bytes at
/// `destination`, whose bytes restoring the scratch would erase; or the
/// target's argument and environment area `source`, which get_mm_cmdline
/// reads from `arg_start` through `env_end` (fs/proc/base.c:319-328), and
/// which a caller sharing the target's memory (`CLONE_VM`) would have
/// overwritten with the path while the kernel reads it. Empty ranges overlap
/// nothing.
fn cmdline_path_overlaps(
    at: usize,
    length: usize,
    destination: usize,
    count: usize,
    source: Option<crate::procfs::CmdlineArea>,
) -> bool {
    let end = at.saturating_add(length);
    let hits = |start: usize, stop: usize| start < stop && at < stop && start < end;
    hits(destination, destination.saturating_add(count))
        || source.is_some_and(|area| {
            hits(
                area.arg_start as usize,
                area.env_end.max(area.arg_end) as usize,
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guest for [`Detcore::bind_task_identity`]: its registers name the
    /// call being answered (`openat`), and nothing else is used.
    struct RefusalGuest<'a> {
        config: &'a crate::config::Config,
        thread: crate::ThreadState<()>,
    }

    struct RefusalStack;
    struct RefusalStackGuard;

    impl Drop for RefusalStackGuard {
        fn drop(&mut self) {}
    }

    impl reverie::Stack for RefusalStack {
        type StackGuard = RefusalStackGuard;

        fn size(&self) -> usize {
            panic!("binding a task identity uses no guest stack")
        }
        fn capacity(&self) -> usize {
            panic!("binding a task identity uses no guest stack")
        }
        fn push<'stack, T>(&mut self, _: T) -> reverie::syscalls::Addr<'stack, T> {
            panic!("binding a task identity uses no guest stack")
        }
        fn reserve<'stack, T>(&mut self) -> reverie::syscalls::AddrMut<'stack, T> {
            panic!("binding a task identity uses no guest stack")
        }
        fn commit(self) -> Result<Self::StackGuard, Errno> {
            panic!("binding a task identity uses no guest stack")
        }
    }

    #[reverie::tool]
    impl reverie::GlobalRPC<crate::tool_global::GlobalState> for RefusalGuest<'_> {
        async fn send_rpc(
            &self,
            _: <crate::tool_global::GlobalState as reverie::GlobalTool>::Request,
        ) -> <crate::tool_global::GlobalState as reverie::GlobalTool>::Response {
            panic!("a refusal without the shutdown policy sends no request")
        }
        fn config(&self) -> &crate::config::Config {
            self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for RefusalGuest<'_> {
        type Memory = reverie::syscalls::LocalMemory;
        type Stack = RefusalStack;

        fn tid(&self) -> reverie::Tid {
            reverie::Tid::from_raw(3)
        }
        fn pid(&self) -> reverie::Tid {
            reverie::Tid::from_raw(3)
        }
        fn ppid(&self) -> Option<reverie::Tid> {
            None
        }
        fn memory(&self) -> Self::Memory {
            panic!("binding a task identity reads no guest memory")
        }
        fn thread_state(&self) -> &crate::ThreadState<()> {
            &self.thread
        }
        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
            &mut self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            // SAFETY: plain integers; zero is a valid value for each.
            let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
            regs.orig_rax = libc::SYS_openat as u64;
            regs
        }
        async fn stack(&mut self) -> Self::Stack {
            RefusalStack
        }
        async fn daemonize(&mut self) {
            panic!("binding a task identity does not daemonize")
        }
        async fn inject<S: reverie::syscalls::SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
            panic!("binding a task identity injects nothing")
        }
        async fn tail_inject<S: reverie::syscalls::SyscallInfo>(&mut self, _: S) -> reverie::Never {
            panic!("binding a task identity does not retire the guest")
        }
        fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("binding a task identity sets no timer")
        }
        fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("binding a task identity sets no timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("binding a task identity reads no clock")
        }
    }

    /// The 255th task generation of a number binds a file; the 256th, which
    /// no inode number tells apart, is refused by name through the
    /// unsupported-operation policy: with --allow-unsupported-syscalls the
    /// call is EOVERFLOW, and a fail-closed run stops naming the call (the
    /// `exit_on_unsupported_syscall` form of the stop: the run's tool error,
    /// `UnsupportedSyscallError`). No file is bound to the 256th.
    #[tokio::test]
    async fn the_256th_task_generation_is_refused_by_name() {
        use reverie::Tool as _;
        let answer = |process| crate::procfs::listing::ProcfsTaskAnswer {
            exists: true,
            without_mm: false,
            process_incarnation: process,
            thread_incarnation: None,
            mm: None,
            runtime_private_memory: None,
        };
        for (fail_closed, incarnation, expected) in [
            (true, procfs_inode::MAX_INCARNATION, "bound"),
            (false, procfs_inode::MAX_INCARNATION + 1, "EOVERFLOW"),
            (true, procfs_inode::MAX_INCARNATION + 1, "stop"),
        ] {
            let config = crate::config::Config {
                panic_on_unsupported_syscalls: fail_closed,
                exit_on_unsupported_syscall: true,
                shutdown_on_unsupported_syscall: false,
                ..crate::config::Config::default()
            };
            let tool = Detcore::new(reverie::Tid::from_raw(3), &config);
            let mut guest = RefusalGuest {
                config: &config,
                thread: crate::ThreadState::new(crate::types::DetTid::from_raw(3), &config, ()),
            };
            let mut file = ProcfsFile::from_path(Path::new("/proc/stat")).unwrap();
            let result = tool
                .bind_task_identity(&mut guest, &mut file, answer(incarnation))
                .await;
            let bound = file.task_incarnation();
            match (expected, result) {
                ("bound", Ok(())) => assert_eq!(bound, Some(answer(incarnation).incarnation())),
                ("EOVERFLOW", Err(Error::Errno(Errno::EOVERFLOW))) => assert_eq!(bound, None),
                ("stop", Err(Error::Tool(error))) => assert_eq!(
                    error
                        .downcast_ref::<crate::UnsupportedSyscallError>()
                        .expect("the policy's named refusal")
                        .0,
                    Sysno::openat
                ),
                (expected, result) => panic!("{incarnation}: expected {expected}, got {result:?}"),
            }
            if expected != "bound" {
                assert_eq!(bound, None, "a refused generation is bound to no file");
            }
        }
    }

    /// A read of the caller's own `cmdline` reads guest memory through the
    /// vectored, protection-respecting copy (`GuestCmdlineMemory`), as
    /// `access_remote_vm` without `FOLL_FORCE`: here real memory of this
    /// process, through `LocalMemory`. The setproctitle test reads exactly
    /// the one byte at `arg_end - 1`, which a following unmapped page does not
    /// hide (a word-sized `PTRACE_PEEKDATA` there would fail); a PROT_NONE
    /// page of the argument area reads nothing, as on Linux, and a read at a
    /// position past it reads the next page.
    #[test]
    fn the_watch_queue_probe_tells_a_notification_pipe() {
        // A pipe without a watch queue, and any pipe of a kernel without them.
        assert_eq!(
            splice_pipe_from_watch_queue_probe(Err(Errno::ENODEV)),
            Some(true)
        );
        assert_eq!(
            splice_pipe_from_watch_queue_probe(Err(Errno::ENOTTY)),
            Some(true)
        );
        // A notification pipe, sized or not.
        assert_eq!(
            splice_pipe_from_watch_queue_probe(Err(Errno::EBUSY)),
            Some(false)
        );
        assert_eq!(
            splice_pipe_from_watch_queue_probe(Err(Errno::EINVAL)),
            Some(false)
        );
        // Anything else classifies nothing; a success cannot happen for the
        // size 0 and is no answer either.
        assert_eq!(splice_pipe_from_watch_queue_probe(Ok(0)), None);
        assert_eq!(splice_pipe_from_watch_queue_probe(Err(Errno::EPERM)), None);
        assert_eq!(splice_pipe_from_watch_queue_probe(Err(Errno::EBADF)), None);
    }

    #[test]
    fn a_cmdline_path_avoids_the_buffer_and_the_source_area() {
        let area = crate::procfs::CmdlineArea {
            arg_start: 0x1000,
            arg_end: 0x1040,
            env_start: 0x1040,
            env_end: 0x1080,
        };
        // The caller's buffer: touching ends do not overlap; a count of 0
        // overlaps nothing.
        assert!(cmdline_path_overlaps(0x500, 64, 0x520, 8, None));
        assert!(!cmdline_path_overlaps(0x500, 64, 0x540, 8, None));
        assert!(!cmdline_path_overlaps(0x500, 64, 0x4f8, 8, None));
        assert!(cmdline_path_overlaps(0x500, 64, 0x4f8, 9, None));
        assert!(!cmdline_path_overlaps(0x500, 64, 0x520, 0, None));
        // The target's area, arguments through environment (a CLONE_VM
        // child's stack pointer inside its parent's arguments).
        assert!(cmdline_path_overlaps(0x1000, 64, 0x9000, 8, Some(area)));
        assert!(cmdline_path_overlaps(0x1070, 64, 0x9000, 8, Some(area)));
        assert!(cmdline_path_overlaps(0xfc1, 64, 0x9000, 8, Some(area)));
        assert!(!cmdline_path_overlaps(0xfc0, 64, 0x9000, 8, Some(area)));
        assert!(!cmdline_path_overlaps(0x1080, 64, 0x9000, 8, Some(area)));
        // An environment area that does not follow the arguments still
        // leaves arg_start..arg_end protected.
        let detached = crate::procfs::CmdlineArea { env_end: 0, ..area };
        assert!(cmdline_path_overlaps(0x1020, 8, 0x9000, 8, Some(detached)));
        assert!(!cmdline_path_overlaps(0x1040, 8, 0x9000, 8, Some(detached)));
    }

    #[test]
    fn a_cmdline_read_respects_page_protection() {
        const PAGE: usize = 4096;
        // SAFETY: a fresh private anonymous mapping, unmapped below.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                3 * PAGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        let base = base as usize;
        let page1 = base + PAGE;
        let page2 = base + 2 * PAGE;
        let titled = b"title\0xyz";
        let other = b"abc\0";
        // SAFETY: both ranges lie in the mapping, before its protection
        // changes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                titled.as_ptr(),
                (page2 - titled.len()) as *mut u8,
                titled.len(),
            );
            std::ptr::copy_nonoverlapping(other.as_ptr(), page1 as *mut u8, other.len());
            assert_eq!(libc::munmap(page2 as *mut libc::c_void, PAGE), 0);
            assert_eq!(
                libc::mprotect(base as *mut libc::c_void, PAGE, libc::PROT_NONE),
                0
            );
        }
        let read = |area: &crate::procfs::CmdlineArea, pos: u64, count: usize| {
            let mut out = vec![0_u8; count];
            let mut memory = reverie::syscalls::LocalMemory::new();
            let mut adapter = GuestCmdlineMemory {
                memory: &mut memory,
                destination: out.as_mut_ptr() as usize,
                failure: None,
            };
            let copied = crate::procfs::read_cmdline(area, pos, count, &mut adapter).unwrap();
            assert_eq!(adapter.failure, None);
            out.truncate(copied);
            out
        };
        let area = |start: usize, end: usize| crate::procfs::CmdlineArea {
            arg_start: start as u64,
            arg_end: end as u64,
            env_start: end as u64,
            env_end: end as u64,
        };
        // The argument area ends at the last byte before an unmapped page,
        // which is not NUL: a title.
        assert_eq!(read(&area(page2 - titled.len(), page2), 0, 64), b"title\0");
        // The argument area lies in a PROT_NONE page: nothing.
        assert_eq!(read(&area(base + 100, base + 110), 0, 64), b"");
        // It starts in the PROT_NONE page and ends in the next: a read from
        // the start reads nothing, one at a position in the next page reads
        // it.
        let spanning = area(page1 - 6, page1 + other.len());
        assert_eq!(read(&spanning, 0, 64), b"");
        assert_eq!(read(&spanning, 6, 64), b"abc\0");
        // SAFETY: the two pages still mapped, unmapped once.
        unsafe {
            assert_eq!(libc::munmap(base as *mut libc::c_void, 2 * PAGE), 0);
        }
    }

    /// The snapshot a placeholder carries across `exec` is the one its open
    /// took, and a later image continues from it.
    #[test]
    fn a_rendered_snapshot_survives_exec() {
        let open = || {
            ProcfsFile::from_path(Path::new("/proc/sys/kernel/random/uuid"))
                .unwrap()
                .into_rendered()
        };
        let mut before = open();
        assert_eq!(before.rendered_snapshot(), None);
        let template = before.rendering_template().to_vec();
        before.initialize(
            template,
            crate::procfs::ProcfsSnapshotContext {
                random_uuid: Some([0x5a; 16]),
                ..Default::default()
            },
        );
        let snapshot = before.rendered_snapshot().unwrap().to_vec();
        assert_eq!(snapshot.len(), 37);
        let head = before.take(10).unwrap();
        // A later image rediscovers the placeholder at the same position.
        let mut after = open().with_rendered_snapshot(snapshot.clone());
        after.set_offset(10);
        assert!(!after.needs_snapshot());
        let mut joined = head;
        joined.extend(after.take(64).unwrap());
        assert_eq!(joined, snapshot);
    }

    #[test]
    fn rendered_stat_is_a_read_only_regular_procfs_file() {
        let epoch = Timespec {
            tv_sec: 1_767_225_600,
            tv_nsec: 0,
        };
        let path = Path::new("/proc/sys/kernel/random/uuid");
        let stat = rendered_stat(path, epoch, (65534, 65534));
        assert_eq!(stat.mode, libc::S_IFREG | 0o444);
        assert_eq!((stat.nlink, stat.size, stat.blocks), (1, 0, 0));
        assert_eq!(stat.blksize, 1024);
        assert_eq!((stat.uid, stat.gid), (65534, 65534));
        assert_eq!(stat.dev, procfs_inode::VIRTUAL_PROC_DEVICE);
        assert_eq!(stat.inode, procfs_inode::rendered_inode(path));
        assert_eq!((stat.atime, stat.mtime, stat.ctime), (epoch, epoch, epoch));
        assert_ne!(
            rendered_stat(Path::new("/proc/sys/kernel/pty/nr"), epoch, (0, 0)).inode,
            stat.inode
        );
    }

    /// Every rendered file is one `ProcfsFile` renders, under its own inode,
    /// and the fixed tunables are writable by their owner as on Linux.
    #[test]
    fn rendered_files_have_distinct_inodes_and_linux_modes() {
        let epoch = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let paths: Vec<&Path> = rendered_paths(RenderedFiles::ALL)
            .map(|path| Path::new(path.to_str().unwrap()))
            .collect();
        assert!(paths.contains(&Path::new("/proc/cpuinfo")));
        let inodes: std::collections::BTreeSet<u64> = paths
            .iter()
            .map(|path| procfs_inode::rendered_inode(path))
            .collect();
        assert_eq!(inodes.len(), paths.len());
        for path in &paths {
            assert_eq!(
                ProcfsFile::from_path(path).and_then(|file| file.rendered_path(RenderedFiles::ALL)),
                Some(*path)
            );
            // (That its placeholder's name, with its snapshot, fits a memfd
            // name is checked in `namespace`'s readlink scratch test.)
            assert!(could_name_rendered(path, RenderedFiles::ALL), "{path:?}");
        }
        // `/proc/cpuinfo` is rendered only in a session that renders it.
        let without = RenderedFiles::WITHOUT_CPUINFO;
        assert_eq!(rendered_paths(without).count() + 1, paths.len());
        assert!(rendered_paths(without).all(|path| path != c"/proc/cpuinfo"));
        assert!(!could_name_rendered(Path::new("/proc/cpuinfo"), without));
        assert_eq!(rendered_path_within(Path::new("/cpuinfo"), without), None);
        assert_eq!(
            rendered_path_within(Path::new("/cpuinfo"), RenderedFiles::ALL),
            Some(Path::new("/proc/cpuinfo"))
        );
        let mode = |path: &str| rendered_stat(Path::new(path), epoch, (0, 0)).mode;
        assert_eq!(mode("/proc/version"), libc::S_IFREG | 0o444);
        assert_eq!(mode("/proc/sys/kernel/osrelease"), libc::S_IFREG | 0o444);
        assert_eq!(mode("/proc/sys/kernel/overflowgid"), libc::S_IFREG | 0o644);
        assert_eq!(mode("/proc/sys/vm/max_map_count"), libc::S_IFREG | 0o644);
    }

    /// The raw-identity check finds a rendered file by the kernel identity of
    /// its procfs file, however it was reached, and nothing else: measured
    /// against this host's procfs.
    #[test]
    fn raw_identity_finds_rendered_files_and_nothing_else() {
        let host = |path: &str| {
            let path = std::ffi::CString::new(path).unwrap();
            DetStat::from(GuestProcfs::stat(&path).expect("stat the host's procfs"))
        };
        assert_eq!(
            rendered_path_of_raw(&host("/proc/version"), RenderedFiles::ALL),
            Some(Path::new("/proc/version"))
        );
        assert_eq!(
            rendered_path_of_raw(&host("/proc/sys/kernel/osrelease"), RenderedFiles::ALL),
            Some(Path::new("/proc/sys/kernel/osrelease"))
        );
        // A procfs file no rendered file is: not found, and then remembered.
        let status = host("/proc/self/status");
        assert_eq!(rendered_path_of_raw(&status, RenderedFiles::ALL), None);
        assert!(
            NOT_RENDERED
                .lock()
                .unwrap()
                .contains(&(status.dev, status.inode))
        );
        assert_eq!(rendered_path_of_raw(&status, RenderedFiles::ALL), None);
        // The host's /proc/cpuinfo is a rendered file only for a session that
        // renders it, and what one session found is not remembered against
        // another: each sees its own answer, in either order.
        let cpuinfo = host("/proc/cpuinfo");
        for _ in 0..2 {
            assert_eq!(
                rendered_path_of_raw(&cpuinfo, RenderedFiles::WITHOUT_CPUINFO),
                None
            );
            assert_eq!(
                rendered_path_of_raw(&cpuinfo, RenderedFiles::ALL),
                Some(Path::new("/proc/cpuinfo"))
            );
        }
        // A directory is not even looked at.
        assert_eq!(
            rendered_path_of_raw(&host("/proc/sys/kernel"), RenderedFiles::ALL),
            None
        );
    }

    /// pty/nr is an integer sysctl: a read that copied part of it is followed
    /// by end of file, as Linux's `proc_dointvec` gives.
    #[test]
    fn pty_nr_ends_after_a_partial_read() {
        let mut file = ProcfsFile::from_path(Path::new("/proc/sys/kernel/pty/nr"))
            .unwrap()
            .into_rendered();
        let template = file.rendering_template().to_vec();
        file.initialize(
            template,
            crate::procfs::ProcfsSnapshotContext {
                virtual_pty_count: 12,
                ..Default::default()
            },
        );
        assert_eq!(file.take(1), Some(b"1".to_vec()));
        assert_eq!(file.take(64), Some(Vec::new()));
        assert_eq!(file.take_at(0, 64), Some(b"12\n".to_vec()));
    }

    #[test]
    fn placeholder_links_name_only_rendered_files() {
        assert_eq!(
            placeholder_link_path(
                b"/memfd:hermit-procfs:/proc/sys/kernel/random/uuid@30 (deleted)",
                RenderedFiles::ALL
            ),
            Some(Path::new("/proc/sys/kernel/random/uuid"))
        );
        assert_eq!(
            crate::procfs::placeholder_link(
                b"/memfd:hermit-procfs:/proc/sys/kernel/pty/nr@330a (deleted)",
                RenderedFiles::ALL
            ),
            Some((Path::new("/proc/sys/kernel/pty/nr"), b"3\n".to_vec()))
        );
        // The name a placeholder gets is the one its link is read back from.
        let name = placeholder_name(Path::new("/proc/sys/kernel/pty/nr"), b"12\n");
        assert_eq!(
            crate::procfs::placeholder_link(
                format!("/memfd:{name} (deleted)").as_bytes(),
                RenderedFiles::ALL
            ),
            Some((Path::new("/proc/sys/kernel/pty/nr"), b"12\n".to_vec()))
        );
        // A file Linux renders at read: its placeholder carries no snapshot.
        assert_eq!(
            crate::procfs::placeholder_link(
                b"/memfd:hermit-procfs:/proc/uptime@ (deleted)",
                RenderedFiles::ALL
            ),
            Some((Path::new("/proc/uptime"), Vec::new()))
        );
        assert_eq!(
            crate::procfs::placeholder_link(
                b"/memfd:hermit-procfs:/proc/stat@ (deleted)",
                RenderedFiles::ALL
            ),
            Some((Path::new("/proc/stat"), Vec::new()))
        );
        // A /proc/cpuinfo placeholder is one in a session that renders it.
        assert_eq!(
            crate::procfs::placeholder_link(
                b"/memfd:hermit-procfs:/proc/cpuinfo@ (deleted)",
                RenderedFiles::ALL
            ),
            Some((Path::new("/proc/cpuinfo"), Vec::new()))
        );
        for link in [
            b"/memfd:hermit-procfs:/proc/diskstats@30 (deleted)".as_slice(),
            b"/memfd:hermit-procfs:/proc/sys/kernel/random/uuid (deleted)",
            b"/memfd:hermit-procfs:/proc/sys/kernel/random/uuid@3 (deleted)",
            b"/memfd:hermit-procfs:/proc/sys/kernel/random/uuid@zz (deleted)",
            b"/memfd:hermit-procfs:/proc/sys/kernel/random/uuid@30",
            b"/memfd: (deleted)",
            b"/proc/sys/kernel/random/uuid",
        ] {
            assert_eq!(
                placeholder_link_path(link, RenderedFiles::ALL),
                None,
                "{link:?}"
            );
        }
        assert_eq!(
            rendered_path_within(Path::new("/sys/kernel/random/uuid"), RenderedFiles::ALL),
            Some(Path::new("/proc/sys/kernel/random/uuid"))
        );
        assert_eq!(
            rendered_path_within(Path::new("/uptime"), RenderedFiles::ALL),
            Some(Path::new("/proc/uptime"))
        );
        assert_eq!(
            rendered_path_within(Path::new("/stat"), RenderedFiles::ALL),
            Some(Path::new("/proc/stat"))
        );
        assert_eq!(
            rendered_path_within(Path::new("/diskstats"), RenderedFiles::ALL),
            None
        );
        assert_eq!(
            rendered_path_within(Path::new("/self/stat"), RenderedFiles::ALL),
            None
        );
    }

    /// A placeholder carries the uptime of its file's first read in its
    /// modification time, exactly for every uptime, and only a time before
    /// 1970 is read as one: a memfd's own time never is.
    #[test]
    fn a_placeholder_carries_the_uptime_of_the_first_read() {
        for uptime in [
            0,
            1,
            120,
            (1 << 29) - 1,
            1 << 29,
            1 << 63,
            u64::MAX - 1,
            u64::MAX,
        ] {
            let mtime = carried_uptime_mtime(uptime);
            assert!(mtime.tv_sec < 0, "{uptime}");
            assert!((0..1_000_000_000).contains(&mtime.tv_nsec), "{uptime}");
            assert_eq!(carried_uptime(mtime.tv_sec, mtime.tv_nsec), Some(uptime));
        }
        assert_eq!(carried_uptime(0, 0), None);
        assert_eq!(carried_uptime(1_767_225_600, 123), None);
        assert_eq!(carried_uptime(-1, 1 << 29), None);
        assert_eq!(carried_uptime(-1, -1), None);
        assert_eq!(carried_uptime(i64::MIN, 0), None);
        // Only where no thread and no guest handler can change the
        // descriptor between Detcore's injected calls, and only on the
        // placeholder of the file read.
        assert!(descriptor_stable_across_injections(true, false));
        assert!(!descriptor_stable_across_injections(false, false));
        assert!(!descriptor_stable_across_injections(true, true));
        assert!(!descriptor_stable_across_injections(false, true));
        let stat = Path::new("/proc/stat");
        assert!(may_carry_uptime(true, Some(stat), stat));
        assert!(!may_carry_uptime(false, Some(stat), stat));
        assert!(!may_carry_uptime(true, None, stat));
        assert!(!may_carry_uptime(
            true,
            Some(Path::new("/proc/uptime")),
            stat
        ));
        assert_eq!(
            carried_uptime(-(1 << 35), 7),
            Some((((1 << 35) - 1) << 29) | 7)
        );
    }

    #[test]
    fn open_flags_follow_linux_for_a_read_only_sysctl_file() {
        let ok = |flags| rendered_open_flags(flags, FileAccess::GLOBAL_ROOT).unwrap();
        let err = |flags| rendered_open_flags(flags, FileAccess::GLOBAL_ROOT).unwrap_err();
        assert_eq!(ok(OFlag::O_RDONLY), OFlag::O_RDONLY);
        assert_eq!(
            ok(OFlag::O_RDONLY | OFlag::O_APPEND | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC),
            OFlag::O_APPEND | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC
        );
        assert_eq!(
            ok(OFlag::O_PATH | OFlag::O_WRONLY | OFlag::O_CLOEXEC),
            OFlag::O_PATH | OFlag::O_CLOEXEC
        );
        assert_eq!(ok(OFlag::O_RDONLY | OFlag::O_CREAT), OFlag::O_RDONLY);
        assert_eq!(err(OFlag::O_WRONLY), Errno::EACCES);
        assert_eq!(err(OFlag::O_RDWR), Errno::EACCES);
        assert_eq!(err(OFlag::O_RDONLY | OFlag::O_TRUNC), Errno::EACCES);
        assert_eq!(err(OFlag::O_RDONLY | OFlag::O_DIRECTORY), Errno::ENOTDIR);
        assert_eq!(err(OFlag::O_PATH | OFlag::O_DIRECTORY), Errno::ENOTDIR);
        assert_eq!(err(OFlag::O_CREAT | OFlag::O_EXCL), Errno::EEXIST);
        assert_eq!(err(OFlag::O_CREAT | OFlag::O_DIRECTORY), Errno::EINVAL);
        assert_eq!(
            err(OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_DIRECTORY),
            Errno::EINVAL
        );
        assert_eq!(err(OFlag::O_TMPFILE | OFlag::O_RDONLY), Errno::EINVAL);
        assert_eq!(err(OFlag::O_TMPFILE | OFlag::O_RDWR), Errno::ENOTDIR);
        assert_eq!(
            err(OFlag::O_CREAT | OFlag::O_TMPFILE | OFlag::O_RDWR),
            Errno::EINVAL
        );
        assert_eq!(err(OFlag::O_PATH | OFlag::O_TMPFILE), Errno::ENOTDIR);
        assert_eq!(err(OFlag::O_RDONLY | OFlag::O_NOATIME), Errno::EPERM);
        assert_eq!(err(OFlag::O_RDONLY | OFlag::O_DIRECT), Errno::EINVAL);
    }

    #[test]
    fn mapping_follows_do_mmap_order() {
        let private = MapFlags::MAP_PRIVATE;
        let shared = MapFlags::MAP_SHARED;
        let read = ProtFlags::PROT_READ;
        let write = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE;
        let exec = ProtFlags::PROT_READ | ProtFlags::PROT_EXEC;
        let answer = |len, offset, prot, flags| {
            rendered_mmap_answer(0, len, offset, prot, flags, true, false)
        };
        assert_eq!(answer(0, 0, read, private), Err(Errno::EINVAL));
        assert_eq!(answer(4096, 1, read, private), Err(Errno::EINVAL));
        assert_eq!(answer(4096, 0, write, shared), Err(Errno::EACCES));
        assert_eq!(answer(4096, 0, exec, private), Err(Errno::EPERM));
        assert_eq!(answer(4096, 0, write, private), Err(Errno::ENODEV));
        assert_eq!(answer(4096, 0, read, shared), Err(Errno::ENODEV));
        assert_eq!(
            rendered_mmap_answer(0, 4096, 0, exec, private, false, false),
            Err(Errno::ENODEV)
        );
        // `file_mmap_ok`: an offset past the largest file offset, taken
        // unsigned.
        assert_eq!(answer(4096, -4096, read, private), Err(Errno::EOVERFLOW));
        assert_eq!(
            answer(4096, 0x7fff_ffff_ffff_f000, read, private),
            Err(Errno::EOVERFLOW)
        );
        assert_eq!(
            answer(4096, 0x7fff_ffff_ffff_e000, read, private),
            Err(Errno::ENODEV)
        );
        // A fixed mapping that would end above TASK_SIZE.
        let fixed = MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED;
        assert_eq!(
            rendered_mmap_answer(0x7fff_ffff_f000, 4096, 0, read, fixed, false, false),
            Err(Errno::ENOMEM)
        );
        assert_eq!(
            rendered_mmap_answer(0x7fff_ffff_e000, 4096, 0, read, fixed, false, false),
            Err(Errno::ENODEV)
        );
        assert_eq!(
            rendered_mmap_answer(0x1001, 4096, 0, read, fixed, false, false),
            Err(Errno::EINVAL)
        );
        // Below vm.mmap_min_addr, after the alignment check.
        assert_eq!(
            rendered_mmap_answer(0x1000, 4096, 0, read, fixed, false, false),
            Err(Errno::EPERM)
        );
    }

    /// A rendered `seq_file` (every procfs file outside `/proc/sys`) refuses
    /// a mapping as `proc_reg_mmap` does, with `EIO` where a sysctl file
    /// gives `ENODEV`, after the same earlier checks; and a positioned write
    /// with `ESPIPE`, after a negative position's `EINVAL`, except
    /// `pwritev2` at -1, which writes at the file position (measured on
    /// Linux 7.1 for /proc/uptime, /proc/loadavg, /proc/meminfo and
    /// /proc/version against /proc/sys/kernel/osrelease).
    #[test]
    fn a_seq_file_refuses_mappings_and_positioned_writes_as_linux() {
        let read = ProtFlags::PROT_READ;
        let write = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE;
        let private = MapFlags::MAP_PRIVATE;
        let shared = MapFlags::MAP_SHARED;
        let seq = |prot, flags| rendered_mmap_answer(0, 4096, 0, prot, flags, false, true);
        assert_eq!(seq(read, private), Err(Errno::EIO));
        assert_eq!(seq(read, shared), Err(Errno::EIO));
        assert_eq!(seq(write, shared), Err(Errno::EACCES));
        // A growing-down mapping is refused once the method exists.
        assert_eq!(
            seq(read, private | MapFlags::MAP_GROWSDOWN),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            rendered_mmap_answer(
                0,
                4096,
                0,
                read,
                private | MapFlags::MAP_GROWSDOWN,
                false,
                false
            ),
            Err(Errno::ENODEV)
        );
        // MAP_SHARED_VALIDATE refuses MAP_FIXED_NOREPLACE and MAP_SYNC, which
        // procfs does not support, on both kinds (a vacant range).
        let validate = MapFlags::from_bits_retain(libc::MAP_SHARED_VALIDATE);
        for seq_file in [false, true] {
            for flag in [MapFlags::MAP_FIXED_NOREPLACE, MapFlags::MAP_SYNC] {
                assert_eq!(
                    rendered_mmap_answer(
                        0x7000_0000_0000,
                        4096,
                        0,
                        read,
                        validate | flag,
                        false,
                        seq_file
                    ),
                    Err(Errno::EOPNOTSUPP)
                );
            }
            assert_eq!(
                rendered_mmap_answer(0, 4096, 0, read, validate, false, seq_file),
                Err(if seq_file { Errno::EIO } else { Errno::ENODEV })
            );
            // LEGACY_MAP_MASK takes MAP_HUGE_2MB and MAP_HUGE_1GB's bits, not
            // bit 31.
            assert_eq!(
                rendered_mmap_answer(
                    0,
                    4096,
                    0,
                    read,
                    validate | MapFlags::from_bits_retain(i32::MIN),
                    false,
                    seq_file
                ),
                Err(Errno::EOPNOTSUPP)
            );
            assert_eq!(
                rendered_mmap_answer(
                    0,
                    4096,
                    0,
                    read,
                    validate | MapFlags::from_bits_retain(30 << 26),
                    false,
                    seq_file
                ),
                Err(if seq_file { Errno::EIO } else { Errno::ENODEV })
            );
        }
        assert_eq!(
            rendered_mmap_answer(0, 0, 0, read, private, false, true),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            rendered_mmap_answer(0, 4096, 0, read, private, false, false),
            Err(Errno::ENODEV)
        );
        assert!(crate::procfs::rendered_path_is_seq_file(Path::new(
            "/proc/uptime"
        )));
        assert!(crate::procfs::rendered_path_is_seq_file(Path::new(
            "/proc/meminfo"
        )));
        assert!(!crate::procfs::rendered_path_is_seq_file(Path::new(
            "/proc/sys/kernel/osrelease"
        )));
        for sysno in [Sysno::pwrite64, Sysno::pwritev, Sysno::pwritev2] {
            assert_eq!(
                rendered_seq_file_pwrite_answer(sysno, 0),
                Some(Err(Errno::ESPIPE))
            );
            assert_eq!(
                rendered_seq_file_pwrite_answer(sysno, -2),
                Some(Err(Errno::EINVAL))
            );
        }
        assert_eq!(
            rendered_seq_file_pwrite_answer(Sysno::pwrite64, -1),
            Some(Err(Errno::EINVAL))
        );
        assert_eq!(rendered_seq_file_pwrite_answer(Sysno::pwritev2, -1), None);
    }

    #[test]
    fn extended_attribute_changes_follow_linux() {
        let one = Some(&b"1"[..]);
        let change = |name: &[u8], size, value| {
            xattr_change_answer(name, size, value, FileAccess::GLOBAL_ROOT)
        };
        assert_eq!(change(b"user.x", Some(1), one), Err(Errno::EACCES));
        assert_eq!(change(b"user.x", Some(1), None), Err(Errno::EFAULT));
        assert_eq!(change(b"foo.x", Some(1), one), Err(Errno::EACCES));
        assert_eq!(change(b"trusted.x", None, None), Err(Errno::EPERM));
        assert_eq!(change(b"security.x", None, None), Err(Errno::EPERM));
        assert_eq!(change(b"security.x", Some(1), one), Err(Errno::EPERM));
        assert_eq!(
            change(b"security.capability", Some(1), one),
            Err(Errno::EINVAL)
        );
        assert_eq!(change(b"system.x", Some(1), one), Err(Errno::EOPNOTSUPP));
        assert_eq!(change(b"", None, None), Err(Errno::ERANGE));
        assert_eq!(
            change(b"user.x", Some(XATTR_SIZE_MAX + 1), one),
            Err(Errno::E2BIG)
        );
        // The lists measured natively: a bad tag; owner, group, other; empty.
        let bad = [2, 0, 0, 0, 0x40, 0, 7, 0, 0, 0, 0, 0];
        let good = [
            2, 0, 0, 0, 1, 0, 6, 0, 255, 255, 255, 255, 4, 0, 4, 0, 255, 255, 255, 255, 0x20, 0, 4,
            0, 255, 255, 255, 255,
        ];
        let acl = b"system.posix_acl_access";
        assert_eq!(change(acl, Some(12), Some(&bad)), Err(Errno::EINVAL));
        assert_eq!(change(acl, Some(28), Some(&good)), Err(Errno::EOPNOTSUPP));
        assert_eq!(
            change(acl, Some(4), Some(&good[..4])),
            Err(Errno::EOPNOTSUPP)
        );
        assert_eq!(change(acl, None, None), Err(Errno::EOPNOTSUPP));
        // Measured: only the parser refuses (EINVAL); a list it accepts,
        // valid or not, is EOPNOTSUPP on procfs.
        assert_eq!(change(acl, Some(0), Some(&[])), Err(Errno::EOPNOTSUPP));
        assert_eq!(
            change(acl, Some(12), Some(&good[..12])),
            Err(Errno::EOPNOTSUPP)
        );
        assert_eq!(change(acl, Some(6), Some(&good[..6])), Err(Errno::EINVAL));
        assert_eq!(change(acl, Some(2), Some(&good[..2])), Err(Errno::EINVAL));
        let mut bad_version = good;
        bad_version[0] = 7;
        assert_eq!(
            change(acl, Some(28), Some(&bad_version)),
            Err(Errno::EOPNOTSUPP)
        );
        let mut bad_permissions = good;
        bad_permissions[6] = 0x40;
        assert_eq!(
            change(acl, Some(28), Some(&bad_permissions)),
            Err(Errno::EOPNOTSUPP)
        );
        // A named user must be one the namespace maps; -1 never is one.
        let named = [
            2, 0, 0, 0, 1, 0, 6, 0, 255, 255, 255, 255, 2, 0, 4, 0, 255, 255, 255, 255, 4, 0, 4, 0,
            255, 255, 255, 255, 0x10, 0, 4, 0, 255, 255, 255, 255, 0x20, 0, 4, 0, 255, 255, 255,
            255,
        ];
        assert!(posix_acl_parser_refuses(&named), "user ID -1");
        assert_eq!(xattr_get_answer(b"", true), Err(Errno::ERANGE));
        assert_eq!(xattr_get_answer(b"trusted.x", true), Err(Errno::ENODATA));
        assert_eq!(xattr_get_answer(b"user.x", true), Err(Errno::EOPNOTSUPP));
        assert_eq!(xattr_get_answer(b"", false), Err(Errno::ERANGE));
        assert_eq!(xattr_get_answer(b"trusted.x", false), Err(Errno::ENODATA));
        assert_eq!(xattr_get_answer(b"user.x", false), Err(Errno::EACCES));
        assert_eq!(
            xattr_get_answer(b"security.x", false),
            Err(Errno::EOPNOTSUPP)
        );
        assert_eq!(xattr_get_answer(b"system.x", false), Err(Errno::EOPNOTSUPP));
    }

    /// Measured natively against /proc/sys/kernel/random/uuid as a user the
    /// file's user namespace does not own.
    #[test]
    fn path_calls_follow_linux_for_a_read_only_sysctl_file() {
        let access = |mode: i32, flags: i32| {
            path_request_answer(
                PathRequest::Access {
                    mode: mode as u64,
                    flags: flags as u64,
                },
                None,
                None,
                FileAccess::GLOBAL_ROOT,
            )
        };
        assert_eq!(access(libc::F_OK, 0), Ok(0));
        assert_eq!(access(libc::R_OK, libc::AT_EACCESS), Ok(0));
        assert_eq!(access(libc::W_OK, 0), Err(Errno::EACCES));
        assert_eq!(access(libc::X_OK, 0), Err(Errno::EACCES));
        assert_eq!(access(8, 0), Err(Errno::EINVAL));
        assert_eq!(access(libc::R_OK, 0x10000), Err(Errno::EINVAL));
        assert_eq!(
            path_request_answer(
                PathRequest::GetXattr,
                Some(b"user.x"),
                None,
                FileAccess::GLOBAL_ROOT
            ),
            Err(Errno::EOPNOTSUPP)
        );
        assert_eq!(
            path_request_answer(PathRequest::ListXattr, None, None, FileAccess::GLOBAL_ROOT),
            Ok(0)
        );
        let change = |name: &[u8], flags: i32| {
            path_request_answer(
                PathRequest::ChangeXattr {
                    size: Some(1),
                    value: 0,
                    flags: flags as u64,
                },
                Some(name),
                Some(&b"1"[..]),
                FileAccess::GLOBAL_ROOT,
            )
        };
        assert_eq!(change(b"user.x", 0), Err(Errno::EACCES));
        assert_eq!(change(b"user.x", 4), Err(Errno::EINVAL));
        assert_eq!(
            path_request_answer(
                PathRequest::Chmod { flags: 0 },
                None,
                None,
                FileAccess::GLOBAL_ROOT
            ),
            Err(Errno::EPERM)
        );
        assert_eq!(
            path_request_answer(
                PathRequest::Truncate { length: 100 },
                None,
                None,
                FileAccess::GLOBAL_ROOT
            ),
            Err(Errno::EACCES)
        );
        assert_eq!(
            path_request_answer(
                PathRequest::Truncate { length: -1 },
                None,
                None,
                FileAccess::GLOBAL_ROOT
            ),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            path_request_answer(PathRequest::Chdir, None, None, FileAccess::GLOBAL_ROOT),
            Err(Errno::ENOTDIR)
        );
    }

    #[test]
    fn times_ask_for_write_permission_or_the_owner() {
        let at = |nsec: [i64; 2]| {
            Some([
                libc::timespec {
                    tv_sec: 1,
                    tv_nsec: nsec[0],
                },
                libc::timespec {
                    tv_sec: 1,
                    tv_nsec: nsec[1],
                },
            ])
        };
        assert_eq!(utimens_request(None), None);
        assert_eq!(utimens_request(at([libc::UTIME_NOW; 2])), None);
        assert_eq!(utimens_request(at([libc::UTIME_OMIT; 2])), Some(false));
        // One time now and one left alone is an explicit change, which
        // needs the owner (measured natively: EPERM).
        assert_eq!(
            utimens_request(at([libc::UTIME_NOW, libc::UTIME_OMIT])),
            Some(true)
        );
        assert_eq!(utimens_request(at([0, libc::UTIME_OMIT])), Some(true));
        let times = |explicit| {
            path_request_answer(
                PathRequest::Times { explicit },
                None,
                None,
                FileAccess::GLOBAL_ROOT,
            )
        };
        assert_eq!(times(None), Err(Errno::EACCES));
        assert_eq!(times(Some(false)), Ok(0));
        assert_eq!(times(Some(true)), Err(Errno::EPERM));
    }

    #[test]
    fn a_trailing_slash_asks_for_a_directory() {
        assert!(names_a_directory(Path::new(
            "/proc/sys/kernel/random/uuid/"
        )));
        assert!(names_a_directory(Path::new(
            "/proc/sys/kernel/random/uuid/."
        )));
        assert!(!names_a_directory(Path::new(
            "/proc/sys/kernel/random/uuid"
        )));
    }

    /// The two capability sets the per-process answers are measured under:
    /// a guest's own under Hermit, every capability (ptrace, KVM), and the
    /// file's owner without any (natively, as a user).
    const ALL_CAPABILITIES: u64 = (1 << 41) - 1;

    /// Linux's answers to opening a per-process file, which the caller owns,
    /// measured on Linux 7.1 against /proc/self/cgroup with and without
    /// CAP_DAC_OVERRIDE: writing needs the override, and then the placeholder
    /// is reopened with the guest's access mode (never `O_TRUNC`); `O_NOATIME`
    /// needs only the owner.
    #[test]
    fn open_flags_follow_linux_for_a_process_file_its_owner_opens() {
        let with = FileAccess::owned_with(ALL_CAPABILITIES);
        let without = FileAccess::owned_with(0);
        assert_eq!(
            rendered_open_flags(OFlag::O_WRONLY, with),
            Ok(OFlag::O_WRONLY)
        );
        assert_eq!(
            rendered_open_flags(OFlag::O_RDWR | OFlag::O_APPEND, with),
            Ok(OFlag::O_RDWR | OFlag::O_APPEND)
        );
        assert_eq!(
            rendered_open_flags(OFlag::O_RDONLY | OFlag::O_TRUNC, with),
            Ok(OFlag::O_RDONLY)
        );
        assert_eq!(
            rendered_open_flags(OFlag::O_RDONLY | OFlag::O_NOATIME, without),
            Ok(OFlag::O_RDONLY | OFlag::O_NOATIME)
        );
        for flags in [
            OFlag::O_WRONLY,
            OFlag::O_RDWR,
            OFlag::O_RDONLY | OFlag::O_TRUNC,
        ] {
            assert_eq!(rendered_open_flags(flags, without), Err(Errno::EACCES));
        }
        assert_eq!(
            rendered_open_flags(OFlag::O_RDONLY | OFlag::O_DIRECT, with),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            rendered_open_flags(OFlag::O_CREAT | OFlag::O_EXCL, with),
            Err(Errno::EEXIST)
        );
        // A system-wide file keeps its answers whatever the guest holds.
        assert_eq!(
            rendered_open_flags(OFlag::O_WRONLY, FileAccess::GLOBAL_ROOT),
            Err(Errno::EACCES)
        );
        assert_eq!(
            rendered_open_flags(OFlag::O_NOATIME, FileAccess::GLOBAL_ROOT),
            Err(Errno::EPERM)
        );
    }

    /// The path calls a per-process file's owner makes, measured on Linux 7.1
    /// against /proc/self/cgroup under Hermit's ptrace backend (every
    /// capability) and natively (none).
    #[test]
    fn path_calls_follow_linux_for_a_process_file_its_owner_names() {
        for (capabilities, writes, xattr, security) in [
            (
                ALL_CAPABILITIES,
                Ok(0),
                Errno::EOPNOTSUPP,
                Errno::EOPNOTSUPP,
            ),
            (0, Err(Errno::EACCES), Errno::EACCES, Errno::EPERM),
        ] {
            let access = FileAccess::owned_with(capabilities);
            let answer = |request| path_request_answer(request, None, None, access);
            let probe = |mode: i32| {
                answer(PathRequest::Access {
                    mode: mode as u64,
                    flags: 0,
                })
            };
            assert_eq!(probe(libc::R_OK), Ok(0));
            assert_eq!(probe(libc::W_OK), writes);
            assert_eq!(probe(libc::X_OK), Err(Errno::EACCES));
            assert_eq!(answer(PathRequest::Truncate { length: 0 }), writes);
            assert_eq!(
                answer(PathRequest::Truncate { length: -1 }),
                Err(Errno::EINVAL)
            );
            assert_eq!(answer(PathRequest::Chmod { flags: 0 }), Err(Errno::EPERM));
            for explicit in [None, Some(false), Some(true)] {
                assert_eq!(answer(PathRequest::Times { explicit }), Ok(0));
            }
            let change = |name: &[u8]| {
                path_request_answer(
                    PathRequest::ChangeXattr {
                        size: Some(1),
                        value: 0,
                        flags: 0,
                    },
                    Some(name),
                    Some(&b"1"[..]),
                    access,
                )
            };
            assert_eq!(change(b"user.x"), Err(xattr));
            assert_eq!(change(b"security.x"), Err(security));
            assert_eq!(change(b"trusted.x"), Err(Errno::EPERM));
            assert_eq!(
                xattr_change_answer(b"user.x", None, None, access),
                Err(xattr)
            );
        }
    }

    /// A per-process file has no `mmap` method: a mapping is `ENODEV`, as for
    /// a sysctl file, where a system-wide `seq_file` is `EIO`; and a
    /// descriptor opened for writing may be mapped shared and writable up to
    /// that answer, but one that cannot read may not be mapped at all.
    #[test]
    fn a_process_file_maps_as_a_file_without_mmap() {
        let read = ProtFlags::PROT_READ;
        let write = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE;
        let check = |prot, flags, seq_file, mode| {
            rendered_mmap_file_checks(4096, 0, prot, flags, false, seq_file, mode)
        };
        let private = MapFlags::MAP_PRIVATE;
        let shared = MapFlags::MAP_SHARED;
        assert_eq!(check(read, private, false, (true, false)), Errno::ENODEV);
        assert_eq!(check(write, shared, false, (true, true)), Errno::ENODEV);
        assert_eq!(check(write, shared, false, (true, false)), Errno::EACCES);
        assert_eq!(check(read, private, false, (false, true)), Errno::EACCES);
        assert_eq!(check(read, shared, false, (false, true)), Errno::EACCES);
        assert_eq!(
            check(read, private | MapFlags::MAP_GROWSDOWN, false, (true, true)),
            Errno::ENODEV
        );
        assert_eq!(check(read, private, true, (true, false)), Errno::EIO);
    }

    /// Detcore's process table decides whether a per-process file's task
    /// exists: a listed process (a zombie too), a live thread of a listed
    /// process below its `task`, and the namespace's init with its one thread.
    #[test]
    fn a_process_file_exists_for_exactly_the_tasks_the_table_lists() {
        use crate::procfs::BoundTask;
        use crate::procfs::listing::ListingFacts;
        let facts = ListingFacts {
            processes: vec![1, 3, 7],
            threads: vec![3, 5],
        };
        let task = |process, thread| BoundTask { process, thread };
        assert!(crate::procfs::listing::task_exists(
            task(3, None),
            Some(3),
            &facts
        ));
        assert!(crate::procfs::listing::task_exists(
            task(3, Some(5)),
            Some(3),
            &facts
        ));
        assert!(!crate::procfs::listing::task_exists(
            task(3, Some(6)),
            Some(3),
            &facts
        ));
        // A live non-leader thread's own directory, in its group.
        assert!(crate::procfs::listing::task_exists(
            task(5, None),
            Some(3),
            &facts
        ));
        assert!(crate::procfs::listing::task_exists(
            task(5, Some(3)),
            Some(3),
            &facts
        ));
        // A reaped process (its leader is no longer listed), or an id that
        // names no task.
        assert!(!crate::procfs::listing::task_exists(
            task(9, None),
            Some(9),
            &facts
        ));
        assert!(!crate::procfs::listing::task_exists(
            task(11, None),
            None,
            &facts
        ));
        // Hermit's own init, when no guest holds pid 1.
        let without_init = ListingFacts {
            processes: vec![3],
            threads: Vec::new(),
        };
        assert!(crate::procfs::listing::task_exists(
            task(1, None),
            None,
            &without_init
        ));
        assert!(crate::procfs::listing::task_exists(
            task(1, Some(1)),
            None,
            &without_init
        ));
        assert!(!crate::procfs::listing::task_exists(
            task(1, Some(2)),
            None,
            &without_init
        ));
    }

    /// A process's `auxv` is mode 0400, readable by its owner only, measured
    /// natively on Linux 7.1: its owner opens it to read; a zombie's (global
    /// root's) cannot be opened, `access(R_OK)` and a `user.` attribute read
    /// are EACCES, a `security.` one is the file system's EOPNOTSUPP, and a
    /// `trusted.` one is invisible (ENODATA); with CAP_DAC_OVERRIDE its owner
    /// may open it to write.
    #[test]
    fn a_process_auxv_is_readable_by_its_owner_only() {
        let mut file = crate::procfs::ProcfsFile::from_path(Path::new("/proc/self/auxv")).unwrap();
        file.bind_task(3, 3);
        let path = file.into_rendered().opened_path().unwrap();
        assert!(rendered_path_is_owner_only(path));
        let mut cgroup =
            crate::procfs::ProcfsFile::from_path(Path::new("/proc/self/cgroup")).unwrap();
        cgroup.bind_task(3, 3);
        assert!(!rendered_path_is_owner_only(
            cgroup.into_rendered().opened_path().unwrap()
        ));
        assert!(!rendered_path_is_owner_only(Path::new("/proc/stat")));
        let epoch = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            rendered_stat(path, epoch, (0, 0)).mode,
            libc::S_IFREG | 0o400
        );
        let owner = FileAccess::owned_with(ALL_CAPABILITIES);
        assert_eq!(
            rendered_open_flags(OFlag::O_RDONLY, owner),
            Ok(OFlag::O_RDONLY)
        );
        assert_eq!(rendered_open_flags(OFlag::O_RDWR, owner), Ok(OFlag::O_RDWR));
        let zombie = FileAccess {
            readable: false,
            ..FileAccess::GLOBAL_ROOT
        };
        for flags in [
            OFlag::O_RDONLY,
            OFlag::O_WRONLY,
            OFlag::O_RDWR,
            OFlag::O_ACCMODE,
        ] {
            assert_eq!(rendered_open_flags(flags, zombie), Err(Errno::EACCES));
        }
        // An `O_PATH` descriptor asks for no access.
        assert_eq!(
            rendered_open_flags(OFlag::O_PATH, zombie),
            Ok(OFlag::O_PATH)
        );
        let probe = |mode: i32| {
            path_request_answer(
                PathRequest::Access {
                    mode: mode as u64,
                    flags: 0,
                },
                None,
                None,
                zombie,
            )
        };
        assert_eq!(probe(libc::F_OK), Ok(0));
        assert_eq!(probe(libc::R_OK), Err(Errno::EACCES));
        let get =
            |name: &[u8]| path_request_answer(PathRequest::GetXattr, Some(name), None, zombie);
        assert_eq!(get(b"user.x"), Err(Errno::EACCES));
        assert_eq!(get(b"security.x"), Err(Errno::EOPNOTSUPP));
        assert_eq!(get(b"trusted.x"), Err(Errno::ENODATA));
    }

    /// What decides another process's open of a rendered `auxv`: the
    /// target's probe opens (allowed); it is refused while the caller's own
    /// `/proc/thread-self/stat` opens (the kernel beneath decided: its errno);
    /// that is refused with EACCES too (no `/proc` beneath: the caller's
    /// `CAP_SYS_PTRACE` decides); any other errno is the open's answer.
    #[test]
    fn another_process_opens_auxv_as_the_kernel_beneath_answers() {
        use AuxvAccess::Answer;
        use AuxvAccess::CallerCapability;
        for serves in [Ok(()), Err(Errno::EACCES), Err(Errno::EMFILE)] {
            assert_eq!(auxv_access_answer(Ok(()), serves), Answer(Ok(())));
        }
        for refused in [Errno::EACCES, Errno::EPERM, Errno::ESRCH, Errno::EMFILE] {
            assert_eq!(
                auxv_access_answer(Err(refused), Ok(())),
                Answer(Err(refused))
            );
        }
        assert_eq!(
            auxv_access_answer(Err(Errno::EACCES), Err(Errno::EACCES)),
            CallerCapability
        );
        for operational in [Errno::EMFILE, Errno::ENFILE, Errno::ENOMEM] {
            assert_eq!(
                auxv_access_answer(Err(operational), Err(operational)),
                Answer(Err(operational))
            );
            assert_eq!(
                auxv_access_answer(Err(Errno::EACCES), Err(operational)),
                Answer(Err(operational))
            );
        }
    }

    /// A per-process file belongs to the guest's own identity, and the path
    /// calls Detcore resolves itself include its name.
    #[test]
    fn a_process_file_is_owned_by_the_guest_and_named_by_path_calls() {
        assert_eq!(rendered_owner(Path::new("/proc/3/cgroup")), Ok((0, 0)));
        assert_eq!(
            rendered_owner(Path::new("/proc/3/task/5/cgroup")),
            Ok((0, 0))
        );
        let stat = rendered_stat(
            Path::new("/proc/3/cgroup"),
            Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            (0, 0),
        );
        assert_eq!(stat.mode, libc::S_IFREG | 0o444);
        assert_eq!(
            stat.inode,
            procfs_inode::rendered_inode(Path::new("/proc/3/cgroup"))
        );
        assert_ne!(
            stat.inode,
            procfs_inode::rendered_inode(Path::new("/proc/3/task/3/cgroup"))
        );
        assert!(could_name_rendered(
            Path::new("/proc/self/cgroup"),
            RenderedFiles::WITHOUT_CPUINFO
        ));
        assert!(could_name_rendered(
            Path::new("cgroup"),
            RenderedFiles::WITHOUT_CPUINFO
        ));
    }
}
