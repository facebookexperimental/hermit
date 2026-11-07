/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Another guest process's `cmdline`, `environ`, descriptor links and
//! `fdinfo`, and the directories that list them, looked up on a backend whose
//! `/proc/<pid>` is not the guest process with that number
//! (<https://github.com/rrnewton/hermit/issues/4564>).
//!
//! Since package K item 2b slice 2b-3 Detcore serves another process's
//! `cmdline` and `environ` there from the strings its image's `execve` passed
//! (`Detcore::serves_exec_strings`), where it recorded them or the process has
//! exited: such a lookup is left to the rendered path. Linux reads both from
//! the process's memory at each read; the record is the named interim
//! fallback, which does not show a rewrite of either after the exec (minibeads
//! dh-ngy9z). Where no record exists, the file is refused by name, as before.
//!
//! Another process's directories, its `stat` and its `comm` Detcore serves
//! itself since package K item 2b slice 2b-1
//! (<https://github.com/rrnewton/hermit/issues/5063>:
//! `descriptor_directory`'s task directories, `procfs::task_stat`); this
//! module covers the files it does not serve yet.
//!
//! Detcore renders a process's `cmdline` for the caller's own process only (it
//! cannot read another process's memory, which Linux reads at each read), and
//! answers descriptor links, `fdinfo` and the directories that list them for
//! the caller's own descriptors only. Another process's it leaves to the
//! kernel beneath. Where that kernel's `/proc/<pid>` is the guest's process
//! with that number (ptrace and the in-guest backends), the kernel answers for
//! it. Where the guest's threads are not host tasks
//! ([`crate::procfs_inode::tool_can_name_guest_descriptors`] is false: KVM),
//! `/proc/<pid>` in the run's PID namespace is whatever host task holds that
//! number, if any, one of the supervisor's own. The backend refuses an object
//! on procfs (EACCES) and fails a lookup that finds no task (ENOENT), so the
//! guest's answer said whether such a host task existed at that moment, which
//! host timing decides: a child's number was measured to be the number of its
//! VM's `kvm-nx-lpage-recovery` worker thread, which exits when the VM is
//! destroyed, after the child exits, on the host's schedule.
//!
//! So on such a backend Detcore decides every lookup of these files itself,
//! before it can reach the backend, for every spelling it can place: through
//! the caller's own descriptor links to the `/proc` directories it serves
//! (`/proc/self/fd/<a /proc descriptor>/<pid>/cmdline`) and through `..`, as
//! Linux's walk resolves them. A task its process table does not hold has no
//! `/proc` directory (ENOENT, Linux's answer), and another guest process's
//! file of these kinds is refused by name through the unsupported-operation
//! policy, the same answer on every run, until Detcore serves it from the
//! Guest facts.
//!
//! A path that goes on past another process's `cmdline`, a regular file
//! ([`past_a_task_file`]), gets Linux's ENOTDIR: through a further component
//! (`cmdline/x`, `cmdline/.`, `cmdline/..`), which Linux's walk cannot
//! descend through a file, and after slashes alone (`cmdline/`) for a call
//! that looks its path up whole. A call that creates, links, renames or
//! removes its last name answers a trailing slash otherwise in Linux
//! (EISDIR, EEXIST, a permission error), so that one stays refused by name,
//! as does every path past another process's `fdinfo/<N>`, whose answer
//! depends on whether the process holds descriptor N.
//!
//! Linux checks a call's own arguments before its walk (an `access` mode, a
//! `readlink` size, open flags), so the call is first given to the kernel
//! beneath with the task's directory spelled as a name procfs does not have,
//! as the level check does (`procfs_levels`): an argument the kernel refuses
//! is refused as Linux refuses it, and only a call that reaches the walk gets
//! Detcore's answer. That holds only where the kernel beneath makes Linux's
//! check before its own walk, with Linux's errno, and KVM's backend does not
//! always (<https://github.com/rrnewton/reverie/issues/1222>): it checks
//! renameat2's flag pairs, symlink's target, statx's mask and an extended
//! attribute's name, flags, size and value after its walk or not at all, so
//! that the respelled walk fails first; it checks utimensat's flags before
//! its times, and answers two UTIME_OMIT with 0, which Detcore takes for a
//! respelled call that succeeded (`issue_substituted`); it refuses every
//! execveat flag with EOPNOTSUPP; and it refuses a mknod of every type but a
//! regular file or a FIFO with EPERM, which Detcore takes for the backend's
//! own refusal, as it is for a device, although for a directory EPERM is
//! Linux's answer too. So Detcore decides those checks itself, from the
//! arguments, before the respelled call ([`linux_answer_before_the_walk`]),
//! and the backend's answer to every other argument stands. KVM's backend
//! does not implement `utime` and `utimes` at all, which Detcore's own
//! handlers issue as `utimensat`: their times are checked from the
//! arguments, and the respelled call is issued as `utimensat` too
//! ([`respelled_call`]).
//!
//! An empty pathname names no file of its own, although Detcore observes it
//! as its descriptor's path. With AT_EMPTY_PATH the call acts on the
//! descriptor itself (`getname_uflags`' LOOKUP_EMPTY), and without it the
//! path fails `getname` with ENOENT. Neither looks a `/proc/<pid>` name up,
//! so host timing cannot reach the answer: Detcore gives Linux's answer to
//! the call's own arguments ([`linux_answer_before_the_walk`], which knows
//! statx's form for a descriptor) and leaves the rest to the descriptor's
//! handlers and the backend, as for the descriptor's other operations. The
//! exception is an execveat of the descriptor, which KVM's backend cannot
//! run: Detcore answers Linux's EACCES for a procfs file or directory, and
//! refuses one opened through another process's descriptor link by name
//! ([`Detcore::execveat_of_another_process_procfs_descriptor`]).
//!
//! A call with two paths (`link`, `linkat`, `rename`, `renameat`,
//! `renameat2`) is given to the kernel with only one of them respelled, so
//! its ENOENT can be the other path's. Linux walks the first path before the
//! second: `do_linkat` looks the whole source up before it walks the
//! destination (fs/namei.c:4904-4908 at v6.17), and `do_renameat2` walks the
//! source's directory before the destination's (:5196-5201) and looks the
//! source's last name up after both (:5230). Where the path Detcore decides
//! is the call's first, its failing walk is Linux's first, and the answer
//! stands. Where it is the second, an ENOENT does not say whether, or how,
//! the first path failed, and the backend need not look the two up in
//! Linux's order either (KVM's looks up both paths' directories before
//! link's source), so ENOTDIR past another process's `cmdline` and ENOENT
//! for a task the table does not hold are refused by name there instead.
//!
//! The one task Linux's lookup finds that no guest process holds is Hermit's
//! own init, pid 1 where no guest holds it (`procfs::listing::task_exists`).
//! Its files are left to the backend, as before, apart from a descriptor's
//! fdinfo (`/proc/1[/task/1]/fdinfo/<N>`), which is refused by name: the
//! backend has no fdinfo of its own any more, and its procfs would answer
//! from the supervisor's descriptor table. The rest is a KNOWN LIMITATION,
//! not a determinism guarantee: on KVM pid 1 is the supervisor, whose
//! descriptor table also holds each child VM's descriptors, so which of its
//! descriptor entries exist changes with host timing, right after a child is
//! reaped, until the host tears the child's VM down. The backend isolation
//! matrix pins the backend's answers for pid 1's entries, so no fixed answer
//! Detcore could give leaves them unchanged. Hermit's init is owned by D-PID1
//! (<https://github.com/rrnewton/hermit/issues/4071>).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use super::descriptor_directory::resolved_procfs_dotdot;
use super::procfs_levels::Lookups;
use super::procfs_levels::dashed_spelling;
use super::procfs_levels::looked_up;
#[cfg(target_arch = "x86_64")]
use super::rendered_procfs::legacy_times_error;
use super::rendered_procfs::read_guest_string;
use crate::procfs::BoundTask;
use crate::record_or_replay::RecordOrReplay;
use crate::syscalls::helpers::report_refusal_reason;
use crate::syscalls::namespace::decimal_fd;
use crate::tool_local::Detcore;

/// Which of a process's files a path names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskFile {
    /// `cmdline`.
    Cmdline,
    /// `environ`.
    Environ,
    /// A descriptor link, `fd/<N>`, or a path below one.
    DescriptorLink,
    /// A descriptor's `fdinfo/<N>`.
    DescriptorInfo,
    /// The directory `fd` or `fdinfo` itself, which lists the descriptors.
    DescriptorDirectory,
}

