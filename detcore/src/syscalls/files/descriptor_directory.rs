/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The `/proc` directories Detcore serves itself, each on a placeholder
//! descriptor: the caller's own descriptor directory, `/proc/<pid>/fd`, from
//! Detcore's own descriptor table (package K slice F1, toward
//! <https://github.com/rrnewton/hermit/issues/4604>), and `/proc` itself, from
//! Detcore's procfs model (`procfs::listing`, package K slice P1). The models,
//! and the parts of Linux they follow, are in [`crate::procfs::fd_directory`].
//!
//! An open of such a directory injects an open of the descriptor's object,
//! with the caller's `O_CLOEXEC`, and records which directory the descriptor
//! stands for (`DetFd::placeholder`). The object depends on one capability,
//! whether the guest's procfs is the kernel's
//! (`procfs_inode::tool_can_name_guest_descriptors`), and on nothing else:
//! - where it is (ptrace), the kernel's procfs root, `openat(AT_FDCWD,
//!   "/proc", O_RDONLY | O_DIRECTORY)`, as slice F1 opened it. The kernel
//!   resolves every path through the descriptor: below `/proc` itself, and
//!   through its descriptor link, exactly as Linux does. A name looked up
//!   below a descriptor directory is looked up below that directory's own
//!   path within procfs, from the descriptor itself (`<pid>/fd/<name>`
//!   relative to it), and a path through a descriptor link to one goes on
//!   below that path after the link (`/proc/self/fd/D/<pid>/fd/<name>`), so
//!   that every lookup starts where the guest's does, whatever the caller's
//!   root or PID namespace ([`Detcore::respelled_below_descriptor_directory`]);
//! - where it is not (KVM, whose executor serves no procfs directory), the
//!   placeholder directory, the empty, read-only
//!   tmpfs hermit-cli mounts at the container's `/proc/fs/nfsd`
//!   (`DetConfig::directory_placeholder`), procfs's own permanently empty
//!   mount point. A name looked up below the descriptor, or through its
//!   link, is looked up at the directory's absolute spelling (`/proc/x`,
//!   `/proc/self/fd/N`) instead, for each injection that carries it.
//!
//! Either way Detcore answers every call through the descriptor from its
//! model, with one implementation ([`Detcore::answer_placeholder_call`]):
//! `getdents` and `getdents64`, `lseek`, `fstat` and `statx`, `fstatfs`, the
//! fdinfo, `fcntl`'s status flags and write hints, the `O_PATH` refusals,
//! `fchdir`, `flock` (each directory its own lock, as each has an inode of its
//! own on Linux), and every call procfs refuses, with procfs's errno, in
//! Linux's order of checks. Only the object differs, and the guest never sees
//! it: the placeholder directory is hidden. Detcore's `--proc` level table
//! hides `/proc/fs` at every level, and KVM's executor hides every other
//! spelling of the placeholder too, for hermit-cli reserves it there
//! (`reverie_kvm::KvmBackend::reserve_tool_directory`, which requires a
//! read-only file system, as the fresh read-only tmpfs is): every lookup by
//! such a spelling, and every call that changes a name whose parent the
//! executor does not find on procfs, fails with `ENOENT`, as for the absent
//! `/proc/fs`. Where Detcore leaves such a spelling to the executor and its
//! parent is procfs's own `/proc/fs` (a path relative to the working
//! directory, <https://github.com/rrnewton/hermit/issues/4541>, or a lookup
//! from an inherited procfs descriptor,
//! <https://github.com/rrnewton/hermit/issues/4816>), the calls that check
//! that parent first answer procfs's `EACCES` instead: `mkdir`, `mknod`,
//! `unlink`, `rmdir`, `rename` (from, over and exchange), `link`,
//! `symlink`'s new path, `fchmodat` (`chmod`) and `utimensat` (`utimes`). A
//! creation through a final symbolic link into the placeholder, and the
//! other cases Reverie's contract names (a race between a check and its
//! call; a creation below another user's link under
//! `fs.protected_symlinks`), get the read-only file system's `EROFS` (or
//! `EXDEV` for a racing `rename`) and create nothing. An open or stat that
//! reaches it by a spelling Detcore made itself is hidden after the call
//! ([`Detcore::hidden_placeholder_reached`]). hermit-cli mounts the
//! placeholder only where the guest's procfs is not the kernel's
//! ([`crate::needs_directory_placeholder`]): on ptrace there is none.
//!
//! The table a descriptor directory lists is Detcore's own, which the tool
//! keeps for the guest (ptrace, KVM) and F0a made complete: every descriptor
//! it models, and every one it knows is open but does not model
//! (`FileMetadata::present_unmodelled`). Where a receive may have installed
//! descriptors Detcore cannot name (`FileMetadata::unaccounted_receipts`),
//! and for another process's or thread's table (a descriptor a fork's child
//! inherited), the kernel beneath the backend lists `/proc/<pid>/fd`, as
//! before.
//!
//! Where Detcore runs in the guest process and discovers descriptors only as
//! they are used (`BackendCapabilities::tool_shares_guest_descriptor_table`:
//! DBT, SaBRe, LiteInst, in-guest-trap), the guest's table also holds the
//! backend runtime's own descriptors, which no fact names but the one
//! `Guest::is_runtime_owned_fd` reports (DBT's RPC socket): measured,
//! DynamoRIO's 197 to 201 under DBT, and SaBRe's 3, 101 and 1025, in a
//! listing of the guest's table. A listing of that table could not be
//! Linux's, so there both directories stay the kernel's, as before (a
//! capability-keyed residual, until a provider fact names the runtime's
//! descriptors). Without a placeholder (`--no-namespace`, library callers,
//! record and replay, a user mount that covers it) Detcore serves neither
//! directory: on ptrace each is the kernel's, and where the guest's procfs is
//! not the kernel's an open of either is refused with `EACCES` before it
//! reaches the backend ([`Detcore::refuses_backend_procfs_directory`]), whose
//! executor would refuse it too.

// TODO-HUMAN-REVIEW(#4604): Review the Detcore-served descriptor directory, a
// new determinization strategy: on ptrace its listing replaces the kernel's.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use nix::fcntl::OFlag;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::FcntlCmd;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::StatxMask;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use tracing::warn;

use super::DirectoryOrder;
use super::GetdentsCall;
use super::StatTarget;
use crate::dirents::DirEntry;
use crate::dirents::DirentFormat;
use crate::fd::FdType;
use crate::fd::PlaceholderState;
use crate::procfs::fd_directory;
use crate::procfs::fd_directory::DescriptorDirectoryBinding;
use crate::procfs::fd_directory::DescriptorDirectoryTable;
use crate::procfs::fd_directory::PlaceholderDirectory;
use crate::procfs_inode;
use crate::procfs_inode::DirectoryInodes;
use crate::procfs_inode::ProcfsDirectory;
use crate::record_or_replay::RecordOrReplay;
use crate::stat::DetStat;
use crate::stat::StatxAttributes;
use crate::tool_local::Detcore;

/// The identities `(dev, ino)` of the objects placeholder descriptors are
/// open on, as the guest's backend reports them, learnt at each placeholder
/// open in this tool process: procfs's root where the guest's procfs is the
/// kernel's, else the placeholder directory. A received directory descriptor
/// of such an identity whose kernel position is a placeholder's tag is that
/// placeholder ([`Detcore::recognize_received_placeholder`]); no guest
/// descriptor on it is ever seeked into the tags' range
/// ([`Detcore::seek_would_place_a_tag`]), so a tag proves a placeholder.
static PLACEHOLDER_IDENTITIES: Mutex<BTreeSet<(u64, u64)>> = Mutex::new(BTreeSet::new());

/// The identity `(dev, ino)` of the placeholder directory hermit-cli mounted
/// (`DetConfig::directory_placeholder`), as the guest's backend reports it,
/// once learnt in this tool process ([`Detcore::hidden_placeholder_identity`]):
/// what no open or stat of the guest's may reach unless Detcore made the
/// descriptor ([`Detcore::hidden_placeholder_reached`]).
static HIDDEN_IDENTITY: Mutex<Option<(u64, u64)>> = Mutex::new(None);

/// Whether this tool process has opened a placeholder: until it has, no path
/// can lead below one and no received descriptor can be one, and nothing is
/// read or injected to look ([`Detcore::may_respell_below_descriptor_directory`],
/// [`Detcore::recognize_received_placeholder`]).
static OPENED_ANY: AtomicBool = AtomicBool::new(false);

/// What a placeholder descriptor is open on: the kernel's procfs root, where
/// the guest's procfs is the kernel's (`procfs_inode::
/// tool_can_name_guest_descriptors`: ptrace), so the kernel resolves every path
/// through the descriptor below procfs; else the placeholder directory, an
/// empty directory a path below which reaches nothing (KVM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaceholderObject {
    /// The kernel's procfs root.
    ProcfsRoot,
    /// The placeholder directory hermit-cli mounted.
    Placeholder,
}

/// The directory an open names.
#[derive(Debug, Clone, Copy)]
pub(super) enum OpenedDirectory {
    /// One of the caller's own descriptor directories, by its spelling.
    Own(DescriptorDirectoryTable),
    /// `/proc` itself, by any spelling that reaches it ([`names_procfs_root`]).
    Root,
    /// The directory a descriptor link of the caller's (`/proc/self/fd/N`)
    /// reaches: the placeholder directory `N` is.
    Reopened(PlaceholderDirectory),
}

/// The calls whose path operands are each looked up relative to a directory
/// descriptor operand, as (descriptor, path) argument indices. Not `execve`
/// or `execveat`, whose path Detcore refuses by name where it leads below a
/// placeholder ([`Detcore::refuse_lookup_below_placeholder`]): an exec reads
/// its image before Detcore could stage a respelled path for it.
fn relative_operands(sysno: Sysno) -> &'static [(usize, usize)] {
    match sysno {
        Sysno::openat
        | Sysno::openat2
        | Sysno::newfstatat
        | Sysno::statx
        | Sysno::faccessat
        | Sysno::faccessat2
        | Sysno::readlinkat
        | Sysno::mkdirat
        | Sysno::mknodat
        | Sysno::unlinkat
        | Sysno::fchmodat
        | Sysno::fchmodat2
        | Sysno::fchownat
        | Sysno::utimensat
        | Sysno::futimesat
        | Sysno::name_to_handle_at => &[(0, 1)],
        Sysno::symlinkat => &[(1, 2)],
        Sysno::renameat | Sysno::renameat2 | Sysno::linkat => &[(0, 1), (2, 3)],
        _ => &[],
    }
}

/// The path operands of the calls that look a path up from the working
/// directory or the root, by argument index. With [`relative_operands`]'s
/// paths, an absolute one may lead through a descriptor link to a
/// placeholder (`/proc/self/fd/D/N`), which
/// [`Detcore::respelled_below_descriptor_directory`] spells as Linux resolves
/// it.
fn absolute_operands(sysno: Sysno) -> &'static [usize] {
    match sysno {
        Sysno::open
        | Sysno::creat
        | Sysno::stat
        | Sysno::lstat
        | Sysno::access
        | Sysno::readlink
        | Sysno::chdir
        | Sysno::chroot
        | Sysno::mkdir
        | Sysno::rmdir
        | Sysno::unlink
        | Sysno::chmod
        | Sysno::chown
        | Sysno::lchown
        | Sysno::truncate
        | Sysno::utime
        | Sysno::utimes
        | Sysno::mknod
        | Sysno::statfs
        | Sysno::getxattr
        | Sysno::lgetxattr
        | Sysno::setxattr
        | Sysno::lsetxattr
        | Sysno::listxattr
        | Sysno::llistxattr
        | Sysno::removexattr
        | Sysno::lremovexattr => &[0],
        Sysno::link | Sysno::rename => &[0, 1],
        Sysno::symlink | Sysno::inotify_add_watch => &[1],
        _ => &[],
    }
}

/// Whether `sysno`'s lookup of its path argument `path` (by index) follows
/// a final symbolic link, so that a path that is a descriptor link to a
/// placeholder, and nothing more (`/proc/self/fd/D`), names the directory
/// the link reaches rather than the link: as each call's `LOOKUP_FOLLOW`
/// says (v6.17). The calls that operate on a final link itself (`lstat`,
/// `readlink`, the `l*xattr` calls, `lchown`, the calls that make, remove or
/// rename a name, `AT_SYMLINK_NOFOLLOW`, `inotify_add_watch`'s
/// `IN_DONT_FOLLOW`) do not; nor `open` and `openat`, whose open of such a
/// link Detcore makes itself (`OpenedDirectory::Reopened`), or which opens
/// the link itself with `O_NOFOLLOW`.
fn follows_final(sysno: Sysno, args: &syscalls::SyscallArgs, path: usize) -> bool {
    const IN_DONT_FOLLOW: i32 = 0x0200_0000;
    let flags = |index: usize| args_flag(args, index);
    match sysno {
        Sysno::stat
        | Sysno::access
        | Sysno::faccessat
        | Sysno::chdir
        | Sysno::chroot
        | Sysno::chmod
        | Sysno::fchmodat
        | Sysno::chown
        | Sysno::truncate
        | Sysno::utime
        | Sysno::utimes
        | Sysno::futimesat
        | Sysno::statfs
        | Sysno::getxattr
        | Sysno::setxattr
        | Sysno::listxattr
        | Sysno::removexattr => true,
        Sysno::newfstatat | Sysno::faccessat2 | Sysno::fchmodat2 | Sysno::utimensat => {
            flags(3) & libc::AT_SYMLINK_NOFOLLOW == 0
        }
        Sysno::statx => flags(2) & libc::AT_SYMLINK_NOFOLLOW == 0,
        Sysno::fchownat => flags(4) & libc::AT_SYMLINK_NOFOLLOW == 0,
        Sysno::name_to_handle_at => flags(4) & libc::AT_SYMLINK_FOLLOW != 0,
        Sysno::linkat => path == 1 && flags(4) & libc::AT_SYMLINK_FOLLOW != 0,
        Sysno::inotify_add_watch => flags(2) & IN_DONT_FOLLOW == 0,
        _ => false,
    }
}

/// `openat2`'s `RESOLVE_*` flags that scope a lookup to the directory it
/// starts at, or to that directory's mount: `RESOLVE_BENEATH`,
/// `RESOLVE_IN_ROOT` and `RESOLVE_NO_XDEV`. A respelled lookup starts at the
/// root instead, from `AT_FDCWD`, where none of them would mean what the
/// caller asked, so an `openat2` with one of them below a placeholder is
/// refused (`EXDEV`, the answer each gives a lookup that leaves its scope:
/// named residual). Detcore answers every guest `openat2` with `ENOSYS`
/// today (`lib.rs`), so this guards the respelling for any later caller.
const SCOPED_RESOLVE: u64 = libc::RESOLVE_BENEATH | libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_XDEV;

/// The argument `index` of a call's raw arguments.
fn argument(args: &syscalls::SyscallArgs, index: usize) -> usize {
    match index {
        0 => args.arg0,
        1 => args.arg1,
        2 => args.arg2,
        3 => args.arg3,
        4 => args.arg4,
        _ => args.arg5,
    }
}

/// `relative`, a path looked up below a placeholder directory, below `base`,
/// the directory's absolute spelling: with its `.` components and repeated
/// slashes left out, which name nothing (so that a valid path of `./`
/// repeats, up to Linux's limit, stays one), keeping a final `/` where the
/// path ends in one or in `.` (Linux then requires a directory and follows
/// a final link; a final `..` is a directory already). A path of only `.`
/// components, or none, is the directory itself, `base/.`. The bytes of each
/// component are kept as they are, whatever they hold.
fn below(base: &Path, relative: &[u8]) -> Vec<u8> {
    let components = relative
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty() && *component != b".")
        .collect::<Vec<_>>();
    let mut out = base.as_os_str().as_bytes().to_vec();
    if components.is_empty() {
        out.extend_from_slice(b"/.");
        return out;
    }
    for component in &components {
        out.push(b'/');
        out.extend_from_slice(component);
    }
    let last = relative
        .rsplit(|byte| *byte == b'/')
        .next()
        .unwrap_or_default();
    if relative.ends_with(b"/") || last == b"." {
        out.push(b'/');
    }
    out
}

/// What one component of a path below `/proc` is, for
/// [`resolved_procfs_dotdot`]: a directory every procfs has, which a `..`
/// can leave as Linux's walk does, by kind; or anything else, which it
/// cannot (a file, a link that leads elsewhere, a name that may not exist).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProcfsLevel {
    /// `/proc`.
    Proc,
    /// A process's directory: `self`, or a number.
    Process,
    /// A process's `task`.
    Tasks,
    /// A thread's directory: a number in `task`.
    Thread,
    /// `thread-self`'s thread, which spells back as `thread-self`.
    ThreadSelf,
    /// A process's or thread's `fd`.
    Descriptors,
    /// Anything else.
    Opaque,
}

/// A task that a path below `/proc` names by number on the way, which
/// Linux's walk looks up there and which must exist for the walk to go on
/// (`proc_pid_lookup`, `proc_task_lookup`, v6.17 `fs/proc/base.c`): a task
/// by its number at the top (`/proc/<n>`, any thread's number, as Linux finds
/// any task there), or a thread of a process (`/proc/<n>/task/<m>`), the
/// process `None` for the caller's own (`self`, `thread-self`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct NamedTask {
    /// The number at the top, `None` for the caller's own process. A number
    /// no task can have (one past any `pid_t`) is 0, which names none.
    pub(super) process: Option<i32>,
    /// The thread below `task`, if one is named.
    pub(super) thread: Option<i32>,
}

/// `path`, absolute, with each `..` resolved as Linux's walk resolves it
/// where every component a `..` leaves is a directory every procfs has on
/// the way down from `/proc`, so no symbolic link or file can be on the way:
/// `self` (a link to the caller's process directory, one level down) and a
/// numbered process directory, `thread-self` (a link to `<pid>/task/<tid>`,
/// three levels down), a process's `task` and a numbered thread in it, and
/// `fd`. A `..` at the root stays there. With it, every task the path names
/// by number on the way, in order ([`NamedTask`]): the walk looks each up, so
/// the path names what this resolves it to only while each exists, and
/// fails with `ENOENT` at the first that does not
/// ([`Detcore::named_tasks_exist`]). `None` for a path without `..`, and for
/// one whose `..` follows anything else, which only the kernel's walk can
/// resolve.
pub(super) fn resolved_procfs_dotdot(path: &Path) -> Option<(PathBuf, Vec<NamedTask>)> {
    let bytes = path.as_os_str().as_bytes();
    if !bytes.starts_with(b"/") {
        return None;
    }
    let components = bytes
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty() && *component != b".")
        .collect::<Vec<_>>();
    if !components.contains(&b"..".as_slice()) {
        return None;
    }
    // A decimal number procfs looks up (no leading zero, as `name_to_int`
    // takes), as the task number it names: 0 for one no task can have.
    let number = |name: &[u8]| {
        (!name.is_empty() && name[0] != b'0' && name.iter().all(u8::is_ascii_digit)).then(|| {
            std::str::from_utf8(name)
                .ok()
                .and_then(|name| name.parse::<i32>().ok())
                .unwrap_or(0)
        })
    };
    // Each component kept, with what it is, whether `thread-self`'s
    // expansion pushed it, and the process number it names (for a process
    // directory: `None` for `self`).
    let mut kept: Vec<(&[u8], ProcfsLevel, bool, Option<i32>)> = Vec::new();
    let mut tasks = Vec::new();
    for name in components {
        if name == b".." {
            match kept.last() {
                None => {}
                Some((_, ProcfsLevel::Opaque, _, _)) => return None,
                Some(_) => {
                    kept.pop();
                }
            }
            continue;
        }
        let parent = kept.last().map(|(_, level, _, _)| *level);
        let level = match (parent, name) {
            (None, b"proc") => ProcfsLevel::Proc,
            (Some(ProcfsLevel::Proc), b"self") => {
                kept.push((name, ProcfsLevel::Process, false, None));
                continue;
            }
            (Some(ProcfsLevel::Proc), b"thread-self") => {
                kept.push((b"self", ProcfsLevel::Process, true, None));
                kept.push((b"task", ProcfsLevel::Tasks, true, None));
                kept.push((b"thread-self", ProcfsLevel::ThreadSelf, true, None));
                continue;
            }
            (Some(ProcfsLevel::Proc), name) if number(name).is_some() => {
                let process = number(name);
                tasks.push(NamedTask {
                    process,
                    thread: None,
                });
                kept.push((name, ProcfsLevel::Process, false, process));
                continue;
            }
            (Some(ProcfsLevel::Process), b"task") => ProcfsLevel::Tasks,
            (Some(ProcfsLevel::Tasks), name) if number(name).is_some() => {
                // The process directory two levels up.
                let process = kept.len().checked_sub(2).and_then(|index| kept[index].3);
                tasks.push(NamedTask {
                    process,
                    thread: number(name),
                });
                ProcfsLevel::Thread
            }
            (Some(ProcfsLevel::Process | ProcfsLevel::Thread | ProcfsLevel::ThreadSelf), b"fd") => {
                ProcfsLevel::Descriptors
            }
            _ => ProcfsLevel::Opaque,
        };
        kept.push((name, level, false, None));
    }
    let mut out = Vec::new();
    let mut index = 0;
    while index < kept.len() {
        // `thread-self`'s expansion, whole, spells back as itself.
        if kept[index].2
            && kept
                .get(index + 2)
                .is_some_and(|(_, level, _, _)| *level == ProcfsLevel::ThreadSelf)
        {
            out.extend_from_slice(b"/thread-self");
            index += 3;
            continue;
        }
        out.push(b'/');
        out.extend_from_slice(kept[index].0);
        index += 1;
    }
    if out.is_empty() {
        out.push(b'/');
    }
    Some((PathBuf::from(std::ffi::OsString::from_vec(out)), tasks))
}

/// Whether the absolute guest path `path` names `/proc` itself: its
/// components, with the empty and `.` ones left out, are `proc` alone, or
/// reach it back through `..` from directories every procfs has
/// ([`resolved_procfs_dotdot`]), as `Some` of the tasks that spelling names
/// by number on the way, each of which must exist for it to
/// ([`Detcore::named_tasks_exist`]). Any other spelling is the kernel's to
/// resolve.
fn names_procfs_root(path: &Path) -> Option<Vec<NamedTask>> {
    let (resolved, tasks) =
        resolved_procfs_dotdot(path).unwrap_or_else(|| (path.to_path_buf(), Vec::new()));
    (resolved.as_os_str().as_bytes().starts_with(b"/")
        && resolved
            .components()
            .filter(|component| *component != std::path::Component::CurDir)
            .eq(Path::new("/proc").components()))
    .then_some(tasks)
}

/// Whether the absolute path `path` spells a descriptor directory, the
/// caller's or another task's, whatever its numbers: `/proc/<n>/fd`,
/// `/proc/<n>/task/<m>/fd` (`<n>` a number, `self` or `thread-self`), or
/// `/dev/fd`, with empty and `.` components left out
/// ([`Detcore::refuses_backend_procfs_directory`]).
fn spells_a_descriptor_directory(path: &Path) -> bool {
    let task = |name: &str| {
        name == "self"
            || name == "thread-self"
            || (!name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
    };
    let names = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    path.is_absolute()
        && !path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
        && match names.as_slice() {
            ["dev", "fd"] => true,
            ["proc", pid, "fd"] => task(pid),
            ["proc", pid, "task", tid, "fd"] => {
                task(pid) && !tid.is_empty() && tid.bytes().all(|byte| byte.is_ascii_digit())
            }
            _ => false,
        }
}

/// The leading descriptor link of an absolute path (`/proc/self/fd/D`,
/// `/dev/fd/D`, `/proc/thread-self/fd/D`, `/proc/<pid>/task/<tid>/fd/D`),
/// spelled canonically, and the index in `path` just past its last
/// component, whatever follows. Empty and `.` components before the
/// descriptor's number name nothing, as in Linux's walk, and are skipped
/// (`//proc/self/fd/D`, `/proc//self/fd/D`, `/proc/./self/fd/D`). The link's
/// components are ASCII. Any other spelling (a `..` before the number, a
/// symbolic link of the guest's to `/proc/self/fd/D`) is not recognized
/// (named residual).
fn descriptor_link_prefix(path: &[u8]) -> Option<(PathBuf, usize)> {
    if !path.starts_with(b"/") {
        return None;
    }
    let mut names: Vec<&str> = Vec::new();
    let mut at = 0;
    loop {
        while path.get(at) == Some(&b'/') {
            at += 1;
        }
        let start = at;
        while at < path.len() && path[at] != b'/' {
            at += 1;
        }
        if start == at {
            return None;
        }
        let name = &path[start..at];
        if name == b"." {
            continue;
        }
        if !name.is_ascii() {
            return None;
        }
        names.push(std::str::from_utf8(name).ok()?);
        match names.as_slice() {
            ["dev", "fd", _] | ["proc", _, "fd", _] | ["proc", _, "task", _, "fd", _] => break,
            ["dev"]
            | ["dev", "fd"]
            | ["proc"]
            | ["proc", _]
            | ["proc", _, "fd"]
            | ["proc", _, "task"]
            | ["proc", _, "task", _]
            | ["proc", _, "task", _, "fd"] => {}
            _ => return None,
        }
    }
    Some((PathBuf::from(format!("/{}", names.join("/"))), at))
}

/// The leading descriptor link and the rest of an absolute path that leads
/// through one ([`descriptor_link_prefix`]), if the path goes on past the
/// link: a rest, or a final `/`, which follows the link (an empty rest then,
/// the directory itself). The rest is kept byte for byte, whatever it holds.
fn through_descriptor_link(path: &[u8]) -> Option<(PathBuf, Vec<u8>)> {
    let (link, end) = descriptor_link_prefix(path)?;
    let rest = &path[end..];
    if rest.is_empty() {
        return None;
    }
    let rest = &rest[rest.iter().take_while(|byte| **byte == b'/').count()..];
    Some((link, rest.to_vec()))
}

/// How many symbolic links Linux follows to reach the directory of the
/// descriptor link `link` ([`through_descriptor_link`]) before the link
/// itself: `/dev/fd` (a link to `/proc/self/fd`) and then `self`, 2;
/// `/proc/self` or `/proc/thread-self`, 1; a numbered task, 0. Each of these
/// counts against the lookup's `MAXSYMLINKS`, as the descriptor link does
/// (`pick_link`'s `total_link_count`).
fn selector_links(link: &Path) -> usize {
    let mut names = link.components().filter_map(|component| match component {
        std::path::Component::Normal(name) => Some(name),
        _ => None,
    });
    match (names.next(), names.next()) {
        (Some(first), _) if first == "dev" => 2,
        (_, Some(second)) if second == "self" || second == "thread-self" => 1,
        _ => 0,
    }
}

/// procfs's largest pathname, `PATH_MAX`, with its NUL.
const PATH_LIMIT: usize = libc::PATH_MAX as usize;

/// How many descriptor links to placeholder directories a respelled path may
/// lead through, as Linux's lookup follows at most `MAXSYMLINKS` links
/// (<https://github.com/torvalds/linux/blob/v6.17/include/linux/namei.h>).
const LINK_LIMIT: usize = 40;

/// One path operand of a call, respelled as Linux resolves it below a
/// placeholder directory ([`Detcore::respelled_below_descriptor_directory`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RespelledOperand {
    /// The index of the call's directory descriptor argument, which the
    /// respelled call gives as `AT_FDCWD`, for a path respelled absolutely;
    /// `None` for one looked up from where the guest's was (the same
    /// descriptor, or the working directory).
    pub(crate) dirfd: Option<usize>,
    /// The index of the path argument.
    pub(crate) path: usize,
    /// The respelled path, without its NUL.
    pub(crate) bytes: Vec<u8>,
}

