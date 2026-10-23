/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic views of kernel namespace metadata.

#[cfg(test)]
#[path = "namespace/own_getters_tests.rs"]
mod own_getters_tests;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;

use super::deterministic_stdio_inode;
use super::deterministic_stdio_inode_for_resource;
use crate::record_or_replay::RecordOrReplay;
use crate::tool_global::determinize_inode;
use crate::tool_local::Detcore;
use crate::types::DetInode;
use crate::types::RawInode;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#877)
fn is_proc_id(component: &OsStr) -> bool {
    component == "self"
        || component == "thread-self"
        || component.to_str().is_some_and(|value| {
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
        })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#877)
fn canonical_namespace_name(name: &OsStr) -> Option<&'static [u8]> {
    match name.to_str()? {
        "cgroup" => Some(b"cgroup:[4026531835]"),
        "ipc" => Some(b"ipc:[4026531839]"),
        "mnt" => Some(b"mnt:[4026531841]"),
        "net" => Some(b"net:[4026531840]"),
        "pid" | "pid_for_children" => Some(b"pid:[4026531836]"),
        "time" | "time_for_children" => Some(b"time:[4026531834]"),
        "user" => Some(b"user:[4026531837]"),
        "uts" => Some(b"uts:[4026531838]"),
        _ => None,
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#877)
fn canonical_namespace_target(path: &Path) -> Option<&'static [u8]> {
    if !path.is_absolute() {
        return None;
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => parts.push(part),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => return None,
        }
    }

    let namespace = match parts.as_slice() {
        [proc, subject, ns, namespace] if *proc == "proc" && is_proc_id(subject) && *ns == "ns" => {
            namespace
        }
        [proc, subject, task, tid, ns, namespace]
            if *proc == "proc"
                && is_proc_id(subject)
                && *task == "task"
                && is_proc_id(tid)
                && *ns == "ns" =>
        {
            namespace
        }
        _ => return None,
    };
    canonical_namespace_name(namespace)
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-972): Review proc-fd path alias coverage.
fn normalized_absolute_parts(path: &Path) -> Option<Vec<&OsStr>> {
    if !path.is_absolute() {
        return None;
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Prefix(_) => return None,
        }
    }
    Some(parts)
}

fn decimal_u32(component: &OsStr) -> Option<u32> {
    let value = component.to_str()?;
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())?
}

fn decimal_fd(component: &OsStr) -> Option<i32> {
    let value = component.to_str()?;
    // procfs names descriptors without leading zeros (`name_to_int`).
    (!value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0')))
    .then(|| value.parse().ok())?
}

/// The bytes of a readlink buffer that a placeholder's link may cover, read
/// before the kernel writes the link (`restore_link_tail`).
pub(crate) fn saved_link_bytes<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &G,
    buffer: Option<AddrMut<'_, libc::c_char>>,
    buffer_len: usize,
) -> Option<Vec<u8>> {
    // Also what a pipe or socket link's canonicalization restores, so it is
    // read for every link, not only a placeholder's.
    saved_link_prefix(
        guest,
        buffer,
        buffer_len.min(ANONYMOUS_PROC_FD_TARGET_CAPACITY),
    )
}

/// The bytes of a readlink buffer that the kernel's answer for an `exe` link
/// may cover, read before the kernel writes it: as many as a path Linux
/// renders can have, so that every byte past a shorter substituted target
/// is restored (`Detcore::substitute_exe_link`).
fn saved_exe_link_bytes<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &G,
    buffer: Option<AddrMut<'_, libc::c_char>>,
    buffer_len: usize,
) -> Option<Vec<u8>> {
    saved_link_prefix(guest, buffer, buffer_len.min(libc::PATH_MAX as usize))
}

/// The first `len` bytes of a readlink buffer, or its readable prefix.
fn saved_link_prefix<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &G,
    buffer: Option<AddrMut<'_, libc::c_char>>,
    len: usize,
) -> Option<Vec<u8>> {
    let buffer = buffer?;
    let mut saved = vec![0; len];
    if guest
        .memory()
        .read_exact(buffer.cast::<u8>(), &mut saved)
        .is_ok()
    {
        return Some(saved);
    }
    // The readable prefix, a page at a time: a buffer may run into an
    // unmapped page past the link the kernel writes.
    const PAGE: usize = 4096;
    let start = buffer.as_raw();
    let mut read = 0;
    while read < saved.len() {
        let end = (((start + read) / PAGE + 1) * PAGE - start).min(saved.len());
        // SAFETY: `read < saved.len() <= buffer_len`, so the address stays
        // inside the guest's buffer; nothing is dereferenced here.
        let chunk = unsafe { buffer.cast::<u8>().add(read) };
        if guest
            .memory()
            .read_exact(chunk, &mut saved[read..end])
            .is_err()
        {
            break;
        }
        read = end;
    }
    saved.truncate(read);
    (read > 0).then_some(saved)
}

/// Put back the caller's bytes after a rewritten link of `written` bytes,
/// where the kernel had written `kernel_len`: Linux writes only the link.
pub(crate) fn restore_link_tail<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &mut G,
    buffer: AddrMut<'_, libc::c_char>,
    written: usize,
    kernel_len: usize,
    saved: Option<&[u8]>,
) -> Result<(), Error> {
    if let Some(saved) = saved {
        let end = kernel_len.min(saved.len());
        if written < end {
            // SAFETY: `written < end <= saved.len() <= buffer_len`, so the
            // address stays inside the guest's buffer; nothing is dereferenced
            // here, only written through the guest memory interface.
            let tail = unsafe { buffer.cast::<u8>().add(written) };
            guest.memory().write_exact(tail, &saved[written..end])?;
        }
    }
    Ok(())
}

/// Whether `path` spells a task's descriptor link,
/// `/proc/<pid>/task/<tid>/fd/N`.
fn names_task_descriptor_link(path: &Path) -> bool {
    let Some(parts) = normalized_absolute_parts(path) else {
        return false;
    };
    matches!(
        parts.as_slice(),
        [proc, _, task, _, fd_dir, fd]
            if *proc == "proc" && *task == "task" && *fd_dir == "fd" && decimal_fd(fd).is_some()
    )
}

/// Returns the optional numeric proc subject and the descriptor number.
pub(crate) fn proc_fd_target(path: &Path) -> Option<(Option<u32>, i32)> {
    let parts = normalized_absolute_parts(path)?;
    match parts.as_slice() {
        [dev, fd_dir, fd] if *dev == "dev" && *fd_dir == "fd" => Some((None, decimal_fd(fd)?)),
        [proc, subject, fd_dir, fd] if *proc == "proc" && *fd_dir == "fd" => {
            let subject = match subject.to_str()? {
                "self" | "thread-self" => None,
                _ => Some(decimal_u32(subject)?),
            };
            Some((subject, decimal_fd(fd)?))
        }
        _ => None,
    }
}

/// Whose descriptor table a descriptor link names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DescriptorTable {
    /// The process's: `/proc/self/fd/N`, `/proc/<pid>/fd/N` and `/dev/fd/N`
    /// (a symbolic link to `/proc/self/fd`). procfs reads it from the
    /// thread-group leader, whose table a thread cloned without `CLONE_FILES`
    /// does not share.
    Process,
    /// The calling thread's own: `/proc/thread-self/fd/N` and
    /// `/proc/<pid>/task/<tid>/fd/N`.
    Thread,
}

/// A descriptor that a descriptor link of the caller names, by number, and
/// the table it is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DescriptorLink {
    pub(crate) fd: i32,
    pub(crate) table: DescriptorTable,
}

/// The descriptor that the absolute guest path `path` names through one of
/// the caller's descriptor links, and whose table that is:
/// `/dev/fd/N`, `/proc/self/fd/N` and `/proc/<pid>/fd/N` for the caller's
/// own pid name the process's table ([`DescriptorTable::Process`]);
/// `/proc/thread-self/fd/N` and `/proc/<pid>/task/<tid>/fd/N` (or
/// `/proc/self/task/<tid>/fd/N`) for its own pid and tid name the calling
/// thread's ([`DescriptorTable::Thread`]). `own` is the caller's (pid, tid)
/// as the guest's numeric `/proc` spellings name them (Detcore's, see
/// `Detcore::procfs_caller_ids`), or `None` where Detcore does not know
/// those names (in a nested PID namespace), which recognizes only the
/// spellings without numbers. `None` for a spelling with `..`: the kernel walks it through the
/// directory the link reaches, not lexically. A sibling thread's directory
/// is not recognized, since that thread may have a descriptor table of its
/// own.
pub(crate) fn own_descriptor_link(path: &Path, own: Option<(i32, i32)>) -> Option<DescriptorLink> {
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
    let own_process = |part: &OsStr| {
        part == "self" || own.is_some_and(|(pid, _)| part.to_str() == Some(&pid.to_string()))
    };
    let own_thread =
        |part: &OsStr| own.is_some_and(|(_, tid)| part.to_str() == Some(&tid.to_string()));
    let link = |fd: &OsStr, table| decimal_fd(fd).map(|fd| DescriptorLink { fd, table });
    match parts.as_slice() {
        [dev, fd_dir, fd] if *dev == "dev" && *fd_dir == "fd" => link(fd, DescriptorTable::Process),
        [proc, subject, fd_dir, fd]
            if *proc == "proc" && *fd_dir == "fd" && *subject == "thread-self" =>
        {
            link(fd, DescriptorTable::Thread)
        }
        [proc, subject, fd_dir, fd]
            if *proc == "proc" && *fd_dir == "fd" && own_process(subject) =>
        {
            link(fd, DescriptorTable::Process)
        }
        [proc, subject, task, thread, fd_dir, fd]
            if *proc == "proc"
                && *task == "task"
                && *fd_dir == "fd"
                && own_process(subject)
                && own_thread(thread) =>
        {
            link(fd, DescriptorTable::Thread)
        }
        _ => None,
    }
}

/// Whether the absolute guest path `path` names something through a
/// selector that procfs or `/dev` resolves for whoever looks it up:
/// `/proc/self`, `/proc/thread-self`, `/proc/mounts` and `/proc/net` (links
/// into `self`), `/dev/fd`, and `/dev/stdin`, `/dev/stdout` and `/dev/stderr`
/// (links into `/proc/self/fd`), or anything below them. A descriptor opened
/// at such a path is the opener's file, so the path cannot stand for that
/// descriptor in another process or thread. A spelling with `..`, or a
/// relative one, is treated the same way, since it is not resolved
/// lexically.
pub(crate) fn names_through_caller_selector(path: &Path) -> bool {
    if !path.is_absolute() {
        return true;
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => parts.push(part),
            Component::ParentDir | Component::Prefix(_) => return true,
        }
    }
    match parts.as_slice() {
        [proc, selector, ..] if *proc == "proc" => {
            ["self", "thread-self", "mounts", "net"].contains(&selector.to_str().unwrap_or(""))
        }
        [dev, selector, ..] if *dev == "dev" => {
            ["fd", "stdin", "stdout", "stderr"].contains(&selector.to_str().unwrap_or(""))
        }
        _ => false,
    }
}

/// The optional numeric proc subject and descriptor number of a descriptor
/// link spelled with leading zeros (`/proc/self/fd/03`, `/dev/fd/03`), which
/// procfs does not have.
pub(crate) fn proc_fd_target_with_leading_zeros(path: &Path) -> Option<(Option<u32>, i32)> {
    let parts = normalized_absolute_parts(path)?;
    let (subject, fd) = match parts.as_slice() {
        [dev, fd_dir, fd] if *dev == "dev" && *fd_dir == "fd" => (None, fd),
        [proc, subject, fd_dir, fd] if *proc == "proc" && *fd_dir == "fd" => (Some(subject), fd),
        _ => return None,
    };
    let fd = fd.to_str()?;
    if fd.len() < 2 || !fd.starts_with('0') || !fd.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let subject = match subject {
        None => None,
        Some(subject) => match subject.to_str()? {
            "self" | "thread-self" => None,
            _ => Some(decimal_u32(subject)?),
        },
    };
    Some((subject, fd.parse().ok()?))
}

/// A `/proc` link whose target Detcore decides itself
/// ([`decided_procfs_link`]).
#[derive(Debug, PartialEq, Eq)]
enum DecidedLink {
    /// The link's target.
    Target(Vec<u8>),
    /// The caller's own descriptor link, by a spelling the backend may not
    /// answer (`/proc/self/task/<tid>/fd/N`): read as `/proc/self/fd/N`.
    SelfFd(i32),
    /// A spelling that follows a decided link (a trailing slash or `.`) and
    /// so names no link: `EINVAL` where the target is a directory, `ENOTDIR`
    /// where it is a file.
    Followed(Errno),
}

