/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The caller's own descriptor directory, `/proc/<pid>/fd`, which Detcore
//! lists from its own descriptor table on every backend (package K slice F1,
//! toward <https://github.com/rrnewton/hermit/issues/4604>), and the parts of
//! Linux's `fs/proc/fd.c` it models: the entries `proc_readfd_common` emits,
//! their positions, and `generic_file_llseek` on such a directory.
//!
//! An open of the caller's own directory (`/proc/self/fd`,
//! `/proc/thread-self/fd`, `/proc/<own pid>/fd`,
//! `/proc/<own pid>/task/<own tid>/fd`) gets a placeholder descriptor (package
//! K slice P1): one on procfs's root where the guest's procfs is the kernel's
//! (ptrace), as slice F1 opened it, and elsewhere (KVM) one on the
//! placeholder directory, the empty read-only tmpfs hermit-cli mounts in the
//! guest's container (`DetConfig::directory_placeholder`). Detcore marks it
//! with the task the directory is bound to, as Linux binds it at the open
//! (`DetFd::placeholder`). A listing then comes from Detcore's own
//! table, read at each `getdents` from the descriptor's position, as Linux
//! reads the task's table at each call. An open of `/proc` itself gets such a
//! descriptor too ([`PlaceholderDirectory::Root`]), listed from Detcore's
//! procfs model (`procfs::listing`).
//!
//! The position of each entry is Linux's: `.` at 0, `..` at 1, descriptor
//! `N` at `N + 2`, and the end at the table's capacity plus 2
//! (`fget_task_next` walks to `max_fds`). Linux's capacity is its descriptor
//! table's allocation: 64 (`NR_OPEN_DEFAULT`) until a descriptor at or above
//! it is installed, then the next power of two above the highest one
//! (`expand_files`, `alloc_fdtable`), kept when descriptors close; a fork's
//! copy is sized from the descriptors open at the fork (`dup_fd`,
//! `sane_fdtable_size`). Detcore keeps that history for the tables it keeps
//! (`FileMetadata::descriptor_capacity`); for a table the kernel lists for it
//! (another task's), it takes the capacity of a table holding only the
//! descriptors listed ([`capacity`]), which can be lower than Linux's after a
//! high descriptor there was closed (named residual).

use std::ffi::OsStr;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use nix::errno::Errno;
use serde::Deserialize;
use serde::Serialize;

/// Whose descriptor table a descriptor directory lists.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize
)]
pub enum DescriptorDirectoryTable {
    /// The process's (`/proc/<pid>/fd`), which procfs reads from the thread
    /// group leader.
    Process,
    /// One thread's (`/proc/<pid>/task/<tid>/fd`, `/proc/thread-self/fd`).
    Thread,
}

/// The task a descriptor directory is bound to, as Linux binds the
/// directory's inode to a task at the open (`proc_pid_make_inode`): every
/// later listing and lookup below it reads that task's table, whoever holds
/// the descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescriptorDirectoryBinding {
    /// Whose table.
    pub table: DescriptorDirectoryTable,
    /// The process, as the guest numbers it.
    pub pid: i32,
    /// The thread, as the guest numbers it: the opener for a process
    /// directory, the directory's thread for a thread directory.
    pub tid: i32,
    /// How many tasks of the run had held the process number at the open
    /// (`InodePool::count_incarnation`), 0 where unknown.
    pub process_incarnation: u32,
    /// The same for the thread number of a thread directory.
    pub thread_incarnation: Option<u32>,
    /// The device that keys the directory's inode number and its entries'
    /// with their paths: the device of the guest's procfs as the backend
    /// reports it (`Detcore::procfs_key_device`), never the placeholder's,
    /// whose device is the host's choice.
    pub device: u64,
    /// The descriptor table the bound thread held at the open, for a
    /// thread's directory: another thread holding the same table reads it as
    /// its own.
    pub files: Option<detcore_model::fd::FilesId>,
}

impl DescriptorDirectoryBinding {
    /// The directory's path within procfs, as the guest numbers its task:
    /// `/<pid>/fd` or `/<pid>/task/<tid>/fd`.
    pub(crate) fn within(&self) -> PathBuf {
        match self.table {
            DescriptorDirectoryTable::Process => PathBuf::from(format!("/{}/fd", self.pid)),
            DescriptorDirectoryTable::Thread => {
                PathBuf::from(format!("/{}/task/{}/fd", self.pid, self.tid))
            }
        }
    }