/// The argument of a call that names the descriptor it operates on, for the
/// calls Detcore answers through a placeholder descriptor
/// ([`Detcore::answer_placeholder_call`]): `Some(index)` for a call that
/// operates on its descriptor operand `index`, the path calls among them that
/// name it with an empty path and `AT_EMPTY_PATH` (or, for `utimensat`, no
/// path at all). Every other call that names a descriptor reaches it as the
/// kernel's descriptor table does (`close`, the `dup`s, `fcntl`'s
/// table commands, `poll` and `epoll`, which see a directory with no `poll`
/// method alike on every file system), or looks a name up below it
/// ([`relative_operands`]).
fn placeholder_operand<M: MemoryAccess>(
    memory: &M,
    sysno: Sysno,
    args: &syscalls::SyscallArgs,
) -> Option<usize> {
    let names_itself = |path: usize, flags: usize| {
        args_flag(args, flags) & libc::AT_EMPTY_PATH != 0
            && PathPtr::from_ptr(argument(args, path) as *const libc::c_char).is_some_and(
                |pointer| {
                    pointer
                        .read(memory)
                        .is_ok_and(|path: PathBuf| path.as_os_str().is_empty())
                },
            )
    };
    match sysno {
        Sysno::getdents
        | Sysno::getdents64
        | Sysno::lseek
        | Sysno::fchdir
        | Sysno::fcntl
        | Sysno::read
        | Sysno::readv
        | Sysno::pread64
        | Sysno::preadv
        | Sysno::preadv2
        | Sysno::write
        | Sysno::writev
        | Sysno::pwrite64
        | Sysno::pwritev
        | Sysno::pwritev2
        | Sysno::ioctl
        | Sysno::fsync
        | Sysno::fdatasync
        | Sysno::syncfs
        | Sysno::flock
        | Sysno::fchmod
        | Sysno::fchown
        | Sysno::fgetxattr
        | Sysno::fsetxattr
        | Sysno::flistxattr
        | Sysno::fremovexattr
        | Sysno::ftruncate
        | Sysno::fallocate
        | Sysno::fadvise64
        | Sysno::readahead
        | Sysno::sync_file_range
        | Sysno::fstatfs => Some(0),
        // A file-backed mapping only: an anonymous one ignores its
        // descriptor argument (`ksys_mmap_pgoff`).
        Sysno::mmap if args.arg3 as i32 & libc::MAP_ANONYMOUS == 0 => Some(4),
        // `futimens`: no path at all names the descriptor itself.
        Sysno::utimensat if args.arg1 == 0 => Some(0),
        Sysno::utimensat if names_itself(1, 3) => Some(0),
        Sysno::fchmodat2 if names_itself(1, 3) => Some(0),
        Sysno::fchownat if names_itself(1, 4) => Some(0),
        Sysno::faccessat2 if names_itself(1, 3) => Some(0),
        // `name_to_handle_at` is not here: Detcore answers every one with
        // `EOPNOTSUPP` (`Detcore::handle_syscall_event`), Linux's answer for
        // procfs, which exports no handles.
        _ => None,
    }
}

