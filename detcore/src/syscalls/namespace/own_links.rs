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
//! it), where Linux checks search permission (on KVM, see below).
//!
//! The backend split keys on
//! [`crate::procfs_inode::tool_can_name_guest_descriptors`], which stands in
//! for two filesystem facts no provider reports yet (the backend's procfs is
//! the guest's kernel procfs; the tool's filesystem view is the guest's):
//! <https://github.com/rrnewton/hermit/issues/4700>.
//!
//! Where the backend's procfs is not the guest's kernel procfs (KVM), so that
//! its own readlink answer is not the kernel's, Detcore reads the caller's
//! own `fd/<n>`, `exe` and `cwd` links from the backend's `link_path` fact
//! (`reverie::Guest::link_path`, package K slice F2-h) before anything else:
//! the name the backend's own records give the descriptor's file, the
//! process's image and the thread's working directory, rendered as `d_path`
//! renders them (a path, " (deleted)" once it is unlinked, or an object's
//! name such as `pipe:[<inode>]` and `anon_inode:[eventfd]`). A pipe's and a
//! socket's inode number, and a placeholder's name, are then Detcore's, as
//! for every descriptor link. Where the backend does not provide the fact
//! for a link, Detcore answers without the backend's `/proc` (package K
//! slice F2-d0), which is not the guest's there:
//! - an object of the backend's own (the KVM executor's rendering of the
//!   process's `status` and `maps`) is the procfs file Detcore recorded at
//!   the descriptor's open (`ProcfsFile::link_name`), and is refused by name
//!   where it recorded none;
//! - another thread's working or root directory is refused by name: the fact
//!   names the calling thread's alone, and nothing Detcore keeps says whether
//!   the other thread shares the caller's;
//! - a `.` after a link is looked up from what the link leads to: from the
//!   descriptor (`readlinkat(fd, ".")`), the working directory (`.`) or the
//!   root (`/.`), each an injected lookup whose search permission the kernel
//!   beneath checks, and `ENOTDIR` past the image.
//!
//! Another process's links still take the sources above and then the
//! backend's readlink, as before.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;

use reverie::Errno;
use reverie::Error;
use reverie::FactError;
use reverie::Guest;
use reverie::PathLink;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
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

