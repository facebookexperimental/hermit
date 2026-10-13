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
//! exists, what its metadata is and what it holds. The backend opens no
//! procfs file. The guest's descriptor is a placeholder: a memfd named
//! `PLACEHOLDER_NAME_PREFIX` and the file's path, reopened read-only with
//! the guest's own flags, a kernel object every backend has. Reopened that
//! way, the kernel itself refuses writes, write locks and size changes, and
//! reports the guest's status flags and a read-only descriptor link, as it
//! does for the procfs file. Detcore answers the rest as Linux answers for a
//! 0444 procfs sysctl file:
//!
//! - opening it, including the flag errors Linux reports;
//! - reads, from the procfs snapshot, as for every procfs file;
//! - `stat` of the path and `fstat` of the descriptor, with Detcore's procfs
//!   device (`procfs_inode::VIRTUAL_PROC_DEVICE`) and an inode that its path
//!   alone names (`procfs_inode::rendered_inode`), the same on every backend
//!   and for every spelling that reaches the file;
//! - the descriptor's link in any process's `/proc/*/fd`, and reopening
//!   through it;
//! - its `fdinfo`, `statfs`, and the calls a procfs sysctl file refuses:
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

use super::StatTarget;
use crate::fd::FdType;
use crate::procfs::ProcfsFile;
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

/// The rendered path a path within a procfs mount names, if it names one.
/// Detcore names procfs files by their path within the mount
/// (`ProcfsEntry::At`), as `/sys/kernel/random/uuid`.
pub(crate) fn rendered_path_within(within: &Path) -> Option<&'static Path> {
    let relative = within.strip_prefix("/").unwrap_or(within);
    ProcfsFile::from_path(&Path::new("/proc").join(relative))?.rendered_path()
}

/// The seals a placeholder carries where the backend supports seals: nothing
/// can change the empty file, or its seals. (`MFD_NOEXEC_SEAL`, where the
/// kernel knows it, adds `F_SEAL_EXEC`.)
pub(crate) const PLACEHOLDER_SEALS: libc::c_int =
    libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;

/// The bytes of the buffer a placeholder's memfd name, with its NUL, is built
/// in: the most a memfd name holds (`NAME_MAX` less `memfd:`), and its NUL.
const PLACEHOLDER_NAME_CAPACITY: usize = 250;

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

/// The paths of every procfs file Detcore renders.
const RENDERED_PATHS: [&std::ffi::CStr; 2] =
    [c"/proc/sys/kernel/random/uuid", c"/proc/sys/kernel/pty/nr"];

