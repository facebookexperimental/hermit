/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic views of kernel namespace metadata.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

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
    let buffer = buffer?;
    // Also what a pipe or socket link's canonicalization restores, so it is
    // read for every link, not only a placeholder's.
    let mut saved = vec![0; buffer_len.min(ANONYMOUS_PROC_FD_TARGET_CAPACITY)];
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
        if let Some(path) = crate::procfs::placeholder_link_path(raw_target)
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
                let current_pid = guest.inject(syscalls::Getpid::new()).await?;
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
                crate::procfs::placeholder_link_path(&target)
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
                        restore_link_tail(guest, address, written, length, saved.as_deref())?;
                        written as i64
                    }
                    _ => result,
                }
            }
            _ => {
                self.rewrite_placeholder_link(
                    guest,
                    link,
                    buffer,
                    buffer_len,
                    result,
                    saved.as_deref(),
                )
                .await?
            }
        };
        self.canonicalize_namespace_readlink_result(guest, path, buffer, buffer_len, result)
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
        let path: PathBuf = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        if let Some(result) = self
            .readlink_rendered_procfs(guest, &path, call.buf(), call.bufsize())
            .await?
        {
            return Ok(result);
        }
        let (host_path, other_proc_fd) = if let Some((Some(subject), _)) = proc_fd_target(&path) {
            let current_pid = guest.inject(syscalls::Getpid::new()).await?;
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
        let path: PathBuf = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        let observed_path = if path.is_absolute() || call.dirfd() == libc::AT_FDCWD {
            path
        } else {
            guest
                .thread_state()
                .with_detfd(call.dirfd(), |detfd| detfd.path())?
                .map_or(path.clone(), |directory| directory.join(path))
        };
        if let Some(result) = self
            .readlink_rendered_procfs(guest, &observed_path, call.buf(), call.buf_len())
            .await?
        {
            return Ok(result);
        }
        if let Some((Some(subject), _)) = proc_fd_target(&observed_path) {
            let current_pid = guest.inject(syscalls::Getpid::new()).await?;
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
        // Each rendered file's placeholder, with its snapshot, fits a memfd
        // name and is read back from its link.
        let machine = crate::machine::MachineDescription::default();
        for path in crate::syscalls::rendered_procfs::rendered_paths() {
            let path = std::path::Path::new(path.to_str().unwrap());
            let snapshot = match crate::procfs::FixedProcfsFile::from_path(path.to_str().unwrap()) {
                Some(file) => file.contents(&machine),
                None => b"00000000-0000-4000-8000-000000000000\n".to_vec(),
            };
            let name = crate::procfs::placeholder_name(path, &snapshot);
            assert!(name.len() <= 249, "{name}");
            let placeholder = format!("/memfd:{name} (deleted)");
            assert!(crate::procfs::placeholder_link_path(placeholder.as_bytes()).is_some());
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
