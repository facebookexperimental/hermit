/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Which system-wide `/proc` names exist at each `--proc` level.
//!
//! A system-wide or `/proc/sys` name exists only if this table lists it, or
//! lists a name below it; so does a name in a process's directory
//! (`/proc/<pid>`, `/proc/self`) or a thread's (`/proc/<pid>/task/<tid>`,
//! `/proc/thread-self`). Every other name is ENOENT and missing from
//! listings, on every backend. Still not decided here, so still answered by
//! the kernel or the KVM executor: whether a process or thread directory
//! exists, the entries of `fd`, `fdinfo`, `ns` and `net`, what the `cwd`,
//! `root` and `exe` links reach, and the few names Hermit's own tests and
//! tools still need the kernel to answer ([`PROCESS_KERNEL`]). The table is the checklist in the
//! dev-hermit design note `ai_docs/proc-virtualization-design.md` (section
//! 5): its V and F entries, split into the two levels of its section 1b.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Component;
use std::path::Path;

use detcore_model::config::ProcLevel;

/// The names at the `minimal` level, relative to `/proc`. Each directory on
/// the way to a name exists too.
const MINIMAL: &[&str] = &[
    "cmdline",
    "cpuinfo",
    "filesystems",
    "loadavg",
    "meminfo",
    "modules",
    "mounts",
    "net",
    "stat",
    "sys/kernel/cap_last_cap",
    "sys/kernel/domainname",
    "sys/kernel/hostname",
    "sys/kernel/osrelease",
    "sys/kernel/ostype",
    "sys/kernel/overflowgid",
    "sys/kernel/overflowuid",
    "sys/kernel/pid_max",
    "sys/kernel/random/boot_id",
    "sys/kernel/random/uuid",
    "sys/kernel/version",
    "sys/vm/max_map_count",
    "sys/vm/mmap_min_addr",
    "sys/vm/overcommit_memory",
    "uptime",
    "version",
];

/// The names `standard` adds. `sys/net/core/somaxconn` and
/// `sys/net/ipv4/ip_local_port_range` are still the kernel's: they are per
/// network namespace, and Detcore's port allocator reads the host's range,
/// so the two change together (design note, slice B2a).
///
/// `sys/crypto/fips_enabled`, planned for `standard`, is not here yet: its
/// directory `sys/crypto` exists only on a kernel built with it (not on
/// every development host's), and a file in a directory the guest cannot
/// see or list is not one view. It comes with Detcore's own directory nodes (slice B3a's
/// placeholder directories). Until then it is ENOENT on every host, which
/// is what a host without the directory answers.
const STANDARD: &[&str] = &[
    "buddyinfo",
    "cgroups",
    "diskstats",
    "driver/rtc",
    "interrupts",
    "key-users",
    "locks",
    "pressure/cpu",
    "pressure/io",
    "pressure/memory",
    "schedstat",
    "softirqs",
    "swaps",
    "sys/fs/aio-max-nr",
    "sys/fs/aio-nr",
    "sys/fs/dentry-state",
    "sys/fs/file-max",
    "sys/fs/file-nr",
    "sys/fs/inode-nr",
    "sys/fs/inode-state",
    "sys/fs/pipe-max-size",
    "sys/kernel/pty/nr",
    "sys/kernel/random/entropy_avail",
    "sys/net/core/somaxconn",
    "sys/net/ipv4/ip_local_port_range",
    "tty/drivers",
    "vmstat",
    "zoneinfo",
];

/// The names in a process's directory at the `minimal` level. A thread's
/// directory has the same names, except that Linux gives it no `task` (and
/// gives `children` only to it), which the kernel then refuses itself.
const PROCESS_MINIMAL: &[&str] = &[
    "auxv",
    "cgroup",
    "cmdline",
    "comm",
    "cwd",
    "environ",
    "exe",
    "fd",
    "fdinfo",
    "maps",
    "mountinfo",
    "mounts",
    "net",
    "ns",
    "root",
    "smaps",
    "stat",
    "statm",
    "status",
    "task",
];