/// The task an absolute guest path names by number, and which of its files,
/// when the path is a process's `cmdline`, a descriptor link (or a path below
/// one), a descriptor's `fdinfo`, or the `fd` or `fdinfo` directory:
/// `/proc/<pid>/...` or
/// `/proc/<pid>/task/<tid>/...`, with each number as procfs looks it up, in
/// decimal without a leading zero (`name_to_int`). `None` for every other
/// path, for the `self` and `thread-self` spellings, which name the caller's
/// own, and for a spelling with `..`, which a caller resolves first.
pub(crate) fn named_task_file(path: &Path) -> Option<(BoundTask, TaskFile)> {
    if !path.is_absolute() {
        return None;
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => parts.push(part),
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    let (task, rest) = match parts.as_slice() {
        [proc, process, task, thread, rest @ ..] if *proc == "proc" && *task == "task" => (
            BoundTask {
                process: decimal_fd(process)?,
                thread: Some(decimal_fd(thread)?),
            },
            rest,
        ),
        [proc, process, rest @ ..] if *proc == "proc" => (
            BoundTask {
                process: decimal_fd(process)?,
                thread: None,
            },
            rest,
        ),
        _ => return None,
    };
    let file = match rest {
        [name] if *name == "cmdline" => TaskFile::Cmdline,
        [name] if *name == "environ" => TaskFile::Environ,
        [directory, fd, ..] if *directory == "fd" && decimal_fd(fd).is_some() => {
            TaskFile::DescriptorLink
        }
        [directory, fd] if *directory == "fdinfo" && decimal_fd(fd).is_some() => {
            TaskFile::DescriptorInfo
        }
        [directory] if *directory == "fd" || *directory == "fdinfo" => {
            TaskFile::DescriptorDirectory
        }
        _ => return None,
    };
    Some((task, file))
}

/// What follows a task's regular file in a path that goes on past it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PastFile {
    /// Another component (`x`, `.` or `..`): Linux's walk must go on through
    /// the file, and fails with ENOTDIR whatever the call, after the call's
    /// own argument checks (`link_path_walk`, fs/namei.c:2507-2512 at v6.17).
    Component,
    /// Slashes alone: the file is the last component, which must then be a
    /// directory. A lookup of the whole path fails with ENOTDIR
    /// (`lookup_last`, fs/namei.c:2625-2631), and a call that creates,
    /// links, renames or removes that name answers otherwise.
    Slashes,
}

/// The task and file `parts` name when they end at a task's regular file,
/// a process's `cmdline` or a descriptor's `fdinfo/<N>`, as
/// [`named_task_file`] names them.
fn task_regular_file(parts: &[&OsStr]) -> Option<(BoundTask, TaskFile)> {
    let (task, rest) = match parts {
        [proc, process, task, thread, rest @ ..] if *proc == "proc" && *task == "task" => (
            BoundTask {
                process: decimal_fd(process)?,
                thread: Some(decimal_fd(thread)?),
            },
            rest,
        ),
        [proc, process, rest @ ..] if *proc == "proc" => (
            BoundTask {
                process: decimal_fd(process)?,
                thread: None,
            },
            rest,
        ),
        _ => return None,
    };
    match rest {
        [name] if *name == "cmdline" => Some((task, TaskFile::Cmdline)),
        [name] if *name == "environ" => Some((task, TaskFile::Environ)),
        [directory, fd] if *directory == "fdinfo" && decimal_fd(fd).is_some() => {
            Some((task, TaskFile::DescriptorInfo))
        }
        _ => None,
    }
}

/// The task, its regular file (`cmdline` or `fdinfo/<N>`) and what follows
/// that file when the absolute path `named` goes on past it, and `None`
/// when it ends at it or names no such file. `named` is the path with its
/// `..` resolved ([`resolved_procfs_dotdot`]), which drops the `.` and
/// slashes after its last name; `spelled`, the same path before that
/// resolution, keeps them.
pub(crate) fn past_a_task_file(
    named: &Path,
    spelled: &Path,
) -> Option<(BoundTask, TaskFile, PastFile)> {
    if !named.is_absolute() {
        return None;
    }
    let mut parts = Vec::new();
    let mut found = None;
    for component in named.components() {
        match (component, found) {
            (Component::RootDir | Component::CurDir, _) => {}
            (Component::Normal(part), None) => {
                parts.push(part);
                found = task_regular_file(&parts);
            }
            (Component::Normal(_) | Component::ParentDir, Some((task, file))) => {
                return Some((task, file, PastFile::Component));
            }
            (Component::ParentDir | Component::Prefix(_), _) => return None,
        }
    }
    let (task, file) = found?;
    // The `.` and slashes after the path's last name.
    let bytes = spelled.as_os_str().as_bytes();
    let mut tail = bytes;
    while let Some(index) = tail.iter().rposition(|byte| *byte == b'/') {
        match &tail[index + 1..] {
            b"" | b"." => tail = &tail[..index],
            _ => break,
        }
    }
    let tail = &bytes[tail.len()..];
    if tail.is_empty() {
        None
    } else if tail
        .split(|byte| *byte == b'/')
        .any(|segment| segment == b".")
    {
        Some((task, file, PastFile::Component))
    } else {
        Some((task, file, PastFile::Slashes))
    }
}

/// Whether `sysno`, with argument `values`, looks its path up whole, where
/// Linux's walk fails a last component that must be a directory and is a
/// file with ENOTDIR before the call acts (`lookup_last`, fs/namei.c), and
/// does not create, link, rename or remove that last name. Those answer a
/// trailing slash after a file otherwise: `O_CREAT` with EISDIR
/// (`lookup_fast_for_open`, fs/namei.c:3733-3735 at v6.17), a name to
/// create that exists with EEXIST (`filename_create`, :4137-4141, through
/// `lookup_one_qstr_excl`, :1709-1712), and a removal or a rename after
/// checks on its directory.
fn looks_up_whole_path(sysno: Sysno, values: &[usize; 6]) -> bool {
    match sysno {
        #[cfg(target_arch = "x86_64")]
        Sysno::open => values[1] as i32 & libc::O_CREAT == 0,
        Sysno::openat => values[2] as i32 & libc::O_CREAT == 0,
        #[cfg(target_arch = "x86_64")]
        Sysno::creat
        | Sysno::mkdir
        | Sysno::mknod
        | Sysno::rmdir
        | Sysno::unlink
        | Sysno::symlink
        | Sysno::rename
        | Sysno::link => false,
        Sysno::mkdirat
        | Sysno::mknodat
        | Sysno::unlinkat
        | Sysno::symlinkat
        | Sysno::renameat
        | Sysno::renameat2
        | Sysno::linkat => false,
        _ => true,
    }
}

/// The reason a lookup of another guest process's `path` is refused.
pub(crate) fn other_process_refusal_reason(path: &Path) -> String {
    format!(
        "another guest process's {} cannot be served: on this backend /proc/<pid> names the \
         host task with that number, if any, not the guest's process \
         (https://github.com/rrnewton/hermit/issues/4564)",
        path.display()
    )
}

/// The reason a descriptor's fdinfo of Hermit's own init, at `path`, is
/// refused by name where the guest's procfs is not the kernel's. On KVM pid 1
/// is the supervisor, whose descriptors no guest process holds and whose
/// descriptor table changes with host timing (see the module
/// documentation), so no fixed answer is Linux's; the backend's own procfs
/// would answer from the supervisor's table, which is host state.
pub(crate) fn init_fdinfo_refusal_reason(path: &Path) -> String {
    format!(
        "{}, a descriptor's fdinfo of Hermit's own init, cannot be served: on this backend its \
         descriptors are the supervisor's, which no guest process holds, and which of them \
         exist changes with host timing (https://github.com/rrnewton/hermit/issues/4071)",
        path.display()
    )
}

/// The reason the call `sysno` is refused for `path`, its second path, which
/// goes on past another guest process's `cmdline` or names a task the process
/// table does not hold, once the kernel beneath answers ENOENT: Linux walks
/// the call's first path before it, and that ENOENT does not say whether, or
/// how, the first path failed (see the module documentation).
pub(crate) fn second_path_refusal_reason(sysno: Sysno, path: &Path) -> String {
    format!(
        "{sysno} cannot be answered on this backend for {}, the call's second path: Linux \
         walks the first path before it, and the backend's ENOENT does not say whether or how \
         the first path failed (https://github.com/rrnewton/hermit/issues/4564)",
        path.display()
    )
}

/// The most bytes `getname` copies of a path, its NUL included (`PATH_MAX`,
/// include/uapi/linux/limits.h:13 at v6.17).
const PATH_MAX: usize = 4096;
/// The most bytes `import_xattr_name` copies of an extended attribute's
/// name, its NUL included: `struct xattr_name` (fs/internal.h:269-271),
/// `XATTR_NAME_MAX + 1` (include/uapi/linux/limits.h:15).
const XATTR_NAME_SIZE: usize = 256;
/// The largest extended attribute value (`XATTR_SIZE_MAX`,
/// include/uapi/linux/limits.h:16).
const XATTR_SIZE_MAX: usize = 65536;

/// Reads `bytes.len()` bytes at `address` from `memory` with the guest's own
/// access, as Linux's `copy_from_user` copies them: EFAULT where any of them
/// cannot be read.
fn read_guest_bytes<M: MemoryAccess>(
    memory: &M,
    address: usize,
    bytes: &mut [u8],
) -> Result<(), Errno> {
    if bytes.is_empty() {
        return Ok(());
    }
    let at = Addr::<u8>::from_raw(address).ok_or(Errno::EFAULT)?;
    memory
        .read_exact_with_user_access(at, bytes)
        .map_err(|_| Errno::EFAULT)
}

/// The length of the NUL-terminated string at `address` in `memory` as
/// Linux's `strncpy_from_user` copies it with room for `limit` bytes:
/// `Some` length when a NUL ends it within the limit, `None` when none
/// does, and EFAULT when a byte before its NUL, within the limit, cannot be
/// read ([`read_guest_string`], which reads a page at a time, so that a
/// string that ends before an unreadable page is read whole, as Linux reads
/// it).
fn guest_string_length<M: MemoryAccess>(
    memory: &M,
    address: usize,
    limit: usize,
) -> Result<Option<usize>, Errno> {
    let string = read_guest_string(memory, address as u64, limit)?;
    Ok((string.len() < limit).then_some(string.len()))
}

/// What `getname` answers for the path or name at `address`
/// (`getname_flags`, fs/namei.c:157-171 and :207-211 at v6.17): EFAULT where
/// it cannot be read, ENOENT where it is empty, and ENAMETOOLONG where no
/// NUL ends it within `PATH_MAX` bytes; `None` for a path it copies.
fn getname_error<M: MemoryAccess>(memory: &M, address: usize) -> Option<Errno> {
    match guest_string_length(memory, address, PATH_MAX) {
        Err(errno) => Some(errno),
        Ok(Some(0)) => Some(Errno::ENOENT),
        Ok(Some(_)) => None,
        Ok(None) => Some(Errno::ENAMETOOLONG),
    }
}