    /// The directory's absolute guest path, which is what Linux's
    /// `/proc/self/fd/N` link to it reads (`/proc/<pid>/fd`).
    pub(crate) fn canonical(&self) -> PathBuf {
        let within = self.within();
        Path::new("/proc").join(within.strip_prefix("/").unwrap_or(&within))
    }

    /// The absolute spelling of the directory that a name relative to its
    /// descriptor is looked up below, for a caller whose pid and tid, as the
    /// guest numbers them, are `caller`: through `self` or `thread-self` where
    /// that is the bound task (which every backend's `/proc` resolves for the
    /// caller), and by number for another process's or thread's directory.
    pub(crate) fn lookup_base(&self, caller: (i32, i32)) -> PathBuf {
        match self.table {
            DescriptorDirectoryTable::Process if caller.0 == self.pid => {
                PathBuf::from("/proc/self/fd")
            }
            DescriptorDirectoryTable::Thread if caller.1 == self.tid => {
                PathBuf::from("/proc/thread-self/fd")
            }
            _ => self.canonical(),
        }
    }
}

/// A `/proc` directory Detcore serves on a placeholder descriptor (package K
/// slice P1): every call through the descriptor is answered from Detcore's
/// model, and a name looked up below it reaches what Linux reaches below the
/// directory: below procfs's root, the descriptor's object on ptrace, by the
/// directory's path within it; at the directory's absolute spelling where
/// the object is the placeholder directory (KVM).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaceholderDirectory {
    /// `/proc` itself, procfs's root, the same directory for every task.
    Root,
    /// A task's descriptor directory, bound to that task at the open.
    Descriptors(DescriptorDirectoryBinding),
}

impl PlaceholderDirectory {
    /// The directory's absolute guest path: `/proc`, or the descriptor
    /// directory's (`/proc/<pid>/fd`), which is what Linux's
    /// `/proc/self/fd/N` link to it reads.
    pub(crate) fn canonical(&self) -> PathBuf {
        match self {
            Self::Root => PathBuf::from("/proc"),
            Self::Descriptors(binding) => binding.canonical(),
        }
    }

    /// The absolute spelling a name relative to the directory's descriptor is
    /// looked up below, for a caller whose pid and tid are `caller`
    /// ([`DescriptorDirectoryBinding::lookup_base`]).
    pub(crate) fn lookup_base(&self, caller: (i32, i32)) -> PathBuf {
        match self {
            Self::Root => PathBuf::from("/proc"),
            Self::Descriptors(binding) => binding.lookup_base(caller),
        }
    }

    /// Which directory, as Linux tells its inodes apart for `flock`: procfs's
    /// one root, or the descriptor directory of one task incarnation (a
    /// thread's own, by its thread).
    pub(crate) fn lock_key(&self) -> PlaceholderLockKey {
        match self {
            Self::Root => PlaceholderLockKey::Root,
            Self::Descriptors(binding) => PlaceholderLockKey::Descriptors {
                table: binding.table,
                pid: binding.pid,
                process_incarnation: binding.process_incarnation,
                thread: (binding.table == DescriptorDirectoryTable::Thread)
                    .then_some((binding.tid, binding.thread_incarnation)),
            },
        }
    }
}

/// The directory a `flock` through a placeholder descriptor locks
/// ([`PlaceholderDirectory::lock_key`]): Linux keeps one lock list per inode,
/// procfs's root has one inode, and each task's descriptor directory one of
/// its own.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize
)]
pub enum PlaceholderLockKey {
    /// `/proc`.
    Root,
    /// A task's descriptor directory.
    Descriptors {
        /// Whose table.
        table: DescriptorDirectoryTable,
        /// The process.
        pid: i32,
        /// Its incarnation.
        process_incarnation: u32,
        /// For a thread's directory, the thread and its incarnation.
        thread: Option<(i32, Option<u32>)>,
    },
}

