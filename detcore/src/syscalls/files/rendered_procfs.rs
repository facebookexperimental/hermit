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
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;

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
use crate::procfs::AddressSpaceFile;
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
use crate::types::OpenFileId;
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

/// Whether the tool may read a task's kernel `comm` by path at all: not
/// where it shares the guest's root (`shares_root`: DBT, SaBRe, LiteInst)
/// and the guest has changed that root (`changed_root`), because the path is
/// then looked up through a root the guest controls, where no check on the
/// opened file authenticates the task (`Detcore::kernel_task_comm`). A
/// tracer's own root is not the guest's.
fn comm_read_is_trusted(shares_root: bool, changed_root: bool) -> bool {
    !(shares_root && changed_root)
}

/// Whether the open file `file` is on a procfs (`fstatfs` reports
/// `PROC_SUPER_MAGIC`), which refuses an ordinary file mounted over a
/// task's `comm` path.
fn comm_file_is_on_procfs(file: &crate::util::RawFd) -> bool {
    file.fs_type()
        .is_ok_and(|kind| kind == reverie::syscalls::linux_abi::PROC_SUPER_MAGIC as u64)
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
/// belongs to, its identity, and the snapshot its name carries, by the
/// placeholder's kernel identity: every rendered open makes its own
/// placeholder, so one identity is one open file description
/// (`Detcore::adopt_received_placeholders`). One small entry per rendered
/// open of the run, as `PLACEHOLDERS` keeps one; the handle keeps no
/// description alive. Where the tool runs in each guest process, a fork's
/// child has its own copy, whose handles may reach the parent's model of a
/// description rather than the copy the child's table holds; the identity
/// (`OpenFileId`), which every copy keeps, is what names the open file there
/// (`FileMetadata::adopt_placeholder`).
static PLACEHOLDER_OPENS: std::sync::Mutex<
    std::collections::BTreeMap<(u64, u64), PlaceholderOpen>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// The open file description a placeholder belongs to, its identity, and the
/// snapshot its name carries (`PLACEHOLDER_OPENS`).
type PlaceholderOpen = (crate::fd::OpenFileHandle, OpenFileId, Vec<u8>);

/// Record that the placeholder `identity` belongs to the open file
/// description `handle`, whose identity is `id`, its name carrying
/// `snapshot`.
pub(crate) fn register_placeholder_open(
    identity: (u64, u64),
    handle: crate::fd::OpenFileHandle,
    id: OpenFileId,
    snapshot: Vec<u8>,
) {
    PLACEHOLDER_OPENS
        .lock()
        .expect("placeholder open registry poisoned")
        .insert(identity, (handle, id, snapshot));
}

/// The open file description the placeholder `identity` belongs to, its
/// identity, and the snapshot its name carries, if this tool process opened
/// it (or, where the tool runs in each guest process, its process or an
/// ancestor before a fork did).
fn placeholder_open(identity: (u64, u64)) -> Option<PlaceholderOpen> {
    PLACEHOLDER_OPENS
        .lock()
        .expect("placeholder open registry poisoned")
        .get(&identity)
        .cloned()
}

/// The identity of the open file the placeholder `identity` belongs to, as
/// this tool process's registry names it (`PLACEHOLDER_OPENS`).
pub(crate) fn placeholder_open_id(identity: (u64, u64)) -> Option<OpenFileId> {
    placeholder_open(identity).map(|(_, id, _)| id)
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
/// fixed files of `procfs::fixed`; `/proc/diskstats` from the host's block
/// devices, which `/sys/block` shows the guest too.
const DYNAMIC_PATHS: [&std::ffi::CStr; 5] = [
    c"/proc/sys/kernel/random/uuid",
    c"/proc/sys/kernel/pty/nr",
    c"/proc/uptime",
    c"/proc/stat",
    c"/proc/diskstats",
];

/// The paths of every procfs file a session renders (`files`):
/// `/proc/cpuinfo` and `/proc/locks` too where it renders them.
pub(crate) fn rendered_paths(
    files: RenderedFiles,
) -> impl Iterator<Item = &'static std::ffi::CStr> {
    DYNAMIC_PATHS
        .into_iter()
        .chain(files.cpuinfo.then_some(c"/proc/cpuinfo"))
        .chain(files.locks.then_some(c"/proc/locks"))
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

/// Linux's `MAXSYMLINKS` (include/linux/namei.h): `reserve_stack` fails the
/// 41st symbolic link of one lookup with ELOOP.
const MAXSYMLINKS: usize = 40;

/// procfs's root inode number (`PROC_ROOT_INO`, fs/proc/internal.h).
const PROC_ROOT_INO: u64 = 1;

/// A descriptor the walk below opened, closed when dropped.
struct WalkFd(i32);

impl Drop for WalkFd {
    fn drop(&mut self) {
        // SAFETY: the descriptor is the walk's own, closed once.
        let _ = unsafe { raw_syscall(libc::SYS_close, [self.0 as u64, 0, 0, 0, 0, 0]) };
    }
}

/// What the walk below reads of an object it opened (`statx`).
#[derive(Clone, Copy)]
struct WalkStat {
    dev: u64,
    ino: u64,
    mode: u32,
    /// An automount point (`STATX_ATTR_AUTOMOUNT`): Linux's walk mounts on
    /// it and charges the mount to the link budget (`follow_automount`).
    automount: bool,
}

impl WalkStat {
    fn kind(&self) -> u32 {
        self.mode & libc::S_IFMT
    }
}

/// `openat(dirfd, name, O_PATH | O_NOFOLLOW)`, and what [`WalkStat`] keeps
/// of the object.
fn walk_open(dirfd: i32, name: &[u8]) -> Result<(WalkFd, WalkStat), Errno> {
    let name = std::ffi::CString::new(name).map_err(|_| Errno::ENOENT)?;
    // SAFETY: the name is NUL-terminated; the descriptor is owned below.
    let fd = raw_errno(unsafe {
        raw_syscall(
            libc::SYS_openat,
            [
                dirfd as u64,
                name.as_ptr() as u64,
                (libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64,
                0,
                0,
                0,
            ],
        )
    })? as i32;
    let fd = WalkFd(fd);
    let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: `fd` is open, the empty path is NUL-terminated and `stat` is
    // writable.
    raw_errno(unsafe {
        raw_syscall(
            libc::SYS_statx,
            [
                fd.0 as u64,
                c"".as_ptr() as u64,
                (libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT) as u64,
                (libc::STATX_TYPE | libc::STATX_MODE | libc::STATX_INO) as u64,
                stat.as_mut_ptr() as u64,
                0,
            ],
        )
    })?;
    // SAFETY: the call succeeded and initialized the structure.
    let stat = unsafe { stat.assume_init() };
    Ok((
        fd,
        WalkStat {
            dev: libc::makedev(stat.stx_dev_major, stat.stx_dev_minor),
            ino: stat.stx_ino,
            mode: u32::from(stat.stx_mode),
            automount: stat.stx_attributes & libc::STATX_ATTR_AUTOMOUNT as u64 != 0,
        },
    ))
}

/// `ST_NOSYMFOLLOW` in `statfs`'s `f_flags` (`MNT_NOSYMFOLLOW`).
const ST_NOSYMFOLLOW: u64 = 0x2000;

/// Whether the mount the symbolic link `fd` (`O_PATH`) is on refuses to
/// follow symbolic links (`nosymfollow`), or `None` if `fstatfs` fails.
fn on_nosymfollow_mount(fd: i32) -> Option<bool> {
    /// The kernel's `struct statfs` on x86_64 (include/uapi/asm-generic/
    /// statfs.h), whose `f_flags` libc keeps private.
    #[repr(C)]
    struct KernelStatfs {
        f_type: i64,
        f_bsize: i64,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        f_files: u64,
        f_ffree: u64,
        f_fsid: [i32; 2],
        f_namelen: i64,
        f_frsize: i64,
        f_flags: i64,
        f_spare: [i64; 4],
    }
    let mut stat = std::mem::MaybeUninit::<KernelStatfs>::zeroed();
    // SAFETY: `fd` is open and `stat` is writable.
    raw_errno(unsafe {
        raw_syscall(
            libc::SYS_fstatfs,
            [fd as u64, stat.as_mut_ptr() as u64, 0, 0, 0, 0],
        )
    })
    .ok()?;
    // SAFETY: the call succeeded and initialized the structure.
    let stat = unsafe { stat.assume_init() };
    Some(stat.f_flags as u64 & ST_NOSYMFOLLOW != 0)
}

/// Linux's `fs.protected_symlinks`, read now, or `None` if unreadable.
fn protected_symlinks() -> Option<bool> {
    let mut bytes = [0u8; 16];
    // SAFETY: the path is NUL-terminated; the descriptor is owned below.
    let fd = WalkFd(
        raw_errno(unsafe {
            raw_syscall(
                libc::SYS_openat,
                [
                    libc::AT_FDCWD as u64,
                    c"/proc/sys/fs/protected_symlinks".as_ptr() as u64,
                    (libc::O_RDONLY | libc::O_CLOEXEC) as u64,
                    0,
                    0,
                    0,
                ],
            )
        })
        .ok()? as i32,
    );
    // SAFETY: `fd` is open and `bytes` is writable for its length.
    let length = raw_errno(unsafe {
        raw_syscall(
            libc::SYS_read,
            [
                fd.0 as u64,
                bytes.as_mut_ptr() as u64,
                bytes.len() as u64,
                0,
                0,
                0,
            ],
        )
    })
    .ok()? as usize;
    match bytes[..length].trim_ascii() {
        b"0" => Some(false),
        b"" => None,
        _ => Some(true),
    }
}

/// The security modules that can refuse to follow a symbolic link
/// (`security_inode_follow_link`) and that each have their own directory
/// below `/proc/self/attr` when active.
const LINK_FOLLOWING_SECURITY_MODULES: [&[u8]; 3] = [b"selinux", b"smack", b"apparmor"];

/// Whether a security module that can refuse to follow a symbolic link may
/// be active ([`LINK_FOLLOWING_SECURITY_MODULES`]), or `None` if Detcore
/// cannot tell now, read through `probe`, which opens a path (`Ok`) or gives
/// the errno, and kept in `cache` once definite.
///
/// Only a definite answer is kept: a directory that opens is an active
/// module; ENOENT is an absent one, and ENOENT for `/proc/self/attr` itself
/// is a kernel with none. Any other errno (EMFILE, ENFILE, EACCES, EIO, ...)
/// says nothing about the modules: that lookup is `None`, and nothing is
/// kept, so that the next one asks again.
pub(super) fn security_module_answer(
    cache: &std::sync::OnceLock<bool>,
    probe: &dyn Fn(&[u8]) -> Result<(), Errno>,
) -> Option<bool> {
    if let Some(active) = cache.get() {
        return Some(*active);
    }
    let answer = match probe(b"/proc/self/attr") {
        Err(Errno::ENOENT) => Some(false),
        Err(_) => None,
        Ok(()) => {
            let mut answer = Some(false);
            for module in LINK_FOLLOWING_SECURITY_MODULES {
                let mut path = b"/proc/self/attr/".to_vec();
                path.extend_from_slice(module);
                match probe(&path) {
                    Ok(()) => {
                        answer = Some(true);
                        break;
                    }
                    Err(Errno::ENOENT) => {}
                    Err(_) => answer = None,
                }
            }
            answer
        }
    };
    if let Some(active) = answer {
        let _ = cache.set(active);
    }
    answer
}

/// Whether the security modules let Detcore's walk follow a link itself:
/// only a definite "none active". An active module, or an answer Detcore
/// cannot have now, leaves the lookup to the kernel.
pub(super) fn security_modules_allow_the_walk(answer: Option<bool>) -> bool {
    answer == Some(false)
}

/// [`security_module_answer`] for this process.
fn link_following_security_module() -> Option<bool> {
    static ACTIVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    security_module_answer(&ACTIVE, &|path| walk_open(libc::AT_FDCWD, path).map(|_| ()))
}

/// What Linux's `pick_link` decides before it follows the symbolic link
/// `link` (`stat`) in the directory `dir`, the link costing one of `links`
/// (fs/namei.c v6.12): `Err(ELOOP)` past [`MAXSYMLINKS`] (`reserve_stack`)
/// or on a `nosymfollow` mount (`MNT_NOSYMFOLLOW`); and `Ok(false)` where
/// the answer depends on what Detcore cannot see for the guest, so that the
/// walk is [`Walked::Unclassified`]:
/// - `may_follow_link`, for a final link followed only for a final `/`
///   (`trailing`), under `fs.protected_symlinks`, in any sticky
///   world-writable directory: Linux compares the link's owner with the
///   directory's and with the guest's own fsuid, as the kernel maps them,
///   and the owners the tool reads can be projections that differ from
///   those (two owners outside a user namespace's map both read as the
///   overflow uid), so Detcore decides from none of them;
/// - a security module that can refuse the follow
///   ([`link_following_security_module`]).
///
/// Known limit: a host BPF LSM program on `inode_follow_link` is not
/// consulted by Detcore's walk on KVM; main's kernel lookup consults it.
/// The walk only decides a `readlink` answer: it opens nothing for the
/// guest, whose own opens and stats of the path the backend takes through
/// the host kernel's lookup and its permission and `file_open` hooks.
///
/// The link's atime, which Linux touches (`touch_atime`), is touched as
/// well by Detcore's `readlinkat` of it, and is no guest-visible fact:
/// Detcore answers the guest's `stat` times itself.
fn may_follow(link: i32, dir: &WalkStat, trailing: bool, links: usize) -> Result<bool, Errno> {
    if links > MAXSYMLINKS {
        return Err(Errno::ELOOP);
    }
    let sticky_world_writable = libc::S_ISVTX | libc::S_IWOTH;
    if trailing
        && dir.mode & sticky_world_writable == sticky_world_writable
        && protected_symlinks() != Some(false)
    {
        return Ok(false);
    }
    match on_nosymfollow_mount(link) {
        Some(true) => return Err(Errno::ELOOP),
        Some(false) => {}
        None => return Ok(false),
    }
    Ok(security_modules_allow_the_walk(
        link_following_security_module(),
    ))
}

/// The bytes of the symbolic link `fd` (`O_PATH`) holds, or the kernel's name
/// of any object `fd` holds when `fd` is read through `/proc/self/fd`.
fn walk_readlink(dirfd: i32, name: &std::ffi::CStr) -> Option<Vec<u8>> {
    let mut bytes = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: the name is NUL-terminated and `bytes` is writable for its
    // length.
    let length = raw_errno(unsafe {
        raw_syscall(
            libc::SYS_readlinkat,
            [
                dirfd as u64,
                name.as_ptr() as u64,
                bytes.as_mut_ptr() as u64,
                bytes.len() as u64,
                0,
                0,
            ],
        )
    })
    .ok()? as usize;
    bytes.truncate(length);
    Some(bytes)
}

/// The kernel's name of the object descriptor `fd` holds.
fn kernel_name(fd: i32) -> Option<Vec<u8>> {
    walk_readlink(
        libc::AT_FDCWD,
        &std::ffi::CString::new(format!("/proc/self/fd/{fd}")).ok()?,
    )
}

/// The names of `path` to walk, as `link_path_walk` takes them (fs/namei.c),
/// and whether a `/` follows the last: empty names and `.` change nothing and
/// are dropped, except a final `.`, which is kept, being the last name; a
/// final `/` after another name makes that name one the lookup follows, as
/// the trailing link (`WALK_TRAILING`), and requires the object reached to
/// be a directory (`LOOKUP_FOLLOW | LOOKUP_DIRECTORY`, `lookup_last`).
fn walk_names(path: &[u8]) -> (std::collections::VecDeque<Vec<u8>>, bool) {
    let last = path
        .split(|byte| *byte == b'/')
        .rfind(|name| !name.is_empty());
    let mut names: std::collections::VecDeque<Vec<u8>> = path
        .split(|byte| *byte == b'/')
        .filter(|name| !name.is_empty() && *name != b".")
        .map(<[u8]>::to_vec)
        .collect();
    if last == Some(b".".as_slice()) {
        names.push_back(b".".to_vec());
    }
    let trailing = path.ends_with(b"/") && last.is_some_and(|last| last != b".");
    (names, trailing)
}

/// Where Detcore's own walk of a path ends.
enum Walked {
    /// The object it reaches, without following a final symbolic link.
    Reached(WalkFd),
    /// A step Detcore cannot take for the guest, so that it takes the
    /// kernel's lookup of the whole path instead (`kernel_lookup`).
    Unclassified,
}

/// Linux's lookup of the absolute path `path` (no final link followed),
/// taken one name at a time, each by `openat(O_PATH | O_NOFOLLOW)`, so that
/// no kernel lookup follows a symbolic link and Detcore counts every link
/// itself, from `links` already followed by the guest's walk to `path`.
///
/// So the count does not depend on the host: Linux keeps a lookup's count
/// across the restart of its RCU walk in reference mode (`set_nameidata`
/// once, then `path_lookupat` or `path_openat` again), which a mount or
/// rename anywhere on the host forces, so a kernel lookup of many links can
/// fail with ELOOP below the budget.
///
/// Linux's rules, as fs/namei.c v6.12 takes them: each symbolic link the
/// walk follows costs one (`reserve_stack`; ELOOP past [`MAXSYMLINKS`]); a
/// link before the last name is always followed (`walk_component`, then
/// `step_into`), and the last only before a final `/` or `.` ([`walk_names`],
/// `lookup_last`); an absolute link restarts at the root (`nd_jump_root`),
/// which is the guest's (the tool shares it where Detcore answers); an
/// empty link is ENOENT (`get_link`); `..` is the kernel's own, taken from
/// the directory reached (`handle_dots`), across mount roots and at the
/// root; and ENOTDIR, ENOENT or any other refusal is the kernel's answer to
/// that one name (`Ok(None)` here, as for any failed lookup, so that the
/// backend answers the call).
///
/// What the tool's procfs cannot show the guest's: the task directories
/// `/proc/<n>`, `/proc/self` and `/proc/thread-self` name hermit's own tasks
/// in the tool, not the guest's. `self` and `thread-self` are recognised by
/// identity, as symbolic links on the guest's procfs mount inside its root
/// (`PROC_ROOT_INO`) to `<n>` or `<n>/task/<n>`: each costs one link and
/// enters the caller's own task directory, which always exists, where the
/// walk can only climb back out by `..`. Any other step into or below a task
/// directory, a magic link above all, and any other procfs link, is
/// [`Walked::Unclassified`].
///
/// Before it follows a link the walk decides what `pick_link` does
/// ([`may_follow`]): the budget, `fs.protected_symlinks` for a trailing link
/// and `nosymfollow` mounts. An automount point, which Linux mounts on and
/// charges to the budget (`follow_automount`, from `handle_mounts`), is
/// [`Walked::Unclassified`] too. Known limit: a host BPF LSM program on
/// `inode_follow_link` is not consulted by Detcore's walk on KVM; main's
/// kernel lookup consults it ([`may_follow`]).
fn walk_counting_links(path: &[u8], mut links: usize) -> Result<Option<Walked>, Errno> {
    if !path.starts_with(b"/") {
        return Ok(Some(Walked::Unclassified));
    }
    let procfs = proc_device();
    let is_procfs_root = |stat: &WalkStat| Some(stat.dev) == procfs && stat.ino == PROC_ROOT_INO;
    let Ok((root, root_stat)) = walk_open(libc::AT_FDCWD, b"/") else {
        return Ok(None);
    };
    let mut current: Option<(WalkFd, WalkStat)> = None;
    let (mut names, mut trailing) = walk_names(path);
    // How deep below `current`, a procfs root, the walk is in the caller's
    // own task directory, entered by `self` or `thread-self`.
    let mut task_depth = 0usize;
    while let Some(name) = names.pop_front() {
        if task_depth > 0 {
            match name.as_slice() {
                b".." => task_depth -= 1,
                // A final `.`: the task directory itself, no system directory.
                b"." if names.is_empty() => return Ok(None),
                _ => return Ok(Some(Walked::Unclassified)),
            }
            continue;
        }
        let (dirfd, dir_stat) = current
            .as_ref()
            .map_or((root.0, &root_stat), |(fd, stat)| (fd.0, stat));
        let Ok((fd, stat)) = walk_open(dirfd, &name) else {
            return Ok(None);
        };
        if stat.automount {
            return Ok(Some(Walked::Unclassified));
        }
        let kind = stat.kind();
        let last = names.is_empty();
        if kind == libc::S_IFLNK && (!last || trailing) {
            links += 1;
            if !may_follow(fd.0, dir_stat, last, links)? {
                return Ok(Some(Walked::Unclassified));
            }
            let Some(target) = walk_readlink(fd.0, c"") else {
                return Ok(None);
            };
            if Some(stat.dev) == procfs {
                let numbers: Vec<&[u8]> = target.split(|byte| *byte == b'/').collect();
                let number = |name: &[u8]| !name.is_empty() && name.iter().all(u8::is_ascii_digit);
                let own_task = match numbers.as_slice() {
                    [pid] => number(pid),
                    [pid, b"task", tid] => number(pid) && number(tid),
                    _ => false,
                };
                if !is_procfs_root(dir_stat) || !own_task {
                    return Ok(Some(Walked::Unclassified));
                }
                task_depth = numbers.len();
                continue;
            }
            if target.is_empty() {
                return Ok(None);
            }
            if target.starts_with(b"/") {
                current = None;
            }
            let (linked, linked_trailing) = walk_names(&target);
            // A `/` ending the link's text, with nothing after the link,
            // makes the text's last name the trailing one too.
            trailing |= linked_trailing && last;
            for linked in linked.into_iter().rev() {
                names.push_front(linked);
            }
            continue;
        }
        if kind == libc::S_IFDIR && is_procfs_root(dir_stat) && name != b".." {
            let task = kernel_name(fd.0).is_some_and(|name| {
                name.strip_prefix(b"/proc/")
                    .is_some_and(|rest| !rest.is_empty() && rest.iter().all(u8::is_ascii_digit))
            });
            if task {
                return Ok(Some(Walked::Unclassified));
            }
        }
        current = Some((fd, stat));
    }
    if task_depth > 0 {
        return Ok(None);
    }
    let (fd, stat) = current.unwrap_or((root, root_stat));
    if trailing && stat.kind() != libc::S_IFDIR {
        // `LOOKUP_DIRECTORY`: ENOTDIR, which the backend answers.
        return Ok(None);
    }
    Ok(Some(Walked::Reached(fd)))
}

/// The kernel's lookup of the whole of the absolute path `path`, without
/// following a final symbolic link: the object, or `None`. Linux counts the
/// links of this one lookup, and a restart can inflate the count
/// ([`walk_counting_links`]); it is taken only for a path Detcore's walk
/// cannot take for the guest, as it was taken for every path before.
fn kernel_lookup(path: &[u8]) -> Option<WalkFd> {
    walk_open(libc::AT_FDCWD, path).ok().map(|(fd, _)| fd)
}

/// The name of the procfs directory that a lookup of the absolute path `path`
/// reaches, without following a final symbolic link, if it reaches one: a
/// directory on the guest's procfs mount ([`proc_device`]), named as the
/// kernel names the object it holds open (its `/proc/self/fd` link), with
/// every symbolic link and `..` on the way resolved. The tool runs in the
/// guest's mount namespace ([`GuestProcfs`]), so this is the object, and the
/// name, the guest's own lookup reaches: a user mount over part of `/proc`
/// reaches its own objects, on another device, and a symbolic link is no
/// directory. `Ok(None)` for any other object, or one the lookup does not
/// reach.
///
/// Detcore walks `path` itself ([`walk_counting_links`]), with `links`
/// symbolic links already followed by the guest's walk to it, so that one
/// `MAXSYMLINKS` budget covers the whole lookup: `Err(ELOOP)` past it. A
/// path whose walk Detcore cannot take for the guest is decided by the
/// kernel's lookup of the whole of it ([`kernel_lookup`]), which counts its
/// own links only.
pub(super) fn procfs_directory_reached(
    path: &Path,
    links: usize,
) -> Result<Option<PathBuf>, Errno> {
    let bytes = path.as_os_str().as_bytes();
    let fd = match walk_counting_links(bytes, links)? {
        None => return Ok(None),
        Some(Walked::Reached(fd)) => fd,
        Some(Walked::Unclassified) => match kernel_lookup(bytes) {
            Some(fd) => fd,
            None => return Ok(None),
        },
    };
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `fd` is open and `stat` is writable.
    if raw_errno(unsafe {
        raw_syscall(
            libc::SYS_fstat,
            [fd.0 as u64, stat.as_mut_ptr() as u64, 0, 0, 0, 0],
        )
    })
    .is_err()
    {
        return Ok(None);
    }
    // SAFETY: the call succeeded and initialized the structure.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR || proc_device() != Some(stat.st_dev) {
        return Ok(None);
    }
    Ok(kernel_name(fd.0).map(|name| PathBuf::from(std::ffi::OsString::from_vec(name))))
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

/// The mode, owner and group of the guest's procfs root, `/proc` itself, as
/// the guest's user namespace sees them, read once: the tool runs in the
/// guest's mount and user namespaces outside record and replay
/// ([`GuestProcfs`]), where Detcore serves `/proc` on a placeholder.
pub(super) fn guest_procfs_root() -> Result<(u32, u32, u32), Errno> {
    static ROOT: std::sync::OnceLock<(u32, u32, u32)> = std::sync::OnceLock::new();
    if let Some(root) = ROOT.get() {
        return Ok(*root);
    }
    let stat = GuestProcfs::stat(c"/proc")?;
    Ok(*ROOT.get_or_init(|| (stat.st_mode, stat.st_uid, stat.st_gid)))
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
    } else if rendered_path_is_owner_writable(path) {
        0o644
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
/// process's `auxv` (`proc_pid_auxv`'s `S_IRUSR`), a task's `io`
/// (`ONE("io", S_IRUSR, ...)`, fs/proc/base.c) and a process's `environ`
/// (`REG("environ", S_IRUSR, ...)`).
pub(super) fn rendered_path_is_owner_only(path: &Path) -> bool {
    crate::procfs::per_process_name(path)
        .is_some_and(|name| matches!(name.filename, "auxv" | "io" | "environ"))
}

/// Whether the rendered file `path` has Linux's mode 0644: a task's `comm`
/// (`S_IRUGO|S_IWUSR`, fs/proc/base.c:3335 and 3677 in 6.17). Detcore
/// renders it for reading only: it refuses a write open itself
/// ([`asks_to_write_comm`]), and the access checks it answers grant write
/// permission by `CAP_DAC_OVERRIDE`, as for every rendered file
/// ([`FileAccess::may_write`]), not by the owner's write bit (declared).
fn rendered_path_is_owner_writable(path: &Path) -> bool {
    crate::procfs::per_process_name(path).is_some_and(|name| name.filename == "comm")
}

/// Where Detcore's lexical walk of an absolute `/proc` spelling that holds a
/// `..` and ends in `comm` lands ([`walk_comm_alias`]): package K slice A+,
/// so that no such spelling of a `comm` Detcore renders reaches the kernel
/// beneath, whose answer to it is a host lookup's
/// (<https://github.com/rrnewton/hermit/issues/4263> is the general class).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CommAliasWalk {
    /// Not a spelling the walk decides: no `..`, not under `/proc` as
    /// spelled, or a last component other than `comm`.
    NotApplicable,
    /// The `comm` of `task`, provided every task in `checks` exists, in
    /// order (else Linux's lookup fails with ENOENT there first).
    Comm {
        checks: Vec<crate::procfs::BoundTask>,
        task: crate::procfs::BoundTask,
    },
    /// Linux's lookup fails with `errno`, provided every task in `checks`
    /// exists (else ENOENT first).
    Fails {
        checks: Vec<crate::procfs::BoundTask>,
        errno: Errno,
    },
    /// The walk passes a component Detcore does not model (a link such as
    /// `cwd` or `fd/N`, a directory it does not list, or a step out of
    /// `/proc`), after which a `..` or the last component may land anywhere,
    /// a rendered `comm` included; `checks` are the tasks it passed first.
    Undetermined(Vec<crate::procfs::BoundTask>),
}

/// A procfs directory the walk models: Linux's `proc_pid_lookup` and
/// `proc_task_lookup` decide what is below `/proc` and `task`.
#[derive(Clone, Copy)]
enum ProcNode {
    /// The root directory, `/proc`'s parent.
    Root,
    Proc,
    Pid(i32),
    TaskDir(i32),
    Task(i32, i32),
}

/// A task number as procfs's `name_to_int` takes one: decimal digits, no
/// leading zero unless the number is 0, at most `i32::MAX`. `None` for any
/// other name (which procfs's lookup of a number refuses with ENOENT).
fn proc_number(name: &str) -> Option<i32> {
    if name.is_empty()
        || !name.bytes().all(|byte| byte.is_ascii_digit())
        || (name.len() > 1 && name.starts_with('0'))
    {
        return None;
    }
    name.parse().ok()
}

/// Walk `path`, an absolute spelling, lexically through the procfs
/// directories Detcore models, for a caller whose own process and thread
/// are `own` (`self`, `thread-self`), as Linux's path walk goes through them:
/// `..` goes to the parent directory (procfs has no other way back up:
/// `self` and `thread-self` are links to `<pid>` and `<pid>/task/<tid>`, which
/// the walk follows); a number below `/proc` names a task that must exist,
/// and a number below `<pid>/task` a task of that process; a name below
/// `task` that is no number is ENOENT (`proc_task_lookup`); `comm` is a file,
/// so a component after it is ENOTDIR. Only a spelling under `/proc` as
/// spelled, holding a `..`, whose last component is `comm`
/// ([`CommAliasWalk::NotApplicable`] otherwise).
pub(super) fn walk_comm_alias(path: &Path, own: (i32, i32)) -> CommAliasWalk {
    use std::path::Component;
    let components: Vec<Component<'_>> = path
        .components()
        .filter(|component| *component != Component::CurDir)
        .collect();
    let applies = path.is_absolute()
        && components.get(1) == Some(&Component::Normal("proc".as_ref()))
        && components.contains(&Component::ParentDir)
        && components.last() == Some(&Component::Normal("comm".as_ref()));
    if !applies {
        return CommAliasWalk::NotApplicable;
    }
    let mut checks = Vec::new();
    let mut node = ProcNode::Root;
    let count = components.len();
    for (index, component) in components.iter().enumerate() {
        let last = index + 1 == count;
        let name = match component {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => continue,
            Component::ParentDir => {
                node = match node {
                    ProcNode::Root | ProcNode::Proc => ProcNode::Root,
                    ProcNode::Pid(_) => ProcNode::Proc,
                    ProcNode::TaskDir(process) => ProcNode::Pid(process),
                    ProcNode::Task(process, _) => ProcNode::TaskDir(process),
                };
                continue;
            }
            Component::Normal(name) => match name.to_str() {
                Some(name) => name,
                None => return CommAliasWalk::Undetermined(checks),
            },
        };
        node = match (node, name) {
            (ProcNode::Root, "proc") => ProcNode::Proc,
            (ProcNode::Proc, "self") => ProcNode::Pid(own.0),
            (ProcNode::Proc, "thread-self") => ProcNode::Task(own.0, own.1),
            (ProcNode::Proc, name) if name.bytes().all(|byte| byte.is_ascii_digit()) => {
                match proc_number(name) {
                    Some(process) => {
                        checks.push(crate::procfs::BoundTask {
                            process,
                            thread: None,
                        });
                        ProcNode::Pid(process)
                    }
                    None => {
                        return CommAliasWalk::Fails {
                            checks,
                            errno: Errno::ENOENT,
                        };
                    }
                }
            }
            (ProcNode::Pid(process), "task") => ProcNode::TaskDir(process),
            (ProcNode::TaskDir(process), name) => match proc_number(name) {
                Some(thread) => {
                    checks.push(crate::procfs::BoundTask {
                        process,
                        thread: Some(thread),
                    });
                    ProcNode::Task(process, thread)
                }
                None => {
                    return CommAliasWalk::Fails {
                        checks,
                        errno: Errno::ENOENT,
                    };
                }
            },
            (ProcNode::Pid(process), "comm") | (ProcNode::Task(process, _), "comm") => {
                if !last {
                    return CommAliasWalk::Fails {
                        checks,
                        errno: Errno::ENOTDIR,
                    };
                }
                let thread = match node {
                    ProcNode::Task(_, thread) => Some(thread),
                    _ => None,
                };
                return CommAliasWalk::Comm {
                    checks,
                    task: crate::procfs::BoundTask { process, thread },
                };
            }
            _ => return CommAliasWalk::Undetermined(checks),
        };
    }
    // The last component is `comm`, which every model node answers above.
    CommAliasWalk::Undetermined(checks)
}

/// What [`Detcore::resolve_comm_alias`] makes of a spelling.
pub(super) enum CommAlias {
    /// Not a spelling it decides ([`CommAliasWalk::NotApplicable`]), or
    /// Detcore does not render `comm` on this backend.
    NotApplicable,
    /// The spelling of a task's `comm`, without its `..`:
    /// `/proc/<pid>/comm` or `/proc/<pid>/task/<tid>/comm`.
    Resolved(std::path::PathBuf),
    /// [`CommAliasWalk::Undetermined`]: answered by the kernel beneath for
    /// reading, refused by name for writing
    /// ([`Detcore::refuse_undetermined_comm_write`]).
    Undetermined,
}

/// Where a `..` spelling of `/proc/locks` or of a descriptor's fdinfo lands
/// ([`walk_lock_or_fdinfo_alias`]): package K slice K8-d, so that no such
/// spelling of a file Detcore renders where the guest's procfs is not the
/// kernel's (package K slice K8-h) reaches the backend's own procfs
/// (<https://github.com/rrnewton/hermit/issues/4263> is the general class).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LockOrFdinfoAlias {
    /// Not a spelling the walk decides: not absolute, no `..`, a `..` that
    /// follows a component procfs's walk cannot undo by name (a link such as
    /// `cwd` or `fd/N`, a file, a directory procfs does not always have:
    /// [`super::descriptor_directory::resolved_procfs_dotdot`] leaves it to
    /// the kernel), or a walk that reaches neither file.
    NotApplicable,
    /// The file `file`, spelled without the `..`: `/proc/locks`, or a
    /// `/proc/<pid>[/task/<tid>]/fdinfo/<name>` spelling (`self` and
    /// `thread-self` kept), provided every task in `tasks` exists, in order.
    File {
        file: std::path::PathBuf,
        tasks: Vec<super::descriptor_directory::NamedTask>,
    },
    /// The same, with a component or a slash after the file, which Linux's
    /// walk refuses (ENOTDIR) once it has found the file.
    PastFile {
        file: std::path::PathBuf,
        tasks: Vec<super::descriptor_directory::NamedTask>,
    },
    /// A walk to `file` that follows the `self` or `thread-self` link more
    /// than `MAXSYMLINKS` times before it reaches the file: Linux's walk
    /// fails with ELOOP at the first link past the limit
    /// (`pick_link`'s `total_link_count`), provided every task in `tasks`,
    /// the ones named before that link, exists, in order.
    LinkLimit {
        file: std::path::PathBuf,
        tasks: Vec<super::descriptor_directory::NamedTask>,
    },
}

/// Walk `path`, an absolute spelling with a `..`, to `/proc/locks` or a
/// descriptor's fdinfo, as Linux's path walk resolves it through the procfs
/// directories ([`super::descriptor_directory::resolved_procfs_dotdot`]:
/// `self` and `thread-self` are links it follows first, so `/proc/self/..`
/// is `/proc`, and a `..` then goes back through procfs's own directories).
/// The first prefix of the walk that names one of the two files decides: the
/// file, where it is the whole path, or the file with something after it.
/// Empty and `.` components are dropped as the walk drops them, but a slash
/// or `/.` after the last component asks for a directory, which the file is
/// not ([`LockOrFdinfoAlias::PastFile`]). Each `self` or `thread-self` the
/// walk looks up in `/proc` before the file is a link it follows, and the
/// first one past the limit ends the walk ([`LockOrFdinfoAlias::LinkLimit`]).
pub(super) fn walk_lock_or_fdinfo_alias(path: &Path) -> LockOrFdinfoAlias {
    let bytes = path.as_os_str().as_bytes();
    if !bytes.starts_with(b"/") {
        return LockOrFdinfoAlias::NotApplicable;
    }
    let components = bytes
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty() && *component != b".")
        .collect::<Vec<_>>();
    if !components.contains(&b"..".as_slice()) {
        return LockOrFdinfoAlias::NotApplicable;
    }
    let trailing = bytes.ends_with(b"/") || bytes.ends_with(b"/.");
    let mut prefix = Vec::new();
    let mut passed_dotdot = false;
    for (index, component) in components.iter().enumerate() {
        prefix.push(b'/');
        prefix.extend_from_slice(component);
        if *component == b".." {
            passed_dotdot = true;
            continue;
        }
        let spelled = Path::new(OsStr::from_bytes(&prefix));
        let (file, tasks) = if passed_dotdot {
            match super::descriptor_directory::resolved_procfs_dotdot(spelled) {
                Some(resolved) => resolved,
                // Only the kernel's walk can go back past what came before.
                None => return LockOrFdinfoAlias::NotApplicable,
            }
        } else {
            (spelled.to_path_buf(), Vec::new())
        };
        if file != Path::new("/proc/locks") && crate::procfs::fdinfo_spelling_task(&file).is_none()
        {
            continue;
        }
        if let Some(tasks) = past_the_link_limit(&components[..index], 0) {
            return LockOrFdinfoAlias::LinkLimit { file, tasks };
        }
        return if index + 1 == components.len() && !trailing {
            LockOrFdinfoAlias::File { file, tasks }
        } else {
            LockOrFdinfoAlias::PastFile { file, tasks }
        };
    }
    LockOrFdinfoAlias::NotApplicable
}

/// Whether Linux's walk of `components` (an absolute path's, without empty
/// and `.` ones, whose `..` all resolve over procfs's own directories) follows
/// the `self` or `thread-self` link more than `MAXSYMLINKS` times: each one it
/// looks up in `/proc` is one link (`proc_self_get_link`,
/// `proc_thread_self_get_link`), counted in lookup order, after the
/// `followed` links the same lookup followed to reach the first of
/// `components` (a respelled path's descriptor link and the selector links
/// before it, `Detcore::decided_path`), which count against the same limit
/// (`pick_link`'s `total_link_count`). `Some` of the tasks named before the
/// first link past the limit, which the walk looks up first (and fails with
/// ENOENT at the first that does not exist); `None` within the limit.
pub(super) fn past_the_link_limit(
    components: &[&[u8]],
    followed: usize,
) -> Option<Vec<super::descriptor_directory::NamedTask>> {
    let mut links = followed;
    let mut prefix = Vec::new();
    let mut passed_dotdot = false;
    for component in components {
        if *component == b"self" || *component == b"thread-self" {
            let spelled = Path::new(OsStr::from_bytes(&prefix));
            // Where the walk is when it looks the name up. Every `..` before
            // the file resolves (the caller's walk found the file through
            // them), so a prefix the resolver leaves to the kernel is none.
            let (directory, tasks) = if passed_dotdot {
                super::descriptor_directory::resolved_procfs_dotdot(spelled)
                    .unwrap_or_else(|| (spelled.to_path_buf(), Vec::new()))
            } else {
                (spelled.to_path_buf(), Vec::new())
            };
            if directory == Path::new("/proc") {
                links += 1;
                if links > super::descriptor_directory::LINK_LIMIT {
                    return Some(tasks);
                }
            }
        }
        prefix.push(b'/');
        prefix.extend_from_slice(component);
        passed_dotdot |= *component == b"..";
    }
    None
}

/// Whether an open with `flags` asks to write the file it opens: a write or
/// read-write access mode, or `O_TRUNC`, which needs write permission
/// (`may_open`), but not `O_PATH`, which opens for neither.
pub(super) fn open_asks_to_write(flags: OFlag) -> bool {
    !flags.contains(OFlag::O_PATH)
        && (flags & OFlag::O_ACCMODE != OFlag::O_RDONLY || flags.contains(OFlag::O_TRUNC))
}

/// Whether an open of the rendered file `procfs` with `flags` is an open of
/// a task's `comm` for writing (package K slice A+), which Detcore refuses
/// with EACCES itself, before any injected call or request
/// (`Detcore::open_rendered_procfs`), by every route that reaches a rendered
/// `comm`: its path (`openat`, `open`, `creat`; `openat2` is Detcore's fixed
/// ENOSYS) and a descriptor link to one (`/proc/*/fd/N`, `/dev/fd/N`).
/// Detcore renders a `comm` from its record and writes no name, so the
/// record never falls behind a write it did not see, and the answer depends
/// on neither the host nor the kernel beneath (the KVM executor's own answer
/// to that path is a host lookup's: its EACCES only where the host has the
/// path). DECLARED: Linux renames the task (`comm_write`); Detcore's EACCES
/// is its named difference.
pub(super) fn asks_to_write_comm(procfs: &ProcfsFile, flags: OFlag) -> bool {
    procfs.is_task_comm() && open_asks_to_write(flags)
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

/// Whether the rendered file `path`, which a memfd's name names as a
/// placeholder, is outside the run's `--proc` level `level`: Detcore makes no
/// placeholder for such a file, whose open fails with ENOENT, so a name
/// naming one is not a placeholder Detcore made, and the memfd stays the
/// memfd it is (`Detcore::received_placeholder`,
/// `FileMetadata::discover_fd_from_current_process`).
pub(crate) fn named_file_outside_level(
    path: &Path,
    level: detcore_model::config::ProcLevel,
) -> bool {
    super::procfs_levels::absent_prefix(path, level).is_some()
}

/// The rendered file a memfd's descriptor link `link` names as its
/// placeholder ([`placeholder_link_path`]), where the session renders it
/// (`files`) and the run's `--proc` level (`level`) has it
/// ([`named_file_outside_level`]).
pub(super) fn placeholder_link_within(
    link: &[u8],
    files: RenderedFiles,
    level: detcore_model::config::ProcLevel,
) -> Option<&'static Path> {
    placeholder_link_path(link, files).filter(|path| !named_file_outside_level(path, level))
}

/// What decides another thread group's read of a rendered task's `io`
/// ([`io_access_answer`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IoAccess {
    /// Linux's `ptrace_may_access` allows the reader.
    Allowed,
    /// The kernel beneath refused it: the read is EACCES.
    Refused,
    /// The kernel beneath serves no `/proc` link but the reader's own here
    /// (KVM's provider): `CAP_SYS_PTRACE` in the reader's own effective set
    /// decides.
    CallerCapability,
    /// No `/proc` under the reader's root (a `chroot` to a tree without
    /// one): the kernel beneath is asked without a path
    /// ([`io_access_by_real_credentials`]).
    NoProcfs,
    /// A probe failed with this errno, which says nothing about access.
    Undecided(Errno),
}