/// The flags argument `index` of a call, as an `int`.
fn args_flag(args: &syscalls::SyscallArgs, index: usize) -> i32 {
    argument(args, index) as i32
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Whether Detcore serves placeholder directories in this run: where the
    /// object it opens for one exists ([`PlaceholderObject`]: the container's
    /// own procfs, `DetConfig::container_procfs`, where the guest's procfs is
    /// the kernel's; the placeholder directory hermit-cli mounted,
    /// `DetConfig::directory_placeholder`, where it is not, which neither
    /// `--no-namespace`, a library caller nor a user mount over it leaves),
    /// where Detcore keeps the guest's descriptor table (not the in-guest
    /// backends, above), and renders procfs files (not under record and
    /// replay). A descriptor that is one stays one, and is answered as one,
    /// whatever its holder does later (a `chroot`, a nested PID namespace):
    /// no placeholder is ever handed to the backend unanswered.
    fn serves_placeholders<G: Guest<Self>>(guest: &G) -> bool {
        let config = guest.config();
        let object_exists = match Self::placeholder_object(guest) {
            PlaceholderObject::ProcfsRoot => config.container_procfs,
            PlaceholderObject::Placeholder => config.directory_placeholder.is_some(),
        };
        object_exists
            && !config.backend.tool_shares_guest_descriptor_table
            && !config.recordreplay_modes
    }

    /// The caller's pid and tid as the guest numbers them, where Detcore
    /// serves a new placeholder directory to the caller now
    /// ([`Self::serves_placeholders`]), and classifies the caller's spellings
    /// of the directories and of its descriptor links: where it knows which
    /// `/proc` its absolute paths reach (not after a guest `chroot`) and the
    /// caller's numbers (not in a nested PID namespace, nor before its
    /// process is known). Read from the thread's state alone, so that each
    /// injection can ask ([`Self::respelled_below_descriptor_directory`]).
    fn descriptor_directory_caller<G: Guest<Self>>(guest: &G) -> Option<(i32, i32)> {
        let state = guest.thread_state();
        if !Self::serves_placeholders(guest)
            || state.changed_root.load(Ordering::SeqCst)
            || state.nested_pid_namespace
        {
            return None;
        }
        Some((state.detpid?.as_raw(), state.dettid.as_raw()))
    }

    /// Whether Detcore serves a new placeholder directory to the caller now
    /// ([`Self::descriptor_directory_caller`]).
    fn serves_descriptor_directories<G: Guest<Self>>(&self, guest: &G) -> bool {
        Self::descriptor_directory_caller(guest).is_some()
    }

    /// The caller's pid and tid as Detcore numbers its tasks, which a
    /// placeholder's binding names its task by, in every state of the caller
    /// (after a `chroot`, in a nested PID namespace): for telling whether a
    /// descriptor directory's table is the caller's own. `None` before the
    /// caller's process is known.
    fn caller_numbers<G: Guest<Self>>(guest: &G) -> Option<(i32, i32)> {
        let state = guest.thread_state();
        Some((state.detpid?.as_raw(), state.dettid.as_raw()))
    }

    /// What a placeholder descriptor is open on in this run
    /// ([`PlaceholderObject`]), by the one capability that says whether the
    /// guest's procfs is the kernel's.
    fn placeholder_object<G: Guest<Self>>(guest: &G) -> PlaceholderObject {
        if procfs_inode::tool_can_name_guest_descriptors(&guest.config().backend) {
            PlaceholderObject::ProcfsRoot
        } else {
            PlaceholderObject::Placeholder
        }
    }

    /// The placeholder state of descriptor `fd`, if it is one.
    pub(crate) fn placeholder_of<G: Guest<Self>>(guest: &G, fd: RawFd) -> Option<PlaceholderState> {
        if fd < 0 {
            return None;
        }
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.placeholder())
            .ok()
            .flatten()
    }

    /// The placeholder directory that the caller's descriptor link `path`
    /// (the whole of it: `/proc/self/fd/N`, `/dev/fd/N`, ...) reaches, if it
    /// is one. A process's link (`/proc/self/fd/N`) reads the thread group
    /// leader's table, so a thread that holds a table of its own (cloned
    /// without `CLONE_FILES`) does not answer it from its own; a thread's
    /// link (`/proc/thread-self/fd/N`) reads the caller's.
    fn own_link_directory<G: Guest<Self>>(guest: &G, path: &Path) -> Option<PlaceholderDirectory> {
        let own = Self::descriptor_directory_caller(guest)?;
        Self::own_link_directory_of(guest, path, own)
    }

    /// [`Self::own_link_directory`] for a caller whose pid and tid are `own`,
    /// for a link spelled within the procfs a placeholder's object is
    /// (`/proc/<pid>/fd/N` below procfs's root, `self` the caller's process
    /// there), which is the caller's in every state of the caller.
    fn own_link_directory_of<G: Guest<Self>>(
        guest: &G,
        path: &Path,
        own: (i32, i32),
    ) -> Option<PlaceholderDirectory> {
        let link = crate::syscalls::namespace::own_descriptor_link(path, Some(own))?;
        if link.table == crate::syscalls::namespace::DescriptorTable::Process {
            let state = guest.thread_state();
            if state.dettid.as_raw() != own.0 && !state.holds_process_files() {
                return None;
            }
        }
        Self::placeholder_of(guest, link.fd).map(|state| state.directory)
    }

    /// The placeholder directory that an open of `observed` (the guest's
    /// absolute spelling) names, whose procfs spelling is `spelling`
    /// (`Detcore::procfs_spelling_at`), if Detcore serves it: one of the
    /// caller's own descriptor directories ([`fd_directory::classify`]),
    /// `/proc` itself by any spelling that reaches it ([`names_procfs_root`]),
    /// `/dev/fd` where it is the symbolic link to `/proc/self/fd` it is on
    /// Linux, or a descriptor link of the caller's to a placeholder. The last
    /// two are symbolic links: an open with `O_NOFOLLOW` that does not follow
    /// them anyway (a trailing `/`) opens the link itself (`O_PATH`) or fails
    /// with `ELOOP`, which the kernel answers.
    pub(super) async fn descriptor_directory_opened<G: Guest<Self>>(
        &self,
        guest: &mut G,
        observed: &Path,
        spelling: &Path,
        flags: OFlag,
    ) -> Option<Result<OpenedDirectory, Errno>> {
        if !self.serves_descriptor_directories(guest) {
            return None;
        }
        let own = Self::descriptor_directory_caller(guest)?;
        // A spelling through a descriptor link to a placeholder directory
        // (`/proc/self/fd/D/.`, `/proc/self/fd/D/N`) names what Linux
        // resolves it to, with the links it followed on the way counted.
        let (decided, links) = match Self::decided_path(guest, observed) {
            Ok(decided) => decided,
            Err(errno) => return Some(Err(errno)),
        };
        let (observed, spelling) = if decided.as_path() == observed {
            (observed, spelling)
        } else {
            (decided.as_path(), decided.as_path())
        };
        if let Some(table) = fd_directory::classify(spelling, own) {
            return Some(Ok(OpenedDirectory::Own(table)));
        }
        if let Some(tasks) = names_procfs_root(spelling) {
            // A task the spelling names on the way that does not exist ends
            // Linux's walk there.
            return Some(if Self::named_tasks_exist(guest, &tasks).await {
                Ok(OpenedDirectory::Root)
            } else {
                Err(Errno::ENOENT)
            });
        }
        if super::opens_the_link_itself(observed, flags) {
            return None;
        }
        if let Some(directory) = Self::own_link_directory(guest, observed) {
            // The open follows this last link too, within the one budget.
            if links + selector_links(observed) + 1 > LINK_LIMIT {
                return Some(Err(Errno::ELOOP));
            }
            return Some(Ok(OpenedDirectory::Reopened(directory)));
        }
        let dev_fd = observed
            .components()
            .filter(|component| *component != std::path::Component::CurDir)
            .eq(Path::new("/dev/fd").components());
        if dev_fd
            && self
                .injected_link_target(guest, b"/dev/fd")
                .await
                .as_deref()
                == Some(Path::new("/proc/self/fd"))
        {
            return Some(Ok(OpenedDirectory::Own(DescriptorDirectoryTable::Process)));
        }
        None
    }

    /// The device a `/proc` directory Detcore serves reports, the one the
    /// guest's mount table shows at `/proc`: Detcore's own procfs device
    /// (`procfs_inode::VIRTUAL_PROC_DEVICE`) where it virtualizes metadata,
    /// and otherwise the guest's procfs device itself, which the mount table
    /// then shows as it is (`rendered_procfs::proc_device`).
    fn reported_procfs_device<G: Guest<Self>>(guest: &G) -> u64 {
        if guest.config().virtualize_metadata {
            procfs_inode::VIRTUAL_PROC_DEVICE
        } else {
            super::rendered_procfs::proc_device().unwrap_or(procfs_inode::VIRTUAL_PROC_DEVICE)
        }
    }

    /// The device that keys the inode numbers of the `/proc` directories
    /// Detcore serves, and their entries', with their paths: the device of
    /// the guest's procfs as the guest's own stat of it reports it, never the
    /// placeholder's, whose device the host chose. Where the backend's `/proc`
    /// beneath is the guest's kernel procfs
    /// (`procfs_inode::tool_can_name_guest_descriptors`: ptrace), the tool's
    /// own view of it (`rendered_procfs::proc_device`), which is the
    /// guest's, as the tool runs in the guest's mount namespace; elsewhere
    /// (KVM) the device that backend reports for its procfs files
    /// (`procfs_inode::KVM_SYNTHETIC_PROC_DEVICE`). Either way the device the
    /// descriptor on `/proc` itself reported before slice P1, so the numbers
    /// are the ones they were.
    pub(crate) fn procfs_key_device<G: Guest<Self>>(guest: &G) -> u64 {
        if procfs_inode::tool_can_name_guest_descriptors(&guest.config().backend) {
            super::rendered_procfs::proc_device().unwrap_or(0)
        } else {
            procfs_inode::KVM_SYNTHETIC_PROC_DEVICE
        }
    }

    /// The target of the symbolic link at the absolute guest path `link`, by
    /// one injected `readlinkat`, or `None`.
    async fn injected_link_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        link: &[u8],
    ) -> Option<PathBuf> {
        const LINK_CAPACITY: usize = 32;
        const TARGET_CAPACITY: usize = 64;
        let mut stack = guest.stack().await;
        let link = stack.push(super::rendered_procfs::c_string::<LINK_CAPACITY>(link));
        let target = stack.reserve::<[u8; TARGET_CAPACITY]>();
        let stack_guard = stack.commit().ok()?;
        let length = guest
            .inject(
                syscalls::Readlinkat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(link.as_raw() as *const libc::c_char))
                    .with_buf(Some(target.cast::<libc::c_char>()))
                    .with_buf_len(TARGET_CAPACITY),
            )
            .await;
        let bytes = match length.map(|length| length as usize) {
            Ok(length) if length < TARGET_CAPACITY => {
                let mut bytes = vec![0; length];
                guest
                    .memory()
                    .read_exact(target.cast::<u8>(), &mut bytes)
                    .ok()
                    .map(|()| bytes)
            }
            _ => None,
        };
        drop(stack_guard);
        Some(PathBuf::from(String::from_utf8(bytes?).ok()?))
    }

    /// What Detcore's process table says now of the task `binding` names,
    /// and whether that is still the task the directory was bound to (it
    /// exists and no other task has taken its numbers since).
    async fn bound_task<G: Guest<Self>>(
        guest: &mut G,
        binding: &DescriptorDirectoryBinding,
    ) -> (crate::procfs::listing::ProcfsTaskAnswer, bool) {
        let thread = (binding.table == DescriptorDirectoryTable::Thread).then_some(binding.tid);
        let answer = crate::tool_global::procfs_task(
            guest,
            crate::procfs::BoundTask {
                process: binding.pid,
                thread,
            },
        )
        .await;
        let current = answer.exists
            && answer.process_incarnation == binding.process_incarnation
            && answer.thread_incarnation == binding.thread_incarnation;
        (answer, current)
    }

    /// Whether the placeholder directory `directory` still names a directory:
    /// `/proc` always does; a descriptor directory while its task does
    /// (`get_proc_task`, [`Self::bound_task`]).
    async fn placeholder_current<G: Guest<Self>>(
        guest: &mut G,
        directory: &PlaceholderDirectory,
    ) -> bool {
        match directory {
            PlaceholderDirectory::Root => true,
            PlaceholderDirectory::Descriptors(binding) => Self::bound_task(guest, binding).await.1,
        }
    }

    /// Whether every task in `tasks` exists now, as Linux's walk finds it
    /// ([`NamedTask`]): any task by its number at the top of `/proc`, and a
    /// thread of that process below its `task` (`Scheduler::procfs_task`),
    /// the caller's own process for `self`.
    pub(super) async fn named_tasks_exist<G: Guest<Self>>(
        guest: &mut G,
        tasks: &[NamedTask],
    ) -> bool {
        for task in tasks {
            let process = match task.process {
                Some(process) => process,
                None => match guest.thread_state().detpid {
                    Some(pid) => pid.as_raw(),
                    None => return false,
                },
            };
            if process <= 0 || task.thread.is_some_and(|thread| thread <= 0) {
                return false;
            }
            let answer = crate::tool_global::procfs_task(
                guest,
                crate::procfs::BoundTask {
                    process,
                    thread: task.thread,
                },
            )
            .await;
            if !answer.exists {
                return false;
            }
        }
        true
    }

    /// The identity `(dev, ino)` of the placeholder directory hermit-cli
    /// mounted, as the guest's backend reports it, learnt once in this tool
    /// process: from a placeholder Detcore opened on it (where it is the
    /// object, [`Self::open_placeholder`]), or else by one injected `stat` of
    /// its path, which no guest name spells (Detcore hides `/proc/fs`).
    /// `None` without a placeholder, or where that `stat` fails or does not
    /// find a directory.
    pub(crate) async fn hidden_placeholder_identity<G: Guest<Self>>(
        guest: &mut G,
    ) -> Option<(u64, u64)> {
        if let Some(identity) = *HIDDEN_IDENTITY.lock().unwrap() {
            return Some(identity);
        }
        let placeholder = guest.config().directory_placeholder.clone()?;
        let bytes = placeholder.as_os_str().as_bytes().to_vec();
        let stat = Self::injected_path_stat(guest, &bytes).await.ok()?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return None;
        }
        let identity = (stat.st_dev, stat.st_ino);
        *HIDDEN_IDENTITY.lock().unwrap() = Some(identity);
        Some(identity)
    }

    /// `stat` of the absolute path `path`, following a final link, by one
    /// injected `newfstatat` from `AT_FDCWD`, the path and the buffer staged
    /// together on Reverie's guest stack.
    async fn injected_path_stat<G: Guest<Self>>(
        guest: &mut G,
        path: &[u8],
    ) -> Result<libc::stat, Errno> {
        const PATH_CAPACITY: usize = 64;
        if path.len() >= PATH_CAPACITY {
            return Err(Errno::ENAMETOOLONG);
        }
        let mut stack = guest.stack().await;
        let staged = stack.push(super::rendered_procfs::c_string::<PATH_CAPACITY>(path));
        let buffer = stack.reserve::<libc::stat>();
        let stack_guard = stack.commit().map_err(|_| Errno::EFAULT)?;
        let looked_up = guest
            .inject(
                syscalls::Newfstatat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(staged.as_raw() as *const libc::c_char))
                    .with_stat(Some(reverie::syscalls::StatPtr(buffer)))
                    .with_flags(nix::fcntl::AtFlags::empty()),
            )
            .await;
        let stat = looked_up.and_then(|_| guest.memory().read_value(buffer));
        drop(stack_guard);
        stat
    }

    /// Whether a guest call that named a path reached the placeholder
    /// directory, as the result `(mode, dev, ino)` the backend gave for it
    /// shows: a directory of the placeholder's identity
    /// ([`Self::hidden_placeholder_identity`]). Detcore makes every
    /// descriptor it opens on the placeholder itself, without the guest's
    /// call, so a guest open or stat that reaches it did so by a spelling
    /// Detcore's `--proc` level table could not resolve
    /// (`/proc/self/root/proc/fs/nfsd`, `/proc/1/root/...`,
    /// `/dev/fd/<a descriptor on />/proc/fs/nfsd`), and fails with `ENOENT`
    /// as `/proc/fs` does. Wherever a placeholder is mounted, on every
    /// backend.
    pub(crate) async fn hidden_placeholder_reached<G: Guest<Self>>(
        guest: &mut G,
        mode: u32,
        dev: u64,
        ino: u64,
    ) -> bool {
        if guest.config().directory_placeholder.is_none() || mode & libc::S_IFMT != libc::S_IFDIR {
            return false;
        }
        Self::hidden_placeholder_identity(guest).await == Some((dev, ino))
    }

    /// [`Self::hidden_placeholder_reached`], where it is decided without
    /// learning the placeholder's identity: `false` without a placeholder or
    /// for what is no directory, the comparison once the identity is learnt,
    /// and `None` until it is, for the caller to ask
    /// [`Self::hidden_placeholder_reached`] in a boxed future, so that the
    /// stat and open handlers that ask stay small.
    pub(crate) fn hidden_placeholder_known<G: Guest<Self>>(
        guest: &G,
        mode: u32,
        dev: u64,
        ino: u64,
    ) -> Option<bool> {
        if guest.config().directory_placeholder.is_none() || mode & libc::S_IFMT != libc::S_IFDIR {
            return Some(false);
        }
        HIDDEN_IDENTITY
            .lock()
            .unwrap()
            .map(|identity| identity == (dev, ino))
    }

    /// The bytes of a stat call's result buffer at `buffer`, `size` long,
    /// before the call writes it, where a placeholder directory exists (KVM:
    /// `DetConfig::directory_placeholder`), so that a call that reaches the
    /// directory can leave the buffer as Linux leaves it on its `ENOENT`,
    /// untouched ([`Self::hidden_placeholder_stat`]). `None` elsewhere, and
    /// for a buffer that cannot be read, which the call cannot write either.
    pub(crate) fn stat_buffer_before<G: Guest<Self>>(
        guest: &G,
        buffer: Option<usize>,
        size: usize,
    ) -> Option<Vec<u8>> {
        guest.config().directory_placeholder.as_ref()?;
        let start = reverie::syscalls::Addr::<u8>::from_raw(buffer?)?;
        let mut bytes = vec![0u8; size];
        guest.memory().read_exact(start, &mut bytes).ok()?;
        Some(bytes)
    }

    /// A stat call's answer where its result, which the backend wrote to the
    /// guest's buffer at `buffer`, is the placeholder directory's
    /// ([`Self::hidden_placeholder_reached`]): `ENOENT`, as for `/proc/fs`,
    /// with the buffer as it was before the call (`before`,
    /// [`Self::stat_buffer_before`]), as Linux's `ENOENT` leaves it, so that
    /// nothing of the tmpfs (its device, inode or times) reaches the guest.
    pub(crate) fn hidden_placeholder_stat<G: Guest<Self>>(
        guest: &mut G,
        buffer: usize,
        before: Option<Vec<u8>>,
    ) -> Result<i64, Error> {
        if let (Some(start), Some(before)) = (AddrMut::<u8>::from_raw(buffer), before) {
            let _ = guest.memory().write_exact(start, &before);
        }
        Err(Errno::ENOENT.into())
    }

    /// [`Self::hidden_placeholder_reached`], decided at once where
    /// [`Self::hidden_placeholder_known`] decides it, and otherwise in a boxed
    /// future.
    pub(crate) async fn reaches_hidden_placeholder<G: Guest<Self>>(
        guest: &mut G,
        mode: u32,
        dev: u64,
        ino: u64,
    ) -> bool {
        match Self::hidden_placeholder_known(guest, mode, dev, ino) {
            Some(known) => known,
            None => Box::pin(Self::hidden_placeholder_reached(guest, mode, dev, ino)).await,
        }
    }

    /// Whether an open of the absolute guest path `observed`, whose procfs
    /// spelling is `spelling`, must be refused with `EACCES` before it reaches
    /// the backend: `/proc` itself, or a descriptor directory, opened where
    /// the guest's procfs is not the kernel's (KVM) and Detcore serves no new
    /// placeholder to the caller: no placeholder in the run (`--no-namespace`,
    /// a library caller, a user mount over the placeholder), or a caller in a
    /// nested PID namespace, whose numbers Detcore does not know: the
    /// owner-approved answer for these configurations. The backend's executor
    /// serves no procfs directory either (reverie-linuxemu refuses `/proc`
    /// with `EACCES` since package K slice P2); Detcore answers first, so the
    /// answer is Detcore's on every backend that needs it.
    pub(crate) async fn refuses_backend_procfs_directory<G: Guest<Self>>(
        guest: &mut G,
        observed: &Path,
        spelling: &Path,
    ) -> bool {
        let config = guest.config();
        if Self::placeholder_object(guest) != PlaceholderObject::Placeholder
            || config.backend.tool_shares_guest_descriptor_table
            || config.recordreplay_modes
            || Self::descriptor_directory_caller(guest).is_some()
        {
            return false;
        }
        if spells_a_descriptor_directory(observed) || spells_a_descriptor_directory(spelling) {
            return true;
        }
        match names_procfs_root(spelling) {
            Some(tasks) => Self::named_tasks_exist(guest, &tasks).await,
            None => false,
        }
    }

    /// Open the placeholder directory `opened` names, with the guest's open
    /// `flags`: refuse what Linux refuses for a directory, then inject the
    /// open of the object that stands for it ([`PlaceholderObject`]), record
    /// the descriptor and which directory it is, register it for its tag, and
    /// name its description to the lock ledger, which follows every table
    /// that references it from now on (`procfs::locks::LockLedger`, for its
    /// `flock` locks).
    pub(super) async fn open_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        flags: OFlag,
        opened: OpenedDirectory,
    ) -> Result<i64, Error> {
        // As Linux refuses an open of a directory: invalid combinations
        // first (`build_open_flags`), then an existing name with
        // `O_CREAT|O_EXCL` (EEXIST), an `O_TMPFILE` below a directory that has
        // no `tmpfile` (EOPNOTSUPP), `O_CREAT` (`do_open`'s EISDIR), a write
        // or `O_TRUNC` (`may_open`'s EISDIR), `O_NOATIME` for a caller that
        // neither owns the directory nor may act as its owner (the end of
        // `may_open`, EPERM), and last `O_DIRECT`, which procfs cannot do
        // (`do_dentry_open`'s EINVAL). An `O_PATH` open makes none of these
        // checks. Measured on Linux 7.1.
        if let Some(errno) = super::rendered_procfs::open_flags_refused(flags) {
            return Err(errno.into());
        }
        let path_only = flags.contains(OFlag::O_PATH);
        let (pid, tid) = Self::descriptor_directory_caller(guest).ok_or(Errno::ENOENT)?;
        let directory = match opened {
            OpenedDirectory::Own(table) => {
                let thread = (table == DescriptorDirectoryTable::Thread).then_some(tid);
                let answer = crate::tool_global::procfs_task(
                    guest,
                    crate::procfs::BoundTask {
                        process: pid,
                        thread,
                    },
                )
                .await;
                PlaceholderDirectory::Descriptors(DescriptorDirectoryBinding {
                    table,
                    pid,
                    tid,
                    process_incarnation: answer.process_incarnation,
                    thread_incarnation: answer.thread_incarnation,
                    device: Self::procfs_key_device(guest),
                    files: (table == DescriptorDirectoryTable::Thread)
                        .then(|| guest.thread_state().files_id()),
                })
            }
            OpenedDirectory::Root => PlaceholderDirectory::Root,
            OpenedDirectory::Reopened(directory) => directory,
        };
        if !path_only {
            if flags.contains(OFlag::O_CREAT | OFlag::O_EXCL) {
                return Err(Errno::EEXIST.into());
            }
            if flags.contains(OFlag::O_TMPFILE) {
                return Err(Errno::EOPNOTSUPP.into());
            }
            if flags.contains(OFlag::O_CREAT)
                || flags & OFlag::O_ACCMODE != OFlag::O_RDONLY
                || flags.contains(OFlag::O_TRUNC)
            {
                return Err(Errno::EISDIR.into());
            }
            if flags.contains(OFlag::O_NOATIME) && !placeholder_owner_or_capable(&directory)? {
                return Err(Errno::EPERM.into());
            }
            if flags.contains(OFlag::O_DIRECT) {
                return Err(Errno::EINVAL.into());
            }
        }
        let object = Self::placeholder_object(guest);
        let object_path = match object {
            PlaceholderObject::ProcfsRoot => PathBuf::from("/proc"),
            PlaceholderObject::Placeholder => guest
                .config()
                .directory_placeholder
                .clone()
                .ok_or(Errno::ENOENT)?,
        };
        // A path-only open is an `O_PATH` descriptor on the object as well,
        // so that every call Linux refuses on one is refused beneath too, as
        // on Linux, those Detcore does not answer itself included (`epoll_ctl`,
        // `poll`, the socket calls). Such a descriptor takes no tag, which
        // `lseek` refuses on it, and so is not sent over `SCM_RIGHTS`
        // ([`Self::placeholder_copies_for_send`]).
        let cloexec = flags & OFlag::O_CLOEXEC;
        let object_flags = if path_only {
            OFlag::O_PATH | OFlag::O_DIRECTORY | cloexec
        } else {
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | cloexec
        };
        let fd = Self::inject_openat(
            guest,
            libc::AT_FDCWD,
            object_path.as_os_str().as_bytes(),
            object_flags,
        )
        .await?;
        // The status flags Linux keeps: an `O_PATH` open keeps only
        // `O_PATH_FLAGS` (`build_open_how`); any other gets `O_LARGEFILE`
        // (`force_o_largefile`) and loses the open-time-only flags
        // (`do_dentry_open`).
        let status = if path_only {
            flags & (OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        } else {
            // The kernel's `O_LARGEFILE`, which the C library's value (0 on
            // x86_64) does not name.
            (flags & !(OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOCTTY | OFlag::O_TRUNC))
                | OFlag::from_bits_retain(0o100000)
        };
        let stat = match self
            .add_fd_with_stat(guest, fd, status, FdType::Regular)
            .await
        {
            Ok(Some(stat)) => Ok(stat),
            Ok(None) => self.inject_fstat(guest, fd).await,
            Err(errno) => Err(errno),
        };
        let stat = match stat {
            Ok(stat) => stat,
            Err(errno) => {
                guest.thread_state().remove_fd(fd);
                let _ = guest.inject(syscalls::Close::new().with_fd(fd)).await;
                return Err(errno.into());
            }
        };
        PLACEHOLDER_IDENTITIES
            .lock()
            .unwrap()
            .insert((stat.st_dev, stat.st_ino));
        if object == PlaceholderObject::Placeholder {
            HIDDEN_IDENTITY
                .lock()
                .unwrap()
                .get_or_insert((stat.st_dev, stat.st_ino));
        }
        OPENED_ANY.store(true, Ordering::SeqCst);
        // Registered with the open file description it is, which a receiver
        // of the descriptor takes as its own ([`Self::recognize_received_placeholder`]).
        let open_file = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.open_file_id())?;
        let tag = if path_only {
            None
        } else {
            crate::tool_global::register_placeholder(guest, directory, open_file).await
        };
        // The kernel position tags the descriptor for whoever meets it without
        // its record. A backend that cannot seek it leaves it untagged.
        let tag = match tag {
            Some(tag) => guest
                .inject(
                    syscalls::Lseek::new()
                        .with_fd(fd)
                        .with_offset(tag as i64)
                        .with_whence(syscalls::Whence::SEEK_SET),
                )
                .await
                .ok()
                .filter(|position| *position as u64 == tag)
                .map(|_| tag),
            None => None,
        };
        let canonical = directory.canonical();
        guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_path(canonical.clone());
            detfd.set_placeholder(PlaceholderState {
                directory,
                tag,
                position: 0,
                path_only,
            });
        })?;
        self.follow_placeholder_references(guest, fd).await;
        tracing::debug!(
            "[detcore] descriptor {fd}: {} served on {}",
            canonical.display(),
            object_path.display()
        );
        Ok(fd as i64)
    }

    /// Name descriptor `fd`'s description, a placeholder's, to the lock
    /// ledger with the slots of the caller's table that name it, and mark it
    /// followed, so that every later close, replacement, fork and exec of
    /// the tables that reference it reaches the ledger
    /// (`procfs::locks::LockLedger`): a `flock` on the directory is then
    /// released with the description's last reference, as Linux's `__fput`
    /// releases it, also where the first lock comes after a fork.
    async fn follow_placeholder_references<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) {
        let Ok(open_file) = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_lock_tracked();
            detfd.open_file_id()
        }) else {
            return;
        };
        let slots = guest.thread_state().slots_naming(open_file);
        crate::tool_global::note_description_locks(guest, open_file, Ok(Vec::new()), slots).await;
    }

    /// Make descriptor `fd`, which Detcore just recorded without knowing what
    /// it is (received over `SCM_RIGHTS`, `Detcore::record_received_descriptors`),
    /// a placeholder directory if it is one: its identity (`stat`) is the
    /// placeholder's, which no other object has, and its kernel position,
    /// read by one injected `lseek`, is a placeholder's tag, which says which
    /// directory it stands for. No guest descriptor of that identity is ever
    /// seeked by the guest ([`Self::seek_would_place_a_tag`]). Until a
    /// placeholder is opened in this tool process, whose global tool
    /// registers every tag, no received descriptor can be one, and nothing is
    /// injected to look.
    pub(crate) async fn recognize_received_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        stat: &libc::stat,
        status_flags: i32,
    ) {
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR
            || !OPENED_ANY.load(Ordering::SeqCst)
            || !Self::serves_placeholders(guest)
        {
            return;
        }
        if !PLACEHOLDER_IDENTITIES
            .lock()
            .unwrap()
            .contains(&(stat.st_dev, stat.st_ino))
        {
            return;
        }
        let position = guest
            .inject(
                syscalls::Lseek::new()
                    .with_fd(fd)
                    .with_offset(0)
                    .with_whence(syscalls::Whence::SEEK_CUR),
            )
            .await;
        let Ok(position) = position.map(|position| position as u64) else {
            return;
        };
        if fd_directory::tag_index(position).is_none() {
            return;
        }
        let Some((directory, open_file)) = crate::tool_global::placeholder(guest, position).await
        else {
            return;
        };
        // The description is the sender's, as Linux shares it: the locks
        // Detcore follows by description are that one's, and this copy is no
        // longer in flight (`procfs::locks::LockLedger::placeholder_copies`).
        let _ = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.adopt_open_file_id(open_file);
            detfd.set_path(directory.canonical());
            detfd.set_placeholder(PlaceholderState {
                directory,
                tag: Some(position),
                position: 0,
                path_only: status_flags & libc::O_PATH != 0,
            })
        });
        let _ = crate::tool_global::placeholder_copies(
            guest,
            crate::procfs::locks::PlaceholderCopies::Received(open_file),
        )
        .await;
        self.follow_placeholder_references(guest, fd).await;
    }

    /// Before a send whose messages pass the descriptors `sent` over
    /// `SCM_RIGHTS`: the open file descriptions of the placeholders among
    /// them, one per copy, counted in flight
    /// (`procfs::locks::LockLedger::placeholder_copies`), so that none takes
    /// a placeholder `flock` until its copy is received
    /// ([`Self::recognize_received_placeholder`]) or the send fails (the
    /// caller then uncounts them). Refused by name instead (`Err`, the
    /// refusal): a placeholder whose description holds a placeholder `flock`,
    /// which Linux keeps while the message is queued, a reference the ledger
    /// does not follow; an untagged placeholder (an `O_PATH` one), which its
    /// receiver could not tell for one; and any placeholder `sendmmsg` sends,
    /// of whose messages Detcore does not know which the batch sent.
    pub(crate) async fn placeholder_copies_for_send<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        sent: &[RawFd],
    ) -> Result<Vec<crate::types::OpenFileId>, Result<i64, Error>> {
        if !OPENED_ANY.load(Ordering::SeqCst) || !Self::serves_placeholders(guest) {
            return Ok(Vec::new());
        }
        let mut copies = Vec::new();
        for fd in sent {
            let Some(state) = Self::placeholder_of(guest, *fd) else {
                continue;
            };
            let unsupported = if state.tag.is_none() {
                Some("carries no tag (an O_PATH descriptor) that its receiver could tell it by")
            } else if sysno == Sysno::sendmmsg {
                Some("is sent in a batch, of whose messages Detcore does not know which it sent")
            } else {
                None
            };
            if let Some(why) = unsupported {
                let reason = format!(
                    "descriptor {fd}, which stands for {}, a /proc directory Detcore serves, \
                     is passed over SCM_RIGHTS and {why}",
                    state.directory.canonical().display()
                );
                return Err(self
                    .refuse_placeholder_call(guest, sysno, Errno::EOPNOTSUPP, &reason)
                    .await);
            }
            match guest
                .thread_state()
                .with_detfd(*fd, |detfd| detfd.open_file_id())
            {
                Ok(open_file) => copies.push(open_file),
                Err(error) => return Err(Err(error.into())),
            }
        }
        if copies.is_empty() {
            return Ok(copies);
        }
        match crate::tool_global::placeholder_copies(
            guest,
            crate::procfs::locks::PlaceholderCopies::Sending(copies.clone()),
        )
        .await
        {
            Ok(()) => Ok(copies),
            Err(_) => {
                let reason = "a descriptor it passes over SCM_RIGHTS stands for a /proc directory \
                              Detcore serves and holds a flock on it, which Linux keeps while the \
                              message is queued, a reference Detcore does not follow";
                Err(self
                    .refuse_placeholder_call(guest, sysno, Errno::EOPNOTSUPP, reason)
                    .await)
            }
        }
    }

    /// Whether a send's `SCM_RIGHTS` descriptors can matter to the `/proc`
    /// directories Detcore serves: it serves them, and this tool process has
    /// opened or recognized a placeholder. Asked before anything else of a
    /// send, from Detcore's own state alone.
    pub(crate) fn may_send_placeholders<G: Guest<Self>>(guest: &G) -> bool {
        OPENED_ANY.load(Ordering::SeqCst) && Self::serves_placeholders(guest)
    }

    /// The placeholders among the descriptors `sent`, each with its open file
    /// description, in the order sent.
    fn placeholders_among<G: Guest<Self>>(
        guest: &G,
        sent: &[RawFd],
    ) -> Vec<(RawFd, crate::types::OpenFileId)> {
        sent.iter()
            .filter(|fd| Self::placeholder_of(guest, **fd).is_some())
            .filter_map(|fd| {
                guest
                    .thread_state()
                    .with_detfd(*fd, |detfd| (*fd, detfd.open_file_id()))
                    .ok()
            })
            .collect()
    }

    /// Before a `sendmsg` or `sendmmsg` on `socket` whose messages' headers
    /// are at the guest addresses `headers`, read here for the descriptors
    /// they pass over `SCM_RIGHTS` (`syscalls::io::sent_descriptors`; `unread`
    /// below where a header or control buffer cannot be read in full, so that
    /// the list may be incomplete), where Detcore serves placeholders
    /// ([`Self::may_send_placeholders`]): the open file
    /// descriptions of the placeholders among them, counted in flight
    /// ([`Self::placeholder_copies_for_send`]), and what each attempt of the
    /// send checks again ([`SendInspection`]), or the refusal by name. Besides
    /// the refusals [`Self::placeholder_copies_for_send`] makes, a send is
    /// refused while this process holds a placeholder and its socket can
    /// pass descriptors (`AF_UNIX`, [`Self::socket_carries_descriptors`]),
    /// where Detcore cannot know which descriptors Linux reads:
    /// - its messages cannot be read in full (`unread`): a control buffer
    ///   longer than the 65536 bytes Detcore reads, which Linux takes up to
    ///   the socket's `net.core.optmem_max` (128 KiB in a new network
    ///   namespace, a host setting in the initial one), or memory Detcore
    ///   cannot read. Detcore refuses rather than reading as much as the
    ///   kernel takes: that limit is the host's to raise, and no length helps
    ///   with memory Detcore cannot read;
    /// - the send runs outside this thread's turn in a run that sequentializes
    ///   threads (`outside_turn`, a blocking call on a descriptor that blocks
    ///   in the kernel, `Detcore::nonblockable_call_runs_outside_turn`), where
    ///   another thread may rewrite its messages, or what their descriptor
    ///   numbers name, before Linux reads them, whatever this inspection read.
    ///   Without sequentialized threads no thread has a turn, and every check
    ///   Detcore makes may race the guest's other threads (that mode's named
    ///   residual).
    pub(crate) async fn inspect_placeholder_send<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        socket: RawFd,
        headers: &[usize],
        outside_turn: bool,
    ) -> Result<(Vec<crate::types::OpenFileId>, SendInspection), Result<i64, Error>> {
        let (sent, unread) = crate::syscalls::io::sent_descriptors(guest, headers);
        if (unread || outside_turn)
            && guest.thread_state().holds_placeholder()
            && Self::socket_carries_descriptors(guest, socket).await
        {
            let reason = if unread {
                format!(
                    "a message whose SCM_RIGHTS control data Detcore cannot read in full (more \
                     than {MAX_SEND_CONTROL_BYTES} bytes of it, which Linux takes up to the \
                     socket's net.core.optmem_max, or memory Detcore cannot read) may pass a \
                     descriptor that stands for a /proc directory Detcore serves, whose copies \
                     and lock Detcore follows"
                )
            } else {
                "a send on a socket that passes descriptors runs outside Detcore's turn, where \
                 another thread may change its SCM_RIGHTS messages, or what their descriptor \
                 numbers name, before Linux reads them, while this process holds a descriptor \
                 that stands for a /proc directory Detcore serves, whose copies and lock Detcore \
                 follows"
                    .to_string()
            };
            return Err(self
                .refuse_placeholder_call(guest, sysno, Errno::EOPNOTSUPP, &reason)
                .await);
        }
        let copies = self
            .placeholder_copies_for_send(guest, sysno, &sent)
            .await?;
        let inspection = SendInspection {
            sysno,
            socket,
            headers: headers.to_vec(),
            placeholders: Self::placeholders_among(guest, &sent),
        };
        Ok((copies, inspection))
    }

    /// Why the next attempt of a send Detcore inspected (`inspection`,
    /// [`Self::inspect_placeholder_send`]) must not be made, read right
    /// before it: its messages now pass other placeholders over `SCM_RIGHTS`
    /// than they did when Detcore counted their copies (another thread
    /// rewrote them, or replaced what a descriptor number names, while an
    /// earlier attempt waited for room), or they can no longer be read in full
    /// while this process holds a placeholder and the socket passes
    /// descriptors. `None` where the attempt passes the placeholders
    /// inspected, and nothing Detcore cannot see.
    pub(crate) async fn changed_placeholder_send<G: Guest<Self>>(
        guest: &mut G,
        inspection: &SendInspection,
    ) -> Option<String> {
        let (sent, unread) = crate::syscalls::io::sent_descriptors(guest, &inspection.headers);
        let placeholders = Self::placeholders_among(guest, &sent);
        if placeholders != inspection.placeholders {
            return Some(format!(
                "the descriptors its SCM_RIGHTS messages pass changed while the send waited for \
                 room, and with them the descriptors among them that stand for /proc \
                 directories Detcore serves (descriptors {:?} where Detcore inspected {:?}), \
                 whose copies and locks Detcore follows",
                placeholders.iter().map(|(fd, _)| *fd).collect::<Vec<_>>(),
                inspection
                    .placeholders
                    .iter()
                    .map(|(fd, _)| *fd)
                    .collect::<Vec<_>>(),
            ));
        }
        (unread
            && guest.thread_state().holds_placeholder()
            && Self::socket_carries_descriptors(guest, inspection.socket).await)
            .then(|| {
                "its SCM_RIGHTS messages could no longer be read in full when the send tried \
                 again, while this process holds a descriptor that stands for a /proc \
                 directory Detcore serves, whose copies and lock Detcore follows"
                    .to_string()
            })
    }

    /// After a send on `socket` that ran outside this thread's turn
    /// (`Detcore::nonblockable_call_runs_outside_turn`): the refusal by name
    /// where this process now holds a placeholder and the socket passes
    /// descriptors. [`Self::inspect_placeholder_send`] refused the send if it
    /// held one before, so another thread opened or received this one while
    /// the send ran, and may have put it in the messages before Linux read
    /// them. `None` otherwise.
    pub(crate) async fn placeholder_send_after<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        socket: RawFd,
    ) -> Option<Result<i64, Error>> {
        if !Self::may_send_placeholders(guest)
            || !guest.thread_state().holds_placeholder()
            || !Self::socket_carries_descriptors(guest, socket).await
        {
            return None;
        }
        let reason = "a descriptor that stands for a /proc directory Detcore serves joined this \
                      process's table while a send on a socket that passes descriptors ran \
                      outside Detcore's turn, where another thread may have put it in the send's \
                      SCM_RIGHTS messages before Linux read them";
        Some(
            self.refuse_placeholder_call(guest, sysno, Errno::EOPNOTSUPP, reason)
                .await,
        )
    }

    /// The placeholder directory descriptor `fd` stands for, where Detcore
    /// keeps no record of `fd` (one received over `SCM_RIGHTS` without
    /// sequentialized threads, which `Detcore::record_received_descriptors`
    /// leaves unrecorded): told as [`Self::recognize_received_placeholder`]
    /// tells it, by its identity, which only the placeholder has (one injected
    /// `fstat`), and its tag (one injected `lseek`). Without sequentialized
    /// threads another thread may replace the descriptor between the two, as
    /// it may between this and the guest's own call; the tag is checked
    /// against the registered placeholders, so a descriptor taken for one is
    /// a placeholder there. `None` for a recorded descriptor, any other
    /// object, and where Detcore serves no placeholder.
    pub(crate) async fn unrecorded_placeholder<G: Guest<Self>>(
        guest: &mut G,
        fd: RawFd,
    ) -> Option<PlaceholderDirectory> {
        if fd < 0
            || !OPENED_ANY.load(Ordering::SeqCst)
            || !Self::serves_placeholders(guest)
            || guest.thread_state().with_detfd(fd, |_| ()).is_ok()
        {
            return None;
        }
        let fstat = move |statptr: reverie::syscalls::StatPtr<'_>| {
            Syscall::Fstat(syscalls::Fstat::new().with_fd(fd).with_stat(Some(statptr)))
        };
        let stat = Self::stat_with_fallback(guest, fd, fstat, None)
            .await
            .ok()?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR
            || !PLACEHOLDER_IDENTITIES
                .lock()
                .unwrap()
                .contains(&(stat.st_dev, stat.st_ino))
        {
            return None;
        }
        let position = guest
            .inject(
                syscalls::Lseek::new()
                    .with_fd(fd)
                    .with_offset(0)
                    .with_whence(syscalls::Whence::SEEK_CUR),
            )
            .await
            .ok()? as u64;
        fd_directory::tag_index(position)?;
        crate::tool_global::placeholder(guest, position)
            .await
            .map(|(directory, _)| directory)
    }

    /// One injected `openat(dirfd, path, flags)` of a directory, with the path
    /// staged on Reverie's guest stack.
    async fn inject_openat<G: Guest<Self>>(
        guest: &mut G,
        dirfd: RawFd,
        path: &[u8],
        flags: OFlag,
    ) -> Result<RawFd, Errno> {
        const PATH_CAPACITY: usize = 64;
        if path.len() >= PATH_CAPACITY {
            return Err(Errno::ENAMETOOLONG);
        }
        let mut stack = guest.stack().await;
        let staged = stack.push(super::rendered_procfs::c_string::<PATH_CAPACITY>(path));
        let stack_guard = stack.commit().map_err(|_| Errno::EFAULT)?;
        let opened = guest
            .inject(
                syscalls::Openat::new()
                    .with_dirfd(dirfd)
                    .with_path(PathPtr::from_ptr(staged.as_raw() as *const libc::c_char))
                    .with_flags(flags)
                    .with_mode(None),
            )
            .await;
        drop(stack_guard);
        opened.map(|fd| fd as RawFd)
    }

    /// The absolute spelling Linux looks a name up below, for a placeholder
    /// descriptor `fd` Detcore keeps no record of
    /// ([`Self::unrecorded_placeholder`]): the directory's lookup base
    /// (`/proc`, or the descriptor directory's spelling for the caller).
    pub(super) async fn unrecorded_placeholder_base<G: Guest<Self>>(
        guest: &mut G,
        fd: RawFd,
    ) -> Option<PathBuf> {
        let directory = Self::unrecorded_placeholder(guest, fd).await?;
        let caller = Self::caller_numbers(guest)?;
        Some(directory.lookup_base(caller))
    }

    /// Whether the guest's `lseek(fd, offset, whence)` of a descriptor that
    /// Detcore does not answer itself would move the kernel position of a
    /// descriptor on a placeholder's object into the tags' range, where it
    /// would pass for a placeholder ([`Self::recognize_received_placeholder`]):
    /// a descriptor on procfs's root, where that is the object (ptrace), or
    /// on the placeholder directory (KVM). `handle_lseek` refuses that seek
    /// with `EINVAL`; Linux accepts any position up to 2^31 - 1 on procfs's
    /// root, and lists nothing from one this high (named difference).
    ///
    /// Decided whether or not a placeholder has been opened yet, whether or
    /// not Detcore recorded the descriptor's metadata
    /// (`--no-virtualize-metadata`), and whether or not the guest's table has
    /// a free slot, for nothing is opened to decide: the target position
    /// first, from the call alone (a negative or zero `SEEK_CUR` step cannot
    /// reach the range from a position below it, which every position of
    /// procfs's root is, this guard included), then the descriptor itself
    /// ([`Self::on_placeholder_object`]). A descriptor whose kind or
    /// filesystem cannot be told is refused (fail closed). Not where Detcore
    /// serves no placeholder ([`Self::serves_placeholders`]), where no
    /// placeholder is served or recognized.
    pub(crate) async fn seek_would_place_a_tag<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        offset: i64,
        whence: i32,
    ) -> bool {
        if !Self::serves_placeholders(guest) {
            return false;
        }
        let in_range = |target: i64| {
            u64::try_from(target)
                .ok()
                .and_then(fd_directory::tag_index)
                .is_some()
        };
        match whence {
            libc::SEEK_SET | libc::SEEK_END if !in_range(offset) => return false,
            libc::SEEK_SET | libc::SEEK_END => {}
            libc::SEEK_CUR if offset <= 0 => return false,
            libc::SEEK_CUR => {}
            _ => return false,
        }
        let recorded = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.stat())
            .ok()
            .flatten()
            .map(|stat| (stat.mode, stat.dev, stat.inode));
        let identity = match recorded {
            Some(identity) => identity,
            None => match self.inject_fstat(guest, fd).await {
                Ok(stat) => (stat.st_mode, stat.st_dev, stat.st_ino),
                // Not a descriptor: the kernel's own EBADF.
                Err(Errno::EBADF) => return false,
                // A descriptor that cannot be told is refused (fail closed).
                Err(_) => return true,
            },
        };
        if identity.0 & libc::S_IFMT != libc::S_IFDIR {
            return false;
        }
        let filesystem = match Self::placeholder_object(guest) {
            PlaceholderObject::ProcfsRoot => Some(Self::inject_fstatfs_type(guest, fd).await),
            PlaceholderObject::Placeholder => None,
        };
        if !Self::on_placeholder_object(
            (identity.1, identity.2),
            filesystem,
            &PLACEHOLDER_IDENTITIES.lock().unwrap(),
        ) {
            return false;
        }
        if whence != libc::SEEK_CUR {
            return true;
        }
        let current = guest
            .inject(
                syscalls::Lseek::new()
                    .with_fd(fd)
                    .with_offset(0)
                    .with_whence(syscalls::Whence::SEEK_CUR),
            )
            .await;
        match current {
            Ok(current) => in_range(current.wrapping_add(offset)),
            Err(_) => false,
        }
    }

    /// Whether a directory descriptor of identity `identity` (`(dev, ino)`,
    /// as the kernel beneath the backend reports it) may be on a
    /// placeholder's object: its identity is one this tool process's
    /// placeholders' objects have (`objects`), or, where the object is
    /// procfs's root (`filesystem` given: its `fstatfs` `f_type`, or why that
    /// failed), it is procfs (`PROC_SUPER_MAGIC`) at the root inode
    /// (`PROC_ROOT_INO`, 1), or its filesystem cannot be told (fail closed).
    fn on_placeholder_object(
        identity: (u64, u64),
        filesystem: Option<Result<i64, Errno>>,
        objects: &BTreeSet<(u64, u64)>,
    ) -> bool {
        objects.contains(&identity)
            || match filesystem {
                None => false,
                Some(Ok(f_type)) => {
                    f_type as u64 == reverie::syscalls::linux_abi::PROC_SUPER_MAGIC as u64
                        && identity.1 == procfs_inode::PROC_ROOT_INO
                }
                Some(Err(_)) => true,
            }
    }

    /// The filesystem type (`f_type`) of descriptor `fd`, by one injected
    /// `fstatfs` whose buffer is in Reverie's guest scratch or, where that
    /// cannot hold it, in a transient private page unmapped before the guest
    /// resumes (as `Detcore::inject_fstat`,
    /// <https://github.com/rrnewton/hermit/issues/3328>). No descriptor is
    /// opened.
    // GNU declares this kernel word as i64, musl as u64; retain its bits in
    // the common signed result, including filesystem magic values with bit 63 set.
    #[allow(clippy::unnecessary_cast)]
    async fn inject_fstatfs_type<G: Guest<Self>>(guest: &mut G, fd: RawFd) -> Result<i64, Errno> {
        let fstatfs = |buf: AddrMut<'_, libc::statfs>| {
            syscalls::Fstatfs::new().with_fd(fd).with_buf(Some(buf))
        };
        let on_stack = {
            let mut stack = guest.stack().await;
            let buf = stack.reserve::<libc::statfs>();
            match stack.commit() {
                Ok(guard) => {
                    let result = match guest.inject_with_retry(fstatfs(buf)).await {
                        Ok(_) => guest
                            .memory()
                            .read_value(buf)
                            .map(|statfs| statfs.f_type as i64),
                        Err(errno) => Err(errno),
                    };
                    drop(guard);
                    Some(result)
                }
                Err(_) => None,
            }
        };
        match on_stack {
            Some(Err(Errno::EFAULT)) | None => {}
            Some(result) => return result,
        }
        let length = std::mem::size_of::<libc::statfs>();
        let page = super::procfs_levels::map_transient(guest, length)
            .await
            .ok_or(Errno::ENOMEM)?;
        let buf = page.cast::<libc::statfs>();
        let result = match guest.inject_with_retry(fstatfs(buf)).await {
            Ok(_) => guest
                .memory()
                .read_value(buf)
                .map(|statfs| statfs.f_type as i64),
            Err(errno) => Err(errno),
        };
        super::procfs_levels::unmap_transient(guest, page, length).await;
        result
    }

    /// The descriptors of the table a descriptor directory bound to `binding`
    /// lists now, ascending, with the table's capacity, read by the caller:
    /// `Ok(None)` for a process whose thread group leader has exited (Linux
    /// has released its `files`), `ENOENT` for a task that is gone
    /// (`get_proc_task` finds none). The caller's own table (the process's
    /// for its process, or a thread's for a thread whose table it holds) comes
    /// from Detcore's model unless a receive may have installed descriptors
    /// the model cannot name; that, and another process's or thread's table,
    /// is listed by the kernel beneath the backend, through `object`, a
    /// placeholder descriptor on procfs's root where there is one
    /// ([`Self::kernel_listing_base`]).
    async fn descriptor_directory_table<G: Guest<Self>>(
        &self,
        guest: &mut G,
        binding: &DescriptorDirectoryBinding,
        object: Option<RawFd>,
    ) -> Result<Option<(Vec<i32>, u64)>, Errno> {
        let caller = Self::caller_numbers(guest).ok_or(Errno::EACCES)?;
        let (answer, current) = Self::bound_task(guest, binding).await;
        if !current {
            return Err(Errno::ENOENT);
        }
        if binding.table == DescriptorDirectoryTable::Process && answer.without_mm {
            return Ok(None);
        }
        let own = {
            let state = guest.thread_state();
            match binding.table {
                DescriptorDirectoryTable::Process => {
                    caller.0 == binding.pid
                        && (state.dettid.as_raw() == caller.0 || state.holds_process_files())
                }
                DescriptorDirectoryTable::Thread => {
                    caller.1 == binding.tid || binding.files == Some(state.files_id())
                }
            }
        };
        let base = self.kernel_listing_base(guest, object);
        if own {
            let (numbers, capacity, unaccounted) = guest.thread_state().descriptor_table();
            if !unaccounted {
                return Ok(Some((numbers, capacity)));
            }
            let path: &[u8] = match binding.table {
                DescriptorDirectoryTable::Process => b"self/fd",
                DescriptorDirectoryTable::Thread => b"thread-self/fd",
            };
            let listed = self
                .kernel_descriptor_numbers(guest, base, path, true)
                .await?;
            let capacity = capacity.max(fd_directory::capacity(&listed));
            return Ok(Some((listed, capacity)));
        }
        // Another table: the descriptor opened to read it is in the caller's,
        // and a descriptor of the same number in the one listed stays listed
        // (a fork's child listing the directory it inherited). (A process
        // that shares its table with another process, `CLONE_FILES` without
        // `CLONE_THREAD`, and lists that process's directory sees that
        // descriptor too: named.)
        let within = binding.within().into_os_string().into_encoded_bytes();
        let listed = self
            .kernel_descriptor_numbers(guest, base, &within[1..], false)
            .await?;
        let capacity = fd_directory::capacity(&listed);
        Ok(Some((listed, capacity)))
    }

    /// Where the kernel beneath the backend looks up a path within procfs
    /// for Detcore (a table it lists, a `/proc` entry it stats): from
    /// `object`, a placeholder descriptor on procfs's root, where the
    /// placeholder's object is that root (ptrace), so that the lookup starts
    /// at the guest's procfs whatever the caller's root or PID namespace;
    /// else from `/proc`, by its absolute spelling (`AT_FDCWD`).
    fn kernel_listing_base<G: Guest<Self>>(&self, guest: &G, object: Option<RawFd>) -> RawFd {
        match (Self::placeholder_object(guest), object) {
            (PlaceholderObject::ProcfsRoot, Some(object)) => object,
            _ => libc::AT_FDCWD,
        }
    }

    /// A descriptor table as the kernel beneath the backend lists the
    /// descriptor directory at `path` within procfs (`self/fd`,
    /// `<pid>/task/<tid>/fd`), looked up from `base` (a descriptor on procfs's
    /// root, or `AT_FDCWD`, below `/proc`): one injected open, `getdents64`
    /// until the end into a buffer on Reverie's guest stack, and a close,
    /// leaving out the descriptor this opens to read it where the table
    /// listed is the caller's own (`own`), which holds it.
    async fn kernel_descriptor_numbers<G: Guest<Self>>(
        &self,
        guest: &mut G,
        base: RawFd,
        path: &[u8],
        own: bool,
    ) -> Result<Vec<i32>, Errno> {
        const PATH_CAPACITY: usize = 64;
        const BUFFER: usize = 512;
        let mut spelled = Vec::with_capacity(path.len() + 6);
        if base == libc::AT_FDCWD {
            spelled.extend_from_slice(b"/proc/");
        }
        spelled.extend_from_slice(path);
        if spelled.len() >= PATH_CAPACITY {
            return Err(Errno::ENAMETOOLONG);
        }
        let mut stack = guest.stack().await;
        let staged = stack.push(super::rendered_procfs::c_string::<PATH_CAPACITY>(&spelled));
        let buffer = stack.reserve::<[u8; BUFFER]>();
        let stack_guard = stack.commit()?;
        let opened = guest
            .inject(
                syscalls::Openat::new()
                    .with_dirfd(base)
                    .with_path(PathPtr::from_ptr(staged.as_raw() as *const libc::c_char))
                    .with_flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
                    .with_mode(None),
            )
            .await;
        let mut numbers = Vec::new();
        let mut result = opened.map(|fd| fd as RawFd);
        if let Ok(fd) = result {
            loop {
                let read = guest
                    .inject(
                        syscalls::Getdents64::new()
                            .with_fd(fd as u32)
                            .with_dirent(Some(buffer.cast()))
                            .with_count(BUFFER as u32),
                    )
                    .await;
                let length = match read {
                    Ok(0) => break,
                    Ok(length) => length as usize,
                    Err(errno) => {
                        result = Err(errno);
                        break;
                    }
                };
                let mut bytes = vec![0; length];
                if let Err(errno) = guest.memory().read_exact(buffer.cast::<u8>(), &mut bytes) {
                    result = Err(errno);
                    break;
                }
                numbers.extend(crate::util::descriptor_names(&bytes));
            }
            let _ = guest.inject(syscalls::Close::new().with_fd(fd)).await;
        }
        drop(stack_guard);
        let fd = result?;
        if own {
            numbers.retain(|number| *number != fd);
        }
        numbers.sort_unstable();
        Ok(numbers)
    }

    /// The inode number of the directory a binding names itself (its `.`,
    /// and its `fstat` and `stat`), from Detcore's pool by the directory's
    /// path on the device of the guest's `/proc`: the key the entry `fd` of
    /// the process's directory is listed with (`modeled_directory_snapshot`),
    /// so the two agree.
    async fn directory_inode<G: Guest<Self>>(
        &self,
        guest: &mut G,
        device: u64,
        within: &Path,
        mark: crate::tool_global::SnapshotMark,
    ) -> u64 {
        let inodes = DirectoryInodes::new(device, Some(ProcfsDirectory::new(within.to_path_buf())));
        let (host, name) = inodes.entry_inode(b".", procfs_inode::path_inode(device, within).ino);
        crate::tool_global::determinize_listed_inode(guest, host, name, mark)
            .await
            .0
            .as_raw()
    }

    /// Answer a `getdents` or `getdents64` of the descriptor directory open as
    /// `fd`, whose state is `state`, into the guest's buffer `buf` of
    /// `capacity` bytes, as `proc_readfd_common` answers it: the entries from
    /// the position on that fit, of the table as it is now, each with the
    /// inode number Detcore's pool gives its path, and the position moved
    /// past the entries copied (see [`fd_directory`]).
    async fn serve_descriptor_directory<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        state: PlaceholderState,
        buf: Option<AddrMut<'_, u8>>,
        capacity: usize,
        format: DirentFormat,
    ) -> Result<i64, Error> {
        if state.path_only {
            return Err(Errno::EBADF.into());
        }
        let PlaceholderDirectory::Descriptors(binding) = state.directory else {
            return Err(Errno::EBADF.into());
        };
        let table = self
            .descriptor_directory_table(guest, &binding, Some(fd))
            .await?;
        let (entries, end) = fd_directory::entries_from(
            state.position,
            table
                .as_ref()
                .map(|(numbers, capacity)| (numbers.as_slice(), *capacity)),
        );
        let save = |guest: &mut G, position: u64| {
            let _ = guest.thread_state().with_detfd(fd, |detfd| {
                if let Some(current) = detfd.placeholder() {
                    detfd.set_placeholder(PlaceholderState {
                        position,
                        ..current
                    });
                }
            });
        };
        if entries.is_empty() {
            save(guest, end);
            return Ok(0);
        }
        let count = fd_directory::fitting(&entries, capacity, |name| format.record_len(name));
        let Some(buf) = buf.filter(|_| count > 0) else {
            save(guest, entries[0].position);
            return Err(if count == 0 {
                Errno::EINVAL
            } else {
                Errno::EFAULT
            }
            .into());
        };
        let offsets = fd_directory::offsets(&entries, count, end);
        let mark = crate::tool_global::directory_snapshot_mark(guest).await;
        let within = binding.within();
        let inodes =
            DirectoryInodes::new(binding.device, Some(ProcfsDirectory::new(within.clone())));
        let mut records = Vec::new();
        let mut names = Vec::with_capacity(count);
        for (entry, offset) in entries.iter().zip(&offsets) {
            let ino = if entry.name == b"." {
                self.directory_inode(guest, binding.device, &within, mark)
                    .await
            } else {
                let entry_path = match entry.name.as_slice() {
                    b".." => within.parent().unwrap_or(&within).to_path_buf(),
                    name => within.join(std::ffi::OsStr::from_bytes(name)),
                };
                let key = procfs_inode::path_inode(binding.device, &entry_path).ino;
                let (host, name) = inodes.entry_inode(&entry.name, key);
                crate::tool_global::determinize_listed_inode(guest, host, name, mark)
                    .await
                    .0
                    .as_raw()
            };
            let record = DirEntry {
                name: entry.name.clone(),
                ino: 0,
                off: *offset,
                ty: entry.ty,
            };
            format.encode(&record, ino, *offset, &mut records);
            names.push(entry.name.len());
        }
        let (passed, copied) = super::copy_records(
            &mut guest.memory(),
            buf,
            format,
            &records,
            &names,
            entries[0].position,
        );
        save(guest, fd_directory::position_after(&entries, passed, end));
        Ok(copied.map(|length| length as i64)?)
    }

    /// The entries of `/proc` as Detcore's procfs model lists them
    /// (`procfs::listing`, for the run's `--proc` level and the processes the
    /// scheduler lists), and how to key their inode numbers, for a `/proc`
    /// placeholder's directory stream: what `modeled_directory_snapshot`
    /// gives a descriptor on the kernel's `/proc`, but with every entry
    /// looked up by its absolute spelling (the placeholder holds none), no
    /// kernel cookies (the stream's positions are the guest's), and keyed by
    /// the procfs key device ([`Self::procfs_key_device`]). On the kernel's
    /// procfs an entry the lookup does not find is left out unless Detcore
    /// renders it, as there; on a backend's own `/proc` (KVM), whose lookups
    /// follow its executor at host-timed moments, a lookup decides no entry,
    /// and an entry it does not answer is numbered by its path's key.
    async fn root_placeholder_snapshot<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<(Vec<DirEntry>, DirectoryInodes, DirectoryOrder), Error> {
        use crate::procfs::listing;
        use crate::procfs::listing::ModeledDirectory;
        let directory = ModeledDirectory::System(Vec::new());
        let facts = crate::tool_global::procfs_listing(guest, None).await;
        let names = listing::entries(&directory, self.cfg.proc_level, &facts).unwrap_or_default();
        let device = Self::procfs_key_device(guest);
        let proc = Path::new("/proc");
        // Each entry looked up where the kernel finds it: by name from the
        // descriptor, where it is open on procfs's root (ptrace), whatever the
        // caller's root; else by its absolute spelling.
        let base = self.kernel_listing_base(guest, Some(fd));
        let spellings = names
            .iter()
            .map(|(name, _)| {
                if base == libc::AT_FDCWD {
                    proc.join(OsStr::from_bytes(name))
                } else {
                    PathBuf::from(OsStr::from_bytes(name))
                }
            })
            .collect::<Vec<_>>();
        let looked_up = self.looked_up_inodes(guest, base, &spellings).await?;
        let backend_procfs = !procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend);
        let within = listing::within(&directory);
        let session_files = self.rendered_files(guest);
        let entries = names
            .into_iter()
            .zip(looked_up)
            .filter_map(|((name, ty), looked_up)| {
                let entry = match name.as_slice() {
                    b"." => within.clone(),
                    b".." => within.parent().unwrap_or(&within).to_path_buf(),
                    name => within.join(OsStr::from_bytes(name)),
                };
                // A file Detcore renders exists whatever the kernel has. (Not
                // a task's `comm` where the kernel serves every one.)
                let rendered = crate::procfs::ProcfsFile::from_path(
                    &proc.join(entry.strip_prefix("/").unwrap_or(&entry)),
                )
                .is_some_and(|file| {
                    !self.kernel_serves_every_comm(&file)
                        && file.rendered_path(session_files).is_some()
                });
                let ino = match looked_up {
                    Ok(ino) => ino,
                    Err(Errno::ENOENT)
                        if name != b"." && name != b".." && !rendered && !backend_procfs =>
                    {
                        return None;
                    }
                    Err(_) => procfs_inode::path_inode(device, &entry).ino,
                };
                Some(DirEntry {
                    ino,
                    off: 0,
                    ty,
                    name,
                })
            })
            .collect::<Vec<_>>();
        let inodes = DirectoryInodes::new(device, Some(ProcfsDirectory::new(within)));
        Ok((entries, inodes, DirectoryOrder::ProcfsIds))
    }

    /// Answer a `getdents` or `getdents64` of the `/proc` placeholder open as
    /// `fd` from its directory stream, shared by the description's aliases:
    /// the stream's snapshot of [`Self::root_placeholder_snapshot`], taken at
    /// the first read and at the first after a seek to 0, as for every
    /// directory Detcore lists (`DirectoryStream`). The kernel position stays
    /// at the tag (`Detcore::move_directory_kernel_position` moves no
    /// placeholder). A count that does not fit an `int` fits nothing: `EINVAL`,
    /// or 0 at the end.
    async fn serve_root_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
        fd: RawFd,
        state: PlaceholderState,
    ) -> Result<i64, Error> {
        if state.path_only {
            return Err(Errno::EBADF.into());
        }
        let (buf, capacity, format) = match call {
            Syscall::Getdents(c) => (
                c.dirent().map(|dirent| dirent.cast::<u8>()),
                c.count() as usize,
                DirentFormat::Legacy,
            ),
            Syscall::Getdents64(c) => (
                c.dirent().map(|dirent| dirent.cast::<u8>()),
                c.count() as usize,
                DirentFormat::Dirent64,
            ),
            _ => return Err(Errno::ENOSYS.into()),
        };
        let lock = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.directory_lock())?;
        let _serving = lock.lock().await;
        if guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.directory_needs_snapshot())?
        {
            let mark = crate::tool_global::directory_snapshot_mark(guest).await;
            let (entries, inodes, order) = self.root_placeholder_snapshot(guest, fd).await?;
            let mut entries = Some(entries);
            guest.thread_state().with_detfd(fd, |detfd| {
                detfd.install_directory_snapshot(
                    entries.take().unwrap_or_default(),
                    mark,
                    inodes.clone(),
                    order,
                )
            })?;
        }
        let fits = i32::try_from(capacity).is_ok();
        let batch = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.with_directory_stream(|stream| {
                stream.next_batch(format, if fits { capacity } else { 0 })
            })
        })??;
        match batch {
            Ok(batch) if batch.is_empty() => return Ok(0),
            Ok(_) if !fits => return Err(Errno::EINVAL.into()),
            Ok(_) => {}
            Err(errno) => return Err(errno.into()),
        }
        let Some(buf) = buf else {
            return Err(Errno::EFAULT.into());
        };
        let (sysno, _) = call.into_parts();
        let empty = Syscall::from_raw(
            sysno,
            super::procfs_levels::raw_args([fd as usize, 0, 0, 0, 0, 0]),
        );
        self.serve_next_batch(
            guest,
            GetdentsCall {
                call: *call,
                empty,
                fd,
                buf,
                capacity,
                format,
            },
        )
        .await
    }

    /// The metadata of a descriptor directory bound to `binding`, as Linux's
    /// `proc_fd_getattr` reports it: a directory of mode 0500
    /// (`S_IRUSR|S_IXUSR`, `tgid_base_stuff`), 2 links
    /// (`proc_pident_instantiate`), its task's owner (`task_dump_owner`, as for
    /// the files Detcore renders), procfs's 1024-byte blocks and device, the
    /// inode number Detcore's pool gives its path, and a size that is the
    /// number of open descriptors (`proc_readfd_count`).
    async fn descriptor_directory_metadata<G: Guest<Self>>(
        &self,
        guest: &mut G,
        binding: &DescriptorDirectoryBinding,
        object: Option<RawFd>,
    ) -> Result<DetStat, Error> {
        let open = match self
            .descriptor_directory_table(guest, binding, object)
            .await
        {
            Ok(table) => table.map_or(0, |(numbers, _)| numbers.len()),
            Err(Errno::ENOENT) => return Err(Errno::ENOENT.into()),
            Err(_) => 0,
        };
        let owner_path = match binding.table {
            DescriptorDirectoryTable::Process => {
                PathBuf::from(format!("/proc/{}/stat", binding.pid))
            }
            DescriptorDirectoryTable::Thread => {
                PathBuf::from(format!("/proc/{}/task/{}/stat", binding.pid, binding.tid))
            }
        };
        let owner = self.rendered_file_owner(guest, &owner_path).await?;
        let mark = crate::tool_global::directory_snapshot_mark(guest).await;
        let ino = self
            .directory_inode(guest, binding.device, &binding.within(), mark)
            .await;
        let epoch = super::rendered_procfs::epoch_time(guest.config().epoch);
        Ok(DetStat {
            mask: StatxMask::STATX_BASIC_STATS | StatxMask::STATX_MNT_ID,
            mode: libc::S_IFDIR | 0o500,
            uid: owner.0,
            gid: owner.1,
            attributes: StatxAttributes::empty(),
            attributes_mask: StatxAttributes::STATX_ATTR_AUTOMOUNT
                | StatxAttributes::STATX_ATTR_MOUNT_ROOT
                | StatxAttributes::STATX_ATTR_DAX,
            dev: Self::reported_procfs_device(guest),
            rdev: 0,
            inode: ino,
            nlink: 2,
            size: open as i64,
            blksize: procfs_inode::PROC_BLOCK_SIZE,
            blocks: 0,
            atime: epoch,
            btime: epoch,
            ctime: epoch,
            mtime: epoch,
        })
    }

    /// The metadata of `/proc` itself, as Linux's `proc_root_getattr`
    /// reports it with Detcore's rules for procfs: a directory with the mode
    /// and owner the guest's procfs root has (0555, global root's, which the
    /// guest's user namespace does not map), one link (Detcore's number for
    /// procfs's root, whose own counts the host's processes), size 0,
    /// procfs's 1024-byte blocks and device, the mount root attribute, the
    /// epoch, and the inode number its `.` entry gets in a listing of it
    /// ([`Self::root_placeholder_snapshot`]): the number the kernel's procfs
    /// gives its root (1) where the backend's `/proc` is the kernel's, else
    /// its path's key.
    async fn root_placeholder_metadata<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<DetStat, Error> {
        let (mode, uid, gid) = super::rendered_procfs::guest_procfs_root()?;
        let device = Self::procfs_key_device(guest);
        let within = PathBuf::from("/");
        let raw = if procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend) {
            procfs_inode::PROC_ROOT_INO
        } else {
            procfs_inode::path_inode(device, &within).ino
        };
        let inodes = DirectoryInodes::new(device, Some(ProcfsDirectory::new(within)));
        let (host, name) = inodes.entry_inode(b".", raw);
        let mark = crate::tool_global::directory_snapshot_mark(guest).await;
        let ino = crate::tool_global::determinize_listed_inode(guest, host, name, mark)
            .await
            .0
            .as_raw();
        let epoch = super::rendered_procfs::epoch_time(guest.config().epoch);
        Ok(DetStat {
            mask: StatxMask::STATX_BASIC_STATS | StatxMask::STATX_MNT_ID,
            mode: libc::S_IFDIR | (mode & 0o7777),
            uid,
            gid,
            attributes: StatxAttributes::STATX_ATTR_MOUNT_ROOT,
            attributes_mask: StatxAttributes::STATX_ATTR_AUTOMOUNT
                | StatxAttributes::STATX_ATTR_MOUNT_ROOT
                | StatxAttributes::STATX_ATTR_DAX,
            dev: Self::reported_procfs_device(guest),
            rdev: 0,
            inode: ino,
            nlink: 1,
            size: 0,
            blksize: procfs_inode::PROC_BLOCK_SIZE,
            blocks: 0,
            atime: epoch,
            btime: epoch,
            ctime: epoch,
            mtime: epoch,
        })
    }

    /// The metadata of a placeholder directory: `/proc`'s, or a descriptor
    /// directory's, read through the placeholder descriptor `object` where
    /// one names it ([`Self::kernel_listing_base`]).
    pub(super) async fn placeholder_metadata<G: Guest<Self>>(
        &self,
        guest: &mut G,
        directory: &PlaceholderDirectory,
        object: Option<RawFd>,
    ) -> Result<DetStat, Error> {
        match directory {
            PlaceholderDirectory::Root => self.root_placeholder_metadata(guest).await,
            PlaceholderDirectory::Descriptors(binding) => {
                self.descriptor_directory_metadata(guest, binding, object)
                    .await
            }
        }
    }

    /// The fdinfo Linux gives placeholder descriptor `fd`, as the kernel's
    /// fdinfo for it would need to be (it describes the placeholder: its tag
    /// as the position, its tmpfs mount and inode): Detcore's position, the
    /// guest's procfs mount and inode 0, which the snapshot's sanitizer
    /// replaces with the directory's inode number (`procfs::sanitize_fdinfo`),
    /// returned beside it, as the placeholder's `fstat` numbers it. `None`
    /// for any other descriptor.
    pub(super) async fn placeholder_fdinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<(Vec<u8>, u64)>, Error> {
        let Some(state) = Self::placeholder_of(guest, fd) else {
            return Ok(None);
        };
        let position = match state.directory {
            PlaceholderDirectory::Root => guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.directory_stream_position())?,
            PlaceholderDirectory::Descriptors(_) => state.position,
        };
        let contents = format!(
            "pos:\t{position}\nflags:\t0\nmnt_id:\t{}\nino:\t0\n",
            super::rendered_procfs::guest_procfs_mount_id()?
        )
        .into_bytes();
        let inode = self
            .placeholder_metadata(guest, &state.directory, Some(fd))
            .await?
            .inode;
        Ok(Some((contents, inode)))
    }

    /// The metadata of the placeholder directory a stat names, if Detcore
    /// serves it: a descriptor that is one, a path that spells `/proc` or one
    /// of the caller's own descriptor directories, or, followed, a descriptor
    /// link of the caller's to a placeholder (`/proc/self/fd/D`; not
    /// followed, that is the link, which the kernel describes). A spelling of
    /// `/proc` through a task that does not exist is `ENOENT`, where Linux's
    /// walk stops ([`NamedTask`]).
    pub(super) async fn placeholder_stat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target: StatTarget<'_>,
    ) -> Result<Option<DetStat>, Error> {
        if !Self::serves_placeholders(guest) {
            return Ok(None);
        }
        let (directory, object) = match target {
            StatTarget::Descriptor(fd) => match Self::placeholder_of(guest, fd) {
                Some(state) => (state.directory, Some(fd)),
                None => return Ok(None),
            },
            StatTarget::Path {
                dirfd,
                path: Some(pointer),
                follow,
            } => {
                let path = pointer.read(&guest.memory())?;
                if path.as_os_str().is_empty()
                    || Self::starts_at_a_recorded_link(guest, dirfd, &path)
                {
                    return Ok(None);
                }
                let Some((pid, tid)) = Self::descriptor_directory_caller(guest) else {
                    return Ok(None);
                };
                let spelling = self.procfs_spelling_at(guest, dirfd, &path).await?;
                let (spelling, links) = Self::decided_path(guest, &spelling)?;
                let directory = match fd_directory::classify(&spelling, (pid, tid)) {
                    Some(table) => {
                        let thread = (table == DescriptorDirectoryTable::Thread).then_some(tid);
                        let answer = crate::tool_global::procfs_task(
                            guest,
                            crate::procfs::BoundTask {
                                process: pid,
                                thread,
                            },
                        )
                        .await;
                        PlaceholderDirectory::Descriptors(DescriptorDirectoryBinding {
                            table,
                            pid,
                            tid,
                            process_incarnation: answer.process_incarnation,
                            thread_incarnation: answer.thread_incarnation,
                            device: Self::procfs_key_device(guest),
                            files: (table == DescriptorDirectoryTable::Thread)
                                .then(|| guest.thread_state().files_id()),
                        })
                    }
                    None => match names_procfs_root(&spelling) {
                        Some(tasks) if Self::named_tasks_exist(guest, &tasks).await => {
                            PlaceholderDirectory::Root
                        }
                        Some(_) => return Err(Errno::ENOENT.into()),
                        None if follow => match Self::own_link_directory(guest, &spelling) {
                            // Following this last link too, within the one
                            // budget.
                            Some(_) if links + selector_links(&spelling) + 1 > LINK_LIMIT => {
                                return Err(Errno::ELOOP.into());
                            }
                            Some(directory) => directory,
                            None => return Ok(None),
                        },
                        None => return Ok(None),
                    },
                };
                (directory, None)
            }
            StatTarget::Path { path: None, .. } | StatTarget::Cwd | StatTarget::Overwritten => {
                return Ok(None);
            }
        };
        Ok(Some(
            self.placeholder_metadata(guest, &directory, object).await?,
        ))
    }

    /// The named refusal of `sysno` for `reason` (ARCHITECTURE.md rule 6),
    /// logged as Detcore's other refusals are, then stopping the run, or,
    /// with `--allow-unsupported-syscalls`, giving the guest `fallback`
    /// (`Detcore::refuse_unserviceable_operation_because`).
    async fn refuse_placeholder_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        fallback: Errno,
        reason: &str,
    ) -> Result<i64, Error> {
        tracing::error!("Detcore refuses {sysno}: {reason}");
        self.refuse_unserviceable_operation_because(guest, sysno, fallback, reason)
            .await
    }

    /// Answer `call` if it operates on a placeholder descriptor
    /// ([`placeholder_operand`]), from Detcore's model, with one implementation
    /// whatever the descriptor's object ([`PlaceholderObject`]), so that no
    /// call through it reaches that object's filesystem, whose answers are
    /// not the directory's. The calls, and their answers, as Linux answers
    /// them for procfs's root and for a descriptor directory, each in Linux's
    /// order of checks (v6.17; measured on Linux 7.1):
    /// - `getdents`, `getdents64`: the listing (above); `lseek`:
    ///   `generic_file_llseek` on an empty directory (`fd_directory::seek`),
    ///   on the listing's position;
    /// - `fchdir`: the directory entered ([`Self::enter_placeholder`]);
    /// - `fcntl` ([`Self::placeholder_fcntl`]): the status flags, as `setfl`
    ///   keeps them; the table commands, which reach the descriptor; the
    ///   query commands; the write hints, kept for the directory's inode; the
    ///   record locks refused by name (`ENOLCK`: named residual); the lease,
    ///   seal, pipe and notify commands as Linux answers them for a directory;
    /// - `read` and its relatives: the position's `EINVAL` where one is given,
    ///   the buffer's `access_ok` (`EFAULT`), the vectors, then `EISDIR`
    ///   ([`read_answer`]); `write` and its relatives: a negative position's
    ///   `EINVAL`, then `EBADF`, the description cannot write;
    /// - `ioctl` ([`Self::placeholder_ioctl`]);
    /// - `fsync`, `fdatasync`: `EINVAL` (procfs has no `fsync`); `syncfs`: 0;
    /// - `flock`: each directory its own lock (`procfs::locks::LockLedger::
    ///   placeholder_flock`), as each is its own inode on Linux;
    /// - `fchmod` and the path-less `fchmodat2`: `EPERM`; `fchown` and the
    ///   path-less `fchownat`: 0, Detcore's virtual root's answer
    ///   (`handle_ownership_change_noop`) with no change to report;
    /// - the `f*xattr` calls: procfs's answers (no attribute handlers);
    /// - `ftruncate`: a negative length's `EINVAL`, then `EINVAL` (not a
    ///   regular file); `fallocate`: `vfs_fallocate`'s checks, then `EBADF`;
    ///   `fadvise64`: `generic_fadvise` on a filesystem with no backing
    ///   device; `readahead`: `EINVAL`; `sync_file_range`: its argument
    ///   checks, then 0;
    /// - `fstatfs`: procfs's (`write_procfs_statfs`);
    /// - a file-backed `mmap`: as for a procfs file without `mmap`
    ///   (`answer_rendered_procfs_mapping`);
    /// - the path-less `utimensat`: `EPERM` with times or `EACCES` without on
    ///   global root's `/proc`, 0 on the caller's own directory;
    ///   `faccessat2`: `EACCES` for write access to `/proc`, else 0;
    /// - `sendfile`, `splice`, `tee` and `copy_file_range` with a placeholder
    ///   on either side ([`Self::answer_placeholder_transfer`]).
    ///
    /// An `O_PATH` descriptor fails with `EBADF` where Linux's descriptor
    /// lookup refuses it (`fdget`), which is first for most calls and after
    /// the arguments a call checks first for the others (a negative position
    /// or length, `flock`'s command, the attribute name); `fchdir`, `fstatfs`,
    /// `fcntl`'s table and query commands and the path calls take it. A
    /// placeholder Detcore keeps no record of (one received without
    /// sequentialized threads) has no state to answer from, and its call is
    /// refused by name. `None` for every other call.
    pub(crate) async fn answer_placeholder_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Option<Result<i64, Error>> {
        let (sysno, args) = (*call).into_parts();
        if !Self::serves_placeholders(guest) {
            return None;
        }
        // Calls with two descriptor operands, either a placeholder.
        if matches!(
            sysno,
            Sysno::sendfile | Sysno::splice | Sysno::tee | Sysno::copy_file_range
        ) {
            return self.answer_placeholder_transfer(guest, sysno, &args).await;
        }
        let fd_position = placeholder_operand(&guest.memory(), sysno, &args)?;
        let fd = argument(&args, fd_position) as i32;
        let state = match Self::placeholder_of(guest, fd) {
            Some(state) => state,
            None => {
                let directory = Self::unrecorded_placeholder(guest, fd).await?;
                return Some(
                    self.refuse_placeholder_call(
                        guest,
                        sysno,
                        Errno::EBADF,
                        &format!(
                            "descriptor {fd} stands for {}, a /proc directory Detcore serves, \
                             but was received without sequentialized threads, and Detcore \
                             keeps no state to answer it from",
                            directory.canonical().display()
                        ),
                    )
                    .await,
                );
            }
        };
        // `flock` ignores `LOCK_MAND` before it looks at the descriptor.
        if sysno == Sysno::flock && args.arg1 as i32 & LOCK_MAND != 0 {
            return Some(Ok(0));
        }
        // The calls whose descriptor lookup (`fdget`, `fdget_pos`) refuses
        // an `O_PATH` descriptor before they look at anything else.
        let path_only_refused = state.path_only
            && matches!(
                sysno,
                Sysno::getdents
                    | Sysno::getdents64
                    | Sysno::lseek
                    | Sysno::read
                    | Sysno::readv
                    | Sysno::write
                    | Sysno::writev
                    | Sysno::ioctl
                    | Sysno::fsync
                    | Sysno::fdatasync
                    | Sysno::syncfs
                    | Sysno::fchmod
                    | Sysno::fchown
                    | Sysno::fallocate
                    | Sysno::fadvise64
                    | Sysno::readahead
                    | Sysno::sync_file_range
                    | Sysno::flistxattr
                    | Sysno::mmap
            );
        if path_only_refused {
            return Some(Err(Errno::EBADF.into()));
        }
        // The calls that check an argument before their descriptor lookup
        // refuses an `O_PATH` descriptor: a negative position or length.
        let positioned = |position: i64, minus_one_reads_on: bool| {
            if position < 0 && !(minus_one_reads_on && position == -1) {
                Some(Err(Errno::EINVAL.into()))
            } else if state.path_only {
                Some(Err(Errno::EBADF.into()))
            } else {
                None
            }
        };
        let early = match sysno {
            Sysno::pread64 | Sysno::pwrite64 | Sysno::preadv | Sysno::pwritev => {
                positioned(args.arg3 as i64, false)
            }
            Sysno::preadv2 | Sysno::pwritev2 => positioned(args.arg3 as i64, true),
            Sysno::ftruncate => positioned(args.arg1 as i64, false),
            _ => None,
        };
        if let Some(answer) = early {
            return Some(answer);
        }
        Some(match call {
            Syscall::Getdents(_) | Syscall::Getdents64(_)
                if state.directory == PlaceholderDirectory::Root =>
            {
                self.serve_root_placeholder(guest, call, fd, state).await
            }
            Syscall::Getdents(c) => {
                self.serve_descriptor_directory(
                    guest,
                    fd,
                    state,
                    c.dirent().map(|dirent| dirent.cast::<u8>()),
                    c.count() as usize,
                    DirentFormat::Legacy,
                )
                .await
            }
            Syscall::Getdents64(c) => {
                self.serve_descriptor_directory(
                    guest,
                    fd,
                    state,
                    c.dirent().map(|dirent| dirent.cast::<u8>()),
                    c.count() as usize,
                    DirentFormat::Dirent64,
                )
                .await
            }
            Syscall::Lseek(c) => {
                self.seek_placeholder(guest, fd, state, c.offset(), args.arg2 as i32)
            }
            Syscall::Fchdir(_) => self.enter_placeholder(guest, fd, &state.directory).await?,
            Syscall::Fcntl(c) => self.placeholder_fcntl(guest, fd, state, c).await?,
            Syscall::Read(_)
            | Syscall::Readv(_)
            | Syscall::Pread64(_)
            | Syscall::Preadv(_)
            | Syscall::Preadv2(_) => read_answer(&guest.memory(), sysno, &args).map_err(Into::into),
            Syscall::Write(_)
            | Syscall::Writev(_)
            | Syscall::Pwrite64(_)
            | Syscall::Pwritev(_)
            | Syscall::Pwritev2(_) => Err(Errno::EBADF.into()),
            Syscall::Ioctl(_) => self
                .placeholder_ioctl(guest, fd, args.arg1 as u32, args.arg2)
                .await?
                .map_err(Into::into),
            Syscall::Fsync(_) | Syscall::Fdatasync(_) => Err(Errno::EINVAL.into()),
            Syscall::Flock(c) => {
                self.placeholder_flock(guest, fd, state, c.operation())
                    .await
            }
            Syscall::Fchmod(_) => Err(Errno::EPERM.into()),
            Syscall::Fchown(_) => Ok(0),
            Syscall::Fgetxattr(_)
            | Syscall::Fsetxattr(_)
            | Syscall::Flistxattr(_)
            | Syscall::Fremovexattr(_) => self.placeholder_xattr(guest, call, state).await,
            Syscall::Ftruncate(_) => Err(Errno::EINVAL.into()),
            Syscall::Fallocate(_) => {
                Err(fallocate_answer(args.arg1 as i32, args.arg2 as i64, args.arg3 as i64).into())
            }
            Syscall::Readahead(_) => Err(Errno::EINVAL.into()),
            Syscall::Fstatfs(c) => self.write_procfs_statfs(guest, c.buf()).map(|()| 0),
            Syscall::Mmap(_) => return None,
            _ => match sysno {
                Sysno::syncfs => Ok(0),
                Sysno::fadvise64 => {
                    fadvise_answer(args.arg2 as i64, args.arg3 as i32).map_err(Into::into)
                }
                Sysno::sync_file_range => {
                    sync_file_range_answer(args.arg1 as i64, args.arg2 as i64, args.arg3 as u32)
                        .map_err(Into::into)
                }
                Sysno::utimensat => {
                    self.placeholder_utimensat(
                        guest,
                        state,
                        args.arg1 == 0,
                        args.arg2,
                        args.arg3 as i32,
                    )
                    .await
                }
                // `do_fchmodat`'s flags, then procfs's answer to a mode change.
                Sysno::fchmodat2
                    if args.arg3 as i32 & !(libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH)
                        != 0 =>
                {
                    Err(Errno::EINVAL.into())
                }
                Sysno::fchmodat2 => Err(Errno::EPERM.into()),
                // `do_fchownat`'s flags, then Detcore's virtual root's answer.
                Sysno::fchownat
                    if args.arg4 as i32 & !(libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH)
                        != 0 =>
                {
                    Err(Errno::EINVAL.into())
                }
                Sysno::fchownat => Ok(0),
                Sysno::faccessat2 => self
                    .placeholder_access(&state.directory, args.arg2 as i32, args.arg3 as i32)
                    .map_err(Into::into),
                _ => return None,
            },
        })
    }

    /// Whether `call` is one [`Self::refuse_lookup_below_placeholder`] may
    /// refuse, by its number alone, so that every other call skips it.
    pub(crate) fn looks_up_below_placeholders(call: &Syscall) -> bool {
        matches!(
            call.number(),
            Sysno::execve
                | Sysno::execveat
                | Sysno::bind
                | Sysno::connect
                | Sysno::sendto
                | Sysno::sendmsg
                | Sysno::sendmmsg
        ) && OPENED_ANY.load(Ordering::SeqCst)
    }

    /// The named refusal (ARCHITECTURE.md rule 6) of a call whose path leads
    /// below a placeholder directory, and which Detcore does not respell for
    /// the backend ([`Self::respelled_lookup`]): `execve` and `execveat`,
    /// which read their image before an injection could carry a respelled
    /// path, and the pathname `AF_UNIX` addresses of `bind`, `connect`,
    /// `sendto`, `sendmsg` and `sendmmsg`, which live in the guest's own
    /// structures. Such a path would reach the placeholder's object, not the
    /// directory, where Linux reaches the directory: on ptrace only a
    /// descriptor directory (`/proc` itself is its descriptor's object), on
    /// KVM either. A path that is a descriptor link and nothing more is left
    /// to the backend: an exec of a directory and an `AF_UNIX` address on one
    /// fail alike there (`EACCES`, `ECONNREFUSED` or `EADDRINUSE`). A send's
    /// address is looked up only by a datagram socket (`unix_dgram_sendmsg`):
    /// a stream socket refuses it without a lookup (`EISCONN` or
    /// `EOPNOTSUPP`, `unix_stream_sendmsg`) and a sequenced-packet socket
    /// ignores it (`unix_seqpacket_sendmsg`), so their sends go on as Linux's
    /// do; where the socket's type cannot be asked, the send is refused.
    /// `None` for every other call and path.
    pub(crate) async fn refuse_lookup_below_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Option<Result<i64, Error>> {
        if !OPENED_ANY.load(Ordering::SeqCst) || !Self::serves_placeholders(guest) {
            return None;
        }
        let (sysno, args) = call.into_parts();
        let read_path = |guest: &G, pointer: usize| -> Option<Vec<u8>> {
            PathPtr::from_ptr(pointer as *const libc::c_char)
                .and_then(|pointer| pointer.read(&guest.memory()).ok())
                .map(|path: PathBuf| path.into_os_string().into_encoded_bytes())
                .filter(|path| !path.is_empty())
        };
        let lookups: Vec<(Option<RawFd>, Vec<u8>)> = match sysno {
            Sysno::execve => read_path(guest, args.arg0)
                .map(|path| (None, path))
                .into_iter()
                .collect(),
            Sysno::execveat => read_path(guest, args.arg1)
                .map(|path| (Some(args.arg0 as RawFd), path))
                .into_iter()
                .collect(),
            Sysno::bind | Sysno::connect => {
                unix_address_path(&guest.memory(), args.arg1, args.arg2)
                    .map(|path| (None, path))
                    .into_iter()
                    .collect()
            }
            Sysno::sendto => unix_address_path(&guest.memory(), args.arg4, args.arg5)
                .map(|path| (None, path))
                .into_iter()
                .collect(),
            Sysno::sendmsg => message_addresses(&guest.memory(), args.arg1, 1, 0)
                .into_iter()
                .map(|path| (None, path))
                .collect(),
            Sysno::sendmmsg => message_addresses(&guest.memory(), args.arg1, args.arg2, 64)
                .into_iter()
                .map(|path| (None, path))
                .collect(),
            _ => return None,
        };
        let send = matches!(sysno, Sysno::sendto | Sysno::sendmsg | Sysno::sendmmsg);
        for (dirfd, path) in lookups {
            let looked_up = Self::respelled_lookup(guest, dirfd, &path, false, 0).await;
            if matches!(looked_up, Ok(None)) {
                continue;
            }
            // Only a datagram socket looks a send's address up, so no answer
            // of the lookup's, its errors included, is a stream or
            // sequenced-packet socket's; one socket makes every send of the
            // call.
            if send
                && self
                    .socket_is_datagram(guest, args.arg0 as RawFd)
                    .await
                    .is_some_and(|datagram| !datagram)
            {
                return None;
            }
            if let Err(errno) = looked_up {
                return Some(Err(errno.into()));
            }
            return Some(
                self.refuse_placeholder_call(
                    guest,
                    sysno,
                    Errno::ENOENT,
                    &format!(
                        "the path {} leads below a /proc directory Detcore serves on a \
                         placeholder descriptor, and Detcore cannot spell it for this call as \
                         Linux resolves it",
                        String::from_utf8_lossy(&path)
                    ),
                )
                .await,
            );
        }
        None
    }

    /// `lseek` of a placeholder descriptor, which is `generic_file_llseek` on
    /// an empty directory ([`fd_directory::seek`]) on both: a descriptor
    /// directory's position, and `/proc`'s, the position of its directory
    /// stream, whose snapshot a seek to 0 drops, as `rewinddir` refreshes
    /// one (`DirectoryStream::seek`).
    fn seek_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        state: PlaceholderState,
        offset: i64,
        whence: i32,
    ) -> Result<i64, Error> {
        match state.directory {
            PlaceholderDirectory::Descriptors(_) => {
                let position = fd_directory::seek(state.position, offset, whence)?;
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_placeholder(PlaceholderState { position, ..state })
                })?;
                Ok(position as i64)
            }
            PlaceholderDirectory::Root => {
                let current = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.directory_stream_position())?;
                let position = fd_directory::seek(current, offset, whence)?;
                guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.seek_directory_stream(position))?;
                Ok(position as i64)
            }
        }
    }

    /// `fchdir` to placeholder descriptor `fd`, which enters the directory it
    /// stands for as Linux's `fchdir` enters it (`set_fs_pwd`), so that the
    /// working directory, `getcwd`, `/proc/self/cwd` and every name looked
    /// up from it are the directory's own:
    /// - on procfs's root (ptrace): `/proc` itself is the descriptor's own
    ///   object, which the guest's `fchdir` enters (`None`: the kernel answers
    ///   it); a descriptor directory is entered from the descriptor, in every
    ///   state of the caller: one injected `openat` of the directory's path
    ///   within procfs (`<pid>/fd`, `O_PATH`), `fchdir` to it and its
    ///   `close`. Where the caller's table has no free slot for that
    ///   descriptor, one injected `chdir` to the directory's absolute path,
    ///   where Detcore knows that path reaches it (the caller has not changed
    ///   its root and is in no nested PID namespace), and otherwise a refusal
    ///   by name: either way the working directory changes only if the call
    ///   succeeds, as Linux's `fchdir` changes it;
    /// - on the placeholder (KVM): an injected `chdir` to the directory's
    ///   path, which the backend resolves (KVM's executor refuses a working
    ///   directory on procfs with `EACCES`, as it does for the guest's own
    ///   `chdir("/proc")`).
    ///
    /// A descriptor directory whose task is gone (or whose numbers another
    /// task holds now) is one Linux still enters, a directory below which no
    /// lookup finds anything, and whose `getcwd` fails; no object of the
    /// backend's stands for it, so that `fchdir` is refused by name.
    async fn enter_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        directory: &PlaceholderDirectory,
    ) -> Option<Result<i64, Error>> {
        let object = Self::placeholder_object(guest);
        if object == PlaceholderObject::ProcfsRoot && *directory == PlaceholderDirectory::Root {
            return None;
        }
        if !Self::placeholder_current(guest, directory).await {
            return Some(
                self.refuse_placeholder_call(
                    guest,
                    Sysno::fchdir,
                    Errno::ENOENT,
                    &format!(
                        "fchdir to {}, a descriptor directory whose task has exited, which \
                         Linux enters though nothing below it exists; Detcore has no directory \
                         to stand for it",
                        directory.canonical().display()
                    ),
                )
                .await,
            );
        }
        Some(match (object, directory) {
            (PlaceholderObject::ProcfsRoot, PlaceholderDirectory::Descriptors(binding)) => {
                let within = binding.within().into_os_string().into_encoded_bytes();
                let within = &within[1..];
                match Self::inject_openat(
                    guest,
                    fd,
                    within,
                    OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                )
                .await
                {
                    Ok(opened) => {
                        let entered = guest
                            .inject_with_retry(syscalls::Fchdir::new().with_fd(opened))
                            .await;
                        let _ = guest.inject(syscalls::Close::new().with_fd(opened)).await;
                        entered.map_err(Into::into)
                    }
                    Err(Errno::EMFILE | Errno::ENFILE)
                        if Self::descriptor_directory_caller(guest).is_some() =>
                    {
                        let canonical = binding.canonical();
                        Self::inject_chdir(guest, canonical.as_os_str().as_bytes())
                            .await
                            .map_err(Into::into)
                    }
                    Err(Errno::EMFILE | Errno::ENFILE) => {
                        self.refuse_placeholder_call(
                            guest,
                            Sysno::fchdir,
                            Errno::EMFILE,
                            &format!(
                                "fchdir to {}, which Detcore enters through one more \
                                 descriptor, while the caller's table has no free slot for it \
                                 and its root or PID namespace keeps the directory's absolute \
                                 path from naming it",
                                binding.canonical().display()
                            ),
                        )
                        .await
                    }
                    Err(errno) => Err(errno.into()),
                }
            }
            _ => {
                let canonical = directory.canonical();
                Self::inject_chdir(guest, canonical.as_os_str().as_bytes())
                    .await
                    .map_err(Into::into)
            }
        })
    }

    /// One injected `chdir(path)`, with the path staged on Reverie's guest
    /// stack.
    async fn inject_chdir<G: Guest<Self>>(guest: &mut G, path: &[u8]) -> Result<i64, Errno> {
        const PATH_CAPACITY: usize = 64;
        if path.len() >= PATH_CAPACITY {
            return Err(Errno::ENAMETOOLONG);
        }
        let mut stack = guest.stack().await;
        let staged = stack.push(super::rendered_procfs::c_string::<PATH_CAPACITY>(path));
        let stack_guard = stack.commit().map_err(|_| Errno::EFAULT)?;
        let entered = guest
            .inject_with_retry(
                syscalls::Chdir::new()
                    .with_path(PathPtr::from_ptr(staged.as_raw() as *const libc::c_char)),
            )
            .await;
        drop(stack_guard);
        entered
    }

    /// `fcntl` through a placeholder descriptor (see
    /// [`Self::answer_placeholder_call`]), as `do_fcntl` answers it for the
    /// directory, or `None` for a command that reaches the descriptor
    /// (v6.17 `fs/fcntl.c`):
    /// - an `O_PATH` descriptor takes only `F_DUPFD`, `F_DUPFD_CLOEXEC`,
    ///   `F_DUPFD_QUERY`, `F_GETFD`, `F_SETFD`, `F_GETFL` and
    ///   `F_CREATED_QUERY` (`check_fcntl_cmd`), every other `EBADF`;
    /// - `F_GETFL`: the description's status flags; `F_SETFL` (`setfl`):
    ///   setting `O_NOATIME` needs the directory's owner or one who may act
    ///   as it (`EPERM` on global root's `/proc`), `O_DIRECT` is `EINVAL` (no
    ///   direct I/O), and only `SETFL_MASK` is kept, which `O_ASYNC` is not
    ///   (a directory has no `fasync`: the call succeeds, the flag stays as
    ///   the open left it);
    /// - `F_DUPFD_QUERY`: whether another descriptor names the same
    ///   description (`EBADF` for one not open); `F_CREATED_QUERY`: 0, the
    ///   open created no directory;
    /// - the write hints are the directory's inode's (`i_write_hint`), kept
    ///   for every open of it (`procfs::locks::LockLedger::
    ///   placeholder_write_hint`): `F_GET_RW_HINT` writes it, 0 until set;
    ///   `F_SET_RW_HINT` needs the owner (`EPERM`) before it reads the hint
    ///   (`EFAULT`) and checks it (`EINVAL` past 5);
    /// - the record locks are refused by name (`ENOLCK`: named residual); a
    ///   lease is an invalid type's `EINVAL`, then `EACCES` without the
    ///   owner (global root's `/proc`), then `EINVAL` for a directory;
    ///   `F_GETLEASE` is `F_UNLCK`; `F_ADD_SEALS` `EPERM` (not writable),
    ///   `F_GET_SEALS` `EINVAL`; the pipe sizes `EBADF`; `F_NOTIFY` 0, with no
    ///   event ever sent (named residual); any other command `EINVAL`.
    async fn placeholder_fcntl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        state: PlaceholderState,
        call: &syscalls::Fcntl,
    ) -> Option<Result<i64, Error>> {
        let (_, args) = Syscall::Fcntl(*call).into_parts();
        let command = args.arg1 as libc::c_int;
        Some(match command {
            libc::F_GETFL => {
                let status = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.status_flags())
                    .unwrap_or(libc::O_DIRECTORY);
                Ok(i64::from(status & !libc::O_CLOEXEC))
            }
            // The table commands, which the kernel's descriptor table answers
            // for every file, `O_PATH` ones included.
            libc::F_DUPFD | libc::F_DUPFD_CLOEXEC | libc::F_GETFD | libc::F_SETFD => return None,
            // Whether another descriptor names this one's description
            // (`f_dupfd_query`, which takes an `O_PATH` one too): `EBADF` for
            // one that is not open.
            F_DUPFD_QUERY => {
                let other = args.arg2 as i32;
                let mine = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.open_file_id())
                    .ok();
                match guest
                    .thread_state()
                    .with_detfd(other, |detfd| detfd.open_file_id())
                {
                    Ok(theirs) => Ok(i64::from(Some(theirs) == mine)),
                    Err(_) if other < 0 => Err(Errno::EBADF.into()),
                    Err(_) => match guest
                        .inject(
                            syscalls::Fcntl::new()
                                .with_fd(other)
                                .with_cmd(FcntlCmd::F_GETFD),
                        )
                        .await
                    {
                        Ok(_) => Ok(0),
                        Err(errno) => Err(errno.into()),
                    },
                }
            }
            // A directory the open did not create (`FMODE_CREATED`).
            F_CREATED_QUERY => Ok(0),
            // `check_fcntl_cmd`: no other command is allowed on `O_PATH`.
            _ if state.path_only => Err(Errno::EBADF.into()),
            libc::F_SETFL => {
                // `setfl` keeps only `SETFL_MASK`.
                const SETFL_MASK: i32 =
                    libc::O_APPEND | libc::O_NONBLOCK | libc::O_DIRECT | libc::O_NOATIME;
                let requested = args.arg2 as i32;
                let status = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.status_flags())
                    .unwrap_or(libc::O_DIRECTORY);
                if requested & libc::O_NOATIME != 0
                    && status & libc::O_NOATIME == 0
                    && !match placeholder_owner_or_capable(&state.directory) {
                        Ok(owner) => owner,
                        Err(errno) => return Some(Err(errno.into())),
                    }
                {
                    Err(Errno::EPERM.into())
                } else if requested & libc::O_DIRECT != 0 {
                    Err(Errno::EINVAL.into())
                } else {
                    let _ = guest.thread_state().with_detfd(fd, |detfd| {
                        detfd.set_status_flags((status & !SETFL_MASK) | (requested & SETFL_MASK));
                    });
                    Ok(0)
                }
            }
            // The owner and signal commands, which act on the description
            // alike for every file.
            libc::F_SETOWN
            | libc::F_GETOWN
            | F_SETSIG
            | F_GETSIG
            | F_SETOWN_EX
            | F_GETOWN_EX
            | F_GETOWNER_UIDS => return None,
            // Record locks on the object would conflict across directories
            // (on procfs's root every placeholder is one inode), where Linux's
            // inodes are each the directory's own: refused by name (named
            // residual).
            libc::F_GETLK
            | libc::F_SETLK
            | libc::F_SETLKW
            | libc::F_OFD_GETLK
            | libc::F_OFD_SETLK
            | libc::F_OFD_SETLKW => {
                warn!(
                    "[detcore] fcntl({fd}, {command}), a record lock on a /proc directory \
                     Detcore serves, is refused: ENOLCK"
                );
                self.refuse_unserviceable_operation(guest, Sysno::fcntl, Errno::ENOLCK)
                    .await
            }
            // A lease of an invalid type first (`lease_alloc`); then the
            // file's owner, or CAP_LEASE in the initial user namespace, which
            // no guest in a user namespace has (`vfs_setlease`); then a
            // regular file.
            libc::F_SETLEASE
                if !matches!(
                    args.arg2 as libc::c_int,
                    libc::F_RDLCK | libc::F_WRLCK | libc::F_UNLCK
                ) =>
            {
                Err(Errno::EINVAL.into())
            }
            libc::F_SETLEASE => match placeholder_lease_owner(&state.directory) {
                Ok(true) => Err(Errno::EINVAL.into()),
                Ok(false) => Err(Errno::EACCES.into()),
                Err(errno) => Err(errno.into()),
            },
            libc::F_GETLEASE => Ok(i64::from(libc::F_UNLCK)),
            // Adding seals needs a writable descriptor, before Linux looks
            // for seals the file cannot have.
            libc::F_ADD_SEALS => Err(Errno::EPERM.into()),
            libc::F_GET_SEALS => Err(Errno::EINVAL.into()),
            // Not a pipe (`get_pipe_info`).
            libc::F_SETPIPE_SZ | libc::F_GETPIPE_SZ => Err(Errno::EBADF.into()),
            // dnotify takes the directory; Detcore sends no event.
            libc::F_NOTIFY => Ok(0),
            F_GET_RW_HINT => {
                let hint = crate::tool_global::placeholder_write_hint(
                    guest,
                    state.directory.lock_key(),
                    None,
                )
                .await;
                match AddrMut::<u64>::from_raw(args.arg2) {
                    Some(address) => guest
                        .memory()
                        .write_value(address, &hint)
                        .map(|()| 0)
                        .map_err(Into::into),
                    None => Err(Errno::EFAULT.into()),
                }
            }
            F_SET_RW_HINT => {
                match placeholder_owner_or_capable(&state.directory) {
                    Ok(true) => {}
                    Ok(false) => return Some(Err(Errno::EPERM.into())),
                    Err(errno) => return Some(Err(errno.into())),
                }
                match reverie::syscalls::Addr::<u64>::from_raw(args.arg2)
                    .map(|hint| guest.memory().read_value(hint))
                {
                    Some(Ok(hint)) if hint <= RWH_WRITE_LIFE_EXTREME => {
                        crate::tool_global::placeholder_write_hint(
                            guest,
                            state.directory.lock_key(),
                            Some(hint),
                        )
                        .await;
                        Ok(0)
                    }
                    Some(Ok(_)) => Err(Errno::EINVAL.into()),
                    _ => Err(Errno::EFAULT.into()),
                }
            }
            _ => Err(Errno::EINVAL.into()),
        })
    }

    /// `ioctl` through placeholder descriptor `fd`
    /// (see [`Self::answer_placeholder_call`]), as `do_vfs_ioctl` answers it
    /// for the directory, or `None` for a command that reaches the descriptor
    /// (v6.17 `fs/ioctl.c`):
    /// - `FIOCLEX` and `FIONCLEX` reach the descriptor's table slot;
    /// - `FIONBIO` reads its flag (`EFAULT`) and sets or clears
    ///   `O_NONBLOCK` in the description's status flags; `FIOASYNC` reads its
    ///   flag, then a change of the description's `O_ASYNC` needs a `fasync`
    ///   the directory has not (`ENOTTY`), and no change is 0;
    /// - `FIGETBSZ` procfs's 1024; `FIOQSIZE` 0;
    /// - `FIFREEZE` and `FITHAW` need `CAP_SYS_ADMIN` in procfs's user
    ///   namespace, the guest's (`EPERM`), then procfs cannot freeze
    ///   (`EOPNOTSUPP`) and is not frozen (`thaw_super`'s `EINVAL`);
    ///   `FS_IOC_FIEMAP` `EOPNOTSUPP`;
    /// - `FICLONE` and `FICLONERANGE` (whose argument is read first): the
    ///   source descriptor (`EBADF`), then another filesystem's (`EXDEV`),
    ///   then a directory (`EISDIR`); `FIDEDUPERANGE`: its destination count
    ///   (`EFAULT`), a request past a page (`ENOMEM`), the request
    ///   (`EFAULT`), the reserved fields (`EINVAL`), then this directory
    ///   (`EISDIR`);
    /// - `FS_IOC_SETFLAGS` reads its argument (`EFAULT`); it and every other
    ///   command, `FS_IOC_GETFLAGS` among them, are `ENOTTY`: procfs has no
    ///   `fileattr_get` and directories no `unlocked_ioctl`.
    async fn placeholder_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        command: u32,
        argument: usize,
    ) -> Option<Result<i64, Errno>> {
        const FIONBIO: usize = 0x5421;
        const FIOASYNC: usize = 0x5452;
        const FIGETBSZ: usize = 2;
        const FIOQSIZE: usize = 0x5460;
        const FIFREEZE: usize = 0xc004_5877;
        const FITHAW: usize = 0xc004_5878;
        const FS_IOC_FIEMAP: usize = 0xc020_660b;
        const FICLONE: usize = 0x4004_9409;
        const FICLONERANGE: usize = 0x4020_940d;
        const FIDEDUPERANGE: usize = 0xc018_9436;
        const FS_IOC_SETFLAGS: usize = 0x4008_6602;
        const CAP_SYS_ADMIN: u32 = 21;
        let flag = |guest: &G| -> Result<bool, Errno> {
            let address =
                reverie::syscalls::Addr::<libc::c_int>::from_raw(argument).ok_or(Errno::EFAULT)?;
            guest
                .memory()
                .read_value(address)
                .map(|value| value != 0)
                .map_err(|_| Errno::EFAULT)
        };
        Some(match command as usize {
            // FIONCLEX and FIOCLEX, on the descriptor's table slot.
            0x5450 | 0x5451 => return None,
            FIONBIO => flag(guest).and_then(|on| {
                guest
                    .thread_state()
                    .with_detfd(fd, |detfd| {
                        let status = detfd.status_flags();
                        detfd.set_status_flags(if on {
                            status | libc::O_NONBLOCK
                        } else {
                            status & !libc::O_NONBLOCK
                        });
                    })
                    .map(|()| 0)
            }),
            FIOASYNC => flag(guest).and_then(|on| {
                let asynchronous = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.status_flags() & libc::O_ASYNC != 0)
                    .unwrap_or(false);
                if on == asynchronous {
                    Ok(0)
                } else {
                    Err(Errno::ENOTTY)
                }
            }),
            FIGETBSZ => match AddrMut::<libc::c_int>::from_raw(argument) {
                Some(address) => guest
                    .memory()
                    .write_value(address, &(procfs_inode::PROC_BLOCK_SIZE as libc::c_int))
                    .map(|()| 0),
                None => Err(Errno::EFAULT),
            },
            // The bytes the directory uses: none.
            FIOQSIZE => match AddrMut::<libc::loff_t>::from_raw(argument) {
                Some(address) => guest.memory().write_value(address, &0).map(|()| 0),
                None => Err(Errno::EFAULT),
            },
            FIFREEZE | FITHAW => {
                let effective = match Self::inject_capget(
                    guest,
                    super::rendered_procfs::CapabilitySet::Effective,
                )
                .await
                {
                    Ok(effective) => effective.unwrap_or(0),
                    Err(_) => 0,
                };
                if effective & (1 << CAP_SYS_ADMIN) == 0 {
                    Err(Errno::EPERM)
                } else if command as usize == FIFREEZE {
                    Err(Errno::EOPNOTSUPP)
                } else {
                    Err(Errno::EINVAL)
                }
            }
            FS_IOC_FIEMAP => Err(Errno::EOPNOTSUPP),
            FICLONE => self.placeholder_clone_answer(guest, argument as i32).await,
            // The whole `struct file_clone_range` is read first.
            FICLONERANGE => match reverie::syscalls::Addr::<[u64; 4]>::from_raw(argument)
                .map(|address| guest.memory().read_value(address))
            {
                Some(Ok(range)) => self.placeholder_clone_answer(guest, range[0] as i32).await,
                _ => Err(Errno::EFAULT),
            },
            FIDEDUPERANGE => {
                // `struct file_dedupe_range`: src_offset, src_length,
                // dest_count (u16), reserved1 (u16), reserved2 (u32), then
                // dest_count 32-byte `struct file_dedupe_range_info`.
                let count = (argument != 0)
                    .then(|| reverie::syscalls::Addr::<u16>::from_raw(argument.wrapping_add(16)))
                    .flatten()
                    .ok_or(Errno::EFAULT)
                    .and_then(|address| {
                        guest
                            .memory()
                            .read_value(address)
                            .map_err(|_| Errno::EFAULT)
                    });
                match count {
                    Err(errno) => Err(errno),
                    Ok(count) if 24 + 32 * usize::from(count) > 4096 => Err(Errno::ENOMEM),
                    Ok(count) => {
                        let mut request = vec![0u8; 24 + 32 * usize::from(count)];
                        match reverie::syscalls::Addr::<u8>::from_raw(argument)
                            .map(|address| guest.memory().read_exact(address, &mut request))
                        {
                            Some(Ok(())) if request[18..24].iter().any(|byte| *byte != 0) => {
                                Err(Errno::EINVAL)
                            }
                            Some(Ok(())) => Err(Errno::EISDIR),
                            _ => Err(Errno::EFAULT),
                        }
                    }
                }
            }
            FS_IOC_SETFLAGS => flag(guest).and(Err(Errno::ENOTTY)),
            // Every other command, FS_IOC_GETFLAGS and FS_IOC_FSGETXATTR
            // among them: procfs has no `fileattr_get` and directories no
            // `unlocked_ioctl`.
            _ => Err(Errno::ENOTTY),
        })
    }

    /// Linux's answer to cloning descriptor `source` into a directory on the
    /// guest's procfs (`ioctl_file_clone`, `vfs_clone_file_range`): `EBADF`
    /// for no such descriptor or an `O_PATH` one, `EXDEV` for one on another
    /// filesystem (checked first), else `EISDIR` (`generic_file_rw_checks`):
    /// a placeholder, a procfs file Detcore renders, or a descriptor on the
    /// guest's procfs device.
    async fn placeholder_clone_answer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        source: RawFd,
    ) -> Result<i64, Errno> {
        let facts = self.descriptor_facts(guest, source).await?;
        if facts.path_only {
            return Err(Errno::EBADF);
        }
        if facts.on_procfs {
            Err(Errno::EISDIR)
        } else {
            Err(Errno::EXDEV)
        }
    }

    /// What Linux's checks of a call with a second descriptor operand need of
    /// descriptor `fd`: `EBADF` where it is not open; else its access mode,
    /// `O_PATH`, `O_APPEND`, whether it is a pipe, and whether it is on the
    /// guest's procfs (a placeholder, a procfs file Detcore renders, or a
    /// descriptor on the guest's procfs device). A placeholder's come from
    /// Detcore's model; any other descriptor's from one injected
    /// `fcntl(F_GETFL)` and one injected `fstat`.
    async fn descriptor_facts<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<DescriptorFacts, Errno> {
        if fd < 0 {
            return Err(Errno::EBADF);
        }
        if let Some(state) = Self::placeholder_of(guest, fd) {
            let status = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.status_flags())
                .unwrap_or(libc::O_DIRECTORY);
            return Ok(DescriptorFacts {
                readable: !state.path_only,
                writable: false,
                path_only: state.path_only,
                append: status & libc::O_APPEND != 0,
                pipe: false,
                on_procfs: true,
                placeholder: Some(state),
            });
        }
        let status = guest
            .inject(
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(FcntlCmd::F_GETFL),
            )
            .await? as i32;
        let rendered = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_rendered_path().is_some())
            .unwrap_or(false);
        let stat = if status & libc::O_PATH == 0 {
            self.inject_fstat(guest, fd).await.ok()
        } else {
            None
        };
        let accmode = status & libc::O_ACCMODE;
        Ok(DescriptorFacts {
            readable: status & libc::O_PATH == 0 && accmode != libc::O_WRONLY,
            writable: status & libc::O_PATH == 0 && accmode != libc::O_RDONLY,
            path_only: status & libc::O_PATH != 0,
            append: status & libc::O_APPEND != 0,
            pipe: stat.is_some_and(|stat| stat.st_mode & libc::S_IFMT == libc::S_IFIFO),
            on_procfs: rendered
                || stat.is_some_and(|stat| stat.st_dev == Self::procfs_key_device(guest)),
            placeholder: None,
        })
    }

    /// `flock` through a placeholder descriptor (see
    /// [`Self::answer_placeholder_call`]): each directory is locked apart
    /// (`procfs::locks::LockLedger::placeholder_flock`), the description
    /// owning the lock as on Linux, and losing it with its last reference. A
    /// contended blocking request is refused as `Detcore::handle_flock`
    /// refuses one, and so is a lock asked for by a process whose descriptor
    /// tables the ledger cannot tell apart (named residual).
    async fn placeholder_flock<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        state: PlaceholderState,
        operation: i32,
    ) -> Result<i64, Error> {
        use crate::procfs::locks::FlockMode;
        use crate::procfs::locks::PlaceholderFlockOutcome;
        let mode = match operation & !libc::LOCK_NB {
            libc::LOCK_SH => Some(FlockMode::Shared),
            libc::LOCK_EX => Some(FlockMode::Exclusive),
            libc::LOCK_UN => None,
            _ => return Err(Errno::EINVAL.into()),
        };
        // An `O_PATH` descriptor, after the command (`flock_translate_cmd`).
        if state.path_only {
            return Err(Errno::EBADF.into());
        }
        let open_file = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_lock_tracked();
            detfd.open_file_id()
        })?;
        let slots = guest.thread_state().slots_naming(open_file);
        let outcome = crate::tool_global::placeholder_flock(
            guest,
            crate::procfs::locks::PlaceholderFlock {
                key: state.directory.lock_key(),
                open_file,
                slots,
                mode,
                nonblocking: operation & libc::LOCK_NB != 0,
            },
        )
        .await;
        match outcome {
            PlaceholderFlockOutcome::Done => Ok(0),
            PlaceholderFlockOutcome::WouldBlock => Err(Errno::EWOULDBLOCK.into()),
            PlaceholderFlockOutcome::Contended => {
                let dettid = guest.thread_state().dettid;
                super::report_flock_refusal(&format!(
                    "[dtid {dettid}] blocking flock(fd={fd}, operation={operation:#x}) of a /proc \
                     directory Detcore serves refused: another open file holds a conflicting \
                     lock. {}",
                    super::FLOCK_REFUSAL_ADVICE
                ));
                self.refuse_unserviceable_operation(guest, Sysno::flock, Errno::ENOLCK)
                    .await
            }
            PlaceholderFlockOutcome::Unfollowable => {
                self.refuse_placeholder_call(
                    guest,
                    Sysno::flock,
                    Errno::ENOLCK,
                    &format!(
                        "flock(fd={fd}) of {}, a /proc directory Detcore serves, where a \
                         process that has had a thread with a descriptor table of its own \
                         holds the lock or asks for it: Detcore cannot follow that process's \
                         references to the lock's open file",
                        state.directory.canonical().display()
                    ),
                )
                .await
            }
            PlaceholderFlockOutcome::InFlight => {
                self.refuse_placeholder_call(
                    guest,
                    Sysno::flock,
                    Errno::ENOLCK,
                    &format!(
                        "flock(fd={fd}) of {}, a /proc directory Detcore serves, while a copy of \
                         its open file is in flight over SCM_RIGHTS, a reference Detcore does \
                         not follow",
                        state.directory.canonical().display()
                    ),
                )
                .await
            }
        }
    }

    /// The extended-attribute calls through a placeholder descriptor:
    /// procfs's answers (`rendered_procfs::placeholder_xattr_answer`), as
    /// for a file global root owns on `/proc`, and for one the caller owns
    /// on its descriptor directory. An `O_PATH` descriptor is `EBADF` after
    /// the arguments Linux reads first (the flags, the name, the size and the
    /// value: `filename_setxattr`'s `setxattr_copy`, `import_xattr_name`).
    async fn placeholder_xattr<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
        state: PlaceholderState,
    ) -> Result<i64, Error> {
        let owned = match state.directory {
            PlaceholderDirectory::Root => None,
            PlaceholderDirectory::Descriptors(binding) => Some(binding.pid),
        };
        if state.path_only {
            return Err(xattr_arguments_answer(&guest.memory(), call)
                .err()
                .unwrap_or(Errno::EBADF)
                .into());
        }
        self.placeholder_xattr_answer(guest, call, owned).await
    }

    /// The `utimensat` of a placeholder itself, with no path (`futimens`) or
    /// an empty one and `AT_EMPTY_PATH`, in Linux's order (`utimensat`,
    /// `do_utimes_fd` or `do_utimes_path`, `vfs_utimes`, `notify_change`):
    /// both times omitted is 0 before anything else is looked at; then the
    /// flags (`futimens` takes none), an `O_PATH` descriptor without a path
    /// (`EBADF`), the times' nanoseconds (`EINVAL`); then, on global root's
    /// `/proc`, explicit times need its owner (`EPERM`, `setattr_prepare`)
    /// and the current time write access (`EACCES`, `may_setattr`); the
    /// caller's own descriptor directory is its own (0, the times kept no
    /// further: named).
    async fn placeholder_utimensat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        state: PlaceholderState,
        futimens: bool,
        times: usize,
        flags: i32,
    ) -> Result<i64, Error> {
        const UTIME_NOW: i64 = (1 << 30) - 1;
        const UTIME_OMIT: i64 = (1 << 30) - 2;
        let times = match reverie::syscalls::Addr::<[libc::timespec; 2]>::from_raw(times) {
            None => None,
            Some(address) => Some(guest.memory().read_value(address)?),
        };
        if times.is_some_and(|times| times.iter().all(|time| time.tv_nsec == UTIME_OMIT)) {
            return Ok(0);
        }
        if futimens && flags != 0
            || !futimens && flags & !(libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) != 0
        {
            return Err(Errno::EINVAL.into());
        }
        if futimens && state.path_only {
            return Err(Errno::EBADF.into());
        }
        let valid = |nsec: i64| {
            nsec == UTIME_NOW || nsec == UTIME_OMIT || (0..1_000_000_000).contains(&nsec)
        };
        if times.is_some_and(|times| times.iter().any(|time| !valid(time.tv_nsec))) {
            return Err(Errno::EINVAL.into());
        }
        if state.directory != PlaceholderDirectory::Root {
            return Ok(0);
        }
        match times {
            Some(times) if times.iter().any(|time| time.tv_nsec != UTIME_NOW) => {
                Err(Errno::EPERM.into())
            }
            _ => Err(Errno::EACCES.into()),
        }
    }

    /// The path-less `faccessat2` of a placeholder: `/proc`'s mode (0555)
    /// grants no write, and global root's directory no override; the
    /// caller's own descriptor directory is open to it
    /// (`proc_fd_permission`).
    fn placeholder_access(
        &self,
        directory: &PlaceholderDirectory,
        mode: i32,
        flags: i32,
    ) -> Result<i64, Errno> {
        if mode & !(libc::R_OK | libc::W_OK | libc::X_OK) != 0
            || flags & !(libc::AT_EACCESS | libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) != 0
        {
            return Err(Errno::EINVAL);
        }
        if *directory == PlaceholderDirectory::Root && mode & libc::W_OK != 0 {
            return Err(Errno::EACCES);
        }
        Ok(0)
    }

    /// `sendfile`, `splice`, `tee` and `copy_file_range` with a placeholder
    /// descriptor on either side, each in Linux's order of checks (v6.17
    /// `fs/read_write.c`, `fs/splice.c`), the other descriptor's facts from
    /// [`Self::descriptor_facts`]. A directory has no `splice_read` and cannot
    /// be written or copied. `None` where neither operand is a placeholder.
    /// - `copy_file_range`: both descriptors (`EBADF`), both offsets
    ///   (`EFAULT`), the flags (`EINVAL`), then a directory on either side
    ///   (`EISDIR`), whatever the length;
    /// - `sendfile`: the offset (`EFAULT`); the input (`EBADF`, not readable
    ///   `EBADF`, an offset into a pipe `ESPIPE`, a count past `SSIZE_MAX` or
    ///   a negative position `EINVAL`); the output (`EBADF`, not writable
    ///   `EBADF`); a directory's position past procfs's limit (`EOVERFLOW`),
    ///   an appending output (`EINVAL`); then a count of 0 is 0 and any other
    ///   `EINVAL`;
    /// - `splice`: a length of 0 is 0 before anything; the flags (`EINVAL`);
    ///   both descriptors (`EBADF`); an offset given for a pipe (`ESPIPE`);
    ///   the offsets (`EFAULT`); the input not readable or the output not
    ///   writable (`EBADF`); then `EINVAL`;
    /// - `tee`: the flags (`EINVAL`), a length of 0 (0), both descriptors
    ///   (`EBADF`), the input not readable or the output not writable
    ///   (`EBADF`), then not two pipes (`EINVAL`).
    ///
    /// Each flags argument is read at its Linux type's width, as the kernel
    /// reads the register (`unsigned int` for `copy_file_range`, `splice`
    /// and `tee`).
    ///
    /// A transfer from a placeholder into a pipe (`sendfile` to a pipe, or
    /// `splice` with a pipe output), past the checks above, is refused by
    /// name: Linux makes it only once the pipe has a reader and room
    /// (`splice_file_to_pipe`'s `wait_for_space`: `SIGPIPE` and `EPIPE`
    /// without a reader, a wait while it is full), which Detcore does not
    /// model, and only then fails for the directory (`EINVAL`).
    async fn answer_placeholder_transfer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        args: &syscalls::SyscallArgs,
    ) -> Option<Result<i64, Error>> {
        const SPLICE_F_ALL: u32 = 0xf;
        let (input_fd, output_fd) = match sysno {
            Sysno::sendfile => (args.arg1 as i32, args.arg0 as i32),
            Sysno::splice | Sysno::copy_file_range => (args.arg0 as i32, args.arg2 as i32),
            _ => (args.arg0 as i32, args.arg1 as i32),
        };
        if Self::placeholder_of(guest, input_fd).is_none()
            && Self::placeholder_of(guest, output_fd).is_none()
        {
            return None;
        }
        let readable_offset = |guest: &G, address: usize| -> Result<Option<i64>, Errno> {
            if address == 0 {
                return Ok(None);
            }
            let address = reverie::syscalls::Addr::<i64>::from_raw(address).ok_or(Errno::EFAULT)?;
            guest
                .memory()
                .read_value(address)
                .map(Some)
                .map_err(|_| Errno::EFAULT)
        };
        // `Ok(None)`: a transfer into a pipe, refused by name below.
        let answer: Result<Option<i64>, Errno> = async {
            match sysno {
                Sysno::copy_file_range => {
                    let input = self.descriptor_facts(guest, input_fd).await?;
                    let output = self.descriptor_facts(guest, output_fd).await?;
                    if input.path_only || output.path_only {
                        return Err(Errno::EBADF);
                    }
                    readable_offset(guest, args.arg1)?;
                    readable_offset(guest, args.arg3)?;
                    if args.arg5 as u32 != 0 {
                        return Err(Errno::EINVAL);
                    }
                    Err(Errno::EISDIR)
                }
                Sysno::sendfile => {
                    let offset = readable_offset(guest, args.arg2)?;
                    let input = self.descriptor_facts(guest, input_fd).await?;
                    if input.path_only || !input.readable {
                        return Err(Errno::EBADF);
                    }
                    if offset.is_some() && input.pipe {
                        return Err(Errno::ESPIPE);
                    }
                    let count = args.arg3;
                    let position = match (offset, input.placeholder) {
                        (Some(offset), _) => offset,
                        (None, Some(state)) => self.placeholder_position(guest, input_fd, state),
                        (None, None) => 0,
                    };
                    if isize::try_from(count).is_err() || position < 0 {
                        return Err(Errno::EINVAL);
                    }
                    let output = self.descriptor_facts(guest, output_fd).await?;
                    if output.path_only || !output.writable {
                        return Err(Errno::EBADF);
                    }
                    if input.placeholder.is_some() && position >= PROCFS_MAX_BYTES {
                        return Err(Errno::EOVERFLOW);
                    }
                    if output.pipe && input.placeholder.is_some() {
                        return Ok(None);
                    }
                    if !output.pipe && output.append {
                        return Err(Errno::EINVAL);
                    }
                    if count == 0 {
                        Ok(Some(0))
                    } else {
                        Err(Errno::EINVAL)
                    }
                }
                Sysno::splice => {
                    if args.arg4 == 0 {
                        return Ok(Some(0));
                    }
                    if args.arg5 as u32 & !SPLICE_F_ALL != 0 {
                        return Err(Errno::EINVAL);
                    }
                    let input = self.descriptor_facts(guest, input_fd).await?;
                    let output = self.descriptor_facts(guest, output_fd).await?;
                    if input.path_only || output.path_only {
                        return Err(Errno::EBADF);
                    }
                    if (input.pipe && args.arg1 != 0) || (output.pipe && args.arg3 != 0) {
                        return Err(Errno::ESPIPE);
                    }
                    readable_offset(guest, args.arg1)?;
                    readable_offset(guest, args.arg3)?;
                    if !input.readable || !output.writable {
                        return Err(Errno::EBADF);
                    }
                    if output.pipe && input.placeholder.is_some() {
                        return Ok(None);
                    }
                    Err(Errno::EINVAL)
                }
                _ => {
                    if args.arg3 as u32 & !SPLICE_F_ALL != 0 {
                        return Err(Errno::EINVAL);
                    }
                    if args.arg2 == 0 {
                        return Ok(Some(0));
                    }
                    let input = self.descriptor_facts(guest, input_fd).await?;
                    let output = self.descriptor_facts(guest, output_fd).await?;
                    if input.path_only || output.path_only || !input.readable || !output.writable {
                        return Err(Errno::EBADF);
                    }
                    Err(Errno::EINVAL)
                }
            }
        }
        .await;
        Some(match answer {
            Ok(Some(value)) => Ok(value),
            Err(errno) => Err(errno.into()),
            Ok(None) => {
                self.refuse_placeholder_call(
                    guest,
                    sysno,
                    Errno::EINVAL,
                    &format!(
                        "{sysno} from descriptor {input_fd}, a /proc directory Detcore serves, \
                         into the pipe {output_fd}: Linux makes the transfer only once the pipe \
                         has a reader and room (SIGPIPE and EPIPE without a reader, a wait while \
                         it is full), which Detcore does not model"
                    ),
                )
                .await
            }
        })
    }

    /// The guest's position in the directory placeholder descriptor `fd`
    /// stands for: a descriptor directory's, or `/proc`'s directory stream's.
    fn placeholder_position<G: Guest<Self>>(
        &self,
        guest: &G,
        fd: RawFd,
        state: PlaceholderState,
    ) -> i64 {
        match state.directory {
            PlaceholderDirectory::Descriptors(_) => state.position as i64,
            PlaceholderDirectory::Root => guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.directory_stream_position())
                .map_or(0, |position| position as i64),
        }
    }

    /// Whether [`Self::respelled_below_descriptor_directory`] may respell
    /// anything in `call`: it looks a path up, a placeholder has been opened
    /// or recognized in this tool process (until then no path can lead below
    /// one), and Detcore serves placeholders. Asked before each injection that
    /// carries the guest's paths, so it reads nothing from the guest.
    pub(crate) fn may_respell_below_descriptor_directory<G: Guest<Self>>(
        guest: &G,
        call: &Syscall,
    ) -> bool {
        let sysno = call.number();
        (!relative_operands(sysno).is_empty() || !absolute_operands(sysno).is_empty())
            && OPENED_ANY.load(Ordering::SeqCst)
            && Self::serves_placeholders(guest)
    }

    /// The path operands of `call` that lead below a placeholder directory,
    /// each respelled so that the kernel beneath the backend resolves it as
    /// Linux resolves it below the directory the placeholder stands for
    /// ([`Self::respelled_lookup`]). The caller stages the respelled paths
    /// for one injection (`Detcore::procfs_translated_call`), before anything
    /// else runs. A stale descriptor directory (its task gone, or its numbers
    /// held by another task) fails the lookup with `ENOENT`, as
    /// `get_proc_task` does; more than `MAXSYMLINKS` links, `ELOOP`; a
    /// respelled path longer than `PATH_MAX`, `ENAMETOOLONG`; an `openat2`
    /// whose resolution is scoped ([`SCOPED_RESOLVE`]), `EXDEV`. Empty when
    /// nothing leads below one.
    pub(crate) async fn respelled_below_descriptor_directory<G: Guest<Self>>(
        guest: &mut G,
        call: &Syscall,
    ) -> Result<Vec<RespelledOperand>, Errno> {
        if !Self::serves_placeholders(guest) {
            return Ok(Vec::new());
        }
        let (sysno, args) = call.into_parts();
        let read_operand = |guest: &G, pointer: usize| -> Option<Vec<u8>> {
            PathPtr::from_ptr(pointer as *const libc::c_char)
                .and_then(|pointer| pointer.read(&guest.memory()).ok())
                .map(|path: PathBuf| path.into_os_string().into_encoded_bytes())
        };
        // `openat2`'s resolution flags, in its `struct open_how`
        // (`{ flags, mode, resolve }`, at least 24 bytes).
        let resolve = if sysno == Sysno::openat2 {
            match reverie::syscalls::Addr::<[u64; 3]>::from_raw(args.arg2) {
                Some(how) if args.arg3 >= 24 => guest
                    .memory()
                    .read_value(how)
                    .map(|how| how[2])
                    .unwrap_or(0),
                _ => 0,
            }
        } else {
            0
        };
        let mut respelled = Vec::new();
        let operands = relative_operands(sysno)
            .iter()
            .map(|&(dirfd, path)| (Some(dirfd), path))
            .chain(absolute_operands(sysno).iter().map(|&path| (None, path)))
            .collect::<Vec<_>>();
        for (dirfd_index, path_index) in operands {
            let pointer = argument(&args, path_index);
            if pointer == 0 {
                continue;
            }
            let Some(path) = read_operand(guest, pointer) else {
                continue;
            };
            if path.is_empty() {
                continue;
            }
            let dirfd = dirfd_index.map(|index| argument(&args, index) as i32);
            let follows = follows_final(sysno, &args, path_index);
            let Some((bytes, absolute)) =
                Self::respelled_lookup(guest, dirfd, &path, follows, resolve).await?
            else {
                continue;
            };
            if resolve & SCOPED_RESOLVE != 0 {
                return Err(Errno::EXDEV);
            }
            if bytes.len() >= PATH_LIMIT {
                return Err(Errno::ENAMETOOLONG);
            }
            respelled.push(RespelledOperand {
                dirfd: if absolute { dirfd_index } else { None },
                path: path_index,
                bytes,
            });
        }
        Ok(respelled)
    }

    /// `path`, looked up from `dirfd` (`None` for a call that looks it up
    /// from the working directory or the root), respelled where it leads
    /// below a placeholder directory, as `Some` of the respelled bytes and
    /// whether they are absolute (looked up from `AT_FDCWD`) or looked up
    /// from where the guest's path was. `follows` says whether the call
    /// follows a final link (a path that is a descriptor link to a
    /// placeholder, and nothing more, then names the directory). By the
    /// placeholder's object ([`PlaceholderObject`]):
    /// - procfs's root (ptrace): the kernel resolves every path through the
    ///   descriptor below procfs, so `/proc` itself needs nothing, and a path
    ///   below a descriptor directory goes on below that directory's own path
    ///   within procfs: relative to the descriptor, `<pid>/fd/<path>`, and
    ///   through a descriptor link to one, the same path after the link
    ///   (`/proc/self/fd/D/<pid>/fd/<rest>`, [`Self::within_after_links`]),
    ///   so that each lookup starts where the guest's does, from the
    ///   descriptor or the guest's own root, in every state of the caller;
    /// - the placeholder (KVM): a path below the descriptor, or through its
    ///   link, at the directory's absolute spelling, with every link counted
    ///   against `MAXSYMLINKS` ([`Self::through_own_links`]).
    ///
    /// A path relative to the working directory, or to a descriptor that is
    /// no placeholder, that leads through a descriptor link to one (`fd/D/N`
    /// from `/proc/self`, `proc/self/fd/D/N` from `/`) is respelled as Detcore
    /// decides about it ([`Self::respelled_through_links_from`]).
    async fn respelled_lookup<G: Guest<Self>>(
        guest: &mut G,
        dirfd: Option<RawFd>,
        path: &[u8],
        follows: bool,
        resolve: u64,
    ) -> Result<Option<(Vec<u8>, bool)>, Errno> {
        let object = Self::placeholder_object(guest);
        if !path.starts_with(b"/") {
            // A call without a directory descriptor looks its path up from the
            // working directory.
            let dirfd = dirfd.unwrap_or(libc::AT_FDCWD);
            // A placeholder Detcore keeps no record of (one received without
            // sequentialized threads) is told at its use.
            let directory = match Self::placeholder_of(guest, dirfd) {
                Some(state) => state.directory,
                None => match Self::unrecorded_placeholder(guest, dirfd).await {
                    Some(directory) => directory,
                    None => {
                        return Self::respelled_through_links_from(guest, dirfd, path, follows)
                            .await;
                    }
                },
            };
            if !Self::placeholder_current(guest, &directory).await {
                return Err(Errno::ENOENT);
            }
            let caller = Self::caller_numbers(guest).ok_or(Errno::ENOENT)?;
            return Ok(match object {
                PlaceholderObject::ProcfsRoot => {
                    let (within, logical) = match &directory {
                        PlaceholderDirectory::Root => (Vec::new(), PathBuf::from("/proc")),
                        PlaceholderDirectory::Descriptors(binding) => {
                            let within = binding.within().into_os_string().into_encoded_bytes();
                            (within[1..].to_vec(), binding.canonical())
                        }
                    };
                    let (rest, changed) =
                        Self::within_after_links(guest, &logical, path, follows, caller);
                    if within.is_empty() {
                        changed.then_some((rest, false))
                    } else {
                        Some((below(Path::new(OsStr::from_bytes(&within)), &rest), false))
                    }
                }
                PlaceholderObject::Placeholder => {
                    // With `RESOLVE_NO_SYMLINKS` the directory is spelled by
                    // number, which crosses no symbolic link of its own.
                    let base = if resolve & libc::RESOLVE_NO_SYMLINKS != 0 {
                        directory.canonical()
                    } else {
                        directory.lookup_base(caller)
                    };
                    let (bytes, _) =
                        Self::through_own_links(guest, below(&base, path), caller, false, follows)
                            .await?;
                    Some((bytes, true))
                }
            });
        }
        let Some(caller) = Self::descriptor_directory_caller(guest) else {
            return Ok(None);
        };
        Ok(match object {
            PlaceholderObject::ProcfsRoot => {
                let (bytes, changed) =
                    Self::within_after_links(guest, Path::new(""), path, follows, caller);
                changed.then_some((bytes, false))
            }
            PlaceholderObject::Placeholder => {
                let (bytes, through) =
                    Self::through_own_links(guest, path.to_vec(), caller, true, follows).await?;
                through.then_some((bytes, true))
            }
        })
    }

    /// `path`, relative, looked up from `dirfd`, the working directory
    /// (`AT_FDCWD`) or a descriptor that is no placeholder, respelled where it
    /// leads through one of the caller's descriptor links to a placeholder
    /// directory, as Detcore decides about it: the path made absolute from
    /// the base's path (`Detcore::observed_path`, the decisions'
    /// [`Self::path_below_descriptor_directory`] and the `--proc` level
    /// table's), then through the links ([`Self::decided_path`]). On ptrace
    /// it stays relative, with the directory's path within procfs inserted
    /// after each such link ([`Self::within_after_links`], from the base);
    /// on KVM it is the absolute spelling ([`Self::through_own_links`]).
    /// `None` where the path leads through no such link, or where Detcore
    /// does not know the base's path (from a descriptor whose recorded path
    /// is a link's spelling, [`Self::starts_at_a_recorded_link`], or one it
    /// keeps no path for), where nothing is decided through the links
    /// either and the kernel looks the guest's path up.
    async fn respelled_through_links_from<G: Guest<Self>>(
        guest: &mut G,
        dirfd: RawFd,
        path: &[u8],
        follows: bool,
    ) -> Result<Option<(Vec<u8>, bool)>, Errno> {
        let Some(caller) = Self::descriptor_directory_caller(guest) else {
            return Ok(None);
        };
        let relative = Path::new(OsStr::from_bytes(path));
        if Self::starts_at_a_recorded_link(guest, dirfd, relative) {
            return Ok(None);
        }
        Self::prepare_guest_cwd(guest, dirfd, relative).await;
        let Some(observed) = Self::observed_path(guest, dirfd, relative)
            .ok()
            .filter(|observed| observed.is_absolute())
        else {
            return Ok(None);
        };
        match Self::placeholder_object(guest) {
            PlaceholderObject::ProcfsRoot => {
                // The base is the absolute path without the guest's relative
                // spelling, which `observed_path` appends to it.
                let observed = observed.as_os_str().as_bytes();
                let Some(base) = observed.strip_suffix(path) else {
                    return Ok(None);
                };
                let base = base.strip_suffix(b"/").unwrap_or(base);
                let base = if base.is_empty() { &b"/"[..] } else { base };
                let (rest, changed) = Self::within_after_links(
                    guest,
                    Path::new(OsStr::from_bytes(base)),
                    path,
                    follows,
                    caller,
                );
                Ok(changed.then_some((rest, false)))
            }
            PlaceholderObject::Placeholder => {
                let (bytes, through) = Self::through_own_links(
                    guest,
                    observed.into_os_string().into_vec(),
                    caller,
                    true,
                    follows,
                )
                .await?;
                Ok(through.then_some((bytes, true)))
            }
        }
    }

    /// `rest`, a path the kernel resolves from the procfs directory
    /// `logical` (absolute, `/proc` or `/proc/<pid>/fd`; empty for an
    /// absolute `rest`), with the path within procfs of each descriptor
    /// directory a descriptor link to a placeholder reaches inserted after
    /// that link (`fd/D/x` from `/proc/self` becomes `fd/D/<pid>/fd/x`), as
    /// the kernel needs it where every placeholder is open on procfs's root
    /// (ptrace): the link leads there, and the rest of the path then goes
    /// on below the directory the placeholder stands for. A link to the
    /// root placeholder needs nothing; a final link that the call does not
    /// follow (`follows`) is the link itself. Each link is told with the
    /// caller's numbers `caller` ([`Self::own_link_directory_of`]); the scan
    /// stops at a link that is no placeholder of the caller's, whose file
    /// only the kernel knows. Returns the path and whether anything was
    /// inserted. The kernel counts every link it follows on the way, as it
    /// does for the guest's own path, which this lengthens by no link.
    fn within_after_links<G: Guest<Self>>(
        guest: &G,
        logical: &Path,
        rest: &[u8],
        follows: bool,
        caller: (i32, i32),
    ) -> (Vec<u8>, bool) {
        let mut logical = logical.as_os_str().as_bytes().to_vec();
        let mut rest = rest;
        let mut out = Vec::with_capacity(rest.len());
        let mut changed = false;
        let mut hops = 0;
        loop {
            let mut candidate = logical.clone();
            if !candidate.is_empty() {
                candidate.push(b'/');
            }
            let offset = candidate.len();
            candidate.extend_from_slice(rest);
            let Some((link, end)) = descriptor_link_prefix(&candidate) else {
                break;
            };
            if end <= offset {
                break;
            }
            let consumed = end - offset;
            let directory = match Self::own_link_directory_of(guest, &link, caller) {
                Some(directory) => directory,
                None => break,
            };
            let tail = &rest[consumed..];
            if tail.is_empty() && !follows {
                break;
            }
            hops += 1;
            if hops > LINK_LIMIT {
                break;
            }
            out.extend_from_slice(&rest[..consumed]);
            rest = tail;
            match directory {
                PlaceholderDirectory::Root => logical = b"/proc".to_vec(),
                PlaceholderDirectory::Descriptors(binding) => {
                    let within = binding.within().into_os_string().into_encoded_bytes();
                    out.extend_from_slice(&within);
                    logical = binding.canonical().into_os_string().into_encoded_bytes();
                    changed = true;
                }
            }
        }
        out.extend_from_slice(rest);
        (out, changed)
    }

    /// `path`, absolute, spelled as Linux resolves it where it leads through
    /// one of the caller's descriptor links to a placeholder directory and on
    /// below it ([`through_descriptor_link`]), or ends at one that the call
    /// follows (`follows`): the directory's absolute spelling joined with
    /// the rest, again for each such link the result leads through, and
    /// whether any did. `ENOENT` for a descriptor directory whose task is
    /// gone, `ELOOP` where the links Linux follows on the way (each
    /// descriptor link, and, where `guest_spelled` says the first one is the
    /// guest's own spelling, the selector links before it,
    /// [`selector_links`]) pass `MAXSYMLINKS`.
    async fn through_own_links<G: Guest<Self>>(
        guest: &mut G,
        mut path: Vec<u8>,
        caller: (i32, i32),
        guest_spelled: bool,
        follows: bool,
    ) -> Result<(Vec<u8>, bool), Errno> {
        let mut hops = 0;
        let mut links = 0;
        loop {
            // A final link the call follows names the directory itself,
            // spelled as the guest spells it.
            let (link, rest) = match through_descriptor_link(&path) {
                Some((link, rest)) => (link, Some(rest)),
                None if follows => match descriptor_link_prefix(&path) {
                    Some((link, end)) if end == path.len() => (link, None),
                    _ => break,
                },
                None => break,
            };
            let Some(directory) = Self::own_link_directory(guest, &link) else {
                break;
            };
            if hops == 0 && guest_spelled {
                links += selector_links(&link);
            }
            hops += 1;
            links += 1;
            if links > LINK_LIMIT {
                return Err(Errno::ELOOP);
            }
            if !Self::placeholder_current(guest, &directory).await {
                return Err(Errno::ENOENT);
            }
            let base = directory.lookup_base(caller);
            path = match rest {
                Some(rest) => below(&base, &rest),
                None => base.into_os_string().into_encoded_bytes(),
            };
        }
        Ok((path, hops > 0))
    }

    /// Whether a lookup of `path` relative to `dirfd` starts at a descriptor
    /// that is no placeholder and whose recorded path is, or walks through,
    /// a descriptor link (`/proc/<pid>/fd/N`, `/dev/fd/N`). An open through a
    /// link records the link's own spelling where the link may reach another
    /// file than the one it reached (another process's link, or a link whose
    /// descriptor a thread replaced while the open waited:
    /// `syscalls::files::path_to_record_for_open`). Linux starts the lookup
    /// at the file that open returned, which no later walk of that spelling
    /// need reach, so Detcore decides nothing about it through the
    /// descriptor links (a FIFO a blocked open returned stays a FIFO, below
    /// which every name is `ENOTDIR`, though its link's number now holds
    /// `/proc`), and the kernel answers it.
    pub(super) fn starts_at_a_recorded_link<G: Guest<Self>>(
        guest: &G,
        dirfd: RawFd,
        path: &Path,
    ) -> bool {
        if dirfd == libc::AT_FDCWD
            || path.is_absolute()
            || Self::placeholder_of(guest, dirfd).is_some()
        {
            return false;
        }
        guest
            .thread_state()
            .with_detfd(dirfd, |detfd| detfd.path())
            .ok()
            .flatten()
            .is_some_and(|base| {
                let mut walked = base.into_os_string().into_vec();
                walked.push(b'/');
                through_descriptor_link(&walked).is_some()
            })
    }

    /// The absolute guest path an open of `path` relative to `dirfd`, whose
    /// guest spelling made absolute is `observed` (`Detcore::observed_path`),
    /// reaches where it leads below a placeholder directory Detcore serves: a
    /// name relative to a placeholder descriptor, or a path through the
    /// caller's descriptor links to one ([`Self::decided_path`]). `None` for
    /// any other path, which names what it spells.
    pub(super) fn path_below_descriptor_directory<G: Guest<Self>>(
        guest: &G,
        dirfd: RawFd,
        path: &Path,
        observed: &Path,
    ) -> Option<PathBuf> {
        Self::descriptor_directory_caller(guest)?;
        if Self::starts_at_a_recorded_link(guest, dirfd, path) {
            return None;
        }
        let relative_below = !path.is_absolute()
            && !path.as_os_str().is_empty()
            && Self::placeholder_of(guest, dirfd).is_some();
        let (decided, _) = Self::decided_path(guest, observed).ok()?;
        (relative_below || decided.as_path() != observed).then_some(decided)
    }

    /// The absolute guest `path` as Detcore decides about it (which
    /// directory an open, a `stat` or a `readlink` names): where it leads
    /// through the caller's descriptor links to placeholder directories,
    /// below each directory's canonical spelling (`/proc/<pid>/fd/rest`,
    /// `/proc/rest`), as Linux resolves it ([`Self::through_own_links`]);
    /// `path` itself otherwise; with the symbolic links Linux follows on the
    /// way (the selector links before the first descriptor link,
    /// [`selector_links`], and each descriptor link), or `ELOOP` where they
    /// pass `MAXSYMLINKS`. The canonical spellings it walks through name
    /// tasks by number, so only the guest's own spelling has selector links
    /// to count. Whether a binding is stale is left to the injection of the
    /// call, which respells the same path.
    pub(super) fn decided_path<G: Guest<Self>>(
        guest: &G,
        path: &Path,
    ) -> Result<(PathBuf, usize), Errno> {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        let mut links = 0;
        let mut first = true;
        while let Some((link, rest)) = through_descriptor_link(&bytes) {
            let Some(directory) = Self::own_link_directory(guest, &link) else {
                break;
            };
            if first {
                links += selector_links(&link);
                first = false;
            }
            links += 1;
            if links > LINK_LIMIT {
                return Err(Errno::ELOOP);
            }
            bytes = below(&directory.canonical(), &rest);
        }
        Ok((PathBuf::from(std::ffi::OsString::from_vec(bytes)), links))
    }

    /// Answer a `readlink` of the absolute guest path `path` into the buffer
    /// `buf` of `buf_len` bytes if it is a descriptor link of the caller's to
    /// a placeholder directory ([`Self::own_link_directory`]): the directory's
    /// path (`/proc/<pid>/fd`, which `proc_fd_link` reads, or `/proc`),
    /// truncated to the buffer and without a NUL, as `do_readlinkat` copies
    /// it. The placeholder's own path never reaches the guest.
    pub(crate) async fn answer_descriptor_directory_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
        buf: Option<AddrMut<'_, libc::c_char>>,
        buf_len: usize,
    ) -> Option<Result<i64, Error>> {
        if !self.serves_descriptor_directories(guest) {
            return None;
        }
        let decided = match Self::decided_path(guest, path) {
            Ok((decided, _)) => decided,
            Err(errno) => return Some(Err(errno.into())),
        };
        let target = Self::own_link_directory(guest, &decided)?.canonical();
        let size = match crate::syscalls::namespace::readlink_size(buf_len) {
            Ok(size) => size,
            Err(errno) => return Some(Err(errno.into())),
        };
        let bytes = target.as_os_str().as_bytes();
        let length = bytes.len().min(size);
        let Some(buf) = buf else {
            return Some(Err(Errno::EFAULT.into()));
        };
        Some(
            guest
                .memory()
                .write_exact(buf.cast::<u8>(), &bytes[..length])
                .map(|()| length as i64)
                .map_err(Into::into),
        )
    }

    /// Whether the absolute guest path `observed`, followed, names a
    /// placeholder directory Detcore serves: `/proc` by any spelling (through
    /// tasks that exist, [`NamedTask`]), one of the caller's own descriptor
    /// directories, or a descriptor link of the caller's to a placeholder, as
    /// `statfs` reaches it.
    pub(super) async fn names_placeholder_directory<G: Guest<Self>>(
        guest: &mut G,
        observed: &Path,
    ) -> bool {
        let Some(own) = Self::descriptor_directory_caller(guest) else {
            return false;
        };
        let Ok((decided, _)) = Self::decided_path(guest, observed) else {
            return false;
        };
        if fd_directory::classify(&decided, own).is_some()
            || Self::own_link_directory(guest, &decided).is_some()
        {
            return true;
        }
        match names_procfs_root(&decided) {
            Some(tasks) => Self::named_tasks_exist(guest, &tasks).await,
            None => false,
        }
    }

    /// The absolute spelling of `path` looked up from `dirfd` where `dirfd`
    /// is a placeholder descriptor ([`below`] its lookup base), for the
    /// `readlinkat` of a name relative to one (`readlinkat(proc, "self/fd/D")`),
    /// whose link Detcore answers by its absolute spelling
    /// ([`Self::answer_descriptor_directory_link`]). `None` for any other
    /// `dirfd`, or an absolute or empty `path`.
    pub(crate) fn spelling_below_placeholder<G: Guest<Self>>(
        guest: &G,
        dirfd: RawFd,
        path: &Path,
    ) -> Option<PathBuf> {
        if !Self::serves_placeholders(guest) || path.is_absolute() || path.as_os_str().is_empty() {
            return None;
        }
        let caller = Self::caller_numbers(guest)?;
        let state = Self::placeholder_of(guest, dirfd)?;
        Some(PathBuf::from(std::ffi::OsString::from_vec(below(
            &state.directory.lookup_base(caller),
            path.as_os_str().as_bytes(),
        ))))
    }
}