/// The number of bytes `readlink` may copy for a `bufsiz` argument of
/// `buffer_len`, which Linux takes as an `int`: EINVAL unless positive.
pub(crate) fn readlink_size(buffer_len: usize) -> Result<usize, Errno> {
    let size = buffer_len as u32 as i32;
    if size <= 0 {
        return Err(Errno::EINVAL);
    }
    Ok(size as usize)
}

/// The link at the absolute spelling `path` whose target Detcore decides from
/// its own model, for a caller whose pid and tid, as the guest sees them, are
/// `pid` and `tid`: `self` names the pid and `thread-self` the pid's task
/// `tid`; the `mounts` and `net` links name `self/mounts` and `self/net`; the
/// caller's namespace links name their canonical constants; and the caller's
/// own descriptor link spelled through its thread is its `fd/N`. A trailing
/// slash or `.` follows the link, so such a spelling of a decided target is
/// [`DecidedLink::Followed`]. The caller's own process directory by number,
/// its `task` directory and its own thread's directory are no links, so
/// EINVAL by any of these spellings. `None` for any other path, for a
/// spelling with `..`, and for a followed descriptor link, which the kernel
/// resolves.
fn decided_procfs_link(path: &[u8], pid: i32, tid: i32) -> Option<DecidedLink> {
    let rest = path.strip_prefix(b"/proc/")?;
    let mut parts = Vec::new();
    let mut follows = false;
    for part in rest.split(|byte| *byte == b'/') {
        match part {
            b"" | b"." => follows = !parts.is_empty(),
            b".." => return None,
            part => {
                parts.push(part);
                follows = false;
            }
        }
    }
    if follows {
        // `self`, `thread-self` and `net` name directories; `mounts` and the
        // namespace links name files.
        return match decided_procfs_link_parts(&parts, pid, tid)? {
            DecidedLink::Target(_) => Some(DecidedLink::Followed(
                if matches!(parts.as_slice(), [b"self" | b"thread-self" | b"net"]) {
                    Errno::EINVAL
                } else {
                    Errno::ENOTDIR
                },
            )),
            // A directory is one followed or not.
            followed @ DecidedLink::Followed(_) => Some(followed),
            DecidedLink::SelfFd(_) => None,
        };
    }
    decided_procfs_link_parts(&parts, pid, tid)
}

/// [`decided_procfs_link`] of the components below `/proc` of a spelling
/// that does not follow its last one.
fn decided_procfs_link_parts(parts: &[&[u8]], pid: i32, tid: i32) -> Option<DecidedLink> {
    let pid_name = pid.to_string();
    let tid_name = tid.to_string();
    let own_process = |part: &[u8]| part == b"self" || part == pid_name.as_bytes();
    let own_thread = |part: &[u8]| part == tid_name.as_bytes();
    let namespace = |parts: &[&[u8]]| {
        let spelled = std::iter::once(b"/proc".as_slice())
            .chain(parts.iter().copied())
            .collect::<Vec<_>>()
            .join(b"/".as_slice());
        canonical_namespace_target(Path::new(OsStr::from_bytes(&spelled)))
            .map(|target| DecidedLink::Target(target.to_vec()))
    };
    match parts {
        [b"self"] => Some(DecidedLink::Target(pid_name.into_bytes())),
        [b"thread-self"] => Some(DecidedLink::Target(
            format!("{pid}/task/{tid}").into_bytes(),
        )),
        [b"mounts"] => Some(DecidedLink::Target(b"self/mounts".to_vec())),
        [b"net"] => Some(DecidedLink::Target(b"self/net".to_vec())),
        [process, b"ns", _] if own_process(process) || *process == b"thread-self" => {
            namespace(parts)
        }
        [process, b"task", thread, b"ns", _] if own_process(process) && own_thread(thread) => {
            namespace(parts)
        }
        [process, b"task", thread, b"fd", fd] if own_process(process) && own_thread(thread) => {
            decimal_fd(OsStr::from_bytes(fd)).map(DecidedLink::SelfFd)
        }
        // The caller's own process directory by number, its `task`
        // directory, and its own thread's directory: directories, not links,
        // so `readlink` fails with EINVAL, as on Linux. Decided here so that
        // no backend's `/proc` answers for them: the KVM executor's is the
        // supervisor's, where the number names another task or none.
        [process] if *process == pid_name.as_bytes() => Some(DecidedLink::Followed(Errno::EINVAL)),
        [process, b"task"] if own_process(process) => Some(DecidedLink::Followed(Errno::EINVAL)),
        [process, b"task", thread] if own_process(process) && own_thread(thread) => {
            Some(DecidedLink::Followed(Errno::EINVAL))
        }
        _ => None,
    }
}

/// The procfs directory of guest thread `tid` as the tool reaches it: its
/// own `/proc/thread-self` where the tool runs in the guest's process
/// (`tool_shares_guest_descriptor_table`), `/proc/<tid>` where it is a
/// separate tracer. `None` where guest threads are not host tasks
/// ([`crate::procfs_inode::tool_can_name_guest_descriptors`]: KVM), after a
/// guest `chroot` (`changed_root`), and where a tracer's guest has a mount
/// namespace of its own: the tool would look paths up in another
/// filesystem view than the guest's.
pub(crate) fn guest_task_procfs(
    backend: &reverie::BackendCapabilities,
    tid: i32,
    changed_root: bool,
) -> Option<PathBuf> {
    if !crate::procfs_inode::tool_can_name_guest_descriptors(backend) || changed_root {
        return None;
    }
    if backend.tool_shares_guest_descriptor_table {
        return Some(PathBuf::from("/proc/thread-self"));
    }
    let task = PathBuf::from(format!("/proc/{tid}"));
    // Raw syscalls: in-guest Detcore must not call libc's wrappers
    // (`crate::util::raw_syscall`).
    let namespace = |path: &Path| crate::util::raw_stat(path).ok().map(|stat| stat.st_ino);
    (namespace(&task.join("ns/mnt"))? == namespace(Path::new("/proc/self/ns/mnt"))?).then_some(task)
}

/// Whether the guest's pathname `path` may name an `exe` link Detcore
/// could not resolve: its final component is `exe`, or it is empty (a
/// descriptor for the link itself). The launcher guards look only at these
/// (`Detcore::refuse_launcher_link`, `Detcore::opened_the_launcher`), so
/// that a guest's own link or open of any other file is never refused.
pub(crate) fn exe_link_candidate(path: &Path) -> bool {
    path.as_os_str().is_empty() || path.file_name() == Some(OsStr::new("exe"))
}

/// Put back the first `written` bytes of a readlink buffer from `saved`,
/// read before the kernel wrote it, as far as `saved` reaches.
fn restore_saved_link_bytes<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &mut G,
    buffer: AddrMut<'_, libc::c_char>,
    written: usize,
    saved: Option<&[u8]>,
) {
    if let Some(saved) = saved {
        let end = written.min(saved.len());
        let _ = guest
            .memory()
            .write_exact(buffer.cast::<u8>(), &saved[..end]);
    }
}

/// Whose `exe` link a `/proc` spelling names ([`exe_link_subject`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExeLinkSubject {
    /// The caller's own process: `self`, `thread-self`, its pid, or a thread
    /// in its `task` directory.
    Own,
    /// Another process, by the pid the guest sees.
    Other(i32),
}

/// Whose `exe` link the absolute, already resolved spelling `path` names
/// (callers resolve its directory first, `Detcore::resolve_exe_subject`),
/// for a caller whose pid, as the guest sees it, is `pid`: `/proc/<p>/exe` and
/// `/proc/<p>/task/<t>/exe` name process `p` (`self` and `thread-self` the
/// caller's). Linux reads a thread's link from its process's image, and
/// answers a thread outside `p` with `ENOENT`, which the kernel decides
/// here too. `None` for any other path, for a spelling with `..`, and for
/// one that follows the link (a trailing slash or `.`).
pub(crate) fn exe_link_subject(path: &[u8], pid: i32) -> Option<ExeLinkSubject> {
    let rest = path.strip_prefix(b"/proc/")?;
    if path.ends_with(b"/") || path.ends_with(b"/.") {
        return None;
    }
    let mut parts = Vec::new();
    for part in rest.split(|byte| *byte == b'/') {
        match part {
            b"" | b"." => {}
            b".." => return None,
            part => parts.push(part),
        }
    }
    let number = |part: &[u8]| -> Option<i32> {
        let digits = std::str::from_utf8(part).ok()?;
        // procfs names tasks without leading zeros.
        (!digits.starts_with('0') && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| digits.parse().ok())?
    };
    let process = |part: &[u8]| -> Option<ExeLinkSubject> {
        if part == b"self" {
            return Some(ExeLinkSubject::Own);
        }
        let process = number(part)?;
        Some(if process == pid {
            ExeLinkSubject::Own
        } else {
            ExeLinkSubject::Other(process)
        })
    };
    match parts.as_slice() {
        [b"thread-self", b"exe"] => Some(ExeLinkSubject::Own),
        [subject, b"exe"] => process(subject),
        [subject, b"task", thread, b"exe"] => {
            number(thread)?;
            process(subject)
        }
        _ => None,
    }
}

// TODO-HUMAN-REVIEW(PR-1079): Review numeric virtual-self proc-fd path rewriting.
fn host_self_proc_fd_alias(path: &Path, current_pid: i64) -> Option<PathBuf> {
    let (Some(subject), fd) = proc_fd_target(path)? else {
        return None;
    };
    (i64::from(subject) == current_pid).then(|| PathBuf::from(format!("/proc/self/fd/{fd}")))
}

#[derive(Debug, Eq, PartialEq)]
struct AnonymousProcFdIdentity {
    kind: &'static str,
    /// The inode number the link names. Its device is the kernel's pipe or
    /// socket filesystem ([`anonymous_object_device`]).
    raw_inode: u64,
}

/// The host devices of the kernel's pipe and socket filesystems, on which
/// every `pipe:[N]` or `socket:[N]` link's inode lives. Each is one
/// kernel-wide superblock, so a pipe and a socket made here show them. A host
/// inode is identified by device AND number, and a pipe's `fstat` reports this
/// device, so the identity a link names must carry it too.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnonymousObjectDevices {
    pipe: u64,
    socket: u64,
}

/// The [`AnonymousObjectDevices`], read once per process, or why they could
/// not be read. Detcore reads them when it starts in a process
/// ([`crate::Detcore`]'s `Tool::new`), before any guest syscall, so a guest's
/// readlink never depends on whether the tracer can make a pipe or socket at
/// that moment, and keeps the outcome either way: a later call neither retries
/// nor panics.
pub(crate) fn anonymous_object_devices() -> &'static Result<AnonymousObjectDevices, String> {
    static DEVICES: std::sync::OnceLock<Result<AnonymousObjectDevices, String>> =
        std::sync::OnceLock::new();
    DEVICES.get_or_init(|| retained_probe_outcome(probe_anonymous_object_devices()))
}

/// The probe's outcome as Detcore keeps it. Under in-guest LiteInst the probe
/// runs inside the guest before `main`, so a failure is described without the
/// C library's errno message (see [`crate::util::describe_io_error`]).
fn retained_probe_outcome(
    probe: std::io::Result<AnonymousObjectDevices>,
) -> Result<AnonymousObjectDevices, String> {
    probe.map_err(|error| crate::util::describe_io_error(&error))
}

