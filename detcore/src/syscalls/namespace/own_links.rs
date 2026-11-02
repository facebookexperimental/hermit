/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The caller's own `exe`, `cwd` and `root` links (`/proc/self/exe`,
//! `/proc/thread-self/cwd`, `/proc/<own pid>/root`, ...), answered by Detcore
//! from its own knowledge wherever that answer is exactly Linux's (package K
//! slice E1): `cwd` from an injected `getcwd` of the calling thread and
//! `root` as `/` on every backend, and `exe`, only where the backend's own
//! procfs is not the guest's kernel procfs (KVM), from the exec record
//! Detcore keeps for the process ([`crate::exec_record`]) while it still
//! names exactly the image the process executes. No backend's `/proc` is
//! read for them, so on KVM the executor's readlink answers for these links
//! are reached only where Detcore cannot render one exactly.
//!
//! Linux (v6.17) answers each from the task the spelling names
//! (`proc_exe_link`, `proc_cwd_link`, `proc_root_link`, fs/proc/base.c), as
//! `d_path` renders the path under the reader's root
//! (`do_proc_readlink`, fs/proc/base.c; `d_path`, fs/d_path.c): the image's
//! path with " (deleted)" once it is unlinked, the working directory's path,
//! and `/` for the caller's own root. A task without an address space or
//! filesystem information (an exited thread-group leader) has no link
//! (`ENOENT`). `readlink` copies at most its buffer's size, without a NUL.
//!
//! Elsewhere the call takes the path it took before this slice, so that every
//! backend answers as it did: the kernel on ptrace, LiteInst and
//! in-guest-trap, the launcher runs' own `exe` handling on DBT and SaBRe
//! (`Detcore::launched_exe`), and the executor's existing readlink on KVM.
//! That is: `exe` on a backend whose procfs is the guest's kernel procfs (the
//! kernel names the image's dentry exactly, which no record can prove), and
//! on KVM an image without a record or no longer exactly at its recorded
//! path; a removed working directory, which `getcwd` refuses with `ENOENT`;
//! a leader or another thread whose filesystem information the caller does
//! not share; and a spelling that looks a name up past the link (a `.` after
//! it), where Linux checks search permission.
//!
//! The backend split keys on
//! [`crate::procfs_inode::tool_can_name_guest_descriptors`], which stands in
//! for two filesystem facts no provider reports yet (the backend's procfs is
//! the guest's kernel procfs; the tool's filesystem view is the guest's):
//! <https://github.com/rrnewton/hermit/issues/4700>.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sysno;

use crate::record_or_replay::RecordOrReplay;
use crate::tool_local::Detcore;

/// The longest path `d_path` renders into `do_proc_readlink`'s buffer, and
/// the longest `getcwd` returns, with its NUL (`PATH_MAX`).
const PATH_MAX: usize = libc::PATH_MAX as usize;

/// Which of a task's three path links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnLink {
    /// `exe`: the image the task's address space executes.
    Exe,
    /// `cwd`: the task's working directory.
    Cwd,
    /// `root`: the task's root directory.
    Root,
}

impl OwnLink {
    fn name(self) -> &'static str {
        match self {
            OwnLink::Exe => "exe",
            OwnLink::Cwd => "cwd",
            OwnLink::Root => "root",
        }
    }
}

/// The task a spelling of an own link names, before Detcore checks the
/// numbers it does not know to be the caller's against its process table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NamedTask {
    /// The caller itself: `thread-self`, or its own tid below its process.
    Caller,
    /// The caller's thread-group leader: `self` or the caller's pid.
    Leader,
    /// `/proc/<self or own pid>/task/<t>` for another thread `t`: one of
    /// the caller's process if Detcore lists it so, else no such entry.
    GroupThread(i32),
    /// `/proc/<n>` for a number `n` other than the caller's pid: a thread of
    /// the caller's process if Detcore lists it so, else another process.
    Numbered(i32),
    /// `/proc/<n>/task/<t>` for a number `n` other than the caller's pid.
    NumberedThread(i32, i32),
}

/// How a spelling of an own link goes on past the link's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Past {
    /// It ends at the link: the link is read.
    Nothing,
    /// Only slashes follow: the link is followed, and what it leads to is
    /// read, with no further lookup.
    Slash,
    /// A `.` follows: a lookup in what the link leads to, which Linux
    /// checks for search permission there.
    Walk,
}