/// The rendered file that the kernel's procfs inode `(dev, ino)` is, if it is
/// one: the guest reached it through a spelling Detcore did not resolve, as a
/// symbolic link or a directory listing, under a backend whose procfs files
/// Detcore does not name by path (`procfs_inode::guest_tids_name_host_tasks`).
pub(crate) fn rendered_path_of_raw(dev: u64, ino: u64) -> Option<&'static Path> {
    static PROC_DEVICE: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    let proc_device =
        *PROC_DEVICE.get_or_init(|| GuestProcfs::stat(c"/proc").ok().map(|stat| stat.st_dev));
    if proc_device != Some(dev) {
        return None;
    }
    RENDERED_PATHS.iter().find_map(|path| {
        let stat = GuestProcfs::stat(path).ok()?;
        (stat.st_dev == dev && stat.st_ino == ino)
            .then(|| Path::new(OsStr::from_bytes(path.to_bytes())))
            .and_then(|path| ProcfsFile::from_path(path)?.rendered_path())
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

    /// The owner Linux reports for the guest's procfs sysctl files: global
    /// root as the guest's user namespace sees it.
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
        let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
        // SAFETY: the path is NUL-terminated and `statx` is writable.
        let result = unsafe {
            raw_syscall(
                libc::SYS_statx,
                [
                    libc::AT_FDCWD as u64,
                    c"/proc".as_ptr() as u64,
                    0,
                    u64::from(libc::STATX_MNT_ID),
                    statx.as_mut_ptr() as u64,
                    0,
                ],
            )
        };
        raw_errno(result)?;
        // SAFETY: the call succeeded and initialized the structure.
        let statx = unsafe { statx.assume_init() };
        if statx.stx_mask & libc::STATX_MNT_ID == 0 {
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

/// The metadata Linux gives the procfs file at `path`, a 0444 sysctl file
/// owned by `owner`, with Detcore's procfs device, the inode its path names,
/// and every time at `epoch`, on every backend.
fn rendered_stat(path: &Path, epoch: Timespec, owner: (u32, u32)) -> DetStat {
    DetStat {
        // What a procfs file's `statx` reports.
        mask: StatxMask::STATX_BASIC_STATS | StatxMask::STATX_MNT_ID,
        mode: libc::S_IFREG | 0o444,
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

/// The metadata of the rendered file `path` under `epoch`, for a placeholder
/// the kernel reached through a spelling Detcore did not resolve.
pub(super) fn placeholder_stat(
    path: &Path,
    epoch: chrono::DateTime<chrono::Utc>,
) -> Result<DetStat, Error> {
    Ok(rendered_stat(
        path,
        epoch_time(epoch),
        GuestProcfs::sysctl_owner()?,
    ))
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

/// Linux's answer to opening an existing 0444 procfs sysctl file with
/// `flags`, from a process whose user namespace does not own it, before it
/// opens anything: `build_open_flags`, then the lookup's `O_EXCL` and type
/// checks, then `may_open` and `do_dentry_open`. On success, the flags the
/// placeholder is reopened with.
fn rendered_open_flags(flags: OFlag) -> Result<OFlag, Errno> {
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
    // Only the owner bits allow writing; `O_TRUNC` asks for write access.
    if flags & OFlag::O_ACCMODE != OFlag::O_RDONLY || flags.contains(OFlag::O_TRUNC) {
        return Err(Errno::EACCES);
    }
    // `O_NOATIME` needs the file's owner.
    if flags.contains(OFlag::O_NOATIME) {
        return Err(Errno::EPERM);
    }
    // Procfs sysctl files cannot do direct I/O.
    if flags.contains(OFlag::O_DIRECT) {
        return Err(Errno::EINVAL);
    }
    let kept = OFlag::O_APPEND
        | OFlag::O_ASYNC
        | OFlag::O_CLOEXEC
        | OFlag::O_DSYNC
        | OFlag::O_LARGEFILE
        | OFlag::O_NONBLOCK
        | OFlag::O_SYNC;
    Ok(OFlag::O_RDONLY | (flags & kept))
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

/// Linux's answer to mapping a procfs sysctl file, which has no `mmap`
/// method, in `do_mmap`'s order. `noexec` says whether procfs is mounted
/// `noexec`.
fn rendered_mmap_answer(
    addr: usize,
    len: usize,
    offset: libc::off_t,
    prot: ProtFlags,
    flags: MapFlags,
    noexec: bool,
) -> Result<i64, Errno> {
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
        // `security_mmap_addr`: below `vm.mmap_min_addr` (65536, the value
        // Hermit's machine reports) needs CAP_SYS_RAWIO, which a guest lacks.
        if addr < 65536 {
            return Err(Errno::EPERM);
        }
    }
    // `file_mmap_ok`: the mapping must end within the largest offset a
    // regular file can have (`MAX_LFS_FILESIZE`), the offset taken unsigned.
    const MAX_LFS_FILESIZE: u64 = i64::MAX as u64;
    if offset as u64 / 4096 > (MAX_LFS_FILESIZE - len as u64) >> 12 {
        return Err(Errno::EOVERFLOW);
    }
    let map_type = flags.bits() & libc::MAP_TYPE;
    if map_type != libc::MAP_SHARED
        && map_type != libc::MAP_PRIVATE
        && map_type != libc::MAP_SHARED_VALIDATE
    {
        return Err(Errno::EINVAL);
    }
    // MAP_SHARED_VALIDATE refuses flags the file does not support, as
    // MAP_SYNC on procfs.
    if map_type == libc::MAP_SHARED_VALIDATE && flags.contains(MapFlags::MAP_SYNC) {
        return Err(Errno::EOPNOTSUPP);
    }
    if map_type != libc::MAP_PRIVATE && prot.contains(ProtFlags::PROT_WRITE) {
        return Err(Errno::EACCES);
    }
    if noexec && prot.contains(ProtFlags::PROT_EXEC) {
        return Err(Errno::EPERM);
    }
    Err(Errno::ENODEV)
}

/// Linux's answer to setting the extended attribute `name` of a procfs file,
/// which has no attribute handlers, to `value` (`size` bytes, `None` if the
/// guest's buffer cannot be read), or to removing it (`size` is `None`), in
/// the kernel's order: the name, the size, the value, then the namespace.
/// Measured against /proc/sys/kernel/random/uuid as a user its user namespace
/// does not own.
fn xattr_change_answer(
    name: &[u8],
    size: Option<usize>,
    value: Option<&[u8]>,
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
        // Changing a security attribute needs CAP_SYS_ADMIN.
        Err(Errno::EPERM)
    } else if name.starts_with(b"system.") {
        Err(Errno::EOPNOTSUPP)
    } else {
        // `user.` and unknown namespaces need write permission first.
        Err(Errno::EACCES)
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
/// without the global capability.
fn xattr_get_answer(name: &[u8]) -> Result<i64, Errno> {
    if name.is_empty() || name.len() > 255 {
        Err(Errno::ERANGE)
    } else if name.starts_with(b"trusted.") {
        Err(Errno::ENODATA)
    } else {
        Err(Errno::EOPNOTSUPP)
    }
}

/// A NUL-terminated string in a fixed buffer, to push on the guest's stack.
fn c_string<const N: usize>(text: &[u8]) -> [u8; N] {
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
        if let Some(procfs) = ProcfsFile::from_path(path) {
            if procfs.rendered_path().is_none() {
                return Ok(None);
            }
            if names_a_directory(path) {
                return Err(Errno::ENOTDIR.into());
            }
            return Ok(Some(RenderedTarget {
                procfs,
                via_link: false,
            }));
        }
        // procfs names descriptors without leading zeros, so `fd/03` does not
        // exist even where descriptor 3 is a placeholder. Detcore answers so
        // on every backend (KVM's own procfs resolves the spelling).
        if let Some((subject, fd)) = proc_fd_target_with_leading_zeros(path) {
            let own = match subject {
                Some(subject) => guest.inject(syscalls::Getpid::new()).await? == i64::from(subject),
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
            && guest.inject(syscalls::Getpid::new()).await? != i64::from(subject)
        {
            return Ok(None);
        }
        let target = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_rendered_path())
            .ok()
            .flatten()
            .and_then(ProcfsFile::from_path)
            .map(|procfs| RenderedTarget {
                procfs,
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
        let link = stack.push(c_string::<32>(format!("/proc/self/fd/{fd}").as_bytes()));
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
                placeholder_link_path(&bytes)
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
        let reopen = rendered_open_flags(flags)?;
        GuestProcfs::remember();
        // Read before the placeholder exists, so that a failure leaves the
        // guest nothing open.
        let owner = GuestProcfs::sysctl_owner()?;
        let path = target
            .procfs
            .rendered_path()
            .expect("an opened rendered procfs file has a path");
        // The snapshot this open takes, which its placeholder's name carries,
        // so that every backend has it after `exec`.
        let procfs = target.procfs.into_rendered();
        let snapshot = self.render_snapshot(guest, &procfs);
        let fd = self
            .inject_placeholder(guest, path, &snapshot, reopen)
            .await?;
        // The status flags Linux keeps, which include `O_NOFOLLOW`: the
        // placeholder cannot be reopened with it through its link.
        guest.thread_state().add_fd(
            fd,
            reopen | (flags & OFlag::O_NOFOLLOW),
            FdType::Regular,
            Some(rendered_stat(path, epoch_time(guest.config().epoch), owner)),
        )?;
        let procfs = procfs.with_rendered_snapshot(snapshot);
        guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_path(path);
            detfd.set_procfs(procfs.clone());
        })?;
        Ok(fd as i64)
    }

    /// The snapshot of the rendered file `procfs` an open takes, from
    /// Detcore's state, as its first read used to: a UUID from the thread's
    /// generator, or Detcore's count of open ptys.
    fn render_snapshot<G: Guest<Self>>(&self, guest: &mut G, procfs: &ProcfsFile) -> Vec<u8> {
        use rand::RngExt as _;
        let mut file = procfs.clone();
        let random_uuid = file
            .needs_random_uuid()
            .then(|| guest.thread_state_mut().thread_prng().random::<[u8; 16]>());
        let virtual_pty_count = guest
            .thread_state()
            .count_open_files_at_paths(&[Path::new("/dev/ptmx"), Path::new("/dev/pts/ptmx")]);
        let template = file.rendering_template().to_vec();
        file.initialize(
            template,
            crate::procfs::ProcfsSnapshotContext {
                random_uuid,
                virtual_pty_count,
                ..Default::default()
            },
        );
        file.rendered_snapshot()
            .expect("a rendered file has its snapshot once initialized")
            .to_vec()
    }

    /// Inject a placeholder for `path` open with `flags`, at the lowest free
    /// descriptor number, as `open` would place the file.
    async fn inject_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &'static Path,
        snapshot: &[u8],
        flags: OFlag,
    ) -> Result<RawFd, Error> {
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
        guest.memory().write_exact(
            link.cast::<u8>(),
            &c_string::<32>(format!("/proc/self/fd/{memfd}").as_bytes()),
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
        Ok(memfd)
    }

    /// The inode number of the directory entry `name`, which the host listed
    /// with inode `host_ino`, if it is a procfs file Detcore renders.
    pub(super) fn rendered_listed_inode(
        &self,
        inodes: &procfs_inode::DirectoryInodes,
        name: &[u8],
        host_ino: u64,
    ) -> Option<u64> {
        if self.cfg.recordreplay_modes {
            return None;
        }
        // By kernel identity only for an entry named as a rendered file is,
        // which costs two stats, and only then.
        inodes.rendered_entry_inode(name).or_else(|| {
            RENDERED_PATHS
                .iter()
                .any(|path| path.to_bytes().rsplit(|byte| *byte == b'/').next() == Some(name))
                .then(|| rendered_path_of_raw(inodes.device(), host_ino))
                .flatten()
                .map(procfs_inode::rendered_inode)
        })
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
        let rendered = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_rendered_path().is_some())
            .unwrap_or(false);
        if !rendered {
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
        let offset = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.procfs_rendered_path()?;
            detfd.procfs_position().map(|(offset, _)| offset)
        })?;
        if let Some(offset) = offset {
            guest
                .inject(
                    syscalls::Lseek::new()
                        .with_fd(fd)
                        .with_offset(offset as libc::off_t)
                        .with_whence(syscalls::Whence::SEEK_SET),
                )
                .await?;
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
                let observed = Self::observed_path(guest, dirfd, &path)?;
                match self.rendered_procfs_at(guest, &observed).await? {
                    // A link's own metadata is not its target's: `lstat` of
                    // `/proc/self/fd/N` describes the link, which the kernel has.
                    Some(target) if follow || !target.via_link => target.procfs.rendered_path(),
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
        Ok(Some(rendered_stat(
            path,
            epoch_time(guest.config().epoch),
            GuestProcfs::sysctl_owner()?,
        )))
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
        if !target.via_link || buffer_len == 0 || buffer_len > i32::MAX as usize {
            return Err(Errno::EINVAL.into());
        }
        let link = target
            .procfs
            .rendered_path()
            .expect("a rendered procfs file has a path");
        write_link(&mut guest.memory(), link, buffer, buffer_len).map(Some)
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
        match placeholder_link_path(&bytes) {
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

    /// Answer `call` if it must not reach the placeholder behind a rendered
    /// procfs descriptor: what a procfs sysctl file refuses, in the order
    /// Linux checks. The placeholder's read-only description already makes
    /// the kernel refuse writes and size changes, and an `O_PATH` one every
    /// call but `fstat`, which Detcore answers.
    pub(crate) fn answer_rendered_procfs_operation<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Option<Result<i64, Error>> {
        let (sysno, args) = call.into_parts();
        let fd = match sysno {
            Sysno::mmap => args.arg4 as i32,
            Sysno::fsync
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
        let status_flags = guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                detfd.procfs_rendered_path().map(|_| detfd.status_flags())
            })
            .ok()
            .flatten()?;
        if status_flags & libc::O_PATH != 0 {
            return None;
        }
        let answer = match call {
            Syscall::Mmap(c) => {
                if c.flags().contains(MapFlags::MAP_ANONYMOUS) {
                    return None;
                }
                let noexec = match GuestProcfs::statfs() {
                    Ok((_, flags)) => flags & libc::ST_NOEXEC != 0,
                    Err(errno) => return Some(Err(errno.into())),
                };
                rendered_mmap_answer(
                    c.addr().map_or(0, |addr| addr.as_raw()),
                    c.len(),
                    c.offset(),
                    c.prot(),
                    c.flags(),
                    noexec,
                )
            }
            Syscall::Fsync(_) | Syscall::Fdatasync(_) => Err(Errno::EINVAL),
            Syscall::Fchmod(_) => Err(Errno::EPERM),
            Syscall::Fgetxattr(c) => match c.name().map(|name| name.read(&guest.memory())) {
                Some(Ok(name)) => xattr_get_answer(name.as_bytes()),
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
                ),
                Some(Err(errno)) => Err(errno),
                None => Err(Errno::EFAULT),
            },
            Syscall::Fremovexattr(c) => match c.name().map(|name| name.read(&guest.memory())) {
                Some(Ok(name)) => xattr_change_answer(name.as_bytes(), None, None),
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
                libc::F_ADD_SEALS => Err(Errno::EPERM),
                libc::F_GET_SEALS => Err(Errno::EINVAL),
                // A lease needs the file's owner, so the file never has one.
                libc::F_SETLEASE => Err(Errno::EACCES),
                // F_SET_RW_HINT needs the file's owner.
                1036 => Err(Errno::EPERM),
                libc::F_GETLEASE => Ok(i64::from(libc::F_UNLCK)),
                // `O_NOATIME` needs the file's owner, and procfs sysctl
                // files cannot do direct I/O.
                libc::F_SETFL if args.arg2 as libc::c_int & libc::O_NOATIME != 0 => {
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

/// Linux's answer to `request` for a 0444 procfs sysctl file owned by a user
/// the guest's user namespace does not map, given the attribute `name` when
/// it changes one.
fn path_request_answer(
    request: PathRequest,
    name: Option<&[u8]>,
    value: Option<&[u8]>,
) -> Result<i64, Errno> {
    match request {
        PathRequest::Access { mode, flags } => {
            if flags & !(libc::AT_EACCESS | libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) as u64
                != 0
                || mode & !7 != 0
            {
                Err(Errno::EINVAL)
            } else if mode & (libc::W_OK | libc::X_OK) as u64 != 0 {
                Err(Errno::EACCES)
            } else {
                Ok(0)
            }
        }
        PathRequest::GetXattr => xattr_get_answer(name.ok_or(Errno::EFAULT)?),
        PathRequest::ListXattr => Ok(0),
        PathRequest::ChangeXattr { size, flags, .. } => {
            if flags & !(libc::XATTR_CREATE | libc::XATTR_REPLACE) as u64 != 0 {
                return Err(Errno::EINVAL);
            }
            xattr_change_answer(name.ok_or(Errno::EFAULT)?, size, value)
        }
        PathRequest::Chmod { flags } => {
            if flags & !(libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) as u64 != 0 {
                Err(Errno::EINVAL)
            } else {
                Err(Errno::EPERM)
            }
        }
        PathRequest::Truncate { length } if length < 0 => Err(Errno::EINVAL),
        PathRequest::Truncate { .. } => Err(Errno::EACCES),
        // Leaving both times alone changes nothing; the current time needs
        // write permission, and any other time the owner.
        PathRequest::Times { explicit: None } => Err(Errno::EACCES),
        PathRequest::Times {
            explicit: Some(false),
        } => Ok(0),
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
            Sysno::access => (at, a0, 0, PathRequest::Access { mode: a1, flags: 0 }, 0),
            Sysno::faccessat => (a0, a1, 0, PathRequest::Access { mode: a2, flags: 0 }, 0),
            Sysno::faccessat2 => (
                a0,
                a1,
                a3,
                PathRequest::Access {
                    mode: a2,
                    flags: a3,
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
                    flags: a4,
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
        let rendered = if by_descriptor {
            let rendered = guest
                .thread_state()
                .with_detfd(dirfd, |detfd| {
                    detfd
                        .procfs_rendered_path()
                        .map(|path| (path, detfd.status_flags() & libc::O_PATH != 0))
                })
                .ok()
                .flatten();
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
            let lexical = if could_name_rendered(&text) {
                let observed = Self::observed_path(guest, dirfd, &text)?;
                match self.rendered_procfs_at(guest, &observed).await? {
                    // A call that does not follow a descriptor link acts on
                    // the link itself, which the kernel has.
                    Some(target) if target.via_link && flags & nofollow() != 0 => return Ok(None),
                    Some(target) => target.procfs.rendered_path(),
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
        let name = match name {
            0 => None,
            address => match syscalls::CStrPtr::from_ptr(address as *const libc::c_char) {
                Some(name) => Some(name.read(&guest.memory())?),
                None => return Err(Errno::EFAULT.into()),
            },
        };
        let value = match request {
            PathRequest::ChangeXattr {
                size: Some(size),
                value,
                ..
            } => read_value(&guest.memory(), value, size),
            _ => None,
        };
        path_request_answer(
            request,
            name.as_ref().map(|name| name.as_bytes()),
            value.as_deref(),
        )
        .map(Some)
        .map_err(Into::into)
    }
}

/// `AT_SYMLINK_NOFOLLOW`, as the path-call table records `l*` calls.
const fn nofollow() -> u64 {
    libc::AT_SYMLINK_NOFOLLOW as u64
}

/// Whether `path` could be a spelling Detcore resolves to a rendered file
/// itself: its last component is a rendered file's name or a descriptor
/// number.
fn could_name_rendered(path: &Path) -> bool {
    let Some(std::path::Component::Normal(last)) = path.components().next_back() else {
        return false;
    };
    let last = last.as_bytes();
    RENDERED_PATHS
        .iter()
        .any(|rendered| rendered.to_bytes().rsplit(|byte| *byte == b'/').next() == Some(last))
        || (!last.is_empty() && last.iter().all(u8::is_ascii_digit))
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
    memory.read_exact(address, &mut value).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn placeholder_links_name_only_rendered_files() {
        assert_eq!(
            placeholder_link_path(
                b"/memfd:hermit-procfs:/proc/sys/kernel/random/uuid@30 (deleted)"
            ),
            Some(Path::new("/proc/sys/kernel/random/uuid"))
        );
        assert_eq!(
            crate::procfs::placeholder_link(
                b"/memfd:hermit-procfs:/proc/sys/kernel/pty/nr@330a (deleted)"
            ),
            Some((Path::new("/proc/sys/kernel/pty/nr"), b"3\n".to_vec()))
        );
        // The name a placeholder gets is the one its link is read back from.
        let name = placeholder_name(Path::new("/proc/sys/kernel/pty/nr"), b"12\n");
        assert_eq!(
            crate::procfs::placeholder_link(format!("/memfd:{name} (deleted)").as_bytes()),
            Some((Path::new("/proc/sys/kernel/pty/nr"), b"12\n".to_vec()))
        );
        for link in [
            b"/memfd:hermit-procfs:/proc/uptime@30 (deleted)".as_slice(),
            b"/memfd:hermit-procfs:/proc/sys/kernel/random/uuid (deleted)",
            b"/memfd:hermit-procfs:/proc/sys/kernel/random/uuid@3 (deleted)",
            b"/memfd:hermit-procfs:/proc/sys/kernel/random/uuid@zz (deleted)",
            b"/memfd:hermit-procfs:/proc/sys/kernel/random/uuid@30",
            b"/memfd: (deleted)",
            b"/proc/sys/kernel/random/uuid",
        ] {
            assert_eq!(placeholder_link_path(link), None, "{link:?}");
        }
        assert_eq!(
            rendered_path_within(Path::new("/sys/kernel/random/uuid")),
            Some(Path::new("/proc/sys/kernel/random/uuid"))
        );
        assert_eq!(rendered_path_within(Path::new("/uptime")), None);
    }

    #[test]
    fn open_flags_follow_linux_for_a_read_only_sysctl_file() {
        let ok = |flags| rendered_open_flags(flags).unwrap();
        let err = |flags| rendered_open_flags(flags).unwrap_err();
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
        let answer =
            |len, offset, prot, flags| rendered_mmap_answer(0, len, offset, prot, flags, true);
        assert_eq!(answer(0, 0, read, private), Err(Errno::EINVAL));
        assert_eq!(answer(4096, 1, read, private), Err(Errno::EINVAL));
        assert_eq!(answer(4096, 0, write, shared), Err(Errno::EACCES));
        assert_eq!(answer(4096, 0, exec, private), Err(Errno::EPERM));
        assert_eq!(answer(4096, 0, write, private), Err(Errno::ENODEV));
        assert_eq!(answer(4096, 0, read, shared), Err(Errno::ENODEV));
        assert_eq!(
            rendered_mmap_answer(0, 4096, 0, exec, private, false),
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
            rendered_mmap_answer(0x7fff_ffff_f000, 4096, 0, read, fixed, false),
            Err(Errno::ENOMEM)
        );
        assert_eq!(
            rendered_mmap_answer(0x7fff_ffff_e000, 4096, 0, read, fixed, false),
            Err(Errno::ENODEV)
        );
        assert_eq!(
            rendered_mmap_answer(0x1001, 4096, 0, read, fixed, false),
            Err(Errno::EINVAL)
        );
        // Below vm.mmap_min_addr, after the alignment check.
        assert_eq!(
            rendered_mmap_answer(0x1000, 4096, 0, read, fixed, false),
            Err(Errno::EPERM)
        );
    }

    #[test]
    fn extended_attribute_changes_follow_linux() {
        let one = Some(&b"1"[..]);
        let change = |name: &[u8], size, value| xattr_change_answer(name, size, value);
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
        assert_eq!(xattr_get_answer(b""), Err(Errno::ERANGE));
        assert_eq!(xattr_get_answer(b"trusted.x"), Err(Errno::ENODATA));
        assert_eq!(xattr_get_answer(b"user.x"), Err(Errno::EOPNOTSUPP));
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
            )
        };
        assert_eq!(access(libc::F_OK, 0), Ok(0));
        assert_eq!(access(libc::R_OK, libc::AT_EACCESS), Ok(0));
        assert_eq!(access(libc::W_OK, 0), Err(Errno::EACCES));
        assert_eq!(access(libc::X_OK, 0), Err(Errno::EACCES));
        assert_eq!(access(8, 0), Err(Errno::EINVAL));
        assert_eq!(access(libc::R_OK, 0x10000), Err(Errno::EINVAL));
        assert_eq!(
            path_request_answer(PathRequest::GetXattr, Some(b"user.x"), None),
            Err(Errno::EOPNOTSUPP)
        );
        assert_eq!(
            path_request_answer(PathRequest::ListXattr, None, None),
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
            )
        };
        assert_eq!(change(b"user.x", 0), Err(Errno::EACCES));
        assert_eq!(change(b"user.x", 4), Err(Errno::EINVAL));
        assert_eq!(
            path_request_answer(PathRequest::Chmod { flags: 0 }, None, None),
            Err(Errno::EPERM)
        );
        assert_eq!(
            path_request_answer(PathRequest::Truncate { length: 100 }, None, None),
            Err(Errno::EACCES)
        );
        assert_eq!(
            path_request_answer(PathRequest::Truncate { length: -1 }, None, None),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            path_request_answer(PathRequest::Chdir, None, None),
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
        let times = |explicit| path_request_answer(PathRequest::Times { explicit }, None, None);
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
}
