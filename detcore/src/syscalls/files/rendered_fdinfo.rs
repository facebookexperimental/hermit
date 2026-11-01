/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Where the values of a descriptor's fdinfo that Detcore renders come from
//! (package K slice K8-h of <https://github.com/rrnewton/hermit/issues/4213>);
//! the text is `crate::procfs::fdinfo`'s.
//!
//! Where the guest's procfs is not the kernel's
//! (`procfs_inode::tool_can_name_guest_descriptors` false: KVM), Detcore
//! serves the caller's own process's `/proc/<pid>/fdinfo/<fd>`, by every
//! spelling, as a per-process rendered file (`Detcore::rendered_procfs_at`),
//! and renders it at its first read, and again where Linux's `seq_file` would
//! run `seq_show` again, as for a rendered `stat`
//! (`ProcfsFile::renders_task_record`). Each line comes from the source Linux
//! takes it from, as the guest can observe it:
//!
//! - `pos`: the position the guest's own `lseek(fd, 0, SEEK_CUR)` answers:
//!   Detcore's for a procfs file it serves from a snapshot, a `/proc`
//!   directory it serves on a placeholder, and a directory it lists from a
//!   stream; elsewhere an injected `lseek(fd, 0, SEEK_CUR)`, Linux's `f_pos`,
//!   and 0 where the file cannot seek (a pipe, a socket, a terminal:
//!   `ESPIPE`; an `O_PATH` descriptor: `EBADF`), whose position Linux never
//!   moves.
//! - `flags`: the status flags the guest's own `F_GETFL` returns
//!   (`guest_status_flags`), with `O_CLOEXEC` where the guest's own
//!   `F_GETFD` shows `FD_CLOEXEC`, as `seq_show` adds it.
//! - `mnt_id`: the mount an injected `statx(STATX_MNT_ID)` names, through the
//!   run's mount-ID pool, as the guest's own `statx` numbers it, and 0 where
//!   it names none (KVM's captured standard output and error); for a file
//!   Detcore serves, the guest's procfs mount.
//! - `ino`: the inode number the guest's `fstat` of the descriptor shows
//!   (`Detcore::fdinfo_inode`).
//! - `lock:` lines: the locks the backend's `descriptor_status` fact reports
//!   the open file description holds, in Linux's order, each owner from
//!   Detcore's lock ledger (`procfs::locks::LockLedger::owners`): -1 for an
//!   OFD lock; the guest process a call was proved to take any other in (the
//!   description's locks just before the call against those after it,
//!   `syscalls::files::locks_taken`); 0 for one no call was (a lock inherited
//!   or received already held, whose taker the guest's PID namespace cannot
//!   see), as Linux shows such a taker. The device and
//!   inode are the guest's: the device as `mountinfo` shows the file's
//!   superblock, the inode as the `ino` line. A guest's record lock
//!   (`F_SETLK`) that the backend holds as an OFD lock (KVM's provider:
//!   <https://github.com/rrnewton/reverie/issues/1223>) would show as an OFD
//!   lock, where Linux shows a `POSIX` lock to its taker's table only: a read
//!   that would show an OFD lock on such a file is refused by name
//!   (`procfs::locks::LockLedger::record_lock_held_as_ofd`).
//! - an eventfd's count from the fact; its ID as Detcore numbers the
//!   description (the number the sanitizer of a kernel fdinfo prints, so
//!   every backend shows one); `EFD_SEMAPHORE` as `eventfd2` set it.
//! - a signalfd's mask as `signalfd4` last set it, on the description every
//!   alias shares (an update changes the one context, as Linux's does).
//! - a timerfd's clock and last `timerfd_settime` flags as the guest set
//!   them, and its count and time left from Detcore's virtual timer, as
//!   Linux's own timer would show them at this read on the virtual clock
//!   (`LinuxTimerfd::view`).
//! - a Unix socket's queued descriptors (`scm_fds`) from the fact, wherever
//!   the fact reports them, whatever Detcore's type for the descriptor (an
//!   inherited standard stream is modelled as a regular file).
//! - a pidfd's process as Detcore's process table numbers it, or -1 once it
//!   has been reaped.
//!
//! What the object is (a socket, an anonymous inode, a character device whose
//! driver prints lines of its own) comes from the descriptor's own stat: for a
//! container standard stream, whose model holds the supervisor's standard
//! input's stat whatever the stream is (`setup_stdio`), from an injected
//! `fstat` of the descriptor itself, as the guest's own `fstat` sees it.
//!
//! Named differences and residuals, rendered as stated rather than hidden:
//!
//! - a memfd's seals: Linux v7.1.3 prints none (shmem has no `show_fdinfo`),
//!   so nothing is printed and nothing is injected for them;
//! - where the backend declines the fact for a descriptor (`NotProvided`: an
//!   object it keeps itself, such as, with captured output, a guest's own
//!   `/dev/null` or terminal that is the supervisor's standard output), the
//!   file is rendered without the fact's lines. A `/dev/null`, a terminal, a
//!   pipe or a regular file prints none of its own, so only a lock held
//!   through it is missing; a socket's would be its queued descriptors, so a
//!   declined socket's read is refused by name;
//! - a read is refused by name, through the unsupported-operation policy
//!   (`Detcore::scratch_failure`: a fail-closed run stops with the
//!   policy-refusal exit, and with `--allow-unsupported-syscalls` the read
//!   fails with `EOPNOTSUPP`), never answered with an empty file, for an
//!   epoll set (its `tfd:` lines), an inotify instance, a userfaultfd, an
//!   anonymous inode Detcore does not classify, a descriptor Detcore keeps no
//!   description of, an eventfd, signalfd, timerfd or pidfd whose facts
//!   Detcore did not record (inherited, received without joining the
//!   description its sender's table modeled, or rediscovered after an exec),
//!   a timerfd where Detcore's virtual timerfd is off (a run that is not
//!   strict: its timer is the host's), a socket the fact does not describe,
//!   and a character device whose driver prints lines of its own (a
//!   pseudo-terminal master's `tty-index`, the number of a host terminal
//!   that other host programs' terminals decide, a tun or DRM device's). The
//!   ptrace backend's sanitizer of a kernel fdinfo answers such a file with
//!   an empty one, a residual of that path that this one does not share;
//! - an eventfd's ID is Detcore's number for the description, not Linux's;
//! - a read by a thread of another process (a descriptor inherited across
//!   `fork` or received over `SCM_RIGHTS`) is refused by name: the fact and
//!   the injected calls reach the reader's own descriptor table only. KVM
//!   gives no thread a table of its own (`clone` without `CLONE_FILES` is
//!   refused), so every thread of the process reads the one table.
//!
//! Determinism: every value is a function of the guest's own history at the
//! read. The injected calls run in the reader's turn, with every other guest
//! thread parked; the fact reports the backend's state, which only guest
//! calls change (the counter, the queued messages, the locks), all in turns;
//! the ledger's takers come from guest calls in turns; time is the guest's
//! virtual clock at the read. Host identities (mount, device, inode) reach
//! the text only through Detcore's pools, as everywhere else.

