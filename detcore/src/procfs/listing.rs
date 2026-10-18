/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The entries of the `/proc` directories Detcore lists from its own model
//! (slice B3a-3 of <https://github.com/rrnewton/hermit/issues/4266>), rather
//! than from the host's procfs: the root, the level table's other
//! directories, a process's or a thread's directory, and a process's
//! `task`. The names come from the `--proc` level table (see
//! [`super::levels`]), so a listing is the same on every host; the processes
//! and threads come from Detcore's scheduler, as the guest numbers them.
//! A process's `fd` stays the kernel's: on ptrace its table is the guest's,
//! and the in-guest backends' Detcore discovers descriptors only as they are
//! used, so its table cannot list the ones never used (their runtime's own
//! descriptors still show there; slice B3c's descriptor fact).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Component;
use std::path::Path;

use detcore_model::config::ProcLevel;

use super::levels;

/// A `/proc` directory whose entries Detcore lists from its model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModeledDirectory {
    /// A system-wide directory the level table holds: `/proc` itself (no
    /// names), `/proc/sys`, `/proc/sys/kernel`, ...
    System(Vec<Vec<u8>>),
    /// The directory of the process `pid`.
    Process(i32),
    /// The directory of the thread `tid` of the process `pid`.
    Thread(i32, i32),
    /// The `task` directory of the process `pid`.
    Tasks(i32),
}

/// The directory Detcore lists itself at the absolute, `..`-free guest path
/// `path`, for a caller whose pid and tid, as the guest numbers them, are
/// `pid` and `tid`: `None` for a path outside `/proc`, with `..`, or naming a
/// directory the model does not list (`fd`, `fdinfo`, `ns`, `net`). A pid or
/// tid spelled with a leading zero names nothing
/// on Linux, so it is left to the kernel.
pub(crate) fn classify(path: &Path, pid: i32, tid: i32) -> Option<ModeledDirectory> {
    let mut components = path.components().filter(|c| *c != Component::CurDir);
    if components.next() != Some(Component::RootDir)
        || components.next() != Some(Component::Normal(OsStr::new("proc")))
    {
        return None;
    }
    let mut names = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => names.push(name.as_bytes()),
            _ => return None,
        }
    }
    let number = |name: &[u8]| -> Option<i32> {
        if name.first() == Some(&b'0') || !name.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(name).ok()?.parse().ok()
    };
    // The process (and thread) a leading `self`, `thread-self` or number
    // names, and the names below it.
    let (process, thread, below) = match names.split_first() {
        None => return Some(ModeledDirectory::System(Vec::new())),
        Some((first, rest)) if *first == b"self" => (pid, None, rest),
        Some((first, rest)) if *first == b"thread-self" => (pid, Some(tid), rest),
        Some((first, rest)) => match number(first) {
            Some(process) => (process, None, rest),
            None => {
                let names = names.iter().map(|name| name.to_vec()).collect::<Vec<_>>();
                let borrowed = names.iter().map(Vec::as_slice).collect::<Vec<_>>();
                return levels::system_entries(&borrowed, ProcLevel::Standard)
                    .map(|_| ModeledDirectory::System(names));
            }
        },
    };
    // `task/<tid>` below a process directory names a thread.
    let (thread, below) = match (thread, below) {
        (None, [task, tid, rest @ ..]) if *task == b"task" => (Some(number(tid)?), rest),
        (thread, below) => (thread, below),
    };
    match (thread, below) {
        (None, []) => Some(ModeledDirectory::Process(process)),
        (Some(thread), []) => Some(ModeledDirectory::Thread(process, thread)),
        (None, [task]) if *task == b"task" => Some(ModeledDirectory::Tasks(process)),
        _ => None,
    }
}

/// The model's facts a listing needs.
#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize
)]
pub struct ListingFacts {
    /// The live processes, as the guest numbers them.
    pub processes: Vec<i32>,
    /// The live threads of the directory's process, as the guest numbers
    /// them.
    pub threads: Vec<i32>,
}

