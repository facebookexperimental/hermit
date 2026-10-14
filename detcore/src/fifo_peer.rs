/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Does a guest hold the other end of a FIFO?
//!
//! A FIFO opened by path is a `Regular` descriptor, so a read or write that
//! must wait blocks in the kernel while the caller holds the scheduler turn.
//! When the other end belongs to a host process that is right: the host makes
//! progress on its own, and every guest waits at a fixed point. When it
//! belongs to another guest, that guest never runs, and the run deadlocks
//! (<https://github.com/rrnewton/hermit/issues/4200>).
//!
//! So the first blocking read or write on such a FIFO asks whether some guest
//! holds a descriptor for the same FIFO with the access the other end needs
//! (a writer for a read, a reader for a write). Only then does Detcore make
//! the FIFO an internally polled pipe; otherwise the call blocks as before.
//!
//! The answer is deterministic. The scan runs in the caller's turn, when every
//! other guest thread is stopped, and a guest's descriptor table changes only
//! in its own turns. A host process is never counted: the scan covers only the
//! processes of Hermit's private PID namespace, whose `/proc` lists nothing
//! else, and is skipped (answering no) when the tracer is not that
//! namespace's init.

use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// The end of a FIFO a waiting call needs another holder of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FifoPeer {
    /// A descriptor open for writing (`O_WRONLY` or `O_RDWR`), which a read
    /// waits for.
    Writer,
    /// A descriptor open for reading (`O_RDONLY` or `O_RDWR`), which a write
    /// waits for.
    Reader,
}

impl FifoPeer {
    /// Whether a descriptor with these status flags is such an end. An
    /// `O_PATH` handle is neither: it reads and writes nothing, and its access
    /// bits are 0, which would otherwise read as `O_RDONLY`.
    fn accepts_flags(self, flags: i32) -> bool {
        flags & libc::O_PATH == 0 && self.accepts(flags & libc::O_ACCMODE)
    }

    fn accepts(self, access_mode: i32) -> bool {
        match self {
            FifoPeer::Writer => access_mode == libc::O_WRONLY || access_mode == libc::O_RDWR,
            FifoPeer::Reader => access_mode == libc::O_RDONLY || access_mode == libc::O_RDWR,
        }
    }
}

/// The file status flags of `<table>/fdinfo/<fd>`, as the kernel holds them
/// now, where `table` is a process's or a thread's `/proc` directory.
fn fdinfo_flags(table: &Path, fd: &str) -> Option<i32> {
    let info = fs::read_to_string(table.join("fdinfo").join(fd)).ok()?;
    let flags = info.lines().find_map(|line| line.strip_prefix("flags:"))?;
    i32::from_str_radix(flags.trim(), 8).ok()
}

/// Whether Detcore can see a guest's descriptors through `/proc/<tid>` on
/// `backend`, as every check here needs. Only where the tool is a separate
/// tracer, outside the guest's descriptor table (not DBT, whose tool runs in
/// the guest), and where a guest thread's ID is a host task's
/// ([`crate::procfs_inode::guest_tids_name_host_tasks`]; not KVM, where
/// `/proc/<tid>` is whatever host task has that ID). `physical_tid` is the
/// guest thread's host ID where the backend supplies one beside its own.
/// Elsewhere a FIFO keeps blocking, as before.
pub(crate) fn guest_descriptors_are_visible(
    backend: &reverie::BackendCapabilities,
    physical_tid: Option<i32>,
) -> bool {
    !backend.tool_shares_guest_descriptor_table
        && crate::procfs_inode::guest_tids_name_host_tasks(backend, physical_tid)
}

/// Whether guest thread `tid`'s descriptor `fd` names a FIFO (a named FIFO,
/// or a pipe reached by path). Read through the thread, whose descriptor
/// table may not be its leader's (a thread cloned without CLONE_FILES), and
/// only where [`guest_descriptors_are_visible`]: elsewhere `/proc/<tid>` is
/// not the guest's, and the answer is no. Every later check here runs only on
/// a FIFO this said yes to.
pub(crate) fn is_fifo(
    backend: &reverie::BackendCapabilities,
    physical_tid: Option<i32>,
    tid: i32,
    fd: i32,
) -> bool {
    guest_descriptors_are_visible(backend, physical_tid)
        && fs::metadata(format!("/proc/{tid}/fd/{fd}")).is_ok_and(|meta| meta.file_type().is_fifo())
}