use std::os::fd::RawFd;
use std::path::Path;

use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AtFlags;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::StatxMask;
use reverie::syscalls::Sysno;

use super::rendered_procfs::c_string;
use crate::fd::FdType;
use crate::fd::FdinfoFacts;
use crate::procfs::BoundTask;
use crate::procfs::ProcfsFile;
use crate::procfs::ProcfsTask;
use crate::procfs::fdinfo::Fdinfo;
use crate::procfs::fdinfo::FdinfoKind;
use crate::procfs::fdinfo::FdinfoLock;
use crate::procfs::fdinfo::TimerfdLines;
use crate::procfs_inode;
use crate::record_or_replay::RecordOrReplay;
use crate::stat::DetStat;
use crate::tool_local::Detcore;

/// What Detcore's model holds of the descriptor an fdinfo describes.
struct DescriptorModel {
    ty: FdType,
    nonblocking: bool,
    status_flags: i32,
    stat: Option<DetStat>,
    open_file: crate::types::OpenFileId,
    resource: Option<crate::resources::ResourceID>,
    facts: Option<FdinfoFacts>,
    timerfd: Option<crate::fd::VirtualTimerfd>,
    pidfd_target: Option<crate::types::DetPid>,
    /// A procfs file Detcore renders.
    rendered: Option<&'static Path>,
    /// The position of a procfs file Detcore serves from a snapshot, rendered
    /// or not, which the guest's own `lseek` answers.
    procfs_position: Option<usize>,
    /// The position of a directory Detcore lists from a stream
    /// (`DirectoryStream`), which the guest's own `lseek` answers.
    directory_position: Option<u64>,
    placeholder: bool,
}