/// A spelling of one of the caller's own path links ([`own_task_link`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OwnTaskLink {
    pub(crate) link: OwnLink,
    pub(crate) task: NamedTask,
    /// What follows the link's name.
    pub(crate) past: Past,
}

/// Which own path link the absolute spelling `path` names, for a caller whose
/// pid and tid as the guest numbers them are `pid` and `tid`:
/// `/proc/<p>/{exe,cwd,root}` and `/proc/<p>/task/<t>/{exe,cwd,root}`, where
/// `p` is `self` or a number and `t` a number, and
/// `/proc/thread-self/{exe,cwd,root}`. Empty and `.` components are skipped,
/// as Linux's walk skips them (`//proc/./self/exe` too). `None` for any
/// other path and for a spelling with `..`. Numbers follow procfs's names:
/// decimal, no leading zero.
pub(crate) fn own_task_link(path: &[u8], pid: i32, tid: i32) -> Option<OwnTaskLink> {
    let rest = path.strip_prefix(b"/")?;
    let mut parts = Vec::new();
    let mut past = Past::Nothing;
    for part in rest.split(|byte| *byte == b'/') {
        match part {
            b"" if !parts.is_empty() && past == Past::Nothing => past = Past::Slash,
            b"" => {}
            b"." if !parts.is_empty() => past = Past::Walk,
            b"." => {}
            b".." => return None,
            part => {
                parts.push(part);
                past = Past::Nothing;
            }
        }
    }
    let (first, parts) = parts.split_first()?;
    if *first != b"proc" {
        return None;
    }
    let number = |part: &[u8]| -> Option<i32> {
        let digits = std::str::from_utf8(part).ok()?;
        (!digits.is_empty()
            && !digits.starts_with('0')
            && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| digits.parse().ok())?
    };
    let link = |part: &[u8]| match part {
        b"exe" => Some(OwnLink::Exe),
        b"cwd" => Some(OwnLink::Cwd),
        b"root" => Some(OwnLink::Root),
        _ => None,
    };
    // `Some(None)`: the caller's own process directory; `Some(Some(n))`: the
    // directory of another number.
    let process = |part: &[u8]| -> Option<Option<i32>> {
        if part == b"self" {
            return Some(None);
        }
        let number = number(part)?;
        Some((number != pid).then_some(number))
    };
    let thread = |own: Option<i32>, thread: i32| match own {
        None if thread == tid => NamedTask::Caller,
        None if thread == pid => NamedTask::Leader,
        None => NamedTask::GroupThread(thread),
        Some(process) => NamedTask::NumberedThread(process, thread),
    };
    let (task, name) = match parts {
        [b"thread-self", name] => (NamedTask::Caller, name),
        [subject, name] => match process(subject)? {
            None => (NamedTask::Leader, name),
            Some(other) => (NamedTask::Numbered(other), name),
        },
        [subject, b"task", named, name] => (thread(process(subject)?, number(named)?), name),
        _ => return None,
    };
    Some(OwnTaskLink {
        link: link(name)?,
        task,
        past,
    })
}

/// The task an own link describes once Detcore checked its numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DescribedTask {
    /// The calling thread.
    Caller,
    /// The caller's live thread-group leader, another thread.
    Leader,
    /// Another live thread of the caller's process.
    Other,
}

/// What Detcore makes of an own link's spelling ([`Detcore::own_link_task`]).
enum Resolved {
    /// A task of the caller's process with this link.
    Described(DescribedTask),
    /// Linux finds no such link (`ENOENT`): a thread outside the process
    /// below its `task` directory, or a task without an address space or
    /// filesystem information (an exited leader).
    Missing,
    /// Not the caller's process: another process's link, as before.
    Elsewhere,
}

/// The target `readlink` of the working-directory link shows for the path
/// `getcwd` answered, or `None` where it cannot be rendered from it: a path
/// that does not start with `/` (`getcwd`'s "(unreachable)" prefix for a
/// directory outside the caller's root, where `d_path` shows the path from
/// the filesystem's root instead).
pub(crate) fn cwd_link_target(getcwd: &[u8]) -> Option<&[u8]> {
    getcwd.starts_with(b"/").then_some(getcwd)
}

