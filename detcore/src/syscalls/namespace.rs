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
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())?
}

/// Returns the optional numeric proc subject and the descriptor number.
fn proc_fd_target(path: &Path) -> Option<(Option<u32>, i32)> {
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
    raw_inode: RawInode,
}

// Long enough for `socket:[` + every decimal u64 inode + `]`.
const ANONYMOUS_PROC_FD_TARGET_CAPACITY: usize = 32;

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
/// Iterating in fd order deliberately matches the last-insert-wins behavior of
/// the maps sanitizer's `stdio_by_raw_inode` table when stdio descriptors alias.
fn deterministic_stdio_inode_for_raw(
    raw_inode: RawInode,
    stdio_raw_inodes: &[Option<RawInode>; 3],
) -> Option<DetInode> {
    let mut matched = None;
    for (fd, cached) in stdio_raw_inodes.iter().enumerate() {
        if *cached == Some(raw_inode) {
            matched = deterministic_stdio_inode(fd as i32);
        }
    }
    matched
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
        raw_target: &[u8],
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
    ) -> Result<Option<i64>, Error>
    where
        G: Guest<Self>,
    {
        let Some(identity) = anonymous_proc_fd_identity(raw_target) else {
            return Ok(None);
        };
        let mut stdio_raw_inodes = [None; 3];
        for fd in libc::STDIN_FILENO..=libc::STDERR_FILENO {
            stdio_raw_inodes[fd as usize] = guest
                .thread_state()
                .with_detfd(fd, |detfd| {
                    deterministic_stdio_inode_for_resource(fd, detfd.resource())?;
                    detfd.stat().map(|stat| stat.inode)
                })
                .ok()
                .flatten();
        }
        let inode = match deterministic_stdio_inode_for_raw(identity.raw_inode, &stdio_raw_inodes) {
            Some(inode) => inode,
            None => determinize_inode(guest, identity.raw_inode).await.0,
        };
        let target = canonical_anonymous_proc_fd_target(&identity, inode, buffer_len);
        let buffer = buffer.ok_or(Errno::EFAULT)?;
        guest.memory().write_exact(buffer.cast(), &target)?;
        Ok(Some(target.len() as i64))
    }

    /// Canonicalize a pipe/socket link belonging to another virtual process.
    /// The target descriptor is not in the caller's table, so its raw readlink
    /// bytes are the only safe identity evidence available here.
    async fn canonicalize_other_proc_fd_readlink<G>(
        &self,
        guest: &mut G,
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
        result: i64,
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
            .canonicalize_other_proc_fd_target(guest, &observed, Some(buffer), buffer_len)
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
                        .canonicalize_other_proc_fd_readlink(guest, buffer, buffer_len, result)
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
                None => determinize_inode(guest, stat.st_ino).await.0,
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
        raw_target: &[u8],
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        if let Some(result) = self
            .canonicalize_other_proc_fd_target(guest, raw_target, buffer, buffer_len)
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
        let buffer = call.buf();
        let buffer_len = call.bufsize();
        if !needs_anonymous_proc_fd_scratch(buffer.is_some(), buffer_len) {
            let result = self.record_or_replay(guest, call).await?;
            if result <= 0 {
                return Ok(result);
            }
            return self
                .canonicalize_other_proc_fd_readlink(guest, buffer, buffer_len, result)
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
        self.write_other_proc_fd_target(guest, &raw_target, buffer, buffer_len)
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
        let buffer = call.buf();
        let buffer_len = call.buf_len();
        if !needs_anonymous_proc_fd_scratch(buffer.is_some(), buffer_len) {
            let result = self.record_or_replay(guest, call).await?;
            if result <= 0 {
                return Ok(result);
            }
            return self
                .canonicalize_other_proc_fd_readlink(guest, buffer, buffer_len, result)
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
        self.write_other_proc_fd_target(guest, &raw_target, buffer, buffer_len)
            .await
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#877)
    async fn finish_namespace_readlink<G, S>(
        &self,
        guest: &mut G,
        path: PathBuf,
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
        syscall: S,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
        S: Into<Syscall>,
    {
        let result = self.record_or_replay(guest, syscall).await?;
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
        self.finish_namespace_readlink(guest, path, call.buf(), call.bufsize(), call)
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
        if let Some((Some(subject), _)) = proc_fd_target(&observed_path) {
            let current_pid = guest.inject(syscalls::Getpid::new()).await?;
            if current_pid != i64::from(subject) {
                return self.finish_other_proc_fd_readlinkat(guest, call).await;
            }
        }
        self.finish_namespace_readlink(guest, observed_path, call.buf(), call.buf_len(), call)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(needs_anonymous_proc_fd_scratch(true, 31));
        assert!(!needs_anonymous_proc_fd_scratch(true, 32));

        let maximum = format!("socket:[{}]", RawInode::MAX);
        assert!(maximum.len() < ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        assert_eq!(
            anonymous_proc_fd_identity(maximum.as_bytes()),
            Some(AnonymousProcFdIdentity {
                kind: "socket",
                raw_inode: RawInode::MAX,
            })
        );
    }

    #[test]
    fn stdio_identity_requires_a_raw_inode_match_and_preserves_alias_precedence() {
        let stdio = [Some(11), Some(22), Some(33)];
        assert_eq!(
            deterministic_stdio_inode_for_raw(22, &stdio),
            Some(DetInode::mint(1001))
        );
        assert_eq!(deterministic_stdio_inode_for_raw(44, &stdio), None);

        let aliased = [None, Some(55), Some(55)];
        assert_eq!(
            deterministic_stdio_inode_for_raw(55, &aliased),
            Some(DetInode::mint(1002))
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