/// What decides another thread group's read of a rendered task's `io`
/// ([`Detcore::io_read_allowed`]), from the reader's `readlinkat` of the
/// task's `/proc/<pid>/cwd` (`target`) and, where that fails, of its own
/// `/proc/thread-self/cwd` (`serves`). Linux 6.17 answers a readlink of a
/// task's `cwd` through `proc_pid_readlink` (fs/proc/base.c:1846-1864): EACCES
/// exactly where `proc_fd_access_allowed` (fs/proc/base.c:708-722) refuses,
/// which asks `ptrace_may_access(task, PTRACE_MODE_READ_FSCREDS)`, the one
/// check `do_io_accounting` makes (fs/proc/base.c:3036-3039); after it
/// passes, `proc_cwd_link` (fs/proc/base.c:221-235) is ENOENT for a task
/// that has dropped its `fs` (exiting, or exited and not reaped), whose io
/// Linux still reads. The probe takes no descriptor and asks nothing else
/// (an open of the task's `auxv` also checks the file's owner, which Linux
/// makes global root once the task's memory is gone, and needs a free
/// descriptor). The reader's own link, which every task that makes a call
/// has, tells whether the kernel beneath serves its `/proc` at all.
/// - `target` succeeds: allowed.
/// - `target` is ENOENT and `serves` succeeds: the task's `fs` is gone after
///   the access check passed: allowed.
/// - `target` is EACCES and `serves` succeeds (or is ENOENT): the kernel
///   beneath serves the reader's `/proc` and refused: EACCES.
/// - `serves` is EACCES, the provider's backstop for every other `/proc`
///   link (KVM): the kernel beneath cannot be asked, and the reader's
///   `CAP_SYS_PTRACE` decides.
/// - Both are ENOENT: no `/proc` under the reader's root (a `chroot` to a
///   tree without one), where Linux still reads a descriptor the reader
///   holds, as its check does not depend on the reader's root: the kernel
///   beneath is asked without a path.
/// - Any other errno, or a failed `target` with no `serves`, decides
///   nothing.
pub(super) fn io_access_answer(
    target: Result<(), Errno>,
    serves: Option<Result<(), Errno>>,
) -> IoAccess {
    match (target, serves) {
        (Ok(()), _) | (Err(Errno::ENOENT), Some(Ok(()))) => IoAccess::Allowed,
        (Err(Errno::EACCES), Some(Ok(()) | Err(Errno::ENOENT))) => IoAccess::Refused,
        (Err(Errno::EACCES | Errno::ENOENT), Some(Err(Errno::EACCES))) => {
            IoAccess::CallerCapability
        }
        (Err(Errno::ENOENT), Some(Err(Errno::ENOENT))) => IoAccess::NoProcfs,
        (Err(Errno::EACCES | Errno::ENOENT), Some(Err(errno))) => IoAccess::Undecided(errno),
        (Err(errno), _) => IoAccess::Undecided(errno),
    }
}

/// The reader's own credentials, as the kernel beneath holds them (injected
/// `getuid`, `geteuid`, `getgid`, and `setfsuid(-1)` and `setfsgid(-1)`,
/// which change nothing and return the filesystem IDs: kernel/sys.c:904-916
/// and 948-960 in 6.17), and its capability sets ([`CapabilitySets`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReaderCredentials {
    pub(super) uid: u32,
    pub(super) euid: u32,
    pub(super) fsuid: u32,
    pub(super) gid: u32,
    pub(super) fsgid: u32,
    pub(super) capabilities: CapabilitySets,
}

/// The ID the kernel reports for one its caller's user namespace does not
/// map (`from_kuid_munged`, `overflowuid`'s default): two IDs that both read
/// as it may still differ.
const OVERFLOW_ID: u32 = 65534;

/// Linux's answer to another thread group's read of a rendered task's `io`
/// where the reader's root has no kernel `/proc` to ask
/// ([`io_access_by_real_credentials`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IoAccessByCredentials {
    /// `ptrace_may_access(task, PTRACE_MODE_READ_FSCREDS)` allows it.
    Allowed,
    /// It refuses it: EACCES.
    Refused,
    /// It refuses it where the task shares the reader's user namespace and
    /// allows it where the task is in one the reader owns: the kernel beneath
    /// is asked the check itself, with the reader's filesystem credentials
    /// ([`Detcore::inject_pidfd_namespace_answer`]), and where it cannot be
    /// asked, Detcore's record of user namespaces answers
    /// ([`io_access_by_user_namespaces`]).
    AskTheKernel,
    /// What the kernel beneath answered does not decide Linux's check; the
    /// reason, for a named refusal.
    Unanswerable(&'static str),
}

/// Linux's `ptrace_may_access(task, PTRACE_MODE_READ_FSCREDS)`, the check
/// `do_io_accounting` makes (fs/proc/base.c:3036-3039 in 6.17), decided
/// without a path where the reader's root has no kernel `/proc`: from the
/// kernel beneath's `ptrace_may_access(task, PTRACE_MODE_READ_REALCREDS)`,
/// which `kcmp` asks (kernel/kcmp.c:146-166 and 223-225) of a task named
/// by its number, whatever the reader's root (`real_credentials_allow`); the
/// reader's own credentials and capability sets (`reader`); and the task's
/// permitted set (`task_permitted`, from `capget`, which asks no
/// permission).
///
/// The two modes differ in two places only (kernel/ptrace.c:276-347): the
/// caller's IDs that must match the task's (`fsuid` and `fsgid` with
/// FSCREDS, `uid` and `gid` with REALCREDS), and, in commoncap, the caller's
/// set that must cover the task's permitted set (effective with FSCREDS,
/// permitted with REALCREDS: security/commoncap.c:164-186). The rest is the
/// same in both modes: `ptrace_has_cap`, and the task's dumpability, which
/// 6.17 checks only while the task has memory (kernel/ptrace.c:341-344) and
/// 7.1 also after it has none, from the dumpability `exit_mm` saved, then
/// overridden only by `CAP_SYS_PTRACE` in the initial user namespace
/// (`task_still_dumpable`, kernel/ptrace.c:275-287 and kernel/exit.c:574
/// in 7.1); and so are the kernel's other `ptrace_access_check` hooks
/// (Yama, SELinux, AppArmor, Smack, Landlock), which read neither flag in
/// 6.17 or 7.1. So:
/// - the reader's filesystem IDs must be its real IDs (Detcore makes the
///   guest's credential calls no-ops, so they keep the IDs the guest started
///   with), else, or where an ID reads as [`OVERFLOW_ID`], so that whether
///   they are cannot be told, the credentials cannot answer, and the kernel
///   is asked the FSCREDS check itself
///   ([`Detcore::inject_pidfd_namespace_answer`]), else the read is refused
///   by name;
/// - REALCREDS refuses: FSCREDS refuses too (the effective set is within
///   the permitted set, so commoncap's FSCREDS condition implies its
///   REALCREDS one);
/// - REALCREDS allows, and the task's permitted set is within the reader's
///   effective set, or the reader has `CAP_SYS_PTRACE` in it, or the task's
///   permitted set is not within the reader's permitted set (so REALCREDS
///   passed commoncap by `ns_capable(CAP_SYS_PTRACE)`, which reads the
///   effective set in both modes): FSCREDS allows;
/// - REALCREDS allows, and the task's permitted set is within the reader's
///   permitted set but not its effective set, which lacks `CAP_SYS_PTRACE`:
///   FSCREDS refuses where the task shares the reader's user namespace and
///   allows where the task is in one the reader owns, which nothing here
///   tells apart: the kernel is asked the FSCREDS check itself, else the
///   record of user namespaces answers ([`io_access_by_user_namespaces`]).
pub(super) fn io_access_by_real_credentials(
    real_credentials_allow: bool,
    reader: ReaderCredentials,
    task_permitted: u64,
) -> IoAccessByCredentials {
    if reader.uid != reader.fsuid || reader.gid != reader.fsgid {
        return IoAccessByCredentials::Unanswerable(
            "the reader's filesystem IDs are not its real IDs, which the kernel's check by \
             number compares instead",
        );
    }
    if reader.fsuid == OVERFLOW_ID || reader.fsgid == OVERFLOW_ID {
        return IoAccessByCredentials::Unanswerable(
            "an ID of the reader's reads as the overflow ID 65534, so whether its filesystem \
             IDs are its real IDs, which the kernel's check by number compares instead, cannot \
             be told",
        );
    }
    if !real_credentials_allow {
        return IoAccessByCredentials::Refused;
    }
    let CapabilitySets {
        effective,
        permitted,
        ..
    } = reader.capabilities;
    let within_effective = task_permitted & !effective == 0;
    let within_permitted = task_permitted & !permitted == 0;
    let ptrace = effective & (1 << CAP_SYS_PTRACE) != 0;
    if within_effective || ptrace || !within_permitted {
        IoAccessByCredentials::Allowed
    } else {
        IoAccessByCredentials::AskTheKernel
    }
}

/// Linux's answer in the case [`io_access_by_real_credentials`] leaves to the
/// user namespaces ([`IoAccessByCredentials::AskTheKernel`]), where the
/// kernel cannot be asked it ([`PidfdAccess::NotAsked`]: the task has exited,
/// so its namespaces are gone, or no descriptor is free), from Detcore's
/// record of user namespaces (`scheduler::user_namespaces`): where the task's
/// namespace is from the reader's (`relation`), and, for a task below the
/// reader's namespace, whether the namespace Linux's walk asks about, the one
/// on the task's side whose parent is the reader's, may be owned by an
/// effective user ID other than the real user ID every guest task has
/// (`owner_may_differ`).
///
/// In that case the reader lacks `CAP_SYS_PTRACE` in its effective set and
/// the task's permitted set is not within it, so commoncap's subset rule
/// fails and only `ns_capable(task's namespace, CAP_SYS_PTRACE)` can allow
/// the reader (`cap_ptrace_access_check`, security/commoncap.c:164-186 in
/// 6.17). `cap_capable_helper` (security/commoncap.c:68-104) walks up from
/// the task's namespace: the reader's own namespace answers from its
/// effective set, which lacks the capability; a namespace at or above the
/// reader's level refuses; a namespace whose parent is the reader's allows
/// exactly where its owner, the effective user ID of the task that made it
/// (`create_user_ns`, kernel/user_namespace.c:82-136), is the reader's
/// effective user ID. So:
/// - the task in the reader's namespace, or anywhere but below it: refused;
/// - below it: allowed exactly where that owner is the reader's effective
///   user ID. Every guest task has the same real user ID (Detcore makes the
///   credential-setting calls no-ops), and that namespace's creator had its
///   real one as its effective one unless the record notes otherwise for
///   that namespace (`owner_may_differ`), so the owner is the reader's
///   effective ID exactly where the reader's effective ID is its real ID.
///   Where the record notes otherwise for that namespace, or the reader's
///   effective ID reads as the overflow ID, the answer cannot be told.
pub(super) fn io_access_by_user_namespaces(
    relation: crate::scheduler::user_namespaces::UserNamespaceRelation,
    owner_may_differ: bool,
    reader: ReaderCredentials,
) -> IoAccessByCredentials {
    use crate::scheduler::user_namespaces::UserNamespaceRelation;
    match relation {
        UserNamespaceRelation::Same | UserNamespaceRelation::Elsewhere => {
            IoAccessByCredentials::Refused
        }
        UserNamespaceRelation::Below if owner_may_differ => IoAccessByCredentials::Unanswerable(
            "the user namespace between the reader's and the task's was made by a task whose \
             effective user ID may not have been its real user ID, so whether the reader owns it \
             cannot be told",
        ),
        UserNamespaceRelation::Below if reader.euid == OVERFLOW_ID => {
            IoAccessByCredentials::Unanswerable(
                "the reader's effective user ID reads as the overflow ID 65534, so whether it \
                 owns the namespace above the task's cannot be told",
            )
        }
        UserNamespaceRelation::Below if reader.euid == reader.uid => IoAccessByCredentials::Allowed,
        UserNamespaceRelation::Below => IoAccessByCredentials::Refused,
    }
}