/// The names `standard` adds to a process's or a thread's directory.
/// `numa_maps` is in no level: Detcore presents a kernel built without
/// NUMA, which registers no such file
/// (https://github.com/rrnewton/hermit/issues/5269).
const PROCESS_STANDARD: &[&str] = &[
    "arch_status",
    "children",
    "io",
    "sched",
    "schedstat",
    "smaps_rollup",
    "timerslack_ns",
];

/// The names in a process's directory below which this table decides
/// nothing: links that leave `/proc` (`cwd`, `root`, `exe`), the directories
/// whose entries are a process's descriptors (`fd`, `fdinfo`), and `ns` and
/// `net`, whose entries are not decided yet (design note, slices B3a and B3b).
const PROCESS_UNDECIDED_BELOW: &[&str] = &["cwd", "exe", "fd", "fdinfo", "net", "ns", "root"];

/// The names in a process's directory that the checklist omits but that
/// stay the kernel's for now, because something Hermit runs needs the
/// kernel's answer and Detcore does not serve one yet:
/// - `mem`: Detcore's guard on writes through it, and its test
///   (`verify_claim_names_its_limit`), rr's suite and debuggers;
/// - `uid_map`: the DBT isolation test reads it in the guest
///   (`run_dbt_binds_in_a_user_namespace_of_its_own`);
/// - `gid_map`, `setgroups`: kept with `uid_map`, the files a user-namespace
///   setup writes together, until Detcore models user namespaces. Their
///   writers in this repository run before Hermit or stop at Detcore's
///   refusal of `unshare` before writing, so nothing intercepted needs them
///   yet (https://github.com/rrnewton/hermit/issues/4263).
const PROCESS_KERNEL: &[&str] = &["gid_map", "mem", "setgroups", "uid_map"];

/// The names of a process's directory in Linux's order, `tgid_base_stuff`
/// (fs/proc/base.c:3314-3426 at v6.17), with every configuration's entries
/// but `numa_maps`, which the `!CONFIG_NUMA` kernel Detcore presents does
/// not register: `proc_pident_readdir` emits the table's entries in this
/// order (fs/proc/base.c:2738-2760), so a listing of fewer names keeps it.
const PROCESS_ORDER: &[&str] = &[
    "task",
    "fd",
    "map_files",
    "fdinfo",
    "ns",
    "net",
    "environ",
    "auxv",
    "status",
    "personality",
    "limits",
    "sched",
    "autogroup",
    "timens_offsets",
    "comm",
    "syscall",
    "cmdline",
    "stat",
    "statm",
    "maps",
    "mem",
    "cwd",
    "root",
    "exe",
    "mounts",
    "mountinfo",
    "mountstats",
    "clear_refs",
    "smaps",
    "smaps_rollup",
    "pagemap",
    "attr",
    "wchan",
    "stack",
    "schedstat",
    "latency",
    "cpuset",
    "cgroup",
    "cpu_resctrl_groups",
    "oom_score",
    "oom_adj",
    "oom_score_adj",
    "loginuid",
    "sessionid",
    "make-it-fail",
    "fail-nth",
    "coredump_filter",
    "io",
    "uid_map",
    "gid_map",
    "projid_map",
    "setgroups",
    "timers",
    "timerslack_ns",
    "patch_state",
    "stack_depth",
    "arch_status",
    "seccomp_cache",
    "ksm_merging_pages",
    "ksm_stat",
];