fn probe_anonymous_object_devices() -> std::io::Result<AnonymousObjectDevices> {
    use crate::util::raw_syscall;
    use crate::util::raw_zero_result;
    // Raw syscalls (see crate::util::raw_syscall): Detcore::new runs this
    // inside the guest under in-guest LiteInst.
    let close = |fds: [i32; 2]| {
        for fd in fds {
            unsafe { raw_syscall(libc::SYS_close, [fd as u64, 0, 0, 0, 0, 0]) };
        }
    };
    let device_of = |fd: i32| -> std::io::Result<u64> {
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        raw_zero_result(unsafe {
            raw_syscall(
                libc::SYS_fstat,
                [fd as u64, (&raw mut metadata) as u64, 0, 0, 0, 0],
            )
        })?;
        Ok(metadata.st_dev)
    };
    let mut pipe = [0_i32; 2];
    raw_zero_result(unsafe {
        raw_syscall(
            libc::SYS_pipe2,
            [pipe.as_mut_ptr() as u64, libc::O_CLOEXEC as u64, 0, 0, 0, 0],
        )
    })?;
    let pipe_device = device_of(pipe[0]);
    close(pipe);
    let mut sockets = [0_i32; 2];
    raw_zero_result(unsafe {
        raw_syscall(
            libc::SYS_socketpair,
            [
                libc::AF_UNIX as u64,
                (libc::SOCK_DGRAM | libc::SOCK_CLOEXEC) as u64,
                0,
                sockets.as_mut_ptr() as u64,
                0,
                0,
            ],
        )
    })?;
    let socket_device = device_of(sockets[0]);
    close(sockets);
    Ok(AnonymousObjectDevices {
        pipe: pipe_device?,
        socket: socket_device?,
    })
}

/// The device of a `kind` ("pipe" or "socket") link's inode, from the
/// devices read at start (`devices`). When they could not be read, this is a
/// tool error, not an errno: Detcore cannot give the link a faithful identity,
/// so the run fails as an infrastructure failure through the backend's own
/// failure handling, rather than a link identity getting an invented device or
/// a guest getting an error Linux would not return.
fn anonymous_object_device(
    devices: &Result<AnonymousObjectDevices, String>,
    kind: &str,
) -> Result<u64, Error> {
    let devices = devices.as_ref().map_err(|error| {
        Error::Tool(anyhow::anyhow!(
            "Detcore could not read the pipe and socket filesystem devices when it started \
             ({error}), so it cannot identify the {kind} a /proc fd link names"
        ))
    })?;
    Ok(match kind {
        "pipe" => devices.pipe,
        "socket" => devices.socket,
        _ => unreachable!("anonymous_proc_fd_identity names only pipes and sockets, not {kind}"),
    })
}

// Long enough for `socket:[` + every decimal u64 inode + `]`.
/// Room for a whole pipe or socket link, and for the link of a placeholder
/// for a procfs file Detcore renders (`crate::procfs::placeholder_link_path`).
const ANONYMOUS_PROC_FD_TARGET_CAPACITY: usize = crate::procfs::PLACEHOLDER_LINK_CAPACITY;

fn needs_anonymous_proc_fd_scratch(buffer_present: bool, buffer_len: usize) -> bool {
    buffer_present && buffer_len != 0 && buffer_len < ANONYMOUS_PROC_FD_TARGET_CAPACITY
}

/// Recognize only a complete kernel pipe/socket symlink target.
fn anonymous_proc_fd_identity(target: &[u8]) -> Option<AnonymousProcFdIdentity> {
    for (kind, prefix) in [
        ("pipe", b"pipe:[".as_slice()),
        ("socket", b"socket:[".as_slice()),
    ] {
        let Some(digits) = target
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(b"]"))
        else {
            continue;
        };
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            continue;
        }
        let raw_inode = std::str::from_utf8(digits).ok()?.parse().ok()?;
        return Some(AnonymousProcFdIdentity { kind, raw_inode });
    }
    None
}

/// Match a raw identity against cached current-process inherited-stdio identities.
/// Callers exclude ordinary replacement objects before filling this array.
///
/// When stdio descriptors alias, the lowest one wins, as in the maps
/// sanitizer's `stdio_by_raw_inode` table. Detcore gives all three inherited
/// descriptors the stat of its own stdin, so they alias until the guest
/// replaces them: a link to that object is a link to fd 0, and must read as
/// the `pipe:[1000]` a readlink of fd 0 returns.
fn deterministic_stdio_inode_for_raw(
    raw_inode: RawInode,
    stdio_raw_inodes: &[Option<RawInode>; 3],
) -> Option<DetInode> {
    let fd = stdio_raw_inodes
        .iter()
        .position(|cached| *cached == Some(raw_inode))?;
    deterministic_stdio_inode(fd as i32)
}

fn canonical_anonymous_proc_fd_target(
    identity: &AnonymousProcFdIdentity,
    inode: DetInode,
    buffer_len: usize,
) -> Vec<u8> {
    let mut target = format!("{}:[{}]", identity.kind, inode.as_raw()).into_bytes();
    target.truncate(buffer_len);
    target
}