/// Whether the task `task` exists, as Linux's lookup of `/proc/<process>` and
/// its `task/<thread>` finds it: `leader` is the thread group `task.process`
/// names (itself for a process, its group for a live thread, `None` for an id
/// that names neither), and `facts` the scheduler's listing for that group
/// (`Scheduler::procfs_listing`), whose processes count a zombie until it is
/// reaped. Pid 1, the namespace's init, is Hermit's own process unless a guest
/// holds it, and has the one thread 1, as `/proc` lists it ([`entries`]).
pub(crate) fn task_exists(
    task: crate::procfs::BoundTask,
    leader: Option<i32>,
    facts: &ListingFacts,
) -> bool {
    let listed = |process: i32| facts.processes.binary_search(&process).is_ok();
    match leader {
        Some(leader) if listed(leader) => task
            .thread
            .is_none_or(|thread| facts.threads.contains(&thread)),
        _ => task.process == 1 && task.thread.is_none_or(|thread| thread == 1),
    }
}

/// What Detcore's process table says of the task a per-process `/proc` file
/// names (`Scheduler::procfs_task`), with the incarnation of each of its
/// numbers (`InodePool::count_incarnation`).
#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize
)]
pub struct ProcfsTaskAnswer {
    /// Linux's lookup finds the task: a live task, or a zombie not yet
    /// reaped.
    pub exists: bool,
    /// The task the file describes (the thread below `task`, else the task the
    /// top-level number names) has exited and is not reaped: Linux has freed
    /// its memory map, so its files belong to global root
    /// (`task_dump_owner`).
    pub without_mm: bool,
    /// How many tasks of the run have held the process number.
    pub process_incarnation: u32,
    /// How many tasks of the run have held the thread number, if one is named.
    pub thread_incarnation: Option<u32>,
    /// Address-space incarnation of the exact named task.
    #[serde(default)]
    pub mm: Option<detcore_model::futex::MmId>,
    /// Facts registered for that mm, never the requesting reader's.
    #[serde(default)]
    pub runtime_private_memory: Option<reverie::RuntimePrivateMemory>,
}

impl ProcfsTaskAnswer {
    /// The identity of the task holding the numbers now
    /// ([`crate::procfs::TaskIncarnation`]).
    pub(crate) fn incarnation(&self) -> crate::procfs::TaskIncarnation {
        crate::procfs::TaskIncarnation {
            process: self.process_incarnation,
            thread: self.thread_incarnation,
        }
    }

    /// Whether this answer is about the very task a file or a directory was
    /// bound to with identity `bound`: it still exists, and no other task
    /// has taken its numbers since (each number's incarnation is the one it
    /// had). A pid Detcore gives out again names a new task
    /// (`Scheduler::forget_previous_process_exit`), whose numbers have a
    /// higher incarnation, so a file or directory of the old task is stale.
    pub(crate) fn still_names(&self, bound: crate::procfs::TaskIncarnation) -> bool {
        self.exists && self.incarnation() == bound
    }
}

/// A change to the state Linux keeps on a per-process `/proc` file's inode
/// that Detcore keeps in its global tool, ordered by the scheduler
/// (`InodePool::rendered_file_state`), or none, to read it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize
)]
pub enum RenderedFileChange {
    /// Read the state.
    Read,
    /// Set the modification time to these seconds and nanoseconds
    /// (`utimensat`, `utime`, `utimes`).
    Mtime(i64, i64),
    /// Set the write-life hint (`F_SET_RW_HINT`).
    WriteHint(u64),
}

/// The state of a per-process `/proc` file's inode that Detcore keeps.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize
)]
pub struct RenderedFileState {
    /// The modification time an owner set, if any; the epoch otherwise.
    pub mtime: Option<(i64, i64)>,
    /// The write-life hint (`RWH_WRITE_LIFE_*`), 0 until set.
    pub write_hint: u64,
}