/// The same for a thread's directory, `tid_base_stuff` (fs/proc/base.c:
/// 3664-3762 at v6.17), which has no `task`, `map_files`, `autogroup`,
/// `timens_offsets`, `mountstats`, `coredump_filter`, `timers`,
/// `timerslack_ns` or `stack_depth`, and has `children`.
const THREAD_ORDER: &[&str] = &[
    "fd",
    "fdinfo",
    "ns",
    "net",
    "environ",
    "auxv",
    "status",
    "personality",
    "limits",
    "sched",
    "comm",
    "syscall",
    "cmdline",
    "stat",
    "statm",
    "maps",
    "children",
    "mem",
    "cwd",
    "root",
    "exe",
    "mounts",
    "mountinfo",
    "clear_refs",
    "smaps",
    "smaps_rollup",
    "pagemap",
    "attr",
    "wchan",
    "stack",
    "schedstat",
    "latency",
    "cpuset",
    "cgroup",
    "cpu_resctrl_groups",
    "oom_score",
    "oom_adj",
    "oom_score_adj",
    "loginuid",
    "sessionid",
    "make-it-fail",
    "fail-nth",
    "io",
    "uid_map",
    "gid_map",
    "projid_map",
    "setgroups",
    "patch_state",
    "arch_status",
    "seccomp_cache",
    "ksm_merging_pages",
    "ksm_stat",
];

/// Where Linux lists `name` in a process's directory, or a thread's
/// (`thread`): its index in [`PROCESS_ORDER`] or [`THREAD_ORDER`], `None` for
/// a name Linux's table does not have.
pub(crate) fn process_entry_rank(thread: bool, name: &[u8]) -> Option<usize> {
    let order = if thread { THREAD_ORDER } else { PROCESS_ORDER };
    order.iter().position(|entry| entry.as_bytes() == name)
}

/// Whether the entry `name` of a thread's directory is, for its process's
/// thread group leader, the file of that name in the process's directory: an
/// entry of both tables ([`PROCESS_ORDER`], [`THREAD_ORDER`]) whose answer
/// does not depend on the table it was reached through, which is every entry
/// both tables have but `stat` and `io`, which add up the whole thread group
/// in the process's directory (`proc_tgid_stat`, `proc_tgid_io_accounting`
/// against `proc_tid_stat`, `proc_tid_io_accounting`, fs/proc/base.c at
/// v6.17), and the directories (`fd`, `fdinfo`, `ns`, `net`, `attr`), whose
/// entries are their own matter.
pub(crate) fn leader_thread_file_is_process_file(name: &[u8]) -> bool {
    !matches!(
        name,
        b"stat" | b"io" | b"fd" | b"fdinfo" | b"ns" | b"net" | b"attr"
    ) && process_entry_rank(false, name).is_some()
        && process_entry_rank(true, name).is_some()
}

/// What the table says about a path under `/proc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcfsName {
    /// A per-process name, or one reached through a link the table does not
    /// follow: not decided here.
    Undecided,
    /// A name that exists from this level up.
    From(ProcLevel),
    /// A name in no level.
    Omitted,
}

impl ProcfsName {
    /// Whether the name is absent at `level`.
    pub(crate) fn absent_at(self, level: ProcLevel) -> bool {
        match self {
            Self::Undecided => false,
            Self::From(from) => from > level,
            Self::Omitted => true,
        }
    }
}

/// The level from which the procfs name `path` exists, for an absolute path
/// with no `..` component; `None` for a path outside `/proc`. `path` is the
/// spelling the guest gave, so a link elsewhere into `/proc` is not followed.
pub(crate) fn procfs_name(path: &Path) -> Option<ProcfsName> {
    let mut components = path.components().filter(|c| *c != Component::CurDir);
    if components.next() != Some(Component::RootDir) {
        return None;
    }
    match components.next() {
        Some(Component::Normal(name)) if name == "proc" => {}
        _ => return None,
    }
    let mut names = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => names.push(name.as_bytes()),
            // A `..` is the kernel's to resolve (see the callers).
            _ => return Some(ProcfsName::Undecided),
        }
    }
    let Some(first) = names.first() else {
        return Some(ProcfsName::From(ProcLevel::Minimal));
    };
    if *first == b"self" || *first == b"thread-self" || first.iter().all(u8::is_ascii_digit) {
        return Some(process_name(&names[1..]));
    }
    // Below the `net` and `mounts` links is the process's own directory.
    if names.len() > 1 && (*first == b"net" || *first == b"mounts") {
        return Some(ProcfsName::Undecided);
    }
    // The name, a directory on the way to it, or a name below a file (which
    // the kernel refuses with ENOTDIR, as Linux does).
    let listed = |table: &[&str]| {
        table.iter().any(|entry| {
            names
                .iter()
                .zip(entry.split('/'))
                .all(|(name, part)| *name == part.as_bytes())
        })
    };
    Some(if listed(MINIMAL) {
        ProcfsName::From(ProcLevel::Minimal)
    } else if listed(STANDARD) {
        ProcfsName::From(ProcLevel::Standard)
    } else {
        ProcfsName::Omitted
    })
}