/// The path of the pathname `AF_UNIX` address at `address`, `length` bytes
/// long, as `unix_mkname_bsd` takes it: the bytes of `sun_path` up to its
/// first NUL. `None` for no address, another family, an abstract address
/// (a NUL first) or an unnamed one, or one that cannot be read.
fn unix_address_path<M: MemoryAccess>(
    memory: &M,
    address: usize,
    length: usize,
) -> Option<Vec<u8>> {
    let family_size = std::mem::size_of::<libc::sa_family_t>();
    let length = length.min(std::mem::size_of::<libc::sockaddr_un>());
    if address == 0 || length <= family_size {
        return None;
    }
    let mut bytes = vec![0u8; length];
    memory
        .read_exact(
            reverie::syscalls::Addr::<u8>::from_raw(address)?,
            &mut bytes,
        )
        .ok()?;
    let family = u16::from_ne_bytes([bytes[0], bytes[1]]);
    if family != libc::AF_UNIX as u16 || bytes[family_size] == 0 {
        return None;
    }
    let path = &bytes[family_size..];
    let end = path
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(path.len());
    Some(path[..end].to_vec())
}

/// The pathname `AF_UNIX` addresses of the `count` message headers at
/// `headers` (`struct msghdr`, or `struct mmsghdr` every `stride` bytes),
/// each its `msg_name` of `msg_namelen` bytes ([`unix_address_path`]), up to
/// `UIO_MAXIOV` headers, as `sendmmsg` takes at most. A header that cannot
/// be read ends the list.
fn message_addresses<M: MemoryAccess>(
    memory: &M,
    headers: usize,
    count: usize,
    stride: usize,
) -> Vec<Vec<u8>> {
    const UIO_MAXIOV: usize = 1024;
    let mut paths = Vec::new();
    for index in 0..count.min(UIO_MAXIOV) {
        let Some(header) =
            reverie::syscalls::Addr::<[u64; 2]>::from_raw(headers.wrapping_add(index * stride))
        else {
            break;
        };
        let Ok([name, length]) = memory.read_value(header) else {
            break;
        };
        if let Some(path) = unix_address_path(memory, name as usize, length as u32 as usize) {
            paths.push(path);
        }
    }
    paths
}

