/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A directory's virtual mtime follows the names created in, removed from and
//! renamed into or out of it, and a new object's mtime is its creation time
//! (<https://github.com/facebookexperimental/hermit/issues/433>).
//!
//! Linux sets a directory's mtime and ctime to the current time whenever a
//! name is added to it or removed from it, and gives a new inode its creation
//! time. On tmpfs (mm/shmem.c) that is `shmem_mknod` (which also creates
//! regular files and directories), `shmem_symlink`, `shmem_link`,
//! `shmem_unlink` and `shmem_rmdir`, and for a rename `shmem_rename2`, through
//! `simple_rename_timestamp` (fs/libfs.c), for the old directory and the new
//! one, an exchange included; ext4 does the same through the callers of
//! `ext4_add_entry` and `ext4_delete_entry`. Two things change no directory:
//! a rename between two names of one file, which `vfs_rename` (fs/namei.c)
//! returns from at once (`source == target`), and an `O_TMPFILE` open, whose
//! file has no name (`shmem_tmpfile`, `ext4_tmpfile`).
//!
//! Detcore's virtual mtime otherwise moves only on writes (`touch_file`) and
//! on `utimensat` (`set_file_mtime`). A directory kept the mtime it was first
//! seen with, or the one `tar` restored, and a new file or directory read the
//! epoch, so a build that archives a directory it changed with
//! `tar --clamp-mtime --mtime=@SOURCE_DATE_EPOCH` recorded another mtime than
//! Linux gives it (Debian's apg 2.2.3.dfsg.1-7).
//!
//! After an entry-changing call succeeds, in the same turn, Detcore looks up
//! the object it created and each directory whose entries it changed with an
//! `fstatat` it injects, and sets their virtual mtimes to the virtual now
//! (`touch_file`): the object first, then its directories, the order in which
//! Linux stamps them. A directory is looked up by the guest's spelling of its
//! name without the last component, from the same directory descriptor; that
//! spelling is staged where Detcore stages the paths it respells
//! (`Detcore::stage_translated`), and the `struct stat` where its other
//! lookups put theirs (`Detcore::stat_with_fallback`), so neither maps a page
//! on the common path. An `O_CREAT` open is decided in `handle_openat`, in the
//! open's turn: it creates its file where the name did not resolve just before
//! the open ran (`Detcore::open_creates`).
//!
//! Only with virtualized metadata, the only mode with virtual mtimes, and
//! sequentialized threads: otherwise another guest thread could rename a
//! directory between the call and the lookup, as for the `utimensat` lookups
//! and inode retirement (`Detcore::last_names_removed_by`). `touch_file`
//! stamps the logical clock's value, or host realtime where time is not
//! virtualized, as for writes; the CLI refuses virtualized metadata without
//! virtualized time, so only a hand-built `Config` stamps host time. Record and
//! replay keep the host's mtimes (they run without virtualized metadata); the
//! lookups still go through `Detcore::record_or_replay`, as `utimensat`'s do.
//!
//! Not modelled:
//! - ctime. Detcore reports every file's ctime as the epoch
//!   (`determinize_stat`), a written file's too, so a directory's stays there
//!   as well, and so does the ctime of a renamed object and of a linked or
//!   unlinked one, whose link count changed.
//! - An `O_CREAT` open through a dangling symbolic link, which creates the
//!   link's target in the target's directory: the name resolved before the
//!   open, so no directory is touched and the new file reads its first-seen
//!   mtime.
//! - A name whose spelling passes through the name the call removed
//!   (`rmdir("d/../d")`): its directory's spelling no longer resolves.
//! - Names an AF_UNIX `bind` creates, and the whiteout `RENAME_WHITEOUT`
//!   leaves (its directory is touched; the whiteout reads its first-seen
//!   mtime).
//! - `--passthru-opt`, under which Detcore sees no `mkdir`, `mknod`,
//!   `symlink`, `link`, `unlink`, `rmdir` or `rename`, in any form (none is on
//!   its allow-list), so only a creating open touches its directory.
//! - An object or directory whose injected lookup fails, which keeps its
//!   mtime, as for the `utimensat` and inode-retirement lookups.
//! - A file an open created that Detcore then refuses, closing the descriptor:
//!   neither the file nor its directory is touched.