/// What the table says about the names below a process's or a thread's
/// directory.
fn process_name(names: &[&[u8]]) -> ProcfsName {
    let Some((first, rest)) = names.split_first() else {
        // Whether the process or thread exists is the kernel's.
        return ProcfsName::Undecided;
    };
    let is = |table: &[&str]| table.iter().any(|entry| entry.as_bytes() == *first);
    if *first == b"task" {
        return match rest.split_first() {
            None => ProcfsName::From(ProcLevel::Minimal),
            // A thread's directory and the names below it.
            Some((tid, below)) if tid.iter().all(u8::is_ascii_digit) => process_name(below),
            // `task` holds only threads.
            Some(_) => ProcfsName::Omitted,
        };
    }
    if (!rest.is_empty() && is(PROCESS_UNDECIDED_BELOW)) || is(PROCESS_KERNEL) {
        return ProcfsName::Undecided;
    }
    // A name below a file stays the file's (the kernel's ENOTDIR).
    if is(PROCESS_MINIMAL) {
        ProcfsName::From(ProcLevel::Minimal)
    } else if is(PROCESS_STANDARD) {
        ProcfsName::From(ProcLevel::Standard)
    } else {
        ProcfsName::Omitted
    }
}

/// The names the table gives the system-wide directory `/proc/<names>` at
/// `level` (the root for no names), each with its `d_type`, in the table's
/// order; `None` when the table holds no directory there. A name with names
/// below it is a directory; the root's `mounts` and `net` are links into the
/// caller's own directory.
pub(crate) fn system_entries(names: &[&[u8]], level: ProcLevel) -> Option<Vec<(Vec<u8>, u8)>> {
    let tables: &[&[&str]] = match level {
        ProcLevel::Minimal => &[MINIMAL],
        ProcLevel::Standard => &[MINIMAL, STANDARD],
    };
    let mut entries: Vec<(Vec<u8>, u8)> = Vec::new();
    let mut found = names.is_empty();
    for entry in tables.iter().flat_map(|table| table.iter()) {
        let mut parts = entry.split('/');
        if !names
            .iter()
            .all(|name| parts.next().is_some_and(|part| part.as_bytes() == *name))
        {
            continue;
        }
        let Some(child) = parts.next() else {
            // `names` is this entry itself, a file.
            return None;
        };
        found = true;
        let ty = if parts.next().is_some() {
            libc::DT_DIR
        } else if names.is_empty() && (child == "mounts" || child == "net") {
            libc::DT_LNK
        } else {
            libc::DT_REG
        };
        match entries
            .iter_mut()
            .find(|(name, _)| name == child.as_bytes())
        {
            Some(existing) if ty == libc::DT_DIR => existing.1 = ty,
            Some(_) => {}
            None => entries.push((child.as_bytes().to_vec(), ty)),
        }
    }
    found.then_some(entries)
}

/// The system-wide directory below `/proc` that the absolute path `path`
/// names at `level`, if it names one the table gives `level`
/// ([`system_entries`]): `/proc/sys`, `/proc/sys/kernel/random`, and so on,
/// by its index in [`system_directories`], which [`system_directory_path`]
/// takes back. Not `/proc` itself, procfs's root, nor a name in or below a
/// process's or thread's directory (`self`, `thread-self`, a number), nor a
/// spelling with a `..` component, which the kernel resolves (see
/// [`procfs_name`]'s callers).
pub(crate) fn system_directory(path: &Path, level: ProcLevel) -> Option<u16> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return None;
    }
    match components.next() {
        Some(Component::Normal(name)) if name == "proc" => {}
        _ => return None,
    }
    let mut names = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => names.push(name.as_bytes()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    let first = names.first()?;
    if *first == b"self" || *first == b"thread-self" || first.iter().all(u8::is_ascii_digit) {
        return None;
    }
    system_entries(&names, level)?;
    let relative = names.join(&b'/');
    let index = system_directories()
        .iter()
        .position(|directory| directory.as_bytes() == relative.as_slice())?;
    u16::try_from(index).ok()
}