/// Which of the caller's own descriptor directories the absolute guest path
/// `path` spells, for a caller whose pid and tid, as the guest numbers them,
/// are `own`: `/proc/self/fd` or `/proc/<pid>/fd` its process's,
/// `/proc/thread-self/fd`, `/proc/self/task/<tid>/fd` or
/// `/proc/<pid>/task/<tid>/fd` its thread's. `None` for any other path,
/// another process's or thread's directory, a number with a leading zero
/// (which names nothing on Linux), and a spelling with `..` (the kernel walks
/// it through the directories it names, not lexically).
pub(crate) fn classify(path: &Path, own: (i32, i32)) -> Option<DescriptorDirectoryTable> {
    let mut components = path.components().filter(|c| *c != Component::CurDir);
    if components.next() != Some(Component::RootDir)
        || components.next() != Some(Component::Normal(OsStr::new("proc")))
    {
        return None;
    }
    let mut names = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => names.push(name.to_str()?),
            _ => return None,
        }
    }
    let number = |name: &str| -> Option<i32> {
        if name.starts_with('0') || name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        name.parse().ok()
    };
    let own_process = |name: &str| name == "self" || number(name) == Some(own.0);
    match names.as_slice() {
        [process, "fd"] if own_process(process) => Some(DescriptorDirectoryTable::Process),
        ["thread-self", "fd"] => Some(DescriptorDirectoryTable::Thread),
        [process, "task", thread, "fd"]
            if own_process(process) && number(thread) == Some(own.1) =>
        {
            Some(DescriptorDirectoryTable::Thread)
        }
        _ => None,
    }
}

/// One entry of a descriptor directory, at its Linux position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListedEntry {
    pub(crate) name: Vec<u8>,
    pub(crate) ty: u8,
    pub(crate) position: u64,
}

/// The capacity Linux's descriptor table has when it holds `descriptors`
/// (ascending) and never held a higher one: `NR_OPEN_DEFAULT` (64), or the
/// next power of two above the highest descriptor (`alloc_fdtable`). For a
/// table Detcore keeps, its own record of the capacity is used instead
/// (`FileMetadata::descriptor_capacity`), which remembers descriptors since
/// closed.
pub(crate) fn capacity(descriptors: &[i32]) -> u64 {
    match descriptors.last() {
        Some(&highest) if highest >= 64 => (u64::try_from(highest).unwrap_or(0) + 1)
            .checked_next_power_of_two()
            .unwrap_or(u64::MAX / 2),
        _ => 64,
    }
}

/// The entries `proc_readfd_common` emits from `position` on, in order, and
/// the position it leaves at the end: `.` (at 0) and `..` (at 1)
/// (`dir_emit_dots`), then each open descriptor `N` at or after position
/// `N + 2`, ascending, typed as a link (`proc_fd_instantiate`'s `S_IFLNK`).
/// `table` is the descriptors open now, ascending, or `None` where the task
/// has no table (a thread group leader that has exited, whose `files` Linux
/// has released): then nothing follows the dots, and the position stays
/// where the dots left it. With a table, given with its capacity, the end is
/// that capacity plus 2; a position already past it stays.
pub(crate) fn entries_from(position: u64, table: Option<(&[i32], u64)>) -> (Vec<ListedEntry>, u64) {
    let mut entries = Vec::new();
    if position == 0 {
        entries.push(ListedEntry {
            name: b".".to_vec(),
            ty: libc::DT_DIR,
            position: 0,
        });
    }
    if position <= 1 {
        entries.push(ListedEntry {
            name: b"..".to_vec(),
            ty: libc::DT_DIR,
            position: 1,
        });
    }
    let end = match table {
        None => position.max(2),
        Some((descriptors, capacity)) => {
            for &fd in descriptors {
                let Ok(number) = u64::try_from(fd) else {
                    continue;
                };
                if number + 2 >= position {
                    entries.push(ListedEntry {
                        name: fd.to_string().into_bytes(),
                        ty: libc::DT_LNK,
                        position: number + 2,
                    });
                }
            }
            position.max(capacity + 2)
        }
    };
    (entries, end)
}

/// How many of `entries` (each with the record length `record_len` gives its
/// name length) fit in a buffer of `capacity` bytes, which Linux keeps in an
/// `int` (`dir_context::count`): a count above `i32::MAX` fits nothing.
pub(crate) fn fitting(
    entries: &[ListedEntry],
    capacity: usize,
    record_len: impl Fn(usize) -> usize,
) -> usize {
    let Ok(mut left) = i32::try_from(capacity).map(i64::from) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries {
        let length = record_len(entry.name.len()) as i64;
        if length > left {
            break;
        }
        left -= length;
        count += 1;
    }
    count
}