/// The value of an fdinfo `pos:` line (`pos:\t%lli`), if `text` has one.
fn fdinfo_position(text: &[u8]) -> Option<i64> {
    String::from_utf8_lossy(text)
        .lines()
        .find_map(|line| line.strip_prefix("pos:\t"))
        .and_then(|position| position.parse().ok())
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Whether Detcore renders the fdinfo of the caller's own descriptors
    /// (package K slice K8-h): where the guest's procfs is not the kernel's
    /// (`procfs_inode::tool_can_name_guest_descriptors`, KVM), so no kernel
    /// fdinfo of the guest's descriptors exists to sanitize. Elsewhere the
    /// kernel's fdinfo is the guest's own and is sanitized, as before
    /// (`procfs::sanitize_fdinfo`).
    pub(crate) fn renders_fdinfo(&self) -> bool {
        !procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend)
    }

    /// Whether `task` is a thread of the caller's own process, whose
    /// descriptor table the backend's `descriptor_status` fact and the
    /// injected calls reach: `/proc/self`, `/proc/thread-self`, the caller's
    /// pid and its `task/<tid>` directories, and `/proc/<tid>` of a thread of
    /// the caller's process, by Detcore's process table.
    pub(super) async fn names_own_process<G: Guest<Self>>(
        &self,
        guest: &mut G,
        task: BoundTask,
    ) -> bool {
        let own = self.own_detpid(guest).as_raw();
        task.process == own
            || crate::tool_global::task_stat_facts(guest, task)
                .await
                .process
                == Some(own)
    }

    /// [`Self::names_own_process`] for a spelling of a task, bound to the
    /// caller, which Detcore's process table must have.
    pub(super) async fn names_own_process_spelling<G: Guest<Self>>(
        &self,
        guest: &mut G,
        task: ProcfsTask,
    ) -> bool {
        let Some((pid, tid)) = self.procfs_caller_ids(guest) else {
            return false;
        };
        let Some(task) = task.bound_for(pid, tid) else {
            return false;
        };
        crate::tool_global::procfs_task(guest, task).await.exists
            && self.names_own_process(guest, task).await
    }

    /// Whether the caller's descriptor table holds descriptor `fd`, as
    /// Linux's lookup of `fdinfo/<fd>` requires (`proc_lookupfd_common`):
    /// Detcore's table says, modelled or not; where a receive may have
    /// installed descriptors Detcore does not know, the backend's
    /// `descriptor_status` fact says whether one it does not know is open.
    pub(super) fn table_holds<G: Guest<Self>>(guest: &G, fd: RawFd) -> bool {
        let (numbers, _, unaccounted) = guest.thread_state().descriptor_table();
        numbers.binary_search(&fd).is_ok()
            || (unaccounted
                && !matches!(
                    guest.descriptor_status(fd),
                    Err(reverie::FactError::Errno(Errno::EBADF))
                ))
    }

    /// The text of the fdinfo Detcore opened as rendered at descriptor `fd`,
    /// as Linux's `seq_show` would print it at this read
    /// (`procfs::fdinfo::render`); `None` for any other file. The task it
    /// belongs to must still be the one it was bound to, and its descriptor
    /// must still be open in the reader's table (ENOENT otherwise, as
    /// `seq_show` fails). A read by a thread of another process is refused by
    /// name through the unsupported-operation policy.
    pub(super) async fn render_own_fdinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<Vec<u8>>, Error> {
        let Some(path) = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_rendered_path())?
        else {
            return Ok(None);
        };
        let Some(file) = ProcfsFile::from_rendered_path(path).filter(ProcfsFile::is_fdinfo) else {
            return Ok(None);
        };
        let (Some(task), Some(target)) = (file.bound_task(), file.rendered_fdinfo_target()) else {
            return Err(Errno::ENOENT.into());
        };
        match self.rendered_task_state(guest, path).await {
            Some((_, true)) => {}
            _ => return Err(Errno::ENOENT.into()),
        }
        if !self.names_own_process(guest, task).await {
            let reason = format!(
                "a thread of another process read the rendered fdinfo of descriptor {target} of \
                 task {task:?} (a descriptor inherited across fork or received over SCM_RIGHTS); \
                 Detcore reads the reader's own descriptor table only"
            );
            let sysno = Sysno::new(guest.regs().await.orig_rax as usize).unwrap_or(Sysno::read);
            return Err(Self::scratch_failure(guest, sysno, reason).await);
        }
        if !Self::table_holds(guest, target) {
            return Err(Errno::ENOENT.into());
        }
        Ok(Some(self.fdinfo_text(guest, target).await?))
    }

    /// Refuse the read of descriptor `target`'s rendered fdinfo by name,
    /// through the unsupported-operation policy (`Self::scratch_failure`):
    /// Detcore cannot render it as Linux prints it, for the reason `why`. Never
    /// an empty file, which would be a successful read of nothing (see the
    /// module comment).
    async fn refuse_fdinfo<G: Guest<Self>>(guest: &mut G, target: RawFd, why: String) -> Error {
        let reason = format!(
            "the rendered fdinfo of descriptor {target}: {why}; Detcore cannot render it as \
             Linux prints it"
        );
        let sysno = Sysno::new(guest.regs().await.orig_rax as usize).unwrap_or(Sysno::read);
        Self::scratch_failure(guest, sysno, reason).await
    }

    /// The fdinfo text of descriptor `target` of the reader's table, now.
    async fn fdinfo_text<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target: RawFd,
    ) -> Result<Vec<u8>, Error> {
        let model = guest
            .thread_state()
            .with_detfd(target, |detfd| DescriptorModel {
                ty: detfd.ty(),
                nonblocking: detfd.is_nonblocking(),
                status_flags: detfd.status_flags(),
                stat: detfd.stat(),
                open_file: detfd.open_file_id(),
                resource: detfd.resource(),
                facts: detfd.fdinfo_facts(),
                timerfd: detfd.virtual_timerfd(),
                pidfd_target: detfd.pidfd_target(),
                rendered: detfd.procfs_rendered_path(),
                procfs_position: detfd.procfs_position().map(|(offset, _)| offset),
                directory_position: detfd
                    .has_directory_stream()
                    .then(|| detfd.directory_stream_position()),
                placeholder: detfd.placeholder().is_some(),
            })
            .ok();
        // A descriptor Detcore keeps no description of (a received anonymous
        // inode): no flags, type or identity to render. Refused by name.
        let Some(model) = model else {
            return Err(Self::refuse_fdinfo(
                guest,
                target,
                "Detcore keeps no description of the descriptor (a descriptor received over \
                 SCM_RIGHTS that it did not record), so it has no flags, type or identity to \
                 print"
                    .to_owned(),
            )
            .await);
        };
        // What the backend reports that no call reads back. `EBADF`: the
        // descriptor is not open after all, as `seq_show` finds it.
        let status = match guest.descriptor_status(target) {
            Ok(status) => Some(status),
            Err(reverie::FactError::Errno(Errno::EBADF)) => return Err(Errno::ENOENT.into()),
            Err(reverie::FactError::NotProvided) => None,
            Err(error) => {
                let reason = format!(
                    "the backend's descriptor_status of descriptor {target}, which its fdinfo \
                     prints, failed: {error:?}"
                );
                let sysno = Sysno::new(guest.regs().await.orig_rax as usize).unwrap_or(Sysno::read);
                return Err(Self::scratch_failure(guest, sysno, reason).await);
            }
        };
        let placeholder = if model.placeholder {
            Box::pin(self.placeholder_fdinfo(guest, target)).await?
        } else {
            None
        };
        // The position, as the guest's own `lseek(fd, 0, SEEK_CUR)` answers
        // it: Detcore's for a file it serves from a snapshot, a directory it
        // serves on a placeholder, or one it lists from a stream; the
        // kernel's for every other file.
        let pos = if let Some(offset) = model.procfs_position {
            offset as i64
        } else if let Some((text, _)) = &placeholder {
            fdinfo_position(text).unwrap_or(0)
        } else if let Some(position) = model.directory_position {
            position as i64
        } else {
            guest
                .inject(
                    syscalls::Lseek::new()
                        .with_fd(target)
                        .with_offset(0)
                        .with_whence(syscalls::Whence::SEEK_CUR),
                )
                .await
                .unwrap_or(0)
        };
        // The status flags, as the guest's own `F_GETFL` answers them, with
        // `O_CLOEXEC` from the descriptor slot.
        let status_flags = if placeholder.is_some() {
            i64::from(model.status_flags & !libc::O_CLOEXEC)
        } else {
            let physical = guest
                .inject(
                    syscalls::Fcntl::new()
                        .with_fd(target)
                        .with_cmd(syscalls::FcntlCmd::F_GETFL),
                )
                .await?;
            super::guest_status_flags(
                physical,
                model.nonblocking,
                model.rendered.is_some() && model.status_flags & libc::O_NOFOLLOW != 0,
            )
        };
        // `O_CLOEXEC` as the descriptor table holds it (`close_on_exec`), which
        // the guest's own `F_GETFD` reads.
        let descriptor_flags = guest
            .inject(
                syscalls::Fcntl::new()
                    .with_fd(target)
                    .with_cmd(syscalls::FcntlCmd::F_GETFD),
            )
            .await?;
        let cloexec = descriptor_flags & i64::from(libc::FD_CLOEXEC) != 0;
        let flags = status_flags as u32 | if cloexec { libc::O_CLOEXEC as u32 } else { 0 };
        // The mount, as the guest's own `statx` numbers it, and 0 where that
        // reports none: KVM's captured standard output and error under
        // `--verify` (pipes its executor keeps itself) report no mount.
        let raw_mount_id = if model.rendered.is_some() || placeholder.is_some() {
            Some(super::rendered_procfs::guest_procfs_mount_id()?)
        } else {
            Self::inject_statx_mount_id(guest, target).await?
        };
        let mnt_id = self.fdinfo_mount_id(guest, raw_mount_id).await;
        // The inode, as the descriptor's `fstat` numbers it, and what the
        // object is, which its type lines are keyed on (`file_kind`): from the
        // descriptor's own `fstat` where Detcore's model has no stat of it, or
        // one that need not be its own: the container's standard streams
        // share the one stat `setup_stdio` took of the supervisor's standard
        // input, whatever each is.
        let stat = match model.stat {
            Some(stat) if !super::is_container_stream(model.resource.clone()) => stat,
            _ => DetStat::from(self.inject_fstat(guest, target).await?),
        };
        let ino = self
            .fdinfo_inode(
                guest,
                target,
                &stat,
                model.resource.clone(),
                model.rendered,
                placeholder.as_ref().map(|(_, inode)| *inode),
            )
            .await
            .as_raw();
        // What the file's own `show_fdinfo` adds, or why Detcore cannot
        // render it.
        let kind = match self
            .fdinfo_kind(guest, &model, &stat, status.as_ref())
            .await
        {
            Ok(kind) => kind,
            Err(why) => return Err(Self::refuse_fdinfo(guest, target, why).await),
        };
        // The locks held through the description, with their owners, unless a
        // line would show a guest's record lock the backend holds as an OFD
        // lock.
        let mut locks = Vec::new();
        // The flock Detcore's model holds through the description, which no
        // host lock mirrors here: first, as Linux lists a description's flock
        // locks before its record locks and leases (`show_fd_locks`).
        if let Some(identity) = guest
            .thread_state()
            .with_detfd(target, |detfd| detfd.flock_identity())
            .ok()
            .flatten()
            .filter(|identity| identity.modelled)
            && let Some((write, owner)) =
                crate::tool_global::flock_held_by(guest, model.open_file).await
        {
            locks.push(FdinfoLock {
                class: reverie::LockClass::Flock,
                mandatory: false,
                write,
                owner,
                device: Self::lock_line_device(guest, identity.dev).await,
                inode: ino,
                start: 0,
                end: None,
            });
        }
        if let Some(status) = &status
            && !status.locks.is_empty()
        {
            let held = super::held_locks_of_status(status);
            let owners = match crate::tool_global::lock_owners(guest, model.open_file, held).await {
                Ok(owners) => owners,
                Err(why) => return Err(Self::refuse_fdinfo(guest, target, why).await),
            };
            for (lock, owner) in status.locks.iter().zip(owners) {
                locks.push(FdinfoLock {
                    class: lock.class,
                    mandatory: lock.mandatory,
                    write: lock.write,
                    owner,
                    device: Self::lock_line_device(guest, lock.raw_device).await,
                    inode: ino,
                    start: lock.start,
                    end: lock.end,
                });
            }
        }
        Ok(crate::procfs::fdinfo::render(&Fdinfo {
            pos,
            flags,
            mnt_id,
            ino,
            locks,
            kind,
        }))
    }

    /// The lines the file's own `show_fdinfo` adds, from the descriptor's
    /// model and the backend's fact (`status`, where it reports one), or why
    /// Detcore cannot render them, for which the read is refused by name (see
    /// the module comment).
    async fn fdinfo_kind<G: Guest<Self>>(
        &self,
        guest: &mut G,
        model: &DescriptorModel,
        stat: &DetStat,
        status: Option<&reverie::DescriptorStatus>,
    ) -> Result<FdinfoKind, String> {
        /// Why a descriptor's facts are missing.
        const UNRECORDED: &str = "(inherited, received over SCM_RIGHTS without joining the \
                                  description its sender's table modeled, or rediscovered \
                                  after an exec)";
        Ok(match model.ty {
            FdType::Eventfd => {
                match (model.facts, status.and_then(|status| status.eventfd_count)) {
                    (Some(FdinfoFacts::Eventfd { semaphore }), Some(count)) => {
                        FdinfoKind::Eventfd {
                            count,
                            id: model.open_file.deterministic_socket_cookie(),
                            semaphore,
                        }
                    }
                    (None, _) => {
                        return Err(format!(
                            "an eventfd whose flags Detcore did not record {UNRECORDED}"
                        ));
                    }
                    _ => {
                        return Err(
                            "an eventfd whose count the backend's fact did not report".to_owned()
                        );
                    }
                }
            }
            FdType::Signalfd => match model.facts {
                Some(FdinfoFacts::Signalfd { mask }) => FdinfoKind::Signalfd { mask },
                _ => {
                    return Err(format!(
                        "a signalfd whose mask Detcore did not record {UNRECORDED}"
                    ));
                }
            },
            FdType::Timerfd => {
                let Some(FdinfoFacts::Timerfd {
                    clockid,
                    settime_flags,
                    linux,
                }) = model.facts
                else {
                    return Err(format!(
                        "a timerfd whose clock and settings Detcore did not record {UNRECORDED}"
                    ));
                };
                if !crate::syscalls::timerfd::enabled(guest) {
                    return Err("a timerfd whose timer is the host's: Detcore's virtual \
                                timerfd is off in a run that is not strict"
                        .to_owned());
                }
                let (ticks, value_ns, interval_ns) = match model.timerfd {
                    Some(timer) => {
                        let now = crate::syscalls::timerfd::timerfd_now(guest).await;
                        let (ticks, value_ns) = linux.view(now);
                        (ticks, value_ns, timer.interval_ns)
                    }
                    None => (0, 0, 0),
                };
                FdinfoKind::Timerfd(TimerfdLines {
                    clockid,
                    ticks,
                    settime_flags,
                    value_ns,
                    interval_ns,
                })
            }
            FdType::Pidfd => {
                let Some(target) = model.pidfd_target else {
                    return Err(format!(
                        "a pidfd whose process Detcore did not record {UNRECORDED}"
                    ));
                };
                let task = BoundTask {
                    process: target.as_raw(),
                    thread: None,
                };
                let exists = crate::tool_global::procfs_task(guest, task).await.exists;
                FdinfoKind::Pidfd {
                    pid: if exists {
                        i64::from(target.as_raw())
                    } else {
                        -1
                    },
                }
            }
            FdType::Epoll => {
                return Err(
                    "an epoll instance, whose tfd: lines name the host's descriptors \
                            and inodes of its watched files"
                        .to_owned(),
                );
            }
            FdType::Inotify => {
                return Err(
                    "an inotify instance, whose watch lines name the host's inodes and \
                            devices"
                        .to_owned(),
                );
            }
            FdType::Userfaultfd => {
                return Err("a userfaultfd, whose lines Detcore does not model".to_owned());
            }
            FdType::Socket | FdType::Regular | FdType::Pipe | FdType::Memfd | FdType::Rng => {
                file_kind(model.ty, stat, status)?
            }
        })
    }
}