/// Whether x86-64's `access_ok(address, size)` accepts a user range of a
/// size not known at compile time (`__access_ok`, v6.17
/// `arch/x86/include/asm/uaccess_64.h`): its end, `address + size` modulo
/// 2^64, neither wraps below `address` nor passes `USER_PTR_MAX`, which this
/// host's paging mode sets (`threads::user_ptr_max`, measured once).
fn access_ok(address: usize, size: usize) -> bool {
    let end = address.wrapping_add(size);
    end >= address && end <= crate::syscalls::threads::user_ptr_max()
}

/// A read of a directory (`read`, `readv`, `pread64`, `preadv`, `preadv2`),
/// in Linux's order of checks (v6.17 `fs/read_write.c`, `lib/iov_iter.c`),
/// after the descriptor lookup and a negative position's `EINVAL`
/// ([`Detcore::answer_placeholder_call`]): for `read` and `pread64` the
/// buffer's `access_ok` (`EFAULT`, which also refuses every count past
/// `SSIZE_MAX`) and a position whose end overflows (`EINVAL`); for the
/// vectored reads `import_iovec` (more than `UIO_MAXIOV` vectors `EINVAL`
/// before the array is read, an unreadable array `EFAULT`, a length past
/// `SSIZE_MAX` `EINVAL`, a vector `access_ok` refuses `EFAULT`, a lone
/// vector's length clamped to `MAX_RW_COUNT` first), nothing to read (0),
/// and a `preadv2` flag the loop does not take (`EOPNOTSUPP`); then
/// `generic_read_dir`'s `EISDIR`, also for a count of 0.
fn read_answer<M: MemoryAccess>(
    memory: &M,
    sysno: Sysno,
    args: &syscalls::SyscallArgs,
) -> Result<i64, Errno> {
    const UIO_MAXIOV: usize = 1024;
    const RWF_HIPRI: u32 = 1;
    const MAX_RW_COUNT: usize = 0x7fff_f000;
    let position_overflows =
        |position: i64, count: usize| position.checked_add(count as i64).is_none_or(|end| end < 0);
    match sysno {
        Sysno::read | Sysno::pread64 => {
            let count = args.arg2;
            if !access_ok(args.arg1, count) {
                return Err(Errno::EFAULT);
            }
            if sysno == Sysno::pread64 && position_overflows(args.arg3 as i64, count) {
                return Err(Errno::EINVAL);
            }
            Err(Errno::EISDIR)
        }
        _ => {
            // `import_iovec` takes the count as an `unsigned int`.
            let count = args.arg2 as u32 as usize;
            if count > UIO_MAXIOV {
                return Err(Errno::EINVAL);
            }
            let mut total: usize = 0;
            for index in 0..count {
                let Some(vector) = reverie::syscalls::Addr::<[u64; 2]>::from_raw(
                    args.arg1.wrapping_add(index * 16),
                ) else {
                    return Err(Errno::EFAULT);
                };
                let [base, length] = memory.read_value(vector).map_err(|_| Errno::EFAULT)?;
                if i64::try_from(length).is_err() {
                    return Err(Errno::EINVAL);
                }
                let length = if count == 1 {
                    (length as usize).min(MAX_RW_COUNT)
                } else {
                    length as usize
                };
                if !access_ok(base as usize, length) {
                    return Err(Errno::EFAULT);
                }
                total = total.saturating_add(length);
            }
            if total == 0 {
                return Ok(0);
            }
            if sysno != Sysno::readv
                && !(sysno == Sysno::preadv2 && args.arg3 as i64 == -1)
                && position_overflows(args.arg3 as i64, total)
            {
                return Err(Errno::EINVAL);
            }
            // The flags are an `rwf_t`, an `int`: Linux reads the register's
            // low 32 bits.
            if sysno == Sysno::preadv2 && args.arg5 as u32 & !RWF_HIPRI != 0 {
                return Err(Errno::EOPNOTSUPP);
            }
            Err(Errno::EISDIR)
        }
    }
}