/// The `d_off` of each of the first `count` of `entries`: the position of the
/// entry after it, or for the last one, the position the call leaves, which
/// is the next entry's if one did not fit and `end` otherwise (`filldir64`
/// stores each record's offset as the next entry's position, and the call
/// stores the last as `ctx->pos`).
pub(crate) fn offsets(entries: &[ListedEntry], count: usize, end: u64) -> Vec<i64> {
    (0..count)
        .map(|index| {
            let next = entries.get(index + 1).map_or(end, |entry| entry.position);
            i64::try_from(next).unwrap_or(i64::MAX)
        })
        .collect()
}

/// The position a `getdents` leaves after it passed `passed` of `entries`:
/// the first entry not passed (whose position Linux stores before it tries
/// to emit it), or `end`.
pub(crate) fn position_after(entries: &[ListedEntry], passed: usize, end: u64) -> u64 {
    entries.get(passed).map_or(end, |entry| entry.position)
}

/// procfs's largest offset: its superblock keeps the default `s_maxbytes`,
/// `MAX_NON_LFS`.
pub(crate) const MAX_POSITION: i64 = (1 << 31) - 1;

/// `generic_file_llseek` on a descriptor directory at `position`: its size is
/// 0, so `SEEK_END` counts from 0 and `SEEK_DATA` and `SEEK_HOLE` find
/// nothing (`ENXIO`); a result below 0 or above [`MAX_POSITION`] is `EINVAL`,
/// as is an unknown `whence` (`ksys_lseek`). `SEEK_CUR` by 0 reads the
/// position without setting it.
pub(crate) fn seek(position: u64, offset: i64, whence: i32) -> Result<u64, Errno> {
    let current = i64::try_from(position).unwrap_or(i64::MAX);
    let target = match whence {
        libc::SEEK_SET => offset,
        libc::SEEK_CUR if offset == 0 => return Ok(position),
        libc::SEEK_CUR => current.wrapping_add(offset),
        libc::SEEK_END => offset,
        libc::SEEK_DATA | libc::SEEK_HOLE => return Err(Errno::ENXIO),
        _ => return Err(Errno::EINVAL),
    };
    if !(0..=MAX_POSITION).contains(&target) {
        return Err(Errno::EINVAL);
    }
    Ok(target as u64)
}

/// The kernel position a placeholder descriptor is left at, which tells
/// which directory it stands for where Detcore meets the descriptor without
/// its record (received over `SCM_RIGHTS`): above any ordinal position
/// Detcore gives a directory of its own, within procfs's [`MAX_POSITION`].
/// The guest never reads it: Detcore answers the descriptor's `lseek` and
/// `getdents` from its model, so the position stays where the open left it.
pub(crate) const TAG_BASE: u64 = 0x4000_0000;

/// The kernel position that tags the `index`th registered placeholder
/// directory of the run, if it fits.
pub(crate) fn tag(index: usize) -> Option<u64> {
    let tag = TAG_BASE.checked_add(u64::try_from(index).ok()?)?;
    (tag <= MAX_POSITION as u64).then_some(tag)
}