/// What `d_path` makes of a rendered `target` in `do_proc_readlink`'s
/// `PATH_MAX` buffer: the target, or `ENAMETOOLONG` where it and its NUL do
/// not fit.
pub(crate) fn fits_d_path(target: &[u8]) -> Result<&[u8], Errno> {
    if target.len() >= PATH_MAX {
        Err(Errno::ENAMETOOLONG)
    } else {
        Ok(target)
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Answer `readlink` of one of the caller's own `exe`, `cwd` and `root`
    /// links at the guest's absolute spelling `path` ([`own_task_link`]) from
    /// Detcore's own knowledge, into the guest's `buffer` of `buffer_len`
    /// bytes. `Ok(None)` leaves the call to the path it took before: for
    /// any other path, under record and replay, in a nested PID namespace or
    /// after a guest `chroot` (where `/proc`, the caller's numbers or its root
    /// are not Detcore's), and where Detcore cannot render the link exactly
    /// (the module's documentation).
    pub(crate) async fn readlink_own_task_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        path: &Path,
        (buffer, buffer_len): (Option<AddrMut<'_, libc::c_char>>, usize),
    ) -> Result<Option<i64>, Error> {
        let (pid, tid) = {
            let state = guest.thread_state();
            if self.cfg.recordreplay_modes
                || state.nested_pid_namespace
                || state.changed_root.load(std::sync::atomic::Ordering::SeqCst)
                || path.as_os_str().len() >= PATH_MAX
            {
                return Ok(None);
            }
            let Some(detpid) = state.detpid else {
                return Ok(None);
            };
            (detpid.as_raw(), state.dettid.as_raw())
        };
        let Some(spelled) = own_task_link(path.as_os_str().as_bytes(), pid, tid) else {
            return Ok(None);
        };
        // Where the backend's procfs is the guest's kernel procfs, the
        // kernel names the image exactly, as Detcore's record cannot prove
        // it would (the image's dentry may have moved while its inode stayed
        // at the path): its answer stands, as before. A lookup past the link
        // (a `.` after it) is left to the backend too, which checks search
        // permission where Detcore would not.
        if (spelled.link == OwnLink::Exe
            && crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend))
            || spelled.past == Past::Walk
        {
            return Ok(None);
        }
        // Linux refuses a size that is not positive before it looks the path up.
        let size = super::readlink_size(buffer_len)?;
        let described = match self.own_link_task(guest, spelled.task, (pid, tid)).await {
            Resolved::Described(described) => described,
            Resolved::Missing => return Err(Errno::ENOENT.into()),
            Resolved::Elsewhere => return Ok(None),
        };
        if spelled.past == Past::Slash {
            // The link followed, with no lookup past it: the image (a file)
            // or a directory, which is no link.
            return Err(match spelled.link {
                OwnLink::Exe => Errno::ENOTDIR,
                OwnLink::Cwd | OwnLink::Root => Errno::EINVAL,
            }
            .into());
        }
        let target = match spelled.link {
            OwnLink::Exe => self.own_exe_target(guest),
            OwnLink::Cwd | OwnLink::Root if !self.shares_filesystem(guest, described) => None,
            OwnLink::Cwd => match self.injected_working_directory(guest, buffer, size).await {
                Ok(directory) => cwd_link_target(&directory).map(<[u8]>::to_vec),
                // A removed working directory (`getcwd` refuses it, and only
                // its name, which Detcore does not know, would render it),
                // and any other failure: answered as before.
                Err(_) => None,
            },
            OwnLink::Root => Some(b"/".to_vec()),
        };
        // Where Detcore cannot render the link exactly, the call takes its
        // previous path (the module's documentation).
        let Some(target) = target else {
            tracing::debug!(
                "[detcore, tid {}] {sysno} of the caller's own {} link answered as before",
                guest.tid(),
                spelled.link.name()
            );
            return Ok(None);
        };
        let target = fits_d_path(&target)?;
        let copied = &target[..target.len().min(size)];
        let address = buffer.ok_or(Errno::EFAULT)?;
        copy_link_out(guest, address.cast::<u8>(), copied)
    }

    /// Which task of the caller's process, if any, the spelling's `task`
    /// names, from Detcore's process table: a number Detcore does not know
    /// to be the caller's is one of its threads only where the table lists
    /// it in the caller's process, and the leader, where it is not the
    /// caller, still has its address space only until it exits.
    async fn own_link_task<G: Guest<Self>>(
        &self,
        guest: &mut G,
        task: NamedTask,
        (pid, tid): (i32, i32),
    ) -> Resolved {
        let thread_of = |thread: i32| {
            if thread == tid {
                DescribedTask::Caller
            } else if thread == pid {
                DescribedTask::Leader
            } else {
                DescribedTask::Other
            }
        };
        let described = match task {
            NamedTask::Caller => DescribedTask::Caller,
            NamedTask::Leader => thread_of(pid),
            NamedTask::GroupThread(thread) => {
                if !thread_in_process(guest, pid, thread).await {
                    return Resolved::Missing;
                }
                thread_of(thread)
            }
            NamedTask::Numbered(number) => {
                if !thread_in_process(guest, pid, number).await {
                    return Resolved::Elsewhere;
                }
                thread_of(number)
            }
            NamedTask::NumberedThread(number, thread) => {
                if !thread_in_process(guest, pid, number).await {
                    return Resolved::Elsewhere;
                }
                if thread != tid && thread != pid && !thread_in_process(guest, pid, thread).await {
                    return Resolved::Missing;
                }
                thread_of(thread)
            }
        };
        if described == DescribedTask::Leader {
            // An exited leader stays in the table as a zombie while its
            // threads run, without an address space or filesystem
            // information: Linux's link lookup finds neither.
            let answer = crate::tool_global::procfs_task(
                guest,
                crate::procfs::BoundTask {
                    process: pid,
                    thread: None,
                },
            )
            .await;
            if !answer.exists || answer.without_mm {
                return Resolved::Missing;
            }
        }
        Resolved::Described(described)
    }

    /// Whether the caller's working and root directories are those of the
    /// `described` task: its own, or its live leader's where the caller
    /// shares the leader's filesystem information (`CLONE_FS`,
    /// `ThreadState::fs_apart_from_leader`). Another thread's are its own
    /// to know.
    fn shares_filesystem<G: Guest<Self>>(&self, guest: &G, described: DescribedTask) -> bool {
        match described {
            DescribedTask::Caller => true,
            DescribedTask::Leader => !guest.thread_state().fs_apart_from_leader,
            DescribedTask::Other => false,
        }
    }

    /// The target of the caller's process's `exe` link, from its exec record:
    /// the image's path, where that path still names exactly the file the
    /// process executes ([`crate::exec_record::ExecRecord::names_path_exactly`]:
    /// the file untouched at its path, with the link count and change time it
    /// had at the exec, below the same directories), in the caller's view
    /// ([`Self::exec_record_root`]). Whatever Linux's link would name
    /// otherwise (a new path, " (deleted)"), it is not this answer: `None`,
    /// as without a record, or where Detcore cannot see the caller's
    /// filesystem.
    fn own_exe_target<G: Guest<Self>>(&self, guest: &G) -> Option<Vec<u8>> {
        let exe = guest.thread_state().exe.clone()?;
        let root = self.exec_record_root(guest)?;
        exe.names_path_exactly(&root)
            .then(|| exe.exe.as_os_str().as_bytes().to_vec())
    }

    /// The caller's root directory as the tool sees the guest's files, for an
    /// exec record's paths: the root its procfs directory names where the
    /// tool can read that ([`Self::guest_root`]), and otherwise, where guest
    /// threads are no host tasks (KVM, whose executor opens every absolute
    /// guest path in the tool's own filesystem view), `/`. `None` after a
    /// guest `chroot`.
    pub(crate) fn exec_record_root<G: Guest<Self>>(&self, guest: &G) -> Option<PathBuf> {
        if guest
            .thread_state()
            .changed_root
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return None;
        }
        if crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend) {
            self.guest_root(guest)
        } else {
            Some(PathBuf::from("/"))
        }
    }

    /// The calling thread's working directory as its own `getcwd` answers
    /// it, without the NUL: the call's buffer is the guest's readlink
    /// `buffer` (`size` bytes) where it holds the answer, the guest may write
    /// it, and no other guest thread can run meanwhile (sequentialized
    /// threads: the call holds the turn), and is given back its bytes
    /// afterwards; otherwise a private
    /// page mapped for the call alone
    /// ([`crate::syscalls::files::procfs_levels::map_transient`]). Nothing is
    /// written below the guest's stack pointer.
    pub(crate) async fn injected_working_directory<G: Guest<Self>>(
        &self,
        guest: &mut G,
        buffer: Option<AddrMut<'_, libc::c_char>>,
        size: usize,
    ) -> Result<Vec<u8>, Errno> {
        let buffer = buffer.filter(|_| self.cfg.sequentialize_threads);
        let saved = super::saved_link_prefix(guest, buffer, size.min(PATH_MAX));
        // The buffer takes getcwd's write only where the guest itself could
        // write it: its bytes written back over themselves with its own
        // permissions first (a backend's getcwd need not check them).
        if let (Some(address), Some(saved)) = (buffer, saved.as_deref())
            && saved.len() >= 2
            && matches!(
                copy_link_out(guest, address.cast::<u8>(), saved),
                Ok(Some(_))
            )
        {
            match getcwd_at(guest, address.cast::<u8>(), saved.len()).await {
                Ok(directory) => {
                    // Linux's readlink writes only the link: put back the
                    // bytes getcwd wrote, the path and its NUL, with the
                    // buffer's own permissions (it took getcwd's write).
                    let written = &saved[..(directory.len() + 1).min(saved.len())];
                    return match copy_link_out(guest, address.cast::<u8>(), written) {
                        Ok(Some(_)) => Ok(directory),
                        Ok(None) => guest
                            .memory()
                            .write_exact(address.cast::<u8>(), written)
                            .map(|()| directory)
                            .map_err(|_| Errno::EFAULT),
                        Err(_) => Err(Errno::EFAULT),
                    };
                }
                // Nothing written (ERANGE), or a fault the private page's
                // answer meets again at the copy (EFAULT): no restore.
                Err(Errno::ERANGE | Errno::EFAULT) => {}
                Err(errno) => return Err(errno),
            }
        }
        let Some(page) =
            crate::syscalls::files::procfs_levels::map_transient(guest, PATH_MAX).await
        else {
            return Err(Errno::ENOMEM);
        };
        let answered = getcwd_at(guest, page, PATH_MAX).await;
        crate::syscalls::files::procfs_levels::unmap_transient(guest, page, PATH_MAX).await;
        answered
    }
}