/// The entries of `directory` at `level`, given `facts`, each a name and its
/// `d_type`, with `.` and `..` first. `None` when the level hides the
/// directory itself (a standard-level directory at minimal).
pub(crate) fn entries(
    directory: &ModeledDirectory,
    level: ProcLevel,
    facts: &ListingFacts,
) -> Option<Vec<(Vec<u8>, u8)>> {
    let mut entries = vec![
        (b".".to_vec(), libc::DT_DIR),
        (b"..".to_vec(), libc::DT_DIR),
    ];
    match directory {
        ModeledDirectory::System(names) => {
            let borrowed = names.iter().map(Vec::as_slice).collect::<Vec<_>>();
            entries.extend(levels::system_entries(&borrowed, level)?);
            if names.is_empty() {
                entries.push((b"self".to_vec(), libc::DT_LNK));
                entries.push((b"thread-self".to_vec(), libc::DT_LNK));
                // The namespace's init, which is Hermit's own process unless
                // a guest holds pid 1.
                let mut processes = facts.processes.clone();
                if !processes.contains(&1) {
                    processes.push(1);
                }
                processes.sort_unstable();
                processes.dedup();
                entries.extend(
                    processes
                        .into_iter()
                        .map(|process| (process.to_string().into_bytes(), libc::DT_DIR)),
                );
            }
        }
        ModeledDirectory::Process(_) | ModeledDirectory::Thread(..) => {
            let thread = matches!(directory, ModeledDirectory::Thread(..));
            entries.extend(
                levels::process_entries(thread, level)
                    .into_iter()
                    .map(|(name, ty)| (name.as_bytes().to_vec(), ty)),
            );
        }
        ModeledDirectory::Tasks(_) => {
            let mut threads = facts.threads.clone();
            threads.sort_unstable();
            threads.dedup();
            entries.extend(
                threads
                    .into_iter()
                    .map(|thread| (thread.to_string().into_bytes(), libc::DT_DIR)),
            );
        }
    }
    Some(entries)
}