/// Whether the kernel's open file description behind thread `tid`'s `fd` is
/// nonblocking now. `None` when procfs cannot say.
pub(crate) fn kernel_nonblocking(tid: i32, fd: i32) -> Option<bool> {
    fdinfo_flags(&Path::new("/proc").join(tid.to_string()), &fd.to_string())
        .map(|flags| flags & libc::O_NONBLOCK != 0)
}

/// Whether a guest holds a descriptor for the FIFO behind thread `tid`'s
/// `fd` with the access `peer` names, other than `fd`'s own open file
/// description, wherever that is held (a `dup`, or a child that inherited it).
/// A FIFO the guest opened read-write is its own writer to the kernel, but
/// whether data ever comes is up to the FIFO's other holders, which may be
/// host processes only (`exec 3<>fifo`). `fd` is read in `tid`'s own table.
pub(crate) fn guest_holds_peer(tid: i32, fd: i32, peer: FifoPeer) -> bool {
    // Outside a private PID namespace `/proc` lists host processes too.
    let tracer = std::process::id();
    if tracer != 1 {
        return false;
    }
    let Ok(fifo) = fs::metadata(format!("/proc/{tid}/fd/{fd}")) else {
        return false;
    };
    let proc_root = Path::new("/proc");
    let Ok(entries) = fs::read_dir(proc_root) else {
        return false;
    };
    let tracer = tracer.to_string();
    entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.bytes().all(|b| b.is_ascii_digit()) && *name != tracer)
        .any(|process| {
            descriptor_tables(&proc_root.join(process))
                .iter()
                .any(|(table, task)| {
                    holds_peer(
                        table,
                        *task,
                        (fifo.dev(), fifo.ino()),
                        peer,
                        Some((tid, fd)),
                    )
                })
        })
}

/// Whether the descriptor table at `table` (a task's `/proc` directory, of
/// task `task`) holds a descriptor for the FIFO `(dev, ino)` with the access
/// `peer` names, other than the open file description of `own`, a
/// `(tid, fd)` pair.
fn holds_peer(
    table: &Path,
    task: i32,
    (dev, ino): (u64, u64),
    peer: FifoPeer,
    own: Option<(i32, i32)>,
) -> bool {
    let Ok(descriptors) = fs::read_dir(table.join("fd")) else {
        return false;
    };
    descriptors.filter_map(Result::ok).any(|descriptor| {
        let Some(fd) = descriptor
            .file_name()
            .to_str()
            .and_then(|fd| fd.parse::<i32>().ok())
        else {
            return false;
        };
        fs::metadata(descriptor.path())
            .is_ok_and(|meta| meta.file_type().is_fifo() && meta.dev() == dev && meta.ino() == ino)
            && fdinfo_flags(table, &fd.to_string()).is_some_and(|flags| peer.accepts_flags(flags))
            && !own.is_some_and(|(tid, own_fd)| same_description(tid, own_fd, task, fd))
    })
}

/// Each distinct descriptor table of a process, as a task's `/proc` directory
/// and that task's id. Its threads usually share one table, but a thread
/// cloned without CLONE_FILES has its own, and the leader's `fd` directory is
/// empty once the leader has exited (`pthread_exit`). Threads with the same
/// table are listed once (`kcmp(KCMP_FILES)`); when the kernel cannot say,
/// each is listed, which costs only a second look.
///
/// The leader-exit case is untested: on ptrace any leader `pthread_exit`
/// beside a live thread hangs before a FIFO is read
/// (<https://github.com/rrnewton/hermit/issues/4100>).
fn descriptor_tables(process: &Path) -> Vec<(std::path::PathBuf, i32)> {
    let Ok(tasks) = fs::read_dir(process.join("task")) else {
        return Vec::new();
    };
    let mut tasks: Vec<(std::path::PathBuf, i32)> = tasks
        .filter_map(|task| {
            let task = task.ok()?;
            let id = task.file_name().to_str()?.parse::<i32>().ok()?;
            Some((task.path(), id))
        })
        .collect();
    tasks.sort_by_key(|(_, id)| *id);
    let mut tables: Vec<(std::path::PathBuf, i32)> = Vec::new();
    for (dir, id) in tasks {
        let listed = fs::read_dir(dir.join("fd")).is_ok_and(|mut entries| entries.next().is_some());
        if listed && !tables.iter().any(|(_, seen)| same_table(*seen, id)) {
            tables.push((dir, id));
        }
    }
    tables
}