/// The registration a kernel position tags, if it is a tag.
pub(crate) fn tag_index(position: u64) -> Option<usize> {
    (TAG_BASE..=MAX_POSITION as u64)
        .contains(&position)
        .then(|| usize::try_from(position - TAG_BASE).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(entries: &[ListedEntry]) -> Vec<(String, u8, u64)> {
        entries
            .iter()
            .map(|entry| {
                (
                    String::from_utf8(entry.name.clone()).unwrap(),
                    entry.ty,
                    entry.position,
                )
            })
            .collect()
    }

    #[test]
    fn classifies_the_callers_own_descriptor_directories() {
        use DescriptorDirectoryTable::*;
        let own = (7, 9);
        let classified = |path: &str| classify(Path::new(path), own);
        assert_eq!(classified("/proc/self/fd"), Some(Process));
        assert_eq!(classified("/proc/self/fd/"), Some(Process));
        assert_eq!(classified("/proc/./self/fd/."), Some(Process));
        assert_eq!(classified("/proc/7/fd"), Some(Process));
        assert_eq!(classified("/proc/thread-self/fd"), Some(Thread));
        assert_eq!(classified("/proc/self/task/9/fd"), Some(Thread));
        assert_eq!(classified("/proc/7/task/9/fd"), Some(Thread));
        for path in [
            "/proc/8/fd",
            "/proc/07/fd",
            "/proc/7/task/8/fd",
            "/proc/8/task/9/fd",
            "/proc/self/task/09/fd",
            "/proc/self/fdinfo",
            "/proc/self/fd/3",
            "/proc/self/../self/fd",
            "/proc/self",
            "/dev/fd",
            "proc/self/fd",
            "/proc/self/task/fd",
        ] {
            assert_eq!(classified(path), None, "{path}");
        }
    }

    #[test]
    fn lists_dots_then_descriptors_at_linux_positions() {
        let table = [0, 1, 2, 5];
        let (entries, end) = entries_from(0, Some((&table, capacity(&table))));
        assert_eq!(
            names(&entries),
            [
                (".".to_string(), libc::DT_DIR, 0),
                ("..".to_string(), libc::DT_DIR, 1),
                ("0".to_string(), libc::DT_LNK, 2),
                ("1".to_string(), libc::DT_LNK, 3),
                ("2".to_string(), libc::DT_LNK, 4),
                ("5".to_string(), libc::DT_LNK, 7),
            ]
        );
        // Measured on Linux: a table that never held a descriptor at or above
        // 64 ends at 66.
        assert_eq!(end, 66);
        assert_eq!(offsets(&entries, entries.len(), end), [1, 2, 3, 4, 7, 66]);
        // From the position of descriptor 2: no dots.
        let (entries, _) = entries_from(4, Some((&table, 64)));
        assert_eq!(names(&entries)[0], ("2".to_string(), libc::DT_LNK, 4));
        // Between descriptors: the next open one.
        let (entries, _) = entries_from(5, Some((&table, 64)));
        assert_eq!(names(&entries), [("5".to_string(), libc::DT_LNK, 7)]);
        // At ".." only.
        let (entries, _) = entries_from(1, Some((&table, 64)));
        assert_eq!(names(&entries)[0].0, "..");
        // Past the end, the position stays.
        assert_eq!(entries_from(1000, Some((&table, 64))), (vec![], 1000));
    }

    /// Linux's end follows the table's allocation: after `dup2(0, 100);
    /// close(100)` the table keeps 128 slots, so the end is 130 though only
    /// 0 to 2 are open.
    #[test]
    fn the_end_follows_the_tables_capacity() {
        let table = [0, 1, 2];
        assert_eq!(entries_from(0, Some((&table, 128))).1, 130);
        assert_eq!(entries_from(0, Some((&table, 64))).1, 66);
    }

    #[test]
    fn a_leader_without_a_table_lists_only_the_dots() {
        let (entries, end) = entries_from(0, None);
        assert_eq!(names(&entries).len(), 2);
        assert_eq!(end, 2);
        assert_eq!(entries_from(9, None), (vec![], 9));
    }

    #[test]
    fn capacity_grows_to_the_next_power_of_two() {
        assert_eq!(capacity(&[]), 64);
        assert_eq!(capacity(&[0, 63]), 64);
        assert_eq!(capacity(&[64]), 128);
        assert_eq!(capacity(&[127]), 128);
        assert_eq!(capacity(&[128]), 256);
        assert_eq!(capacity(&[1000]), 1024);
    }

    #[test]
    fn paging_fits_whole_records_and_leaves_the_next_position() {
        let record = |name: usize| crate::dirents::DirentFormat::Dirent64.record_len(name);
        let table = [0, 1, 2];
        let (entries, end) = entries_from(0, Some((&table, capacity(&table))));
        // Each of these records is 24 bytes.
        assert_eq!(fitting(&entries, 24, record), 1);
        assert_eq!(fitting(&entries, 47, record), 1);
        assert_eq!(fitting(&entries, 48, record), 2);
        assert_eq!(fitting(&entries, 10, record), 0);
        assert_eq!(fitting(&entries, 0, record), 0);
        assert_eq!(fitting(&entries, 1 << 31, record), 0);
        assert_eq!(fitting(&entries, 4096, record), entries.len());
        // One record per call: each d_off is the next entry's position.
        assert_eq!(offsets(&entries, 1, end), [1]);
        assert_eq!(position_after(&entries, 1, end), 1);
        assert_eq!(position_after(&entries, entries.len(), end), end);
    }

    #[test]
    fn seeks_as_generic_file_llseek_on_an_empty_directory() {
        assert_eq!(seek(9, 0, libc::SEEK_SET), Ok(0));
        assert_eq!(seek(9, 0, libc::SEEK_CUR), Ok(9));
        assert_eq!(seek(9, 3, libc::SEEK_CUR), Ok(12));
        assert_eq!(seek(9, -10, libc::SEEK_CUR), Err(Errno::EINVAL));
        assert_eq!(seek(9, 0, libc::SEEK_END), Ok(0));
        assert_eq!(seek(9, 5, libc::SEEK_END), Ok(5));
        assert_eq!(seek(9, 0, libc::SEEK_DATA), Err(Errno::ENXIO));
        assert_eq!(seek(9, 0, libc::SEEK_HOLE), Err(Errno::ENXIO));
        assert_eq!(seek(9, -1, libc::SEEK_SET), Err(Errno::EINVAL));
        assert_eq!(
            seek(9, MAX_POSITION, libc::SEEK_SET),
            Ok(MAX_POSITION as u64)
        );
        assert_eq!(
            seek(9, MAX_POSITION + 1, libc::SEEK_SET),
            Err(Errno::EINVAL)
        );
        assert_eq!(seek(9, 0, 7), Err(Errno::EINVAL));
        assert_eq!(seek(9, i64::MAX, libc::SEEK_CUR), Err(Errno::EINVAL));
    }

    #[test]
    fn tags_round_trip_inside_procfs_positions() {
        assert_eq!(tag(0), Some(TAG_BASE));
        assert_eq!(tag_index(TAG_BASE + 5), Some(5));
        assert_eq!(tag_index(66), None);
        assert_eq!(tag_index(MAX_POSITION as u64 + 1), None);
        assert_eq!(tag((MAX_POSITION as u64 - TAG_BASE) as usize + 1), None);
    }

    #[test]
    fn a_bound_directory_is_looked_up_through_its_selector_for_its_task() {
        let binding = DescriptorDirectoryBinding {
            table: DescriptorDirectoryTable::Process,
            pid: 7,
            tid: 7,
            process_incarnation: 1,
            thread_incarnation: None,
            device: 0,
            files: None,
        };
        assert_eq!(binding.lookup_base((7, 9)), Path::new("/proc/self/fd"));
        assert_eq!(binding.lookup_base((8, 8)), Path::new("/proc/7/fd"));
        assert_eq!(binding.canonical(), Path::new("/proc/7/fd"));
        let thread = DescriptorDirectoryBinding {
            table: DescriptorDirectoryTable::Thread,
            tid: 9,
            thread_incarnation: Some(1),
            ..binding
        };
        assert_eq!(
            thread.lookup_base((7, 9)),
            Path::new("/proc/thread-self/fd")
        );
        assert_eq!(thread.lookup_base((7, 7)), Path::new("/proc/7/task/9/fd"));
        assert_eq!(thread.within(), Path::new("/7/task/9/fd"));
    }

    /// The root placeholder is `/proc` for every caller; a descriptor
    /// directory's is its binding's; each locks as its own inode, and a
    /// thread's directory apart from its process's.
    #[test]
    fn a_placeholder_names_its_directory_and_its_lock() {
        let process = DescriptorDirectoryBinding {
            table: DescriptorDirectoryTable::Process,
            pid: 7,
            tid: 7,
            process_incarnation: 1,
            thread_incarnation: None,
            device: 0,
            files: None,
        };
        let root = PlaceholderDirectory::Root;
        assert_eq!(root.canonical(), Path::new("/proc"));
        assert_eq!(root.lookup_base((7, 9)), Path::new("/proc"));
        let directory = PlaceholderDirectory::Descriptors(process);
        assert_eq!(directory.canonical(), Path::new("/proc/7/fd"));
        assert_eq!(directory.lookup_base((7, 7)), Path::new("/proc/self/fd"));
        let thread = PlaceholderDirectory::Descriptors(DescriptorDirectoryBinding {
            table: DescriptorDirectoryTable::Thread,
            tid: 9,
            thread_incarnation: Some(1),
            ..process
        });
        let keys = [root.lock_key(), directory.lock_key(), thread.lock_key()];
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[1], keys[2]);
        assert_ne!(keys[0], keys[2]);
        let reopened = PlaceholderDirectory::Descriptors(DescriptorDirectoryBinding {
            device: 5,
            files: Some(detcore_model::fd::FilesId::initial(
                detcore_model::pid::DetTid::from_raw(7),
            )),
            ..process
        });
        assert_eq!(reopened.lock_key(), keys[1], "the same task's directory");
    }
}