/// What `may_mknod` answers for a mknod of the mode `mode`, before the walk
/// (fs/namei.c:4242-4257, called at :4268 at v6.17): EPERM for a directory,
/// EINVAL for a type that is no file, and `None` for any other type, whose
/// call goes on to the walk.
fn may_mknod(mode: usize) -> Option<Errno> {
    match mode as u32 & libc::S_IFMT {
        0 | libc::S_IFREG | libc::S_IFCHR | libc::S_IFBLK | libc::S_IFIFO | libc::S_IFSOCK => None,
        libc::S_IFDIR => Some(Errno::EPERM),
        _ => Some(Errno::EINVAL),
    }
}

/// What `import_xattr_name` answers for the extended attribute name at
/// `address` (fs/xattr.c:589-598 at v6.17): EFAULT where it cannot be read,
/// ERANGE where it is empty or no NUL ends it within `XATTR_NAME_SIZE`
/// bytes, and `None` for a name it copies.
fn xattr_name_error<M: MemoryAccess>(memory: &M, address: usize) -> Option<Errno> {
    match guest_string_length(memory, address, XATTR_NAME_SIZE) {
        Err(errno) => Some(errno),
        Ok(Some(0) | None) => Some(Errno::ERANGE),
        Ok(Some(_)) => None,
    }
}

/// Linux's answer to the call `sysno`, with argument `values`, from the checks
/// it makes of the call's own arguments before it walks any path, where one
/// fails or answers the call without a walk; `None` where the call goes on to
/// its walk. These are the checks the backend beneath
/// [`Detcore::decide_other_process_procfs`] (KVM's) makes after its own walk,
/// makes otherwise, or does not make (see the module documentation). The
/// other calls whose paths it decides it checks first, as Linux does, and its
/// answer stands (`issue_substituted`): an `access` mode or flags, open flags,
/// a `readlink` size, a negative `truncate` length, and the flags of
/// `unlinkat`, `fchmodat2`, `fchownat`, `linkat`, `newfstatat` and
/// `inotify_add_watch`, with its mask and descriptor. None of those, and none
/// of these, is a refusal for permission but mknod's of a directory, so the
/// EACCES or EPERM the backend answers after this is its own refusal. The
/// backend also refuses some arguments Linux accepts, `linkat`'s
/// AT_EMPTY_PATH, renameat2's RENAME_WHITEOUT, `newfstatat`'s AT_STATX_
/// flags, and any execveat flag or directory descriptor, as it does whatever
/// the path: those are its gaps, not checks Linux makes, and its answer
/// stands (<https://github.com/rrnewton/reverie/issues/1222>).
fn linux_answer_before_the_walk<M: MemoryAccess>(
    memory: &M,
    sysno: Sysno,
    values: &[usize; 6],
) -> Option<Result<i64, Errno>> {
    match sysno {
        #[cfg(target_arch = "x86_64")]
        Sysno::mknod => may_mknod(values[1]).map(Err),
        Sysno::mknodat => may_mknod(values[2]).map(Err),
        // `do_renameat2`, before either walk (fs/namei.c:5183-5188 at v6.17):
        // an unknown flag, or RENAME_EXCHANGE with RENAME_NOREPLACE or
        // RENAME_WHITEOUT, is EINVAL.
        Sysno::renameat2 => {
            let flags = values[4] as u32;
            let unknown =
                flags & !(libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE | libc::RENAME_WHITEOUT);
            let exchange_with_another = flags & libc::RENAME_EXCHANGE != 0
                && flags & (libc::RENAME_NOREPLACE | libc::RENAME_WHITEOUT) != 0;
            (unknown != 0 || exchange_with_another).then_some(Err(Errno::EINVAL))
        }
        // `do_symlinkat` fails the target's `getname` before its walk
        // (fs/namei.c:4744-4747 at v6.17).
        #[cfg(target_arch = "x86_64")]
        Sysno::symlink => getname_error(memory, values[0]).map(Err),
        Sysno::symlinkat => getname_error(memory, values[0]).map(Err),
        // utimensat reads the times first, EFAULT where it cannot, and
        // returns 0 for two UTIME_OMIT without looking at the path
        // (fs/utimes.c:148-156 at v6.17); `do_utimes_path` then refuses an
        // unknown flag with EINVAL before its walk (:86-87).
        Sysno::utimensat => {
            if values[2] != 0 {
                // Two `struct __kernel_timespec`, each a 64-bit `tv_sec`
                // then a 64-bit `tv_nsec`.
                let mut times = [0u8; 32];
                if let Err(errno) = read_guest_bytes(memory, values[2], &mut times) {
                    return Some(Err(errno));
                }
                let nanoseconds = |offset: usize| {
                    i64::from_ne_bytes(times[offset..offset + 8].try_into().expect("eight bytes"))
                };
                if nanoseconds(8) == libc::UTIME_OMIT && nanoseconds(24) == libc::UTIME_OMIT {
                    return Some(Ok(0));
                }
            }
            let flags = values[3] as i32;
            (flags & !(libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) != 0)
                .then_some(Err(Errno::EINVAL))
        }
        // `do_statx` refuses a reserved mask bit and both synchronisation
        // flags (fs/stat.c:758-761 at v6.17), and `vfs_statx` an unknown
        // flag (:348-350), each with EINVAL before the walk. With
        // AT_EMPTY_PATH, an empty pathname on a descriptor is the descriptor
        // itself (`getname_maybe_null`, fs/namei.c:225-242; fs/stat.c:818-821):
        // `do_statx_fd` refuses the same mask bit and flags (:782-785), but
        // no unknown flag.
        Sysno::statx => {
            let flags = values[2] as i32;
            let mask = values[3] as u32;
            let known = libc::AT_SYMLINK_NOFOLLOW
                | libc::AT_NO_AUTOMOUNT
                | libc::AT_EMPTY_PATH
                | libc::AT_STATX_SYNC_TYPE;
            let the_descriptor = flags & libc::AT_EMPTY_PATH != 0
                && values[0] as i32 >= 0
                && guest_string_length(memory, values[1], 1) == Ok(Some(0));
            let refused = mask & libc::STATX__RESERVED as u32 != 0
                || flags & libc::AT_STATX_SYNC_TYPE == libc::AT_STATX_SYNC_TYPE
                || (!the_descriptor && flags & !known != 0);
            refused.then_some(Err(Errno::EINVAL))
        }
        // The name, before the walk (fs/xattr.c:845 and :1076 at v6.17).
        Sysno::getxattr | Sysno::lgetxattr | Sysno::removexattr | Sysno::lremovexattr => {
            xattr_name_error(memory, values[1]).map(Err)
        }
        // `setxattr_copy`, before the walk (fs/xattr.c:604-627, called at
        // :701 at v6.17): an unknown flag is EINVAL, then the name, then a
        // value larger than `XATTR_SIZE_MAX` is E2BIG and one that cannot be
        // read EFAULT.
        Sysno::setxattr | Sysno::lsetxattr => {
            if values[4] as u32 & !((libc::XATTR_CREATE | libc::XATTR_REPLACE) as u32) != 0 {
                return Some(Err(Errno::EINVAL));
            }
            if let Some(errno) = xattr_name_error(memory, values[1]) {
                return Some(Err(errno));
            }
            let size = values[3];
            if size > XATTR_SIZE_MAX {
                return Some(Err(Errno::E2BIG));
            }
            let mut value = vec![0; size];
            read_guest_bytes(memory, values[2], &mut value)
                .err()
                .map(Err)
        }
        // `do_open_execat` refuses an unknown flag with EINVAL before it opens
        // the file (fs/exec.c:775-777 at v7.1.3), and only then consumes the
        // path's `getname_uflags` error, where an empty path is the descriptor
        // itself with AT_EMPTY_PATH (LOOKUP_EMPTY, fs/namei.c:228-233). That is
        // the order of Linux 7.1, whose native answers and ptrace's the tests
        // compare against. Linux 6.17 failed the path's getname first
        // (`do_execveat_common`, fs/exec.c:1792-1793 at v6.17); the two differ
        // only for a call with both an unknown flag and a bad path.
        // `utime` reads its times first (`get_user`, EFAULT), and `utimes`
        // copies its two `timeval`s (EFAULT) and checks their microseconds
        // (EINVAL), each before the walk (fs/utimes.c:214-217 and :175-186 at
        // v6.17; `legacy_times_error`).
        #[cfg(target_arch = "x86_64")]
        Sysno::utime => legacy_times_error(memory, values[1] as u64, false).map(Err),
        #[cfg(target_arch = "x86_64")]
        Sysno::utimes => legacy_times_error(memory, values[1] as u64, true).map(Err),
        Sysno::execveat => {
            let flags = values[4] as i32;
            let known = libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH | libc::AT_EXECVE_CHECK;
            if flags & !known != 0 {
                return Some(Err(Errno::EINVAL));
            }
            match getname_error(memory, values[1]) {
                Some(Errno::ENOENT) if flags & libc::AT_EMPTY_PATH != 0 => None,
                error => error.map(Err),
            }
        }
        _ => None,
    }
}