/// The path relative to `/proc` (`sys/kernel`) of the system-wide directory
/// whose index in [`system_directories`] is `index`.
pub(crate) fn system_directory_path(index: u16) -> Option<&'static str> {
    system_directories().get(usize::from(index)).copied()
}

/// Every system-wide directory below `/proc` that the table holds at any
/// level, as its path relative to `/proc`, once each, in the table's order:
/// the directories on the way to each name of [`MINIMAL`], then of
/// [`STANDARD`] (`sys`, `sys/kernel`, `sys/kernel/random`, ...).
fn system_directories() -> Vec<&'static str> {
    let mut directories: Vec<&'static str> = Vec::new();
    for entry in MINIMAL.iter().chain(STANDARD) {
        for (at, _) in entry.match_indices('/') {
            let directory = &entry[..at];
            if !directories.contains(&directory) {
                directories.push(directory);
            }
        }
    }
    directories
}

/// The names in a process's directory, or a thread's (`thread`), at
/// `level`, each with its `d_type`: the level's names and the kernel's
/// ([`PROCESS_KERNEL`]), less `task` and `timerslack_ns` in a thread's
/// directory and `children` in a process's, as on Linux
/// (`tgid_base_stuff`, `tid_base_stuff`), in Linux's order
/// ([`process_entry_rank`]).
pub(crate) fn process_entries(thread: bool, level: ProcLevel) -> Vec<(&'static str, u8)> {
    let mut names: Vec<&'static str> = PROCESS_MINIMAL.to_vec();
    if level >= ProcLevel::Standard {
        names.extend_from_slice(PROCESS_STANDARD);
    }
    names.extend_from_slice(PROCESS_KERNEL);
    names.sort_by_key(|name| process_entry_rank(thread, name.as_bytes()).unwrap_or(usize::MAX));
    names
        .into_iter()
        .filter(|name| match *name {
            "task" | "timerslack_ns" => !thread,
            "children" => thread,
            _ => true,
        })
        .map(|name| {
            let ty = match name {
                "fd" | "fdinfo" | "net" | "ns" | "task" => libc::DT_DIR,
                "cwd" | "exe" | "root" => libc::DT_LNK,
                _ => libc::DT_REG,
            };
            (name, ty)
        })
        .collect()
}