/// What a pidfd's `PIDFD_GET_USER_NAMESPACE` ioctl tells of the kernel's
/// `ptrace_may_access(task, PTRACE_MODE_READ_FSCREDS)` for its caller
/// ([`Detcore::inject_pidfd_namespace_answer`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PidfdAccess {
    /// The check allows the caller.
    Allowed,
    /// The check refuses the caller: EACCES.
    Refused,
    /// The check was not reached: `pidfd_open` failed (`opened` false), or
    /// the ioctl failed before the check (`opened` true), with `errno`.
    NotAsked { opened: bool, errno: Errno },
}

/// Why the kernel's own check could not be asked through a pidfd
/// ([`PidfdAccess::NotAsked`]), for a named refusal.
fn pidfd_not_asked_reason(opened: bool, errno: Errno) -> String {
    match (opened, errno) {
        (true, Errno::ESRCH) => {
            "the task has exited, so the kernel has no namespaces of it to ask about".to_owned()
        }
        (false, Errno::EMFILE | Errno::ENFILE) => {
            format!("the pidfd needs a free descriptor ({errno})")
        }
        (false, errno) => format!("pidfd_open failed with {errno}"),
        (true, errno) => format!("the ioctl failed with {errno}"),
    }
}

/// [`PidfdAccess`] from the ioctl's result, once `pidfd_open` has
/// succeeded. Before the check, `pidfd_ioctl` fails with ENOTTY (a request
/// it does not know), EINVAL, or ESRCH (the task is gone, or has no
/// namespaces left: it has exited); the check itself fails with EACCES;
/// after the check, the ioctl fails only where the namespace's descriptor
/// cannot be made: EOPNOTSUPP (no namespace to open) or `open_namespace`'s
/// EMFILE, ENFILE or ENOMEM (fs/pidfs.c:449-566 in 6.17, 537-667 in 7.1).
/// Those mean the check allowed the caller.
pub(super) fn pidfd_ioctl_access(answer: Result<i64, Errno>) -> PidfdAccess {
    match answer {
        Ok(_) | Err(Errno::EOPNOTSUPP | Errno::EMFILE | Errno::ENFILE | Errno::ENOMEM) => {
            PidfdAccess::Allowed
        }
        Err(Errno::EACCES) => PidfdAccess::Refused,
        Err(errno) => PidfdAccess::NotAsked {
            opened: true,
            errno,
        },
    }
}

/// Which file a refusal by the reader's own capabilities is for
/// ([`Detcore::caller_capability_answer`]), each warned about once.
#[derive(Clone, Copy, Debug)]
enum CapabilityRefusal {
    /// Another process's `/proc/<pid>/auxv`, at its open.
    Auxv,
    /// Another thread group's `/proc/<pid>/io`, at a read or a seek.
    Io,
    /// Another process's `/proc/<pid>/environ`, at its open (package K item
    /// 2b slice 2b-3).
    Environ,
}

/// Whether the run has warned that the reader's own capabilities refused
/// another process's `auxv` (`AUXV`), `io` (`IO`) or `environ` (`ENVIRON`)
/// where Linux might allow it: once each per tool process, which is once per
/// run where the kernel beneath serves no `/proc` (KVM's tool runs in
/// Hermit's process).
static CAPABILITY_REFUSAL_WARNED_AUXV: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static CAPABILITY_REFUSAL_WARNED_IO: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static CAPABILITY_REFUSAL_WARNED_ENVIRON: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether the run has warned that another process's `cmdline` or `environ`
/// was served from the strings its image's `execve` passed rather than from
/// its memory (`Detcore::read_rendered_cmdline`, the interim of minibeads
/// dh-ngy9z): once per tool process, which is once per run on the one
/// backend that serves them (KVM's tool runs in Hermit's process).
static EXEC_STRINGS_FALLBACK_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
    /// read it (0444) but a process's `auxv`'s and a task's `io`'s (0400),
    /// which only their owner may ([`rendered_path_is_owner_only`]).
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

    /// The caller's own descriptor directory (mode 0500), which it owns and
    /// may also write as a thread of the directory's task
    /// (`proc_fd_permission`), with `CAP_SYS_ADMIN` from the caller's
    /// `capabilities` (a `capget` set).
    fn own_descriptor_directory(capabilities: u64) -> Self {
        Self {
            owner: true,
            readable: true,
            dac_override: true,
            sys_admin: capabilities & (1 << CAP_SYS_ADMIN) != 0,
        }
    }

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