/// The lines a file's own `show_fdinfo` adds for a descriptor Detcore models
/// as a socket, a pipe, a memfd, a random device or a regular file, keyed on
/// what the object is rather than on Detcore's type for the descriptor (an
/// inherited standard stream is modelled as a regular file whatever it is),
/// or why Detcore cannot render them, for which the read is refused by name.
///
/// - A Unix socket's queued descriptors, wherever the backend's fact reports
///   them: it reports `scm_fds` for a Unix socket only, as Linux prints it
///   for every one (`unix_show_fdinfo`, net/unix/af_unix.c).
/// - A socket the fact does not describe (the backend declined it): whether
///   it is a Unix socket, whose fdinfo prints its queued descriptors, is
///   unknown, so the read is refused, as an eventfd without its count is.
///   One the fact describes without `scm_fds` is another kind of socket,
///   whose fdinfo has no lines of its own.
/// - An anonymous inode (no file type in its mode) Detcore does not classify,
///   which may have lines of its own: refused.
/// - A character device whose driver prints lines of its own
///   (`show_fdinfo`): a pseudo-terminal master's `tty-index`
///   (`pty_show_fdinfo`, drivers/tty/pty.c; the ptmx device, 5:2), a tun
///   device's `iff` (10:200) and a DRM device's (major 226): refused, as a
///   file whose lines Detcore cannot render.
fn file_kind(
    ty: FdType,
    stat: &DetStat,
    status: Option<&reverie::DescriptorStatus>,
) -> Result<FdinfoKind, String> {
    if let Some(scm_fds) = status.and_then(|status| status.scm_fds) {
        return Ok(FdinfoKind::UnixSocket { scm_fds });
    }
    let file_type = stat
        .mask
        .contains(StatxMask::STATX_TYPE)
        .then_some(stat.mode & libc::S_IFMT);
    if ty == FdType::Socket || file_type == Some(libc::S_IFSOCK) {
        return match status {
            Some(_) => Ok(FdinfoKind::Plain),
            None => Err(
                "a socket the backend's descriptor_status fact does not describe, \
                         whose fdinfo would print its queued descriptors (scm_fds) if it is a \
                         Unix socket"
                    .to_owned(),
            ),
        };
    }
    if file_type == Some(0) {
        return Err(
            "an anonymous inode Detcore does not classify, which may print lines of \
                    its own"
                .to_owned(),
        );
    }
    if file_type == Some(libc::S_IFCHR) && driver_prints_fdinfo(stat.rdev) {
        return Err(format!(
            "a character device {}:{} whose driver prints lines of its own (a pseudo-terminal \
             master's tty-index, a tun device's iff, a DRM device's), which Detcore does not \
             render",
            libc::major(stat.rdev),
            libc::minor(stat.rdev)
        ));
    }
    Ok(FdinfoKind::Plain)
}