/// Whether the entry `name` of the directory at the canonical path
/// `directory` is absent at `level`.
pub(crate) fn absent_entry(directory: &Path, name: &[u8], level: ProcLevel) -> bool {
    if name == b"." || name == b".." {
        return false;
    }
    procfs_name(&directory.join(OsStr::from_bytes(name)))
        .is_some_and(|entry| entry.absent_at(level))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(path: &str) -> Option<ProcfsName> {
        procfs_name(Path::new(path))
    }

    #[test]
    fn levels_follow_the_checklist() {
        use ProcLevel::*;
        use ProcfsName::*;
        for (path, expected) in [
            ("/proc", From(Minimal)),
            ("/proc/", From(Minimal)),
            ("/proc/version", From(Minimal)),
            ("/proc/./version", From(Minimal)),
            ("/proc/sys", From(Minimal)),
            ("/proc/sys/kernel", From(Minimal)),
            ("/proc/sys/kernel/random", From(Minimal)),
            ("/proc/sys/kernel/random/uuid", From(Minimal)),
            ("/proc/sys/fs", From(Standard)),
            ("/proc/sys/fs/file-nr", From(Standard)),
            ("/proc/sys/kernel/random/entropy_avail", From(Standard)),
            ("/proc/sys/net", From(Standard)),
            ("/proc/interrupts", From(Standard)),
            ("/proc/tty", From(Standard)),
            ("/proc/tty/drivers", From(Standard)),
            ("/proc/version/x", From(Minimal)),
            ("/proc/kallsyms", Omitted),
            ("/proc/kcore", Omitted),
            ("/proc/sysvipc", Omitted),
            ("/proc/sysvipc/shm", Omitted),
            ("/proc/slabinfo", Omitted),
            ("/proc/bus/pci/devices", Omitted),
            ("/proc/tty/ldiscs", Omitted),
            ("/proc/sys/kernel/core_pattern", Omitted),
            ("/proc/sys/kernel/ngroups_max", Omitted),
            ("/proc/sys/kernel/random/poolsize", Omitted),
            ("/proc/sys/fs/binfmt_misc", Omitted),
            ("/proc/sys/crypto/fips_enabled", Omitted),
            ("/proc/sys/net/ipv4/tcp_rmem", Omitted),
            ("/proc/versionx", Omitted),
            ("/proc/self", Undecided),
            ("/proc/thread-self", Undecided),
            ("/proc/1", Undecided),
            ("/proc/1/stat", From(Minimal)),
            ("/proc/self/status", From(Minimal)),
            ("/proc/thread-self/status", From(Minimal)),
            ("/proc/self/task", From(Minimal)),
            ("/proc/self/task/7", Undecided),
            ("/proc/self/task/7/comm", From(Minimal)),
            ("/proc/self/task/7/children", From(Standard)),
            ("/proc/self/task/x", Omitted),
            ("/proc/self/io", From(Standard)),
            ("/proc/1/timerslack_ns", From(Standard)),
            // No NUMA: a kernel built without it registers no `numa_maps`
            // in a process's or a thread's directory, at either level.
            ("/proc/self/numa_maps", Omitted),
            ("/proc/thread-self/numa_maps", Omitted),
            ("/proc/1/task/1/numa_maps", Omitted),
            ("/proc/self/limits", Omitted),
            ("/proc/self/mem", Undecided),
            ("/proc/1/task/1/mem", Undecided),
            ("/proc/thread-self/wchan", Omitted),
            ("/proc/self/uid_map", Undecided),
            ("/proc/self/setgroups", Undecided),
            ("/proc/self/oom_score_adj", Omitted),
            ("/proc/self/loginuid", Omitted),
            ("/proc/1/task/1/attr/current", Omitted),
            ("/proc/self/map_files", Omitted),
            ("/proc/self/status/x", From(Minimal)),
            ("/proc/self/fd", From(Minimal)),
            ("/proc/self/fd/3", Undecided),
            ("/proc/self/fdinfo/3", Undecided),
            ("/proc/self/ns/pid", Undecided),
            ("/proc/self/net/tcp", Undecided),
            ("/proc/self/root/proc/kallsyms", Undecided),
            ("/proc/self/cwd/x", Undecided),
            ("/proc/net/tcp", Undecided),
            ("/proc/mounts/x", Undecided),
            ("/proc/self/../kallsyms", Undecided),
        ] {
            assert_eq!(name(path), Some(expected), "{path}");
        }
        for path in [
            "/",
            "/procx/version",
            "/sys/kernel",
            "proc/version",
            "/tmp/proc/kcore",
        ] {
            assert_eq!(name(path), None, "{path}");
        }
        // A name that is not UTF-8 is in no level.
        assert_eq!(
            procfs_name(Path::new(OsStr::from_bytes(b"/proc/\xff"))),
            Some(Omitted)
        );
    }

    #[test]
    fn each_level_contains_the_one_before() {
        let interrupts = name("/proc/interrupts").unwrap();
        assert!(interrupts.absent_at(ProcLevel::Minimal));
        assert!(!interrupts.absent_at(ProcLevel::Standard));
        let version = name("/proc/version").unwrap();
        assert!(!version.absent_at(ProcLevel::Minimal));
        assert!(!version.absent_at(ProcLevel::Standard));
        let kallsyms = name("/proc/kallsyms").unwrap();
        assert!(kallsyms.absent_at(ProcLevel::Minimal));
        assert!(kallsyms.absent_at(ProcLevel::Standard));
        // Every rendered or fixed file is in a level.
        for path in super::super::fixed::FixedProcfsFile::ALL
            .iter()
            .map(|file| file.path())
        {
            assert!(
                !procfs_name(path).unwrap().absent_at(ProcLevel::Standard),
                "{path:?}"
            );
        }
    }

    #[test]
    fn listings_leave_out_what_lookups_refuse() {
        let proc = Path::new("/proc");
        let level = ProcLevel::Standard;
        assert!(absent_entry(proc, b"kallsyms", level));
        assert!(!absent_entry(proc, b"interrupts", level));
        assert!(absent_entry(proc, b"interrupts", ProcLevel::Minimal));
        assert!(!absent_entry(proc, b"self", level));
        assert!(!absent_entry(proc, b"42", level));
        assert!(!absent_entry(proc, b".", level));
        assert!(!absent_entry(proc, b"..", level));
        assert!(absent_entry(Path::new("/proc/sys"), b"crypto", level));
        assert!(!absent_entry(Path::new("/proc/sys"), b"fs", level));
        assert!(absent_entry(
            Path::new("/proc/sys"),
            b"fs",
            ProcLevel::Minimal
        ));
        assert!(absent_entry(
            Path::new("/proc/sys/kernel"),
            b"core_pattern",
            level
        ));
        assert!(absent_entry(Path::new("/proc/self"), b"limits", level));
        assert!(absent_entry(Path::new("/proc/self"), b"numa_maps", level));
        assert!(absent_entry(
            Path::new("/proc/self"),
            b"numa_maps",
            ProcLevel::Minimal
        ));
        assert!(!absent_entry(Path::new("/proc/42"), b"status", level));
        assert!(absent_entry(
            Path::new("/proc/42"),
            b"io",
            ProcLevel::Minimal
        ));
        assert!(!absent_entry(Path::new("/proc/42/task"), b"43", level));
        assert!(!absent_entry(Path::new("/proc/42/fd"), b"3", level));
        assert!(!absent_entry(Path::new("/tmp"), b"kallsyms", level));
        assert!(absent_entry(proc, b"\xff", level));
    }

    /// A process's and a thread's directory list their names in Linux's
    /// order (`tgid_base_stuff`, `tid_base_stuff`; package K slice K10).
    #[test]
    fn process_entries_keep_linuxs_order() {
        let names = |thread| {
            process_entries(thread, ProcLevel::Minimal)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(false),
            [
                "task",
                "fd",
                "fdinfo",
                "ns",
                "net",
                "environ",
                "auxv",
                "status",
                "comm",
                "cmdline",
                "stat",
                "statm",
                "maps",
                "mem",
                "cwd",
                "root",
                "exe",
                "mounts",
                "mountinfo",
                "smaps",
                "cgroup",
                "uid_map",
                "gid_map",
                "setgroups",
            ]
        );
        assert_eq!(names(true)[..3], ["fd", "fdinfo", "ns"]);
        assert!(!names(true).contains(&"task"));
        for thread in [false, true] {
            let ranks = process_entries(thread, ProcLevel::Standard)
                .into_iter()
                .map(|(name, _)| process_entry_rank(thread, name.as_bytes()))
                .collect::<Vec<_>>();
            assert!(ranks.iter().all(Option::is_some), "{ranks:?}");
            assert!(ranks.windows(2).all(|pair| pair[0] < pair[1]), "{ranks:?}");
        }
        assert_eq!(process_entry_rank(false, b"task"), Some(0));
        assert_eq!(process_entry_rank(true, b"task"), None);
        assert_eq!(process_entry_rank(false, b"children"), None);
        assert_eq!(process_entry_rank(true, b"children"), Some(16));
    }

    /// A leader's thread file is its process's file of that name, except
    /// `stat` and `io` and the directories, and the names only one table
    /// has.
    #[test]
    fn a_leaders_thread_files_are_its_processs_but_stat_and_io() {
        for name in [
            "status", "maps", "comm", "environ", "cmdline", "mounts", "cgroup",
        ] {
            assert!(
                leader_thread_file_is_process_file(name.as_bytes()),
                "{name}"
            );
        }
        for name in [
            "stat",
            "io",
            "fd",
            "fdinfo",
            "ns",
            "net",
            "attr",
            "task",
            "children",
            "timerslack_ns",
            "mountstats",
            "nothing",
        ] {
            assert!(
                !leader_thread_file_is_process_file(name.as_bytes()),
                "{name}"
            );
        }
    }

    /// The system-wide directories the table holds at each level, by any
    /// spelling without `..`; not procfs's root, a file, a link (`net`,
    /// `mounts`) or what is below one, a process's or thread's name, a name
    /// the level omits, or a path outside `/proc`.
    #[test]
    fn system_directories_follow_the_table() {
        use ProcLevel::*;
        for (path, minimal, standard) in [
            ("/proc/sys", true, true),
            ("/proc/sys/", true, true),
            ("/proc//sys/./kernel", true, true),
            ("/proc/sys/kernel/random", true, true),
            ("/proc/sys/vm", true, true),
            ("/proc/sys/fs", false, true),
            ("/proc/sys/net/ipv4", false, true),
            ("/proc/pressure", false, true),
            ("/proc/driver", false, true),
            ("/proc", false, false),
            ("/proc/", false, false),
            ("/proc/sys/kernel/random/uuid", false, false),
            ("/proc/version", false, false),
            ("/proc/net", false, false),
            ("/proc/net/dev", false, false),
            ("/proc/mounts", false, false),
            ("/proc/self", false, false),
            ("/proc/self/task", false, false),
            ("/proc/thread-self/fd", false, false),
            ("/proc/1/fd", false, false),
            ("/proc/sys/crypto", false, false),
            ("/proc/fs", false, false),
            ("/proc/sys/../sys", false, false),
            ("/proc/sys/kernel/..", false, false),
            ("proc/sys", false, false),
            ("/sys/kernel", false, false),
            ("/procfs/sys", false, false),
        ] {
            let path = Path::new(path);
            assert_eq!(
                system_directory(path, Minimal).is_some(),
                minimal,
                "{path:?}"
            );
            assert_eq!(
                system_directory(path, Standard).is_some(),
                standard,
                "{path:?}"
            );
        }
    }

    /// Each system-wide directory has one index at every level that holds
    /// it, which names it back: the twelve directories of the two levels,
    /// the four of `minimal` first.
    #[test]
    fn system_directories_are_named_by_their_index() {
        use ProcLevel::*;
        let expected = [
            ("sys", true),
            ("sys/kernel", true),
            ("sys/kernel/random", true),
            ("sys/vm", true),
            ("driver", false),
            ("pressure", false),
            ("sys/fs", false),
            ("sys/kernel/pty", false),
            ("sys/net", false),
            ("sys/net/core", false),
            ("sys/net/ipv4", false),
            ("tty", false),
        ];
        assert_eq!(system_directories().len(), expected.len());
        for (index, (relative, minimal)) in expected.into_iter().enumerate() {
            let index = u16::try_from(index).unwrap();
            assert_eq!(system_directory_path(index), Some(relative));
            let spelled = format!("/proc//{relative}/.");
            let path = Path::new(&spelled);
            assert_eq!(system_directory(path, Standard), Some(index), "{relative}");
            assert_eq!(
                system_directory(path, Minimal),
                minimal.then_some(index),
                "{relative}"
            );
        }
        assert_eq!(system_directory_path(12), None);
        assert_eq!(system_directory(Path::new("/proc"), Standard), None);
        assert_eq!(system_directory(Path::new("/proc/net"), Standard), None);
    }
}