/// Linux's answer to the call `sysno`, with argument `values`, from every
/// check it makes of the call's own arguments before it walks any path:
/// [`linux_answer_before_the_walk`]'s, and those the backend beneath makes
/// first itself where it is given the call (see that function), each with
/// EINVAL: open flags `build_open_flags` refuses (v6.17 fs/open.c:1281 and
/// the rest of `build_open_flags`); an `access` mode or flags
/// (`do_faccessat`, :475 and :478); a `readlink` size that is not a
/// positive `int` (fs/stat.c:571); a negative `truncate` length
/// (`do_sys_truncate`); an unknown flag of `unlinkat`, `fchmodat2`,
/// `fchownat`, `linkat` or `newfstatat` (fs/namei.c, fs/open.c, and
/// `vfs_statx`, fs/stat.c:348). `None` where the call goes on to its walk.
/// For a call whose walk Detcore decides before the backend sees the call
/// (package K slice K10), so that its walk's error does not come first.
pub(super) fn arguments_refused_before_the_walk<M: MemoryAccess>(
    memory: &M,
    sysno: Sysno,
    values: &[usize; 6],
) -> Option<Result<i64, Errno>> {
    if let Some(answer) = linux_answer_before_the_walk(memory, sysno, values) {
        return Some(answer);
    }
    let flag = |index: usize| values[index] as i32;
    let access = |mode: usize, flags: i32| {
        mode as i32 & !(libc::S_IRWXO as i32) != 0
            || flags & !(libc::AT_EACCESS | libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) != 0
    };
    let refused = match sysno {
        #[cfg(target_arch = "x86_64")]
        Sysno::open => {
            return super::rendered_procfs::open_flags_refused(
                nix::fcntl::OFlag::from_bits_retain(flag(1)),
            )
            .map(Err);
        }
        Sysno::openat => {
            return super::rendered_procfs::open_flags_refused(
                nix::fcntl::OFlag::from_bits_retain(flag(2)),
            )
            .map(Err);
        }
        #[cfg(target_arch = "x86_64")]
        Sysno::access => access(values[1], 0),
        Sysno::faccessat => access(values[2], 0),
        Sysno::faccessat2 => access(values[2], flag(3)),
        #[cfg(target_arch = "x86_64")]
        Sysno::readlink => flag(2) <= 0,
        Sysno::readlinkat => flag(3) <= 0,
        Sysno::truncate => (values[1] as i64) < 0,
        Sysno::unlinkat => flag(2) & !libc::AT_REMOVEDIR != 0,
        Sysno::fchmodat2 => flag(3) & !(libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) != 0,
        Sysno::fchownat => flag(4) & !(libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) != 0,
        Sysno::linkat => flag(4) & !(libc::AT_SYMLINK_FOLLOW | libc::AT_EMPTY_PATH) != 0,
        Sysno::newfstatat => {
            flag(3)
                & !(libc::AT_SYMLINK_NOFOLLOW
                    | libc::AT_NO_AUTOMOUNT
                    | libc::AT_EMPTY_PATH
                    | libc::AT_STATX_SYNC_TYPE)
                != 0
        }
        _ => false,
    };
    refused.then_some(Err(Errno::EINVAL))
}

/// The call the kernel beneath is given for the call `sysno`, with argument
/// `values` and its path at argument `path` (which is then respelled): the
/// call itself, but for `utime` and `utimes`, which a backend need not
/// implement (KVM's does not), `utimensat` with no times, as Detcore's own
/// handlers issue them (`handle_utime`, `handle_utimes`). Their times are
/// checked already ([`linux_answer_before_the_walk`]), and a respelled walk
/// fails before any time is used.
fn respelled_call(sysno: Sysno, values: [usize; 6], path: usize) -> (Sysno, [usize; 6], usize) {
    match sysno {
        #[cfg(target_arch = "x86_64")]
        Sysno::utime | Sysno::utimes => (
            Sysno::utimensat,
            [libc::AT_FDCWD as usize, values[path], 0, 0, 0, 0],
            1,
        ),
        _ => (sysno, values, path),
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// The answer, where Detcore must give it, to a call that looks up
    /// another guest process's `cmdline`, descriptor link, `fdinfo` or
    /// descriptor directory ([`named_task_file`]) on a backend whose
    /// `/proc/<pid>` is not the guest process with that number (see the module
    /// documentation): Linux's answer from the call's own arguments where it
    /// makes one before the walk, decided from them
    /// ([`linux_answer_before_the_walk`]) or the kernel's beneath; otherwise
    /// ENOENT for a task Detcore's process table does not
    /// hold, and a refusal by name for one that belongs to another guest
    /// process, or for either as the second path of a call with two. `None`
    /// on every other backend, for every other path, for the caller's own
    /// process, which its handler answers as before, and for Hermit's own
    /// init, which the backend answers, apart from a descriptor's fdinfo,
    /// refused by name (see the module documentation).
    pub(crate) async fn other_process_procfs_answer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Option<Result<i64, Error>> {
        if crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend)
            || !matches!(looked_up(call.number()), Some(Lookups::Paths(_)))
        {
            return None;
        }
        Box::pin(self.decide_other_process_procfs(guest, call)).await
    }

    /// [`Self::other_process_procfs_answer`] for a call with path operands,
    /// on a backend whose `/proc/<pid>` is not the guest's process.
    async fn decide_other_process_procfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Option<Result<i64, Error>> {
        let (sysno, args) = call.into_parts();
        let Some(Lookups::Paths(operands)) = looked_up(sysno) else {
            return None;
        };
        // The caller's own numbers, as its numeric `/proc` spellings name
        // them; none in a nested PID namespace, whose numbers Detcore does
        // not know, where the kernel keeps deciding.
        let (own_pid, _) = self.procfs_caller_ids(guest)?;
        let values = [
            args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
        ];
        for (position, (dirfd, path)) in operands.into_iter().enumerate() {
            let Some(spelling): Option<PathBuf> =
                PathPtr::from_ptr(values[path] as *const libc::c_char)
                    .and_then(|address| address.read(&guest.memory()).ok())
            else {
                continue;
            };
            // An empty pathname names no file of its own: the descriptor
            // itself with AT_EMPTY_PATH, or nothing (see the module
            // documentation).
            let empty = spelling.as_os_str().is_empty();
            let dirfd = dirfd.map_or(libc::AT_FDCWD, |position| values[position] as i32);
            Self::prepare_guest_cwd(guest, dirfd, &spelling).await;
            let Some(observed) = Self::observed_path(guest, dirfd, &spelling)
                .ok()
                .filter(|observed| observed.is_absolute())
            else {
                continue;
            };
            // Through the caller's own descriptor links to the `/proc`
            // directories Detcore serves, as the call's injection respells it
            // (`Detcore::decided_path`), then back through `..` over the
            // directories every procfs has, as Linux's walk resolves it, which
            // looks up each task it passes on the way.
            let observed = if Self::starts_at_a_recorded_link(guest, dirfd, &spelling) {
                observed
            } else {
                Self::decided_path(guest, &observed).map_or(observed, |(decided, _)| decided)
            };
            let (named, walked) = match resolved_procfs_dotdot(&observed) {
                Some((resolved, walked)) => (resolved, walked),
                None => (observed.clone(), Vec::new()),
            };
            // An empty pathname's observed path is its descriptor's own, with
            // a slash after it that no spelling holds, so it never goes on
            // past a file.
            let past_the_file = if empty {
                None
            } else {
                past_a_task_file(&named, &observed)
            };
            let (task, file, past) = match past_the_file {
                Some((task, file, past)) => (task, file, Some(past)),
                None => match named_task_file(&named) {
                    Some((task, file)) => (task, file, None),
                    None => continue,
                },
            };
            if task.process == own_pid {
                continue;
            }
            let answer = crate::tool_global::procfs_task(guest, task).await;
            let found = Self::named_tasks_exist(guest, &walked).await && answer.exists;
            let verdict = if !found {
                Verdict::Absent
            } else {
                // The process the number names (a thread's number names its
                // process), or none for Hermit's own init, which is left to
                // the backend (see the module documentation), apart from a
                // descriptor's fdinfo, which the backend no longer answers.
                match crate::tool_global::task_stat_facts(guest, task)
                    .await
                    .process
                {
                    None if file == TaskFile::DescriptorInfo => Verdict::InitDescriptorInfo,
                    None => continue,
                    Some(process) if process == self.own_detpid(guest).as_raw() => continue,
                    // Another process's `cmdline` or `environ` itself, by a
                    // pathname, which the rendered path serves where Detcore
                    // holds the strings its image's `execve` passed, or the
                    // process has exited (`Detcore::kept_by_kernel`). An
                    // empty pathname names the descriptor, answered below.
                    Some(process)
                        if !empty
                            && past.is_none()
                            && matches!(file, TaskFile::Cmdline | TaskFile::Environ)
                            && (answer.without_mm
                                || crate::tool_global::process_exec_strings(
                                    guest,
                                    detcore_model::pid::DetPid::from_raw(process),
                                )
                                .await
                                .1
                                .is_some()) =>
                    {
                        continue;
                    }
                    // Past another process's cmdline or environ, which every
                    // task has (`past_a_task_file`).
                    Some(_) => match (file, past) {
                        (TaskFile::Cmdline | TaskFile::Environ, Some(PastFile::Component)) => {
                            Verdict::NotADirectory
                        }
                        (TaskFile::Cmdline | TaskFile::Environ, Some(PastFile::Slashes))
                            if looks_up_whole_path(sysno, &values) =>
                        {
                            Verdict::NotADirectory
                        }
                        _ => Verdict::OtherProcess,
                    },
                }
            };
            if empty {
                // Linux checks the call's own arguments for an operation on
                // the descriptor as well (see `linux_answer_before_the_walk`).
                if let Some(answer) = linux_answer_before_the_walk(&guest.memory(), sysno, &values)
                {
                    return Some(answer.map_err(Into::into));
                }
                // Otherwise the call acts on the descriptor itself, with
                // AT_EMPTY_PATH, or fails in the backend's getname without it:
                // it looks no name up, so the descriptor's handlers and the
                // backend answer it as they answer the descriptor's other
                // operations. An execveat of the descriptor is the exception.
                if sysno != Sysno::execveat {
                    continue;
                }
                return Some(
                    self.execveat_of_another_process_procfs_descriptor(guest, file, &named)
                        .await,
                );
            }
            // The kernel judges the call's own arguments first, on the task's
            // directory spelled as a name procfs does not have; a call it lets
            // reach the walk fails there with ENOENT.
            let prefix = named.components().take(3).collect::<PathBuf>();
            let Some(dashed) = dashed_spelling(
                Path::new("/proc"),
                &prefix,
                &named,
                spelling.as_os_str().as_encoded_bytes(),
            ) else {
                continue;
            };
            // The checks Linux makes of the call's own arguments before any
            // walk, where the backend makes them after its walk, otherwise or
            // not at all (see the module documentation): decided from the
            // arguments, before the respelled walk can fail first.
            if let Some(answer) = linux_answer_before_the_walk(&guest.memory(), sysno, &values) {
                return Some(answer.map_err(Into::into));
            }
            let (issued, issued_values, issued_path) = respelled_call(sysno, values, path);
            let answer = self
                .answer_for_absent_name(guest, issued, issued_values, issued_path, dashed)
                .await;
            if !matches!(answer, Err(Error::Errno(Errno::ENOENT))) {
                return Some(answer);
            }
            // The verdict's errno, and the reason it is refused by name
            // instead, if it is; a run that allows unsupported operations
            // gets the errno then.
            let (errno, refusal) = match verdict {
                Verdict::Absent => (Errno::ENOENT, None),
                Verdict::NotADirectory => (Errno::ENOTDIR, None),
                Verdict::OtherProcess => {
                    (Errno::EACCES, Some(other_process_refusal_reason(&named)))
                }
                Verdict::InitDescriptorInfo => {
                    (Errno::EACCES, Some(init_fdinfo_refusal_reason(&named)))
                }
            };
            // The second path of a call with two: Linux walks the first one
            // before it, and the ENOENT may be the first one's (see the module
            // documentation).
            let refusal = refusal
                .or_else(|| (position > 0).then(|| second_path_refusal_reason(sysno, &named)));
            let Some(reason) = refusal else {
                return Some(Err(errno.into()));
            };
            report_refusal_reason(
                self.cfg.panic_on_unsupported_syscalls,
                &format!(
                    "[dtid {}] {sysno} refused: {reason}",
                    guest.thread_state().dettid
                ),
            );
            if !self.cfg.panic_on_unsupported_syscalls {
                crate::tool_global::report_unsupported_syscall(guest, sysno).await;
            }
            return Some(
                self.refuse_unserviceable_operation_because(guest, sysno, errno, &reason)
                    .await,
            );
        }
        None
    }

    /// The answer to an execveat of a descriptor (AT_EMPTY_PATH with an empty
    /// pathname, the flags valid) open on another guest process's `file` at
    /// `named`, or on one of a task the process table no longer holds. KVM's
    /// backend cannot execute a descriptor (it declines any execveat flag or
    /// directory descriptor with EOPNOTSUPP), so Detcore answers. Linux opens
    /// the descriptor's own file to execute it (`do_open_execat` with
    /// LOOKUP_EMPTY), and a procfs regular file (`cmdline`, `fdinfo/<N>`) or
    /// directory (`fd`, `fdinfo`) cannot be executed: `may_open` refuses
    /// MAY_EXEC on a directory (fs/namei.c:3452-3456 at v6.17), and on a file
    /// of a noexec mount (:3469-3471), and `generic_permission` on a file with
    /// no execute bit, root's too (:498-506), each with EACCES, whether or
    /// not the task still exists. A descriptor opened
    /// through a descriptor link holds the link's target, which Detcore does
    /// not know: refused by name.
    async fn execveat_of_another_process_procfs_descriptor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        file: TaskFile,
        named: &Path,
    ) -> Result<i64, Error> {
        if file != TaskFile::DescriptorLink {
            return Err(Errno::EACCES.into());
        }
        let reason = format!(
            "execveat of a descriptor open on another guest process's {} cannot be answered on \
             this backend: the descriptor holds that link's target, which Detcore does not know \
             (https://github.com/rrnewton/hermit/issues/4564)",
            named.display()
        );
        report_refusal_reason(
            self.cfg.panic_on_unsupported_syscalls,
            &format!(
                "[dtid {}] execveat refused: {reason}",
                guest.thread_state().dettid
            ),
        );
        if !self.cfg.panic_on_unsupported_syscalls {
            crate::tool_global::report_unsupported_syscall(guest, Sysno::execveat).await;
        }
        self.refuse_unserviceable_operation_because(guest, Sysno::execveat, Errno::EACCES, &reason)
            .await
    }
}