/// The arguments Linux reads before an extended-attribute call through a
/// descriptor looks the descriptor up, and the error the first that fails
/// gives (v6.17 `fs/xattr.c`): for `fsetxattr` the flags (`EINVAL`), the
/// name, the size (`E2BIG` past `XATTR_SIZE_MAX`) and the value (`EFAULT`);
/// for `fgetxattr` and `fremovexattr` the name: an unreadable one `EFAULT`,
/// an empty one or one of 256 bytes or more `ERANGE` (`import_xattr_name`).
fn xattr_arguments_answer<M: MemoryAccess>(memory: &M, call: &Syscall) -> Result<(), Errno> {
    const XATTR_NAME_MAX: usize = 255;
    const XATTR_SIZE_MAX: usize = 65536;
    let (sysno, args) = call.into_parts();
    let name = |address: usize| -> Result<(), Errno> {
        let mut bytes = Vec::with_capacity(XATTR_NAME_MAX + 1);
        for offset in 0..=XATTR_NAME_MAX {
            let byte = reverie::syscalls::Addr::<u8>::from_raw(address.wrapping_add(offset))
                .ok_or(Errno::EFAULT)
                .and_then(|address| memory.read_value(address).map_err(|_| Errno::EFAULT))?;
            if byte == 0 {
                break;
            }
            bytes.push(byte);
        }
        if bytes.is_empty() || bytes.len() > XATTR_NAME_MAX {
            return Err(Errno::ERANGE);
        }
        Ok(())
    };
    match sysno {
        Sysno::fsetxattr => {
            if args.arg4 as i32 & !(libc::XATTR_CREATE | libc::XATTR_REPLACE) != 0 {
                return Err(Errno::EINVAL);
            }
            name(args.arg1)?;
            let size = args.arg3;
            if size > XATTR_SIZE_MAX {
                return Err(Errno::E2BIG);
            }
            if size > 0 {
                let mut value = vec![0u8; size];
                reverie::syscalls::Addr::<u8>::from_raw(args.arg2)
                    .ok_or(Errno::EFAULT)
                    .and_then(|address| {
                        memory
                            .read_exact(address, &mut value)
                            .map_err(|_| Errno::EFAULT)
                    })?;
            }
            Ok(())
        }
        Sysno::fgetxattr | Sysno::fremovexattr => name(args.arg1),
        _ => Ok(()),
    }
}