/// How the absolute spelling `path` of a link goes on past the link's name,
/// from its last bytes: only slashes ([`Past::Slash`]), a `.` component among
/// them ([`Past::Walk`]), or nothing ([`Past::Nothing`]).
pub(crate) fn past_the_link(path: &[u8]) -> Past {
    let mut rest = path;
    let mut past = Past::Nothing;
    loop {
        if let Some(before) = rest.strip_suffix(b"/") {
            rest = before;
            if past == Past::Nothing {
                past = Past::Slash;
            }
        } else if let Some(before) = rest.strip_suffix(b"/.") {
            rest = before;
            past = Past::Walk;
        } else {
            return past;
        }
    }
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
        // (a `.` after it) is left to that kernel too, which checks search
        // permission where Detcore would not; where Detcore reads the links
        // from the backend's facts it makes that lookup itself
        // ([`Self::lookup_past_task_link`]).
        if (spelled.link == OwnLink::Exe
            && crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend))
            || (spelled.past == Past::Walk && !self.reads_link_facts())
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
        // Another thread's working or root directory, where Detcore reads the
        // links from the backend's facts: the fact names the calling thread's
        // alone, and nothing Detcore keeps says whether the other thread
        // shares the caller's (`CLONE_FS`). Linux gives that thread's
        // directory; refused by name rather than left to the backend, whose
        // own `/proc` is not the guest's.
        if matches!(spelled.link, OwnLink::Cwd | OwnLink::Root)
            && !self.shares_filesystem(guest, described)
            && self.reads_link_facts()
        {
            return self
                .refuse_unrendered_link(guest, sysno, &other_thread_directory_reason(path))
                .await
                .map(Some);
        }
        if spelled.past == Past::Walk {
            return self
                .lookup_past_task_link(guest, spelled.link, (buffer, size))
                .await
                .map(Some);
        }
        let target = match spelled.link {
            // The process's image is one for all its threads: the caller's
            // fact names it.
            OwnLink::Exe => match self.own_link_fact(guest, PathLink::Executable)? {
                Some(target) => Some(target),
                None => self.own_exe_target(guest),
            },
            OwnLink::Cwd | OwnLink::Root if !self.shares_filesystem(guest, described) => None,
            // The caller's working directory is the one the spelling names
            // (`shares_filesystem`).
            OwnLink::Cwd => match self.own_link_fact(guest, PathLink::WorkingDirectory)? {
                Some(target) => Some(target),
                None => match self.injected_working_directory(guest, buffer, size).await {
                    Ok(directory) => cwd_link_target(&directory).map(<[u8]>::to_vec),
                    // A removed working directory (`getcwd` refuses it, and
                    // only its name, which Detcore does not know, would
                    // render it), and any other failure: answered as before.
                    Err(_) => None,
                },
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

    /// Answer `readlink` of the caller's own descriptor link at the absolute
    /// spelling `spelled` from the backend's `link_path` fact, where Detcore
    /// reads it ([`Self::reads_link_facts`]), into the guest's `buffer` of
    /// `buffer_len` bytes. `spelled` is the spelling the walk resolved (each
    /// `..` within the caller's own task tree resolved, the components
    /// before `proc` that name nothing dropped); `guest_bytes` the guest's own
    /// spelling, whose last bytes say whether the link is followed
    /// ([`past_the_link`]).
    ///
    /// The spellings are those [`super::own_descriptor_link`] names: the
    /// process's table (`/proc/self/fd/N`, `/proc/<pid>/fd/N`, and
    /// `/dev/fd/N` where `/dev/fd` is the link to `/proc/self/fd` it is on
    /// Linux) and the calling thread's (`/proc/thread-self/fd/N`,
    /// `/proc/<pid>/task/<tid>/fd/N`). Linux (v6.17) looks the descriptor
    /// up in that task's table (`proc_lookupfd_common`, fs/proc/fd.c), so a
    /// descriptor it does not hold is `ENOENT`; and renders the file's path
    /// with `d_path` into `do_proc_readlink`'s buffer, copied without a NUL
    /// and cut to the buffer's size (fs/proc/base.c). A spelling that follows
    /// the link with a final `/` (`lookup_last`, fs/namei.c) needs a
    /// directory there: `ENOTDIR` for any other file, and `EINVAL` for a
    /// directory, which is no link (`do_readlinkat`, fs/stat.c).
    ///
    /// A spelling with a `.` after the link looks `.` up in the
    /// descriptor's file, as Linux's walk does past the link it follows
    /// (`link_path_walk`, fs/namei.c): Detcore issues that lookup from the
    /// descriptor itself (`readlinkat(fd, ".")`), so the kernel beneath
    /// answers `ENOTDIR` for a file that is not a directory, its search
    /// permission check for a directory, and then `EINVAL`, which a
    /// directory is (no link). A link the backend does not name (an object
    /// it keeps itself) is the procfs file Detcore recorded at the
    /// descriptor's open (`DetFd::procfs_link_name`), and is refused by name
    /// where Detcore recorded none.
    ///
    /// `Ok(None)` leaves the call to the path it took before: the process's
    /// table read by a thread that does not hold it, `/dev/fd` that is not
    /// that link, and a backend that cannot write with the guest's
    /// permissions.
    pub(crate) async fn readlink_own_descriptor_fact<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        (spelled, guest_bytes): (&[u8], &[u8]),
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
    ) -> Result<Option<i64>, Error> {
        if !self.reads_link_facts() {
            return Ok(None);
        }
        let Some(own) = self.procfs_caller_ids(guest) else {
            return Ok(None);
        };
        let Some(descriptor) =
            super::own_descriptor_link(Path::new(std::ffi::OsStr::from_bytes(spelled)), Some(own))
        else {
            return Ok(None);
        };
        let past = past_the_link(guest_bytes);
        if descriptor.table == super::DescriptorTable::Process {
            let state = guest.thread_state();
            if state.dettid.as_raw() != own.0 && !state.holds_process_files() {
                return Ok(None);
            }
        }
        if spelled.starts_with(b"/dev/")
            && self
                .injected_link_target(guest, b"/dev/fd")
                .await
                .as_deref()
                != Some(Path::new("/proc/self/fd"))
        {
            return Ok(None);
        }
        // Linux refuses a size that is not positive before it looks the path up.
        let size = super::readlink_size(buffer_len)?;
        // `ENOENT` for a descriptor the table does not hold, before anything
        // past the link is looked up.
        let text = self.own_link_fact(guest, PathLink::Descriptor(descriptor.fd))?;
        match past {
            Past::Nothing => {}
            // A `.` after the link: `.` looked up in the descriptor's file,
            // as Linux's walk looks it up past the link it follows.
            // A directory Detcore serves on a placeholder (package K slice
            // K10) is no file of the kernel beneath to look `.` up in: the
            // caller decides the spelling from Detcore's model
            // (`served_directory_walk`).
            Past::Walk => {
                if self
                    .served_directory_walk(guest, guest_bytes)
                    .await
                    .is_some()
                {
                    return Ok(None);
                }
                return self
                    .inject_dot_lookup(guest, descriptor.fd, b".", (buffer, size))
                    .await
                    .map(Some);
            }
            Past::Slash => {
                let stat = self.inject_fstat(guest, descriptor.fd).await?;
                return Err(if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                    Errno::EINVAL
                } else {
                    Errno::ENOTDIR
                }
                .into());
            }
        }
        let target = match text {
            Some(text) => {
                self.descriptor_link_target(guest, descriptor.fd, text)
                    .await?
            }
            // An object the backend keeps itself and does not name (the KVM
            // executor's own rendering of a process's `status` or `maps`):
            // the procfs file Detcore recorded at the descriptor's open, or a
            // refusal by name where it recorded none (a descriptor received
            // over `SCM_RIGHTS`, the captured output's own descriptor).
            None => match guest
                .thread_state()
                .with_detfd(descriptor.fd, |detfd| detfd.procfs_link_name())
                .ok()
                .flatten()
            {
                Some(name) => name.into_os_string().into_vec(),
                None => {
                    let reason = unnamed_descriptor_reason(spelled, descriptor.fd);
                    return self
                        .refuse_unrendered_link(guest, sysno, &reason)
                        .await
                        .map(Some);
                }
            },
        };
        let copied = &target[..target.len().min(size)];
        let address = buffer.ok_or(Errno::EFAULT)?;
        copy_link_out(guest, address.cast::<u8>(), copied)
    }

    /// The target the guest sees for the caller's descriptor `fd`, whose
    /// link the backend names `text`: a placeholder's link, confirmed by the
    /// kernel identity of the descriptor itself (an injected `fstat` of `fd`,
    /// `placeholder_identity`), names the procfs file it stands for, as
    /// Linux's link names the file, by every spelling of the link alike; a
    /// pipe's or a socket's
    /// (`pipe:[<inode>]`, `socket:[<inode>]`, the names `pipefs_dname` and
    /// `sockfs_dname` give an object with no path) names the inode number
    /// Detcore gives the descriptor's file (an inherited standard stream's,
    /// or the pool's for its host identity), as its `fstat` does; any other
    /// is the backend's text, a path (a named FIFO's included) or an
    /// object's name.
    async fn descriptor_link_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        text: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        // Confirmed through the descriptor, not the guest's spelling: the
        // calling thread's spelling, `/proc/<pid>/task/<tid>/fd/N`, names
        // nothing the KVM executor's `/proc` looks up
        // (`fd_directory::leader_process_spelling` respells a file of the
        // leader's directory, not a path below its `fd`), so a stat of it
        // failed and the placeholder's own name reached the guest. Every
        // rendered file had that defect; `status`, rendered on KVM since
        // package K item 2b slice 2b-2, is the one
        // run_kvm_the_callers_own_link_targets_answer_as_linux reads so.
        if let Some(path) = self.placeholder_link_within_level(guest, &text)
            && (self.cfg.backend.tool_shares_guest_descriptor_table
                || self.inject_fstat(guest, fd).await.is_ok_and(|stat| {
                    crate::syscalls::files::rendered_procfs::placeholder_identity(
                        stat.st_dev,
                        stat.st_ino,
                    )
                    .is_some()
                }))
        {
            return Ok(path.as_os_str().as_bytes().to_vec());
        }
        let Some(identity) = super::anonymous_proc_fd_identity(&text) else {
            return Ok(text);
        };
        let standard = guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                crate::syscalls::deterministic_stdio_inode_for_resource(fd, detfd.resource())
            })
            .ok()
            .flatten();
        let inode = match standard {
            Some(inode) => inode,
            None => {
                let stat = self.inject_fstat(guest, fd).await?;
                crate::tool_global::determinize_inode(
                    guest,
                    crate::types::RawInode::new(stat.st_dev, stat.st_ino),
                )
                .await
                .0
            }
        };
        Ok(format!("{}:[{inode}]", identity.kind).into_bytes())
    }

    /// `readlink` of the caller's own `link` followed by a `.`, where Detcore
    /// reads the links from the backend's facts, into the guest's `buffer`
    /// of `size` bytes. Linux follows the link and looks `.` up in what it
    /// leads to (`link_path_walk`, fs/namei.c): a file that is not a
    /// directory, the image, is `ENOTDIR`; a directory is searched
    /// (`may_lookup`'s `MAY_EXEC`, `EACCES` where that is denied), and is
    /// then no link (`EINVAL`, `do_readlinkat`, fs/stat.c). The working
    /// directory's and the root's lookup is the same lookup made from the
    /// caller's own working directory (`.`) and root (`/.`), which Detcore
    /// issues, so the kernel beneath decides the permission.
    async fn lookup_past_task_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        link: OwnLink,
        (buffer, size): (Option<AddrMut<'_, libc::c_char>>, usize),
    ) -> Result<i64, Error> {
        match link {
            OwnLink::Exe => Err(Errno::ENOTDIR.into()),
            OwnLink::Cwd => {
                self.inject_dot_lookup(guest, libc::AT_FDCWD, b".", (buffer, size))
                    .await
            }
            OwnLink::Root => {
                self.inject_dot_lookup(guest, libc::AT_FDCWD, b"/.", (buffer, size))
                    .await
            }
        }
    }

    /// Inject `readlinkat(dirfd, name, buffer, size)` for a `name` that ends
    /// in `.` (a directory, never a link), from the guest's stack, and give
    /// its answer: the kernel beneath's walk to `name` from `dirfd`.
    async fn inject_dot_lookup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        dirfd: i32,
        name: &[u8],
        (buffer, size): (Option<AddrMut<'_, libc::c_char>>, usize),
    ) -> Result<i64, Error> {
        let mut path_buffer = [0_u8; 8];
        path_buffer[..name.len()].copy_from_slice(name);
        let mut stack = guest.stack().await;
        let path_address = stack.push(path_buffer).cast::<libc::c_char>();
        let stack_guard = stack.commit()?;
        let result = guest
            .inject_with_retry(
                syscalls::Readlinkat::new()
                    .with_dirfd(dirfd)
                    .with_path(PathPtr::from_ptr(
                        path_address.as_raw() as *const libc::c_char
                    ))
                    .with_buf(buffer)
                    .with_buf_len(size),
            )
            .await;
        drop(stack_guard);
        Ok(result?)
    }

    /// Refuse a `readlink` of one of the caller's own links that Detcore
    /// cannot render where the backend's own `/proc` is not the guest's, by
    /// name, through the unsupported-operation policy (`EACCES` where the
    /// run allows unsupported operations).
    async fn refuse_unrendered_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        reason: &str,
    ) -> Result<i64, Error> {
        crate::syscalls::helpers::report_refusal_reason(
            self.cfg.panic_on_unsupported_syscalls,
            &format!(
                "[dtid {}] {sysno} refused: {reason}",
                guest.thread_state().dettid
            ),
        );
        if !self.cfg.panic_on_unsupported_syscalls {
            crate::tool_global::report_unsupported_syscall(guest, sysno).await;
        }
        self.refuse_unserviceable_operation_because(guest, sysno, Errno::EACCES, reason)
            .await
    }

    /// Whether Detcore reads the caller's own `fd/<n>`, `exe` and `cwd`
    /// links from the backend's `link_path` fact: where the backend's procfs
    /// is not the guest's kernel procfs
    /// ([`crate::procfs_inode::tool_can_name_guest_descriptors`], KVM), so
    /// that the backend's own readlink answer is not the kernel's, and not
    /// under record and replay, which leave procfs to the kernel. Where it is
    /// the kernel's (ptrace), the kernel answers, as before.
    fn reads_link_facts(&self) -> bool {
        !self.cfg.recordreplay_modes
            && !crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend)
    }

    /// What the calling thread's descriptor `fd` links to, read for one of
    /// Detcore's own decisions rather than for a guest `readlink`, where
    /// Detcore reads the caller's links from the backend's `link_path` fact
    /// ([`Self::reads_link_facts`], KVM): `Some(Some(text))`, Linux's
    /// readlink text; `Some(None)` where the backend does not name the file
    /// (an object it keeps itself) or the read fails. `None` elsewhere: the
    /// caller reads the link in the guest (`/proc/thread-self/fd/<fd>`, an
    /// injected `readlinkat`), whose answer there is the kernel's. Where the
    /// fact is read, an injected read would reach the backend's own `/proc`,
    /// which is not the guest's: the KVM executor no longer answers the
    /// guest's links itself (package K slice F2-d).
    pub(crate) fn own_descriptor_link_from_fact<G: Guest<Self>>(
        guest: &G,
        fd: i32,
    ) -> Option<Option<Vec<u8>>> {
        let config = guest.config();
        if config.recordreplay_modes
            || crate::procfs_inode::tool_can_name_guest_descriptors(&config.backend)
        {
            return None;
        }
        Some(
            guest
                .link_path(guest.tid(), PathLink::Descriptor(fd))
                .ok()
                .map(|text| text.as_bytes().to_vec()),
        )
    }

    /// The text of the caller's own path link `link` from the backend's
    /// `link_path` fact, where Detcore reads it ([`Self::reads_link_facts`]):
    /// `Ok(Some(text))`, Linux's readlink text; `Err(ENOENT)` for a
    /// descriptor the caller's table does not hold, as procfs has no such
    /// entry; `Err` with the backend's errno for a name it could not render
    /// (`ENAMETOOLONG`, as `d_path`'s); `Ok(None)` where Detcore does not
    /// read the fact or the backend does not provide it, and for an `ESRCH`
    /// that would say the calling thread is not live, which the fact's
    /// contract excludes: the caller's other sources answer.
    fn own_link_fact<G: Guest<Self>>(
        &self,
        guest: &G,
        link: PathLink,
    ) -> Result<Option<Vec<u8>>, Errno> {
        if !self.reads_link_facts() {
            return Ok(None);
        }
        match guest.link_path(guest.tid(), link) {
            Ok(text) => Ok(Some(text.as_bytes().to_vec())),
            Err(FactError::Errno(Errno::EBADF)) if matches!(link, PathLink::Descriptor(_)) => {
                Err(Errno::ENOENT)
            }
            Err(FactError::Errno(Errno::ESRCH)) => {
                tracing::debug!(
                    "[detcore, tid {}] the backend's link_path fact says the caller is not live; \
                     its {link:?} link is answered as before",
                    guest.tid()
                );
                Ok(None)
            }
            Err(FactError::Errno(errno)) => Err(errno),
            Err(_) => Ok(None),
        }
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

/// The reason a `readlink` of another thread's working or root directory
/// link at `path` is refused where Detcore reads the links from the
/// backend's facts.
fn other_thread_directory_reason(path: &Path) -> String {
    format!(
        "{} cannot be served: on this backend Detcore reads the working and root directories \
         of the calling thread alone, from the backend's link_path fact, and does not know \
         whether another thread shares them (https://github.com/rrnewton/hermit/issues/4698)",
        path.display()
    )
}

/// The reason a `readlink` of the caller's own descriptor link at `spelled`
/// is refused where the backend does not name descriptor `fd`'s file and
/// Detcore recorded no procfs file for it.
fn unnamed_descriptor_reason(spelled: &[u8], fd: i32) -> String {
    format!(
        "{} cannot be served: the backend keeps descriptor {fd}'s file itself and does not \
         name it, and Detcore recorded no procfs file at its open (a descriptor received over \
         SCM_RIGHTS, or the run's captured output)",
        String::from_utf8_lossy(spelled)
    )
}

/// The path `readlink` of a descriptor link gives for a per-process procfs
/// file opened at the absolute spelling `observed` by a caller whose pid and
/// tid, as the guest numbers them, are `own`: the spelling with `self` and
/// `thread-self` resolved as Linux's lookup resolves them, which is the path
/// `d_path` renders for the opened file (`/proc/<pid>/status`,
/// `/proc/<pid>/task/<tid>/status`). `None` for any other spelling: one
/// with `..`, a name below a further directory, a number procfs would not
/// have (a leading zero), or no `own`.
pub(crate) fn procfs_file_link_name(observed: &Path, own: Option<(i32, i32)>) -> Option<PathBuf> {
    let (pid, tid) = own?;
    let below = observed.strip_prefix("/proc").ok()?;
    let names = below
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let number = |name: &str| {
        (!name.starts_with('0') && name.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| name.parse::<i32>().ok())
            .flatten()
    };
    let file = |name: &str| (!name.is_empty()).then_some(());
    Some(PathBuf::from(match names.as_slice() {
        ["self", name] => {
            file(name)?;
            format!("/proc/{pid}/{name}")
        }
        ["thread-self", name] => {
            file(name)?;
            format!("/proc/{pid}/task/{tid}/{name}")
        }
        ["self", "task", thread, name] => {
            file(name)?;
            format!("/proc/{pid}/task/{}/{name}", number(thread)?)
        }
        [process, name] => {
            file(name)?;
            format!("/proc/{}/{name}", number(process)?)
        }
        [process, "task", thread, name] => {
            file(name)?;
            format!("/proc/{}/task/{}/{name}", number(process)?, number(thread)?)
        }
        _ => return None,
    }))
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
    fn a_links_last_bytes_say_whether_it_is_followed() {
        for path in ["/proc/self/fd/3", "/dev/fd/3", "/proc/self/fd/3/.."] {
            assert_eq!(past_the_link(path.as_bytes()), Past::Nothing, "{path}");
        }
        for path in ["/proc/self/fd/3/", "/proc/thread-self/fd/3///"] {
            assert_eq!(past_the_link(path.as_bytes()), Past::Slash, "{path}");
        }
        for path in [
            "/proc/self/fd/3/.",
            "/proc/self/fd/3/./",
            "/proc/self/fd/3//.",
            "/proc/self/fd/3/./.",
        ] {
            assert_eq!(past_the_link(path.as_bytes()), Past::Walk, "{path}");
        }
    }

    #[test]
    fn a_procfs_files_link_names_its_task_by_number() {
        let own = Some((3, 5));
        let named = |path: &str| {
            procfs_file_link_name(Path::new(path), own).map(|name| name.display().to_string())
        };
        assert_eq!(
            named("/proc/self/status").as_deref(),
            Some("/proc/3/status")
        );
        assert_eq!(named("/proc/self/maps").as_deref(), Some("/proc/3/maps"));
        assert_eq!(
            named("/proc/thread-self/status").as_deref(),
            Some("/proc/3/task/5/status")
        );
        assert_eq!(
            named("/proc/self/task/7/status").as_deref(),
            Some("/proc/3/task/7/status")
        );
        assert_eq!(named("/proc/9/status").as_deref(), Some("/proc/9/status"));
        assert_eq!(
            named("/proc/9/task/11/maps").as_deref(),
            Some("/proc/9/task/11/maps")
        );
        // Linux's walk skips empty and `.` components.
        assert_eq!(
            named("/proc//self/./status").as_deref(),
            Some("/proc/3/status")
        );
        for path in [
            "/proc/self/../self/status",
            "/proc/09/status",
            "/proc/thread-self/task/5/status",
            "/proc/self/fd/3/x",
            "/proc/status",
            "/proc",
            "/tmp/self/status",
        ] {
            assert_eq!(named(path), None, "{path}");
        }
        assert_eq!(
            procfs_file_link_name(Path::new("/proc/self/status"), None),
            None
        );
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