/// Whether the character device `rdev` is one whose Linux v7.1.3 driver
/// prints fdinfo lines of its own: the ptmx device (5:2, a pseudo-terminal
/// master), the tun device (10:200) and the DRM devices (major 226).
fn driver_prints_fdinfo(rdev: u64) -> bool {
    let (major, minor) = (libc::major(rdev), libc::minor(rdev));
    (major, minor) == (5, 2) || (major, minor) == (10, 200) || major == 226
}

impl<T: RecordOrReplay> Detcore<T> {
    /// The device a lock line shows for the superblock device `raw_device`
    /// the backend reported, as the guest's `mountinfo` shows that file
    /// system's device: through a proven mountinfo-to-metadata rewrite
    /// (`Config::mountinfo_device_rewrites`), then the run's device pool where
    /// metadata is virtualized, as `mountinfo` and `stat` number it.
    async fn lock_line_device<G: Guest<Self>>(guest: &mut G, raw_device: u64) -> u64 {
        let device = guest
            .config()
            .mountinfo_device_rewrites
            .iter()
            .find(|(mountinfo, _)| *mountinfo == raw_device)
            .map_or(raw_device, |(_, metadata)| *metadata);
        if guest.config().virtualize_metadata {
            crate::tool_global::determinize_device(guest, device).await
        } else {
            device
        }
    }