/// What Linux's checks of a transfer need of its other descriptor
/// ([`Detcore::descriptor_facts`]).
#[derive(Debug, Clone, Copy)]
struct DescriptorFacts {
    /// Opened for reading (`FMODE_READ`).
    readable: bool,
    /// Opened for writing (`FMODE_WRITE`).
    writable: bool,
    /// An `O_PATH` descriptor, which `fdget` refuses.
    path_only: bool,
    /// `O_APPEND`.
    append: bool,
    /// A pipe.
    pipe: bool,
    /// On the guest's procfs.
    on_procfs: bool,
    /// The placeholder state, for a placeholder.
    placeholder: Option<PlaceholderState>,
}

/// procfs's largest file offset (`MAX_NON_LFS`, `s_maxbytes`, v6.17
/// `fs/proc/root.c`), past which `sendfile` from one of its directories is
/// `EOVERFLOW`.
const PROCFS_MAX_BYTES: i64 = (1 << 31) - 1;

/// The largest write hint `F_SET_RW_HINT` takes (`RWH_WRITE_LIFE_EXTREME`).
const RWH_WRITE_LIFE_EXTREME: u64 = 5;

/// Whether the caller may act as the owner of the directory `directory`
/// stands for (`inode_owner_or_capable`), which `O_NOATIME`, `F_SETFL`'s
/// `O_NOATIME` and `F_SET_RW_HINT` require. Detcore takes the caller to be
/// the guest's root, with its capabilities in the guest's user namespace,
/// as it renders every live task's files as owned by it
/// (`rendered_file_owner`, named approximation): a task's descriptor
/// directory is its own; `/proc` is global root's, whom the caller may act
/// as only where the guest's user namespace maps it, as the guest's own stat
/// of `/proc` shows (`guest_procfs_root`: an unmapped owner reads as the
/// overflow ID, which no mapped caller has).
fn placeholder_owner_or_capable(directory: &PlaceholderDirectory) -> Result<bool, Errno> {
    const OVERFLOW_ID: u32 = 65534;
    match directory {
        PlaceholderDirectory::Descriptors(_) => Ok(true),
        PlaceholderDirectory::Root => {
            let (_, uid, gid) = super::rendered_procfs::guest_procfs_root()?;
            Ok(uid != OVERFLOW_ID && gid != OVERFLOW_ID)
        }
    }
}

/// Whether the caller owns the directory `directory` stands for, as
/// `vfs_setlease` asks for a lease (the file's owner is the caller's
/// filesystem uid; `CAP_LEASE` counts only in the initial user namespace):
/// Detcore takes the caller to be the guest's root, which owns its own task's
/// descriptor directory (`rendered_file_owner`), and owns `/proc` only where
/// global root is the guest's root, as the guest's own stat of `/proc` shows
/// (`guest_procfs_root`: uid 0). Read from the same facts as
/// [`placeholder_owner_or_capable`], which also lets a caller act as an owner
/// its user namespace maps, as `inode_owner_or_capable` does and
/// `vfs_setlease` does not.
fn placeholder_lease_owner(directory: &PlaceholderDirectory) -> Result<bool, Errno> {
    match directory {
        PlaceholderDirectory::Descriptors(_) => Ok(true),
        PlaceholderDirectory::Root => {
            let (_, uid, _) = super::rendered_procfs::guest_procfs_root()?;
            Ok(uid == 0)
        }
    }
}

/// The bytes of a sent message's control buffer Detcore reads
/// (`syscalls::io::sent_descriptors`).
const MAX_SEND_CONTROL_BYTES: usize = 64 * 1024;

/// What a `sendmsg` or `sendmmsg` whose `SCM_RIGHTS` descriptors Detcore
/// inspected ([`Detcore::inspect_placeholder_send`]) keeps for the attempts
/// the internal polling loop makes of it, each of which may come after other
/// threads ran (`ThreadState::send_inspection`): the socket, the guest
/// addresses of its messages' headers, and the placeholders among the
/// descriptors they passed, with their open file descriptions, whose copies
/// Detcore counted in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SendInspection {
    /// The call, `sendmsg` or `sendmmsg`.
    pub(crate) sysno: Sysno,
    socket: RawFd,
    headers: Vec<usize>,
    placeholders: Vec<(RawFd, crate::types::OpenFileId)>,
}

/// `fallocate` of a directory Linux refuses in `vfs_fallocate`'s order: a
/// negative offset or a length that is not positive (`EINVAL`), an unknown
/// mode bit or a mode with a flag it does not take (`EOPNOTSUPP`), then a
/// description opened without write access (`EBADF`), as every directory's
/// is.
fn fallocate_answer(mode: i32, offset: i64, length: i64) -> Errno {
    const KEEP_SIZE: i32 = 0x01;
    const PUNCH_HOLE: i32 = 0x02;
    const COLLAPSE_RANGE: i32 = 0x08;
    const ZERO_RANGE: i32 = 0x10;
    const INSERT_RANGE: i32 = 0x20;
    const UNSHARE_RANGE: i32 = 0x40;
    const WRITE_ZEROES: i32 = 0x80;
    const MODE_MASK: i32 =
        PUNCH_HOLE | COLLAPSE_RANGE | ZERO_RANGE | INSERT_RANGE | UNSHARE_RANGE | WRITE_ZEROES;
    if offset < 0 || length <= 0 {
        return Errno::EINVAL;
    }
    if mode & !(MODE_MASK | KEEP_SIZE) != 0 {
        return Errno::EOPNOTSUPP;
    }
    match mode & MODE_MASK {
        0 | UNSHARE_RANGE | ZERO_RANGE => {}
        PUNCH_HOLE if mode & KEEP_SIZE == 0 => return Errno::EOPNOTSUPP,
        PUNCH_HOLE => {}
        COLLAPSE_RANGE | INSERT_RANGE | WRITE_ZEROES if mode & KEEP_SIZE != 0 => {
            return Errno::EOPNOTSUPP;
        }
        COLLAPSE_RANGE | INSERT_RANGE | WRITE_ZEROES => {}
        _ => return Errno::EOPNOTSUPP,
    }
    Errno::EBADF
}