/// What [`Detcore::decide_other_process_procfs`] answers a lookup that the
/// kernel beneath lets reach its walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verdict {
    /// A task the process table does not hold, or one a `..` walk passes
    /// that it does not hold: ENOENT, Linux's answer. Refused by name as the
    /// second path of a call with two ([`second_path_refusal_reason`]).
    Absent,
    /// A path that goes on past another guest process's `cmdline` where
    /// Linux's walk fails with ENOTDIR ([`PastFile`]). Refused by name as the
    /// second path of a call with two ([`second_path_refusal_reason`]).
    NotADirectory,
    /// A file of another guest process: refused by name.
    OtherProcess,
    /// A descriptor's fdinfo of Hermit's own init: refused by name, as no
    /// guest process holds its descriptors ([`init_fdinfo_refusal_reason`]).
    InitDescriptorInfo,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(process: i32, thread: Option<i32>) -> BoundTask {
        BoundTask { process, thread }
    }

    /// The paths that name a process's `cmdline`, a descriptor link (or a path
    /// below one), a descriptor's `fdinfo` or a descriptor directory by
    /// number, through `/proc/<pid>` or `/proc/<pid>/task/<tid>`, name that
    /// task and file.
    #[test]
    fn names_a_tasks_cmdline_descriptor_links_and_fdinfo() {
        for (path, expected) in [
            ("/proc/5/cmdline", (task(5, None), TaskFile::Cmdline)),
            (
                "/proc/5/task/7/cmdline",
                (task(5, Some(7)), TaskFile::Cmdline),
            ),
            ("/proc/5/fd/1", (task(5, None), TaskFile::DescriptorLink)),
            ("/proc/5/fd/0/", (task(5, None), TaskFile::DescriptorLink)),
            (
                "/proc/5/fd/12/below",
                (task(5, None), TaskFile::DescriptorLink),
            ),
            (
                "/proc/5/task/5/fd/3",
                (task(5, Some(5)), TaskFile::DescriptorLink),
            ),
            (
                "/proc/5/fdinfo/1",
                (task(5, None), TaskFile::DescriptorInfo),
            ),
            (
                "/proc/9/task/11/fdinfo/0",
                (task(9, Some(11)), TaskFile::DescriptorInfo),
            ),
            ("//proc/./1/cmdline", (task(1, None), TaskFile::Cmdline)),
            ("/proc/5/environ", (task(5, None), TaskFile::Environ)),
            (
                "/proc/5/task/7/environ",
                (task(5, Some(7)), TaskFile::Environ),
            ),
            ("/proc/5/fd", (task(5, None), TaskFile::DescriptorDirectory)),
            (
                "/proc/5/fdinfo/",
                (task(5, None), TaskFile::DescriptorDirectory),
            ),
            (
                "/proc/5/task/7/fd",
                (task(5, Some(7)), TaskFile::DescriptorDirectory),
            ),
        ] {
            assert_eq!(named_task_file(Path::new(path)), Some(expected), "{path}");
        }
    }

    /// Every other path names none of them: the caller's own spellings,
    /// numbers procfs does not look up (a leading zero, a sign, a name), the
    /// task's own directories, other files of a task, a `..` spelling, a
    /// relative one, and paths outside `/proc`.
    #[test]
    fn names_nothing_else() {
        for path in [
            "/proc/self/cmdline",
            "/proc/thread-self/fd/1",
            "/proc/self/task/5/cmdline",
            "/proc/05/cmdline",
            "/proc/5/fd/01",
            "/proc/5/fdinfo/01",
            "/proc/+5/cmdline",
            "/proc/5/task/x/cmdline",
            "/proc/5",
            "/proc/5/task",
            "/proc/5/fdinfo/1/x",
            "/proc/5/status",
            "/proc/5/environs",
            "/proc/5/task/7",
            "/proc/5/task/7/status",
            "/proc/5/../5/cmdline",
            "proc/5/cmdline",
            "/proc",
            "/proc/cmdline",
            "/tmp/proc/5/cmdline",
        ] {
            assert_eq!(named_task_file(Path::new(path)), None, "{path}");
        }
    }

    /// A path that goes on past a task's `cmdline` or `fdinfo/<N>`: through a
    /// further component, `.` and `..` included, or with slashes alone, which
    /// only the spelling from before the `..` resolution shows. A path that
    /// ends at the file, or goes on past any other name, does not.
    #[test]
    fn names_a_path_past_a_tasks_regular_file() {
        use PastFile::Component as Further;
        use PastFile::Slashes;
        let cmdline = |past| Some((task(5, None), TaskFile::Cmdline, past));
        let fdinfo = |past| Some((task(5, None), TaskFile::DescriptorInfo, past));
        for (named, spelled, expected) in [
            ("/proc/5/cmdline/", "/proc/5/cmdline/", cmdline(Slashes)),
            ("/proc/5/cmdline//", "/proc/5/cmdline//", cmdline(Slashes)),
            ("/proc/5/cmdline/.", "/proc/5/cmdline/.", cmdline(Further)),
            ("/proc/5/cmdline/./", "/proc/5/cmdline/./", cmdline(Further)),
            ("/proc/5/cmdline/x", "/proc/5/cmdline/x", cmdline(Further)),
            ("/proc/5/cmdline/..", "/proc/5/cmdline/..", cmdline(Further)),
            (
                "/proc/5/environ/",
                "/proc/5/environ/",
                Some((task(5, None), TaskFile::Environ, Slashes)),
            ),
            (
                "/proc/5/environ/x",
                "/proc/5/environ/x",
                Some((task(5, None), TaskFile::Environ, Further)),
            ),
            ("/proc/5/fdinfo/3/", "/proc/5/fdinfo/3/", fdinfo(Slashes)),
            ("/proc/5/fdinfo/3/x", "/proc/5/fdinfo/3/x", fdinfo(Further)),
            (
                "/proc/5/cmdline",
                "/proc/5/fd/../cmdline/",
                cmdline(Slashes),
            ),
            (
                "/proc/5/cmdline",
                "/proc/5/fd/../cmdline/.",
                cmdline(Further),
            ),
            (
                "/proc/5/task/7/cmdline/",
                "/proc/5/task/7/cmdline/",
                Some((task(5, Some(7)), TaskFile::Cmdline, Slashes)),
            ),
        ] {
            assert_eq!(
                past_a_task_file(Path::new(named), Path::new(spelled)),
                expected,
                "{spelled}"
            );
        }
        for path in [
            "/proc/5/cmdline",
            "/proc/5/environ",
            "/proc/5/fdinfo/3",
            "/proc/5/fd/3/",
            "/proc/5/fd/3/x",
            "/proc/5/fdinfo/",
            "/proc/5/status/",
            "/proc/5/",
            "/proc/self/cmdline/",
            "/proc/05/cmdline/",
            "/tmp/proc/5/cmdline/",
        ] {
            assert_eq!(
                past_a_task_file(Path::new(path), Path::new(path)),
                None,
                "{path}"
            );
        }
    }

    /// The calls that look their path up whole, and the ones that create,
    /// link, rename or remove its last name, which answer a trailing slash
    /// after a file otherwise in Linux.
    #[test]
    fn tells_whole_path_lookups_from_calls_on_the_last_name() {
        let at = |flags: i32| [0, 0, flags as usize, 0, 0, 0];
        assert!(looks_up_whole_path(Sysno::openat, &at(libc::O_RDONLY)));
        assert!(looks_up_whole_path(
            Sysno::openat,
            &at(libc::O_RDONLY | libc::O_DIRECTORY)
        ));
        assert!(!looks_up_whole_path(
            Sysno::openat,
            &at(libc::O_WRONLY | libc::O_CREAT)
        ));
        for sysno in [
            Sysno::newfstatat,
            Sysno::statx,
            Sysno::faccessat2,
            Sysno::readlinkat,
            Sysno::execveat,
            Sysno::utimensat,
            Sysno::fchmodat,
            Sysno::truncate,
            Sysno::getxattr,
            Sysno::statfs,
            Sysno::chdir,
        ] {
            assert!(looks_up_whole_path(sysno, &[0; 6]), "{sysno}");
        }
        for sysno in [
            Sysno::mkdirat,
            Sysno::mknodat,
            Sysno::unlinkat,
            Sysno::symlinkat,
            Sysno::renameat2,
            Sysno::linkat,
        ] {
            assert!(!looks_up_whole_path(sysno, &[0; 6]), "{sysno}");
        }
    }

    /// The refusals name the path (and for a second path, the call) and the
    /// issue, with the same text on every run for the same spelling.
    #[test]
    fn the_refusal_reason_names_the_path() {
        let reason = other_process_refusal_reason(Path::new("/proc/5/cmdline"));
        assert!(
            reason.starts_with("another guest process's /proc/5/cmdline cannot be served"),
            "{reason}"
        );
        assert!(
            reason.ends_with("(https://github.com/rrnewton/hermit/issues/4564)"),
            "{reason}"
        );
        assert_eq!(
            reason,
            other_process_refusal_reason(Path::new("/proc/5/cmdline"))
        );
        let reason = second_path_refusal_reason(Sysno::linkat, Path::new("/proc/5/cmdline/x"));
        assert!(
            reason.starts_with(
                "linkat cannot be answered on this backend for /proc/5/cmdline/x, the call's \
                 second path"
            ),
            "{reason}"
        );
        assert!(
            reason.ends_with("(https://github.com/rrnewton/hermit/issues/4564)"),
            "{reason}"
        );
        assert_eq!(
            reason,
            second_path_refusal_reason(Sysno::linkat, Path::new("/proc/5/cmdline/x"))
        );
    }

    /// The calls with two paths are `link`, `linkat`, `rename`, `renameat`
    /// and `renameat2`, each with its source first: the calls whose order of
    /// lookups the module documentation cites (`do_linkat`, `do_renameat2`),
    /// on which a second path's refusal rests. A call added with two paths
    /// must be checked against Linux's order before it joins them.
    #[test]
    fn the_calls_with_two_paths_are_link_and_rename() {
        let mut two_paths = crate::syscall_classification::all_pinned_syscalls()
            .filter_map(|sysno| match looked_up(sysno) {
                Some(Lookups::Paths(paths)) if paths.len() > 1 => Some((sysno, paths)),
                _ => None,
            })
            .collect::<Vec<_>>();
        two_paths.sort_by_key(|(sysno, _)| sysno.id());
        let mut expected = vec![
            (Sysno::renameat, vec![(Some(0), 1), (Some(2), 3)]),
            (Sysno::linkat, vec![(Some(0), 1), (Some(2), 3)]),
            (Sysno::renameat2, vec![(Some(0), 1), (Some(2), 3)]),
        ];
        #[cfg(target_arch = "x86_64")]
        expected.extend([
            (Sysno::rename, vec![(None, 0), (None, 1)]),
            (Sysno::link, vec![(None, 0), (None, 1)]),
        ]);
        expected.sort_by_key(|(sysno, _)| sysno.id());
        assert_eq!(two_paths, expected);
    }

    /// Where [`Pages`] begins, on a page boundary.
    const BASE: usize = 0x10_0000;

    /// The size of a page of [`Pages`].
    const PAGE_SIZE: usize = 4096;

    /// Guest memory for the argument checks: three pages at `BASE`, of which
    /// the first two can be read, read as `process_vm_readv` reads them, a
    /// readable prefix, or EFAULT where none is.
    struct Pages(Vec<u8>);

    impl Pages {
        /// The pages, holding each `(offset, bytes)` and `x` elsewhere.
        fn holding(contents: &[(usize, &[u8])]) -> Self {
            let mut bytes = vec![b'x'; 3 * PAGE_SIZE];
            for (offset, content) in contents {
                bytes[*offset..offset + content.len()].copy_from_slice(content);
            }
            Pages(bytes)
        }
    }

    impl MemoryAccess for Pages {
        fn read_vectored(
            &self,
            from: &[std::io::IoSlice],
            to: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            let ([from], [to]) = (from, to) else {
                panic!("one remote extent into one buffer");
            };
            let readable = 2 * PAGE_SIZE;
            let start = (from.as_ptr() as usize)
                .checked_sub(BASE)
                .ok_or(Errno::EFAULT)?;
            let count = from.len().min(to.len()).min(readable.saturating_sub(start));
            if count == 0 {
                return Err(Errno::EFAULT);
            }
            to[..count].copy_from_slice(&self.0[start..start + count]);
            Ok(count)
        }

        fn write_vectored(
            &mut self,
            _: &[std::io::IoSlice],
            _: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            panic!("the argument checks never write")
        }
    }

    /// `may_mknod`: a directory is EPERM and a type that is no file EINVAL,
    /// whatever the permission bits; every file type goes on to the walk.
    #[test]
    fn decides_a_mknod_by_its_type() {
        let memory = Pages::holding(&[]);
        let mknodat = |mode: u32| {
            linux_answer_before_the_walk(
                &memory,
                Sysno::mknodat,
                &[0, BASE, mode as usize, 0, 0, 0],
            )
        };
        assert_eq!(mknodat(libc::S_IFDIR | 0o755), Some(Err(Errno::EPERM)));
        assert_eq!(mknodat(libc::S_IFLNK | 0o777), Some(Err(Errno::EINVAL)));
        assert_eq!(mknodat(0o030000), Some(Err(Errno::EINVAL)));
        assert_eq!(mknodat(libc::S_IFMT), Some(Err(Errno::EINVAL)));
        for mode in [
            0,
            0o600,
            libc::S_IFREG,
            libc::S_IFCHR,
            libc::S_IFBLK,
            libc::S_IFIFO,
            libc::S_IFSOCK | 0o600,
        ] {
            assert_eq!(mknodat(mode), None, "{mode:o}");
        }
        // The mode is a umode_t: the bits above its 16 do not reach the type.
        assert_eq!(mknodat(0x1_0000 | libc::S_IFIFO), None);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            linux_answer_before_the_walk(
                &memory,
                Sysno::mknod,
                &[BASE, (libc::S_IFDIR | 0o700) as usize, 0, 0, 0, 0]
            ),
            Some(Err(Errno::EPERM))
        );
    }

    /// `do_renameat2`: an unknown flag, or RENAME_EXCHANGE with
    /// RENAME_NOREPLACE or RENAME_WHITEOUT, is EINVAL; every other set of
    /// flags goes on to the walk.
    #[test]
    fn decides_renameat2_flags() {
        let memory = Pages::holding(&[]);
        let renameat2 = |flags: u32| {
            linux_answer_before_the_walk(
                &memory,
                Sysno::renameat2,
                &[0, BASE, 0, BASE, flags as usize, 0],
            )
        };
        for flags in [
            libc::RENAME_EXCHANGE | libc::RENAME_NOREPLACE,
            libc::RENAME_EXCHANGE | libc::RENAME_WHITEOUT,
            libc::RENAME_EXCHANGE | libc::RENAME_NOREPLACE | libc::RENAME_WHITEOUT,
            8,
            1 << 31,
        ] {
            assert_eq!(renameat2(flags), Some(Err(Errno::EINVAL)), "{flags:#x}");
        }
        for flags in [
            0,
            libc::RENAME_NOREPLACE,
            libc::RENAME_EXCHANGE,
            libc::RENAME_WHITEOUT,
            libc::RENAME_NOREPLACE | libc::RENAME_WHITEOUT,
        ] {
            assert_eq!(renameat2(flags), None, "{flags:#x}");
        }
    }

    /// A symlink's target, which `getname` copies before the walk: EFAULT
    /// where it cannot be read, ENOENT where it is empty, ENAMETOOLONG where
    /// no NUL ends it within `PATH_MAX` bytes; a target that ends before an
    /// unreadable page, or in its last byte, goes on to the walk.
    #[test]
    fn decides_a_symlink_target() {
        let at_end = 2 * PAGE_SIZE - 4;
        let long = 100;
        let memory = Pages::holding(&[
            (0, b"\0"),
            (16, b"target\0"),
            (at_end, b"abc\0"),
            (long + PATH_MAX - 1, b"\0"),
        ]);
        let symlinkat = |target: usize| {
            linux_answer_before_the_walk(
                &memory,
                Sysno::symlinkat,
                &[target, 0, BASE + 16, 0, 0, 0],
            )
        };
        assert_eq!(symlinkat(BASE), Some(Err(Errno::ENOENT)));
        assert_eq!(symlinkat(0), Some(Err(Errno::EFAULT)));
        assert_eq!(symlinkat(BASE + 2 * PAGE_SIZE), Some(Err(Errno::EFAULT)));
        assert_eq!(symlinkat(BASE + 16), None);
        assert_eq!(symlinkat(BASE + at_end), None);
        // PATH_MAX - 1 bytes and a NUL; one more byte has no NUL in room.
        assert_eq!(symlinkat(BASE + long), None);
        assert_eq!(symlinkat(BASE + long - 1), Some(Err(Errno::ENAMETOOLONG)));
        // No NUL before the unreadable page, within PATH_MAX bytes.
        assert_eq!(
            linux_answer_before_the_walk(
                &Pages::holding(&[]),
                Sysno::symlinkat,
                &[BASE + 2 * PAGE_SIZE - 2, 0, BASE + 16, 0, 0, 0]
            ),
            Some(Err(Errno::EFAULT))
        );
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            linux_answer_before_the_walk(&memory, Sysno::symlink, &[BASE, BASE + 16, 0, 0, 0, 0]),
            Some(Err(Errno::ENOENT))
        );
    }

    /// utimensat: times that cannot be read are EFAULT, and two UTIME_OMIT
    /// answer 0, each before the flags, which `do_utimes_path` then refuses
    /// with EINVAL; an invalid nanosecond count is checked after the walk.
    #[test]
    fn decides_utimensat_times_and_flags() {
        let times = |atime: i64, mtime: i64| {
            [0i64, atime, 0, mtime]
                .iter()
                .flat_map(|value| value.to_ne_bytes())
                .collect::<Vec<_>>()
        };
        let omitted = times(libc::UTIME_OMIT, libc::UTIME_OMIT);
        let half = times(libc::UTIME_OMIT, libc::UTIME_NOW);
        let invalid = times(2_000_000_000, 0);
        let memory = Pages::holding(&[(0, &omitted), (64, &half), (128, &invalid)]);
        let utimensat = |times: usize, flags: i32| {
            linux_answer_before_the_walk(
                &memory,
                Sysno::utimensat,
                &[0, BASE + 512, times, flags as usize, 0, 0],
            )
        };
        assert_eq!(utimensat(BASE, 0), Some(Ok(0)));
        assert_eq!(utimensat(BASE, 0x4000_0000), Some(Ok(0)));
        assert_eq!(
            utimensat(BASE + 2 * PAGE_SIZE - 16, 0x4000_0000),
            Some(Err(Errno::EFAULT))
        );
        assert_eq!(utimensat(BASE + 64, 0x4000_0000), Some(Err(Errno::EINVAL)));
        assert_eq!(utimensat(0, 1), Some(Err(Errno::EINVAL)));
        assert_eq!(utimensat(BASE + 64, 0), None);
        assert_eq!(utimensat(BASE + 128, 0), None);
        assert_eq!(
            utimensat(0, libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH),
            None
        );
    }

    /// statx: a reserved mask bit, both synchronisation flags and an unknown
    /// flag are EINVAL; the flags it knows go on to the walk.
    #[test]
    fn decides_statx_masks_and_flags() {
        let memory = Pages::holding(&[]);
        let statx = |flags: i32, mask: u32| {
            linux_answer_before_the_walk(
                &memory,
                Sysno::statx,
                &[0, BASE, flags as usize, mask as usize, BASE, 0],
            )
        };
        assert_eq!(statx(0, 0x8000_0000), Some(Err(Errno::EINVAL)));
        assert_eq!(statx(libc::AT_STATX_SYNC_TYPE, 0), Some(Err(Errno::EINVAL)));
        assert_eq!(statx(0x4000_0000, 0), Some(Err(Errno::EINVAL)));
        assert_eq!(statx(0, libc::STATX_ALL), None);
        assert_eq!(statx(libc::AT_STATX_FORCE_SYNC, 0), None);
        assert_eq!(statx(libc::AT_STATX_DONT_SYNC, 0), None);
        assert_eq!(
            statx(
                libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT | libc::AT_EMPTY_PATH,
                0x7fff_ffff
            ),
            None
        );
    }

    /// An extended attribute's name, flags, size and value, in
    /// `setxattr_copy`'s order: an unknown flag is EINVAL, a name that is
    /// empty or has no NUL within 256 bytes ERANGE and one that cannot be
    /// read EFAULT, a size over `XATTR_SIZE_MAX` E2BIG, and a value that
    /// cannot be read EFAULT. The calls that read only a name check it alone.
    #[test]
    fn decides_extended_attribute_arguments() {
        let longest = [b'n'; XATTR_NAME_SIZE - 1];
        let memory = Pages::holding(&[(0, b"\0"), (16, b"user.x\0"), (64, &longest), (319, b"\0")]);
        let call = |sysno: Sysno, name: usize, value: usize, size: usize, flags: i32| {
            linux_answer_before_the_walk(
                &memory,
                sysno,
                &[BASE + 512, name, value, size, flags as usize, 0],
            )
        };
        for sysno in [
            Sysno::getxattr,
            Sysno::lgetxattr,
            Sysno::removexattr,
            Sysno::lremovexattr,
            Sysno::setxattr,
            Sysno::lsetxattr,
        ] {
            assert_eq!(
                call(sysno, BASE, 0, 0, 0),
                Some(Err(Errno::ERANGE)),
                "{sysno}"
            );
            assert_eq!(call(sysno, 0, 0, 0, 0), Some(Err(Errno::EFAULT)), "{sysno}");
            // 255 bytes and a NUL, then 256 bytes without one.
            assert_eq!(call(sysno, BASE + 64, 0, 0, 0), None, "{sysno}");
            assert_eq!(
                call(sysno, BASE + 63, 0, 0, 0),
                Some(Err(Errno::ERANGE)),
                "{sysno}"
            );
            assert_eq!(call(sysno, BASE + 16, 0, 0, 0), None, "{sysno}");
        }
        // The getters' and removers' value, size and flags are not checked
        // before the walk.
        assert_eq!(call(Sysno::getxattr, BASE + 16, 0, 1 << 20, 4), None);
        let setxattr = |name, value, size, flags| call(Sysno::setxattr, name, value, size, flags);
        assert_eq!(setxattr(BASE, 0, 0, 4), Some(Err(Errno::EINVAL)));
        assert_eq!(
            setxattr(BASE, 0, XATTR_SIZE_MAX + 1, 0),
            Some(Err(Errno::ERANGE))
        );
        assert_eq!(
            setxattr(BASE + 16, 0, XATTR_SIZE_MAX + 1, 0),
            Some(Err(Errno::E2BIG))
        );
        assert_eq!(setxattr(BASE + 16, 0, 1, 0), Some(Err(Errno::EFAULT)));
        assert_eq!(
            setxattr(BASE + 16, BASE + 2 * PAGE_SIZE - 8, 16, 0),
            Some(Err(Errno::EFAULT))
        );
        assert_eq!(setxattr(BASE + 16, BASE + 2 * PAGE_SIZE - 16, 16, 0), None);
        assert_eq!(setxattr(BASE + 16, 0, 0, libc::XATTR_CREATE), None);
        assert_eq!(setxattr(BASE + 16, 0, 0, libc::XATTR_REPLACE), None);
    }

    /// execveat, in Linux 7.1's order: an unknown flag is EINVAL before the
    /// path's `getname` (with a path too long, or empty without
    /// AT_EMPTY_PATH, too), and then the path: ENAMETOOLONG for one with no
    /// NUL in PATH_MAX bytes, ENOENT for an empty one, which AT_EMPTY_PATH
    /// makes the descriptor itself (LOOKUP_EMPTY). The flags it knows go on.
    #[test]
    fn decides_execveat_flags_before_its_path() {
        let memory = Pages::holding(&[
            (0, b"\0"),
            (16, b"/proc/5/cmdline/x\0"),
            (100 + PATH_MAX - 1, b"\0"),
        ]);
        let execveat = |path: usize, flags: i32| {
            linux_answer_before_the_walk(
                &memory,
                Sysno::execveat,
                &[3, path, 0, 0, flags as usize, 0],
            )
        };
        assert_eq!(execveat(BASE + 16, 0x1), Some(Err(Errno::EINVAL)));
        assert_eq!(execveat(BASE + 99, 0x1), Some(Err(Errno::EINVAL)));
        assert_eq!(execveat(BASE, 0x1), Some(Err(Errno::EINVAL)));
        assert_eq!(
            execveat(BASE, libc::AT_EMPTY_PATH | 0x1),
            Some(Err(Errno::EINVAL))
        );
        assert_eq!(execveat(BASE + 99, 0), Some(Err(Errno::ENAMETOOLONG)));
        assert_eq!(execveat(BASE, 0), Some(Err(Errno::ENOENT)));
        assert_eq!(execveat(BASE, libc::AT_EMPTY_PATH), None);
        assert_eq!(execveat(BASE + 16, 0), None);
        assert_eq!(
            execveat(
                BASE + 16,
                libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH | libc::AT_EXECVE_CHECK
            ),
            None
        );
    }

    /// statx with AT_EMPTY_PATH and an empty pathname on a descriptor is the
    /// descriptor's own: the reserved mask bit and both synchronisation
    /// flags are still EINVAL (`do_statx_fd`), but an unknown flag is not.
    /// On AT_FDCWD, or with a pathname, an unknown flag is EINVAL as before.
    #[test]
    fn decides_statx_for_a_descriptor() {
        let memory = Pages::holding(&[(0, b"\0"), (16, b"cmdline\0")]);
        let statx = |dirfd: i32, path: usize, flags: i32, mask: u32| {
            linux_answer_before_the_walk(
                &memory,
                Sysno::statx,
                &[dirfd as usize, path, flags as usize, mask as usize, BASE, 0],
            )
        };
        let empty = libc::AT_EMPTY_PATH;
        assert_eq!(statx(3, BASE, empty | 0x4000_0000, 0), None);
        assert_eq!(statx(3, BASE, empty, libc::STATX_ALL), None);
        assert_eq!(statx(3, BASE, empty, 0x8000_0000), Some(Err(Errno::EINVAL)));
        assert_eq!(
            statx(3, BASE, empty | libc::AT_STATX_SYNC_TYPE, 0),
            Some(Err(Errno::EINVAL))
        );
        assert_eq!(
            statx(libc::AT_FDCWD, BASE, empty | 0x4000_0000, 0),
            Some(Err(Errno::EINVAL))
        );
        assert_eq!(
            statx(3, BASE + 16, empty | 0x4000_0000, 0),
            Some(Err(Errno::EINVAL))
        );
        assert_eq!(statx(3, BASE, 0x4000_0000, 0), Some(Err(Errno::EINVAL)));
    }

    /// `utime`'s times and `utimes`' two `timeval`s, read before the walk:
    /// EFAULT where they cannot be read (a fault in their last byte too),
    /// EINVAL for `utimes`' microseconds outside [0, 1000000) in either, and
    /// nothing for NULL (the current time) or valid times.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn decides_utime_and_utimes_times() {
        let words = |values: &[i64]| {
            values
                .iter()
                .flat_map(|value| value.to_ne_bytes())
                .collect::<Vec<_>>()
        };
        let utimbuf = words(&[1, 2]);
        let valid = words(&[1, 999_999, 2, 0]);
        let negative = words(&[1, -1, 2, 0]);
        let too_many = words(&[1, 0, 2, 1_000_000]);
        let memory = Pages::holding(&[
            (0, &utimbuf),
            (64, &valid),
            (128, &negative),
            (192, &too_many),
        ]);
        let utime = |times: usize| {
            linux_answer_before_the_walk(&memory, Sysno::utime, &[BASE + 512, times, 0, 0, 0, 0])
        };
        let utimes = |times: usize| {
            linux_answer_before_the_walk(&memory, Sysno::utimes, &[BASE + 512, times, 0, 0, 0, 0])
        };
        assert_eq!(utime(0), None);
        assert_eq!(utime(BASE), None);
        assert_eq!(utime(8), Some(Err(Errno::EFAULT)));
        assert_eq!(utime(BASE + 2 * PAGE_SIZE - 8), Some(Err(Errno::EFAULT)));
        assert_eq!(utimes(0), None);
        assert_eq!(utimes(BASE + 64), None);
        assert_eq!(utimes(BASE + 128), Some(Err(Errno::EINVAL)));
        assert_eq!(utimes(BASE + 192), Some(Err(Errno::EINVAL)));
        assert_eq!(utimes(8), Some(Err(Errno::EFAULT)));
        assert_eq!(utimes(BASE + 2 * PAGE_SIZE - 16), Some(Err(Errno::EFAULT)));
    }

    /// The kernel beneath is given `utime` and `utimes` as `utimensat` on
    /// AT_FDCWD with no times and no flags, the path at argument 1, as
    /// Detcore's own handlers issue them; every other call as itself.
    #[test]
    fn issues_utime_and_utimes_as_utimensat() {
        let values = [11, 22, 33, 44, 55, 66];
        #[cfg(target_arch = "x86_64")]
        for sysno in [Sysno::utime, Sysno::utimes] {
            assert_eq!(
                respelled_call(sysno, values, 0),
                (
                    Sysno::utimensat,
                    [libc::AT_FDCWD as usize, 11, 0, 0, 0, 0],
                    1
                )
            );
        }
        for (sysno, path) in [
            (Sysno::utimensat, 1),
            (Sysno::openat, 1),
            (Sysno::statx, 1),
            (Sysno::renameat2, 3),
        ] {
            assert_eq!(respelled_call(sysno, values, path), (sysno, values, path));
        }
    }

    /// The other calls whose paths the module decides are left to the
    /// kernel beneath, which checks their arguments before its walk.
    #[test]
    fn leaves_the_other_calls_to_the_kernel() {
        let memory = Pages::holding(&[]);
        for sysno in [
            Sysno::openat,
            Sysno::newfstatat,
            Sysno::faccessat2,
            Sysno::readlinkat,
            Sysno::unlinkat,
            Sysno::fchmodat2,
            Sysno::fchownat,
            Sysno::linkat,
            Sysno::renameat,
            Sysno::truncate,
            Sysno::mkdirat,
            Sysno::listxattr,
            Sysno::inotify_add_watch,
            Sysno::execve,
        ] {
            assert_eq!(
                linux_answer_before_the_walk(&memory, sysno, &[usize::MAX; 6]),
                None,
                "{sysno}"
            );
        }
        // Where Detcore decides the walk itself (package K slice K10), it
        // checks those arguments too, so that its walk's error does not come
        // first: each refusal, then the same call with valid arguments.
        let call = |sysno: Sysno, values: [usize; 6]| {
            arguments_refused_before_the_walk(&memory, sysno, &values)
        };
        let creat_directory = (libc::O_CREAT | libc::O_DIRECTORY) as usize;
        let refused = Some(Err(Errno::EINVAL));
        for (sysno, bad, good) in [
            (
                Sysno::openat,
                [0, 0, creat_directory, 0, 0, 0],
                [0, 0, 0, 0, 0, 0],
            ),
            (
                Sysno::openat,
                [0, 0, libc::O_TMPFILE as usize, 0, 0, 0],
                [0, 0, (libc::O_TMPFILE | libc::O_RDWR) as usize, 0, 0, 0],
            ),
            (Sysno::faccessat, [0, 0, 0o100, 0, 0, 0], [0, 0, 0, 0, 0, 0]),
            (
                Sysno::faccessat2,
                [0, 0, 4, 0x4000_0000, 0, 0],
                [0, 0, 4, libc::AT_EACCESS as usize, 0, 0],
            ),
            (
                Sysno::newfstatat,
                [0, 0, 0, 0x4000_0000, 0, 0],
                [0, 0, 0, libc::AT_SYMLINK_NOFOLLOW as usize, 0, 0],
            ),
            (Sysno::readlinkat, [0, 0, 0, 0, 0, 0], [0, 0, 0, 1, 0, 0]),
            (
                Sysno::truncate,
                [0, usize::MAX, 0, 0, 0, 0],
                [0, 1, 0, 0, 0, 0],
            ),
            (
                Sysno::unlinkat,
                [0, 0, 1, 0, 0, 0],
                [0, 0, libc::AT_REMOVEDIR as usize, 0, 0, 0],
            ),
            (Sysno::fchownat, [0, 0, 0, 0, 1, 0], [0, 0, 0, 0, 0, 0]),
            (
                Sysno::linkat,
                [0, 0, 0, 0, 1, 0],
                [0, 0, 0, 0, libc::AT_SYMLINK_FOLLOW as usize, 0],
            ),
            (Sysno::fchmodat2, [0, 0, 0, 1, 0, 0], [0, 0, 0, 0, 0, 0]),
            (
                Sysno::statx,
                [0, 0, 0, 0x8000_0000, 0, 0],
                [0, 0, 0, 0x7ff, 0, 0],
            ),
        ] {
            assert_eq!(call(sysno, bad), refused, "{sysno} {bad:?}");
            assert_eq!(call(sysno, good), None, "{sysno} {good:?}");
        }
        assert_eq!(call(Sysno::mkdirat, [usize::MAX; 6]), None);
    }
}