    /// The mount ID the backend reports for descriptor `fd`, from an injected
    /// `statx(fd, "", AT_EMPTY_PATH, STATX_MNT_ID)`, as Linux's fdinfo
    /// prints it (`real_mount(file->f_path.mnt)->mnt_id`, the mount `statx`
    /// reports): `None` where the call did not report one. The empty path
    /// and the buffer are on the guest's stack scratch.
    async fn inject_statx_mount_id<G: Guest<Self>>(
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<u64>, Error> {
        let mut stack = guest.stack().await;
        let path = stack.push(c_string::<8>(b""));
        let statx = stack.reserve::<reverie::syscalls::linux_abi::Statx>();
        let stack_guard = stack.commit()?;
        let result = guest
            .inject(
                syscalls::Statx::new()
                    .with_dirfd(fd)
                    .with_path(PathPtr::from_ptr(path.as_raw() as *const libc::c_char))
                    .with_flags(AtFlags::AT_EMPTY_PATH)
                    .with_mask(StatxMask::STATX_MNT_ID)
                    .with_statx(Some(reverie::syscalls::StatxPtr(statx))),
            )
            .await;
        let answer = match result {
            Ok(_) => {
                let value: reverie::syscalls::linux_abi::Statx =
                    guest.memory().read_value(statx)?;
                (value.stx_mask & reverie::syscalls::linux_abi::STATX_MNT_ID != 0)
                    .then_some(value.stx_mnt_id)
            }
            Err(_) => None,
        };
        drop(stack_guard);
        Ok(answer)
    }
}

#[cfg(test)]
mod file_kind_tests {
    use super::*;