/// `fadvise64` of a procfs directory (`generic_fadvise`): a negative length
/// is `EINVAL`; procfs has no backing device, so any of the six advices is
/// taken and ignored, and any other value is `EINVAL`.
fn fadvise_answer(length: i64, advice: i32) -> Result<i64, Errno> {
    if length < 0 {
        return Err(Errno::EINVAL);
    }
    match advice {
        libc::POSIX_FADV_NORMAL
        | libc::POSIX_FADV_RANDOM
        | libc::POSIX_FADV_SEQUENTIAL
        | libc::POSIX_FADV_WILLNEED
        | libc::POSIX_FADV_DONTNEED
        | libc::POSIX_FADV_NOREUSE => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

/// `sync_file_range` of a directory (`sync_file_range`): an unknown flag, a
/// negative offset or end, or an end that wraps is `EINVAL`; a directory
/// passes the file type check, and has nothing to write back.
fn sync_file_range_answer(offset: i64, bytes: i64, flags: u32) -> Result<i64, Errno> {
    const VALID_FLAGS: u32 = 1 | 2 | 4;
    if flags & !VALID_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    let end = offset.wrapping_add(bytes);
    if offset < 0 || end < 0 || end < offset {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// `F_DUPFD_QUERY` (Linux 6.10).
const F_DUPFD_QUERY: libc::c_int = 1027;
/// `F_CREATED_QUERY` (Linux 6.12).
const F_CREATED_QUERY: libc::c_int = 1028;
/// `F_GET_RW_HINT`.
const F_GET_RW_HINT: libc::c_int = 1035;
/// `F_SET_RW_HINT`.
const F_SET_RW_HINT: libc::c_int = 1036;
/// `F_GETOWNER_UIDS`.
const F_GETOWNER_UIDS: libc::c_int = 17;
/// `F_SETSIG`.
const F_SETSIG: libc::c_int = 10;
/// `F_GETSIG`.
const F_GETSIG: libc::c_int = 11;
/// `F_SETOWN_EX`.
const F_SETOWN_EX: libc::c_int = 15;
/// `F_GETOWN_EX`.
const F_GETOWN_EX: libc::c_int = 16;
/// `flock`'s `LOCK_MAND`, which Linux ignores (`SYSCALL_DEFINE2(flock)`).
const LOCK_MAND: libc::c_int = 32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_below_a_directory_keeps_what_linux_resolves() {
        let base = Path::new("/proc/self/fd");
        let spelled = |relative: &str| String::from_utf8(below(base, relative.as_bytes())).unwrap();
        assert_eq!(spelled("3"), "/proc/self/fd/3");
        assert_eq!(spelled("3/"), "/proc/self/fd/3/");
        assert_eq!(spelled("."), "/proc/self/fd/.");
        assert_eq!(spelled("./././."), "/proc/self/fd/.");
        assert_eq!(spelled(".//3//./x"), "/proc/self/fd/3/x");
        assert_eq!(spelled("3/."), "/proc/self/fd/3/");
        assert_eq!(spelled(".."), "/proc/self/fd/..");
        // A 4093-byte path of `./` repeats and a final `.`, valid on Linux,
        // stays short.
        let long = "./".repeat(2046) + ".";
        assert_eq!(long.len(), 4093);
        assert_eq!(spelled(&long), "/proc/self/fd/.");
        // Below `/proc` itself.
        let root = |relative: &str| {
            String::from_utf8(below(Path::new("/proc"), relative.as_bytes())).unwrap()
        };
        assert_eq!(root("self/status"), "/proc/self/status");
        assert_eq!(root(".."), "/proc/..");
        assert_eq!(root("."), "/proc/.");
    }

    #[test]
    fn a_path_through_a_descriptor_link_is_split_after_the_link() {
        let split = |path: &str| {
            through_descriptor_link(path.as_bytes())
                .map(|(link, rest)| (link, String::from_utf8(rest).unwrap()))
        };
        assert_eq!(
            split("/proc/self/fd/5/3"),
            Some((PathBuf::from("/proc/self/fd/5"), "3".to_string()))
        );
        assert_eq!(
            split("/dev/fd/5/3/x"),
            Some((PathBuf::from("/dev/fd/5"), "3/x".to_string()))
        );
        assert_eq!(
            split("/proc/7/task/9/fd/5/3"),
            Some((PathBuf::from("/proc/7/task/9/fd/5"), "3".to_string()))
        );
        assert_eq!(split("/proc/self/fd/5"), None);
        // A final `/` follows the link: the directory itself.
        assert_eq!(
            split("/proc/self/fd/5/"),
            Some((PathBuf::from("/proc/self/fd/5"), String::new()))
        );
        assert_eq!(split("/proc/self/status"), None);
        assert_eq!(split("/tmp/x/y/z/w"), None);
        // Empty and `.` components before the number name nothing.
        for spelling in [
            "//proc/self/fd/5/3",
            "/proc//self/fd/5/3",
            "/proc/./self/fd/5/3",
            "/proc/self/fd/./5//3",
            "/./proc/self//fd/5/3",
        ] {
            assert_eq!(
                split(spelling),
                Some((PathBuf::from("/proc/self/fd/5"), "3".to_string())),
                "{spelling}"
            );
        }
        // A `..` before the number is not recognized.
        assert_eq!(split("/proc/self/../self/fd/5/3"), None);
        assert_eq!(split("proc/self/fd/5/3"), None);
    }

    #[test]
    fn the_rest_of_a_path_through_a_link_keeps_its_bytes() {
        let path = b"/proc/self/fd/5/7/\xff\xfe/x";
        let (link, rest) = through_descriptor_link(path).unwrap();
        assert_eq!(link, PathBuf::from("/proc/self/fd/5"));
        assert_eq!(rest, b"7/\xff\xfe/x".to_vec());
        assert_eq!(
            below(Path::new("/proc/self/fd"), &rest),
            b"/proc/self/fd/7/\xff\xfe/x".to_vec()
        );
        // A link component that is not ASCII is no link.
        assert_eq!(through_descriptor_link(b"/proc/\xffself/fd/5/3"), None);
    }

    #[test]
    fn the_links_before_a_descriptor_link_are_counted() {
        assert_eq!(selector_links(Path::new("/proc/self/fd/5")), 1);
        assert_eq!(selector_links(Path::new("/proc/thread-self/fd/5")), 1);
        assert_eq!(selector_links(Path::new("/proc/self/task/9/fd/5")), 1);
        assert_eq!(selector_links(Path::new("/dev/fd/5")), 2);
        assert_eq!(selector_links(Path::new("/proc/7/fd/5")), 0);
        assert_eq!(selector_links(Path::new("/proc/7/task/9/fd/5")), 0);
    }

    /// `/proc` itself by every spelling Linux's walk takes there: trailing
    /// and repeated slashes, `.` components, and `..` back from the
    /// directories every procfs has below it; nothing else.
    #[test]
    fn every_spelling_of_proc_itself_is_the_root() {
        for spelling in [
            "/proc",
            "/proc/",
            "//proc",
            "/proc/.",
            "/./proc/./",
            "/proc/self/..",
            "/proc/self/fd/../..",
            "/proc/7/task/9/../../..",
            "/proc/thread-self/../../..",
            "/proc/self/task/7/fd/../../../..",
            "/../proc",
            "/proc/../proc",
        ] {
            assert!(
                names_procfs_root(Path::new(spelling)).is_some(),
                "{spelling}"
            );
        }
        for spelling in [
            "proc",
            "/",
            "/proc/self",
            "/proc/self/fd/..",
            "/proc/thread-self/..",
            "/proc/sys/..",
            "/proc/self/cwd/..",
            "/proc/self/fd/3/..",
            "/proc/self/status/..",
            "/tmp/../proc",
            "/proc/07/..",
        ] {
            assert!(
                names_procfs_root(Path::new(spelling)).is_none(),
                "{spelling}"
            );
        }
        // A spelling through numbered tasks names `/proc` only while each
        // exists: they are reported, in the walk's order.
        let task = |process, thread| NamedTask { process, thread };
        assert_eq!(names_procfs_root(Path::new("/proc")), Some(Vec::new()));
        assert_eq!(
            names_procfs_root(Path::new("/proc/7/task/9/../../..")),
            Some(vec![task(Some(7), None), task(Some(7), Some(9))])
        );
        assert_eq!(
            names_procfs_root(Path::new("/proc/self/task/9/../../..")),
            Some(vec![task(None, Some(9))])
        );
        // A number past any `pid_t` names no task (0).
        assert_eq!(
            names_procfs_root(Path::new("/proc/2147483648/..")),
            Some(vec![task(Some(0), None)])
        );
    }

    /// `..` resolves back over the directories every procfs has, as the
    /// kernel's walk does, and not over anything else.
    #[test]
    fn a_dotdot_resolves_over_procfs_directories_only() {
        let resolved = |path: &str| {
            resolved_procfs_dotdot(Path::new(path)).map(|(path, _)| path.display().to_string())
        };
        let tasks = |path: &str| resolved_procfs_dotdot(Path::new(path)).map(|(_, tasks)| tasks);
        let task = |process, thread| NamedTask { process, thread };
        // A numbered task on the way is looked up there, so the resolution
        // holds only while it exists (`Detcore::named_tasks_exist`): it is
        // reported, never taken to exist.
        assert_eq!(
            tasks("/proc/7/fd/../../fs/nfsd"),
            Some(vec![task(Some(7), None)])
        );
        assert_eq!(tasks("/proc/self/../fs"), Some(Vec::new()));
        assert_eq!(
            tasks("/proc/thread-self/../../../7/.."),
            Some(vec![task(Some(7), None)])
        );
        assert_eq!(resolved("/proc/self/../fs"), Some("/proc/fs".to_string()));
        assert_eq!(
            resolved("/proc/7/fd/../../fs/nfsd"),
            Some("/proc/fs/nfsd".to_string())
        );
        assert_eq!(
            resolved("/proc/thread-self/../stat"),
            Some("/proc/self/task/stat".to_string())
        );
        assert_eq!(
            resolved("/proc/thread-self/fd/.."),
            Some("/proc/thread-self".to_string())
        );
        assert_eq!(
            resolved("/proc/thread-self/../../.."),
            Some("/proc".to_string())
        );
        assert_eq!(resolved("/../proc/./self/.."), Some("/proc".to_string()));
        assert_eq!(resolved("/proc/.."), Some("/".to_string()));
        assert_eq!(resolved("/proc/self/fd"), None, "no ..");
        assert_eq!(
            resolved("/proc/self/cwd/../x"),
            None,
            "a link leads elsewhere"
        );
        assert_eq!(resolved("/proc/self/status/.."), None, "a file");
        assert_eq!(resolved("/proc/sys/../fs"), None);
        assert_eq!(resolved("/tmp/../proc/fs"), None);
        assert_eq!(resolved("/proc/self/fd/3/.."), None, "a descriptor link");
    }

    /// A directory descriptor is told to be on a placeholder's object from
    /// its identity and filesystem alone, with nothing opened: its identity
    /// is an object's; or, where the object is procfs's root, its filesystem
    /// is procfs at the root inode, and one whose filesystem cannot be told
    /// is refused all the same (fail closed); where the object is the
    /// placeholder, the identity alone decides.
    #[test]
    fn a_procfs_root_is_told_without_opening_anything() {
        type Tool = Detcore<crate::record_or_replay::NoopTool>;
        let proc = reverie::syscalls::linux_abi::PROC_SUPER_MAGIC as i64;
        let none = BTreeSet::new();
        assert!(Tool::on_placeholder_object((22, 1), Some(Ok(proc)), &none));
        assert!(!Tool::on_placeholder_object((22, 2), Some(Ok(proc)), &none));
        assert!(!Tool::on_placeholder_object(
            (22, 1),
            Some(Ok(0x9123683e)),
            &none
        ));
        assert!(Tool::on_placeholder_object(
            (22, 5),
            Some(Err(Errno::EACCES)),
            &none
        ));
        assert!(Tool::on_placeholder_object(
            (22, 5),
            Some(Err(Errno::ENOMEM)),
            &none
        ));
        let objects = BTreeSet::from([(22, 5)]);
        assert!(Tool::on_placeholder_object(
            (22, 5),
            Some(Ok(0x9123683e)),
            &objects
        ));
        assert!(Tool::on_placeholder_object((22, 5), None, &objects));
        assert!(!Tool::on_placeholder_object((22, 1), None, &none));
    }

    #[test]
    fn every_redirected_call_has_its_operands() {
        assert_eq!(relative_operands(Sysno::fchmodat2), &[(0, 1)]);
        assert_eq!(relative_operands(Sysno::openat2), &[(0, 1)]);
        assert_eq!(relative_operands(Sysno::execveat), &[] as &[(usize, usize)]);
        assert_eq!(absolute_operands(Sysno::rename), &[0, 1]);
        assert_eq!(absolute_operands(Sysno::execve), &[] as &[usize]);
        assert_eq!(absolute_operands(Sysno::read), &[] as &[usize]);
    }

    /// The calls that follow a final link, and so take a descriptor link to a
    /// placeholder for the directory, as each call's `LOOKUP_FOLLOW` says;
    /// the ones that operate on the link itself do not.
    #[test]
    fn the_calls_that_follow_a_final_link_are_told_apart() {
        let args = |values: [usize; 6]| syscalls::SyscallArgs {
            arg0: values[0],
            arg1: values[1],
            arg2: values[2],
            arg3: values[3],
            arg4: values[4],
            arg5: values[5],
        };
        let none = args([0; 6]);
        for sysno in [
            Sysno::chdir,
            Sysno::chroot,
            Sysno::access,
            Sysno::stat,
            Sysno::chmod,
            Sysno::chown,
            Sysno::utimes,
            Sysno::statfs,
            Sysno::getxattr,
            Sysno::setxattr,
            Sysno::removexattr,
            Sysno::listxattr,
            Sysno::truncate,
        ] {
            assert!(follows_final(sysno, &none, 0), "{sysno}");
        }
        for sysno in [
            Sysno::lstat,
            Sysno::readlink,
            Sysno::lchown,
            Sysno::lgetxattr,
            Sysno::unlink,
            Sysno::rmdir,
            Sysno::mkdir,
            Sysno::rename,
            Sysno::open,
            Sysno::openat,
            Sysno::execve,
        ] {
            assert!(!follows_final(sysno, &none, 0), "{sysno}");
        }
        let nofollow = libc::AT_SYMLINK_NOFOLLOW as usize;
        assert!(follows_final(Sysno::newfstatat, &none, 1));
        assert!(!follows_final(
            Sysno::newfstatat,
            &args([0, 0, 0, nofollow, 0, 0]),
            1
        ));
        assert!(!follows_final(
            Sysno::statx,
            &args([0, 0, nofollow, 0, 0, 0]),
            1
        ));
        assert!(!follows_final(
            Sysno::fchownat,
            &args([0, 0, 0, 0, nofollow, 0]),
            1
        ));
        let follow = libc::AT_SYMLINK_FOLLOW as usize;
        assert!(follows_final(
            Sysno::linkat,
            &args([0, 0, 0, 0, follow, 0]),
            1
        ));
        assert!(!follows_final(
            Sysno::linkat,
            &args([0, 0, 0, 0, follow, 0]),
            3
        ));
        assert!(!follows_final(Sysno::linkat, &none, 1));
        assert!(follows_final(Sysno::inotify_add_watch, &none, 1));
        assert!(!follows_final(
            Sysno::inotify_add_watch,
            &args([0, 0, 0x0200_0000, 0, 0, 0]),
            1
        ));
    }

    /// A descriptor link's prefix ends just past the link, whatever follows,
    /// so that a respelling can insert after it; a bare link is all prefix.
    #[test]
    fn a_descriptor_links_prefix_ends_past_the_link() {
        let prefix = |path: &str| descriptor_link_prefix(path.as_bytes());
        assert_eq!(
            prefix("/proc/self/fd/5/x"),
            Some((PathBuf::from("/proc/self/fd/5"), 15))
        );
        assert_eq!(
            prefix("/proc/self/fd/5"),
            Some((PathBuf::from("/proc/self/fd/5"), 15))
        );
        assert_eq!(
            prefix("/proc//self/./fd/5/"),
            Some((PathBuf::from("/proc/self/fd/5"), 18))
        );
        assert_eq!(
            prefix("/dev/fd/12"),
            Some((PathBuf::from("/dev/fd/12"), 10))
        );
        assert_eq!(prefix("/proc/self/status"), None);
        assert_eq!(prefix("self/fd/5"), None);
    }

    /// A descriptor directory by any task's spelling, for the refusal where
    /// the backend's `/proc` is its own; no other path.
    #[test]
    fn every_descriptor_directory_spelling_is_told() {
        for path in [
            "/proc/self/fd",
            "/proc/thread-self/fd",
            "/proc/7/fd",
            "/proc/7/task/9/fd",
            "/proc/self/task/9/fd/",
            "/dev/fd",
            "//proc/./self/fd",
        ] {
            assert!(spells_a_descriptor_directory(Path::new(path)), "{path}");
        }
        for path in [
            "/proc",
            "/proc/self",
            "/proc/self/fdinfo",
            "/proc/self/fd/3",
            "/proc/x/fd",
            "/proc/self/fd/..",
            "/dev",
            "proc/self/fd",
        ] {
            assert!(!spells_a_descriptor_directory(Path::new(path)), "{path}");
        }
    }

    /// x86-64's `access_ok` for a size not known at compile time: the end
    /// may neither wrap nor pass `USER_PTR_MAX`.
    #[test]
    fn access_ok_refuses_a_wrapping_or_kernel_range() {
        let limit = crate::syscalls::threads::user_ptr_max();
        assert!(access_ok(0x1000, 1));
        assert!(access_ok(0, 0));
        assert!(access_ok(limit, 0));
        assert!(!access_ok(limit, 1));
        assert!(!access_ok(usize::MAX, 1));
        assert!(!access_ok(0x1000, usize::MAX));
        assert!(!access_ok(0x1000, isize::MAX as usize + 1));
    }

    /// A directory read in Linux's order: the buffer before `EISDIR`, also
    /// for a count of 0; the vectors' count, array, lengths and ranges before
    /// anything; nothing to read is 0; `preadv2`'s flags last.
    #[test]
    fn a_directory_read_checks_in_linux_order() {
        let memory = reverie::syscalls::LocalMemory::new();
        let args = |values: [usize; 6]| syscalls::SyscallArgs {
            arg0: values[0],
            arg1: values[1],
            arg2: values[2],
            arg3: values[3],
            arg4: values[4],
            arg5: values[5],
        };
        let buffer = [0u8; 16];
        let at = buffer.as_ptr() as usize;
        assert_eq!(
            read_answer(&memory, Sysno::read, &args([3, at, 16, 0, 0, 0])),
            Err(Errno::EISDIR)
        );
        assert_eq!(
            read_answer(&memory, Sysno::read, &args([3, at, 0, 0, 0, 0])),
            Err(Errno::EISDIR)
        );
        assert_eq!(
            read_answer(&memory, Sysno::read, &args([3, usize::MAX, 1, 0, 0, 0])),
            Err(Errno::EFAULT)
        );
        assert_eq!(
            read_answer(&memory, Sysno::read, &args([3, at, usize::MAX, 0, 0, 0])),
            Err(Errno::EFAULT)
        );
        assert_eq!(
            read_answer(
                &memory,
                Sysno::pread64,
                &args([3, at, 16, i64::MAX as usize, 0, 0])
            ),
            Err(Errno::EINVAL)
        );
        let empty: [[u64; 2]; 1] = [[at as u64, 0]];
        let vectors = empty.as_ptr() as usize;
        assert_eq!(
            read_answer(&memory, Sysno::readv, &args([3, vectors, 1, 0, 0, 0])),
            Ok(0)
        );
        assert_eq!(
            read_answer(&memory, Sysno::readv, &args([3, vectors, 1025, 0, 0, 0])),
            Err(Errno::EINVAL)
        );
        let one: [[u64; 2]; 1] = [[at as u64, 16]];
        let vectors = one.as_ptr() as usize;
        assert_eq!(
            read_answer(&memory, Sysno::readv, &args([3, vectors, 1, 0, 0, 0])),
            Err(Errno::EISDIR)
        );
        assert_eq!(
            read_answer(&memory, Sysno::preadv2, &args([3, vectors, 1, 0, 0, 8])),
            Err(Errno::EOPNOTSUPP)
        );
        assert_eq!(
            read_answer(&memory, Sysno::preadv2, &args([3, vectors, 1, 0, 0, 1])),
            Err(Errno::EISDIR)
        );
        let bad: [[u64; 2]; 2] = [[at as u64, 1], [u64::MAX, 1]];
        let vectors = bad.as_ptr() as usize;
        assert_eq!(
            read_answer(&memory, Sysno::readv, &args([3, vectors, 2, 0, 0, 0])),
            Err(Errno::EFAULT)
        );
    }

    /// The calls through a placeholder descriptor that never reach it are
    /// told by their descriptor operand; a path call only by an empty path
    /// with `AT_EMPTY_PATH`, or `futimens`'s none.
    #[test]
    fn the_answered_calls_name_their_descriptor() {
        let args = |values: [usize; 6]| syscalls::SyscallArgs {
            arg0: values[0],
            arg1: values[1],
            arg2: values[2],
            arg3: values[3],
            arg4: values[4],
            arg5: values[5],
        };
        let memory = reverie::syscalls::LocalMemory::new();
        for sysno in [
            Sysno::getdents64,
            Sysno::lseek,
            Sysno::fchdir,
            Sysno::fcntl,
            Sysno::read,
            Sysno::write,
            Sysno::ioctl,
            Sysno::fsync,
            Sysno::fdatasync,
            Sysno::syncfs,
            Sysno::flock,
            Sysno::fchmod,
            Sysno::fchown,
            Sysno::fgetxattr,
            Sysno::fsetxattr,
            Sysno::flistxattr,
            Sysno::fremovexattr,
            Sysno::ftruncate,
            Sysno::fallocate,
            Sysno::fadvise64,
            Sysno::readahead,
            Sysno::sync_file_range,
            Sysno::fstatfs,
        ] {
            assert_eq!(
                placeholder_operand(&memory, sysno, &args([3, 0, 0, 0, 0, 0])),
                Some(0),
                "{sysno}"
            );
        }
        // A file-backed mapping names its descriptor; an anonymous one does
        // not.
        assert_eq!(
            placeholder_operand(&memory, Sysno::mmap, &args([0, 4096, 1, 2, 3, 0])),
            Some(4)
        );
        assert_eq!(
            placeholder_operand(
                &memory,
                Sysno::mmap,
                &args([
                    0,
                    4096,
                    1,
                    (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as usize,
                    3,
                    0
                ])
            ),
            None
        );
        // `futimens`.
        assert_eq!(
            placeholder_operand(&memory, Sysno::utimensat, &args([3, 0, 0, 0, 0, 0])),
            Some(0)
        );
        // A path call through the descriptor itself: an empty path with
        // `AT_EMPTY_PATH`; any name is a lookup below it.
        let empty = c"";
        let name = c"x";
        let at_empty = libc::AT_EMPTY_PATH as usize;
        assert_eq!(
            placeholder_operand(
                &memory,
                Sysno::fchmodat2,
                &args([3, empty.as_ptr() as usize, 0o600, at_empty, 0, 0])
            ),
            Some(0)
        );
        assert_eq!(
            placeholder_operand(
                &memory,
                Sysno::fchmodat2,
                &args([3, name.as_ptr() as usize, 0o600, at_empty, 0, 0])
            ),
            None
        );
        assert_eq!(
            placeholder_operand(
                &memory,
                Sysno::fchownat,
                &args([3, empty.as_ptr() as usize, 0, 0, at_empty, 0])
            ),
            Some(0)
        );
        assert_eq!(
            placeholder_operand(
                &memory,
                Sysno::faccessat2,
                &args([3, empty.as_ptr() as usize, 0, 0, 0, 0])
            ),
            None,
            "an empty path without AT_EMPTY_PATH names nothing"
        );
        // The descriptor-table calls are the kernel's.
        for sysno in [Sysno::close, Sysno::dup, Sysno::dup2, Sysno::dup3] {
            assert_eq!(
                placeholder_operand(&memory, sysno, &args([3, 4, 0, 0, 0, 0])),
                None
            );
        }
    }

    /// `fallocate`, `fadvise64` and `sync_file_range` of a directory, in
    /// Linux's order of checks (v6.17 `vfs_fallocate`, `generic_fadvise`,
    /// `sync_file_range`).
    #[test]
    fn the_refused_writes_of_a_directory_follow_linux_order() {
        assert_eq!(fallocate_answer(0, -1, 1), Errno::EINVAL);
        assert_eq!(fallocate_answer(0, 0, 0), Errno::EINVAL);
        assert_eq!(fallocate_answer(0x100, 0, 1), Errno::EOPNOTSUPP);
        assert_eq!(fallocate_answer(0x02, 0, 1), Errno::EOPNOTSUPP);
        assert_eq!(fallocate_answer(0x03, 0, 1), Errno::EBADF);
        assert_eq!(fallocate_answer(0x09, 0, 1), Errno::EOPNOTSUPP);
        assert_eq!(fallocate_answer(0x08, 0, 1), Errno::EBADF);
        assert_eq!(fallocate_answer(0x0a, 0, 1), Errno::EOPNOTSUPP);
        assert_eq!(fallocate_answer(0, 0, 1), Errno::EBADF);
        assert_eq!(fadvise_answer(-1, 0), Err(Errno::EINVAL));
        assert_eq!(fadvise_answer(0, libc::POSIX_FADV_DONTNEED), Ok(0));
        assert_eq!(fadvise_answer(0, 99), Err(Errno::EINVAL));
        assert_eq!(sync_file_range_answer(0, 0, 8), Err(Errno::EINVAL));
        assert_eq!(sync_file_range_answer(-1, 0, 0), Err(Errno::EINVAL));
        assert_eq!(sync_file_range_answer(i64::MAX, 1, 0), Err(Errno::EINVAL));
        assert_eq!(sync_file_range_answer(0, 4096, 7), Ok(0));
    }

    /// Every call Detcore answers through a placeholder descriptor passes the
    /// prefilter that lets `Detcore::handle_syscall_event` ask
    /// (`may_name_file_or_descriptor`), so that no such answer is dead code:
    /// each call [`placeholder_operand`] names a descriptor for, with the
    /// arguments that make it (a file-backed `mmap` and a path-less
    /// `utimensat` with zeros, the `AT_EMPTY_PATH` calls with an empty path),
    /// and the transfers.
    #[test]
    fn every_call_a_placeholder_answers_passes_the_prefilter() {
        use crate::syscall_classification::all_pinned_syscalls;
        use crate::syscalls::may_name_file_or_descriptor;
        let memory = reverie::syscalls::LocalMemory::new();
        // An empty path at an address whose low 32 bits carry
        // `AT_EMPTY_PATH`, so that one value serves as each call's path and
        // its flags.
        let buffer = vec![0u8; 3 * 0x1000];
        let start = buffer.as_ptr() as usize;
        let empty = (start..start + 2 * 0x1000)
            .find(|address| address & libc::AT_EMPTY_PATH as usize != 0)
            .expect("an address with the flag's bit");
        let names_itself = syscalls::SyscallArgs::new(0, empty, empty, empty, empty, empty);
        let zero = syscalls::SyscallArgs::new(0, 0, 0, 0, 0, 0);
        let mut answered = 0;
        let mut unasked = Vec::new();
        for sysno in all_pinned_syscalls() {
            if placeholder_operand(&memory, sysno, &zero).is_some()
                || placeholder_operand(&memory, sysno, &names_itself).is_some()
                || matches!(
                    sysno,
                    Sysno::sendfile | Sysno::splice | Sysno::tee | Sysno::copy_file_range
                )
            {
                answered += 1;
                if !may_name_file_or_descriptor(sysno) {
                    unasked.push(sysno);
                }
            }
        }
        assert!(unasked.is_empty(), "answered but never asked: {unasked:?}");
        assert!(answered > 40, "{answered} calls answered");
    }
}