/// Copy a rendered link, `target` (already cut to the call's size), to the
/// guest's readlink buffer at `address` as Linux's `copy_to_user` does, with
/// the destination's own permissions: the bytes in order, a write-protected
/// or unmapped byte ending the copy, whose prefix stays written, and then
/// `EFAULT`; otherwise the number of bytes copied. `Ok(None)` where the
/// backend cannot write with the guest's permissions (`ENOSYS`, before
/// anything is copied): the call then takes its previous path.
fn copy_link_out<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &mut G,
    address: AddrMut<'_, u8>,
    target: &[u8],
) -> Result<Option<i64>, Error> {
    let mut copied = 0;
    while copied < target.len() {
        // SAFETY: `copied < target.len()`, so the address stays inside the
        // guest's buffer; nothing is dereferenced here.
        let at = unsafe { address.add(copied) };
        match guest.memory().write_with_user_access(at, &target[copied..]) {
            Ok(0) | Err(Errno::EFAULT) => return Err(Errno::EFAULT.into()),
            Ok(written) => copied += written,
            Err(Errno::ENOSYS) if copied == 0 => return Ok(None),
            Err(errno) => return Err(errno.into()),
        }
    }
    Ok(Some(copied as i64))
}

/// Whether Detcore's process table lists the live task `thread` as a thread
/// of process `pid`.
async fn thread_in_process<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &mut G,
    pid: i32,
    thread: i32,
) -> bool {
    let answer = crate::tool_global::procfs_task(
        guest,
        crate::procfs::BoundTask {
            process: pid,
            thread: Some(thread),
        },
    )
    .await;
    answer.exists && !answer.without_mm
}