    fn stat(mode: u32, rdev: u64) -> DetStat {
        DetStat {
            mask: StatxMask::STATX_TYPE,
            mode,
            rdev,
            ..DetStat::default()
        }
    }

    fn status(fdinfo: &str) -> reverie::DescriptorStatus {
        reverie::DescriptorStatus::from_fdinfo(fdinfo.as_bytes()).unwrap()
    }

    /// A socket's, a device's or a plain file's fdinfo lines follow what the
    /// object is, not Detcore's type for the descriptor: a Unix socket's
    /// queued descriptors wherever the fact reports them (an inherited
    /// standard stream is a regular file to Detcore); no lines for another
    /// socket the fact describes; a named refusal for a socket the fact
    /// declines, for an anonymous inode, and for a character device whose
    /// driver prints lines of its own (a pseudo-terminal master, tun, DRM),
    /// which Detcore cannot render; no lines for `/dev/null` or a regular
    /// file.
    #[test]
    fn a_files_lines_follow_what_the_object_is() {
        let unix = status("pos:\t0\nflags:\t02\nmnt_id:\t9\nino:\t5\nscm_fds: 2\n");
        let inet = status("pos:\t0\nflags:\t02\nmnt_id:\t9\nino:\t5\n");
        let socket = stat(libc::S_IFSOCK | 0o777, 0);
        assert_eq!(
            file_kind(FdType::Regular, &socket, Some(&unix)),
            Ok(FdinfoKind::UnixSocket { scm_fds: 2 })
        );
        assert_eq!(
            file_kind(FdType::Socket, &socket, Some(&unix)),
            Ok(FdinfoKind::UnixSocket { scm_fds: 2 })
        );
        assert_eq!(
            file_kind(FdType::Socket, &socket, Some(&inet)),
            Ok(FdinfoKind::Plain)
        );
        for ty in [FdType::Regular, FdType::Socket] {
            let refusal = file_kind(ty, &socket, None).unwrap_err();
            assert!(refusal.contains("a socket"), "{refusal}");
        }
        let refusal = file_kind(FdType::Regular, &stat(0, 0), Some(&inet)).unwrap_err();
        assert!(refusal.contains("an anonymous inode"), "{refusal}");
        let device = |major, minor| stat(libc::S_IFCHR | 0o666, libc::makedev(major, minor));
        for (major, minor) in [(5, 2), (10, 200), (226, 0), (226, 128)] {
            let refusal = file_kind(FdType::Regular, &device(major, minor), Some(&inet))
                .expect_err("a driver that prints lines of its own is refused");
            assert!(
                refusal.contains(&format!("a character device {major}:{minor} ")),
                "{refusal}"
            );
        }
        assert_eq!(
            file_kind(FdType::Regular, &device(1, 3), None),
            Ok(FdinfoKind::Plain)
        );
        assert_eq!(
            file_kind(
                FdType::Regular,
                &stat(libc::S_IFREG | 0o644, 0),
                Some(&inet)
            ),
            Ok(FdinfoKind::Plain)
        );
    }
}