/// A task's effective, permitted and inheritable capability sets, their low
/// 64 bits, as `capget` reports them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CapabilitySets {
    pub(super) effective: u64,
    pub(super) permitted: u64,
    pub(super) inheritable: u64,
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
    // an owner-only file (`auxv`, `io`) needs its owner (`FileAccess::readable`).
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
    let page = crate::page::page_size() as usize;
    // `ksys_mmap_pgoff` refuses MAP_HUGETLB for a file that is not on
    // hugetlbfs before `do_mmap` looks at anything, and the offset must be
    // page aligned (`offset_in_page`).
    if flags.contains(MapFlags::MAP_HUGETLB) || len == 0 || offset as usize & (page - 1) != 0 {
        return Err(Errno::EINVAL);
    }
    // A length that cannot fit the address space fails before the file is
    // looked at (`PAGE_ALIGN` overflowing, then `__get_unmapped_area`).
    // The 47-bit user address space of x86_64 without 5-level paging, which
    // Hermit assumes: on a host with LA57, Linux would take a higher fixed
    // address and answer ENODEV where this answers ENOMEM.
    const TASK_SIZE: usize = 0x7fff_ffff_f000;
    let Some(len) = len.checked_add(page - 1).map(|length| length & !(page - 1)) else {
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
        if addr & (page - 1) != 0 {
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
    let page = crate::page::page_size();
    if offset as u64 / page > (MAX_LFS_FILESIZE - len as u64) >> page.trailing_zeros() {
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
    | MAP_32BIT_AND_ABOVE4G
    | (21 << 26) // MAP_HUGE_2MB
    | (30 << 26); // MAP_HUGE_1GB

/// `MAP_32BIT | MAP_ABOVE4G`: x86's placement flags. Every other architecture
/// defines both as 0 in `include/linux/mman.h`.
#[cfg(target_arch = "x86_64")]
const MAP_32BIT_AND_ABOVE4G: libc::c_int = libc::MAP_32BIT | 0x80;
/// `MAP_32BIT | MAP_ABOVE4G`: x86's placement flags. Every other architecture
/// defines both as 0 in `include/linux/mman.h`.
#[cfg(not(target_arch = "x86_64"))]
const MAP_32BIT_AND_ABOVE4G: libc::c_int = 0;

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
    pub(crate) fn renders_procfs<G: Guest<Self>>(&self, guest: &G) -> bool {
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
    ///
    /// A task's `io` also asks, at every `show`, whether the reader may read
    /// the task: `do_io_accounting` refuses with EACCES where
    /// `ptrace_may_access(task, PTRACE_MODE_READ_FSCREDS)` fails
    /// (fs/proc/base.c:3036-3039 in 6.17). The file's mode, 0400, is checked
    /// at the open only, so a descriptor opened while the task allowed it is
    /// refused at a later `show` once the task no longer does (it made itself
    /// not dumpable, say). The reader's own thread group needs no permission
    /// (`__ptrace_may_access`'s `same_thread_group`); another's is decided
    /// without a descriptor ([`Self::io_read_allowed`]: the kernel beneath,
    /// through the reader's `/proc` where it serves it and by the task's
    /// number where the reader's root has none; on KVM, whose provider serves
    /// no other task's link, the reader's own `CAP_SYS_PTRACE`). Bytes a
    /// `show` already buffered are read on without asking again, as Linux
    /// reads on from its buffer.
    ///
    /// `Err` with an errno is Linux's answer; any other `Err` is a probe
    /// Detcore could not finish cleanly, after the failure policy
    /// ([`Self::io_read_allowed`]).
    pub(crate) async fn require_rendered_task<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<(), Error> {
        let Some(path) = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_rendered_path())
            .ok()
            .flatten()
        else {
            return Ok(());
        };
        let Some(file) = ProcfsFile::from_rendered_path(path).filter(|file| file.shows_at_read())
        else {
            return Ok(());
        };
        match self.rendered_task_state(guest, path).await {
            // An fdinfo's `seq_show` finds no task: ENOENT (fs/proc/fd.c).
            Some((_, false)) if file.is_fdinfo() => return Err(Errno::ENOENT.into()),
            Some((_, false)) => return Err(Errno::ESRCH.into()),
            _ => {}
        }
        if file.is_process_io()
            && let Some(task) = file.bound_task()
            && self.own_detpid(guest) != detcore_model::pid::DetPid::from_raw(task.process)
        {
            // Boxed: the probe's injected calls would otherwise grow every
            // read's future, which some backends poll on a small stack.
            Box::pin(Self::io_read_allowed(guest, task)).await??;
        }
        Ok(())
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
        // <https://github.com/rrnewton/hermit/issues/4263>.) Except a
        // spelling of a task's `comm` where Detcore renders it (package K
        // slice A+): Detcore walks it itself, checking every task it passes
        // as Linux's lookup does, and answers it as the file it reaches, or
        // fails it as Linux would ([`Self::resolve_comm_alias`]). And so for
        // a spelling of `/proc/locks` or of a descriptor's fdinfo where
        // Detcore renders them (package K slice K8-d,
        // [`Self::resolve_lock_or_fdinfo_alias`]).
        if !self.renders_procfs(guest) {
            return Ok(None);
        }
        let resolved;
        // A spelling that goes on past `/proc/locks` or an fdinfo, which
        // Linux's walk refuses once it has found the file.
        let mut past_the_file = false;
        let path = if path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
        {
            match self.resolve_comm_alias(guest, path).await? {
                CommAlias::Resolved(comm) => {
                    resolved = comm;
                    resolved.as_path()
                }
                CommAlias::Undetermined => return Ok(None),
                CommAlias::NotApplicable => {
                    match self.resolve_lock_or_fdinfo_alias(guest, path).await? {
                        Some((file, past)) => {
                            past_the_file = past;
                            resolved = file;
                            resolved.as_path()
                        }
                        None => return Ok(None),
                    }
                }
            }
        } else {
            path
        };
        // A descriptor's fdinfo is a per-process file Detcore renders where
        // the guest's procfs is not the kernel's (package K slice K8-h):
        // classified so only there, so that every other backend's opens of
        // one stay the kernel's, as before.
        let classified = if self.renders_fdinfo() {
            if let Some(task) = crate::procfs::fdinfo_spelling_task(path)
                && ProcfsFile::rendered_fdinfo_at(path).is_none()
                && self.names_own_process_spelling(guest, task).await
            {
                // A name that is no descriptor number, as Linux's lookup
                // reads one (`name_to_int`): no such entry.
                return Err(Errno::ENOENT.into());
            }
            ProcfsFile::rendered_fdinfo_at(path).or_else(|| ProcfsFile::from_path(path))
        } else {
            ProcfsFile::from_path(path)
        };
        if let Some(mut procfs) = classified {
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
                if self.kernel_serves_every_comm(&procfs)
                    || self.kernel_serves_every_status(&procfs)
                    || self.kernel_serves_every_environ(&procfs)
                    || self.kernel_serves_every_address_space_file(&procfs)
                {
                    return Ok(None);
                }
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
                // The fdinfo of a descriptor the caller's table does not hold
                // has no entry (`proc_lookupfd_common`): ENOENT.
                if let Some(fd) = procfs.rendered_fdinfo_target()
                    && !Self::table_holds(guest, fd)
                {
                    return Err(Errno::ENOENT.into());
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
            if names_a_directory(path) || past_the_file {
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
    /// the backend supports seals, for a file the run's `--proc` level has
    /// ([`Self::placeholder_named`]): Detcore makes none for a file the level
    /// omits, so a memfd named for one is read as the memfd it is.
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
        // and seals decide, within the run's `--proc` level; a guest can give
        // a memfd of its own the same name and seals, a known limitation, see
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

    /// The rendered file a memfd's descriptor link `link` names as its
    /// placeholder ([`placeholder_link_path`]), where this session renders
    /// it and the run's `--proc` level has it. Detcore makes no placeholder
    /// for a file the level omits, whose open fails with ENOENT
    /// ([`named_file_outside_level`]), so a memfd named for one is the memfd
    /// it is, whichever route reaches it: a descriptor link's open, a
    /// spelling that reaches the memfd (`placeholder_reached`), or the
    /// descriptor link's text.
    pub(crate) fn placeholder_link_within_level<G: Guest<Self>>(
        &self,
        guest: &G,
        link: &[u8],
    ) -> Option<&'static Path> {
        placeholder_link_within(link, self.rendered_files(guest), self.cfg.proc_level)
    }

    /// The rendered procfs file that descriptor `fd`'s memfd is named for,
    /// read from its link, within the run's `--proc` level
    /// ([`Self::placeholder_link_within_level`]). The link is the backend's
    /// `link_path` fact where the backend's own `/proc` is not the guest's
    /// (`Detcore::own_descriptor_link_from_fact`, KVM), and otherwise the
    /// kernel's, read in the guest.
    async fn placeholder_named<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<&'static Path>, Error> {
        const TARGET_CAPACITY: usize = crate::procfs::PLACEHOLDER_LINK_CAPACITY;
        if let Some(fact) = Self::own_descriptor_link_from_fact(guest, fd) {
            return Ok(fact.and_then(|text| {
                self.placeholder_link_within_level(guest, &text[..text.len().min(TARGET_CAPACITY)])
            }));
        }
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
                self.placeholder_link_within_level(guest, &bytes)
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
    /// the name alone decides, within the run's `--proc` level
    /// ([`Self::placeholder_link_within_level`]). (A guest can give a memfd
    /// of its own the same name and seals: a known limitation, see
    /// <https://github.com/rrnewton/hermit/issues/4263>.)
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
        // so it reads an owner-only file (`auxv` or `io`, 0400) no more than it
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
        Ok(match Self::inject_capget_of(guest, 0).await? {
            Some(Ok(sets)) => Some(match set {
                CapabilitySet::Effective => sets.effective,
                CapabilitySet::Permitted => sets.permitted,
            }),
            Some(Err(_)) | None => None,
        })
    }

    /// The calling thread's permitted capability set (its low 64 bits), as
    /// an injected `capget` reports it ([`Self::inject_capget`]), for the
    /// capability part of `commit_creds`'s rule at an exec
    /// (`crate::commit_creds`).
    pub(crate) async fn inject_permitted_capabilities<G: Guest<Self>>(
        guest: &mut G,
    ) -> Result<Option<u64>, Error> {
        Self::inject_capget(guest, CapabilitySet::Permitted).await
    }

    /// The effective and permitted capability sets (their low 64 bits) of
    /// the task the kernel numbers `pid` in the caller's PID namespace, or of
    /// the calling thread for 0, as `capget` reports them (`cap_capget`, no
    /// permission asked: kernel/capability.c:105-126 and
    /// security/commoncap.c:230-243 in 6.17). `Ok(Some(Err(errno)))` where
    /// `capget` fails (ESRCH: no such task), `Ok(None)` where no stack
    /// scratch can be borrowed, with nothing changed. `Err` where the
    /// borrowed scratch could not be put back; the failure policy has then
    /// been applied (`scratch_failure`), so the caller returns it as it is,
    /// never as an ordinary answer.
    async fn inject_capget_of<G: Guest<Self>>(
        guest: &mut G,
        pid: i32,
    ) -> Result<Option<Result<CapabilitySets, Errno>>, Error> {
        // The header (8 bytes: the version, then the pid) and the data (two
        // 12-byte words) in borrowed stack scratch, whose original bytes go
        // back on every path.
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
        let words: Option<Result<[u8; 24], Errno>> = async move {
            let guest = caller;
            let header = scratch;
            let data = AddrMut::<u8>::from_raw(scratch.as_raw() + 8)?;
            let mut bytes = [0_u8; 32];
            bytes[..4].copy_from_slice(&LINUX_CAPABILITY_VERSION_3.to_ne_bytes());
            bytes[4..8].copy_from_slice(&pid.to_ne_bytes());
            guest.memory().write_exact(header, &bytes).ok()?;
            if let Err(errno) = guest
                .inject(
                    syscalls::Capget::new()
                        .with_header(AddrMut::from_raw(header.as_raw()))
                        .with_data(AddrMut::from_raw(data.as_raw())),
                )
                .await
            {
                return Some(Err(errno));
            }
            let mut read = [0_u8; 24];
            guest.memory().read_exact(data, &mut read).ok()?;
            Some(Ok(read))
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
        let words = match words {
            Some(Ok(words)) => words,
            Some(Err(errno)) => return Ok(Some(Err(errno))),
            None => return Ok(None),
        };
        let word =
            |index: usize| u32::from_ne_bytes(words[index * 4..index * 4 + 4].try_into().unwrap());
        let set = |index: usize| u64::from(word(index)) | (u64::from(word(index + 3)) << 32);
        Ok(Some(Ok(CapabilitySets {
            effective: set(0),
            permitted: set(1),
            inheritable: set(2),
        })))
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
    pub(super) async fn scratch_failure<G: Guest<Self>>(
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
        // A rendered `comm` is read only (package K slice A+): an open of
        // it for writing, by its path or a descriptor link, is Detcore's
        // EACCES, with nothing injected and nothing asked of the global tool
        // ([`asks_to_write_comm`], declared).
        if asks_to_write_comm(&procfs, flags) {
            return Err(Errno::EACCES.into());
        }
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
        // A process's `smaps`, `smaps_rollup` or `numa_maps` takes the task's
        // memory map at its open (`proc_maps_open`, `smaps_rollup_open`):
        // bind the address space the task holds now where the task is of the
        // opener's own process, whose address space it shares, which its
        // reads render (`Detcore::render_address_space_file`), as a `maps`
        // the kernel opened binds one (`memory_map::Binding`). A task of
        // another process, or one that holds no address space, binds none.
        let procfs = if procfs.lists_rendered_address_space() && !flags.contains(OFlag::O_PATH) {
            let mut procfs = procfs;
            if let Some(task) = procfs.bound_task()
                && let Some((pid, _)) = self.procfs_caller_ids(guest)
                && task.process == pid
            {
                let answer = crate::tool_global::procfs_task(guest, task).await;
                let opener = guest.thread_state().mm_id;
                if let Some(binding) =
                    crate::procfs::memory_map::Binding::at_open(task, &answer, opener)
                {
                    procfs.bind_memory_map(binding);
                }
            }
            procfs
        } else {
            procfs
        };
        // A process's `auxv`: its snapshot is the process's auxiliary vector
        // now, which the open pins (`open_process_auxv`).
        // Boxed: the access check's injected calls would otherwise grow every
        // open's future, which some backends poll on a small stack.
        let opened = if procfs.is_process_auxv() && !flags.contains(OFlag::O_PATH) {
            Some(Box::pin(self.open_process_auxv(guest, &procfs)).await?)
        } else {
            None
        };
        // A process's `environ`: Linux's open asks the same access check as
        // `auxv`'s for another thread group (`environ_open`), after the
        // owner's permission above. Boxed, as above.
        if procfs.is_process_environ()
            && !flags.contains(OFlag::O_PATH)
            && let Some(task) = procfs.bound_task()
            && !self.names_own_process(guest, task).await
        {
            Box::pin(Self::ptrace_read_allowed_for(
                guest,
                task,
                CapabilityRefusal::Environ,
            ))
            .await??;
        }
        // ... and pins the process's memory map (`__mem_open`), which an exec
        // replaces: pin the image whose strings Detcore recorded for the
        // process now, so that a read after the process has exec'd another
        // image reads nothing (`Self::read_rendered_cmdline`).
        let procfs = if procfs.is_process_environ() && !flags.contains(OFlag::O_PATH) {
            let mut procfs = procfs;
            if let Some(task) = procfs.bound_task()
                && let (_, Some((image, _))) = crate::tool_global::process_exec_strings(
                    guest,
                    detcore_model::pid::DetPid::from_raw(task.process),
                )
                .await
            {
                procfs.pin_exec_image(image);
            }
            procfs
        } else {
            procfs
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
            None if procfs.is_process_auxv() || procfs.reads_exec_strings() => None,
            None if procfs.snapshot_at_first_read() => None,
            None => {
                // `/proc/diskstats` starts from the host's table as the run
                // first read it, from the coordinator: an open that cannot
                // have it fails with the errno of the coordinator's read,
                // never with a table of no devices.
                let host_diskstats = if procfs.renders_host_diskstats() {
                    Some(crate::tool_global::host_diskstats(guest).await?)
                } else {
                    None
                };
                Some(self.render_snapshot(guest, &procfs, host_diskstats))
            }
        };
        // A process's `auxv`'s snapshot does not fit a memfd name: it stays
        // with the open file description (and `PLACEHOLDER_OPENS`). A
        // `cmdline` takes none: its open reads no memory
        // (`proc_pid_cmdline_ops` has no `open`), and every read reads the
        // memory of the process it names (`Self::read_rendered_cmdline`). Nor
        // does an `environ`, whose every read reads that memory too.
        let carried: Vec<u8> = match &snapshot {
            Some(snapshot)
                if !procfs.snapshot_is_rerendered()
                    && !procfs.is_process_auxv()
                    && !procfs.reads_exec_strings() =>
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
        let (handle, id) = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_path(path);
            detfd.set_procfs(procfs.clone());
            (detfd.open_file_handle(), detfd.open_file_id())
        })?;
        register_placeholder_open(identity, handle, id, registered);
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
        Self::ptrace_read_allowed_for(guest, task, CapabilityRefusal::Auxv).await
    }

    /// [`Self::ptrace_read_allowed`] for the open of another process's
    /// `refusal` file: `auxv`, or `environ`, whose open makes the same check
    /// on a file of the same owner and mode (`environ_open` and `auxv_open`
    /// both call `__mem_open`, fs/proc/base.c:1001-1004 and 1080-1083 in
    /// 6.17, with `PTRACE_MODE_READ`, to which `proc_mem_open` adds FSCREDS,
    /// :845), so the kernel's answer to an open of the task's `auxv` is the
    /// answer for its `environ` too. `refusal` names the file in the warning
    /// where the caller's own capability decides.
    async fn ptrace_read_allowed_for<G: Guest<Self>>(
        guest: &mut G,
        task: crate::procfs::BoundTask,
        refusal: CapabilityRefusal,
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
            AuxvAccess::CallerCapability => Self::caller_capability_answer(guest, refusal).await,
        }
    }

    /// Linux's `ptrace_may_access(task, PTRACE_MODE_READ_FSCREDS)` for the
    /// reader and `task`, the task whose rendered `io` it reads, in another
    /// thread group, as `do_io_accounting` asks it at every `show`
    /// (fs/proc/base.c:3036-3039 in 6.17): `Ok(Ok(()))` where the read reads,
    /// `Ok(Err(errno))` where it fails with `errno` (EACCES, or ESRCH for a
    /// task the kernel does not list).
    ///
    /// Decided without a descriptor, by the reader's `readlinkat` of the
    /// kernel's `/proc/<pid>/cwd` (by the kernel's number,
    /// `kernel_procfs_number`) and, where that fails, of its own
    /// `/proc/thread-self/cwd` ([`io_access_answer`]): the kernel beneath,
    /// where it serves the reader's `/proc`, applies Linux's whole rule
    /// (credentials, capabilities and commoncap's subset rule, user
    /// namespaces, dumpability) with nothing else, so a full descriptor table
    /// or a task that exited and is not reaped reads as Linux reads it. Where
    /// KVM's provider refuses every other `/proc` link with EACCES,
    /// `CAP_SYS_PTRACE` in the reader's own effective set decides
    /// ([`Self::caller_capability_answer`]): KVM's named difference, which
    /// never allows a read Linux refuses (every guest process shares one user
    /// namespace, where that capability passes every part of Linux's rule).
    ///
    /// Linux's check does not depend on the reader's root. Once the reader
    /// has changed its root (`ThreadState::changed_root`), what `/proc`
    /// names there need not be the kernel's procfs, nor number its tasks as
    /// Detcore's table does, and under a root with no `/proc` both links are
    /// ENOENT: there the kernel beneath is asked without a path
    /// ([`Self::io_read_allowed_by_number`]).
    ///
    /// With sequentialized threads every call runs in the reader's turn, so
    /// the answer is the guest's own history's. Without them
    /// (`--no-sequentialize-threads`, a declared relaxation) the task may
    /// change its credentials or dumpability concurrently, and the answer is
    /// host-timed.
    ///
    /// `Err` is a probe Detcore could not finish cleanly (no stack scratch,
    /// scratch it could not put back, an errno that decides nothing, the
    /// reader's capabilities unreadable), or a case Linux's answer cannot be
    /// decided in, after the failure policy (`scratch_failure`): never an
    /// access answer.
    // TODO-HUMAN-REVIEW(#4213): Review the read-time access check of another
    // thread group's rendered /proc/<pid>/io by a readlink of its cwd.
    pub(crate) async fn io_read_allowed<G: Guest<Self>>(
        guest: &mut G,
        task: crate::procfs::BoundTask,
    ) -> Result<Result<(), Errno>, Error> {
        let named = task.thread.unwrap_or(task.process);
        if guest
            .thread_state()
            .changed_root
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Box::pin(Self::io_read_allowed_by_number(guest, named)).await;
        }
        let Some(kernel) = Self::kernel_procfs_number(guest, named).await else {
            return Ok(Err(Errno::ESRCH));
        };
        let target = Self::readlink_probe(guest, format!("/proc/{kernel}/cwd").as_bytes()).await?;
        let serves = match target {
            Err(Errno::EACCES | Errno::ENOENT) => {
                Some(Self::readlink_probe(guest, b"/proc/thread-self/cwd").await?)
            }
            _ => None,
        };
        match io_access_answer(target, serves) {
            IoAccess::Allowed => Ok(Ok(())),
            IoAccess::Refused => Ok(Err(Errno::EACCES)),
            IoAccess::CallerCapability => {
                Self::caller_capability_answer(guest, CapabilityRefusal::Io).await
            }
            IoAccess::NoProcfs => Box::pin(Self::io_read_allowed_by_number(guest, named)).await,
            IoAccess::Undecided(errno) => Err(Self::scratch_failure(
                guest,
                reverie::syscalls::Sysno::read,
                format!(
                    "the access check of a read or seek of another process's /proc/{named}/io \
                     could not be decided: its readlinkat probe failed with {errno}"
                ),
            )
            .await),
        }
    }

    /// [`Self::io_read_allowed`] for a reader whose root may have no kernel
    /// `/proc` (it changed its root, or neither link reads): the kernel
    /// beneath's `ptrace_may_access(task, PTRACE_MODE_READ_REALCREDS)`, asked
    /// by an injected `kcmp(pid, pid, KCMP_VM)` of the task's number in the
    /// reader's PID namespace (`tool_global::procfs_ids`), which takes no
    /// descriptor and no path (kernel/kcmp.c:146-166 and 223-225 in 6.17:
    /// ESRCH for no such task, EPERM where the check refuses); the reader's
    /// credentials and capability sets and the task's permitted set; and from
    /// them Linux's FSCREDS answer ([`io_access_by_real_credentials`]). A `kcmp`
    /// EPERM is the check's only where `kcmp` of the reader's own thread,
    /// which the check always allows, succeeds; otherwise the call itself is
    /// unavailable to the guest. In the one case where the user namespaces
    /// decide, the kernel is asked the FSCREDS check itself, through a pidfd
    /// ([`Self::inject_pidfd_namespace_answer`]), which takes two descriptors
    /// for the call's length; where it cannot be asked (the task has exited,
    /// so its namespaces are gone, or no descriptor is free), Detcore's
    /// record of user namespaces answers ([`io_access_by_user_namespaces`]),
    /// which the clones that made each namespace fixed in their turns. Where
    /// the reader's credentials cannot tell the FSCREDS answer from the
    /// REALCREDS one (its filesystem IDs are not its real IDs, or read as the
    /// overflow ID, as every ID does in a user namespace with no mapping),
    /// the kernel is asked the FSCREDS check through the pidfd too.
    ///
    /// Where that cannot decide Linux's answer (a nested PID namespace, whose
    /// numbers are not the table's; `kcmp` or `capget` unavailable; the cases
    /// [`io_access_by_real_credentials`] and [`io_access_by_user_namespaces`]
    /// name, where the pidfd cannot be asked either), the run stops with a
    /// named refusal (the failure policy), never an EACCES Linux might not
    /// give.
    async fn io_read_allowed_by_number<G: Guest<Self>>(
        guest: &mut G,
        named: i32,
    ) -> Result<Result<(), Errno>, Error> {
        let refuse = |detail: String| {
            format!(
                "reading another process's /proc/{named}/io after the reader changed its root \
                 to one without the kernel's /proc is not supported here: {detail}"
            )
        };
        if guest.thread_state().nested_pid_namespace {
            let reason = refuse(
                "the reader is in a nested PID namespace, whose numbers are not \
                                 Detcore's"
                    .to_owned(),
            );
            return Err(Self::scratch_failure(guest, Sysno::read, reason).await);
        }
        let Some(kernel) = crate::tool_global::procfs_ids(guest, true, vec![named])
            .await
            .into_iter()
            .next()
            .flatten()
        else {
            return Ok(Err(Errno::ESRCH));
        };
        let real_credentials_allow = match Self::inject_kcmp_vm(guest, kernel).await {
            Ok(_) => true,
            Err(Errno::ESRCH) => return Ok(Err(Errno::ESRCH)),
            Err(Errno::EPERM) => {
                let own = guest.inject(syscalls::Gettid::new()).await;
                match own {
                    Ok(own) => match Self::inject_kcmp_vm(guest, own as i32).await {
                        Ok(_) => false,
                        Err(errno) => {
                            let reason = refuse(format!(
                                "kcmp, which asks the kernel's check by number, is unavailable \
                                 (kcmp of the reader's own thread failed with {errno})"
                            ));
                            return Err(Self::scratch_failure(guest, Sysno::read, reason).await);
                        }
                    },
                    Err(errno) => {
                        let reason = refuse(format!("gettid failed with {errno}"));
                        return Err(Self::scratch_failure(guest, Sysno::read, reason).await);
                    }
                }
            }
            Err(errno) => {
                let reason = refuse(format!(
                    "kcmp, which asks the kernel's check by number, failed with {errno}"
                ));
                return Err(Self::scratch_failure(guest, Sysno::read, reason).await);
            }
        };
        let reader = match Self::inject_reader_credentials(guest).await? {
            Ok(reader) => reader,
            Err(detail) => {
                let reason = refuse(detail);
                return Err(Self::scratch_failure(guest, Sysno::read, reason).await);
            }
        };
        let task_permitted = match Self::inject_capget_of(guest, kernel).await? {
            Some(Ok(sets)) => sets.permitted,
            Some(Err(Errno::ESRCH)) => return Ok(Err(Errno::ESRCH)),
            Some(Err(errno)) => {
                let reason = refuse(format!("capget of the task failed with {errno}"));
                return Err(Self::scratch_failure(guest, Sysno::read, reason).await);
            }
            None => {
                let reason = refuse("no guest stack scratch can hold capget's data".to_owned());
                return Err(Self::scratch_failure(guest, Sysno::read, reason).await);
            }
        };
        match io_access_by_real_credentials(real_credentials_allow, reader, task_permitted) {
            IoAccessByCredentials::Allowed => Ok(Ok(())),
            IoAccessByCredentials::Refused => Ok(Err(Errno::EACCES)),
            IoAccessByCredentials::AskTheKernel => {
                match Self::inject_pidfd_namespace_answer(guest, kernel).await {
                    Ok(PidfdAccess::Allowed) => Ok(Ok(())),
                    Ok(PidfdAccess::Refused) => Ok(Err(Errno::EACCES)),
                    Ok(PidfdAccess::NotAsked { opened, errno }) => {
                        // The kernel cannot be asked: Detcore's record of
                        // user namespaces answers.
                        let (relation, owner_may_differ) =
                            crate::tool_global::user_namespace_relation(
                                guest,
                                crate::types::DetTid::from_raw(named),
                            )
                            .await;
                        match io_access_by_user_namespaces(relation, owner_may_differ, reader) {
                            IoAccessByCredentials::Allowed => Ok(Ok(())),
                            IoAccessByCredentials::Refused => Ok(Err(Errno::EACCES)),
                            IoAccessByCredentials::AskTheKernel => {
                                unreachable!("the record of user namespaces answers by itself")
                            }
                            IoAccessByCredentials::Unanswerable(detail) => {
                                let why = pidfd_not_asked_reason(opened, errno);
                                let reason = refuse(format!(
                                    "the task's permitted capabilities are within the reader's \
                                     permitted set but not its effective set, which lacks \
                                     CAP_SYS_PTRACE, so Linux's answer turns on the user \
                                     namespaces; the kernel's own check, asked through a \
                                     pidfd's user-namespace ioctl, cannot answer ({why}), and \
                                     Detcore's record of user namespaces cannot either: \
                                     {detail}"
                                ));
                                Err(Self::scratch_failure(guest, Sysno::read, reason).await)
                            }
                        }
                    }
                    Err(reason) => Err(Self::scratch_failure(guest, Sysno::read, reason).await),
                }
            }
            IoAccessByCredentials::Unanswerable(detail) => {
                // The credentials cannot tell Linux's answer: the kernel is
                // asked the FSCREDS check itself, which reads the reader's
                // filesystem IDs whatever they read as from its namespace.
                match Self::inject_pidfd_namespace_answer(guest, kernel).await {
                    Ok(PidfdAccess::Allowed) => Ok(Ok(())),
                    Ok(PidfdAccess::Refused) => Ok(Err(Errno::EACCES)),
                    Ok(PidfdAccess::NotAsked { opened, errno }) => {
                        let why = pidfd_not_asked_reason(opened, errno);
                        let reason = refuse(format!(
                            "{detail}; and the kernel's own check, asked through a pidfd's \
                             user-namespace ioctl, cannot answer ({why})"
                        ));
                        Err(Self::scratch_failure(guest, Sysno::read, reason).await)
                    }
                    Err(reason) => Err(Self::scratch_failure(guest, Sysno::read, reason).await),
                }
            }
        }
    }

    /// The kernel beneath's `ptrace_may_access(task, PTRACE_MODE_READ_FSCREDS)`
    /// for the caller and the task the kernel numbers `pid` in the caller's
    /// PID namespace, asked with a pidfd (`pidfd_open(pid, PIDFD_THREAD)`)
    /// and its `PIDFD_GET_USER_NAMESPACE` ioctl, which makes exactly that
    /// check before it opens the task's user namespace (fs/pidfs.c:471-491 in
    /// 6.17, 558-578 in 7.1), and needs no path ([`pidfd_ioctl_access`]). It
    /// cannot be asked where `pidfd_open` fails (EMFILE or ENFILE: no free
    /// descriptor) or the task has exited (the ioctl's ESRCH: its namespaces
    /// are gone). It takes up to two descriptors for the call's length, each
    /// the lowest free one, and closes both before the guest runs again; with
    /// sequentialized threads no other guest thread runs in between. A call
    /// Reverie reports did not run (ERESTARTSYS) is retried; a close that
    /// fails otherwise than with EINTR, after which Linux has released the
    /// descriptor, is `Err`, a probe that could not be finished cleanly.
    async fn inject_pidfd_namespace_answer<G: Guest<Self>>(
        guest: &mut G,
        pid: i32,
    ) -> Result<PidfdAccess, String> {
        /// `PIDFD_THREAD` (`O_EXCL`), so that a thread's pidfd opens too.
        const PIDFD_THREAD: usize = libc::O_EXCL as usize;
        /// `PIDFD_GET_USER_NAMESPACE`, `_IO(PIDFS_IOCTL_MAGIC, 9)`
        /// (include/uapi/linux/pidfd.h).
        const PIDFD_GET_USER_NAMESPACE: usize = 0xff09;
        async fn retried<G: Guest<Detcore<T>>, T: RecordOrReplay>(
            guest: &mut G,
            call: Syscall,
        ) -> Result<i64, Errno> {
            loop {
                match guest.inject(call).await {
                    Err(Errno::ERESTARTSYS) => continue,
                    answer => return answer,
                }
            }
        }
        async fn close<G: Guest<Detcore<T>>, T: RecordOrReplay>(
            guest: &mut G,
            fd: RawFd,
            what: &str,
        ) -> Result<(), String> {
            loop {
                match guest.inject(syscalls::Close::new().with_fd(fd)).await {
                    Ok(_) | Err(Errno::EINTR) => return Ok(()),
                    Err(Errno::ERESTARTSYS) => continue,
                    Err(errno) => {
                        return Err(format!(
                            "the {what} descriptor {fd} of the access probe could not be closed: \
                             {errno}"
                        ));
                    }
                }
            }
        }
        let raw = |values: [usize; 6]| super::procfs_levels::raw_args(values);
        let pidfd = match retried(
            guest,
            Syscall::from_raw(
                Sysno::pidfd_open,
                raw([pid as usize, PIDFD_THREAD, 0, 0, 0, 0]),
            ),
        )
        .await
        {
            Ok(fd) => fd as RawFd,
            Err(errno) => {
                return Ok(PidfdAccess::NotAsked {
                    opened: false,
                    errno,
                });
            }
        };
        let answer = retried(
            guest,
            Syscall::from_raw(
                Sysno::ioctl,
                raw([pidfd as usize, PIDFD_GET_USER_NAMESPACE, 0, 0, 0, 0]),
            ),
        )
        .await;
        if let Ok(namespace) = answer {
            close(guest, namespace as RawFd, "user namespace").await?;
        }
        close(guest, pidfd, "pidfd").await?;
        Ok(pidfd_ioctl_access(answer))
    }

    /// The kernel beneath's answer to the caller's `kcmp(pid, pid, KCMP_VM,
    /// 0, 0)`, which compares the task's memory map with itself once
    /// `ptrace_may_access(task, PTRACE_MODE_READ_REALCREDS)` allows the
    /// caller (kernel/kcmp.c:146-185 in 6.17). It reads nothing of the
    /// guest's memory and takes no descriptor. A call Reverie reports did not
    /// run (ERESTARTSYS) is retried.
    async fn inject_kcmp_vm<G: Guest<Self>>(guest: &mut G, pid: i32) -> Result<i64, Errno> {
        /// `KCMP_VM` (include/uapi/linux/kcmp.h).
        const KCMP_VM: i32 = 1;
        loop {
            match guest
                .inject(
                    syscalls::Kcmp::new()
                        .with_pid1(pid)
                        .with_pid2(pid)
                        .with_typ(KCMP_VM),
                )
                .await
            {
                Err(Errno::ERESTARTSYS) => continue,
                answer => return answer,
            }
        }
    }

    /// The reader's credentials as the kernel beneath holds them, and its
    /// capability sets ([`ReaderCredentials`]): `Ok(Err(detail))` where they
    /// cannot be read, with what failed.
    async fn inject_reader_credentials<G: Guest<Self>>(
        guest: &mut G,
    ) -> Result<Result<ReaderCredentials, String>, Error> {
        // `setfsuid(-1)` and `setfsgid(-1)` name no ID, so they change
        // nothing and return the current filesystem IDs.
        let no_id = u32::MAX as usize;
        let mut ids = [0_u32; 5];
        for (slot, call) in ids.iter_mut().zip([
            Syscall::from_raw(Sysno::getuid, super::procfs_levels::raw_args([0; 6])),
            Syscall::from_raw(Sysno::geteuid, super::procfs_levels::raw_args([0; 6])),
            Syscall::from_raw(
                Sysno::setfsuid,
                super::procfs_levels::raw_args([no_id, 0, 0, 0, 0, 0]),
            ),
            Syscall::from_raw(Sysno::getgid, super::procfs_levels::raw_args([0; 6])),
            Syscall::from_raw(
                Sysno::setfsgid,
                super::procfs_levels::raw_args([no_id, 0, 0, 0, 0, 0]),
            ),
        ]) {
            let number = call.number();
            match guest.inject(call).await {
                Ok(id) => *slot = id as u32,
                Err(errno) => return Ok(Err(format!("{number} failed with {errno}"))),
            }
        }
        let capabilities = match Self::inject_capget_of(guest, 0).await? {
            Some(Ok(sets)) => sets,
            Some(Err(errno)) => {
                return Ok(Err(format!(
                    "capget of the reader's own capabilities failed with {errno}"
                )));
            }
            None => {
                return Ok(Err(
                    "no guest stack scratch can hold capget's data".to_owned()
                ));
            }
        };
        let [uid, euid, fsuid, gid, fsgid] = ids;
        Ok(Ok(ReaderCredentials {
            uid,
            euid,
            fsuid,
            gid,
            fsgid,
            capabilities,
        }))
    }

    /// `CAP_SYS_PTRACE` in the caller's own effective set (`capget`), which
    /// decides `refusal`'s file where the kernel beneath serves no `/proc`:
    /// `Ok(Ok(()))` with it, `Ok(Err(EACCES))` without it. A refusal is
    /// named once per tool process at Hermit's default level (WARN): Linux
    /// also allows a reader with the task's credentials, a dumpable task and
    /// the task's permitted capabilities. `Err` where the capabilities
    /// cannot be read, after the failure policy.
    async fn caller_capability_answer<G: Guest<Self>>(
        guest: &mut G,
        refusal: CapabilityRefusal,
    ) -> Result<Result<(), Errno>, Error> {
        let (file, sysno, warned) = match refusal {
            CapabilityRefusal::Auxv => (
                "auxv",
                reverie::syscalls::Sysno::openat,
                &CAPABILITY_REFUSAL_WARNED_AUXV,
            ),
            CapabilityRefusal::Io => (
                "io",
                reverie::syscalls::Sysno::read,
                &CAPABILITY_REFUSAL_WARNED_IO,
            ),
            CapabilityRefusal::Environ => (
                "environ",
                reverie::syscalls::Sysno::openat,
                &CAPABILITY_REFUSAL_WARNED_ENVIRON,
            ),
        };
        match Self::inject_capget(guest, CapabilitySet::Effective).await? {
            Some(effective) if effective & (1 << CAP_SYS_PTRACE) != 0 => Ok(Ok(())),
            Some(_) => {
                if !warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    warn!(
                        "[detcore] another process's /proc/<pid>/{file} is refused with EACCES \
                         because the caller has no CAP_SYS_PTRACE: the kernel beneath serves no \
                         /proc here, so the caller's own capability decides, where Linux's \
                         ptrace_may_access also allows a caller with the target's credentials, \
                         a dumpable target and its permitted capabilities (named once per run)"
                    );
                }
                Ok(Err(Errno::EACCES))
            }
            None => Err(Self::scratch_failure(
                guest,
                sysno,
                format!(
                    "the caller's capabilities, which decide another process's \
                     /proc/<pid>/{file} where the kernel beneath serves no /proc, could not be \
                     read"
                ),
            )
            .await),
        }
    }

    /// [`Self::kernel_procfs_readlink_answer`], with the failure policy
    /// applied to a probe Detcore could not finish cleanly.
    async fn readlink_probe<G: Guest<Self>>(
        guest: &mut G,
        path: &[u8],
    ) -> Result<Result<(), Errno>, Error> {
        match Self::kernel_procfs_readlink_answer(guest, path).await {
            Ok(answer) => Ok(answer),
            Err(reason) => {
                Err(Self::scratch_failure(guest, reverie::syscalls::Sysno::read, reason).await)
            }
        }
    }

    /// The kernel beneath's answer to the caller's `readlinkat` of `path` (a
    /// kernel `/proc` link): `Ok(Ok(()))` where it reads the link (truncated
    /// to the buffer, which the probe does not look at), `Ok(Err(errno))`
    /// where it fails with `errno`. It takes no descriptor. A call Reverie
    /// reports did not run (ERESTARTSYS) is retried, so that errno never
    /// reaches the guest. The path and the buffer live in borrowed stack
    /// scratch whose original bytes go back on every path
    /// ([`Self::borrow_scratch`]). `Err` (no scratch for them, scratch not
    /// put back) is a probe that could not be finished cleanly.
    async fn kernel_procfs_readlink_answer<G: Guest<Self>>(
        guest: &mut G,
        path: &[u8],
    ) -> Result<Result<(), Errno>, String> {
        const PATH_CAPACITY: usize = 32;
        const TARGET_CAPACITY: usize = 32;
        let (scratch, original, stack_guard) =
            match Self::borrow_scratch::<G, { PATH_CAPACITY + TARGET_CAPACITY }>(guest).await? {
                Some(borrowed) => borrowed,
                None => {
                    return Err(format!(
                        "no guest stack scratch can hold the access probe of {}",
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
            // SAFETY: the scratch holds PATH_CAPACITY + TARGET_CAPACITY bytes.
            let target = unsafe { scratch.add(PATH_CAPACITY) };
            loop {
                match guest
                    .inject(
                        syscalls::Readlinkat::new()
                            .with_dirfd(libc::AT_FDCWD)
                            .with_path(PathPtr::from_ptr(scratch.as_raw() as *const libc::c_char))
                            .with_buf(Some(target.cast::<libc::c_char>()))
                            .with_buf_len(TARGET_CAPACITY),
                    )
                    .await
                {
                    Err(Errno::ERESTARTSYS) => continue,
                    Ok(_) => return Ok(Ok(())),
                    Err(errno) => return Ok(Err(errno)),
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
    /// instruction), a fact of the machine description, or, for
    /// `/proc/diskstats`, `host_diskstats`, the host's block-device table as
    /// the run first read it (`tool_global::host_diskstats`), which the
    /// caller asked for.
    fn render_snapshot<G: Guest<Self>>(
        &self,
        guest: &mut G,
        procfs: &ProcfsFile,
        host_diskstats: Option<Vec<u8>>,
    ) -> Vec<u8> {
        use rand::RngExt as _;
        let mut file = procfs.clone();
        let random_uuid = file
            .needs_random_uuid()
            .then(|| guest.thread_state_mut().thread_prng().random::<[u8; 16]>());
        let virtual_pty_count = guest
            .thread_state()
            .count_open_files_at_paths(&[Path::new("/dev/ptmx"), Path::new("/dev/pts/ptmx")]);
        let virtual_cpuinfo = file.is_cpuinfo().then(|| {
            self.virtual_cpuid()
                .cpuinfo(guest.config().virtual_cpu_count())
        });
        let virtual_arch_status = file
            .is_arch_status()
            .then(|| self.virtual_cpuid().arch_status());
        let template = match host_diskstats {
            Some(table) if file.renders_host_diskstats() => table,
            _ => file.rendering_template().to_vec(),
        };
        file.initialize(
            template,
            crate::procfs::ProcfsSnapshotContext {
                random_uuid,
                virtual_pty_count,
                virtual_cpuinfo,
                virtual_arch_status,
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
            if !self.renders_procfs(guest)
                || self.kernel_serves_every_comm(&file)
                || self.kernel_serves_every_status(&file)
                || self.kernel_serves_every_environ(&file)
                || self.kernel_serves_every_address_space_file(&file)
            {
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
    /// And whether `file` is a process's `cmdline` or `environ` that the
    /// kernel serves: the caller's own where Detcore recorded no argument
    /// area for its image (the same backends and stacks as for `auxv`), and
    /// another process's, whose memory Detcore cannot read (Linux reads the
    /// target's memory at each read), except where Detcore serves it from the
    /// strings the image's `execve` passed ([`Self::serves_exec_strings`]:
    /// one it recorded them for, or one that has exited).
    ///
    /// And whether `file` is a process's `stat` that the kernel serves: where
    /// the name's source is Detcore's record (`Self::comm_source`), a task's
    /// before a name is recorded; any task's of the process table is
    /// rendered otherwise, whichever process reads it
    /// (`procfs::task_stat::renders_task_stat`, package K slices D4c-1 and
    /// 2b-1).
    ///
    /// And whether `file` is a task's `comm` that the kernel serves: every
    /// one where the name's source is the kernel
    /// ([`Self::kernel_serves_every_comm`]), and elsewhere a task's before a
    /// name is recorded (`procfs::task_stat::renders_task_comm`, package K
    /// slices A+ and 2b-1).
    async fn kept_by_kernel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        file: &ProcfsFile,
        without_mm: bool,
    ) -> bool {
        let Some(task) = file.bound_task() else {
            return false;
        };
        // A descriptor's fdinfo (package K slice K8-h): Detcore renders the
        // caller's own process's, whose descriptor table is the one the
        // backend's `descriptor_status` fact and the injected calls reach;
        // another process's stays the backend's.
        if file.is_fdinfo() {
            return !self.names_own_process(guest, task).await;
        }
        // A task's `comm` (package K slice A+): Detcore renders only the
        // reader's own process's, from its record, where the record is the
        // name's source (`procfs::task_stat::renders_task_comm`).
        if file.is_task_comm() {
            if self.kernel_serves_every_comm(file) {
                return true;
            }
            let facts = crate::tool_global::task_stat_facts(guest, task).await;
            return !crate::procfs::task_stat::renders_task_comm(&facts, self.comm_source());
        }
        // A task's `status` (package K item 2b slice 2b-2): where Detcore
        // renders any (`Self::kernel_serves_every_status`), every task's of
        // the process table but the reader's own process's where the name's
        // source is the kernel, whose file stays the kernel's, sanitized, as
        // before (`procfs::task_status::renders_task_status`).
        if file.is_task_status() {
            if self.kernel_serves_every_status(file) {
                return true;
            }
            let facts = crate::tool_global::task_stat_facts(guest, task).await;
            let own = facts.process == Some(self.own_detpid(guest).as_raw());
            return !crate::procfs::task_status::renders_task_status(
                &facts,
                self.comm_source(),
                own,
            );
        }
        if file.is_task_stat() {
            let facts = crate::tool_global::task_stat_facts(guest, task).await;
            let own = self.own_detpid(guest).as_raw();
            let rendered = crate::procfs::task_stat::renders_task_stat(&facts, self.comm_source());
            // The guest-visible parent of a process whose real parent is
            // outside the guest's tree (the run's root, and a CLONE_PARENT
            // child of such a process), which no other record of Detcore's
            // holds, is its own getppid answer, recorded here, in its own
            // turn, before any descriptor of its rendered stat exists: a
            // descriptor that reaches another process (across fork or
            // SCM_RIGHTS) renders it (`Scheduler::guest_visible_parent`).
            // Such a process also records it at its first clone
            // (`Detcore::record_external_parent_at_clone`), before any
            // process it creates can read its stat; another process's
            // stat, read before either, shows the parent as 0, the parent
            // outside the namespace. The global tool ignores it for a
            // process whose model knows its parent.
            if rendered
                && facts.process == Some(own)
                && facts.parent.is_none()
                && let Ok(parent) = guest.inject(syscalls::Getppid::new()).await
            {
                crate::tool_global::record_external_parent(
                    guest,
                    detcore_model::pid::DetPid::from_raw(own),
                    parent as i32,
                )
                .await;
            }
            return !rendered;
        }
        let process = detcore_model::pid::DetPid::from_raw(task.process);
        if file.reads_exec_strings() {
            let (process, area) = crate::tool_global::process_cmdline(guest, process).await;
            if process == self.own_detpid(guest) {
                return area.is_none();
            }
            // Another process's, where Detcore serves it: from the strings
            // its image's `execve` passed, or, for a process that has exited,
            // as a task without memory, which reads nothing
            // (`Self::read_rendered_cmdline`).
            return !self.serves_exec_strings()
                || (!without_mm
                    && crate::tool_global::process_exec_strings(guest, process)
                        .await
                        .1
                        .is_none());
        }
        if !file.is_process_auxv() || without_mm {
            return false;
        }
        crate::tool_global::process_auxv(guest, process)
            .await
            .is_none()
    }

    /// Where a rendered `stat`'s name comes from on this backend
    /// (`procfs::task_stat::CommSource::of_backend`): the kernel beneath's
    /// own where its guest threads are host tasks the tool can name (the
    /// declared `tool_can_name_guest_descriptors` capability: ptrace, DBT,
    /// SaBRe, LiteInst), Detcore's record elsewhere (KVM).
    pub(super) fn comm_source(&self) -> crate::procfs::task_stat::CommSource {
        crate::procfs::task_stat::CommSource::of_backend(
            crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend),
        )
    }

    /// Whether `file` is a task's `comm` and the name's source on this
    /// backend is the kernel's own ([`Self::comm_source`]): then every
    /// spelling of every task's `comm`, its lookup, metadata and listing
    /// included, is the kernel's, unclassified, exactly as before Detcore
    /// rendered any (package K slice A+), decided before any request to the
    /// global tool, so those backends pay nothing for the file.
    pub(super) fn kernel_serves_every_comm(&self, file: &ProcfsFile) -> bool {
        file.is_task_comm() && self.comm_source() == crate::procfs::task_stat::CommSource::Kernel
    }

    /// Whether Detcore renders any task's `status` on this backend (package
    /// K item 2b slice 2b-2): where the backend reports every guest task's
    /// signal sets (`Guest::signal_sets`), which the file shows: where the
    /// guest's threads are host tasks the tool reads
    /// (`host_task_reads_reach_guest_threads`: ptrace) and where the
    /// backend delivers the guest's signals from the state it keeps, which
    /// is where the name's source is Detcore's record ([`Self::comm_source`]:
    /// KVM). Decided by the declared capabilities alone, never by which
    /// backend it is.
    pub(super) fn renders_any_status(&self) -> bool {
        self.cfg.backend.host_task_reads_reach_guest_threads
            || self.comm_source() == crate::procfs::task_stat::CommSource::Record
    }

    /// Whether `file` is a task's `status` and Detcore renders none on this
    /// backend ([`Self::renders_any_status`]): then every spelling of every
    /// task's `status`, its lookup, metadata and listing included, is the
    /// kernel's, sanitized, exactly as before Detcore rendered any (package
    /// K item 2b slice 2b-2), decided before any request to the global tool.
    pub(super) fn kernel_serves_every_status(&self, file: &ProcfsFile) -> bool {
        file.is_task_status() && !self.renders_any_status()
    }

    /// Whether Detcore serves another process's `cmdline` and `environ`, and
    /// the reader's own `environ`, on this backend (package K item 2b slice
    /// 2b-3): where the tool cannot read another process's host task (the
    /// declared `tool_can_name_guest_descriptors` capability is false: KVM,
    /// the capability [`Self::comm_source`] reads), so the kernel beneath
    /// cannot answer for the guest's process. Decided by the declared
    /// capability alone, never by which backend it is.
    ///
    /// Linux has one semantics for both files: they show the process's
    /// argument and environment areas as they are in its memory at the read,
    /// a `setproctitle` or an in-place rewrite included. Reading that memory
    /// is the target behaviour on every backend. ptrace gets it from the
    /// kernel's own file, and the reader's own files read the reader's
    /// memory everywhere Detcore renders them. Another process's, here, are
    /// served from the strings its image's `execve` passed, recorded before
    /// the image's first instruction (`tool_global::ExecStrings`): the named
    /// interim fallback until the backend contract gives the tool a read of
    /// another guest process's memory
    /// (minibeads dh-ngy9z). KNOWN LIMIT:
    /// a process that rewrites its arguments or environment in memory after
    /// its exec shows the strings its `execve` passed, not the rewritten
    /// bytes, in another process's read.
    pub(super) fn serves_exec_strings(&self) -> bool {
        self.comm_source() == crate::procfs::task_stat::CommSource::Record
    }

    /// Whether `file` is a process's `environ` and Detcore serves none on
    /// this backend ([`Self::serves_exec_strings`]): then every spelling of
    /// every process's `environ`, its lookup, metadata and listing included,
    /// is the kernel's, unclassified, exactly as before Detcore rendered any
    /// (package K item 2b slice 2b-3), decided before any request to the
    /// global tool.
    pub(super) fn kernel_serves_every_environ(&self, file: &ProcfsFile) -> bool {
        file.is_process_environ() && !self.serves_exec_strings()
    }

    /// Whether `file` is a process's `smaps`, `smaps_rollup` or `numa_maps`
    /// and the guest's procfs is the kernel's
    /// ([`Detcore::renders_address_space_files`] is false): then every
    /// spelling of every process's, its lookup, metadata and listing
    /// included, is the kernel's, unclassified, exactly as before Detcore
    /// rendered any (package K slice N4), decided before any request to the
    /// global tool, so those backends pay nothing for the files. A path
    /// call's spelling of one (`faccessat`, `getxattr`, `chmod`) is left to
    /// the kernel earlier still, by the session's files
    /// (`could_name_rendered`), without resolving the spelling.
    pub(super) fn kernel_serves_every_address_space_file(&self, file: &ProcfsFile) -> bool {
        file.lists_rendered_address_space() && !self.renders_address_space_files()
    }

    /// Resolve `path`, an absolute spelling with a `..`, where Detcore renders
    /// `comm` (its record is the name's source, package K slice A+):
    /// [`walk_comm_alias`], then each task the walk passed must exist in
    /// Detcore's process table, in order, as Linux's lookup requires of the
    /// components a `..` later undoes (ENOENT otherwise); a walk that fails
    /// is Linux's error there (ENOENT, ENOTDIR). Only global-tool requests,
    /// no injected call. An undetermined walk is returned unchecked: only a
    /// write open refuses it ([`Self::refuse_undetermined_comm_write`]).
    pub(super) async fn resolve_comm_alias<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
    ) -> Result<CommAlias, Error> {
        if self.comm_source() != crate::procfs::task_stat::CommSource::Record {
            return Ok(CommAlias::NotApplicable);
        }
        let Some(own) = self.procfs_caller_ids(guest) else {
            return Ok(CommAlias::NotApplicable);
        };
        let (checks, outcome) = match walk_comm_alias(path, own) {
            CommAliasWalk::NotApplicable => return Ok(CommAlias::NotApplicable),
            CommAliasWalk::Undetermined(_) => return Ok(CommAlias::Undetermined),
            CommAliasWalk::Comm { checks, task } => (checks, Ok(task)),
            CommAliasWalk::Fails { checks, errno } => (checks, Err(errno)),
        };
        self.check_alias_tasks(guest, checks).await?;
        let task = outcome?;
        Ok(CommAlias::Resolved(std::path::PathBuf::from(
            match task.thread {
                Some(thread) => format!("/proc/{}/task/{thread}/comm", task.process),
                None => format!("/proc/{}/comm", task.process),
            },
        )))
    }

    /// Resolve `path`, an absolute spelling with a `..`, to `/proc/locks` or
    /// a descriptor's fdinfo where Detcore renders that file (package K
    /// slice K8-d; K8-h renders both where the guest's procfs is not the
    /// kernel's): [`walk_lock_or_fdinfo_alias`], then each task the walk
    /// passed must exist in Detcore's process table, in order, as Linux's
    /// lookup requires of the components a `..` later undoes (ENOENT
    /// otherwise). A walk that follows more links than Linux's lookup does
    /// fails with ELOOP, once the tasks named before the first link past the
    /// limit exist ([`LockOrFdinfoAlias::LinkLimit`]). The file's spelling
    /// without the `..`, and whether
    /// something follows it in `path` (which the caller refuses with ENOTDIR
    /// once the file is found, as Linux's walk does), for the caller to
    /// classify as it classifies a plain spelling. `None` for any other
    /// spelling, and where Detcore does not render the file, both of which
    /// stay the kernel's, as before. Only global-tool requests, no injected
    /// call.
    pub(super) async fn resolve_lock_or_fdinfo_alias<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
    ) -> Result<Option<(std::path::PathBuf, bool)>, Error> {
        let (file, tasks, past, loops) = match walk_lock_or_fdinfo_alias(path) {
            LockOrFdinfoAlias::NotApplicable => return Ok(None),
            LockOrFdinfoAlias::File { file, tasks } => (file, tasks, false, false),
            LockOrFdinfoAlias::PastFile { file, tasks } => (file, tasks, true, false),
            LockOrFdinfoAlias::LinkLimit { file, tasks } => (file, tasks, false, true),
        };
        let rendered = if file == Path::new("/proc/locks") {
            self.rendered_files(guest).locks
        } else {
            self.renders_fdinfo()
        };
        if !rendered {
            return Ok(None);
        }
        if !Self::named_tasks_exist(guest, &tasks).await {
            return Err(Errno::ENOENT.into());
        }
        if loops {
            return Err(Errno::ELOOP.into());
        }
        Ok(Some((file, past)))
    }

    /// ENOENT unless every task in `checks` exists in Detcore's process
    /// table (`tool_global::procfs_task`, the lookup Linux makes of
    /// `/proc/<n>` and `task/<tid>`), checked in order.
    async fn check_alias_tasks<G: Guest<Self>>(
        &self,
        guest: &mut G,
        checks: Vec<crate::procfs::BoundTask>,
    ) -> Result<(), Error> {
        for task in checks {
            if !crate::tool_global::procfs_task(guest, task).await.exists {
                return Err(Errno::ENOENT.into());
            }
        }
        Ok(())
    }

    /// Refuse an open with `flags` that asks to write of `path`, an absolute
    /// `/proc` spelling with a `..` and a last component `comm`, whose walk
    /// Detcore cannot finish ([`CommAlias::Undetermined`]): it may name a
    /// `comm` Detcore renders, whose write open is refused
    /// ([`asks_to_write_comm`]), and forwarding it would hand the kernel
    /// beneath a writable open of it (package K slice A+;
    /// <https://github.com/rrnewton/hermit/issues/4263>). The tasks the walk
    /// passed are checked first (ENOENT, as Linux's lookup); then the
    /// failure policy's named refusal. Nothing else is refused.
    pub(super) async fn refuse_undetermined_comm_write<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
        flags: OFlag,
    ) -> Result<(), Error> {
        if !open_asks_to_write(flags)
            || !self.renders_procfs(guest)
            || self.comm_source() != crate::procfs::task_stat::CommSource::Record
        {
            return Ok(());
        }
        let Some(own) = self.procfs_caller_ids(guest) else {
            return Ok(());
        };
        // The walk alone (no request): a spelling it resolves, or fails, is
        // checked once, by `Self::rendered_procfs_at`.
        if let CommAliasWalk::Undetermined(checks) = walk_comm_alias(path, own) {
            self.check_alias_tasks(guest, checks).await?;
            let reason = format!(
                "a writable open of {} passes a /proc component Detcore does not model before \
                 its `..` and may name a task's comm, which Detcore renders and does not write \
                 (https://github.com/rrnewton/hermit/issues/4263)",
                path.display()
            );
            return Err(
                Self::scratch_failure(guest, reverie::syscalls::Sysno::openat, reason).await,
            );
        }
        Ok(())
    }

    /// The parent Detcore's model records for another process's rendered
    /// `stat` (`parent`), numbered as that process's own `getppid` answers
    /// it, which Detcore passes to the kernel: where the kernel numbers
    /// processes otherwise than Detcore (DBT,
    /// `Self::translates_procfs_numbers`), the kernel's number for that
    /// guest process (`tool_global::procfs_ids`); a number of no guest
    /// process (the namespace's init, 1, whose number both numberings share,
    /// and 0 for a parent outside the PID namespace, the only outside
    /// parents a guest has) as it is. So a descriptor that reached another
    /// process shows the parent the described process's own `stat` shows,
    /// the reader's `getppid` answer, on every backend.
    async fn getppid_numbered_parent<G: Guest<Self>>(guest: &mut G, parent: i32) -> i32 {
        if parent <= 1 || !Self::registered_kernel_ids(guest) {
            return parent;
        }
        crate::tool_global::procfs_ids(guest, true, vec![parent])
            .await
            .into_iter()
            .next()
            .flatten()
            .unwrap_or(parent)
    }

    /// Whether this guest thread's backend registered its kernel ids (DBT,
    /// and an in-guest native exec, whose numberings agree), so that a
    /// number Detcore gives a task may not be the kernel's: whatever the
    /// guest's root or PID namespace, which `Self::translates_procfs_numbers`
    /// also weighs because it translates the guest's own paths. A rendered
    /// `stat` reaches the container's procfs by the tool's own spelling, so
    /// it translates by this alone. `tool_global::procfs_ids` returns each
    /// number unchanged where the numberings agree.
    fn registered_kernel_ids<G: Guest<Self>>(guest: &G) -> bool {
        guest.thread_state().physical_tid.is_some()
    }

    /// The kernel beneath's name (`comm`) of the task a rendered `stat`
    /// describes (the thread below `task/`, else the task the top-level
    /// number names), read by the tool itself from that task's own procfs
    /// directory, `/proc/<pid>/task/<tid>/comm` (`/proc/<pid>/comm` for a
    /// top-level number), by the kernel's numbers for them
    /// (`tool_global::procfs_ids` wherever the backend registered kernel ids,
    /// [`Self::registered_kernel_ids`], whatever the guest's root or PID
    /// namespace), with raw syscalls; never through the reader's own
    /// `/proc/self`, so a descriptor that reached another process (across
    /// fork or `SCM_RIGHTS`) reads the described task's name. Only where
    /// [`Self::comm_source`] is the kernel. It runs in the reader's turn, and
    /// the name is the guest's own history's: every way Linux renames a task
    /// (an exec, by Linux's own rule for the host's kernel version, a clone,
    /// `PR_SET_NAME`, a write to a `comm` file by any spelling) is a guest
    /// system call, sequentialized.
    ///
    /// Where the tool shares the guest's root (DBT, SaBRe, LiteInst) and the
    /// guest has changed it, nothing is read: the path would be looked up
    /// through the guest's own root, where the guest decides what
    /// /proc/<pid>/comm is (an ordinary file, a link into a bind-mounted
    /// procfs naming another task, a bind-mounted subdirectory), and no
    /// check on the opened file can tell that it is the described task's.
    /// That read is the failure policy's named refusal (declared). A tracer
    /// (ptrace) reads its own `/proc`, which no guest `chroot` changes.
    /// Otherwise the file must be on a procfs ([`comm_file_is_on_procfs`]),
    /// which refuses an ordinary file mounted over the path.
    /// `Err` names why there is no name.
    async fn kernel_task_comm<G: Guest<Self>>(
        &self,
        guest: &mut G,
        task: crate::procfs::BoundTask,
    ) -> Result<crate::procfs::task_stat::TaskComm, &'static str> {
        let changed_root = guest
            .thread_state()
            .changed_root
            .load(std::sync::atomic::Ordering::SeqCst);
        if !comm_read_is_trusted(
            guest.config().backend.tool_shares_guest_descriptor_table,
            changed_root,
        ) {
            return Err(
                "the guest changed its root, and the tool, which shares it, would look the \
                 task's comm up through it",
            );
        }
        let unreadable = "the tool could not read the kernel's comm of the task";
        let mut ids = vec![task.process];
        ids.extend(task.thread);
        let kernel = if Self::registered_kernel_ids(guest) {
            crate::tool_global::procfs_ids(guest, true, ids)
                .await
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or(unreadable)?
        } else {
            ids
        };
        let path = match kernel[..] {
            [process] => format!("/proc/{process}/comm"),
            [process, thread] => format!("/proc/{process}/task/{thread}/comm"),
            _ => return Err(unreadable),
        };
        let file =
            crate::util::RawFd::open(Path::new(&path), libc::O_RDONLY).map_err(|_| unreadable)?;
        if !comm_file_is_on_procfs(&file) {
            return Err("the task's comm path is not on a procfs");
        }
        let mut bytes = [0_u8; 2 * crate::procfs::task_stat::TASK_COMM_LEN];
        let read = file.read_at(&mut bytes, 0).map_err(|_| unreadable)?;
        Ok(crate::procfs::task_stat::TaskComm::from_comm_file(
            &bytes[..read],
        ))
    }

    /// The facts a snapshot of the rendered `stat` open as `fd` renders
    /// (`procfs::task_stat::TaskStat`), taken at its first read and again
    /// where Linux's `seq_file` would run `do_task_stat` again
    /// (`ProcfsFile::drop_stale_task_snapshot`). The same for a rendered
    /// `comm` (package K slice A+), which shows only the name, chosen by the
    /// same rule, where `comm_show` would run again. `None` for any other
    /// file.
    ///
    /// - The task must still be the one the file was bound to (ESRCH
    ///   otherwise), as `do_task_stat`'s `get_proc_task` requires.
    /// - The name, from [`Self::comm_source`]
    ///   (`procfs::task_stat::stat_comm`): the kernel's own
    ///   ([`Self::kernel_task_comm`]) where the backend's guest threads are
    ///   host tasks, and a failed read there is the failure policy's named
    ///   refusal, never Detcore's record, which a host resource would then
    ///   choose and which a write to a `comm` file leaves stale; Detcore's
    ///   record elsewhere (KVM), and with none, the same refusal.
    /// - The parent: the reader's own `getppid` answer (`ppid`) for its own
    ///   process; for another process's file (a descriptor that reached the
    ///   reader across `fork` or `SCM_RIGHTS`), the guest-visible parent
    ///   Detcore's model records (`procfs::task_stat::TaskStatFacts::parent`),
    ///   numbered as that process's `getppid` answers it
    ///   ([`Self::getppid_numbered_parent`]).
    /// - `rsslim`: the described process's virtual `RLIMIT_RSS`, whoever
    ///   reads it (`procfs::task_stat::TaskStatFacts::rsslim`).
    /// - The CPU times: for the reader's own process's file, its own
    ///   accounting ([`Self::stat_cpu_times`]); for another process's, what
    ///   that process last published (`procfs::task_stat::TaskStatFacts::cpu`,
    ///   package K item 2b slice 2b-1).
    /// - Whether the reader may trace the task, which `do_task_stat` asks
    ///   (`procfs::task_stat::TaskStat::permitted`): always within its own
    ///   thread group, and for another, Detcore's model of
    ///   `ptrace_may_access(PTRACE_MODE_READ_FSCREDS)`
    ///   ([`Self::io_read_allowed`], the same check a task's `io` makes),
    ///   asked at each rendering, as Linux asks it at each `show`. A model
    ///   that cannot decide is its named refusal (`Err`).
    pub(super) async fn rendered_task_stat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        ppid: i32,
    ) -> Result<Option<crate::procfs::task_stat::TaskStat>, Error> {
        let Some(path) = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_rendered_path())?
        else {
            return Ok(None);
        };
        let Some(file) = ProcfsFile::from_rendered_path(path)
            .filter(|file| file.is_task_stat() || file.is_task_comm() || file.is_task_status())
        else {
            return Ok(None);
        };
        let task = file.bound_task().ok_or(Errno::ESRCH)?;
        match self.rendered_task_state(guest, path).await {
            Some((_, true)) => {}
            _ => return Err(Errno::ESRCH.into()),
        }
        let facts = crate::tool_global::task_stat_facts(guest, task).await;
        let source = self.comm_source();
        let kernel = if source == crate::procfs::task_stat::CommSource::Kernel {
            self.kernel_task_comm(guest, task).await
        } else {
            Err("the backend's tasks have no kernel comm the tool can read")
        };
        let comm = match crate::procfs::task_stat::stat_comm(source, kernel, facts.comm) {
            Ok(comm) => comm,
            Err(why) => {
                let file_name = if file.is_task_comm() {
                    "comm"
                } else if file.is_task_status() {
                    "status"
                } else {
                    "stat"
                };
                let reason = format!(
                    "the /proc/<pid>/{file_name} of task {task:?} has no name to render: {why}"
                );
                return Err(Self::scratch_failure(guest, Sysno::read, reason).await);
            }
        };
        let described = task.thread.unwrap_or(task.process);
        let own = facts.process == Some(self.own_detpid(guest).as_raw());
        // A `comm` shows the name alone. A `status` shows the parent as a
        // `stat` does.
        let ppid = if own || file.is_task_comm() {
            ppid
        } else {
            Self::getppid_numbered_parent(guest, facts.parent.unwrap_or(0)).await
        };
        let cpu = if own && file.is_task_stat() {
            Self::stat_cpu_times(guest, task)
        } else {
            facts.cpu
        };
        // `proc_pid_status` asks no permission for any line (fs/proc/array.c
        // in 6.17), so a `status` never asks the model.
        let permitted = own
            || file.is_task_comm()
            || file.is_task_status()
            || Self::io_read_allowed(guest, task).await?.is_ok();
        Ok(Some(crate::procfs::task_stat::TaskStat {
            pid: described,
            comm,
            facts,
            reader_is_task: described == guest.thread_state().dettid.as_raw(),
            ppid,
            rsslim: facts.rsslim,
            processor: facts.cpus.current(guest.config().virtual_cpu_count()),
            cpu,
            permitted,
        }))
    }

    /// What a rendered `status` of the task `stat` describes shows beyond
    /// its `stat` (package K item 2b slice 2b-2, `procfs::task_status`), each
    /// line from the source that answers the guest's own matching query in
    /// this run, on this backend:
    ///
    /// - `Groups`: the reader's own `getgroups`, injected. Detcore makes
    ///   `setgroups` a no-op, so every guest task keeps the groups the run
    ///   started with, which every task's `getgroups` answers.
    /// - The signal sets: the backend's (`Guest::signal_sets`) of the
    ///   described thread, with the guest mask that thread published in
    ///   place of Detcore's private one (`syscalls::status_published_sigblk`,
    ///   as the kernel's `status` is corrected); none for a zombie, for which
    ///   the backends report none.
    /// - `CapInh`, `CapPrm`, `CapEff`: `capget` of the described thread,
    ///   injected (of the reader itself, `capget(0)`); none where it fails
    ///   (KVM's executor answers ESRCH for any task but the caller).
    /// - `CapBnd`, `CapAmb`: the reader's own `PR_CAPBSET_READ` and
    ///   `PR_CAP_AMBIENT_IS_SET`, injected, for the reader's own thread only,
    ///   since no call answers them for another.
    /// - The affinity: Detcore's, under `--virtual-cpus`, as
    ///   `sched_getaffinity` answers.
    ///
    /// `Err` where an injection could not be finished cleanly, after the
    /// failure policy, never an answer with a line made up.
    pub(super) async fn rendered_task_status<G: Guest<Self>>(
        &self,
        guest: &mut G,
        stat: crate::procfs::task_stat::TaskStat,
    ) -> Result<crate::procfs::task_status::TaskStatus, Error> {
        use crate::procfs::task_status::StatusCapabilities;
        use crate::procfs::task_status::StatusSignals;
        let described = stat.pid;
        let groups = Box::pin(Self::inject_reader_groups(guest)).await?;
        let signals = if stat.facts.exited {
            None
        } else {
            match guest.signal_sets(reverie::Pid::from_raw(described)) {
                Ok(sets) => {
                    let published = super::status_published_sigblk(guest, Some(described))
                        .await?
                        .sigblk;
                    Some(StatusSignals {
                        pending: sets.pending,
                        shared_pending: sets.shared_pending,
                        blocked: published.unwrap_or(sets.blocked),
                        ignored: sets.ignored,
                        caught: sets.caught,
                    })
                }
                Err(reverie::FactError::NotProvided | reverie::FactError::Errno(Errno::ESRCH)) => {
                    None
                }
                Err(error) => {
                    let reason = format!(
                        "the signal sets of task {described}, which its /proc status shows, \
                         could not be read: {error:?}"
                    );
                    return Err(Self::scratch_failure(guest, Sysno::read, reason).await);
                }
            }
        };
        let kernel = if stat.reader_is_task {
            0
        } else if Self::registered_kernel_ids(guest) {
            match crate::tool_global::procfs_ids(guest, true, vec![described])
                .await
                .into_iter()
                .next()
                .flatten()
            {
                Some(kernel) => kernel,
                None => {
                    let reason = format!(
                        "task {described}, whose /proc status shows its capability sets, has \
                         no kernel number to ask capget by"
                    );
                    return Err(Self::scratch_failure(guest, Sysno::read, reason).await);
                }
            }
        } else {
            described
        };
        let inheritable_permitted_effective =
            match Box::pin(Self::inject_capget_of(guest, kernel)).await? {
                Some(Ok(sets)) => Some((sets.inheritable, sets.permitted, sets.effective)),
                Some(Err(_)) => None,
                None => {
                    let reason = "no guest stack scratch can hold capget's data for a /proc \
                                  status"
                        .to_owned();
                    return Err(Self::scratch_failure(guest, Sysno::capget, reason).await);
                }
            };
        let (bounding, ambient) = if stat.reader_is_task {
            Self::inject_reader_bounding_and_ambient(guest).await
        } else {
            (None, None)
        };
        let cpus_allowed = self
            .cfg
            .virtual_cpus
            .map(|cpus| (stat.facts.cpus.allowed(cpus.get()), cpus.get()));
        Ok(crate::procfs::task_status::TaskStatus {
            stat,
            groups,
            signals,
            capabilities: StatusCapabilities {
                inheritable_permitted_effective,
                bounding,
                ambient,
            },
            cpus_allowed,
        })
    }

    /// The reader's supplementary groups, as an injected `getgroups`
    /// answers: its count first (`getgroups(0, NULL)`, no memory), then the
    /// groups into borrowed stack scratch, put back on every path. `Err`
    /// where they cannot be read, after the failure policy.
    async fn inject_reader_groups<G: Guest<Self>>(guest: &mut G) -> Result<Vec<u32>, Error> {
        /// The most groups the scratch holds.
        const MOST: usize = 64;
        let count = match guest
            .inject(Syscall::from_raw(
                Sysno::getgroups,
                super::procfs_levels::raw_args([0; 6]),
            ))
            .await
        {
            Ok(count) => count as usize,
            Err(errno) => {
                let reason = format!("getgroups for a /proc status failed with {errno}");
                return Err(Self::scratch_failure(guest, Sysno::getgroups, reason).await);
            }
        };
        if count == 0 {
            return Ok(Vec::new());
        }
        if count > MOST {
            let reason = format!(
                "the reader has {count} supplementary groups, more than the {MOST} a /proc \
                 status rendering reads"
            );
            return Err(Self::scratch_failure(guest, Sysno::getgroups, reason).await);
        }
        let (scratch, original, stack_guard) = match Self::borrow_scratch::<G, { 4 * MOST }>(guest)
            .await
        {
            Ok(Some(borrowed)) => borrowed,
            Ok(None) => {
                let reason = "no guest stack scratch can hold getgroups's list for a /proc status"
                    .to_owned();
                return Err(Self::scratch_failure(guest, Sysno::getgroups, reason).await);
            }
            Err(reason) => {
                return Err(Self::scratch_failure(guest, Sysno::getgroups, reason).await);
            }
        };
        let answer = guest
            .inject(Syscall::from_raw(
                Sysno::getgroups,
                super::procfs_levels::raw_args([count, scratch.as_raw(), 0, 0, 0, 0]),
            ))
            .await;
        let mut bytes = [0_u8; 4 * MOST];
        let read = match answer {
            Ok(written) if written as usize == count => guest
                .memory()
                .read_exact(scratch, &mut bytes[..4 * count])
                .map_err(|errno| format!("its list could not be read back: {errno}")),
            Ok(written) => Err(format!(
                "it wrote {written} groups where it counted {count}"
            )),
            Err(errno) => Err(format!("it failed with {errno}")),
        };
        let restored = guest.memory().write_exact(scratch, &original);
        drop(stack_guard);
        if let Err(errno) = restored {
            let reason = format!(
                "the guest stack scratch under an injected getgroups could not be restored: \
                 {errno}"
            );
            return Err(Self::scratch_failure(guest, Sysno::getgroups, reason).await);
        }
        if let Err(detail) = read {
            let reason = format!("getgroups for a /proc status: {detail}");
            return Err(Self::scratch_failure(guest, Sysno::getgroups, reason).await);
        }
        Ok(bytes[..4 * count]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|group| u32::from_ne_bytes(*group))
            .collect())
    }

    /// The reader's own capability bounding and ambient sets, as injected
    /// `prctl(PR_CAPBSET_READ, n)` and `prctl(PR_CAP_AMBIENT,
    /// PR_CAP_AMBIENT_IS_SET, n)` answer for each capability `n` from 0 up to
    /// the first the kernel beneath does not know (EINVAL), its
    /// `cap_last_cap`. A set is `None` where a query fails otherwise.
    async fn inject_reader_bounding_and_ambient<G: Guest<Self>>(
        guest: &mut G,
    ) -> (Option<u64>, Option<u64>) {
        async fn set<G: Guest<Detcore<T>>, T: RecordOrReplay>(
            guest: &mut G,
            args: impl Fn(usize) -> [usize; 6],
        ) -> Option<u64> {
            let mut bits = 0_u64;
            for capability in 0..64 {
                match guest
                    .inject(Syscall::from_raw(
                        Sysno::prctl,
                        super::procfs_levels::raw_args(args(capability)),
                    ))
                    .await
                {
                    Ok(0) => {}
                    Ok(_) => bits |= 1 << capability,
                    Err(Errno::EINVAL) => return Some(bits),
                    Err(_) => return None,
                }
            }
            Some(bits)
        }
        let bounding = set(guest, |capability| {
            [libc::PR_CAPBSET_READ as usize, capability, 0, 0, 0, 0]
        })
        .await;
        let ambient = set(guest, |capability| {
            [
                libc::PR_CAP_AMBIENT as usize,
                libc::PR_CAP_AMBIENT_IS_SET as usize,
                capability,
                0,
                0,
                0,
            ]
        })
        .await;
        (bounding, ambient)
    }

    /// The CPU times the `stat` of `task`, a task of the reader's own
    /// process, shows now (`procfs::task_stat::TaskCpuTimes`): the process's
    /// for its top-level file, which counts every thread, exited ones
    /// included (`thread_group_cputime_adjusted`), and one thread's for a
    /// file below `task/` (`task_cputime_adjusted`): the reader's own, or a
    /// sibling's as of its last accounting, where it last stopped running
    /// (`ThreadState::task_cpu_time`). The children's are the process's in
    /// both. The same accounting `times(2)`, `getrusage(2)` and the CPU-time
    /// clocks read, so every view agrees.
    fn stat_cpu_times<G: Guest<Self>>(
        guest: &mut G,
        task: crate::procfs::BoundTask,
    ) -> crate::procfs::task_stat::TaskCpuTimes {
        let process = guest.thread_state_mut().process_cpu_time();
        let (user, system) = match task.thread {
            None => (process.user, process.system),
            Some(thread) => guest
                .thread_state()
                .task_cpu_time(crate::types::DetTid::from_raw(thread)),
        };
        crate::procfs::task_stat::TaskCpuTimes {
            user,
            system,
            children_user: process.children_user,
            children_system: process.children_system,
        }
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
    /// Detcore rendered it, and never to a copy; except where Detcore serves
    /// another process's file ([`Self::serves_exec_strings`], package K item
    /// 2b slice 2b-3), where the same rule reads the strings that process's
    /// image's `execve` passed, recorded before its first instruction
    /// (`procfs::RecordedStrings`): the named interim fallback for a read of
    /// its memory, with the known limit that an in-place rewrite after the
    /// exec is not shown.
    ///
    /// A rendered `environ` is read the same way, by Linux's `environ_read`
    /// rule (`procfs::read_environ`, fs/proc/base.c:1006-1067): the reader's
    /// own from its memory, another process's from the record. Its open
    /// checked access (`Self::ptrace_read_allowed_for`) and pinned the
    /// memory map in Linux, which Detcore models by the number of the image
    /// whose strings it recorded at the open (`ProcfsFile::pinned_exec_image`):
    /// a process that has exited, or has exec'd another image since, reads
    /// nothing, as a map that is gone does (`mmget_not_zero` fails). A
    /// `cmdline` pins nothing (`get_task_mm` at each read), so it reads the
    /// process's current image.
    ///
    /// The first read of the run that another process's strings answer from
    /// the record is named at Hermit's default level (WARN), so that the
    /// interim's known limit shows in the run's log.
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
        let file = ProcfsFile::from_rendered_path(path);
        let environ = file.as_ref().is_some_and(ProcfsFile::is_process_environ);
        let task = file
            .and_then(|file| file.bound_task())
            .ok_or(Errno::ESRCH)?;
        match self.rendered_task_state(guest, path).await {
            // `environ_read` reads its pinned map, which outlives the task's
            // lookup; one that is gone reads nothing.
            Some((_, false)) | None if environ => return Ok(0),
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
        // The image an `environ`'s open pinned, which an exec since has
        // replaced where the process's current record names another
        // (`environ_read`'s `mmget_not_zero` of a map nothing uses: 0).
        let pinned = if environ {
            guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.procfs_pinned_exec_image())?
        } else {
            None
        };
        // Another process's file, where Detcore serves it, reads the strings
        // its image's `execve` passed.
        if process != self.own_detpid(guest) && self.serves_exec_strings() {
            let current = crate::tool_global::process_exec_strings(guest, process)
                .await
                .1;
            if let Some(pinned) = pinned
                && current.as_ref().is_none_or(|(image, _)| *image != pinned)
            {
                return Ok(0);
            }
            let Some((_, strings)) = current else {
                // A descriptor kept across the process's exec into an image
                // whose strings Detcore could not record (every image records
                // them where the backend reports its initial stack pointer,
                // as KVM does): refused by name, as an open of the file is
                // (`other_process_procfs`).
                return Err(Self::scratch_failure(
                    guest,
                    Sysno::read,
                    format!(
                        "another guest process's {} cannot be read: Detcore holds no record of \
                         the strings its current image's execve passed",
                        path.display()
                    ),
                )
                .await);
            };
            let mut memory = guest.memory();
            let mut adapter = GuestCmdlineMemory {
                memory: &mut memory,
                destination,
                failure: None,
            };
            let mut recorded = crate::procfs::RecordedStrings {
                argv: &strings.argv,
                envp: &strings.envp,
                copy_out: |offset: usize, bytes: &[u8]| {
                    crate::procfs::CmdlineMemory::copy_out(&mut adapter, offset, bytes)
                },
            };
            let area = recorded.area();
            let read = crate::procfs::read_exec_strings(environ, &area, pos, count, &mut recorded);
            if let Some(failure) = adapter.failure {
                return Err(failure.into());
            }
            if !EXEC_STRINGS_FALLBACK_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                warn!(
                    "[detcore] another process's /proc/<pid>/{} is served from the strings its \
                     image's execve passed, recorded before its first instruction: the tool \
                     cannot read another guest process's memory on this backend, so an \
                     in-place rewrite after the exec (setproctitle) is not shown, where Linux \
                     reads the process's memory (minibeads dh-ngy9z; named once per run)",
                    if environ { "environ" } else { "cmdline" }
                );
            }
            return Ok(read?);
        }
        // The caller's own `environ`, kept across its own exec into another
        // image, reads nothing, as another process's does.
        if let Some(pinned) = pinned
            && crate::tool_global::process_exec_strings(guest, process)
                .await
                .1
                .is_none_or(|(image, _)| image != pinned)
        {
            return Ok(0);
        }
        // Another process's file, or the caller's own where Detcore recorded
        // no argument area for its current image (a stack layout
        // initial_stack_cmdline_area refused, or one it could not read, on a
        // backend that reports the initial stack pointer; a descriptor kept
        // across an exec into such an image), is the kernel beneath's.
        let Some(area) = area.filter(|_| process == self.own_detpid(guest)) else {
            if environ {
                // Never on a backend whose kernel serves every `environ`; an
                // own image without a recorded area keeps the kernel's file
                // from its open (`kept_by_kernel`), so only a descriptor kept
                // across an exec into such an image reaches here.
                return Err(Self::scratch_failure(
                    guest,
                    Sysno::read,
                    format!(
                        "{} cannot be read: Detcore recorded no environment area for the \
                         process's current image",
                        path.display()
                    ),
                )
                .await);
            }
            return Box::pin(self.kernel_cmdline_read(guest, task, area, destination, count, pos))
                .await;
        };
        let mut memory = guest.memory();
        let mut adapter = GuestCmdlineMemory {
            memory: &mut memory,
            destination,
            failure: None,
        };
        let read = crate::procfs::read_exec_strings(environ, &area, pos, count, &mut adapter);
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
        let sysno = reverie::syscalls::Sysno::new(
            crate::arch::syscall_number(&guest.regs().await) as usize
        )
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
    ///
    /// A per-process file that follows its record's sequence keeps that
    /// sequence, the record it buffered and the cursor in Detcore's global
    /// state, where the tool runs in each guest process, from the first fork
    /// that copies its description or send that passes it over `SCM_RIGHTS`
    /// (`ProcfsFile::sequence_is_shared`, `Detcore::take_procfs_sequence`,
    /// `Detcore::share_sent_record_sequences`), so the processes holding it
    /// share them as Linux's share the one `struct file` and `seq_file`; this
    /// kernel cursor then agrees with the stored one.
    ///
    /// Which holders share it, by route. A `dup` and a fork's copy: on every
    /// backend. A descriptor received over `SCM_RIGHTS`: where one tool
    /// process serves every guest (ptrace, KVM), whenever the receipt joins
    /// the open file its sender's table modeled (`crate::syscalls::scm_rights`);
    /// where the tool runs in each guest process (DBT, SaBRe, LiteInst,
    /// in-guest-trap), when the receiver's table holds the open file
    /// (`Detcore::placeholder_alias_in_table`, `FileMetadata::adopt_placeholder`)
    /// or its registry names it, which holds where the receiver or an
    /// ancestor before a fork opened it (`Detcore::rebuild_received_placeholder`).
    /// A descriptor `exec` keeps: on ptrace and KVM, whose model survives exec,
    /// and on DBT and SaBRe, which carry it with its sequence
    /// (`ThreadState::procfs_records_for_exec`); LiteInst and in-guest-trap
    /// refuse `execve` by name. Every other receipt cannot know where the
    /// sequence is (on DBT, SaBRe, LiteInst and in-guest-trap, a rendered
    /// file's by a process that never held the open file, and every receipt
    /// of a file the kernel opened; on ptrace and KVM, a receipt Detcore
    /// could not match, of a file the kernel opened, or of a rendered one
    /// whose every modeled alias closed first): every read and seek through
    /// it is refused by name (`Detcore::refuse_unknown_record_sequence`: exit
    /// 122 and the reason, or on DBT, whose runtime ends a run at any Detcore
    /// refusal through its own exit path, 101 or 137 and the reason; on SaBRe,
    /// where a refusal that ended the run would leave it waiting on the
    /// refusing process, `refusal_would_strand_the_run`, EOPNOTSUPP and the
    /// reason on the guest's standard error, no process ended), so no holder
    /// answers otherwise than Linux without saying so. Every outcome follows
    /// from the guest's own sequentialized history.
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

    /// Keep the kernel position of a procfs file whose position Detcore keeps
    /// (`DetFd::procfs_keeps_kernel_position`: a placeholder, a fixed file,
    /// or a `maps` rendered from the memory map) at Detcore's cursor, which a
    /// backend that rediscovers descriptors after `exec`, or another process
    /// sharing the open file, reads back (`adopt_rendered_procfs_position`),
    /// and which Linux's `fdinfo` shows.
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
            // refuse one past its contents, which differ from Detcore's, and
            // a `maps` refuses every one once its task is reaped (ESRCH from
            // `seq_lseek`, which leaves the kernel's position at 0); Detcore's
            // cursor still answers the guest.
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
        // A placeholder directory Detcore serves (`descriptor_directory`).
        if let Some(stat) = self.placeholder_stat(guest, target).await? {
            return Ok(Some(stat));
        }
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
        match self.placeholder_link_within_level(guest, &bytes) {
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
        // A rendered file, or a placeholder directory Detcore serves
        // (`descriptor_directory`), which has no `mmap` method either.
        let (path, status_flags) = guest
            .thread_state()
            .with_detfd(c.fd(), |detfd| {
                (detfd.status_flags() & libc::O_PATH == 0)
                    .then(|| {
                        detfd
                            .procfs_rendered_path()
                            .map(Some)
                            .or_else(|| detfd.placeholder().map(|_| None))
                    })
                    .flatten()
                    .map(|path| (path, detfd.status_flags()))
            })
            .ok()
            .flatten()?;
        // A per-process file has no `mmap` method, as a sysctl file has none.
        let seq_file = path.is_some_and(|path| {
            crate::procfs::rendered_path_is_seq_file(path)
                && !crate::procfs::rendered_path_is_per_process(path)
        });
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
        let fd = match written_descriptor_position(sysno)? {
            // Only `pwritev2` at the file position writes as `writev` does.
            _ if sysno == Sysno::pwritev2 && args.arg3 as i64 != -1 => return None,
            0 => args.arg0,
            _ => args.arg2,
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
    ///
    /// Where Detcore serves another process's `cmdline` and `environ`
    /// ([`Self::serves_exec_strings`]: KVM), the bytes of both areas go to
    /// the global tool too, in a second request, before the image's first
    /// instruction, so they are exactly what its `execve` passed
    /// (`tool_global::ExecStrings`, package K item 2b slice 2b-3). The record
    /// is the named interim fallback for reading the process's memory, which
    /// Linux does at every read: a rewrite of either area after this point is
    /// not reflected in another process's read (the known limit).
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
        let (found, cmdline, strings) = match guest.initial_stack_pointer() {
            Some(stack) => {
                let memory = guest.memory();
                let read_word = |at: u64| {
                    let address =
                        reverie::syscalls::Addr::<u64>::from_raw(usize::try_from(at).ok()?)?;
                    memory.read_value(address).ok()
                };
                let cmdline =
                    crate::procfs::initial_stack_cmdline_area(stack, read_word, |at, buffer| {
                        read_prefix(&memory, at, buffer)
                    });
                let strings = cmdline
                    .filter(|_| self.serves_exec_strings())
                    .and_then(|area| recorded_exec_strings(&memory, &area));
                (
                    crate::procfs::initial_stack_auxv(stack, read_word)
                        .ok_or(AuxvFromKernel::Implausible),
                    cmdline,
                    strings,
                )
            }
            None => (Err(AuxvFromKernel::NoInitialStackPointer), None, None),
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
                if let Some(strings) = strings {
                    crate::tool_global::record_exec_strings(guest, strings).await;
                }
                return;
            }
        };
        crate::tool_global::record_process_auxv(guest, bytes, cmdline).await;
        // After the request above, which drops the strings of the process's
        // earlier image or its creator's (`ProcessAuxvTable::record_cmdline`).
        if let Some(strings) = strings {
            crate::tool_global::record_exec_strings(guest, strings).await;
        }
    }

    /// Adopt, before `call` runs, each descriptor it names that Detcore has
    /// no record of and that is a placeholder ([`Self::received_placeholder`]):
    /// a rendered procfs descriptor that reached this table over `SCM_RIGHTS`
    /// and was not recorded as it arrived. Where the tool does not discover
    /// descriptors as they are used (ptrace and KVM), Detcore records each
    /// received descriptor as the receive returns, adopting a placeholder
    /// there (`Detcore::record_received_descriptors`), except without
    /// sequentialized threads, which leaves it to this adoption; an in-guest
    /// backend discovers it first (`discover_fd_from_current_process`). Then every
    /// call through it (reads, the cursor, a mapping, the write family, the
    /// task's reap) is answered as for a descriptor Detcore opened, and none
    /// reaches the memfd.
    ///
    /// The descriptor shares the sender's `struct file`, so it is an alias of
    /// the sender's open file description while Detcore still models that
    /// description (`PLACEHOLDER_OPENS`, or where the tool runs in each guest
    /// process this table: [`Self::adopt_received_placeholder`]): one cursor,
    /// one snapshot, one set of status flags, exactly as Linux shares them.
    /// Once no modeled descriptor refers to it (the sender closed its own),
    /// the description is rebuilt from what the kernel keeps of it, as an
    /// in-guest backend rebuilds a placeholder it rediscovers
    /// (`adopt_placeholder`): the file and task its identity names, the status
    /// flags (`F_GETFL`), the cursor Detcore keeps at the kernel position
    /// (`sync_rendered_procfs_position`), and the snapshot its name carries; a
    /// file whose snapshot is taken at its first read takes it then
    /// (`initialize_procfs_snapshot`). A per-process file that follows its
    /// record's sequence takes that sequence from Detcore's global state where
    /// it can, and is otherwise refused at every read and seek
    /// ([`Self::rebuild_received_placeholder`]).
    ///
    /// Only once this tool process has made a placeholder, or where Detcore
    /// runs in each guest process (where another process may have made one),
    /// and only for a descriptor Detcore has no record of: one injected
    /// `fstat`, for each such descriptor a listed call names, every time (no
    /// cache: a descriptor number's identity can only be learned by asking).
    // TODO-HUMAN-REVIEW(#4213): Review the adoption of rendered descriptors
    // received over SCM_RIGHTS: the alias of a modeled description, the
    // rebuild of one no longer modeled, and the refusal without
    // sequentialized threads.
    pub(crate) async fn adopt_received_placeholders<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Result<(), Error> {
        let (sysno, args) = call.into_parts();
        // The number first: it takes no lock, and most calls name no
        // descriptor (`descriptor_arguments` would name none).
        let per_process =
            !crate::syscalls::timerfd::detcore_runs_outside_the_guest(&self.cfg.backend);
        if !names_a_descriptor(sysno)
            || (!placeholders_exist() && !per_process)
            || !self.renders_procfs(guest)
        {
            return Ok(());
        }
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
            let Some((path, named)) = self.received_placeholder(guest, fd, &stat).await? else {
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
            self.adopt_received_placeholder(guest, fd, path, identity, named)
                .await?;
        }
        Ok(())
    }

    /// The rendered file that descriptor `fd`, whose `fstat` is `stat` (an
    /// empty regular file with no link), is Detcore's placeholder for, as it
    /// is received over `SCM_RIGHTS`, and the snapshot its memfd's name
    /// carries where the placeholder is known by that name (`None` where this
    /// tool process registered it: its open is in `PLACEHOLDER_OPENS`).
    ///
    /// Where one tool process serves every guest process (ptrace, KVM), it
    /// made every placeholder, so the kernel identity it registered is the
    /// whole answer. Where Detcore runs in each guest process
    /// (`detcore_runs_outside_the_guest` is false: LiteInst and
    /// in-guest-trap, whose received descriptors are recorded as they
    /// arrive), a process knows only the placeholders it made, inherited at
    /// `fork` or rediscovered after `exec`. One that another process made
    /// after this one was forked, and sent it, is known by its name and
    /// seals instead, as the in-guest discovery of DBT and SaBRe knows one
    /// (`discover_fd_from_current_process`), and then registered here. Before
    /// this, such a descriptor was recorded as the empty memfd it is, so its
    /// reads returned end of file where Linux reads the file.
    ///
    /// A name and seals are also what a guest can give a memfd of its own (a
    /// known limitation, see <https://github.com/rrnewton/hermit/issues/4263>):
    /// a guest that makes a memfd with a placeholder's name and the four
    /// seals and sends it to a process forked before reads, there, the
    /// rendered file the name names, deterministically, and no write through
    /// it changes anything (Detcore answers the write family of a rendered
    /// file itself), though its descriptor keeps the memfd's access mode,
    /// O_RDWR. A name naming a file outside the run's `--proc` level is not
    /// one: Detcore never makes such a placeholder, so the memfd stays the
    /// memfd it is and no file the level omits is read through it.
    /// Registering each placeholder's kernel identity with the run's
    /// coordinator as it is made would make the name unnecessary (issue
    /// 4263).
    pub(crate) async fn received_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        stat: &libc::stat,
    ) -> Result<Option<(&'static Path, Option<Vec<u8>>)>, Error> {
        if let Some(path) = placeholder_identity(stat.st_dev, stat.st_ino) {
            return Ok(Some((path, None)));
        }
        if crate::syscalls::timerfd::detcore_runs_outside_the_guest(&self.cfg.backend) {
            return Ok(None);
        }
        let Some(link) = self.received_placeholder_link(guest, fd).await else {
            return Ok(None);
        };
        let Some((path, snapshot)) =
            crate::procfs::placeholder_link(&link, self.rendered_files(guest))
        else {
            return Ok(None);
        };
        // Detcore makes no placeholder for a file outside the run's `--proc`
        // level: its open fails with ENOENT. So a name naming one is not a
        // placeholder Detcore made, whatever its seals, and stays the memfd it
        // is: no file its level omits is read through a memfd so named.
        if named_file_outside_level(path, self.cfg.proc_level) {
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
        let sealed = match seals {
            Ok(seals) => seals & wanted == wanted,
            Err(errno) => errno == Errno::EBADF,
        };
        if !sealed {
            return Ok(None);
        }
        register_placeholder(stat.st_dev, stat.st_ino, path);
        Ok(Some((path, Some(snapshot))))
    }

    /// The link of descriptor `fd` (`readlinkat` of
    /// `/proc/thread-self/fd/<fd>`, injected; the backend's `link_path` fact
    /// where the backend's own `/proc` is not the guest's,
    /// `Detcore::own_descriptor_link_from_fact`, KVM), at most a
    /// placeholder's (`PLACEHOLDER_LINK_CAPACITY`): `None` where it cannot be
    /// read or is longer. It takes no descriptor, so it reads a descriptor
    /// received into the last free slot of a full table. The path and the
    /// result share one buffer, staged as `inject_fstat` stages its own
    /// ([`crate::syscalls::scratch::inject_with_scratch`]).
    async fn received_placeholder_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Option<Vec<u8>> {
        const LINK_PATH: usize = 32;
        const LINK_TARGET: usize = crate::procfs::PLACEHOLDER_LINK_CAPACITY;
        if let Some(fact) = Self::own_descriptor_link_from_fact(guest, fd) {
            return fact.filter(|text| text.len() < LINK_TARGET);
        }
        let mut buffer = [0_u8; LINK_PATH + LINK_TARGET];
        let spelled = format!("/proc/thread-self/fd/{fd}");
        buffer
            .get_mut(..spelled.len())?
            .copy_from_slice(spelled.as_bytes());
        let (length, bytes) =
            crate::syscalls::scratch::inject_with_scratch(guest, buffer, |base| {
                // SAFETY: the buffer holds LINK_PATH + LINK_TARGET bytes.
                let target = unsafe { base.add(LINK_PATH) };
                syscalls::Readlinkat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(base.as_raw() as *const libc::c_char))
                    .with_buf(Some(target.cast::<libc::c_char>()))
                    .with_buf_len(LINK_TARGET)
                    .into()
            })
            .await
            .ok()?;
        usize::try_from(length)
            .ok()
            .filter(|length| *length < LINK_TARGET)
            .map(|length| bytes[LINK_PATH..LINK_PATH + length].to_vec())
    }

    /// Make descriptor `fd`, which Detcore has no record of, the placeholder
    /// `identity` for the rendered file `path` it is, as it is received
    /// (`Detcore::record_received_descriptors`) or first named
    /// ([`Self::adopt_received_placeholders`]). `named` is the snapshot its
    /// memfd's name carries where it is known by that name
    /// ([`Self::received_placeholder`]).
    ///
    /// Linux installs the sender's `struct file`, so the descriptor joins the
    /// open file Detcore models for the placeholder wherever it can: where one
    /// tool process serves every guest (ptrace, KVM), the one model the
    /// registry reaches, whichever process's table holds it; where the tool
    /// runs in each guest process (DBT, SaBRe, LiteInst, in-guest-trap), a
    /// descriptor of this process's table on the same placeholder
    /// ([`Self::placeholder_alias_in_table`]), not the registry's handle,
    /// which in a fork's child reaches the parent's model of the open file
    /// rather than the copy the child's table holds (on LiteInst and
    /// in-guest-trap the copy then read as a separate open file, Claude's
    /// narrow check of the kvm lane's slice N3, round 5). Where none is
    /// joined, the description is rebuilt ([`Self::rebuild_received_placeholder`]).
    pub(crate) async fn adopt_received_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        path: &'static Path,
        identity: (u64, u64),
        named: Option<Vec<u8>>,
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
        let per_process =
            !crate::syscalls::timerfd::detcore_runs_outside_the_guest(&self.cfg.backend);
        let alias = if per_process {
            self.placeholder_alias_in_table(guest, fd, identity).await
        } else {
            registered
                .as_ref()
                .and_then(|(description, _, _)| description.descriptor(fd, cloexec))
        };
        if let Some(mut alias) = alias {
            alias.set_cloexec(cloexec);
            guest.thread_state().install_alias(alias);
            return Ok(());
        }
        self.rebuild_received_placeholder(guest, fd, path, identity, named, cloexec, registered)
            .await
    }

    /// A copy, as descriptor `fd`, of a descriptor of this process's table
    /// other than `fd` that is open on the placeholder `identity`: an alias
    /// of the open file the received `fd` is, as the in-guest discovery of
    /// DBT and SaBRe finds one (`FileMetadata::adopt_placeholder`). Every
    /// descriptor of a rendered file is a candidate, asked by an injected
    /// `fstat`, lowest number first. Only where the tool runs in each guest
    /// process ([`Self::adopt_received_placeholder`]).
    async fn placeholder_alias_in_table<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        identity: (u64, u64),
    ) -> Option<crate::fd::DetFd> {
        for other in guest.thread_state().rendered_descriptors() {
            if other == fd {
                continue;
            }
            let Ok(stat) = self.inject_fstat(guest, other).await else {
                continue;
            };
            if (stat.st_dev, stat.st_ino) == identity {
                return guest
                    .thread_state()
                    .with_detfd(other, |detfd| detfd.clone().with_fd(fd))
                    .ok();
            }
        }
        None
    }

    /// Rebuild, as descriptor `fd` (`cloexec`), the open file of the
    /// placeholder `identity` for the rendered file `path`, which no
    /// description this process models holds
    /// ([`Self::adopt_received_placeholder`]), from what the kernel keeps of
    /// it: the file and task its identity names, the status flags
    /// (`F_GETFL`), the cursor Detcore keeps at the kernel position
    /// (`sync_rendered_procfs_position`), and the snapshot `registered` (this
    /// process's registry's entry for it) or its name (`named`) carries; a
    /// file whose snapshot is taken at its first read takes it then
    /// (`initialize_procfs_snapshot`).
    ///
    /// A per-process file that follows its record's sequence
    /// (`ProcfsFile::follows_record_sequence`: a task's statm, io,
    /// arch_status, cgroup, stat and comm) has more: Linux keeps where its
    /// sequence is and the record it buffered in the one open file every
    /// holder shares, and a rebuilt description has neither. Where the tool
    /// runs in each guest process and this process's registry names the open
    /// file (this process, or an ancestor before a fork, opened it), the
    /// descriptor takes that open file's identity and its sequence from
    /// Detcore's global state, where every holder keeps it once the open file
    /// has left a process (`ThreadState::share_procfs_sequences` at a fork,
    /// `Detcore::share_sent_record_sequences` at a send), so it continues
    /// the sender's record as Linux's descriptor does. Anywhere else (a
    /// process that never held the open file, or, where one tool process
    /// serves every guest, a receipt Detcore could not match whose every
    /// modeled alias closed first) the sequence cannot be known: every read
    /// and seek through the descriptor is refused by name
    /// (`Detcore::refuse_unknown_record_sequence`), so it never moves the
    /// sequence another holder's Detcore keeps, nor shows a record Linux
    /// would not (on SaBRe, where a refusal that ended the run would strand
    /// it, `refusal_would_strand_the_run`, the call gets EOPNOTSUPP).
    #[allow(clippy::too_many_arguments)]
    async fn rebuild_received_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        path: &'static Path,
        identity: (u64, u64),
        named: Option<Vec<u8>>,
        cloexec: bool,
        registered: Option<PlaceholderOpen>,
    ) -> Result<(), Error> {
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
        // the same, as `cgroup`'s template is, or where the description
        // takes the sequence its open file keeps in Detcore's global state
        // (below). A per-process file whose bytes change (`status`) must
        // carry its first read's snapshot on its placeholder (as
        // `/proc/uptime` carries its first read's uptime,
        // `first_read_uptime`) before it is rendered, or follow its record's
        // sequence as `stat` does, or its rebuilt descriptor reads other
        // bytes than the sender's buffer held.
        let per_process =
            !crate::syscalls::timerfd::detcore_runs_outside_the_guest(&self.cfg.backend);
        let (registered_id, snapshot) = match registered {
            Some((_, id, snapshot)) => (Some(id), snapshot),
            None => (None, named.unwrap_or_default()),
        };
        let metadata = self.rendered_metadata(guest, path).await?;
        let mut flags = OFlag::from_bits_truncate(status);
        flags.set(OFlag::O_CLOEXEC, cloexec);
        guest
            .thread_state()
            .add_fd(fd, flags, FdType::Regular, Some(metadata))?;
        let mut procfs = ProcfsFile::from_rendered_path(path)
            .expect("a placeholder's path is a rendered procfs file")
            .into_rendered();
        if !snapshot.is_empty() {
            procfs = procfs.with_rendered_snapshot(snapshot.clone());
        }
        let follows = procfs.follows_record_sequence();
        let joined = if follows && per_process {
            registered_id
        } else {
            None
        };
        if joined.is_some() {
            procfs.share_sequence();
        }
        let (handle, id) = guest.thread_state().with_detfd(fd, |detfd| {
            // Received unjoined, it is a description of its own whose lock
            // history and references Detcore did not see (the sender's table
            // may hold it too), as is every other such receipt
            // (`FileMetadata::add_discovered_fd`): escaped, so Detcore's flock
            // model never decides a flock through it, under either identity
            // (`DetFd::rebind_open_file_id`).
            detfd.mark_flock_mode_unobserved();
            if let Some(id) = joined {
                detfd.rebind_open_file_id(id);
            }
            detfd.set_path(path);
            detfd.set_procfs(procfs.clone());
            detfd.set_procfs_offset(offset);
            if follows && joined.is_none() {
                detfd.mark_record_sequence_unknown();
            }
            (detfd.open_file_handle(), detfd.open_file_id())
        })?;
        register_placeholder_open(identity, handle, id, snapshot);
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

    /// Linux's answer to an extended-attribute call (`fgetxattr`,
    /// `flistxattr`, `fsetxattr`, `fremovexattr`) through a placeholder
    /// directory Detcore serves (`descriptor_directory`): procfs's, which has
    /// no attribute handlers, as for the files it renders. `owned` is the
    /// process whose descriptor directory it is, which the caller owns and,
    /// as one of its threads, may write; `None` for `/proc`, which global
    /// root owns.
    pub(super) async fn placeholder_xattr_answer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
        owned: Option<i32>,
    ) -> Result<i64, Error> {
        let (_, args) = call.into_parts();
        let answer = match call {
            Syscall::Fgetxattr(c) => match c.name().map(|name| name.read(&guest.memory())) {
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
                Some(Ok(name)) => {
                    let access =
                        Self::placeholder_xattr_access(guest, name.as_bytes(), owned).await?;
                    xattr_change_answer(
                        name.as_bytes(),
                        Some(c.size()),
                        read_value(&guest.memory(), args.arg2 as u64, c.size()).as_deref(),
                        access,
                    )
                }
                Some(Err(errno)) => Err(errno),
                None => Err(Errno::EFAULT),
            },
            Syscall::Fremovexattr(c) => match c.name().map(|name| name.read(&guest.memory())) {
                Some(Ok(name)) => {
                    let access =
                        Self::placeholder_xattr_access(guest, name.as_bytes(), owned).await?;
                    xattr_change_answer(name.as_bytes(), None, None, access)
                }
                Some(Err(errno)) => Err(errno),
                None => Err(Errno::EFAULT),
            },
            _ => Err(Errno::ENOSYS),
        };
        Ok(answer?)
    }

    /// What decides Linux's answer to a change of the extended attribute
    /// `name` through a placeholder directory (see
    /// [`Self::placeholder_xattr_answer`]): global root's `/proc`, or the
    /// caller's own descriptor directory, with `CAP_SYS_ADMIN` from its
    /// effective set, read only for a `security.` attribute.
    async fn placeholder_xattr_access<G: Guest<Self>>(
        guest: &mut G,
        name: &[u8],
        owned: Option<i32>,
    ) -> Result<FileAccess, Error> {
        if owned.is_none() {
            return Ok(FileAccess::GLOBAL_ROOT);
        }
        let capabilities = if name.starts_with(b"security.") {
            Self::inject_capget(guest, CapabilitySet::Effective)
                .await?
                .unwrap_or(0)
        } else {
            0
        };
        Ok(FileAccess::own_descriptor_directory(capabilities))
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
        if !operates_on_descriptor(sysno) {
            return None;
        }
        let fd = args.arg0 as i32;
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
    /// Arguments Linux cannot read: the legacy `utime` and `utimes` times,
    /// which only x86_64 has.
    #[cfg(target_arch = "x86_64")]
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
        #[cfg(target_arch = "x86_64")]
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
        let Some(kind) = path_call_kind(sysno) else {
            return Ok(None);
        };
        let [a0, a1, a2, a3, a4] =
            [args.arg0, args.arg1, args.arg2, args.arg3, args.arg4].map(|arg| arg as u64);
        let at = libc::AT_FDCWD as u64;
        // (dirfd, path, flags, request, attribute name pointer)
        // For `futimens` on an `O_PATH` descriptor: whether the call is done
        // before the descriptor is looked up (both times omitted), or refused
        // first (unknown flags).
        let mut decided_before_descriptor = false;
        let (dirfd, path, flags, request, name) = match kind {
            // Linux declares mode and flags `int`: it ignores the upper bits
            // of the raw registers.
            #[cfg(target_arch = "x86_64")]
            PathCall::Access => (
                at,
                a0,
                0,
                PathRequest::Access {
                    mode: u64::from(a1 as u32),
                    flags: 0,
                },
                0,
            ),
            PathCall::Faccessat => (
                a0,
                a1,
                0,
                PathRequest::Access {
                    mode: u64::from(a2 as u32),
                    flags: 0,
                },
                0,
            ),
            PathCall::Faccessat2 => (
                a0,
                a1,
                u64::from(a3 as u32),
                PathRequest::Access {
                    mode: u64::from(a2 as u32),
                    flags: u64::from(a3 as u32),
                },
                0,
            ),
            PathCall::Getxattr => (at, a0, 0, PathRequest::GetXattr, a1),
            PathCall::Lgetxattr => (at, a0, nofollow(), PathRequest::GetXattr, a1),
            PathCall::Listxattr => (at, a0, 0, PathRequest::ListXattr, 0),
            PathCall::Llistxattr => (at, a0, nofollow(), PathRequest::ListXattr, 0),
            PathCall::Setxattr => (
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
            PathCall::Removexattr => (
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
            #[cfg(target_arch = "x86_64")]
            PathCall::Chmod => (at, a0, 0, PathRequest::Chmod { flags: 0 }, 0),
            PathCall::Fchmodat => (a0, a1, 0, PathRequest::Chmod { flags: 0 }, 0),
            PathCall::Fchmodat2 => (a0, a1, a3, PathRequest::Chmod { flags: a3 }, 0),
            // Ownership changes keep Detcore's policy for every file
            // (`handle_ownership_change_noop`).
            PathCall::Truncate => (at, a0, 0, PathRequest::Truncate { length: a1 as i64 }, 0),
            PathCall::Utimensat => {
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
            #[cfg(target_arch = "x86_64")]
            PathCall::Utime => (
                at,
                a0,
                0,
                legacy_times_request(&guest.memory(), a1, false),
                0,
            ),
            #[cfg(target_arch = "x86_64")]
            PathCall::Utimes => (
                at,
                a0,
                0,
                legacy_times_request(&guest.memory(), a1, true),
                0,
            ),
            // `futimesat` is ENOSYS under Detcore for every file.
            PathCall::Chdir => (at, a0, 0, PathRequest::Chdir, 0),
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
        let renders_comm = self.comm_source() == crate::procfs::task_stat::CommSource::Record;
        let xattr_arguments = if self.renders_procfs(guest)
            && (by_descriptor
                || path_text
                    .as_deref()
                    .is_some_and(|text| could_name_rendered(text, session_files, renders_comm)))
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
            let lexical = if could_name_rendered(&text, files, renders_comm) {
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
        } else if is_utime(sysno) {
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
pub(super) fn descriptor_arguments<M: MemoryAccess>(
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
    let values = [
        args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
    ];
    let fd = |position: usize| values[position] as i32 as RawFd;
    match descriptor_positions(sysno) {
        Some(DescriptorPositions::Always(positions)) => {
            positions.iter().map(|&position| fd(position)).collect()
        }
        Some(DescriptorPositions::WhenPathNamesItself { dirfd, path })
            if names_itself(values[path]) =>
        {
            vec![fd(dirfd)]
        }
        _ => Vec::new(),
    }
}

/// Where a call's descriptor arguments ([`descriptor_arguments`]) are.
#[derive(Clone, Copy, Debug)]
enum DescriptorPositions {
    /// The descriptors at these argument positions, always.
    Always(&'static [usize]),
    /// The directory descriptor at `dirfd`, when the path at `path` is empty
    /// or NULL and so names that descriptor itself.
    WhenPathNamesItself { dirfd: usize, path: usize },
}

/// Whether `sysno` is `utime`, which only x86_64 has.
fn is_utime(sysno: Sysno) -> bool {
    #[cfg(target_arch = "x86_64")]
    return sysno == Sysno::utime;
    #[cfg(not(target_arch = "x86_64"))]
    return {
        let _ = sysno;
        false
    };
}

/// The argument positions of the descriptors a call numbered `sysno` may
/// name ([`descriptor_arguments`]); `None` for a call that names none.
fn descriptor_positions(sysno: Sysno) -> Option<DescriptorPositions> {
    Some(match sysno {
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
        | Sysno::syncfs
        | Sysno::fadvise64
        | Sysno::readahead
        | Sysno::fchmod
        | Sysno::fchown
        | Sysno::fgetxattr
        | Sysno::fsetxattr
        | Sysno::flistxattr
        | Sysno::fremovexattr
        | Sysno::flock
        | Sysno::dup
        | Sysno::dup3
        | Sysno::getdents64
        | Sysno::fchdir => DescriptorPositions::Always(&[0]),
        #[cfg(target_arch = "x86_64")]
        Sysno::sync_file_range | Sysno::dup2 | Sysno::getdents => DescriptorPositions::Always(&[0]),
        #[cfg(target_arch = "aarch64")]
        Sysno::sync_file_range2 => DescriptorPositions::Always(&[0]),
        Sysno::mmap => DescriptorPositions::Always(&[4]),
        Sysno::sendfile | Sysno::tee => DescriptorPositions::Always(&[0, 1]),
        Sysno::splice | Sysno::copy_file_range => DescriptorPositions::Always(&[0, 2]),
        Sysno::epoll_ctl => DescriptorPositions::Always(&[2]),
        Sysno::statx
        | Sysno::utimensat
        | Sysno::faccessat2
        | Sysno::fchownat
        | Sysno::fchmodat
        | Sysno::readlinkat => DescriptorPositions::WhenPathNamesItself { dirfd: 0, path: 1 },
        #[cfg(target_arch = "x86_64")]
        Sysno::newfstatat => DescriptorPositions::WhenPathNamesItself { dirfd: 0, path: 1 },
        #[cfg(target_arch = "aarch64")]
        Sysno::fstatat => DescriptorPositions::WhenPathNamesItself { dirfd: 0, path: 1 },
        _ => return None,
    })
}

/// Whether a call numbered `sysno` may name a descriptor
/// ([`descriptor_arguments`], whatever its arguments): the calls
/// [`Detcore::adopt_received_placeholders`] can act on. Part of
/// `may_name_file_or_descriptor`.
pub(crate) fn names_a_descriptor(sysno: Sysno) -> bool {
    descriptor_positions(sysno).is_some()
}

/// The argument position of the descriptor a call numbered `sysno` writes
/// through, as [`Detcore::answer_rendered_procfs_write`] answers it: `write`,
/// `writev`, `ftruncate`, `fallocate`, `pwritev2` (only at the file
/// position, which its offset decides) and `sendfile` write their argument
/// 0, and `splice` its argument 2. `None` for any other call.
fn written_descriptor_position(sysno: Sysno) -> Option<usize> {
    match sysno {
        Sysno::write
        | Sysno::writev
        | Sysno::ftruncate
        | Sysno::fallocate
        | Sysno::pwritev2
        | Sysno::sendfile => Some(0),
        Sysno::splice => Some(2),
        _ => None,
    }
}

/// Whether a call numbered `sysno` is one whose argument-0 descriptor
/// [`Detcore::answer_rendered_procfs_operation`] answers for a rendered
/// procfs file, after the writes ([`written_descriptor_position`]).
fn operates_on_descriptor(sysno: Sysno) -> bool {
    matches!(
        sysno,
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
            | Sysno::fcntl
    )
}

/// Whether [`Detcore::answer_rendered_procfs_operation`] can answer a call
/// numbered `sysno`: a write through a descriptor
/// ([`written_descriptor_position`], `splice` included, which a `cmdline`
/// source also answers) or an operation on one ([`operates_on_descriptor`]).
/// Part of `may_name_file_or_descriptor`.
pub(crate) fn may_answer_operation(sysno: Sysno) -> bool {
    written_descriptor_position(sysno).is_some() || operates_on_descriptor(sysno)
}

/// Whether [`Detcore::answer_rendered_procfs_mapping`] can answer a call
/// numbered `sysno`: only `mmap` (`Syscall::Mmap`). Part of
/// `may_name_file_or_descriptor`.
pub(crate) fn may_answer_mapping(sysno: Sysno) -> bool {
    sysno == Sysno::mmap
}

/// The calls [`Detcore::answer_rendered_procfs_path_call`] answers, one per
/// arm of its table, which matches on this exhaustively: a call can reach
/// the table only through [`path_call_kind`], and a new arm needs a variant
/// here.
#[derive(Clone, Copy, Debug)]
enum PathCall {
    #[cfg(target_arch = "x86_64")]
    Access,
    Faccessat,
    Faccessat2,
    Getxattr,
    Lgetxattr,
    Listxattr,
    Llistxattr,
    /// `setxattr` and `lsetxattr`.
    Setxattr,
    /// `removexattr` and `lremovexattr`.
    Removexattr,
    #[cfg(target_arch = "x86_64")]
    Chmod,
    Fchmodat,
    Fchmodat2,
    Truncate,
    Utimensat,
    #[cfg(target_arch = "x86_64")]
    Utime,
    #[cfg(target_arch = "x86_64")]
    Utimes,
    Chdir,
}

/// The arm of [`Detcore::answer_rendered_procfs_path_call`]'s table for a
/// call numbered `sysno`, or `None` for a call the table has no arm for.
fn path_call_kind(sysno: Sysno) -> Option<PathCall> {
    Some(match sysno {
        #[cfg(target_arch = "x86_64")]
        Sysno::access => PathCall::Access,
        Sysno::faccessat => PathCall::Faccessat,
        Sysno::faccessat2 => PathCall::Faccessat2,
        Sysno::getxattr => PathCall::Getxattr,
        Sysno::lgetxattr => PathCall::Lgetxattr,
        Sysno::listxattr => PathCall::Listxattr,
        Sysno::llistxattr => PathCall::Llistxattr,
        Sysno::setxattr | Sysno::lsetxattr => PathCall::Setxattr,
        Sysno::removexattr | Sysno::lremovexattr => PathCall::Removexattr,
        #[cfg(target_arch = "x86_64")]
        Sysno::chmod => PathCall::Chmod,
        Sysno::fchmodat => PathCall::Fchmodat,
        Sysno::fchmodat2 => PathCall::Fchmodat2,
        Sysno::truncate => PathCall::Truncate,
        Sysno::utimensat => PathCall::Utimensat,
        #[cfg(target_arch = "x86_64")]
        Sysno::utime => PathCall::Utime,
        #[cfg(target_arch = "x86_64")]
        Sysno::utimes => PathCall::Utimes,
        Sysno::chdir => PathCall::Chdir,
        _ => return None,
    })
}

/// Whether [`Detcore::answer_rendered_procfs_path_call`] can answer a call
/// numbered `sysno`: whether its table has an arm for it
/// ([`path_call_kind`], the only way into the table). Part of
/// `may_name_file_or_descriptor`.
pub(crate) fn may_answer_path_call(sysno: Sysno) -> bool {
    path_call_kind(sysno).is_some()
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
fn read_xattr_name<M: MemoryAccess>(memory: &M, address: u64) -> Result<Vec<u8>, Errno> {
    read_guest_string(memory, address, XATTR_NAME_MAX + 1)
}

/// The NUL-terminated string at guest address `address`, as Linux's
/// `strncpy_from_user` copies it with room for `limit` bytes: its bytes before
/// the NUL, or `limit` bytes when no NUL ends it within them (even when the
/// bytes after them cannot be read), and EFAULT only for a fault within
/// them.
///
/// Each read stays within one page and uses the guest's own access rights
/// (`read_exact_with_user_access`): an exact read that faults only when a
/// needed byte does, as the kernel's copy faults.
pub(super) fn read_guest_string<M: MemoryAccess>(
    memory: &M,
    address: u64,
    limit: usize,
) -> Result<Vec<u8>, Errno> {
    const PAGE: u64 = 4096;
    let mut name = Vec::new();
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
/// number. A task's `comm` is one only where Detcore may render it
/// (`renders_comm`: its record is the name's source, package K slice A+),
/// and a process's `smaps`, `smaps_rollup` and `numa_maps` only where the
/// session renders them (`files.address_space`, package K slice N4).
/// Elsewhere the kernel serves every one of those, and a path call's
/// spelling of one (`faccessat(dirfd, "smaps")`, `getxattr`, `chmod`) goes
/// to the kernel untouched, as before, without the lookup that resolves a
/// spelling (`Detcore::procfs_spelling_at`) or its request to the global
/// tool.
fn could_name_rendered(path: &Path, files: RenderedFiles, renders_comm: bool) -> bool {
    let Some(std::path::Component::Normal(last)) = path.components().next_back() else {
        return false;
    };
    let last = last.as_bytes();
    let address_space_names = [
        AddressSpaceFile::Smaps,
        AddressSpaceFile::SmapsRollup,
        AddressSpaceFile::NumaMaps,
    ]
    .map(AddressSpaceFile::name);
    rendered_paths(files).any(|rendered| rendered_name(rendered) == last)
        || ProcfsFile::PER_PROCESS_NAMES
            .iter()
            .filter(|name| renders_comm || **name != "comm")
            .filter(|name| files.address_space || !address_space_names.contains(*name))
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

/// The argument and environment strings at `area` in a new image's memory,
/// before its first instruction (`tool_global::ExecStrings`): the bytes of
/// `arg_start..arg_end` and `env_start..env_end`, each read whole through
/// [`read_prefix`]. `None` where either cannot be read whole, as the area,
/// which `procfs::initial_stack_cmdline_area` found by reading every string,
/// always can.
fn recorded_exec_strings<M: MemoryAccess>(
    memory: &M,
    area: &crate::procfs::CmdlineArea,
) -> Option<crate::tool_global::ExecStrings> {
    let read = |start: u64, end: u64| {
        let mut bytes = vec![0_u8; usize::try_from(end.checked_sub(start)?).ok()?];
        (bytes.is_empty() || read_prefix(memory, start, &mut bytes) == bytes.len()).then_some(bytes)
    };
    Some(crate::tool_global::ExecStrings {
        argv: read(area.arg_start, area.arg_end)?,
        envp: read(area.env_start, area.env_end)?,
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
#[cfg(target_arch = "x86_64")]
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

/// What Linux answers `utime` (a `struct utimbuf`) or `utimes`/`futimesat`
/// (two `struct timeval`, when `timevals`) from its times at guest address
/// `times` before it looks at the file ([`legacy_times_request`]): EFAULT
/// where they cannot be read, EINVAL for a microsecond count out of range,
/// and `None` otherwise (fs/utimes.c:214-217 and :175-186 at v6.17).
#[cfg(target_arch = "x86_64")]
pub(super) fn legacy_times_error<M: MemoryAccess>(
    memory: &M,
    times: u64,
    timevals: bool,
) -> Option<Errno> {
    match legacy_times_request(memory, times, timevals) {
        PathRequest::Fault => Some(Errno::EFAULT),
        PathRequest::Invalid => Some(Errno::EINVAL),
        _ => None,
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

    /// Where one tool process serves every guest process (ptrace's
    /// capabilities), a memfd identity it did not register is no
    /// placeholder, and learning that takes no injected call: the registry
    /// is the whole answer. Where Detcore runs in each guest process (in-guest
    /// LiteInst's), another process may have made the placeholder, so the
    /// descriptor's name is asked for (`Detcore::received_placeholder`); that
    /// path is exercised end to end by the CLI test
    /// `a_received_diskstats_descriptor_reads_its_table_at_the_descriptor_limit_on_every_backend`.
    #[test]
    fn an_unregistered_identity_is_no_placeholder_where_one_tool_serves_every_guest() {
        use crate::record_or_replay::NoopTool;
        use crate::syscall_hot_path_tests::InjectGuest;
        futures::executor::block_on(async {
            let mut tool = crate::syscall_hot_path_tests::detcore::<NoopTool>();
            tool.cfg.backend = reverie::BackendCapabilities::PTRACE;
            assert!(crate::syscalls::timerfd::detcore_runs_outside_the_guest(
                &tool.cfg.backend
            ));
            assert!(!crate::syscalls::timerfd::detcore_runs_outside_the_guest(
                &reverie::BackendCapabilities::LITEINST_IN_GUEST
            ));
            let mut guest = InjectGuest::<NoopTool>::new(Ok(0));
            // SAFETY: plain integers; zero is a valid value for each.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            stat.st_mode = libc::S_IFREG;
            // No memfd of this run has this identity, so no test registers it.
            stat.st_dev = u64::MAX;
            stat.st_ino = u64::MAX;
            let found = tool
                .received_placeholder(&mut guest, 9, &stat)
                .await
                .unwrap();
            assert!(found.is_none());
            assert!(guest.injected().is_empty(), "{:?}", guest.injected());
        })
    }

    /// Where the guest's procfs is the kernel's (ptrace's capabilities),
    /// every spelling of a process's `smaps`, `smaps_rollup` and `numa_maps`
    /// is the kernel's, decided from the configuration before any request to
    /// the global tool, as before Detcore rendered them (package K slice N4):
    /// an open's, and a path call's too (`faccessat(dirfd, "smaps")`), which
    /// `could_name_rendered` leaves to the kernel. Where it is not (KVM's),
    /// Detcore classifies them. Other per-process files are not affected.
    #[test]
    fn the_kernel_serves_every_address_space_file_where_procfs_is_its() {
        use crate::record_or_replay::NoopTool;
        let mut tool = crate::syscall_hot_path_tests::detcore::<NoopTool>();
        for (backend, kernels) in [
            (reverie::BackendCapabilities::PTRACE, true),
            (reverie::BackendCapabilities::DBT, true),
            (reverie::BackendCapabilities::LITEINST_IN_GUEST, true),
            (reverie::BackendCapabilities::KVM, false),
        ] {
            tool.cfg.backend = backend;
            assert_eq!(tool.renders_address_space_files(), !kernels);
            // The session's files, as `Detcore::rendered_files` makes them.
            let files = RenderedFiles::of(
                true,
                true,
                false,
                !procfs_inode::tool_can_name_guest_descriptors(&tool.cfg.backend),
            );
            assert_eq!(files.address_space, !kernels);
            for path in [
                "/proc/self/smaps",
                "/proc/3/smaps_rollup",
                "/proc/self/task/3/numa_maps",
            ] {
                let file = ProcfsFile::from_path(Path::new(path)).expect(path);
                assert_eq!(
                    tool.kernel_serves_every_address_space_file(&file),
                    kernels,
                    "{path}"
                );
                let name = Path::new(path).file_name().expect(path);
                for spelling in [Path::new(path), Path::new(name)] {
                    assert_eq!(
                        could_name_rendered(spelling, files, false),
                        !kernels,
                        "{spelling:?}"
                    );
                    assert_eq!(
                        could_name_rendered(spelling, files, true),
                        !kernels,
                        "{spelling:?}"
                    );
                }
            }
            let statm = ProcfsFile::from_path(Path::new("/proc/self/statm")).unwrap();
            assert!(!tool.kernel_serves_every_address_space_file(&statm));
            assert!(could_name_rendered(Path::new("statm"), files, false));
        }
        assert!(!could_name_rendered(
            Path::new("/proc/self/smaps"),
            RenderedFiles::WITHOUT_CPUINFO,
            false
        ));
        assert!(could_name_rendered(
            Path::new("/proc/self/smaps"),
            RenderedFiles::ALL,
            false
        ));
    }

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
            crate::arch::set_syscall_number(&mut regs, libc::SYS_openat as u64);
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
        let page = crate::test_pages::page_size();
        // SAFETY: a fresh private anonymous mapping, unmapped below.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                3 * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        let base = base as usize;
        let page1 = base + page;
        let page2 = base + 2 * page;
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
            assert_eq!(libc::munmap(page2 as *mut libc::c_void, page), 0);
            assert_eq!(
                libc::mprotect(base as *mut libc::c_void, page, libc::PROT_NONE),
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
            assert_eq!(libc::munmap(base as *mut libc::c_void, 2 * page), 0);
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

    /// Where the tool shares the guest's root and the guest has changed it,
    /// no task's kernel `comm` is read by path at all: a forged ordinary
    /// file and a substitution on the very procfs (a link or bind mount to
    /// another task's genuine `comm`, which no check on the opened file can
    /// tell apart) both get the named refusal. Shown here: another task's
    /// genuine comm (the parent's) passes the procfs check, so only the
    /// root rule refuses it. A tracer, or an unchanged root, reads; an
    /// ordinary file is refused by the procfs check.
    #[test]
    fn a_comm_is_not_read_through_a_changed_root_the_tool_shares() {
        assert!(!comm_read_is_trusted(true, true));
        assert!(comm_read_is_trusted(true, false));
        assert!(comm_read_is_trusted(false, true));
        assert!(comm_read_is_trusted(false, false));
        let substituted = crate::util::RawFd::open(
            Path::new(&format!(
                "/proc/{}/comm",
                std::os::unix::process::parent_id()
            )),
            libc::O_RDONLY,
        )
        .unwrap();
        assert!(
            comm_file_is_on_procfs(&substituted),
            "a same-device substitution passes every file check"
        );
        let forged = std::env::temp_dir().join(format!(
            "detcore-forged-comm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&forged, b"forged\n").unwrap();
        let file = crate::util::RawFd::open(&forged, libc::O_RDONLY).unwrap();
        assert!(!comm_file_is_on_procfs(&file));
        drop(file);
        std::fs::remove_file(&forged).unwrap();
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
            assert!(
                could_name_rendered(path, RenderedFiles::ALL, false),
                "{path:?}"
            );
        }
        // `/proc/cpuinfo` and `/proc/locks` are rendered only in a session
        // that renders them.
        let without = RenderedFiles::WITHOUT_CPUINFO;
        assert_eq!(rendered_paths(without).count() + 2, paths.len());
        assert!(rendered_paths(without).all(|path| path != c"/proc/cpuinfo"));
        assert!(rendered_paths(without).all(|path| path != c"/proc/locks"));
        assert!(!could_name_rendered(
            Path::new("/proc/cpuinfo"),
            without,
            false
        ));
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
        // A /proc/diskstats placeholder carries no snapshot: the new image
        // renders it again, from the run's table
        // (`ProcfsFile::snapshot_is_rerendered`), so a name carrying one
        // (below) is not one.
        assert_eq!(
            crate::procfs::placeholder_link(
                b"/memfd:hermit-procfs:/proc/diskstats@ (deleted)",
                RenderedFiles::ALL
            ),
            Some((Path::new("/proc/diskstats"), Vec::new()))
        );
        for link in [
            b"/memfd:hermit-procfs:/proc/diskstats@30 (deleted)".as_slice(),
            b"/memfd:hermit-procfs:/proc/cpuinfo@30 (deleted)",
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
            Some(Path::new("/proc/diskstats"))
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

    /// A task's `io` is mode 0400, as `ONE("io", S_IRUSR, ...)` makes it in
    /// Linux, readable by its owner only, like a process's `auxv` (next
    /// test); its `statm` and `arch_status` are 0444. Each is a regular file
    /// with Detcore's procfs device and its task's inode number.
    #[test]
    fn a_task_io_is_readable_by_its_owner_only() {
        let epoch = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        for (name, mode) in [("io", 0o400), ("statm", 0o444), ("arch_status", 0o444)] {
            let mut file =
                crate::procfs::ProcfsFile::from_path(Path::new(&format!("/proc/self/{name}")))
                    .unwrap();
            file.bind_task(3, 3);
            let path = file.into_rendered().opened_path().unwrap();
            assert_eq!(rendered_path_is_owner_only(path), mode == 0o400, "{name}");
            let stat = rendered_stat(path, epoch, (0, 0));
            assert_eq!(stat.mode, libc::S_IFREG | mode, "{name}");
            assert_eq!(stat.dev, procfs_inode::VIRTUAL_PROC_DEVICE, "{name}");
            assert_eq!(stat.inode, procfs_inode::rendered_inode(path), "{name}");
        }
        assert_eq!(
            rendered_stat(Path::new("/proc/diskstats"), epoch, (0, 0)).mode,
            libc::S_IFREG | 0o444
        );
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

    /// What decides another thread group's read of a rendered `io`, from the
    /// reader's readlinks of the task's `cwd` and of its own: the task's
    /// reads, allowed; it is ENOENT while the reader's own reads (a task with
    /// no `fs`, exited and not reaped: the access check passed), allowed; it
    /// is EACCES while the reader's own reads, refused; the reader's own is
    /// EACCES (no `/proc` provider beneath), the reader's `CAP_SYS_PTRACE`
    /// decides; both are ENOENT (no `/proc` under the reader's root), the
    /// kernel beneath is asked without a path (Codex review, round 3: the
    /// reader's capability decided there too, refusing reads Linux allows);
    /// any other errno, and a failed probe of the task with no second probe,
    /// decides nothing. Measured natively on Linux 7.1.3 as root in a user
    /// namespace: the probe of the task and Linux's io read agree for a live
    /// task, a task not dumpable to a reader without CAP_SYS_PTRACE,
    /// commoncap's subset rule, a full descriptor table and an unreaped task.
    #[test]
    fn another_thread_groups_io_is_decided_by_a_readlink_of_its_cwd() {
        use IoAccess::Allowed;
        use IoAccess::CallerCapability;
        use IoAccess::NoProcfs;
        use IoAccess::Refused;
        use IoAccess::Undecided;
        for serves in [None, Some(Ok(())), Some(Err(Errno::EACCES))] {
            assert_eq!(io_access_answer(Ok(()), serves), Allowed);
        }
        assert_eq!(io_access_answer(Err(Errno::ENOENT), Some(Ok(()))), Allowed);
        for serves in [Ok(()), Err(Errno::ENOENT)] {
            assert_eq!(io_access_answer(Err(Errno::EACCES), Some(serves)), Refused);
        }
        for target in [Errno::EACCES, Errno::ENOENT] {
            assert_eq!(
                io_access_answer(Err(target), Some(Err(Errno::EACCES))),
                CallerCapability
            );
            assert_eq!(io_access_answer(Err(target), None), Undecided(target));
        }
        assert_eq!(
            io_access_answer(Err(Errno::ENOENT), Some(Err(Errno::ENOENT))),
            NoProcfs
        );
        for other in [Errno::ENOMEM, Errno::EFAULT, Errno::ELOOP, Errno::EMFILE] {
            assert_eq!(io_access_answer(Err(other), None), Undecided(other));
            assert_eq!(io_access_answer(Err(other), Some(Ok(()))), Undecided(other));
            for target in [Errno::EACCES, Errno::ENOENT] {
                assert_eq!(
                    io_access_answer(Err(target), Some(Err(other))),
                    Undecided(other)
                );
            }
        }
    }

    /// Where the reader's root has no kernel `/proc`, Linux's FSCREDS answer
    /// for another thread group's `io` follows from the kernel's REALCREDS
    /// answer (`kcmp`) and the capability sets, as
    /// `io_access_by_real_credentials` reads kernel/ptrace.c:276-347 and
    /// security/commoncap.c:164-186 in 6.17: a refusal stands; an allowance
    /// stands where the task's permitted set is within the reader's
    /// effective set, where the reader has CAP_SYS_PTRACE in it, or where the
    /// task's permitted set is outside the reader's permitted set; the one
    /// remaining case turns on the user namespaces, so the kernel is asked
    /// the FSCREDS check itself; filesystem IDs that are not the real IDs (or
    /// read as the overflow ID) are unanswerable, never an EACCES.
    #[test]
    fn io_access_without_procfs_follows_the_real_credentials_answer() {
        use IoAccessByCredentials::Allowed;
        use IoAccessByCredentials::AskTheKernel;
        use IoAccessByCredentials::Refused;
        use IoAccessByCredentials::Unanswerable;
        let full: u64 = (1 << 41) - 1;
        let ptrace = 1 << CAP_SYS_PTRACE;
        let reader = |effective: u64, permitted: u64| ReaderCredentials {
            uid: 0,
            euid: 0,
            fsuid: 0,
            gid: 0,
            fsgid: 0,
            capabilities: CapabilitySets {
                effective,
                permitted,
                inheritable: 0,
            },
        };
        // A refusal stands, whatever the sets.
        for (effective, permitted, task) in [
            (full, full, full),
            (full & !ptrace, full & !ptrace, full & !ptrace),
            (0, 0, 0),
        ] {
            assert_eq!(
                io_access_by_real_credentials(false, reader(effective, permitted), task),
                Refused
            );
        }
        // The task's permitted set within the reader's effective set (the
        // chroot test's reader and target, which dropped CAP_SYS_PTRACE
        // from both sets before the fork).
        let dropped = full & !ptrace;
        assert_eq!(
            io_access_by_real_credentials(true, reader(dropped, dropped), dropped),
            Allowed
        );
        assert_eq!(
            io_access_by_real_credentials(true, reader(full, full), full),
            Allowed
        );
        // CAP_SYS_PTRACE in the reader's effective set.
        assert_eq!(
            io_access_by_real_credentials(true, reader(ptrace, full), full),
            Allowed
        );
        // The task's permitted set outside the reader's permitted set: the
        // kernel's allowance came from ns_capable, which reads the effective
        // set in both modes.
        assert_eq!(
            io_access_by_real_credentials(true, reader(dropped, dropped), full),
            Allowed
        );
        // Within the permitted set but not the effective set, which lacks
        // CAP_SYS_PTRACE: the user namespaces decide, so the kernel is asked
        // (the chroot test's reader that dropped it from its effective set
        // only).
        assert_eq!(
            io_access_by_real_credentials(true, reader(dropped, full), full),
            AskTheKernel
        );
        // Filesystem IDs that are not the real IDs, or the overflow ID.
        for (uid, fsuid, gid, fsgid) in [
            (0, 1000, 0, 0),
            (0, 0, 0, 1000),
            (OVERFLOW_ID, OVERFLOW_ID, 0, 0),
            (0, 0, OVERFLOW_ID, OVERFLOW_ID),
            (OVERFLOW_ID, 0, 0, 0),
        ] {
            let credentials = ReaderCredentials {
                uid,
                fsuid,
                gid,
                fsgid,
                ..reader(full, full)
            };
            for allowed in [true, false] {
                assert!(
                    matches!(
                        io_access_by_real_credentials(allowed, credentials, full),
                        Unanswerable(reason) if reason.contains("filesystem IDs")
                    ),
                    "{credentials:?}"
                );
            }
        }
    }

    /// Where the user namespaces decide and the kernel cannot be asked (the
    /// task has exited), the record of user namespaces answers as
    /// `cap_capable_helper` walks (security/commoncap.c:68-104 in 6.17): a
    /// task in the reader's namespace, or not below it, is refused; one below
    /// it is allowed where the reader's effective user ID, its real one, owns
    /// the namespace above the task's, and refused where the reader's
    /// effective ID is another; a run where a namespace's owner may differ,
    /// or a reader whose effective ID reads as the overflow ID, cannot be
    /// told (Codex review of the kvm lane's slice N3, round 3b: the exited
    /// targets of `--io-access-after-chroot-effective-only` and
    /// `-own-userns`).
    #[test]
    fn io_access_by_user_namespaces_follows_the_namespace_walk() {
        use crate::scheduler::user_namespaces::UserNamespaceRelation::*;
        let full: u64 = (1 << 41) - 1;
        let reader = |uid: u32, euid: u32| ReaderCredentials {
            uid,
            euid,
            fsuid: euid,
            gid: 0,
            fsgid: 0,
            capabilities: CapabilitySets {
                effective: full & !(1 << CAP_SYS_PTRACE),
                permitted: full,
                inheritable: 0,
            },
        };
        for owner_may_differ in [false, true] {
            for relation in [Same, Elsewhere] {
                assert_eq!(
                    io_access_by_user_namespaces(relation, owner_may_differ, reader(0, 0)),
                    IoAccessByCredentials::Refused,
                    "{relation:?} {owner_may_differ}"
                );
            }
        }
        assert_eq!(
            io_access_by_user_namespaces(Below, false, reader(0, 0)),
            IoAccessByCredentials::Allowed
        );
        assert_eq!(
            io_access_by_user_namespaces(Below, false, reader(1000, 0)),
            IoAccessByCredentials::Refused
        );
        for (owner_may_differ, credentials) in
            [(true, reader(0, 0)), (false, reader(0, OVERFLOW_ID))]
        {
            assert!(
                matches!(
                    io_access_by_user_namespaces(Below, owner_may_differ, credentials),
                    IoAccessByCredentials::Unanswerable(reason) if reason.contains("cannot be told")
                ),
                "{owner_may_differ} {credentials:?}"
            );
        }
    }

    /// A memfd's descriptor link names the stand-in for a rendered file only
    /// within the run's `--proc` level: at `--proc=minimal`, which omits
    /// `/proc/diskstats` and `/proc/interrupts`, a link naming either names no
    /// stand-in, whichever route reads it (a descriptor link's open, a
    /// spelling that reaches the memfd, the link's text), so such a memfd is
    /// read as the memfd it is; at `--proc=standard` it names the file
    /// (Claude review of the kvm lane's slice N3, round 3: DBT and SaBRe read
    /// both files at `--proc=minimal` through `/proc/self/fd/N`).
    #[test]
    fn a_stand_in_link_names_a_file_only_within_the_proc_level() {
        use detcore_model::config::ProcLevel;
        for files in [RenderedFiles::ALL, RenderedFiles::WITHOUT_CPUINFO] {
            for (level, within) in [(ProcLevel::Minimal, false), (ProcLevel::Standard, true)] {
                for path in ["/proc/diskstats", "/proc/interrupts"] {
                    let link = format!("/memfd:hermit-procfs:{path}@ (deleted)");
                    assert_eq!(
                        placeholder_link_within(link.as_bytes(), files, level),
                        within.then_some(Path::new(path)),
                        "{files:?} {level:?} {path}"
                    );
                }
            }
        }
        // A name of no procfs file names none at either level.
        assert_eq!(
            placeholder_link_within(
                b"/memfd:hermit-procfs:/etc/passwd@ (deleted)",
                RenderedFiles::ALL,
                ProcLevel::Standard
            ),
            None
        );
    }

    /// Once `pidfd_open` succeeded, the `PIDFD_GET_USER_NAMESPACE` ioctl's
    /// answer is the kernel's FSCREDS check: a namespace descriptor, or an
    /// error that only the namespace's open makes after the check (EOPNOTSUPP,
    /// EMFILE, ENFILE, ENOMEM), allows the reader; EACCES refuses it; ESRCH
    /// (an exited task), ENOTTY and EINVAL come before the check and decide
    /// nothing (Claude review of the kvm lane's slice N3, round 3b: a reader
    /// with one free descriptor was stopped where Linux reads).
    #[test]
    fn a_pidfd_ioctl_error_after_the_check_means_allowed() {
        for answer in [
            Ok(7),
            Err(Errno::EOPNOTSUPP),
            Err(Errno::EMFILE),
            Err(Errno::ENFILE),
            Err(Errno::ENOMEM),
        ] {
            assert_eq!(
                pidfd_ioctl_access(answer),
                PidfdAccess::Allowed,
                "{answer:?}"
            );
        }
        assert_eq!(pidfd_ioctl_access(Err(Errno::EACCES)), PidfdAccess::Refused);
        for errno in [Errno::ESRCH, Errno::ENOTTY, Errno::EINVAL, Errno::EBADF] {
            assert_eq!(
                pidfd_ioctl_access(Err(errno)),
                PidfdAccess::NotAsked {
                    opened: true,
                    errno
                }
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
            RenderedFiles::WITHOUT_CPUINFO,
            false
        ));
        assert!(could_name_rendered(
            Path::new("cgroup"),
            RenderedFiles::WITHOUT_CPUINFO,
            false
        ));
    }

    /// A rendered `comm` (package K slice A+) has Linux's mode, 0644, at its
    /// task's path, and Detcore never opens one for writing: an open that
    /// asks to write (a write or read-write access mode, or `O_TRUNC`) is
    /// one it refuses (`asks_to_write_comm`). A read-only or `O_PATH` open
    /// is not, and a write open of any other rendered file stays Detcore's
    /// own access check's. Where the kernel serves every `comm`, a spelling ending in
    /// `comm` is not one Detcore resolves.
    #[test]
    fn a_rendered_comm_is_0644_and_detcore_opens_it_only_for_reading() {
        let bound = |spelling: &str| {
            let mut file = ProcfsFile::from_path(Path::new(spelling)).unwrap();
            file.bind_task(3, 5);
            file
        };
        let comm = bound("/proc/thread-self/comm");
        let path = comm.rendered_path(RenderedFiles::ALL).unwrap();
        assert_eq!(path, Path::new("/proc/3/task/5/comm"));
        let epoch = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            rendered_stat(path, epoch, (0, 0)).mode,
            libc::S_IFREG | 0o644
        );
        let stat = bound("/proc/thread-self/stat");
        let stat_path = stat.rendered_path(RenderedFiles::ALL).unwrap();
        assert_eq!(
            rendered_stat(stat_path, epoch, (0, 0)).mode,
            libc::S_IFREG | 0o444
        );
        for flags in [
            OFlag::O_WRONLY,
            OFlag::O_RDWR,
            OFlag::O_RDWR | OFlag::O_CLOEXEC,
            OFlag::O_ACCMODE,
            OFlag::O_RDONLY | OFlag::O_TRUNC,
            OFlag::O_WRONLY | OFlag::O_APPEND,
        ] {
            assert!(asks_to_write_comm(&comm, flags), "{flags:?}");
            assert!(!asks_to_write_comm(&stat, flags), "{flags:?}");
        }
        for flags in [
            OFlag::O_RDONLY,
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            OFlag::O_PATH,
            OFlag::O_PATH | OFlag::O_CLOEXEC,
        ] {
            assert!(!asks_to_write_comm(&comm, flags), "{flags:?}");
        }
        for spelling in ["/proc/self/comm", "comm", "/proc/3/task/5/comm"] {
            assert!(
                could_name_rendered(Path::new(spelling), RenderedFiles::ALL, true),
                "{spelling}"
            );
            assert!(
                !could_name_rendered(Path::new(spelling), RenderedFiles::ALL, false),
                "{spelling}"
            );
        }
        assert!(could_name_rendered(
            Path::new("/proc/self/stat"),
            RenderedFiles::ALL,
            false
        ));
    }

    /// Detcore refuses an open of a rendered `comm` for writing itself
    /// (package K slice A+): EACCES, with no injected call and no request to
    /// the global tool (the guest records every injection and panics on any
    /// request), by its path and through a descriptor link, for a write or
    /// read-write access mode, `O_TRUNC`, and `creat`'s flags. So the answer
    /// depends on neither the host nor the kernel beneath.
    #[test]
    fn a_write_open_of_a_rendered_comm_is_refused_with_nothing_injected() {
        use crate::record_or_replay::NoopTool;
        use crate::syscall_hot_path_tests::InjectGuest;
        use crate::syscall_hot_path_tests::detcore;
        futures::executor::block_on(async {
            let tool = detcore::<NoopTool>();
            for spelling in ["/proc/self/comm", "/proc/thread-self/comm", "/proc/3/comm"] {
                for via_link in [false, true] {
                    for flags in [
                        OFlag::O_WRONLY,
                        OFlag::O_RDWR,
                        OFlag::O_RDWR | OFlag::O_CLOEXEC,
                        OFlag::O_RDONLY | OFlag::O_TRUNC,
                        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC,
                        OFlag::O_ACCMODE,
                    ] {
                        let mut procfs = ProcfsFile::from_path(Path::new(spelling)).unwrap();
                        procfs.bind_task(3, 5);
                        let target = RenderedTarget { procfs, via_link };
                        let mut guest = InjectGuest::<NoopTool>::new(Ok(0));
                        let answer = tool
                            .open_rendered_procfs(&mut guest, flags, target)
                            .await
                            .map_err(|error| error.into_errno().unwrap());
                        assert_eq!(
                            answer,
                            Err(Errno::EACCES),
                            "{spelling} {flags:?} via_link={via_link}"
                        );
                        assert!(
                            guest.injected().is_empty(),
                            "{spelling} {flags:?}: {:?}",
                            guest.injected()
                        );
                    }
                }
            }
        })
    }

    /// Detcore's walk of a `..` spelling ending in `comm` (package K slice
    /// A+) follows Linux's path walk through the procfs directories it
    /// models: `self` and `thread-self` are links it follows, `..` goes to the
    /// parent, every task a component names is a check in path order, a name
    /// below `task` that is no number is ENOENT, a component after `comm` is
    /// ENOTDIR, and anything else it does not model (a link such as `cwd`, a
    /// directory such as `attr`, a step out of `/proc`) is undetermined.
    #[test]
    fn a_comm_alias_walk_follows_linuxs_lookup_through_what_detcore_models() {
        use crate::procfs::BoundTask;
        let own = (3, 5);
        let task = |process, thread| BoundTask { process, thread };
        let comm = |checks: Vec<BoundTask>, process, thread| CommAliasWalk::Comm {
            checks,
            task: task(process, thread),
        };
        let fails = |checks: Vec<BoundTask>, errno| CommAliasWalk::Fails { checks, errno };
        for (path, expected) in [
            (
                "/proc/self/task/5/../5/comm",
                comm(vec![task(3, Some(5)), task(3, Some(5))], 3, Some(5)),
            ),
            (
                "/proc/self/task/9/../5/comm",
                comm(vec![task(3, Some(9)), task(3, Some(5))], 3, Some(5)),
            ),
            (
                "/proc/thread-self/../5/comm",
                comm(vec![task(3, Some(5))], 3, Some(5)),
            ),
            ("/proc/thread-self/../../comm", comm(vec![], 3, None)),
            ("/proc/self/../3/comm", comm(vec![task(3, None)], 3, None)),
            ("/proc/../proc/self/comm", comm(vec![], 3, None)),
            ("/proc/./self/task/../comm", comm(vec![], 3, None)),
            (
                "/proc/7/../7/task/8/comm",
                comm(
                    vec![task(7, None), task(7, None), task(7, Some(8))],
                    7,
                    Some(8),
                ),
            ),
            ("/proc/self/comm/../comm", fails(vec![], Errno::ENOTDIR)),
            ("/proc/self/task/x/../5/comm", fails(vec![], Errno::ENOENT)),
            ("/proc/self/task/05/../5/comm", fails(vec![], Errno::ENOENT)),
            ("/proc/007/../self/comm", fails(vec![], Errno::ENOENT)),
            ("/proc/self/task/comm/../comm", fails(vec![], Errno::ENOENT)),
            (
                "/proc/self/attr/../comm",
                CommAliasWalk::Undetermined(vec![]),
            ),
            (
                "/proc/self/cwd/../comm",
                CommAliasWalk::Undetermined(vec![]),
            ),
            (
                "/proc/7/fd/../comm",
                CommAliasWalk::Undetermined(vec![task(7, None)]),
            ),
            ("/proc/../etc/comm", CommAliasWalk::Undetermined(vec![])),
            ("/proc/self/comm", CommAliasWalk::NotApplicable),
            ("/proc/self/task/../stat", CommAliasWalk::NotApplicable),
            ("/tmp/../proc/self/comm", CommAliasWalk::NotApplicable),
            ("proc/self/../self/comm", CommAliasWalk::NotApplicable),
        ] {
            assert_eq!(walk_comm_alias(Path::new(path), own), expected, "{path}");
        }
    }

    /// Detcore's walk of a `..` spelling to `/proc/locks` or a descriptor's
    /// fdinfo (package K slice K8-d) resolves it as Linux's path walk does:
    /// `self` and `thread-self` are links it follows first (so
    /// `/proc/self/..` is `/proc`), a `..` then goes back through procfs's
    /// own directories (`fd` and `fdinfo` among them), every task a number
    /// names on the way is a check in path order, and the first prefix that
    /// names one of the two files decides: the file, or the file with a
    /// component or a slash after it. A `..` after anything procfs's walk
    /// cannot undo by name (a link such as `cwd`, a step out of `/proc`) and
    /// a spelling without `..` are not the walk's. More than `MAXSYMLINKS`
    /// links before the file end the walk.
    #[test]
    fn a_lock_or_fdinfo_alias_walk_follows_linuxs_lookup() {
        use super::super::descriptor_directory::NamedTask;
        let named = |process, thread| NamedTask { process, thread };
        let file = |file: &str, tasks: Vec<NamedTask>| LockOrFdinfoAlias::File {
            file: std::path::PathBuf::from(file),
            tasks,
        };
        let past = |file: &str, tasks: Vec<NamedTask>| LockOrFdinfoAlias::PastFile {
            file: std::path::PathBuf::from(file),
            tasks,
        };
        for (path, expected) in [
            ("/proc/self/../locks", file("/proc/locks", vec![])),
            ("/proc/../proc/locks", file("/proc/locks", vec![])),
            ("/proc/./self/../locks", file("/proc/locks", vec![])),
            (
                "/proc/7/../locks",
                file("/proc/locks", vec![named(Some(7), None)]),
            ),
            (
                "/proc/self/task/5/../../../locks",
                file("/proc/locks", vec![named(None, Some(5))]),
            ),
            (
                "/proc/self/fdinfo/../fdinfo/3",
                file("/proc/self/fdinfo/3", vec![]),
            ),
            (
                "/proc/self/fd/../fdinfo/3",
                file("/proc/self/fdinfo/3", vec![]),
            ),
            (
                "/proc/self/../self/fdinfo/3",
                file("/proc/self/fdinfo/3", vec![]),
            ),
            (
                "/proc/thread-self/fdinfo/../fdinfo/3",
                file("/proc/thread-self/fdinfo/3", vec![]),
            ),
            (
                "/proc/self/task/5/fdinfo/../fdinfo/3",
                file("/proc/self/task/5/fdinfo/3", vec![named(None, Some(5))]),
            ),
            (
                "/proc/7/fdinfo/../fdinfo/3",
                file("/proc/7/fdinfo/3", vec![named(Some(7), None)]),
            ),
            (
                "/proc/self/fdinfo/3/..",
                past("/proc/self/fdinfo/3", vec![]),
            ),
            (
                "/proc/self/fdinfo/3/../3",
                past("/proc/self/fdinfo/3", vec![]),
            ),
            (
                "/proc/self/fdinfo/x/../3",
                past("/proc/self/fdinfo/x", vec![]),
            ),
            ("/proc/locks/..", past("/proc/locks", vec![])),
            ("/proc/locks/../locks", past("/proc/locks", vec![])),
            ("/proc/self/../locks/", past("/proc/locks", vec![])),
            ("/proc/self/../locks/.", past("/proc/locks", vec![])),
            (
                "/proc/thread-self/../locks",
                LockOrFdinfoAlias::NotApplicable,
            ),
            ("/proc/self/cwd/../locks", LockOrFdinfoAlias::NotApplicable),
            ("/tmp/../proc/locks", LockOrFdinfoAlias::NotApplicable),
            ("/proc/self/../stat", LockOrFdinfoAlias::NotApplicable),
            ("/proc/locks", LockOrFdinfoAlias::NotApplicable),
            ("/proc/self/fdinfo/3", LockOrFdinfoAlias::NotApplicable),
            ("proc/self/../locks", LockOrFdinfoAlias::NotApplicable),
        ] {
            assert_eq!(
                walk_lock_or_fdinfo_alias(Path::new(path)),
                expected,
                "{path}"
            );
        }
        // Linux's limit on the links one lookup follows: each `self` or
        // `thread-self` looked up in `/proc` before the file is one, counted
        // in lookup order. 40 resolve, the 41st is ELOOP; a task named or a
        // file found before the 41st link decides first, and a task named
        // after it is never looked up.
        let limit = |file: &str, tasks: Vec<NamedTask>| LockOrFdinfoAlias::LinkLimit {
            file: std::path::PathBuf::from(file),
            tasks,
        };
        let repeated = |count: usize| "self/../".repeat(count);
        for (path, expected) in [
            (
                format!("/proc/{}locks", repeated(40)),
                file("/proc/locks", vec![]),
            ),
            (
                format!("/proc/{}locks", repeated(41)),
                limit("/proc/locks", vec![]),
            ),
            (
                format!("/proc/{}locks/..", repeated(41)),
                limit("/proc/locks", vec![]),
            ),
            (
                format!("/proc/{}self/fdinfo/3", repeated(39)),
                file("/proc/self/fdinfo/3", vec![]),
            ),
            (
                format!("/proc/{}self/fdinfo/3", repeated(40)),
                limit("/proc/self/fdinfo/3", vec![]),
            ),
            (
                format!("/proc/thread-self/../../../{}locks", repeated(39)),
                file("/proc/locks", vec![]),
            ),
            (
                format!("/proc/thread-self/../../../{}locks", repeated(40)),
                limit("/proc/locks", vec![]),
            ),
            (
                format!("/proc/7/../{}locks", repeated(41)),
                limit("/proc/locks", vec![named(Some(7), None)]),
            ),
            (
                format!("/proc/{}7/../locks", repeated(41)),
                limit("/proc/locks", vec![]),
            ),
            (
                format!("/proc/{}7/../locks", repeated(40)),
                file("/proc/locks", vec![named(Some(7), None)]),
            ),
            (
                format!("/proc/locks/../{}locks", repeated(41)),
                past("/proc/locks", vec![]),
            ),
        ] {
            assert_eq!(
                walk_lock_or_fdinfo_alias(Path::new(&path)),
                expected,
                "{path}"
            );
        }
        // The links the lookup followed before these components (a respelled
        // path's descriptor link and the selector link before it,
        // `Detcore::decided_path`) count against the same limit: after two,
        // 38 `self` links resolve; after three, the 38th is the 41st link.
        let walked = format!("/proc/{}self", repeated(37));
        let components = walked
            .split('/')
            .filter(|name| !name.is_empty())
            .map(str::as_bytes)
            .collect::<Vec<_>>();
        assert_eq!(past_the_link_limit(&components, 0), None);
        assert_eq!(past_the_link_limit(&components, 2), None);
        assert_eq!(past_the_link_limit(&components, 3), Some(vec![]));
        let walked = format!("/proc/7/../{}self", repeated(37));
        let components = walked
            .split('/')
            .filter(|name| !name.is_empty())
            .map(str::as_bytes)
            .collect::<Vec<_>>();
        assert_eq!(
            past_the_link_limit(&components, 3),
            Some(vec![named(Some(7), None)])
        );
    }
}