impl<T: RecordOrReplay> Detcore<T> {
    async fn canonicalize_other_proc_fd_target<G>(
        &self,
        guest: &mut G,
        link: (i32, Option<PathPtr<'_>>),
        raw_target: &[u8],
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
        saved: Option<&[u8]>,
    ) -> Result<Option<i64>, Error>
    where
        G: Guest<Self>,
    {
        if let Some(path) =
            crate::procfs::placeholder_link_path(raw_target, self.rendered_files(guest))
            && self.placeholder_link_confirmed(guest, link).await
        {
            // Another process's link to a procfs file Detcore renders names
            // the file, as Linux's does.
            let link = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
            let written = link.len().min(buffer_len);
            let buffer = buffer.ok_or(Errno::EFAULT)?;
            guest
                .memory()
                .write_exact(buffer.cast(), &link[..written])?;
            restore_link_tail(guest, buffer, written, raw_target.len(), saved)?;
            return Ok(Some(written as i64));
        }
        let Some(identity) = anonymous_proc_fd_identity(raw_target) else {
            return Ok(None);
        };
        let mut stdio_raw_inodes = [None; 3];
        for fd in libc::STDIN_FILENO..=libc::STDERR_FILENO {
            stdio_raw_inodes[fd as usize] = guest
                .thread_state()
                .with_detfd(fd, |detfd| {
                    deterministic_stdio_inode_for_resource(fd, detfd.resource())?;
                    detfd.stat().map(|stat| stat.raw_inode())
                })
                .ok()
                .flatten();
        }
        let raw_inode = RawInode::new(
            anonymous_object_device(anonymous_object_devices(), identity.kind)?,
            identity.raw_inode,
        );
        // Every link sends its numbering request, and a stdio match only
        // chooses the rendering afterwards, as `/proc/*/maps` does: whether
        // the raw identity matches stdio is host identity, which must not
        // decide how many inode numbers the call consumes
        // (<https://github.com/rrnewton/hermit/issues/2897>).
        let pooled = determinize_inode(guest, raw_inode).await.0;
        let inode =
            deterministic_stdio_inode_for_raw(raw_inode, &stdio_raw_inodes).unwrap_or(pooled);
        let target = canonical_anonymous_proc_fd_target(&identity, inode, buffer_len);
        let buffer = buffer.ok_or(Errno::EFAULT)?;
        guest.memory().write_exact(buffer.cast(), &target)?;
        // The raw link's longer inode digits must not stay behind the
        // canonical one: Linux writes only the link.
        restore_link_tail(guest, buffer, target.len(), raw_target.len(), saved)?;
        Ok(Some(target.len() as i64))
    }

    /// Canonicalize a pipe/socket link belonging to another virtual process.
    /// The target descriptor is not in the caller's table, so its raw readlink
    /// bytes are the only safe identity evidence available here.
    async fn canonicalize_other_proc_fd_readlink<G>(
        &self,
        guest: &mut G,
        link: (i32, Option<PathPtr<'_>>),
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
        result: i64,
        saved: Option<&[u8]>,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        let buffer = buffer.expect("a successful readlink requires a non-null buffer");
        let observed_len = usize::try_from(result)
            .expect("a positive readlink result must fit usize")
            .min(buffer_len);
        let mut observed = vec![0; observed_len];
        guest.memory().read_exact(buffer.cast(), &mut observed)?;
        Ok(self
            .canonicalize_other_proc_fd_target(
                guest,
                link,
                &observed,
                Some(buffer),
                buffer_len,
                saved,
            )
            .await?
            .unwrap_or(result))
    }

    async fn canonicalize_namespace_readlink_result<G>(
        &self,
        guest: &mut G,
        path: PathBuf,
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
        result: i64,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        if result <= 0 {
            return Ok(result);
        }

        let target = if let Some(target) = canonical_namespace_target(&path) {
            target.to_vec()
        } else if let Some((subject, fd)) = proc_fd_target(&path) {
            if let Some(subject) = subject {
                let current_pid = self.guest_visible_own_pid(guest).await?;
                if current_pid != i64::from(subject) {
                    return self
                        .canonicalize_other_proc_fd_readlink(
                            guest,
                            (libc::AT_FDCWD, None),
                            buffer,
                            buffer_len,
                            result,
                            None,
                        )
                        .await;
                }
            }

            let stat = self.inject_fstat(guest, fd).await?;
            let kind = match stat.st_mode & libc::S_IFMT {
                libc::S_IFIFO => "pipe",
                libc::S_IFSOCK => "socket",
                _ => return Ok(result),
            };
            let inode_override = guest
                .thread_state()
                .with_detfd(fd, |detfd| {
                    deterministic_stdio_inode_for_resource(fd, detfd.resource())
                })
                .ok()
                .flatten();
            let inode = match inode_override {
                Some(inode) => inode,
                None => {
                    determinize_inode(guest, RawInode::new(stat.st_dev, stat.st_ino))
                        .await
                        .0
                }
            };
            format!("{kind}:[{inode}]").into_bytes()
        } else {
            return Ok(result);
        };

        let written = target.len().min(buffer_len);
        let buffer = buffer.expect("a successful readlink requires a non-null buffer");
        guest
            .memory()
            .write_exact(buffer.cast(), &target[..written])?;
        Ok(written as i64)
    }
    async fn write_other_proc_fd_target<G>(
        &self,
        guest: &mut G,
        link: (i32, Option<PathPtr<'_>>),
        raw_target: &[u8],
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        if let Some(result) = self
            .canonicalize_other_proc_fd_target(guest, link, raw_target, buffer, buffer_len, None)
            .await?
        {
            return Ok(result);
        }
        let written = raw_target.len().min(buffer_len);
        let buffer = buffer.ok_or(Errno::EFAULT)?;
        guest
            .memory()
            .write_exact(buffer.cast(), &raw_target[..written])?;
        Ok(written as i64)
    }

    async fn finish_other_proc_fd_readlink<G>(
        &self,
        guest: &mut G,
        call: syscalls::Readlink,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        let link = (libc::AT_FDCWD, call.path());
        let buffer = call.buf();
        let buffer_len = call.bufsize();
        if !needs_anonymous_proc_fd_scratch(buffer.is_some(), buffer_len) {
            let saved = saved_link_bytes(guest, buffer, buffer_len);
            let result = self.record_or_replay(guest, call).await?;
            if result <= 0 {
                return Ok(result);
            }
            return self
                .canonicalize_other_proc_fd_readlink(
                    guest,
                    link,
                    buffer,
                    buffer_len,
                    result,
                    saved.as_deref(),
                )
                .await;
        }

        let mut stack = guest.stack().await;
        let scratch = stack
            .reserve::<[u8; ANONYMOUS_PROC_FD_TARGET_CAPACITY]>()
            .cast::<libc::c_char>();
        let guard = stack.commit()?;
        let physical_call = call
            .with_buf(Some(scratch))
            .with_bufsize(ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        let result = self.record_or_replay(guest, physical_call).await?;
        let length = usize::try_from(result)
            .expect("a successful readlink result must fit usize")
            .min(ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        let mut raw_target = vec![0; length];
        guest.memory().read_exact(scratch.cast(), &mut raw_target)?;
        drop(guard);
        self.write_other_proc_fd_target(guest, link, &raw_target, buffer, buffer_len)
            .await
    }

    async fn finish_other_proc_fd_readlinkat<G>(
        &self,
        guest: &mut G,
        call: syscalls::Readlinkat,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        let link = (call.dirfd(), call.path());
        let buffer = call.buf();
        let buffer_len = call.buf_len();
        if !needs_anonymous_proc_fd_scratch(buffer.is_some(), buffer_len) {
            let saved = saved_link_bytes(guest, buffer, buffer_len);
            let result = self.record_or_replay(guest, call).await?;
            if result <= 0 {
                return Ok(result);
            }
            return self
                .canonicalize_other_proc_fd_readlink(
                    guest,
                    link,
                    buffer,
                    buffer_len,
                    result,
                    saved.as_deref(),
                )
                .await;
        }

        let mut stack = guest.stack().await;
        let scratch = stack
            .reserve::<[u8; ANONYMOUS_PROC_FD_TARGET_CAPACITY]>()
            .cast::<libc::c_char>();
        let guard = stack.commit()?;
        let physical_call = call
            .with_buf(Some(scratch))
            .with_buf_len(ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        let result = self.record_or_replay(guest, physical_call).await?;
        let length = usize::try_from(result)
            .expect("a successful readlinkat result must fit usize")
            .min(ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        let mut raw_target = vec![0; length];
        guest.memory().read_exact(scratch.cast(), &mut raw_target)?;
        drop(guard);
        self.write_other_proc_fd_target(guest, link, &raw_target, buffer, buffer_len)
            .await
    }

    /// The rendered procfs file that the descriptor link `path` (absolute)
    /// names a placeholder for, read whole into scratch space.
    async fn placeholder_behind_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
    ) -> Result<Option<&'static Path>, Error> {
        const PATH_CAPACITY: usize = 64;
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() >= PATH_CAPACITY {
            return Ok(None);
        }
        let mut path_buffer = [0_u8; PATH_CAPACITY];
        path_buffer[..bytes.len()].copy_from_slice(bytes);
        let mut stack = guest.stack().await;
        let path_address = stack.push(path_buffer).cast::<libc::c_char>();
        let scratch = stack
            .reserve::<[u8; ANONYMOUS_PROC_FD_TARGET_CAPACITY]>()
            .cast::<libc::c_char>();
        let guard = stack.commit()?;
        let length = guest
            .inject(
                syscalls::Readlinkat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(
                        path_address.as_raw() as *const libc::c_char
                    ))
                    .with_buf(Some(scratch))
                    .with_buf_len(ANONYMOUS_PROC_FD_TARGET_CAPACITY),
            )
            .await;
        let rendered = match length {
            Ok(length) => {
                let mut target = vec![0; (length as usize).min(ANONYMOUS_PROC_FD_TARGET_CAPACITY)];
                guest.memory().read_exact(scratch.cast(), &mut target)?;
                crate::procfs::placeholder_link_path(&target, self.rendered_files(guest))
            }
            Err(_) => None,
        };
        drop(guard);
        Ok(rendered)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#877)
    async fn finish_namespace_readlink<G, S>(
        &self,
        guest: &mut G,
        link: (i32, Option<PathPtr<'_>>),
        path: PathBuf,
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
        syscall: S,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
        S: Into<Syscall>,
    {
        let saved = saved_link_bytes(guest, buffer, buffer_len);
        let result = self.record_or_replay(guest, syscall).await?;
        self.finish_namespace_readlink_result(
            guest,
            link,
            path,
            buffer,
            buffer_len,
            (result, saved.as_deref()),
        )
        .await
    }

    /// [`Self::finish_namespace_readlink`] after the call, which returned
    /// `result` over a buffer that held `saved` before it.
    async fn finish_namespace_readlink_result<G>(
        &self,
        guest: &mut G,
        link: (i32, Option<PathPtr<'_>>),
        path: PathBuf,
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
        (result, saved): (i64, Option<&[u8]>),
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        // A link that filled a buffer too short for a placeholder's link,
        // spelled as a task's descriptor link (`task/<tid>/fd/N`) or starting
        // as a placeholder's link does: the whole link shows whether it was
        // one.
        let filled_like_a_placeholder = match (buffer, usize::try_from(result)) {
            (Some(address), Ok(length)) if length == buffer_len => {
                let prefix = b"/memfd:hermit-procfs:";
                let mut head = vec![0; length.min(prefix.len())];
                guest
                    .memory()
                    .read_exact(address.cast::<u8>(), &mut head)
                    .is_ok()
                    && prefix.starts_with(&head)
            }
            _ => false,
        };
        let result = match usize::try_from(result) {
            Ok(length)
                if length == buffer_len
                    && needs_anonymous_proc_fd_scratch(buffer.is_some(), buffer_len)
                    && !self.cfg.recordreplay_modes
                    && (names_task_descriptor_link(&path) || filled_like_a_placeholder) =>
            {
                match self.placeholder_behind_link(guest, &path).await? {
                    Some(rendered) if self.placeholder_link_confirmed(guest, link).await => {
                        let link = rendered.as_os_str().as_bytes();
                        let written = link.len().min(buffer_len);
                        let address = buffer.ok_or(Errno::EFAULT)?;
                        guest
                            .memory()
                            .write_exact(address.cast(), &link[..written])?;
                        restore_link_tail(guest, address, written, length, saved)?;
                        written as i64
                    }
                    _ => result,
                }
            }
            _ => {
                self.rewrite_placeholder_link(guest, link, buffer, buffer_len, result, saved)
                    .await?
            }
        };
        self.canonicalize_namespace_readlink_result(guest, path, buffer, buffer_len, result)
            .await
    }

    /// The calling process's pid as the guest sees it, which a `/proc/<pid>`
    /// spelling names: Detcore's pid for the process outside a nested PID
    /// namespace ([`Self::procfs_caller_ids`]), the kernel's answer inside
    /// one.
    pub(crate) async fn guest_visible_own_pid<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<i64, Error> {
        match self.procfs_caller_ids(guest) {
            Some((pid, _)) => Ok(i64::from(pid)),
            None => Ok(guest.inject(syscalls::Getpid::new()).await?),
        }
    }

    /// The calling thread's pid and tid as the guest's `/proc` spellings
    /// name them (`/proc/<pid>/...`, `/proc/<pid>/task/<tid>/...`): Detcore's
    /// pid for the thread's process and its tid. This is the one rule for
    /// whether such a spelling is the caller's own, which every check that
    /// recognizes one uses (the descriptor-link recording of an open, and
    /// [`Self::guest_visible_own_pid`]'s callers, such as
    /// `rendered_procfs_at`), so that they cannot disagree.
    ///
    /// They are the numbers the guest's own `getpid` and `gettid` return, on
    /// every backend: measured on ptrace, DBT and KVM for the root process,
    /// one of its threads and its first fork child, an injected `getpid` and
    /// `gettid` returned exactly these. And they are the numbers procfs, as
    /// the guest reaches it, uses for them: ptrace's and KVM's procfs number
    /// processes as Detcore does, and DBT, whose kernel numbers them
    /// otherwise, spells each guest number with the kernel's before the
    /// kernel looks the path up (`Detcore::translates_procfs_numbers`;
    /// DBT refuses record and replay, the one mode that would leave its
    /// numbers untranslated). So nothing is injected to learn them.
    ///
    /// `None` in a nested PID namespace, whose numbers are its own; a caller
    /// that needs the namespace's pid there asks the kernel. Detcore's pid is
    /// the thread's own record of it, as at thread exit: a DBT fork child
    /// keeps its parent's tool.
    pub(crate) fn procfs_caller_ids<G: Guest<Self>>(&self, guest: &G) -> Option<(i32, i32)> {
        let state = guest.thread_state();
        (!state.nested_pid_namespace)
            .then(|| (self.own_detpid(guest).as_raw(), state.dettid.as_raw()))
    }

    /// The calling thread's procfs directory as the tool reaches it
    /// ([`guest_task_procfs`]).
    pub(crate) fn guest_task_directory<G: Guest<Self>>(&self, guest: &G) -> Option<PathBuf> {
        guest_task_procfs(
            &self.cfg.backend,
            guest.tid().as_raw(),
            guest
                .thread_state()
                .changed_root
                .load(std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// The calling thread's root directory as the tool sees it, read from its
    /// procfs directory ([`Self::guest_task_directory`]).
    pub(crate) fn guest_root<G: Guest<Self>>(&self, guest: &G) -> Option<PathBuf> {
        crate::util::raw_read_link(&self.guest_task_directory(guest)?.join("root")).ok()
    }

    /// The absolute spelling of an `exe` link the caller names by `path`
    /// from `dirfd`, read through its own procfs directory: a relative path
    /// joined to the directory the kernel holds for its working directory
    /// or descriptor, which names the process a descriptor on `/proc/self`
    /// was opened for (a child that inherited it reads its parent's link).
    /// `None` where the directory cannot be read, and for an empty path.
    pub(crate) fn exe_link_spelling<G: Guest<Self>>(
        &self,
        guest: &G,
        dirfd: i32,
        path: &Path,
    ) -> Option<PathBuf> {
        if path.is_absolute() {
            return Some(path.to_path_buf());
        }
        if path.as_os_str().is_empty() {
            return None;
        }
        let task = self.guest_task_directory(guest)?;
        let directory = if dirfd == libc::AT_FDCWD {
            task.join("cwd")
        } else {
            task.join("fd").join(dirfd.to_string())
        };
        let directory = std::fs::read_link(directory).ok()?;
        let root = std::fs::read_link(task.join("root")).ok()?;
        Some(
            Path::new("/")
                .join(directory.strip_prefix(&root).ok()?)
                .join(path),
        )
    }

    /// Whose `exe` link the absolute spelling `spelled` reaches, decided the
    /// way the kernel looks it up: its directory resolved in the guest's
    /// view, so `self`, `thread-self`, `..` and symbolic links within it name
    /// the process they lead to, and with `follow` (an open or an exec, which
    /// follow the final link) a symbolic link in the final component followed
    /// too. `None` for any other file, and where the tool cannot read the
    /// guest's filesystem view ([`Self::guest_task_directory`]).
    pub(crate) fn resolve_exe_subject<G: Guest<Self>>(
        &self,
        guest: &G,
        spelled: &Path,
        follow: bool,
    ) -> Option<ExeLinkSubject> {
        let root = self.guest_root(guest)?;
        let inside = |path: &Path| -> Option<PathBuf> {
            Some(Path::new("/").join(path.strip_prefix(&root).ok()?))
        };
        // Where the tool reads the guest's procfs by number rather than
        // through its own `thread-self` (`guest_task_procfs`), name the
        // guest's directory by its pid: a tracer's own `self` and
        // `thread-self` are not the guest's. In-guest tools take the same
        // path, where the numbered directory is their own.
        let numbered_procfs = !self.cfg.backend.tool_shares_guest_descriptor_table;
        let spelled = if numbered_procfs {
            let (pid, tid) = (guest.pid().as_raw(), guest.tid().as_raw());
            let mut parts = spelled.components();
            match (parts.next(), parts.next(), parts.next()) {
                (Some(Component::RootDir), Some(proc), Some(own)) if proc.as_os_str() == "proc" => {
                    let own = match own.as_os_str().to_str() {
                        Some("self") => PathBuf::from(format!("/proc/{pid}")),
                        Some("thread-self") => PathBuf::from(format!("/proc/{pid}/task/{tid}")),
                        _ => spelled.to_path_buf(),
                    };
                    if own.as_path() == spelled {
                        own
                    } else {
                        own.join(parts.as_path())
                    }
                }
                _ => spelled.to_path_buf(),
            }
        } else {
            spelled.to_path_buf()
        };
        let spelled = spelled.as_path();
        // The caller's pid as its procfs numbers it: a tracer's view of it,
        // Detcore's, or in a nested PID namespace the namespace's, read from
        // its own `thread-self` (the tool shares the guest's process there).
        let pid = if numbered_procfs {
            guest.pid().as_raw()
        } else if guest.thread_state().nested_pid_namespace {
            let own = crate::util::canonicalize(&root.join("proc/thread-self")).ok()?;
            let own = inside(&own)?;
            let mut parts = own.components().skip(2);
            parts.next()?.as_os_str().to_str()?.parse().ok()?
        } else {
            self.own_detpid(guest).as_raw()
        };
        let mut spelled = spelled.to_path_buf();
        // As many links as the kernel follows (`MAXSYMLINKS`).
        for _ in 0..40 {
            let name = spelled.file_name()?.to_owned();
            let parent = spelled.parent()?;
            let directory = inside(
                &crate::util::canonicalize(&root.join(parent.strip_prefix("/").ok()?)).ok()?,
            )?;
            let candidate = directory.join(&name);
            if let Some(subject) = exe_link_subject(candidate.as_os_str().as_bytes(), pid) {
                return Some(subject);
            }
            if !follow {
                return None;
            }
            let link = root.join(candidate.strip_prefix("/").ok()?);
            if !std::fs::symlink_metadata(&link)
                .ok()?
                .file_type()
                .is_symlink()
            {
                return None;
            }
            spelled = directory.join(std::fs::read_link(&link).ok()?);
        }
        None
    }

    /// How Detcore answers an `exe` link the caller names by `path` from
    /// `dirfd`: in a run whose launch exec'd a launcher in the guest's place
    /// ([`crate::LaunchImage::launcher`]: SaBRe's loader, DynamoRIO's
    /// library), where the kernel's link names the launcher instead of the
    /// guest's image,
    /// `Some(record)`: the image Detcore recorded at the exec of the process
    /// the link reaches ([`crate::exec_record`]), `None` when it has none to
    /// give, which the caller refuses by name rather than let the kernel name
    /// the launcher. `None` outside such a run and for any other path: the
    /// kernel's answer stands, as on every backend whose kernel exec'd the
    /// guest itself. `follow` as [`Self::resolve_exe_subject`].
    pub(crate) async fn launched_exe<G: Guest<Self>>(
        &self,
        guest: &mut G,
        (dirfd, path): (i32, &Path),
        follow: bool,
    ) -> Option<Option<Arc<crate::exec_record::ExecRecord>>> {
        self.cfg.launch_image.as_ref()?.launcher.as_ref()?;
        let spelled = self.exe_link_spelling(guest, dirfd, path)?;
        let subject = self.resolve_exe_subject(guest, &spelled, follow)?;
        if guest.thread_state().nested_pid_namespace && matches!(subject, ExeLinkSubject::Other(_))
        {
            // Its pids are not Detcore's.
            return Some(None);
        }
        Some(match subject {
            ExeLinkSubject::Own => guest.thread_state().exe.clone(),
            ExeLinkSubject::Other(pid) => {
                crate::tool_global::process_image(guest, crate::types::DetPid::from_raw(pid))
                    .await
                    .map(Arc::new)
            }
        })
    }

    /// Refuse, by name, an `exe` link Detcore cannot answer faithfully in a
    /// run whose kernel names the launcher (`Self::launched_exe`): without a
    /// record, or for an image no longer at its path, whose current name
    /// (or " (deleted)") only the kernel's link to the image could give.
    pub(crate) async fn refuse_launched_exe<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: reverie::syscalls::Sysno,
        exe: Option<&crate::exec_record::ExecRecord>,
    ) -> Result<i64, Error> {
        let reason = match exe {
            None => "unsupported: a /proc/<pid>/exe link of an image Detcore has no record of, \
                     where the kernel's link names the launcher"
                .to_owned(),
            Some(exe) => format!(
                "unsupported: a /proc/<pid>/exe link of {:?}, no longer at its path, \
                 where the kernel's link names the launcher",
                exe.exe
            ),
        };
        self.refuse_launched_exe_because(guest, sysno, reason).await
    }

    /// [`Self::refuse_launched_exe`], for `reason`.
    pub(crate) async fn refuse_launched_exe_because<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: reverie::syscalls::Sysno,
        reason: String,
    ) -> Result<i64, Error> {
        tracing::error!("[tid {}] {}", guest.tid(), reason);
        // Said here too: a launched run's launcher does not print the reason
        // the coordinator records.
        let _ = std::io::Write::write_fmt(
            &mut crate::util::RetryingStderr,
            format_args!("hermit: {reason}\n"),
        );
        self.refuse_unserviceable_operation_because(guest, sysno, Errno::EOPNOTSUPP, &reason)
            .await
    }

    /// Refuse, by name, a readlink of a possible `exe` link at `path`
    /// ([`exe_link_candidate`]) whose answer, `result` bytes in `buffer`,
    /// is the launcher, in a run that declared one
    /// ([`crate::LaunchImage::launcher`]): a link Detcore could not resolve
    /// (after a guest `chroot`, through a descriptor for the link itself)
    /// whose kernel answer would be the launcher in the guest's image's
    /// place. The answer must be the launcher's whole path; one the kernel
    /// truncated to the buffer is read again whole (`link`, the guest's
    /// lookup) before it counts. The buffer gets back the bytes in `saved`
    /// before the refusal, so a run that lets the guest go on sees it
    /// untouched. Every other result passes through.
    async fn refuse_launcher_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        (sysno, path, link): (reverie::syscalls::Sysno, &Path, (i32, Option<PathPtr<'_>>)),
        (buffer, buffer_len): (Option<AddrMut<'_, libc::c_char>>, usize),
        result: i64,
        saved: Option<&[u8]>,
    ) -> Result<i64, Error> {
        let (Some(launcher), Some(buffer), Ok(len)) =
            (self.declared_launcher(), buffer, usize::try_from(result))
        else {
            return Ok(result);
        };
        if len == 0 || !exe_link_candidate(path) {
            return Ok(result);
        }
        let launcher = launcher.as_os_str().as_bytes();
        let truncated = len == buffer_len && len < launcher.len();
        if len > launcher.len() || (len < launcher.len() && !truncated) {
            return Ok(result);
        }
        let mut answer = vec![0; len];
        if guest
            .memory()
            .read_exact(buffer.cast::<u8>(), &mut answer)
            .is_err()
            || !launcher.starts_with(&answer)
        {
            return Ok(result);
        }
        if truncated && self.whole_link(guest, link).await.as_deref() != Some(launcher) {
            return Ok(result);
        }
        restore_saved_link_bytes(guest, buffer, len, saved);
        self.refuse_launched_exe(guest, sysno, None).await
    }

    /// The whole target of the link the guest names by `link` (its `dirfd`
    /// and pathname), read again by the kernel into a buffer that holds any
    /// path Linux renders. `None` where it cannot be read so, or the guest
    /// stack cannot hold the buffer.
    async fn whole_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        (dirfd, path): (i32, Option<PathPtr<'_>>),
    ) -> Option<Vec<u8>> {
        const CAPACITY: usize = libc::PATH_MAX as usize;
        let mut stack = guest.stack().await;
        if stack.capacity() < CAPACITY {
            return None;
        }
        let scratch = stack.reserve::<[u8; CAPACITY]>().cast::<libc::c_char>();
        let guard = stack.commit().ok()?;
        let call = syscalls::Readlinkat::new()
            .with_dirfd(dirfd)
            .with_path(path)
            .with_buf(Some(scratch))
            .with_buf_len(CAPACITY);
        let length = usize::try_from(guest.inject(call).await.ok()?).ok()?;
        let mut target = vec![0; length.min(CAPACITY)];
        let read = guest.memory().read_exact(scratch.cast(), &mut target);
        drop(guard);
        read.ok().map(|()| target)
    }

    /// The launcher a launched run declared ([`crate::LaunchImage::launcher`]),
    /// canonical.
    pub(crate) fn declared_launcher(&self) -> Option<PathBuf> {
        let launcher = self.cfg.launch_image.as_ref()?.launcher.as_ref()?;
        Some(crate::util::canonicalize(launcher).unwrap_or_else(|_| launcher.clone()))
    }

    /// Answer a readlink of an `exe` link from `exe` (`Self::launched_exe`)
    /// after the kernel answered the guest's call, with `result`: the kernel
    /// decides whether the link is there and every error (an exited
    /// thread-group leader's `ENOENT`, a permission refusal, an unwritable
    /// buffer), and Detcore what it names, the image's path. The bytes the
    /// kernel wrote past the shorter target are restored from `saved`, as
    /// Linux leaves them.
    async fn substitute_exe_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        (sysno, exe): (
            reverie::syscalls::Sysno,
            Option<Arc<crate::exec_record::ExecRecord>>,
        ),
        (buffer, buffer_len): (Option<AddrMut<'_, libc::c_char>>, usize),
        result: i64,
        saved: Option<&[u8]>,
    ) -> Result<i64, Error> {
        let (Some(buffer), Ok(kernel_len)) = (buffer, usize::try_from(result)) else {
            return Ok(result);
        };
        let present = exe.as_ref().filter(|exe| {
            self.guest_root(guest)
                .is_some_and(|root| exe.is_present(&root))
        });
        let Some(exe) = present else {
            restore_saved_link_bytes(guest, buffer, kernel_len, saved);
            return self.refuse_launched_exe(guest, sysno, exe.as_deref()).await;
        };
        let target = exe.exe.as_os_str().as_bytes();
        let written = target.len().min(readlink_size(buffer_len)?);
        guest
            .memory()
            .write_exact(buffer.cast::<u8>(), &target[..written])
            .map_err(|_| Errno::EFAULT)?;
        restore_link_tail(guest, buffer, written, kernel_len, saved)?;
        Ok(written as i64)
    }

    /// The layout ([`crate::exec_record::ElfLayout`]) of the image the
    /// calling thread's exec ran, read from the exec record's file and checked
    /// to be the recorded inode, with that inode; or why there is none.
    fn recorded_image_layout<G: Guest<Self>>(
        &self,
        guest: &G,
    ) -> Result<(crate::exec_record::ElfLayout, u64), &'static str> {
        let exe = guest.thread_state().exe.clone().ok_or("no exec record")?;
        let root = self
            .guest_root(guest)
            .ok_or("the guest's root is unreadable")?;
        let path = root.join(
            exe.exe
                .strip_prefix("/")
                .map_err(|_| "a relative image path")?,
        );
        // Pin the object at the path without opening it for I/O (`O_PATH`, no
        // link followed), and read it only once it is the recorded regular
        // file: a FIFO or device put at the path since the exec is never
        // opened. Raw syscalls throughout: in-guest Detcore must not call
        // libc's wrappers (`crate::util::raw_syscall`).
        let pinned = crate::util::RawFd::open(&path, libc::O_PATH | libc::O_NOFOLLOW)
            .map_err(|_| "the image is unreadable")?;
        let stat = pinned.stat().map_err(|_| "the image is unreadable")?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || (stat.st_dev, stat.st_ino) != exe.identity()
        {
            return Err("the image's path names another file since the exec");
        }
        // Reopening the pinned descriptor's own procfs link opens the same
        // inode, whatever the path names by now.
        let file = crate::util::RawFd::open(
            Path::new(&format!("/proc/self/fd/{}", pinned.raw())),
            libc::O_RDONLY,
        )
        .map_err(|_| "the image is unreadable")?;
        let layout = crate::exec_record::ElfLayout::read_file(&file)
            .ok_or("the image is not an ELF64 executable")?;
        Ok((layout, stat.st_ino))
    }

    /// Linux's start of the program break for the image the calling thread's
    /// exec ran: load_elf_binary's rule (`Self::recorded_image_layout`) at
    /// the load bias the process's `AT_PHDR` shows, or why there is none.
    fn modeled_start_brk<G: Guest<Self>>(&self, guest: &G) -> Result<u64, &'static str> {
        let (layout, _) = self.recorded_image_layout(guest)?;
        let auxv = self
            .guest_task_directory(guest)
            .and_then(|directory| crate::util::raw_read_file(&directory.join("auxv")).ok())
            .ok_or("the auxiliary vector is unreadable")?;
        let at_phdr = crate::exec_record::auxv_entry(&auxv, libc::AT_PHDR)
            .ok_or("the auxiliary vector has no AT_PHDR")?;
        layout.start_brk(at_phdr)
    }

    /// Check Detcore's model of the program break for the image an exec just
    /// ran (`Self::modeled_start_brk`) against the kernel's `start_brk`
    /// (`/proc/<pid>/stat` field 47), and log the outcome
    /// ([`crate::exec_record::BrkCheck`]). A model that differs is a Detcore
    /// defect, reported as an error. The model is compared only where Linux's
    /// rule gives the kernel's start: not where the kernel randomized the
    /// break (`Self::break_randomized`), nor where that is unknown. The
    /// personality read is the process's current one. Every launcher Hermit
    /// execs a guest through sets `ADDR_NO_RANDOMIZE` before the exec, and a
    /// ptrace guest's own exec is checked at its exec stop, before any of the
    /// new image's code. The in-guest backends that declare no launcher
    /// (LiteInst, in-guest-trap) check only the image they launched: they
    /// refuse a guest's exec today. So a guest can only clear the flag before
    /// the check (a skipped comparison), not set it after a randomized exec.
    /// Should one of them come to follow an exec, its check would run after
    /// the new image's constructors, which could set the flag after a
    /// randomized exec; that case would need the exec-time personality. DBT
    /// and SaBRe, which follow execs, declare a launcher and take the branch
    /// below, which compares nothing with the kernel.
    ///
    /// Where the run declares a launcher, the kernel exec'd the launcher, so
    /// the process's auxiliary vector and break are the launcher's: Detcore
    /// keeps its own model instead (`Self::launched_brk_model`), with no
    /// kernel to compare it with, and answers `brk(2)` from it
    /// (`Self::handle_brk`). That needs Detcore's tool, which shares the
    /// guest's C library there, never to grow the guest's heap through the
    /// kernel's break; its `realpath` and `opendir` calls were removed for that
    /// (detcore/clippy.toml). Without a model, a brk is refused by name with
    /// the reason (`Detcore::handle_brk`).
    pub(crate) async fn check_brk_model<G: Guest<Self>>(&self, guest: &mut G) {
        use crate::exec_record::BrkCheck;
        let dettid = guest.thread_state().dettid;
        if self.declares_launcher() {
            let model = self.launched_brk_model(guest).await;
            match &model {
                Ok(model) => tracing::debug!(
                    "[detcore, dtid {}] program break serviced from a model starting at {:#x}: the run declares a launcher",
                    dettid,
                    model.start,
                ),
                Err(reason) => tracing::warn!(
                    "[detcore, dtid {}] program break not modeled: {}; a brk is refused by name",
                    dettid,
                    reason,
                ),
            }
            let mut metadata = guest
                .thread_state()
                .memory_metadata
                .lock()
                .expect("memory metadata mutex poisoned");
            metadata.brk_unmodeled = model.as_ref().err().map(|reason| (*reason).to_owned());
            metadata.brk_model = model.ok();
            return;
        }
        let check = {
            match self.modeled_start_brk(guest) {
                Err(reason) => BrkCheck::Unmodeled(reason),
                model => BrkCheck::of(
                    model,
                    self.break_randomized(guest),
                    self.kernel_start_brk(guest),
                ),
            }
        };
        if let BrkCheck::Differs { .. } = check {
            tracing::error!("[detcore, dtid {}] program break {}", dettid, check);
        } else {
            tracing::debug!("[detcore, dtid {}] program break {}", dettid, check);
        }
    }

    /// Whether the run declares a launcher ([`crate::LaunchImage::launcher`]):
    /// the kernel exec'd it in the guest's place, so the process's auxiliary
    /// vector and program break are the launcher's.
    pub(crate) fn declares_launcher(&self) -> bool {
        self.cfg
            .launch_image
            .as_ref()
            .is_some_and(|image| image.launcher.is_some())
    }

    /// Detcore's model of the program break for the image an exec just ran,
    /// where the run declares a launcher (`Self::declares_launcher`):
    /// load_elf_binary's rule (`Self::recorded_image_layout`) at the bias of
    /// the image's own executable mapping ([`crate::exec_record::load_bias`]),
    /// since the process's `AT_PHDR` is the launcher's. Or why there is none.
    async fn launched_brk_model<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<crate::memory::BrkModel, &'static str> {
        // Detcore's brk transaction (`Self::handle_brk`) spans injected
        // calls, which Linux makes atomic under the address space's lock;
        // only sequentialized threads keep another thread out of it.
        if !self.cfg.sequentialize_threads {
            return Err("threads are not sequentialized");
        }
        let exe = guest.thread_state().exe.clone().ok_or("no exec record")?;
        let (layout, ino) = self.recorded_image_layout(guest)?;
        let maps = self
            .guest_task_directory(guest)
            .and_then(|directory| crate::util::raw_read_file(&directory.join("maps")).ok())
            .ok_or("the mappings are unreadable")?;
        let text = layout
            .text
            .ok_or("an image without an executable segment")?;
        let bias = crate::exec_record::load_bias(&maps, &exe.exe, ino, text)?;
        let start = layout.start_brk_at_bias(bias)?;
        // Guest code that ran before Detcore's first callback (a pre-init
        // function, an IFUNC resolver) used the kernel's break, the
        // launcher's, if it moved it: its heap is there, and a model elsewhere
        // would split it.
        let kernel_start = self
            .kernel_start_brk(guest)
            .ok_or("the kernel's start of the break is unreadable")?;
        let kernel_break = guest
            .inject_with_retry(reverie::syscalls::Syscall::Brk(
                reverie::syscalls::Brk::new().with_addr(None),
            ))
            .await
            .map_err(|_| "the kernel's break is unreadable")? as u64;
        if kernel_break != kernel_start {
            return Err("guest code moved the launcher's break before Detcore modeled its own");
        }
        tracing::debug!(
            target: "detcore::brk_diagnostic",
            dettid = guest.thread_state().dettid.as_raw(),
            image = ?exe.exe,
            identity = ?exe.identity(),
            image_inode = ino,
            layout = ?layout,
            bias,
            start,
            kernel_start,
            kernel_break,
            "BRK_MODEL_V1",
        );
        Ok(crate::memory::BrkModel {
            start,
            current: start,
            data_span: layout.data_span,
            launcher_break: kernel_start,
        })
    }

    /// Whether the kernel randomized the calling thread's program break at
    /// its exec: it does where its process runs without `ADDR_NO_RANDOMIZE`
    /// (`/proc/<pid>/personality`) and `kernel.randomize_va_space` is above
    /// 1. `None` where either cannot be read.
    fn break_randomized<G: Guest<Self>>(&self, guest: &G) -> Option<bool> {
        let personality =
            crate::util::raw_read_file(&self.guest_task_directory(guest)?.join("personality"))
                .ok()?;
        let personality =
            u64::from_str_radix(std::str::from_utf8(&personality).ok()?.trim(), 16).ok()?;
        if personality & libc::ADDR_NO_RANDOMIZE as u64 != 0 {
            return Some(false);
        }
        let level =
            crate::util::raw_read_file(Path::new("/proc/sys/kernel/randomize_va_space")).ok()?;
        Some(
            std::str::from_utf8(&level)
                .ok()?
                .trim()
                .parse::<u32>()
                .ok()?
                > 1,
        )
    }

    /// The kernel's `start_brk` for the calling thread's process, field 47 of
    /// its `/proc/<pid>/stat` ([`crate::exec_record::stat_start_brk`]), read
    /// where the tool reaches its procfs.
    fn kernel_start_brk<G: Guest<Self>>(&self, guest: &G) -> Option<u64> {
        let stat =
            crate::util::raw_read_file(&self.guest_task_directory(guest)?.join("stat")).ok()?;
        // The command name may be any bytes; the fields after it are ASCII.
        crate::exec_record::stat_start_brk(&String::from_utf8_lossy(&stat))
    }

    /// Detcore's pid for the calling thread's process.
    pub(crate) fn own_detpid<G: Guest<Self>>(&self, guest: &G) -> crate::types::DetPid {
        guest.thread_state().detpid.unwrap_or(self.detpid)
    }

    /// Project an already-completed own-ID query. Like `guest_visible_own_pid`,
    /// ordinary processes use the current thread's shared identity, including
    /// a fork child that kept the parent Tool. A nested namespace keeps the
    /// answer already delegated to the inner Tool, without a second injection.
    ///
    /// Detcore is a filter-map: a positive inner result establishes success,
    /// not the final guest identity. Preserve unusual nonpositive values;
    /// errors never reach this projection. This synchronous step adds no
    /// future around the existing syscall delegation.
    pub(crate) fn project_own_getter_result<G: Guest<Self>>(
        &self,
        guest: &G,
        call: Syscall,
        result: i64,
    ) -> i64 {
        if result <= 0 || guest.thread_state().nested_pid_namespace {
            return result;
        }
        match call {
            Syscall::Getpid(_) => i64::from(self.own_detpid(guest).as_raw()),
            Syscall::Gettid(_) => i64::from(guest.thread_state().dettid.as_raw()),
            _ => unreachable!("only own-ID getters have a shared result projection"),
        }
    }

    /// Answer `readlink` of a `/proc` link whose target Detcore decides from
    /// its own model ([`decided_procfs_link`]), at the guest's absolute
    /// spelling `path` (for a guest pathname `spelled`), without asking the
    /// backend, so every backend answers alike (KVM's own `/proc` refuses
    /// `thread-self` and the namespace links). The caller's own descriptor
    /// link spelled through its thread is answered as `/proc/self/fd/N` is,
    /// with the kernel reading `/proc/thread-self/fd/N`, the calling thread's
    /// own table. Not in a nested PID namespace, where the caller's pid is
    /// not Detcore's, nor after a guest `chroot`, where `/proc` need not be
    /// procfs, nor for a pathname too long for Linux to look up.
    async fn readlink_decided_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
        (spelled, link): (&Path, (i32, Option<PathPtr<'_>>)),
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
    ) -> Result<Option<i64>, Error> {
        let state = guest.thread_state();
        if state.nested_pid_namespace
            || state.changed_root.load(std::sync::atomic::Ordering::SeqCst)
            || spelled.as_os_str().len() >= libc::PATH_MAX as usize
        {
            return Ok(None);
        }
        let decided = decided_procfs_link(
            path.as_os_str().as_bytes(),
            self.own_detpid(guest).as_raw(),
            state.dettid.as_raw(),
        );
        let Some(decided) = decided else {
            return Ok(None);
        };
        // Linux takes the size as an int, and refuses one that is not
        // positive before it looks up the path.
        let size = readlink_size(buffer_len)?;
        if let DecidedLink::SelfFd(fd) = decided {
            return self
                .readlink_own_descriptor_link(guest, fd, link, buffer, size)
                .await
                .map(Some);
        }
        match decided {
            DecidedLink::Target(target) => {
                let copied = target.len().min(size);
                let address = buffer.ok_or(Errno::EFAULT)?;
                // Linux gives EFAULT for a buffer it cannot write; a backend's
                // write can fail otherwise (ptrace's EIO).
                guest
                    .memory()
                    .write_exact(address.cast::<u8>(), &target[..copied])
                    .map_err(|_| Errno::EFAULT)?;
                Ok(Some(copied as i64))
            }
            DecidedLink::Followed(errno) => Err(errno.into()),
            DecidedLink::SelfFd(_) => unreachable!("answered above"),
        }
    }

    /// Answer `readlink` of the caller's own descriptor link `fd` spelled
    /// through its thread directory, into a buffer of `size` bytes, as
    /// `/proc/self/fd/N` is answered (a rendered procfs file's path, a
    /// placeholder's, or the kernel's link canonicalized), but by
    /// `/proc/thread-self/fd/N`, which every lookup here reads: a thread
    /// made without `CLONE_FILES`, or after `unshare(CLONE_FILES)`, has a
    /// table of its own, and the leader's may be gone. KVM's own `/proc`
    /// refuses `thread-self`, so there they read `/proc/self/fd/N`.
    async fn readlink_own_descriptor_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        link: (i32, Option<PathPtr<'_>>),
        buffer: Option<AddrMut<'_, libc::c_char>>,
        size: usize,
    ) -> Result<i64, Error> {
        let directory = if crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend) {
            "thread-self"
        } else {
            "self"
        };
        let physical = PathBuf::from(format!("/proc/{directory}/fd/{fd}"));
        if let Some(result) = self
            .readlink_rendered_procfs(guest, &physical, buffer, size)
            .await?
        {
            return Ok(result);
        }
        let bytes = physical.as_os_str().as_bytes();
        let mut path_buffer = [0_u8; 64];
        path_buffer[..bytes.len()].copy_from_slice(bytes);
        let saved = saved_link_bytes(guest, buffer, size);
        let mut stack = guest.stack().await;
        let path_address = stack.push(path_buffer).cast::<libc::c_char>();
        let stack_guard = stack.commit()?;
        let call = syscalls::Readlink::new()
            .with_path(PathPtr::from_ptr(
                path_address.as_raw() as *const libc::c_char
            ))
            .with_buf(buffer)
            .with_bufsize(size);
        let result = self.record_or_replay(guest, call).await?;
        // The stack is released before anything else uses it; a placeholder
        // is confirmed through the guest's own pathname, as before.
        drop(stack_guard);
        self.finish_namespace_readlink_result(
            guest,
            link,
            physical,
            buffer,
            size,
            (result, saved.as_deref()),
        )
        .await
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#877)
    /// Preserve Linux readlink errors and canonicalize procfs namespace identities.
    pub async fn handle_readlink<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readlink,
    ) -> Result<i64, Error> {
        // Linux refuses a size that is not positive before it reads the path.
        readlink_size(call.bufsize())?;
        let path: PathBuf = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        if let Some(exe) = self
            .launched_exe(guest, (libc::AT_FDCWD, &path), false)
            .await
        {
            let saved = saved_exe_link_bytes(guest, call.buf(), call.bufsize());
            // As `readlinkat`, the same lookup: a launcher may answer an exact
            // `readlink` of the own link itself (SaBRe's plugin before
            // Reverie dropped it), and the kernel must decide existence.
            let kernel = syscalls::Readlinkat::new()
                .with_dirfd(libc::AT_FDCWD)
                .with_path(call.path())
                .with_buf(call.buf())
                .with_buf_len(call.bufsize());
            let result = self.record_or_replay(guest, kernel).await?;
            let buffer = (call.buf(), call.bufsize());
            return self
                .substitute_exe_link(
                    guest,
                    (reverie::syscalls::Sysno::readlink, exe),
                    buffer,
                    result,
                    saved.as_deref(),
                )
                .await;
        }
        let saved = self
            .declared_launcher()
            .and_then(|_| saved_exe_link_bytes(guest, call.buf(), call.bufsize()));
        let result = self.readlink_unlaunched(guest, call, path.clone()).await?;
        self.refuse_launcher_link(
            guest,
            (
                reverie::syscalls::Sysno::readlink,
                &path,
                (libc::AT_FDCWD, call.path()),
            ),
            (call.buf(), call.bufsize()),
            result,
            saved.as_deref(),
        )
        .await
    }

    /// [`Self::handle_readlink`] of a link Detcore does not answer from an
    /// exec record.
    async fn readlink_unlaunched<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readlink,
        path: PathBuf,
    ) -> Result<i64, Error> {
        if let Some(result) = self
            .readlink_rendered_procfs(guest, &path, call.buf(), call.bufsize())
            .await?
        {
            return Ok(result);
        }
        if let Some(result) = self
            .readlink_decided_link(
                guest,
                &path,
                (&path, (libc::AT_FDCWD, call.path())),
                call.buf(),
                call.bufsize(),
            )
            .await?
        {
            return Ok(result);
        }
        let (host_path, other_proc_fd) = if let Some((Some(subject), _)) = proc_fd_target(&path) {
            let current_pid = self.guest_visible_own_pid(guest).await?;
            (
                host_self_proc_fd_alias(&path, current_pid),
                current_pid != i64::from(subject),
            )
        } else {
            (None, false)
        };
        if let Some(host_path) = host_path {
            let bytes = host_path.as_os_str().as_bytes();
            let mut path_buffer = [0_u8; 64];
            path_buffer[..bytes.len()].copy_from_slice(bytes);
            let mut stack = guest.stack().await;
            let path_address = stack.push(path_buffer).cast::<libc::c_char>();
            let stack_guard = stack.commit()?;
            let physical_call = call.with_path(PathPtr::from_ptr(
                path_address.as_raw() as *const libc::c_char
            ));
            let result = self.record_or_replay(guest, physical_call).await?;
            drop(stack_guard);
            return self
                .canonicalize_namespace_readlink_result(
                    guest,
                    path,
                    call.buf(),
                    call.bufsize(),
                    result,
                )
                .await;
        }
        if other_proc_fd {
            return self.finish_other_proc_fd_readlink(guest, call).await;
        }
        self.finish_namespace_readlink(
            guest,
            (libc::AT_FDCWD, call.path()),
            path,
            call.buf(),
            call.bufsize(),
            call,
        )
        .await
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#877)
    /// Preserve Linux readlinkat errors and canonicalize absolute procfs namespace identities.
    pub async fn handle_readlinkat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readlinkat,
    ) -> Result<i64, Error> {
        // Linux refuses a size that is not positive before it reads the path.
        readlink_size(call.buf_len())?;
        let path: PathBuf = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        if let Some(exe) = self.launched_exe(guest, (call.dirfd(), &path), false).await {
            let saved = saved_exe_link_bytes(guest, call.buf(), call.buf_len());
            let result = self.record_or_replay(guest, call).await?;
            let buffer = (call.buf(), call.buf_len());
            return self
                .substitute_exe_link(
                    guest,
                    (reverie::syscalls::Sysno::readlinkat, exe),
                    buffer,
                    result,
                    saved.as_deref(),
                )
                .await;
        }
        let saved = self
            .declared_launcher()
            .and_then(|_| saved_exe_link_bytes(guest, call.buf(), call.buf_len()));
        let result = self
            .readlinkat_unlaunched(guest, call, path.clone())
            .await?;
        self.refuse_launcher_link(
            guest,
            (
                reverie::syscalls::Sysno::readlinkat,
                &path,
                (call.dirfd(), call.path()),
            ),
            (call.buf(), call.buf_len()),
            result,
            saved.as_deref(),
        )
        .await
    }

    /// [`Self::handle_readlinkat`] of a link Detcore does not answer from an
    /// exec record.
    async fn readlinkat_unlaunched<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readlinkat,
        path: PathBuf,
    ) -> Result<i64, Error> {
        // A relative name below a directory descriptor is spelled as Linux
        // looks it up (`Detcore::procfs_spelling_at`): below a `/proc/self`
        // or `/proc/<pid>` directory, the task the directory was opened on
        // (ESRCH once that task is gone), and below a descriptor Detcore has
        // no record of (one received over `SCM_RIGHTS`), the procfs directory
        // the kernel names for it; so a namespace or descriptor link there is
        // modeled and canonicalized as the same link spelled absolutely. Any
        // other such name stays relative, and the kernel decides.
        let below_directory =
            !path.is_absolute() && !path.as_os_str().is_empty() && call.dirfd() != libc::AT_FDCWD;
        let recorded = if path.is_absolute() || call.dirfd() == libc::AT_FDCWD {
            None
        } else {
            guest
                .thread_state()
                .with_detfd(call.dirfd(), |detfd| detfd.path())
                .unwrap_or(None)
        };
        let lexical = if path.is_absolute() || call.dirfd() == libc::AT_FDCWD {
            path.clone()
        } else {
            recorded
                .as_ref()
                .map_or(path.clone(), |directory| directory.join(&path))
        };
        let observed_path = if below_directory {
            self.procfs_spelling_at(guest, call.dirfd(), &path).await?
        } else {
            lexical.clone()
        };
        // The directory names a task or a kernel path Detcore knows.
        let named_by_directory = below_directory && observed_path != lexical;
        if let Some(result) = self
            .readlink_rendered_procfs(guest, &observed_path, call.buf(), call.buf_len())
            .await?
        {
            return Ok(result);
        }
        // A relative pathname is decided only below a descriptor on `/proc`
        // itself, a directory, or one whose task or kernel path Detcore knows
        // (a selector's directory records its task only where the open
        // followed the selector, not for the link itself, an
        // `O_PATH | O_NOFOLLOW` open of `/proc/self`). An empty pathname reads
        // the descriptor's own link, which the kernel answers (an inherited
        // one keeps its opener's binding).
        let decidable = path.is_absolute()
            || named_by_directory
            || (below_directory
                && recorded
                    .as_deref()
                    .is_some_and(|directory| directory == Path::new("/proc")));
        if decidable
            && let Some(result) = self
                .readlink_decided_link(
                    guest,
                    &observed_path,
                    (&path, (call.dirfd(), call.path())),
                    call.buf(),
                    call.buf_len(),
                )
                .await?
        {
            return Ok(result);
        }
        if let Some((Some(subject), _)) = proc_fd_target(&observed_path) {
            let current_pid = self.guest_visible_own_pid(guest).await?;
            if current_pid != i64::from(subject) {
                return self.finish_other_proc_fd_readlinkat(guest, call).await;
            }
        }
        self.finish_namespace_readlink(
            guest,
            (call.dirfd(), call.path()),
            observed_path,
            call.buf(),
            call.buf_len(),
            call,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    /// A failed probe (here, the pipe's descriptor limit) is kept as its kind
    /// and errno number, not the C library's message, which `to_string`
    /// fetches through strerror_r.
    #[test]
    fn a_failed_device_probe_is_kept_without_the_c_library_message() {
        let failure = || std::io::Error::from_raw_os_error(libc::EMFILE);
        assert!(failure().to_string().contains("Too many open files"));
        let kept = super::retained_probe_outcome(Err(failure())).unwrap_err();
        assert!(kept.ends_with("(errno 24)"), "{kept}");
        assert!(!kept.contains("Too many open files"), "{kept}");
    }

    use super::*;

    /// The devices read once are the pipe and socket filesystems' own: a
    /// fresh pipe and socket show the same ones, and the two differ.
    #[test]
    fn anonymous_object_devices_are_the_pipe_and_socket_filesystems() {
        let read = anonymous_object_devices();
        let devices = read.as_ref().unwrap();
        assert_eq!(&probe_anonymous_object_devices().unwrap(), devices);
        assert_ne!(devices.pipe, devices.socket);
        assert_eq!(anonymous_object_device(read, "pipe").unwrap(), devices.pipe);
        assert_eq!(
            anonymous_object_device(read, "socket").unwrap(),
            devices.socket
        );
        // Devices that could not be read at start, as with the process's
        // descriptors exhausted, fail the run as a tool error and never
        // reach the guest as an errno.
        let unread = Err("Too many open files (os error 24)".to_string());
        for kind in ["pipe", "socket"] {
            let error = anonymous_object_device(&unread, kind).unwrap_err();
            assert!(matches!(error, Error::Tool(_)), "{error:?}");
            assert!(error.to_string().contains("os error 24"), "{error}");
        }
    }

    #[test]
    fn names_whose_exe_link_a_spelling_reads() {
        let subject = |path: &str| exe_link_subject(path.as_bytes(), 3);
        for path in [
            "/proc/self/exe",
            "/proc/thread-self/exe",
            "/proc/3/exe",
            "/proc//self/./exe",
            "/proc/self/task/5/exe",
            "/proc/3/task/7/exe",
        ] {
            assert_eq!(subject(path), Some(ExeLinkSubject::Own), "{path}");
        }
        assert_eq!(subject("/proc/4/exe"), Some(ExeLinkSubject::Other(4)));
        assert_eq!(
            subject("/proc/4/task/9/exe"),
            Some(ExeLinkSubject::Other(4))
        );
        for path in [
            "/proc/self/exe/",
            "/proc/self/exe/.",
            "/proc/self/../self/exe",
            "/proc/04/exe",
            "/proc/self/cwd",
            "/proc/thread-self/task/5/exe",
            "/proc/self/task/x/exe",
            "/proc/exe",
            "proc/self/exe",
            "/tmp/self/exe",
        ] {
            assert_eq!(subject(path), None, "{path}");
        }
    }

    #[test]
    fn decides_the_links_detcore_knows_the_targets_of() {
        let decided = |path: &str| decided_procfs_link(path.as_bytes(), 3, 5);
        let target = |bytes: &[u8]| Some(DecidedLink::Target(bytes.to_vec()));
        assert_eq!(decided("/proc/self"), target(b"3"));
        assert_eq!(decided("/proc/./self"), target(b"3"));
        assert_eq!(decided("/proc/thread-self"), target(b"3/task/5"));
        assert_eq!(decided("/proc/mounts"), target(b"self/mounts"));
        assert_eq!(decided("/proc/net"), target(b"self/net"));
        assert_eq!(decided("/proc/self/ns/pid"), target(b"pid:[4026531836]"));
        assert_eq!(decided("/proc/3/ns/mnt"), target(b"mnt:[4026531841]"));
        assert_eq!(
            decided("/proc/thread-self/ns/user"),
            target(b"user:[4026531837]")
        );
        assert_eq!(
            decided("/proc/3/task/5/ns/net"),
            decided("/proc/self/ns/net")
        );
        assert_eq!(
            decided("/proc/self/task/5/fd/7"),
            Some(DecidedLink::SelfFd(7))
        );
        assert_eq!(decided("/proc/3/task/5/fd/0"), Some(DecidedLink::SelfFd(0)));
        assert_eq!(decided("/proc//self"), target(b"3"));
        // Linux takes the size as an int.
        assert_eq!(readlink_size(0), Err(Errno::EINVAL));
        assert_eq!(readlink_size(1 << 31), Err(Errno::EINVAL));
        assert_eq!(readlink_size((1 << 32) + 5), Ok(5));
        assert_eq!(readlink_size(i32::MAX as usize), Ok(i32::MAX as usize));
        assert_eq!(decided("/proc/self/ns/./pid"), decided("/proc/self/ns/pid"));
        // A trailing slash or `.` follows the link: to a directory, EINVAL; to
        // a file, ENOTDIR.
        let followed = |errno| Some(DecidedLink::Followed(errno));
        for path in [
            "/proc/self/",
            "/proc/self/.",
            "/proc/self//",
            "/proc/thread-self/",
            "/proc/net/",
        ] {
            assert_eq!(decided(path), followed(Errno::EINVAL), "{path}");
        }
        // The caller's own directories are no links: EINVAL, followed or not.
        for path in [
            "/proc/3",
            "/proc/3/",
            "/proc/3/.",
            "/proc/3/task",
            "/proc/self/task",
            "/proc/3/task/5",
            "/proc/self/task/5/",
        ] {
            assert_eq!(decided(path), followed(Errno::EINVAL), "{path}");
        }
        for path in [
            "/proc/mounts/",
            "/proc/mounts/.",
            "/proc/self/ns/pid/",
            "/proc/thread-self/ns/pid/.",
            "/proc/3/task/5/ns/net/",
        ] {
            assert_eq!(decided(path), followed(Errno::ENOTDIR), "{path}");
        }
        // Left to the kernel: `..`, a followed descriptor link, another
        // process's or thread's links, unknown namespaces, and links Detcore
        // does not decide yet.
        for path in [
            "/proc/self/task/5/fd/7/",
            "/proc/4/",
            "/proc/",
            "/proc/self/../self",
            "/proc/4/ns/pid",
            "/proc/self/task/6/ns/pid",
            "/proc/self/task/6/fd/7",
            "/proc/4/task",
            "/proc/3/task/6",
            "/proc/5",
            "/proc/self/ns/unknown",
            "/proc/self/fd/7",
            "/proc/self/exe",
            "/proc/self/cwd",
            "/proc/selfx",
            "/tmp/self",
            "proc/self",
        ] {
            assert_eq!(decided(path), None, "{path}");
        }
    }

    #[test]
    fn recognizes_process_and_thread_namespace_links() {
        assert_eq!(
            canonical_namespace_target(Path::new("/proc/self/ns/mnt")),
            Some(b"mnt:[4026531841]".as_slice())
        );
        assert_eq!(
            canonical_namespace_target(Path::new("/proc/123/task/456/ns/user")),
            Some(b"user:[4026531837]".as_slice())
        );
        assert_eq!(
            canonical_namespace_target(Path::new("/proc/thread-self/ns/pid_for_children")),
            Some(b"pid:[4026531836]".as_slice())
        );
    }

    #[test]
    fn leaves_non_namespace_and_relative_links_untouched() {
        assert_eq!(
            canonical_namespace_target(Path::new("/proc/self/exe")),
            None
        );
        assert_eq!(canonical_namespace_target(Path::new("/tmp/ns/mnt")), None);
        assert_eq!(
            canonical_namespace_target(Path::new("proc/self/ns/mnt")),
            None
        );
        assert_eq!(
            canonical_namespace_target(Path::new("/proc/self/ns/unknown")),
            None
        );
    }

    #[test]
    fn recognizes_proc_fd_aliases_and_lexical_normalization() {
        for (path, expected) in [
            ("/proc/self/fd/1", (None, 1)),
            ("/proc/thread-self/fd/20", (None, 20)),
            ("/proc/123/fd/7", (Some(123), 7)),
            ("/proc/self/fd/../fd/9", (None, 9)),
            ("/dev/fd/3", (None, 3)),
        ] {
            assert_eq!(proc_fd_target(Path::new(path)), Some(expected), "{path}");
        }
    }

    #[test]
    fn rewrites_only_numeric_virtual_self_proc_fd_aliases() {
        assert_eq!(
            host_self_proc_fd_alias(Path::new("/proc/123/fd/7"), 123),
            Some(PathBuf::from("/proc/self/fd/7"))
        );
        assert_eq!(
            host_self_proc_fd_alias(Path::new("/proc/124/fd/7"), 123),
            None
        );
        assert_eq!(
            host_self_proc_fd_alias(Path::new("/proc/self/fd/7"), 123),
            None
        );
    }

    /// The caller's own descriptor links, by every spelling procfs and
    /// `/dev/fd` give them, name its descriptor, in the process's table or
    /// the thread's; another process's or thread's, a spelling through `..`,
    /// a relative one, or a name below the link do not.
    #[test]
    fn own_descriptor_links_name_the_callers_descriptor() {
        use DescriptorTable::Process;
        use DescriptorTable::Thread;
        let own = Some((3, 5));
        for (path, fd, table) in [
            ("/dev/fd/7", 7, Process),
            ("/proc/self/fd/7", 7, Process),
            ("/proc/thread-self/fd/7", 7, Thread),
            ("/proc/3/fd/0", 0, Process),
            ("/proc/3/task/5/fd/12", 12, Thread),
            ("/proc/self/task/5/fd/12", 12, Thread),
            ("/proc/self/fd/7/", 7, Process),
            ("/proc/./self/fd/7", 7, Process),
            ("//dev//fd//7", 7, Process),
            ("/proc/thread-self/fd/7/.", 7, Thread),
        ] {
            assert_eq!(
                own_descriptor_link(Path::new(path), own),
                Some(DescriptorLink { fd, table }),
                "{path}"
            );
        }
        for path in [
            "/proc/4/fd/7",
            "/proc/3/task/6/fd/7",
            "/proc/thread-self/task/5/fd/7",
            "/proc/self/fd/07",
            "/proc/self/fd/7/stat",
            "/proc/self/fd",
            "/proc/self/../self/fd/7",
            "/dev/fd/../fd/7",
            "proc/self/fd/7",
            "/proc/self/fdinfo/7",
            "/dev/fd/x",
        ] {
            assert_eq!(own_descriptor_link(Path::new(path), own), None, "{path}");
        }
        // Without the caller's procfs names, only the spellings without
        // numbers are its own.
        let fd = |fd, table| Some(DescriptorLink { fd, table });
        assert_eq!(
            own_descriptor_link(Path::new("/proc/self/fd/7"), None),
            fd(7, Process)
        );
        assert_eq!(
            own_descriptor_link(Path::new("/proc/thread-self/fd/7"), None),
            fd(7, Thread)
        );
        assert_eq!(
            own_descriptor_link(Path::new("/dev/fd/7"), None),
            fd(7, Process)
        );
        assert_eq!(own_descriptor_link(Path::new("/proc/3/fd/7"), None), None);
        assert_eq!(
            own_descriptor_link(Path::new("/proc/self/task/5/fd/7"), None),
            None
        );
    }

    /// The paths procfs and `/dev` resolve for whoever looks them up, and
    /// the ones they do not.
    #[test]
    fn caller_selectors_are_the_ones_procfs_resolves_per_caller() {
        for path in [
            "/proc/self",
            "/proc/self/",
            "/proc/self/task",
            "/proc/thread-self",
            "/proc/thread-self/fd",
            "/proc/mounts",
            "/proc/net",
            "/proc/net/tcp",
            "/dev/fd",
            "/dev/fd/3",
            "/dev/stdin",
            "/dev/stdout",
            "/dev/stderr",
            "//proc/./self",
            "/proc/sys/../self",
            "/tmp/../proc/stat",
            "relative/dir",
        ] {
            assert!(names_through_caller_selector(Path::new(path)), "{path}");
        }
        for path in [
            "/",
            "/proc",
            "/proc/",
            "/proc/1",
            "/proc/12/task/13",
            "/proc/stat",
            "/proc/sys/kernel",
            "/proc/selfish",
            "/dev",
            "/dev/null",
            "/tmp/self",
        ] {
            assert!(!names_through_caller_selector(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn rejects_non_proc_fd_targets() {
        for path in [
            "/proc/self/fd/",
            "/proc/self/fd/stdout",
            "/proc/self/fd/1/status",
            "/proc/not-a-pid/fd/1",
            "/dev/fd/-1",
            "proc/self/fd/1",
        ] {
            assert_eq!(proc_fd_target(Path::new(path)), None, "{path}");
        }
    }

    #[test]
    fn recognizes_only_anonymous_pipe_and_socket_targets() {
        assert_eq!(
            anonymous_proc_fd_identity(b"pipe:[987654321]"),
            Some(AnonymousProcFdIdentity {
                kind: "pipe",
                raw_inode: 987_654_321,
            })
        );
        assert_eq!(
            anonymous_proc_fd_identity(b"socket:[42]"),
            Some(AnonymousProcFdIdentity {
                kind: "socket",
                raw_inode: 42,
            })
        );

        for target in [
            b"pip".as_slice(),
            b"socket:[12345".as_slice(),
            b"/tmp/regular-file".as_slice(),
            b"anon_inode:[eventpoll]".as_slice(),
            b"pipe:[]".as_slice(),
            b"pipe:[12]suffix".as_slice(),
            b"socket:[not-a-number]".as_slice(),
        ] {
            assert_eq!(anonymous_proc_fd_identity(target), None, "{target:?}");
        }
    }

    #[test]
    fn short_buffers_use_scratch_large_enough_for_every_anonymous_target() {
        assert!(!needs_anonymous_proc_fd_scratch(false, 1));
        assert!(!needs_anonymous_proc_fd_scratch(true, 0));
        assert!(needs_anonymous_proc_fd_scratch(true, 1));
        assert!(needs_anonymous_proc_fd_scratch(
            true,
            ANONYMOUS_PROC_FD_TARGET_CAPACITY - 1
        ));
        assert!(!needs_anonymous_proc_fd_scratch(
            true,
            ANONYMOUS_PROC_FD_TARGET_CAPACITY
        ));

        let maximum = format!("socket:[{}]", u64::MAX);
        assert!(maximum.len() < ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        // The longest link a placeholder can have: the longest memfd name
        // (249 bytes) in `/memfd:<name> (deleted)`.
        let longest = format!("/memfd:{} (deleted)", "x".repeat(249));
        assert!(longest.len() < ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        // Each rendered file's placeholder, with the snapshot it carries,
        // fits a memfd name and is read back from its link in a session that
        // renders it. A file whose snapshots are all the same carries none
        // (`ProcfsFile::snapshot_is_rerendered`).
        let all = crate::procfs::RenderedFiles::ALL;
        for path in crate::syscalls::rendered_procfs::rendered_paths(all) {
            let path = std::path::Path::new(path.to_str().unwrap());
            let rerendered = crate::procfs::ProcfsFile::from_path(path)
                .unwrap()
                .snapshot_is_rerendered();
            let snapshot = match rerendered {
                true => Vec::new(),
                false => b"00000000-0000-4000-8000-000000000000\n".to_vec(),
            };
            let name = crate::procfs::placeholder_name(path, &snapshot);
            assert!(name.len() <= 249, "{name}");
            let placeholder = format!("/memfd:{name} (deleted)");
            assert!(crate::procfs::placeholder_link_path(placeholder.as_bytes(), all).is_some());
        }
        assert_eq!(
            anonymous_proc_fd_identity(maximum.as_bytes()),
            Some(AnonymousProcFdIdentity {
                kind: "socket",
                raw_inode: u64::MAX,
            })
        );
    }

    #[test]
    fn stdio_identity_requires_a_raw_inode_match_and_preserves_alias_precedence() {
        let raw = |ino| RawInode::new(9, ino);
        let stdio = [Some(raw(11)), Some(raw(22)), Some(raw(33))];
        assert_eq!(
            deterministic_stdio_inode_for_raw(raw(22), &stdio),
            Some(DetInode::mint(1001))
        );
        assert_eq!(deterministic_stdio_inode_for_raw(raw(44), &stdio), None);
        // The same number on another device is another file.
        assert_eq!(
            deterministic_stdio_inode_for_raw(RawInode::new(10, 22), &stdio),
            None
        );

        let aliased = [None, Some(raw(55)), Some(raw(55))];
        assert_eq!(
            deterministic_stdio_inode_for_raw(raw(55), &aliased),
            Some(DetInode::mint(1001))
        );
        // What `setup_stdio` caches: stdin's identity on all three.
        let inherited = [Some(raw(66)); 3];
        assert_eq!(
            deterministic_stdio_inode_for_raw(raw(66), &inherited),
            Some(DetInode::mint(1000))
        );
    }

    #[test]
    fn anonymous_target_rewrite_ignores_raw_inode_width_and_truncates_to_buffer() {
        let short_raw = anonymous_proc_fd_identity(b"pipe:[42]").unwrap();
        let long_raw = anonymous_proc_fd_identity(b"pipe:[987654321]").unwrap();
        let short_rewrite =
            canonical_anonymous_proc_fd_target(&short_raw, DetInode::mint(1001), usize::MAX);
        let long_rewrite =
            canonical_anonymous_proc_fd_target(&long_raw, DetInode::mint(1001), usize::MAX);
        assert_eq!(short_rewrite, b"pipe:[1001]");
        assert_eq!(long_rewrite, short_rewrite);

        assert_eq!(
            canonical_anonymous_proc_fd_target(&long_raw, DetInode::mint(1001), 8),
            b"pipe:[10"
        );
    }
}