/// The path within procfs of `directory`, as the guest numbers it, for
/// keying its entries' inode numbers.
pub(crate) fn within(directory: &ModeledDirectory) -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from("/");
    match directory {
        ModeledDirectory::System(names) => {
            for name in names {
                path.push(OsStr::from_bytes(name));
            }
        }
        ModeledDirectory::Process(process) => path.push(process.to_string()),
        ModeledDirectory::Thread(process, thread) => {
            path.push(process.to_string());
            path.push("task");
            path.push(thread.to_string());
        }
        ModeledDirectory::Tasks(process) => {
            path.push(process.to_string());
            path.push("task");
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classified(path: &str) -> Option<ModeledDirectory> {
        classify(Path::new(path), 3, 5)
    }

    #[test]
    fn classifies_the_directories_detcore_lists() {
        use ModeledDirectory::*;
        assert_eq!(classified("/proc"), Some(System(vec![])));
        assert_eq!(classified("/proc/"), Some(System(vec![])));
        assert_eq!(
            classified("/proc/./sys"),
            Some(System(vec![b"sys".to_vec()]))
        );
        assert_eq!(
            classified("/proc/sys/kernel"),
            Some(System(vec![b"sys".to_vec(), b"kernel".to_vec()]))
        );
        assert_eq!(classified("/proc/self"), Some(Process(3)));
        assert_eq!(classified("/proc/7"), Some(Process(7)));
        assert_eq!(classified("/proc/thread-self"), Some(Thread(3, 5)));
        assert_eq!(classified("/proc/7/task/9"), Some(Thread(7, 9)));
        assert_eq!(classified("/proc/self/task"), Some(Tasks(3)));
        assert_eq!(classified("/proc/7/task"), Some(Tasks(7)));
        // Left to the kernel: descriptor and undecided directories, files,
        // leading zeros, `..`, other paths.
        for path in [
            "/proc/self/fd",
            "/proc/7/fd",
            "/proc/thread-self/fd",
            "/proc/self/ns",
            "/proc/self/net",
            "/proc/self/fdinfo",
            "/proc/version",
            "/proc/sys/kernel/hostname",
            "/proc/kallsyms",
            "/proc/03",
            "/proc/self/task/05",
            "/proc/self/..",
            "/proc/../proc",
            "/tmp",
            "proc",
        ] {
            assert_eq!(classified(path), None, "{path}");
        }
    }

    fn names(entries: Option<Vec<(Vec<u8>, u8)>>) -> Vec<String> {
        entries
            .expect("listed")
            .into_iter()
            .map(|(name, ty)| format!("{}/{ty}", String::from_utf8(name).unwrap()))
            .collect()
    }

    #[test]
    fn lists_the_root_from_the_level_and_the_processes() {
        let facts = ListingFacts {
            processes: vec![7, 3],
            ..Default::default()
        };
        let root = names(entries(
            &ModeledDirectory::System(vec![]),
            ProcLevel::Minimal,
            &facts,
        ));
        let expected = [
            "./4",
            "../4",
            "cmdline/8",
            "cpuinfo/8",
            "filesystems/8",
            "loadavg/8",
            "meminfo/8",
            "modules/8",
            "mounts/10",
            "net/10",
            "stat/8",
            "sys/4",
            "uptime/8",
            "version/8",
            "self/10",
            "thread-self/10",
            "1/4",
            "3/4",
            "7/4",
        ];
        assert_eq!(root, expected);
        // A guest holding pid 1 is listed once.
        let facts = ListingFacts {
            processes: vec![1, 2],
            ..Default::default()
        };
        let root = names(entries(
            &ModeledDirectory::System(vec![]),
            ProcLevel::Minimal,
            &facts,
        ));
        assert_eq!(&root[root.len() - 2..], ["1/4", "2/4"]);
        // `standard` adds names, and `sys` gains `fs` and `net`.
        let standard = names(entries(
            &ModeledDirectory::System(vec![]),
            ProcLevel::Standard,
            &ListingFacts::default(),
        ));
        assert!(standard.contains(&"pressure/4".to_string()), "{standard:?}");
        assert!(standard.contains(&"tty/4".to_string()), "{standard:?}");
        let sys = |level| {
            names(entries(
                &ModeledDirectory::System(vec![b"sys".to_vec()]),
                level,
                &ListingFacts::default(),
            ))
        };
        assert_eq!(sys(ProcLevel::Minimal), ["./4", "../4", "kernel/4", "vm/4"]);
        assert_eq!(
            sys(ProcLevel::Standard),
            ["./4", "../4", "kernel/4", "vm/4", "fs/4", "net/4"]
        );
        // A standard-level directory is hidden at minimal.
        assert_eq!(
            entries(
                &ModeledDirectory::System(vec![b"pressure".to_vec()]),
                ProcLevel::Minimal,
                &ListingFacts::default()
            ),
            None
        );
    }

    #[test]
    fn lists_process_thread_and_task_directories() {
        let process = names(entries(
            &ModeledDirectory::Process(3),
            ProcLevel::Minimal,
            &ListingFacts::default(),
        ));
        assert!(process.contains(&"task/4".to_string()));
        assert!(process.contains(&"cwd/10".to_string()));
        assert!(process.contains(&"mem/8".to_string()));
        assert!(!process.contains(&"children/8".to_string()));
        assert!(!process.contains(&"io/8".to_string()));
        let thread = names(entries(
            &ModeledDirectory::Thread(3, 5),
            ProcLevel::Standard,
            &ListingFacts::default(),
        ));
        assert!(!thread.contains(&"task/4".to_string()));
        assert!(!thread.contains(&"timerslack_ns/8".to_string()));
        assert!(thread.contains(&"children/8".to_string()));
        assert!(thread.contains(&"io/8".to_string()));
        let facts = ListingFacts {
            threads: vec![5, 3],
            ..Default::default()
        };
        assert_eq!(
            names(entries(
                &ModeledDirectory::Tasks(3),
                ProcLevel::Minimal,
                &facts
            )),
            ["./4", "../4", "3/4", "5/4"]
        );
        assert_eq!(
            within(&ModeledDirectory::Thread(3, 5)),
            Path::new("/3/task/5")
        );
        assert_eq!(within(&ModeledDirectory::Tasks(3)), Path::new("/3/task"));
        assert_eq!(within(&ModeledDirectory::System(vec![])), Path::new("/"));
    }
}