use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use nix::fcntl::AtFlags;
use nix::fcntl::OFlag;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::Errno;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::StatPtr;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use super::StatFailure;
use crate::record_or_replay::RecordOrReplay;
use crate::tool_global::forget_retired_inode;
use crate::tool_global::touch_file;
use crate::tool_local::Detcore;
use crate::types::RawInode;

/// A name a call changes: the directory descriptor it is looked up from and
/// the guest's spelling of it.
#[derive(Clone, Copy)]
struct Name<'a> {
    dirfd: RawFd,
    path: PathPtr<'a>,
}

/// The name `path` from `dirfd`, or `None` for a NULL path.
fn name(dirfd: RawFd, path: Option<PathPtr<'_>>) -> Option<Name<'_>> {
    path.map(|path| Name { dirfd, path })
}

/// What a successful call changes among the names of the file namespace.
enum EntryChange<'a> {
    /// A new object at the name (`mkdir`, `mknod`, `symlink`): its mtime and
    /// its directory's.
    Created(Name<'a>),
    /// A name added (`link`) or removed (`unlink`, `rmdir`): its directory's.
    Changed(Name<'a>),
    /// A rename, with its `renameat2` flags: both directories', unless it
    /// changed nothing.
    Renamed {
        old: Name<'a>,
        new: Name<'a>,
        flags: u32,
    },
}

/// What a successful `call` changes among names ([`EntryChange`]), or `None`
/// for a call that changes none, and for one with a NULL path, which cannot
/// succeed.
fn entry_change(call: &Syscall) -> Option<EntryChange<'_>> {
    #[cfg(not(target_arch = "aarch64"))]
    let at = libc::AT_FDCWD;
    Some(match call {
        Syscall::Mkdirat(c) => EntryChange::Created(name(c.dirfd(), c.path())?),
        Syscall::Mknodat(c) => EntryChange::Created(name(c.dirfd(), c.path())?),
        Syscall::Symlinkat(c) => EntryChange::Created(name(c.newdirfd(), c.linkpath())?),
        Syscall::Linkat(c) => EntryChange::Changed(name(c.newdirfd(), c.newpath())?),
        Syscall::Unlinkat(c) => EntryChange::Changed(name(c.dirfd(), c.path())?),
        Syscall::Renameat(c) => EntryChange::Renamed {
            old: name(c.olddirfd(), c.oldpath())?,
            new: name(c.newdirfd(), c.newpath())?,
            flags: 0,
        },
        Syscall::Renameat2(c) => EntryChange::Renamed {
            old: name(c.olddirfd(), c.oldpath())?,
            new: name(c.newdirfd(), c.newpath())?,
            flags: c.flags(),
        },
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Mkdir(c) => EntryChange::Created(name(at, c.path())?),
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Mknod(c) => EntryChange::Created(name(at, c.path())?),
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Symlink(c) => EntryChange::Created(name(at, c.linkpath())?),
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Link(c) => EntryChange::Changed(name(at, c.newpath())?),
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Unlink(c) => EntryChange::Changed(name(at, c.path())?),
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Rmdir(c) => EntryChange::Changed(name(at, c.path())?),
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Rename(c) => EntryChange::Renamed {
            old: name(at, c.oldpath())?,
            new: name(at, c.newpath())?,
            flags: 0,
        },
        _ => return None,
    })
}

/// Whether a call numbered `sysno` may change names ([`entry_change`],
/// whatever its arguments), so that every other call skips the hook by its
/// number alone. `open`, `openat` and `creat` are decided in `handle_openat`.
pub(crate) fn changes_entries(sysno: Sysno) -> bool {
    match sysno {
        #[cfg(not(target_arch = "aarch64"))]
        Sysno::mkdir
        | Sysno::mknod
        | Sysno::symlink
        | Sysno::link
        | Sysno::unlink
        | Sysno::rmdir
        | Sysno::rename => true,
        Sysno::mkdirat
        | Sysno::mknodat
        | Sysno::symlinkat
        | Sysno::linkat
        | Sysno::unlinkat
        | Sysno::renameat
        | Sysno::renameat2 => true,
        _ => false,
    }
}

/// The directory that holds `path`'s last component, spelled from the same
/// directory descriptor: `.` for a single name, `/` for a name in the root,
/// otherwise `path` up to its last component, trailing slashes ignored. `None`
/// for an empty path, which names no entry.
fn directory_spelling(path: &[u8]) -> Option<&[u8]> {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    let trimmed = &path[..end];
    if trimmed.is_empty() {
        return None;
    }
    Some(match trimmed.iter().rposition(|byte| *byte == b'/') {
        None => b".",
        Some(0) => b"/",
        Some(slash) => &trimmed[..slash],
    })
}

/// The directory descriptor and spelling of the directory that holds `name`'s
/// last component ([`directory_spelling`]), read from the guest's memory, or
/// `None` where the spelling cannot be read or is empty.
fn directory_of<T, G>(guest: &G, name: Name<'_>) -> Option<(RawFd, Vec<u8>)>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let spelled: PathBuf = name.path.read(&guest.memory()).ok()?;
    let directory = directory_spelling(spelled.as_os_str().as_bytes())?;
    Some((name.dirfd, directory.to_vec()))
}

/// How an open about to run creates its file, if it does
/// ([`Detcore::open_creates`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenCreates {
    /// A file at the name the guest spelled: its mtime and its directory's.
    Named,
    /// An `O_TMPFILE` file, which has no name: its mtime only.
    Unnamed,
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Whether this run keeps directory and new-object mtimes as Linux does
    /// (see the module documentation).
    fn models_entry_mtimes<G: Guest<Self>>(guest: &G) -> bool {
        let config = guest.config();
        config.virtualize_metadata && config.sequentialize_threads
    }

    /// After `call` succeeded, in its turn: the object it created, then each
    /// directory whose entries it changed, take the virtual now as their mtime
    /// (see the module documentation). A lookup that fails touches nothing.
    ///
    /// The post-call hook awaits this unboxed. Without the model, or for a
    /// call that changes no name, it returns before it allocates or looks
    /// anything up; only an admitted change's lookups and touches are boxed
    /// ([`Self::touch_entry_change`]), so that the hook's future carries a
    /// few words for them (`hot_path_futures_stay_small`).
    pub(crate) async fn touch_entries_changed_by<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) {
        if !Self::models_entry_mtimes(guest) {
            return;
        }
        let Some(change) = entry_change(call) else {
            return;
        };
        Box::pin(self.touch_entry_change(guest, change)).await;
    }

    /// The lookups and touches [`Self::touch_entries_changed_by`] makes for
    /// the change `change`, once the model and the call are admitted.
    async fn touch_entry_change<G: Guest<Self>>(&self, guest: &mut G, change: EntryChange<'_>) {
        let names = match change {
            EntryChange::Created(name) => {
                if let Ok(object) = self
                    .lookup_name(guest, name, AtFlags::AT_SYMLINK_NOFOLLOW)
                    .await
                {
                    Self::touch_new_object(guest, object).await;
                }
                [Some(name), None]
            }
            EntryChange::Changed(name) => [Some(name), None],
            EntryChange::Renamed { old, new, flags } => {
                if self.rename_changed_nothing(guest, old, new, flags).await {
                    return;
                }
                [Some(old), Some(new)]
            }
        };
        // A rename within one directory touches it once: looked up once when
        // both names spell it alike, and touched once when two spellings
        // reach it.
        let mut spelled: Vec<(RawFd, Vec<u8>)> = Vec::with_capacity(2);
        for name in names.into_iter().flatten() {
            if let Some(directory) = directory_of(guest, name)
                && !spelled.contains(&directory)
            {
                spelled.push(directory);
            }
        }
        let mut touched: Vec<RawInode> = Vec::with_capacity(2);
        for (dirfd, spelling) in spelled {
            if let Some(directory) = self.lookup_spelled(guest, dirfd, spelling).await
                && !touched.contains(&directory)
            {
                touch_file(guest, directory).await;
                touched.push(directory);
            }
        }
    }

    /// Whether a rename that succeeded changed nothing, as Linux renames
    /// nothing between two names of one file (`vfs_rename`), with the flags
    /// `flags`. Its old name then still names a file, where a rename that
    /// moved one has removed it; after an exchange both names hold the one
    /// file they held before. A rename that leaves a whiteout always moved its
    /// file, and a lookup that fails counts as a change.
    async fn rename_changed_nothing<G: Guest<Self>>(
        &self,
        guest: &mut G,
        old: Name<'_>,
        new: Name<'_>,
        flags: u32,
    ) -> bool {
        if flags & libc::RENAME_WHITEOUT != 0 {
            return false;
        }
        let Ok(at_old) = self
            .lookup_name(guest, old, AtFlags::AT_SYMLINK_NOFOLLOW)
            .await
        else {
            return false;
        };
        if flags & libc::RENAME_EXCHANGE == 0 {
            return true;
        }
        self.lookup_name(guest, new, AtFlags::AT_SYMLINK_NOFOLLOW)
            .await
            .is_ok_and(|at_new| at_new == at_old)
    }

    /// Whether the open `call`, about to run in this turn, creates its file:
    /// `O_TMPFILE`, and `O_CREAT` with `O_EXCL`, whenever they succeed;
    /// `O_CREAT` alone only where its name does not resolve now
    /// ([`Self::creates_unless_name_resolves`]). `O_PATH` ignores both flags,
    /// and creates nothing. `None` also where entry mtimes are not modelled
    /// ([`Self::models_entry_mtimes`]) or the lookup fails otherwise than with
    /// `ENOENT`.
    ///
    /// `handle_openat` awaits this unboxed for every open. The model and the
    /// flags decide every open but an `O_CREAT` one without `O_EXCL` before
    /// anything is allocated or looked up; only that open's lookup is boxed.
    pub(crate) async fn open_creates<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
    ) -> Option<OpenCreates> {
        let flags = call.flags();
        if flags.contains(OFlag::O_PATH) || !Self::models_entry_mtimes(guest) {
            return None;
        }
        if flags.contains(OFlag::O_TMPFILE) {
            return Some(OpenCreates::Unnamed);
        }
        if !flags.contains(OFlag::O_CREAT) {
            return None;
        }
        if flags.contains(OFlag::O_EXCL) {
            return Some(OpenCreates::Named);
        }
        let name = Name {
            dirfd: call.dirfd(),
            path: call.path()?,
        };
        Box::pin(self.creates_unless_name_resolves(guest, name)).await
    }

    /// Whether an `O_CREAT` open of `name` without `O_EXCL` creates its file
    /// ([`Self::open_creates`]): only where the name does not resolve now,
    /// which an injected `fstatat` that follows no last symbolic link tells (a
    /// dangling link is a name; see the module documentation). `None` where
    /// it resolves, or the lookup fails otherwise than with `ENOENT`.
    async fn creates_unless_name_resolves<G: Guest<Self>>(
        &self,
        guest: &mut G,
        name: Name<'_>,
    ) -> Option<OpenCreates> {
        match self
            .lookup_name(guest, name, AtFlags::AT_SYMLINK_NOFOLLOW)
            .await
        {
            Err(StatFailure::Call(Errno::ENOENT)) => Some(OpenCreates::Named),
            _ => None,
        }
    }

    /// After the open `call` created the file `object`, as `creates` says
    /// ([`Self::open_creates`]): its mtime, and for a named file its
    /// directory's, become the virtual now. `handle_openat` has already
    /// dropped any retired mapping of the file's inode
    /// (`forget_retired_inode`).
    pub(crate) async fn touch_created_by_open<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
        creates: OpenCreates,
        object: RawInode,
    ) {
        touch_file(guest, object).await;
        if creates == OpenCreates::Named
            && let Some(path) = call.path()
            && let Some((dirfd, spelling)) = directory_of(
                guest,
                Name {
                    dirfd: call.dirfd(),
                    path,
                },
            )
            && let Some(directory) = self.lookup_spelled(guest, dirfd, spelling).await
        {
            touch_file(guest, directory).await;
        }
    }

    /// The object a call just created takes the virtual now as its mtime. The
    /// host may have given it the inode of a file whose last name the guest
    /// removed; that file's retired mapping is dropped first, as a sighting
    /// of the inode by a name drops it (`InodePool::forget_retired`), or the
    /// mtime would go to that file.
    async fn touch_new_object<G: Guest<Self>>(guest: &mut G, object: RawInode) {
        forget_retired_inode(guest, object, None).await;
        touch_file(guest, object).await;
    }

    /// The host inode `name` resolves to, through an injected `fstatat` with
    /// `flags`, made through the recording (`Detcore::stat_with_fallback`).
    async fn lookup_name<G: Guest<Self>>(
        &self,
        guest: &mut G,
        name: Name<'_>,
        flags: AtFlags,
    ) -> Result<RawInode, StatFailure> {
        let Name { dirfd, path } = name;
        let lookup = move |statptr: StatPtr<'_>| {
            crate::arch::fstatat_syscall(
                syscalls::Fstatat::new()
                    .with_dirfd(dirfd)
                    .with_path(Some(path))
                    .with_stat(Some(statptr))
                    .with_flags(flags),
            )
        };
        Self::stat_with_fallback(guest, dirfd, lookup, Some(self))
            .await
            .map(|stat| RawInode::new(stat.st_dev, stat.st_ino))
    }

    /// The host inode of the directory `spelling` names from `dirfd`
    /// ([`directory_of`]), following a last symbolic link as the kernel
    /// follows every component of a path but the last. The spelling is staged
    /// as a respelled path is (`Detcore::stage_translated`). `None` if any
    /// step fails.
    async fn lookup_spelled<G: Guest<Self>>(
        &self,
        guest: &mut G,
        dirfd: RawFd,
        mut spelling: Vec<u8>,
    ) -> Option<RawInode> {
        spelling.push(0);
        let probe = crate::arch::fstatat_syscall(syscalls::Fstatat::new().with_dirfd(dirfd));
        let (sysno, args) = probe.into_parts();
        let values = [args.arg0, 0, 0, 0, 0, 0];
        let staged = Self::stage_translated(guest, sysno, values, vec![(1, spelling)], true)
            .await
            .ok()?;
        let path = PathPtr::from_ptr(staged.call.into_parts().1.arg1 as *const libc::c_char);
        let found = match path {
            Some(path) => self
                .lookup_name(guest, Name { dirfd, path }, AtFlags::empty())
                .await
                .ok(),
            None => None,
        };
        Self::finish_translated_call(guest, staged).await;
        found
    }
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::SyscallArgs;

    use super::*;
    use crate::record_or_replay::NoopTool;
    use crate::syscall_classification::all_pinned_syscalls;
    use crate::syscall_hot_path_tests::InjectGuest;
    use crate::syscall_hot_path_tests::detcore;

    /// A thread of a run with these two settings, the model's conditions,
    /// that records every call injected through it and has neither the
    /// registers nor the scratch stack a lookup needs (asking panics).
    fn thread(virtualize_metadata: bool, sequentialize_threads: bool) -> InjectGuest<NoopTool> {
        let mut guest = InjectGuest::<NoopTool>::new(Ok(0));
        guest.config.virtualize_metadata = virtualize_metadata;
        guest.config.sequentialize_threads = sequentialize_threads;
        guest
    }

    /// Without the model, neither admission gets as far as a lookup, so
    /// neither builds its boxed lookups: nothing is injected and the guest is
    /// never asked for registers or a scratch stack. With the model, the
    /// flags alone decide every open but an `O_CREAT` one without `O_EXCL`,
    /// and a NULL path changes no name, again with nothing injected.
    #[test]
    fn nothing_is_looked_up_before_the_model_and_the_call_are_admitted() {
        let tool = detcore::<NoopTool>();
        // Every argument a readable name, so that a lookup made in error
        // fails on the guest's refusals, not on a bad address.
        let spelled = std::ffi::CString::new("new").unwrap();
        let at = spelled.as_ptr() as usize;
        let args = SyscallArgs::new(at, at, at, at, at, at);
        let open = |flags: OFlag| {
            syscalls::Openat::new()
                .with_dirfd(libc::AT_FDCWD)
                .with_path(PathPtr::from_ptr(spelled.as_ptr()))
                .with_flags(flags)
        };
        futures::executor::block_on(async {
            for (metadata, sequential) in [(false, true), (true, false), (false, false)] {
                for sysno in all_pinned_syscalls().filter(|sysno| changes_entries(*sysno)) {
                    let mut guest = thread(metadata, sequential);
                    tool.touch_entries_changed_by(&mut guest, &Syscall::from_raw(sysno, args))
                        .await;
                    assert!(guest.injected().is_empty(), "{sysno}");
                }
                for flags in [
                    OFlag::O_CREAT | OFlag::O_WRONLY,
                    OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY,
                    OFlag::O_TMPFILE | OFlag::O_RDWR,
                ] {
                    let mut guest = thread(metadata, sequential);
                    assert_eq!(tool.open_creates(&mut guest, open(flags)).await, None);
                    assert!(guest.injected().is_empty(), "{flags:?}");
                }
            }
            for (flags, creates) in [
                (OFlag::O_TMPFILE | OFlag::O_RDWR, Some(OpenCreates::Unnamed)),
                (
                    OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY,
                    Some(OpenCreates::Named),
                ),
                (OFlag::O_CREAT | OFlag::O_PATH, None),
                (OFlag::O_RDONLY | OFlag::O_DIRECTORY, None),
                (OFlag::O_WRONLY | OFlag::O_TRUNC, None),
            ] {
                let mut guest = thread(true, true);
                assert_eq!(tool.open_creates(&mut guest, open(flags)).await, creates);
                assert!(guest.injected().is_empty(), "{flags:?}");
            }
            let mut guest = thread(true, true);
            let null = Syscall::from_raw(Sysno::unlinkat, SyscallArgs::new(3, 0, 0, 0, 0, 0));
            tool.touch_entries_changed_by(&mut guest, &null).await;
            assert!(guest.injected().is_empty());
        });
    }

    /// Each admission's own future holds its arguments and the box of its
    /// lookups, and so is smaller than the future it boxes: the futures that
    /// await it unboxed (the post-call hook's, which
    /// `hot_path_futures_stay_small` bounds, and `handle_openat`'s) carry it,
    /// not a lookup.
    #[test]
    fn each_admission_is_smaller_than_the_lookups_it_boxes() {
        let tool = detcore::<NoopTool>();
        let mut guest = thread(true, true);
        let call = Syscall::from_raw(Sysno::mkdirat, SyscallArgs::new(3, 0x1000, 0, 0, 0, 0));
        let spelled = std::ffi::CString::new("new").unwrap();
        let name = Name {
            dirfd: libc::AT_FDCWD,
            path: PathPtr::from_ptr(spelled.as_ptr()).unwrap(),
        };
        let entries = std::mem::size_of_val(&tool.touch_entries_changed_by(&mut guest, &call));
        let change = entry_change(&call).unwrap();
        let entries_boxed = std::mem::size_of_val(&tool.touch_entry_change(&mut guest, change));
        let open = std::mem::size_of_val(&tool.open_creates(&mut guest, syscalls::Openat::new()));
        let open_boxed =
            std::mem::size_of_val(&tool.creates_unless_name_resolves(&mut guest, name));
        eprintln!(
            "future size: touch_entries_changed_by = {entries} bytes (boxed: {entries_boxed}); \
             open_creates = {open} bytes (boxed: {open_boxed})"
        );
        assert!(
            entries < entries_boxed,
            "{entries} >= {entries_boxed} bytes"
        );
        assert!(open < open_boxed, "{open} >= {open_boxed} bytes");
    }

    #[test]
    fn a_directory_is_spelled_from_the_same_descriptor() {
        let spelled = |path: &str| directory_spelling(path.as_bytes()).map(<[u8]>::to_vec);
        assert_eq!(spelled("x"), Some(b".".to_vec()));
        assert_eq!(spelled("x/"), Some(b".".to_vec()));
        assert_eq!(spelled("d/x"), Some(b"d".to_vec()));
        assert_eq!(spelled("d/e/x/"), Some(b"d/e".to_vec()));
        assert_eq!(spelled("d//x"), Some(b"d/".to_vec()));
        assert_eq!(spelled("/x"), Some(b"/".to_vec()));
        assert_eq!(spelled("//x"), Some(b"/".to_vec()));
        assert_eq!(spelled("/a/b"), Some(b"/a".to_vec()));
        assert_eq!(spelled("/"), Some(b"/".to_vec()));
        assert_eq!(spelled(""), None);
    }

    /// The number-only prefilter ([`changes_entries`]) is exactly the set of
    /// calls [`entry_change`] acts on, for every syscall number, so that no
    /// entry-changing call skips the hook and no other call pays for it.
    #[test]
    fn the_prefilter_is_the_set_of_calls_that_change_names() {
        // Nonzero, so that every path is non-NULL; as `renameat2` flags it
        // asks for neither an exchange nor a whiteout.
        let args = SyscallArgs::new(0x1000, 0x1000, 0x1000, 0x1000, 0x1000, 0x1000);
        for sysno in all_pinned_syscalls() {
            let call = Syscall::from_raw(sysno, args);
            assert_eq!(
                entry_change(&call).is_some(),
                changes_entries(sysno),
                "{sysno}"
            );
        }
        for sysno in [
            Sysno::openat,
            Sysno::write,
            Sysno::utimensat,
            Sysno::newfstatat,
        ] {
            assert!(!changes_entries(sysno), "{sysno}");
        }
    }

    #[test]
    fn a_rename_names_both_directories_and_creations_name_their_object() {
        let args = SyscallArgs::new(0x1000, 0x2000, 0x3000, 0x4000, 0x6, 0);
        let renamed = Syscall::from_raw(Sysno::renameat2, args);
        match entry_change(&renamed) {
            Some(EntryChange::Renamed { old, new, flags }) => {
                assert_eq!(old.dirfd, 0x1000);
                assert_eq!(new.dirfd, 0x3000);
                assert_eq!(flags, 0x6);
            }
            _ => panic!("renameat2 must change two names"),
        }
        let made = Syscall::from_raw(Sysno::symlinkat, args);
        match entry_change(&made) {
            // symlinkat(target, newdirfd, linkpath): the link is the object.
            Some(EntryChange::Created(name)) => assert_eq!(name.dirfd, 0x2000),
            _ => panic!("symlinkat must create its link"),
        }
        let linked = Syscall::from_raw(Sysno::linkat, args);
        match entry_change(&linked) {
            Some(EntryChange::Changed(name)) => assert_eq!(name.dirfd, 0x3000),
            _ => panic!("linkat must add a name to its new directory"),
        }
        let null = Syscall::from_raw(Sysno::unlinkat, SyscallArgs::new(3, 0, 0, 0, 0, 0));
        assert!(entry_change(&null).is_none());
    }
}