/// Whether tasks `a` and `b` share one descriptor table (`kcmp(KCMP_FILES)`).
fn same_table(a: i32, b: i32) -> bool {
    const KCMP_FILES: libc::c_int = 2;
    // SAFETY: kcmp reads no memory; it compares two tasks' tables.
    let order = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            a as libc::pid_t,
            b as libc::pid_t,
            KCMP_FILES,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    order == 0
}

/// Whether `fd1` of task `pid1` and `fd2` of task `pid2` are one open file
/// description (`kcmp(KCMP_FILE)`). When the kernel cannot say, they are
/// taken to be the same, so the descriptor is not counted as a peer and the
/// FIFO keeps blocking as before.
fn same_description(pid1: i32, fd1: i32, pid2: i32, fd2: i32) -> bool {
    const KCMP_FILE: libc::c_int = 0;
    // SAFETY: kcmp reads no memory; it compares two tasks' descriptors.
    let order = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            pid1 as libc::pid_t,
            pid2 as libc::pid_t,
            KCMP_FILE,
            fd1 as libc::c_ulong,
            fd2 as libc::c_ulong,
        )
    };
    !(1..=3).contains(&order)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    use super::*;

    /// The descriptor checks run only under a separate tracer whose guest
    /// thread IDs are host tasks: ptrace and in-guest LiteInst (whose tool is
    /// still a separate tracer), not DBT or SaBRe (their tool shares the
    /// guest's table) nor KVM (`/proc/<tid>` there is an unrelated host task;
    /// on one host it measured as a `kvm-nx-lpage-recovery` worker thread of
    /// Hermit's own process), nor a thread whose host ID differs from its own.
    #[test]
    fn guest_descriptors_are_visible_only_under_a_host_task_tracer() {
        use reverie::BackendCapabilities;
        assert!(guest_descriptors_are_visible(
            &BackendCapabilities::PTRACE,
            None
        ));
        assert!(guest_descriptors_are_visible(
            &BackendCapabilities::LITEINST_IN_GUEST,
            None
        ));
        assert!(!guest_descriptors_are_visible(
            &BackendCapabilities::KVM,
            None
        ));
        assert!(!guest_descriptors_are_visible(
            &BackendCapabilities::DBT,
            Some(4_321)
        ));
        assert!(!guest_descriptors_are_visible(
            &BackendCapabilities::SABRE,
            None
        ));
        assert!(!guest_descriptors_are_visible(
            &BackendCapabilities::PTRACE,
            Some(4_321)
        ));
    }

    /// A real FIFO, held by this test process: it is a FIFO only where the
    /// backend's guest descriptors are visible, so no caller can read another
    /// task's table on KVM.
    #[test]
    fn a_fifo_is_seen_only_where_guest_descriptors_are_visible() {
        use reverie::BackendCapabilities;
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let end = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&end);
        let tid = nix::unistd::gettid().as_raw();
        assert!(is_fifo(&BackendCapabilities::PTRACE, None, tid, fd));
        assert!(!is_fifo(&BackendCapabilities::KVM, None, tid, fd));
        assert!(!is_fifo(&BackendCapabilities::DBT, Some(tid), tid, fd));
    }

    fn fifo_in(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("fifo");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
        path
    }

    fn identity(file: &File) -> (u64, u64) {
        let meta = file.metadata().unwrap();
        (meta.dev(), meta.ino())
    }

    #[test]
    fn a_peer_is_a_descriptor_for_the_same_fifo_with_the_other_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let me = std::process::id().to_string();
        let proc_root = Path::new("/proc");

        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        let fifo = identity(&reader);
        assert!(holds_peer(
            &proc_root.join(&me),
            me.parse().unwrap(),
            fifo,
            FifoPeer::Reader,
            None
        ));
        assert!(
            !holds_peer(
                &proc_root.join(&me),
                me.parse().unwrap(),
                fifo,
                FifoPeer::Writer,
                None
            ),
            "a reader is not the writer a read waits for"
        );

        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(holds_peer(
            &proc_root.join(&me),
            me.parse().unwrap(),
            fifo,
            FifoPeer::Writer,
            None
        ));

        // Another FIFO's writer is not a peer.
        let other_dir = tempfile::tempdir().unwrap();
        let other = fifo_in(other_dir.path());
        let _other_reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&other)
            .unwrap();
        let _other_writer = OpenOptions::new().write(true).open(&other).unwrap();
        drop(writer);
        assert!(
            !holds_peer(
                &proc_root.join(&me),
                me.parse().unwrap(),
                fifo,
                FifoPeer::Writer,
                None
            ),
            "the other FIFO's writer must not count"
        );
    }

    #[test]
    fn a_read_write_descriptor_is_both_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let me = std::process::id().to_string();
        let both = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let fifo = identity(&both);
        assert!(holds_peer(
            &Path::new("/proc").join(&me),
            me.parse().unwrap(),
            fifo,
            FifoPeer::Writer,
            None
        ));
        assert!(holds_peer(
            &Path::new("/proc").join(&me),
            me.parse().unwrap(),
            fifo,
            FifoPeer::Reader,
            None
        ));
    }

    #[test]
    fn the_callers_own_description_is_not_its_peer() {
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let me = std::process::id() as i32;
        let both = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&both);
        let fifo = identity(&both);
        let own = Some((me, fd));
        let proc_root = Path::new("/proc");
        assert!(
            !holds_peer(
                &proc_root.join(me.to_string()),
                me,
                fifo,
                FifoPeer::Writer,
                own
            ),
            "a read-write description must not be its own writer"
        );
        let alias = both.try_clone().unwrap();
        assert!(
            !holds_peer(
                &proc_root.join(me.to_string()),
                me,
                fifo,
                FifoPeer::Writer,
                own
            ),
            "a dup of the caller's description must not count either"
        );
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(
            holds_peer(
                &proc_root.join(me.to_string()),
                me,
                fifo,
                FifoPeer::Writer,
                own
            ),
            "another description open for writing is a peer"
        );
        drop((alias, writer));
    }

    #[test]
    fn an_o_path_handle_is_neither_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let me = std::process::id().to_string();
        let table = Path::new("/proc").join(&me);
        let task: i32 = me.parse().unwrap();
        let observer = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH)
            .open(&path)
            .unwrap();
        let fifo = identity(&observer);
        assert!(
            !holds_peer(&table, task, fifo, FifoPeer::Reader, None),
            "an O_PATH handle must not count as a reader"
        );
        assert!(!holds_peer(&table, task, fifo, FifoPeer::Writer, None));
        assert!(FifoPeer::Reader.accepts_flags(libc::O_RDONLY));
        assert!(!FifoPeer::Reader.accepts_flags(libc::O_PATH | libc::O_CLOEXEC));
    }

    #[test]
    fn fdinfo_flags_are_the_kernel_status_flags() {
        let dir = tempfile::tempdir().unwrap();
        let path = fifo_in(dir.path());
        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&reader);
        let pid = std::process::id() as i32;
        assert!(is_fifo(
            &reverie::BackendCapabilities::PTRACE,
            None,
            pid,
            fd
        ));
        assert_eq!(kernel_nonblocking(pid, fd), Some(true));
        // SAFETY: plain fcntl on a descriptor this test owns.
        unsafe { libc::fcntl(fd, libc::F_SETFL, 0) };
        assert_eq!(kernel_nonblocking(pid, fd), Some(false));
    }
}