/// Inject `getcwd(address, size)` and read back the path it wrote, without
/// its NUL.
async fn getcwd_at<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &mut G,
    address: AddrMut<'_, u8>,
    size: usize,
) -> Result<Vec<u8>, Errno> {
    let length = guest
        .inject_with_retry(
            syscalls::Getcwd::new()
                .with_buf(Some(address.cast::<libc::c_char>()))
                .with_size(size),
        )
        .await?;
    // The length counts the NUL.
    let length = usize::try_from(length)
        .ok()
        .filter(|length| (1..=size).contains(length))
        .ok_or(Errno::EFAULT)?;
    let mut directory = vec![0; length - 1];
    guest
        .memory()
        .read_exact(address, &mut directory)
        .map_err(|_| Errno::EFAULT)?;
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_own_links_of_every_spelling() {
        let named = |path: &str| own_task_link(path.as_bytes(), 3, 5);
        let link = |link, task| {
            Some(OwnTaskLink {
                link,
                task,
                past: Past::Nothing,
            })
        };
        assert_eq!(
            named("/proc/self/exe"),
            link(OwnLink::Exe, NamedTask::Leader)
        );
        assert_eq!(named("/proc/3/cwd"), link(OwnLink::Cwd, NamedTask::Leader));
        assert_eq!(
            named("/proc/thread-self/root"),
            link(OwnLink::Root, NamedTask::Caller)
        );
        assert_eq!(
            named("/proc/self/task/5/cwd"),
            link(OwnLink::Cwd, NamedTask::Caller)
        );
        assert_eq!(
            named("/proc/3/task/5/exe"),
            link(OwnLink::Exe, NamedTask::Caller)
        );
        assert_eq!(
            named("/proc/3/task/3/root"),
            link(OwnLink::Root, NamedTask::Leader)
        );
        assert_eq!(
            named("/proc/self/task/9/exe"),
            link(OwnLink::Exe, NamedTask::GroupThread(9))
        );
        assert_eq!(
            named("/proc/7/exe"),
            link(OwnLink::Exe, NamedTask::Numbered(7))
        );
        assert_eq!(
            named("/proc/7/task/9/cwd"),
            link(OwnLink::Cwd, NamedTask::NumberedThread(7, 9))
        );
        // Linux's walk skips empty and `.` components.
        assert_eq!(named("/proc//self/./exe"), named("/proc/self/exe"));
        assert_eq!(named("//proc/./self/exe"), named("/proc/self/exe"));
        assert_eq!(
            named("/./proc/thread-self/cwd"),
            named("/proc/thread-self/cwd")
        );
        assert_eq!(
            named("/proc/self//task/./5/cwd"),
            named("/proc/3/task/5/cwd")
        );
        // A trailing slash follows the link; a `.` after it looks a name up
        // in what it leads to.
        for path in ["/proc/self/exe/", "/proc/thread-self/root//"] {
            assert_eq!(named(path).unwrap().past, Past::Slash, "{path}");
        }
        for path in ["/proc/self/cwd/.", "/proc/self/cwd/./", "/proc/self/exe//."] {
            assert_eq!(named(path).unwrap().past, Past::Walk, "{path}");
        }
        for path in [
            "/proc/self/../self/exe",
            "/proc/03/exe",
            "/proc/self/task/05/exe",
            "/proc/self/task/x/cwd",
            "/proc/thread-self/task/5/exe",
            "/proc/self/fd/3",
            "/proc/self/exe/x",
            "/proc/self/cwdx",
            "/proc/exe",
            "/proc/self",
            "/proc",
            "/",
            "/proc/../proc/self/exe",
            "/procx/self/exe",
            "proc/self/exe",
            "/tmp/self/exe",
            "/proc/self/task/5",
        ] {
            assert_eq!(named(path), None, "{path}");
        }
    }

    #[test]
    fn renders_the_working_directory_getcwd_gives() {
        assert_eq!(cwd_link_target(b"/a/b"), Some(b"/a/b".as_slice()));
        assert_eq!(cwd_link_target(b"/"), Some(b"/".as_slice()));
        // getcwd's prefix for a directory outside the caller's root, where
        // d_path shows the path from the filesystem's root instead.
        assert_eq!(cwd_link_target(b"(unreachable)/a"), None);
        assert_eq!(cwd_link_target(b""), None);
    }

    #[test]
    fn a_target_d_path_cannot_hold_is_too_long() {
        let longest = vec![b'a'; PATH_MAX - 1];
        assert_eq!(fits_d_path(&longest), Ok(longest.as_slice()));
        assert_eq!(fits_d_path(&[b'a'; PATH_MAX]), Err(Errno::ENAMETOOLONG));
    }
}
